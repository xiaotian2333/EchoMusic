//! EchoMusic DLNA MediaRenderer 接收端（issue #516）。
//!
//! 基于 vendored dlna-dmr（MIT，见 vendor/dlna-dmr/Cargo.toml 补丁说明）：
//! - SSDP 组播宣告（alive/byebye/keep-alive）+ M-SEARCH 应答
//! - HTTP 设备描述 / SCPD / SOAP 控制服务（axum）
//! - AVTransport / RenderingControl / ConnectionManager 三服务的应答在 native 侧完成，
//!   播放动作以事件形式回调 JS，由 TS 桥接层对接 PlayerController
//! - Get* 查询动作根据 JS 推送的状态缓存（update_renderer_state）直接应答

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::extract::Request;
use axum::Router;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use quick_xml::DeError;
use tokio::net::TcpListener;
use tokio::sync::watch;

use dlna_dmr::ssdp::SSDPServer;
use dlna_dmr::xml::{AVTransport, RenderingControl};
use dlna_dmr::{DMROptions, HTTPServer};

mod gena;

// ---------------------------------------------------------------------------
// log 门面桥：dlna-dmr 内部用 log crate 记日志，这里装一个转发实现，
// 让 SSDP/SOAP 的诊断输出经 echo_native_log 送到主进程日志。
// ---------------------------------------------------------------------------

struct NativeLogBridge;

impl log::Log for NativeLogBridge {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let message = format!("[{}] {}", record.target(), record.args());
        match record.level() {
            log::Level::Error => echo_native_log::error(message),
            log::Level::Warn => echo_native_log::warn(message),
            log::Level::Info => echo_native_log::info(message),
            _ => echo_native_log::debug(message),
        }
    }

    fn flush(&self) {}
}

static LOG_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn ensure_native_logger() {
    LOG_INIT.get_or_init(|| {
        let _ = log::set_boxed_logger(Box::new(NativeLogBridge));
        log::set_max_level(log::LevelFilter::Debug);
    });
}

/// AVTransport 服务命名空间（SOAP 应答与 SOAPACTION 一致）。
const AVT_NS: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// RenderingControl 服务命名空间。
const RC_NS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
/// ConnectionManager 服务命名空间。
const CM_NS: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// ConnectionManager Sink 协议列表：音频 + 视频容器（视频由解码器提取音频轨）。
/// 刻意不列图片与 m3u8（HLS），避免发送端提供我们无法处理的内容类型。
pub(crate) const SINK_PROTOCOLS: &str = "http-get:*:audio/mpeg:*,http-get:*:audio/mp3:*,\
http-get:*:audio/mp4:*,http-get:*:audio/aac:*,http-get:*:audio/x-m4a:*,\
http-get:*:audio/x-aac:*,http-get:*:audio/flac:*,http-get:*:audio/x-flac:*,\
http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,http-get:*:audio/L16:*,\
http-get:*:audio/ogg:*,http-get:*:audio/vorbis:*,http-get:*:audio/opus:*,\
http-get:*:audio/x-ms-wma:*,http-get:*:audio/x-ape:*,http-get:*:audio/ape:*,\
http-get:*:video/mp4:*,http-get:*:video/x-matroska:*,http-get:*:video/quicktime:*,\
http-get:*:video/x-msvideo:*,http-get:*:video/avi:*";

/// 明确不支持的 URI 扩展名：播放列表/流媒体清单、图片、字幕、文档等。
const UNSUPPORTED_EXTS: &[&str] = &[
    "m3u8", "m3u", "mpd", "smil", "asx", "pls", "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg",
    "srt", "ass", "ssa", "txt", "pdf", "html", "htm", "xml",
];

// ---------------------------------------------------------------------------
// NAPI 对象定义
// ---------------------------------------------------------------------------

/// 启动参数。
#[napi(object)]
#[derive(Clone)]
pub struct DlnaRendererOptions {
    /// 对外展示的设备名（friendlyName）。
    pub friendly_name: String,
    /// 设备 UDN 的 UUID（由上层持久化，避免每次启动变成"新设备"）。
    pub uuid: String,
    /// 绑定的局域网 IPv4（如 192.168.1.10）；传空字符串则拒绝启动（上层先用 pickLanIpv4 选好）。
    pub lan_ip: String,
    /// HTTP 服务端口；0 表示由系统分配随机端口。
    pub http_port: u16,
}

/// 推送给 JS 的事件载荷。
#[napi(object)]
#[derive(Default, Clone)]
pub struct DlnaRendererEvent {
    /// 事件类型：set-av-transport-uri / play / pause / stop / seek / next / previous /
    /// set-volume / set-mute / unsupported / started / stopped
    pub kind: String,
    /// 投播 URI（set-av-transport-uri / unsupported）。
    pub uri: Option<String>,
    /// DIDL-Lite 原始元数据（set-av-transport-uri）。
    pub metadata: Option<String>,
    /// 从 DIDL 提取的标题（set-av-transport-uri）。
    pub title: Option<String>,
    /// Seek 单位（ABS_COUNT / TRACK_NR / REL_TIME）。
    pub unit: Option<String>,
    /// Seek 目标值。
    pub target: Option<String>,
    /// 音量 0-100（set-volume）。
    pub value: Option<f64>,
    /// 静音（set-mute）。
    pub mute: Option<bool>,
    /// HTTP 服务实际端口（started）。
    pub port: Option<u32>,
    /// 绑定的 IP（started）。
    pub ip: Option<String>,
    /// 不支持原因（unsupported）。
    pub reason: Option<String>,
}

/// JS 回推的播放状态缓存，用于 native 侧应答 Get* 查询。
#[napi(object)]
pub struct DlnaRendererStateUpdate {
    /// UPnP TransportState：STOPPED / PLAYING / PAUSED_PLAYBACK / TRANSITIONING
    pub transport_state: String,
    /// 当前 URI。
    pub current_uri: Option<String>,
    /// 当前标题。
    pub title: Option<String>,
    /// 当前位置（秒）。
    pub rel_time_secs: Option<f64>,
    /// 总时长（秒）。
    pub duration_secs: Option<f64>,
    /// 音量 0-100。
    pub volume: Option<u8>,
    /// 静音。
    pub mute: Option<bool>,
}

/// 渲染端运行状态快照。
#[napi(object)]
pub struct DlnaRendererStatus {
    /// 是否在运行。
    pub running: bool,
    /// HTTP 服务端口。
    pub port: Option<u32>,
    /// 绑定 IP。
    pub ip: Option<String>,
    /// 设备名。
    pub friendly_name: Option<String>,
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

/// 播放状态缓存（JS 推、native 读，用于应答 Get* 查询）。
#[derive(Clone, Debug)]
struct PlaybackState {
    transport_state: String,
    current_uri: String,
    title: String,
    rel_time_secs: u32,
    duration_secs: u32,
    volume: u8,
    mute: bool,
}

impl PlaybackState {
    const fn new() -> Self {
        Self {
            transport_state: String::new(), // 空串视作 STOPPED
            current_uri: String::new(),
            title: String::new(),
            rel_time_secs: 0,
            duration_secs: 0,
            volume: 100,
            mute: false,
        }
    }

    /// UPnP TransportState；空串归一化为 STOPPED。
    fn upnp_transport_state(&self) -> &str {
        if self.transport_state.is_empty() {
            "STOPPED"
        } else {
            &self.transport_state
        }
    }
}

/// 事件面字段快照（GENA NOTIFY 关心的最小集合；位置进度不进事件，由 Get* 轮询获取）。
#[derive(Clone, PartialEq, Eq)]
struct EventedFields {
    transport_state: String,
    current_uri: String,
    duration_secs: u32,
    volume: u8,
    mute: bool,
}

impl EventedFields {
    fn of(state: &PlaybackState) -> Self {
        Self {
            transport_state: state.upnp_transport_state().to_string(),
            current_uri: state.current_uri.clone(),
            duration_secs: state.duration_secs,
            volume: state.volume,
            mute: state.mute,
        }
    }
}

fn diff_evented(before: &EventedFields, after: &EventedFields) -> gena::StateDiff {
    gena::StateDiff {
        transport_changed: before.transport_state != after.transport_state
            || before.current_uri != after.current_uri
            || before.duration_secs != after.duration_secs,
        volume_changed: before.volume != after.volume || before.mute != after.mute,
    }
}

/// 统一状态提交：写 PLAYBACK → diff 事件面字段 → 触发 GENA NOTIFY。
/// 锁异常时静默跳过（与原各写入点行为一致），不影响调用方应答。
fn commit_state(update: impl FnOnce(&mut PlaybackState)) {
    let diff = {
        let Ok(mut state) = PLAYBACK.write() else {
            return;
        };
        let before = EventedFields::of(&state);
        update(&mut state);
        diff_evented(&before, &EventedFields::of(&state))
    };
    if diff.transport_changed || diff.volume_changed {
        gena::notify_state_changed(diff);
    }
}

/// 事件组装用的状态快照（gena.rs 消费者调用）。
pub(crate) fn playback_event_snapshot() -> (String, String, u32, u8, bool) {
    let state = PLAYBACK
        .read()
        .map(|s| s.clone())
        .unwrap_or_else(|_| PlaybackState::new());
    (
        state.upnp_transport_state().to_string(),
        state.current_uri,
        state.duration_secs,
        state.volume,
        state.mute,
    )
}

/// `String::new()` 是 const fn，可在静态初始化中直接构造。
static PLAYBACK: RwLock<PlaybackState> = RwLock::new(PlaybackState::new());

/// 运行中的渲染端实例（shutdown 通道 + 网络信息）。
struct RendererRuntime {
    shutdown_tx: watch::Sender<bool>,
    port: u16,
    ip: Ipv4Addr,
    friendly_name: String,
}

static RUNTIME: Mutex<Option<RendererRuntime>> = Mutex::new(None);

/// 启动/运行互斥标记：避免跨 await 持锁，也挡住并发双开。
static STARTING: AtomicBool = AtomicBool::new(false);

/// 锁中毒统一转 napi 错误。
fn poisoned<E: std::fmt::Display>(e: E) -> Error {
    Error::from_reason(format!("渲染端内部状态锁异常: {e}"))
}

/// 构造默认空事件。
fn ev(kind: &str) -> DlnaRendererEvent {
    DlnaRendererEvent {
        kind: kind.to_string(),
        ..Default::default()
    }
}

/// UPnP 时间格式 H:MM:SS。
pub(crate) fn format_upnp_time(secs: u32) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// 判断 URI 是否属于明确不支持的内容，返回给发送端展示的原因。
fn uri_unsupported(uri: &str) -> Option<&'static str> {
    let lower = uri.to_ascii_lowercase();
    // 协议白名单：仅 http/https 直链（发送端投本地路径的场景不支持）。
    let (scheme, rest) = lower.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return Some("仅支持 http/https 直链投播");
    }
    let _ = rest;
    // 扩展名检查（去掉 query 与 fragment）。
    let path = lower.split(['?', '#']).next().unwrap_or("");
    let filename = path.rsplit('/').next().unwrap_or("");
    if !filename.contains('.') {
        return None;
    }
    let ext = filename.rsplit('.').next().unwrap_or("");
    if UNSUPPORTED_EXTS.contains(&ext) {
        return Some("该内容类型暂不支持（播放列表/图片/文档等）");
    }
    None
}

/// 从 DIDL-Lite 元数据中提取 dc:title。
fn title_from_metadata(metadata: &str) -> Option<String> {
    let start = metadata.find("<dc:title>")?;
    let rest = &metadata[start + "<dc:title>".len()..];
    let end = rest.find("</dc:title>")?;
    let raw = &rest[..end];
    quick_xml::escape::unescape(raw).ok().map(|v| v.to_string())
}

// ---------------------------------------------------------------------------
// SOAP 应答构造
// ---------------------------------------------------------------------------

/// text/xml 响应。
fn xml_response(xml: String) -> Response {
    (
        axum::http::StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml,
    )
        .into_response()
}

/// 成功的 SOAP 应答信封（args 为动作返回参数原样 XML）。
fn soap_ok(action_ns: &str, action: &str, args: &str) -> Response {
    xml_response(format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
<s:Body>\r\n\
<u:{action}Response xmlns:u=\"{action_ns}\">{args}</u:{action}Response>\r\n\
</s:Body>\r\n\
</s:Envelope>"
    ))
}

/// UPnP 错误应答。
fn soap_fault(error_code: u32, description: &str) -> Response {
    xml_response(format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
<s:Body>\r\n\
<s:Fault>\r\n\
<faultcode>s:Client</faultcode>\r\n\
<faultstring>UPnPError</faultstring>\r\n\
<detail>\r\n\
<UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\r\n\
<errorCode>{error_code}</errorCode>\r\n\
<errorDescription>{description}</errorDescription>\r\n\
</UPnPError>\r\n\
</detail>\r\n\
</s:Fault>\r\n\
</s:Body>\r\n\
</s:Envelope>"
    ))
}

/// 最小化的 DIDL-Lite（仅有标题），供 GetPositionInfo 的 TrackMetaData 使用。
fn minimal_didl(title: &str) -> String {
    if title.is_empty() {
        return String::new();
    }
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"0\" restricted=\"1\">\
<dc:title>{}</dc:title>\
<upnp:class>object.item.audioItem.musicTrack</upnp:class>\
</item></DIDL-Lite>",
        quick_xml::escape::escape(title)
    )
}

/// ConnectionManager 的 SCPD 描述（上游默认返回 204 空响应，部分发送端会失败）。
const CONNECTION_MANAGER_SCPD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\r\n\
<specVersion><major>1</major><minor>0</minor></specVersion>\r\n\
<actionList>\r\n\
<action><name>GetProtocolInfo</name><argumentList>\
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>\
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>\
</argumentList></action>\r\n\
<action><name>GetCurrentConnectionIDs</name><argumentList>\
<argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>\
</argumentList></action>\r\n\
<action><name>GetCurrentConnectionInfo</name><argumentList>\
<argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>\
<argument><name>AVTID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTID</relatedStateVariable></argument>\
<argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>\
<argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>\
<argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_PeerConnectionID</relatedStateVariable></argument>\
<argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>\
<argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>\
</argumentList></action>\r\n\
</actionList>\r\n\
<serviceStateTable>\r\n\
<stateVariable sendEvents=\"yes\"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"yes\"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"yes\"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_RcsID</name><dataType>ui4</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_AVTID</name><dataType>ui4</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_PeerConnectionID</name><dataType>i4</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable>\r\n\
<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable>\r\n\
</serviceStateTable>\r\n\
</scpd>";

// ---------------------------------------------------------------------------
// 渲染服务（HTTPServer trait 实现 + ConnectionManager 扩展）
// ---------------------------------------------------------------------------

/// 渲染端服务：承载事件回调（TSFN 不可 Clone，用 Arc 共享）。
struct RendererService {
    events: Arc<ThreadsafeFunction<DlnaRendererEvent>>,
}

impl RendererService {
    /// 非阻塞推送事件到 JS。
    fn emit(&self, event: DlnaRendererEvent) {
        self.events
            .call(Ok(event), ThreadsafeFunctionCallMode::NonBlocking);
    }

    /// 读一份播放状态缓存快照（锁异常时退回默认值，避免拖垮 HTTP 线程）。
    fn snapshot() -> PlaybackState {
        PLAYBACK
            .read()
            .map(|state| state.clone())
            .unwrap_or_else(|_| PlaybackState::new())
    }

    /// ConnectionManager 控制入口（dlna-dmr 把它路由到 /Ignore 且不解析 body，
    /// 这里自行解析动作名并应答 GetProtocolInfo 等查询）。
    async fn handle_connection_manager(body: String) -> Response {
        if body.contains("GetProtocolInfo") {
            soap_ok(
                CM_NS,
                "GetProtocolInfo",
                &format!("<Source></Source><Sink>{SINK_PROTOCOLS}</Sink>"),
            )
        } else if body.contains("GetCurrentConnectionIDs") {
            soap_ok(
                CM_NS,
                "GetCurrentConnectionIDs",
                "<ConnectionIDs>0</ConnectionIDs>",
            )
        } else if body.contains("GetCurrentConnectionInfo") {
            soap_ok(
                CM_NS,
                "GetCurrentConnectionInfo",
                "<RcsID>0</RcsID><AVTID>0</AVTID><ProtocolInfo></ProtocolInfo>\
<PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID>\
<Direction>Input</Direction><Status>OK</Status>",
            )
        } else {
            soap_fault(401, "Invalid Action")
        }
    }
}

impl RendererService {
    /// AVTransport 控制入口（解析 body → 分发 → 具体类型 Response）。
    async fn av_transport_response(&self, body: String) -> Response {
        let parsed: std::result::Result<AVTransport, DeError> = AVTransport::from_str(&body);
        self.handle_av_transport(parsed).await
    }

    /// AVTransport 动作分发：写动作转发 JS，查动作按状态缓存应答。
    async fn handle_av_transport(
        &self,
        av_transport: std::result::Result<AVTransport, DeError>,
    ) -> Response {
        match av_transport {
                Ok(AVTransport::SetAVTransportURI(set)) => {
                    let uri = set.current_uri;
                    if uri.trim().is_empty() {
                        return soap_fault(402, "CurrentURI is empty");
                    }
                    if let Some(reason) = uri_unsupported(&uri) {
                        self.emit(DlnaRendererEvent {
                            uri: Some(uri),
                            reason: Some(reason.to_string()),
                            ..ev("unsupported")
                        });
                        // 714 Illegal MIME-type：发送端会立即提示投播失败。
                        return soap_fault(714, "Illegal MIME-type");
                    }
                    let title =
                        title_from_metadata(&set.current_uri_meta_data).unwrap_or_default();
                    // 走 commit_state：锁异常静默跳过，URI/状态变化会驱动 GENA NOTIFY。
                    commit_state(|state| {
                        state.current_uri = uri.clone();
                        state.title = title.clone();
                        state.transport_state = "TRANSITIONING".to_string();
                        state.rel_time_secs = 0;
                        state.duration_secs = 0;
                    });
                    self.emit(DlnaRendererEvent {
                        uri: Some(uri),
                        metadata: Some(set.current_uri_meta_data),
                        title: Some(title),
                        ..ev("set-av-transport-uri")
                    });
                    soap_ok(AVT_NS, "SetAVTransportURI", "")
                }
                Ok(AVTransport::Play(_)) => {
                    self.emit(ev("play"));
                    soap_ok(AVT_NS, "Play", "")
                }
                Ok(AVTransport::Pause(_)) => {
                    self.emit(ev("pause"));
                    soap_ok(AVT_NS, "Pause", "")
                }
                Ok(AVTransport::Stop(_)) => {
                    self.emit(ev("stop"));
                    soap_ok(AVT_NS, "Stop", "")
                }
                Ok(AVTransport::Seek(seek)) => {
                    self.emit(DlnaRendererEvent {
                        unit: Some(seek.unit.to_string()),
                        target: Some(seek.target),
                        ..ev("seek")
                    });
                    soap_ok(AVT_NS, "Seek", "")
                }
                Ok(AVTransport::Next(_)) => {
                    self.emit(ev("next"));
                    soap_ok(AVT_NS, "Next", "")
                }
                Ok(AVTransport::Previous(_)) => {
                    self.emit(ev("previous"));
                    soap_ok(AVT_NS, "Previous", "")
                }
                // 播放队列暂不支持，先接受 URI 避免发送端报错，由上层忽略。
                Ok(AVTransport::SetNextAVTransportURI(_)) => {
                    soap_ok(AVT_NS, "SetNextAVTransportURI", "")
                }
                Ok(AVTransport::GetTransportInfo(_)) => {
                    let state = Self::snapshot().upnp_transport_state().to_string();
                    soap_ok(
                        AVT_NS,
                        "GetTransportInfo",
                        &format!(
                            "<CurrentTransportState>{state}</CurrentTransportState>\
<CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>"
                        ),
                    )
                }
                Ok(AVTransport::GetPositionInfo(_)) => {
                    let snapshot = Self::snapshot();
                    soap_ok(
                        AVT_NS,
                        "GetPositionInfo",
                        &format!(
                            "<Track>1</Track>\
<TrackDuration>{}</TrackDuration>\
<TrackMetaData>{}</TrackMetaData>\
<TrackURI>{}</TrackURI>\
<RelTime>{}</RelTime>\
<AbsTime>NOT_IMPLEMENTED</AbsTime>\
<RelCount>2147483647</RelCount>\
<AbsCount>2147483647</AbsCount>",
                            format_upnp_time(snapshot.duration_secs),
                            minimal_didl(&snapshot.title),
                            quick_xml::escape::escape(&snapshot.current_uri),
                            format_upnp_time(snapshot.rel_time_secs),
                        ),
                    )
                }
                Ok(AVTransport::GetMediaInfo(_)) => {
                    let snapshot = Self::snapshot();
                    soap_ok(
                        AVT_NS,
                        "GetMediaInfo",
                        &format!(
                            "<NrTracks>1</NrTracks>\
<MediaDuration>{}</MediaDuration>\
<CurrentURI>{}</CurrentURI>\
<CurrentURIMetaData>{}</CurrentURIMetaData>\
<NextURI></NextURI><NextURIMetaData></NextURIMetaData>\
<PlayMedium>NETWORK</PlayMedium>\
<RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
<WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                            format_upnp_time(snapshot.duration_secs),
                            quick_xml::escape::escape(&snapshot.current_uri),
                            minimal_didl(&snapshot.title),
                        ),
                    )
                }
                Ok(AVTransport::GetDeviceCapabilities(_)) => soap_ok(
                    AVT_NS,
                    "GetDeviceCapabilities",
                    "<PlayMedia>NETWORK</PlayMedia>\
<RecMedia>NOT_IMPLEMENTED</RecMedia>\
<RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>",
                ),
                Ok(AVTransport::GetTransportSettings(_)) => soap_ok(
                    AVT_NS,
                    "GetTransportSettings",
                    "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>",
                ),
                Ok(AVTransport::GetCurrentTransportActions(_)) => soap_ok(
                    AVT_NS,
                    "GetCurrentTransportActions",
                    "<CurrentActions>Play,Pause,Stop,Seek</CurrentActions>",
                ),
                Err(_) => soap_fault(401, "Invalid Action"),
            }
    }

    /// RenderingControl 控制入口（解析 body → 分发）。
    async fn rendering_control_response(&self, body: String) -> Response {
        let parsed: std::result::Result<RenderingControl, DeError> =
            RenderingControl::from_str(&body);
        self.handle_rendering_control(parsed).await
    }

    /// RenderingControl 动作分发：音量/静音。
    async fn handle_rendering_control(
        &self,
        rendering_control: std::result::Result<RenderingControl, DeError>,
    ) -> Response {
        match rendering_control {
                Ok(RenderingControl::SetVolume(set)) => {
                    let volume = set.desired_volume.min(100) as u8;
                    commit_state(|state| state.volume = volume);
                    self.emit(DlnaRendererEvent {
                        value: Some(f64::from(volume)),
                        ..ev("set-volume")
                    });
                    soap_ok(RC_NS, "SetVolume", "")
                }
                Ok(RenderingControl::GetVolume(_)) => {
                    let volume = Self::snapshot().volume;
                    soap_ok(
                        RC_NS,
                        "GetVolume",
                        &format!("<CurrentVolume>{volume}</CurrentVolume>"),
                    )
                }
                Ok(RenderingControl::SetMute(set)) => {
                    commit_state(|state| state.mute = set.desired_mute);
                    self.emit(DlnaRendererEvent {
                        mute: Some(set.desired_mute),
                        ..ev("set-mute")
                    });
                    soap_ok(RC_NS, "SetMute", "")
                }
                Ok(RenderingControl::GetMute(_)) => {
                    let mute = Self::snapshot().mute;
                    soap_ok(
                        RC_NS,
                        "GetMute",
                        &format!("<CurrentMute>{}</CurrentMute>", u8::from(mute)),
                    )
                }
                Ok(RenderingControl::ListPresets(_)) => soap_ok(
                    RC_NS,
                    "ListPresets",
                    "<CurrentPresetNameList>FactoryDefaults</CurrentPresetNameList>",
                ),
                Ok(RenderingControl::SelectPreset(_)) => soap_ok(RC_NS, "SelectPreset", ""),
                Err(_) => soap_fault(401, "Invalid Action"),
            }
    }
}

impl HTTPServer for RendererService {
    /// trait 入口仅做委托：路由实际调用 inherent 方法，避免上游 edition 2024 的
    /// RPIT 捕获 &self 生命周期透传给 axum handler。
    fn post_av_transport(
        &self,
        av_transport: std::result::Result<AVTransport, DeError>,
    ) -> impl Future<Output = impl IntoResponse> + Send {
        async move { self.handle_av_transport(av_transport).await }
    }

    /// trait 入口：RenderingControl 委托。
    fn post_rendering_control(
        &self,
        rendering_control: std::result::Result<RenderingControl, DeError>,
    ) -> impl Future<Output = impl IntoResponse> + Send {
        async move { self.handle_rendering_control(rendering_control).await }
    }

    /// ConnectionManager 的 SCPD 描述（trait 中为无 self 的静态方法）。
    fn get_ignore() -> impl Future<Output = impl IntoResponse> + Send {
        async { xml_response(CONNECTION_MANAGER_SCPD.to_string()) }
    }
}

// ---------------------------------------------------------------------------
// 路由组装
// ---------------------------------------------------------------------------

/// 组装 HTTP 路由（基于 dlna-dmr 的描述/SCPD 模板 + 自定义 SOAP 处理）。
fn build_router(service: Arc<RendererService>, options: Arc<DMROptions>) -> Router {
    let svc_av = service.clone();
    let svc_rc = service.clone();
    let opts_desc = options.clone();
    let opts_alias = options;

    Router::new()
        // 设备描述：/description.xml 为主路径，/DeviceSpec 兼容别名
        .route(
            "/description.xml",
            get(move || {
                let opts = opts_desc.clone();
                async move { RendererService::get_device_spec(opts).await }
            }),
        )
        .route(
            "/DeviceSpec",
            get(move || {
                let opts = opts_alias.clone();
                async move { RendererService::get_device_spec(opts).await }
            }),
        )
        .route(
            "/RenderingControl",
            get(RendererService::get_rendering_control).post(move |body: String| async move {
                svc_rc.rendering_control_response(body).await
            }),
        )
        .route(
            "/AVTransport",
            get(RendererService::get_av_transport).post(move |body: String| async move {
                svc_av.av_transport_response(body).await
            }),
        )
        .route(
            "/Ignore",
            get(RendererService::get_ignore).post(|body: String| async move {
                RendererService::handle_connection_manager(body).await
            }),
        )
        // GENA 事件订阅：SUBSCRIBE/UNSUBSCRIBE 为自定义方法，用 any() 统一接管
        .route(
            "/AVTransport/event",
            any(move |req: Request| async move {
                gena::handle_event_request(gena::GenaService::AVTransport, req).await
            }),
        )
        .route(
            "/RenderingControl/event",
            any(move |req: Request| async move {
                gena::handle_event_request(gena::GenaService::RenderingControl, req).await
            }),
        )
        .route(
            "/Ignore/event",
            any(move |req: Request| async move {
                gena::handle_event_request(gena::GenaService::ConnectionManager, req).await
            }),
        )
        .route("/", get(|| async { "EchoMusic DLNA renderer" }))
}

// ---------------------------------------------------------------------------
// NAPI 入口
// ---------------------------------------------------------------------------

/// 启动 DLNA 渲染端。重复调用会返回错误（上层先 stop_renderer）。
#[napi]
pub async fn start_renderer(
    options: DlnaRendererOptions,
    callback: ThreadsafeFunction<DlnaRendererEvent>,
) -> Result<()> {
    // 用原子标记做互斥，避免在 async fn 中跨 await 持有非 Send 的 MutexGuard。
    if STARTING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::from_reason("DLNA 渲染端已在运行中"));
    }
    let result = start_renderer_inner(options, callback).await;
    if result.is_err() {
        STARTING.store(false, Ordering::Release);
    }
    result
}

/// 启动流程主体（STARTING 标记已占位）。
async fn start_renderer_inner(
    options: DlnaRendererOptions,
    callback: ThreadsafeFunction<DlnaRendererEvent>,
) -> Result<()> {
    ensure_native_logger();
    if options.lan_ip.trim().is_empty() {
        return Err(Error::from_reason("缺少局域网 IP，无法启动 DLNA 渲染端"));
    }
    if options.friendly_name.trim().is_empty() {
        return Err(Error::from_reason("缺少设备名，无法启动 DLNA 渲染端"));
    }
    let ip: Ipv4Addr = options.lan_ip.trim().parse().map_err(|e| {
        Error::from_reason(format!("局域网 IP 无效（{}）: {e}", options.lan_ip))
    })?;

    // 先绑定 HTTP 拿到实际端口，SSDP 的 LOCATION 才能指向正确地址。
    let listener = TcpListener::bind(SocketAddrV4::new(ip, options.http_port))
        .await
        .map_err(|e| Error::from_reason(format!("HTTP 服务端口绑定失败（{ip}）: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::from_reason(format!("读取 HTTP 端口失败: {e}")))?
        .port();

    let dmroptions = Arc::new(DMROptions {
        ip,
        ssdp_port: 1900,
        http_port: port,
        uuid: options.uuid.clone(),
        friendly_name: options.friendly_name.clone(),
        model_name: "EchoMusic".to_string(),
        model_description: "EchoMusic 桌面音乐播放器".to_string(),
        model_url: "https://github.com/hoowhoami/EchoMusic".to_string(),
        manufacturer: "EchoMusic".to_string(),
        manufacturer_url: "https://github.com/hoowhoami/EchoMusic".to_string(),
        serial_number: options.uuid.clone(),
    });

    let ssdp = Arc::new(
        SSDPServer::new(SocketAddrV4::new(ip, 1900), options.uuid.clone(), port)
            .await
            .map_err(|e| Error::from_reason(format!("SSDP 组播初始化失败（端口 1900）: {e}")))?,
    );

    let events = Arc::new(callback);
    let service = Arc::new(RendererService {
        events: events.clone(),
    });
    let app = build_router(service, dmroptions);

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // GENA：启动常驻 NOTIFY 消费者（幂等），并清掉上次会话的残留订阅。
    gena::ensure_notify_consumer();
    gena::clear_all();

    // 重置播放状态缓存，等待上层推送真实状态。
    if let Ok(mut state) = PLAYBACK.write() {
        state.transport_state = "STOPPED".to_string();
        state.current_uri = String::new();
        state.title = String::new();
        state.rel_time_secs = 0;
        state.duration_secs = 0;
    }

    // SSDP + HTTP 常驻循环：收到停机信号后发 byebye 再退出。
    let task_events = events.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = ssdp.keep_alive() => {}
            _ = ssdp.run() => {}
            result = axum::serve(listener, app) => {
                if let Err(e) = result {
                    echo_native_log::warn(&format!("[DlnaRenderer] HTTP 服务异常退出: {e}"));
                }
            }
            _ = shutdown_rx.changed() => {}
        }
        ssdp.stop().await;
        task_events.call(Ok(ev("stopped")), ThreadsafeFunctionCallMode::NonBlocking);
    });

    {
        let mut guard = RUNTIME.lock().map_err(poisoned)?;
        *guard = Some(RendererRuntime {
            shutdown_tx,
            port,
            ip,
            friendly_name: options.friendly_name.clone(),
        });
    }

    events.call(
        Ok(DlnaRendererEvent {
            port: Some(u32::from(port)),
            ip: Some(ip.to_string()),
            ..ev("started")
        }),
        ThreadsafeFunctionCallMode::NonBlocking,
    );
    echo_native_log::info(&format!(
        "[DlnaRenderer] 渲染端已启动：{ip}:{port}（SSDP 1900）"
    ));
    Ok(())
}

/// 停止渲染端（发 byebye 后退出）。
#[napi]
pub async fn stop_renderer() -> Result<()> {
    let runtime = RUNTIME.lock().map_err(poisoned)?.take();
    STARTING.store(false, Ordering::Release);
    // 订阅随会话结束全部失效，避免下次启动残留幽灵订阅。
    gena::clear_all();
    if let Some(runtime) = runtime {
        // 发送停机信号；Sender 随 runtime drop，watch 通道关闭同样会解除任务阻塞。
        let _ = runtime.shutdown_tx.send(true);
        drop(runtime);
        // 给 byebye 与 stopped 事件一点发送时间。
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// JS 推送播放状态缓存（来自 PlayerController 事件）。
#[napi]
pub fn update_renderer_state(update: DlnaRendererStateUpdate) -> Result<()> {
    // 走 commit_state：事件面字段（TransportState/URI/时长/音量/静音）变化时驱动 GENA NOTIFY。
    commit_state(|state| {
        state.transport_state = update.transport_state;
        if let Some(uri) = update.current_uri {
            state.current_uri = uri;
        }
        if let Some(title) = update.title {
            state.title = title;
        }
        if let Some(secs) = update.rel_time_secs {
            state.rel_time_secs = secs.max(0.0) as u32;
        }
        if let Some(secs) = update.duration_secs {
            state.duration_secs = secs.max(0.0) as u32;
        }
        if let Some(volume) = update.volume {
            state.volume = volume;
        }
        if let Some(mute) = update.mute {
            state.mute = mute;
        }
    });
    Ok(())
}

/// 查询渲染端运行状态。
#[napi]
pub fn get_renderer_status() -> Option<DlnaRendererStatus> {
    let guard = RUNTIME.lock().ok()?;
    let runtime = guard.as_ref()?;
    Some(DlnaRendererStatus {
        running: true,
        port: Some(u32::from(runtime.port)),
        ip: Some(runtime.ip.to_string()),
        friendly_name: Some(runtime.friendly_name.clone()),
    })
}

/// 注册日志回调（与其他 echo-* addon 一致，转发 echo_native_log 输出到主进程日志）。
#[napi]
pub fn register_log_handler(
    callback: ThreadsafeFunction<echo_native_log::NativeLogEntry>,
) -> Result<()> {
    echo_native_log::set_log_handler(callback)
        .map_err(|e| Error::from_reason(format!("注册日志回调失败: {e}")))
}
