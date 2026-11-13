//! GENA 事件订阅（UPnP 设备架构）：接收端侧的 eventSubURL 实现。
//!
//! 职责：
//! - 订阅表：SUBSCRIBE（新建/续订）/ UNSUBSCRIBE 处理，SID / TIMEOUT / CALLBACK 解析
//! - 事件组装：AVTransport / RenderingControl 的 LastChange 与 ConnectionManager 静态属性，
//!   字段形状与发送端 genaReceiver.parseLastChange 的 knownKeys 对齐
//! - NOTIFY 投递：手写最小 HTTP NOTIFY（tokio TcpStream，单次 4s 超时），失败仅记日志
//! - 变更分发：lib.rs 的 commit_state 写状态后 diff，经 mpsc 单消费者串行投递，
//!   保证每个订阅的 SEQ 严格递增（初始事件 SEQ=0 由消费者先发出）
//!
//! 安全：CALLBACK 仅接受 http、长度上限 512；订阅数每服务上限 16；TIMEOUT 上限 1800s。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use uuid::Uuid;

/// 每服务订阅上限：防失控控制点/恶意回调占满订阅表。
const MAX_SUBSCRIBERS_PER_SERVICE: usize = 16;
/// 新建订阅缺省时长（秒）。
const DEFAULT_TIMEOUT_SEC: u32 = 300;
/// TIMEOUT 上限（秒）；infinite 按此封顶。
const MAX_TIMEOUT_SEC: u32 = 1800;
/// CALLBACK URL 长度上限。
const MAX_CALLBACK_LEN: usize = 512;
/// 单次 NOTIFY 投递超时。过长会阻塞单消费者管道、拖慢后续事件（发送端状态感知滞后）。
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

/// 承载事件订阅的 UPnP 服务。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenaService {
    AVTransport,
    RenderingControl,
    ConnectionManager,
}

impl GenaService {
    /// 设备描述里的 eventSubURL 路径（与 DeviceSpec 模板一致），供测试对照。
    #[cfg(test)]
    pub fn event_path(self) -> &'static str {
        match self {
            Self::AVTransport => "/AVTransport/event",
            Self::RenderingControl => "/RenderingControl/event",
            Self::ConnectionManager => "/Ignore/event",
        }
    }
}

/// 订阅记录。仅订阅表锁与单消费者触碰。
struct GenaSubscription {
    callback: String,
    service: GenaService,
    expires_at: Instant,
    /// 下一条 NOTIFY 的 SEQ（初始事件用 0 后置 1）。
    seq: u32,
}

/// 订阅表（SID → 记录）。SID 全局唯一，按 sid 键控即可。
/// HashMap::new 非 const，用 LazyLock 惰性初始化。
static SUBSCRIPTIONS: LazyLock<Mutex<HashMap<String, GenaSubscription>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 锁异常统一退化为直接取值（与 lib.rs 缓存模式一致，HTTP 线程不因中毒崩溃）。
fn lock_table() -> MutexGuard<'static, HashMap<String, GenaSubscription>> {
    SUBSCRIPTIONS.lock().unwrap_or_else(|p| p.into_inner())
}

/// 清除已过期订阅（投递与订阅路径上惰性触发，无需独立定时器）。
pub fn sweep_expired_now() {
    sweep_expired(&mut lock_table());
}

fn sweep_expired(table: &mut HashMap<String, GenaSubscription>) {
    let now = Instant::now();
    table.retain(|_, sub| sub.expires_at > now);
}

/// 停止渲染端时清空全部订阅（幂等）。
pub fn clear_all() {
    lock_table().clear();
}

// ---------------------------------------------------------------------------
// 订阅请求处理
// ---------------------------------------------------------------------------

/// 订阅/续订/退订的处理结果（由 axum glue 转成 HTTP 响应）。
pub struct GenaResponse {
    pub status: u16,
    pub sid: Option<String>,
    pub timeout_sec: Option<u32>,
}

impl GenaResponse {
    fn status_only(status: u16) -> Self {
        Self {
            status,
            sid: None,
            timeout_sec: None,
        }
    }
}

/// 解析 CALLBACK 头：取第一个 `<...>` 片段，仅接受 http。
fn extract_callback_url(header: &str) -> Option<String> {
    let start = header.find('<')? + 1;
    let end = header[start..].find('>')? + start;
    let url = header[start..end].trim();
    if url.is_empty() || url.len() > MAX_CALLBACK_LEN {
        return None;
    }
    // https 无法手写 TLS 投递；GENA 控制点回调均为 http。
    if !url.to_ascii_lowercase().starts_with("http://") {
        return None;
    }
    parse_http_url(url).map(|_| url.to_string())
}

/// 手写解析 http URL（不引入 url crate）：返回 (host, port, path)。
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains([' ', '\\', '?', '#']) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            if h.is_empty()
                || p.is_empty()
                || !p.chars().all(|c| c.is_ascii_digit())
                || authority.matches(':').count() > 1
            {
                return None;
            }
            (h, p.parse::<u16>().ok()?)
        }
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

/// 解析 TIMEOUT 头（Second-N / Second-infinite，大小写不敏感），缺省/非法回退 300。
fn parse_timeout_sec(header: Option<&str>) -> u32 {
    let Some(raw) = header else {
        return DEFAULT_TIMEOUT_SEC;
    };
    let lower = raw.trim().to_ascii_lowercase();
    let value = lower.strip_prefix("second-").unwrap_or(&lower);
    if value == "infinite" {
        return MAX_TIMEOUT_SEC;
    }
    match value.parse::<u32>() {
        Ok(v) if v > 0 => v.min(MAX_TIMEOUT_SEC),
        _ => DEFAULT_TIMEOUT_SEC,
    }
}

/// SUBSCRIBE 入口：带 SID 视为续订，否则新建。
pub fn handle_subscribe(
    service: GenaService,
    sid_header: Option<&str>,
    nt_header: Option<&str>,
    callback_header: Option<&str>,
    timeout_header: Option<&str>,
) -> GenaResponse {
    let timeout_sec = parse_timeout_sec(timeout_header);
    let sid = sid_header
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let Some(sid) = sid else {
        // 新建订阅：规范要求 CALLBACK 与 NT 均在场。
        let Some(callback) = callback_header.and_then(extract_callback_url) else {
            return GenaResponse::status_only(412);
        };
        if !nt_header
            .map(|nt| nt.trim().eq_ignore_ascii_case("upnp:event"))
            .unwrap_or(false)
        {
            return GenaResponse::status_only(400);
        }
        let new_sid = format!("uuid:{}", Uuid::new_v4());
        {
            let mut table = lock_table();
            sweep_expired(&mut table);
            if table
                .values()
                .filter(|s| s.service == service)
                .count()
                >= MAX_SUBSCRIBERS_PER_SERVICE
            {
                return GenaResponse::status_only(503);
            }
            table.insert(
                new_sid.clone(),
                GenaSubscription {
                    callback,
                    service,
                    expires_at: Instant::now() + Duration::from_secs(u64::from(timeout_sec)),
                    seq: 0,
                },
            );
        }
        // 初始事件经同一消费者队列发出，保证先于该订阅的后续变更事件。
        queue_initial(&new_sid, service);
        return GenaResponse {
            status: 201,
            sid: Some(new_sid),
            timeout_sec: Some(timeout_sec),
        };
    };
    // 续订：刷新到期时间。
    let mut table = lock_table();
    match table.get_mut(&sid) {
        Some(sub) => {
            sub.expires_at = Instant::now() + Duration::from_secs(u64::from(timeout_sec));
            GenaResponse {
                status: 200,
                sid: Some(sid),
                timeout_sec: Some(timeout_sec),
            }
        }
        None => GenaResponse::status_only(412),
    }
}

/// UNSUBSCRIBE 入口。
pub fn handle_unsubscribe(sid_header: Option<&str>) -> GenaResponse {
    let Some(sid) = sid_header.map(str::trim).filter(|s| !s.is_empty()) else {
        return GenaResponse::status_only(400);
    };
    let mut table = lock_table();
    if table.remove(sid).is_some() {
        GenaResponse::status_only(200)
    } else {
        GenaResponse::status_only(412)
    }
}

// ---------------------------------------------------------------------------
// 事件组装
// ---------------------------------------------------------------------------

/// AVTransport LastChange 的内嵌 Event XML（属性值已转义）。
/// 字段与发送端 genaReceiver.parseLastChange 的 knownKeys 对齐。
pub fn build_avt_lastchange(
    transport_state: &str,
    transport_status: &str,
    current_uri: &str,
    duration_secs: u32,
) -> String {
    let uri = quick_xml::escape::escape(current_uri);
    let status = quick_xml::escape::escape(transport_status);
    format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">\
<InstanceID val=\"0\">\
<TransportState val=\"{transport_state}\"/>\
<TransportStatus val=\"{status}\"/>\
<AVTransportURI val=\"{uri}\"/>\
<CurrentTrackURI val=\"{uri}\"/>\
<CurrentTrackDuration val=\"{}\"/>\
</InstanceID></Event>",
        crate::format_upnp_time(duration_secs)
    )
}

/// RenderingControl LastChange 的内嵌 Event XML（Volume/Mute，Master 声道）。
pub fn build_rc_lastchange(volume: u8, mute: bool) -> String {
    format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\">\
<InstanceID val=\"0\">\
<Volume channel=\"Master\" val=\"{volume}\"/>\
<Mute channel=\"Master\" val=\"{}\"/>\
</InstanceID></Event>",
        u8::from(mute)
    )
}

/// ConnectionManager 事件属性：静态 Sink 协议列表等。
pub fn build_cm_properties() -> String {
    format!(
        "<SourceProtocolInfo></SourceProtocolInfo>\
<SinkProtocolInfo>{}</SinkProtocolInfo>\
<CurrentConnectionIDs>0</CurrentConnectionIDs>",
        crate::SINK_PROTOCOLS
    )
}

/// LastChange 属性：内嵌 Event XML 需整体转义为字符串值。
fn lastchange_property(event_xml: &str) -> String {
    format!(
        "<LastChange>{}</LastChange>",
        quick_xml::escape::escape(event_xml)
    )
}

/// propertyset 信封（GENA NOTIFY body）。
pub fn wrap_propertyset(properties_xml: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n\
<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
<e:property>{properties_xml}</e:property>\
</e:propertyset>"
    )
}

/// 组装服务当前状态的事件 body（初始事件与变更事件共用）。
fn event_body(service: GenaService) -> String {
    let (transport_state, transport_status, current_uri, duration_secs, volume, mute) =
        crate::playback_event_snapshot();
    let properties = match service {
        GenaService::AVTransport => lastchange_property(&build_avt_lastchange(
            &transport_state,
            &transport_status,
            &current_uri,
            duration_secs,
        )),
        GenaService::RenderingControl => {
            lastchange_property(&build_rc_lastchange(volume, mute))
        }
        GenaService::ConnectionManager => build_cm_properties(),
    };
    wrap_propertyset(&properties)
}

// ---------------------------------------------------------------------------
// 状态变更分发（lib.rs commit_state → 通道 → 单消费者）
// ---------------------------------------------------------------------------

/// 事件面字段 diff（lib.rs commit_state 计算后传入）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateDiff {
    /// TransportState / CurrentURI / 时长变化 → 通知 AVTransport 订阅者。
    pub transport_changed: bool,
    /// Volume / Mute 变化 → 通知 RenderingControl 订阅者。
    pub volume_changed: bool,
}

/// NOTIFY 消息。单消费者串行处理，保证每订阅 SEQ 顺序。
enum NotifyMsg {
    /// 新订阅的初始事件（SEQ=0），先于该订阅后续变更。
    Initial {
        sid: String,
        service: GenaService,
    },
    State(StateDiff),
}

static NOTIFY_TX: OnceLock<mpsc::UnboundedSender<NotifyMsg>> = OnceLock::new();

/// 启动常驻消费者（每进程恰好一次；napi tokio runtime 全局共享，跨 start/stop 复用）。
pub fn ensure_notify_consumer() {
    let (tx, rx) = mpsc::unbounded_channel();
    if NOTIFY_TX.set(tx).is_ok() {
        tokio::spawn(consumer_loop(rx));
    }
}

/// 状态变更入口（同步安全：try send 无阻塞）。
pub fn notify_state_changed(diff: StateDiff) {
    if let Some(tx) = NOTIFY_TX.get() {
        let _ = tx.send(NotifyMsg::State(diff));
    }
}

/// 初始事件入队。
fn queue_initial(sid: &str, service: GenaService) {
    if let Some(tx) = NOTIFY_TX.get() {
        let _ = tx.send(NotifyMsg::Initial {
            sid: sid.to_string(),
            service,
        });
    }
}

async fn consumer_loop(mut rx: mpsc::UnboundedReceiver<NotifyMsg>) {
    while let Some(msg) = rx.recv().await {
        sweep_expired_now();
        match msg {
            NotifyMsg::Initial { sid, service } => {
                // 初始事件必须 SEQ=0；等待投递完成后再放行后续事件（严格顺序）。
                let callback = {
                    let table = lock_table();
                    table.get(&sid).map(|s| s.callback.clone())
                };
                if let Some(callback) = callback {
                    deliver_notify(&callback, &sid, 0, &event_body(service)).await;
                    if let Some(sub) = lock_table().get_mut(&sid) {
                        if sub.seq == 0 {
                            sub.seq = 1;
                        }
                    }
                }
            }
            NotifyMsg::State(diff) => {
                if diff.transport_changed {
                    broadcast(GenaService::AVTransport).await;
                }
                if diff.volume_changed {
                    broadcast(GenaService::RenderingControl).await;
                }
            }
        }
    }
}

/// 向某服务的全部订阅者投递事件：取出目标并推进 SEQ，逐个 spawn 投递任务。
/// 单订阅的事件顺序由消费者串行调度近似保证；慢回调不阻塞状态管道。
async fn broadcast(service: GenaService) {
    let body = event_body(service);
    let mut targets: Vec<(String, String, u32)> = Vec::new();
    {
        let mut table = lock_table();
        sweep_expired(&mut table);
        for (sid, sub) in table.iter_mut().filter(|(_, s)| s.service == service) {
            targets.push((sid.clone(), sub.callback.clone(), sub.seq));
            sub.seq = sub.seq.wrapping_add(1);
        }
    }
    for (sid, callback, seq) in targets {
        let body = body.clone();
        tokio::spawn(async move {
            deliver_notify(&callback, &sid, seq, &body).await;
        });
    }
}

/// 投递单条 NOTIFY：手写最小 HTTP/1.1 请求，读响应即收（不解析状态码）。
async fn deliver_notify(callback: &str, sid: &str, seq: u32, body: &str) {
    let Some((host, port, path)) = parse_http_url(callback) else {
        echo_native_log::warn(format!(
            "[DlnaRenderer][GENA] CALLBACK 不可解析，放弃投递: {callback}"
        ));
        return;
    };
    let request = format!(
        "NOTIFY {path} HTTP/1.1\r\n\
HOST: {host}:{port}\r\n\
CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
NT: upnp:event\r\n\
NTS: upnp:propchange\r\n\
SID: {sid}\r\n\
SEQ: {seq}\r\n\
CONTENT-LENGTH: {}\r\n\
CONNECTION: close\r\n\
\r\n{body}",
        body.len()
    );
    let deliver = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok(mut stream) = TcpStream::connect((host.as_str(), port)).await else {
            echo_native_log::debug(format!(
                "[DlnaRenderer][GENA] NOTIFY 连接失败: {host}:{port}"
            ));
            return;
        };
        if let Err(e) = stream.write_all(request.as_bytes()).await {
            echo_native_log::debug(format!("[DlnaRenderer][GENA] NOTIFY 写入失败: {e}"));
            return;
        }
        // 读到响应即认为送达；控制点按 SID 自行解析。
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).await;
    };
    if tokio::time::timeout(NOTIFY_TIMEOUT, deliver).await.is_err() {
        echo_native_log::debug(format!(
            "[DlnaRenderer][GENA] NOTIFY 投递超时: {host}:{port}"
        ));
    }
}

// ---------------------------------------------------------------------------
// axum 路由 glue
// ---------------------------------------------------------------------------

/// eventSubURL 请求入口：SUBSCRIBE / UNSUBSCRIBE / 其余 405。
pub async fn handle_event_request(service: GenaService, req: Request) -> Response {
    match req.method().as_str() {
        "SUBSCRIBE" => {
            let headers = req.headers();
            let sid = header_str(headers, "sid");
            let nt = header_str(headers, "nt");
            let callback = header_str(headers, "callback");
            let timeout = header_str(headers, "timeout");
            let result = handle_subscribe(
                service,
                sid.as_deref(),
                nt.as_deref(),
                callback.as_deref(),
                timeout.as_deref(),
            );
            to_response(result)
        }
        "UNSUBSCRIBE" => {
            let sid = header_str(req.headers(), "sid");
            to_response(handle_unsubscribe(sid.as_deref()))
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn header_str(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(String::from)
}

/// 处理结果转 HTTP 响应（SID / TIMEOUT 头按规范回填）。
fn to_response(result: GenaResponse) -> Response {
    let mut builder = Response::builder().status(
        StatusCode::from_u16(result.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    );
    if let Some(sid) = &result.sid {
        builder = builder.header("SID", sid);
    }
    if let Some(sec) = result.timeout_sec {
        builder = builder.header("TIMEOUT", format!("Second-{sec}"));
    }
    builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AVT: GenaService = GenaService::AVTransport;
    const CM: GenaService = GenaService::ConnectionManager;

    /// eventSubURL 路径必须与设备描述模板一致，否则控制点订阅打到不存在的端点。
    #[test]
    fn event_paths_match_device_spec_template() {
        let template = include_str!("../vendor/dlna-dmr/src/template/DeviceSpec.tmpl.xml");
        for service in [
            GenaService::AVTransport,
            GenaService::RenderingControl,
            GenaService::ConnectionManager,
        ] {
            assert!(
                template.contains(&format!("<eventSubURL>{}</eventSubURL>", service.event_path())),
                "模板缺少 {} 的 eventSubURL",
                service.event_path()
            );
        }
    }

    #[test]
    fn parses_valid_http_callback() {
        let url = extract_callback_url("<http://192.168.1.5:3000/gena/avt/token>");
        assert_eq!(
            url.as_deref(),
            Some("http://192.168.1.5:3000/gena/avt/token")
        );
        let (host, port, path) = parse_http_url(url.as_deref().unwrap()).unwrap();
        assert_eq!(host, "192.168.1.5");
        assert_eq!(port, 3000);
        assert_eq!(path, "/gena/avt/token");
    }

    #[test]
    fn rejects_invalid_callback() {
        // 非 http scheme / 无尖括号 / 超长 / 空主机名一律拒绝。
        assert!(extract_callback_url("<https://secure.example/x>").is_none());
        assert!(extract_callback_url("http://no-brackets.example/x").is_none());
        let long_url = format!("<http://host/{}>", "a".repeat(600));
        assert!(extract_callback_url(&long_url).is_none());
        assert!(extract_callback_url("<http:///path>").is_none());
        // 缺省端口回退 80；路径可省略。
        let (host, port, path) = parse_http_url("http://player.local").unwrap();
        assert_eq!(host, "player.local");
        assert_eq!(port, 80);
        assert_eq!(path, "/");
    }

    #[test]
    fn parses_timeout_variants() {
        assert_eq!(parse_timeout_sec(Some("Second-1800")), 1800);
        assert_eq!(parse_timeout_sec(Some("second-infinite")), 1800);
        assert_eq!(parse_timeout_sec(Some("SECOND-30")), 30);
        assert_eq!(parse_timeout_sec(Some("Second-99999")), 1800); // 封顶
        assert_eq!(parse_timeout_sec(Some("Second-0")), 300); // 非法回退
        assert_eq!(parse_timeout_sec(Some("junk")), 300);
        assert_eq!(parse_timeout_sec(None), 300);
    }

    #[test]
    fn builds_avt_lastchange_with_escaping() {
        let xml = build_avt_lastchange("STOPPED", "ERROR_OCCURRED", "http://host/a&b.mp3", 205);
        assert!(xml.starts_with("<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">"));
        assert!(xml.contains("<TransportState val=\"STOPPED\"/>"));
        assert!(xml.contains("<TransportStatus val=\"ERROR_OCCURRED\"/>"));
        // & 必须转义，避免内嵌 XML 破坏结构。
        assert!(xml.contains("http://host/a&amp;b.mp3"));
        assert!(xml.contains("<CurrentTrackDuration val=\"0:03:25\"/>"));
    }

    #[test]
    fn builds_rc_lastchange_and_propertyset() {
        let event = build_rc_lastchange(37, true);
        assert!(event.contains("<Volume channel=\"Master\" val=\"37\"/>"));
        assert!(event.contains("<Mute channel=\"Master\" val=\"1\"/>"));
        let body = wrap_propertyset(&lastchange_property(&event));
        assert!(body.starts_with("<?xml version=\"1.0\"?>"));
        assert!(body.contains("<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">"));
        // 内嵌 Event XML 被整体转义。
        assert!(body.contains("&lt;Event xmlns="));
        assert!(body.contains("&lt;Volume channel="));
    }

    #[test]
    fn builds_cm_static_properties() {
        let body = wrap_propertyset(&build_cm_properties());
        assert!(body.contains("<SinkProtocolInfo>http-get:*:audio/mpeg:*,"));
        assert!(body.contains("<CurrentConnectionIDs>0</CurrentConnectionIDs>"));
    }

    /// 生命周期：新建 → 续订 → 退订；SID 未知/缺失按规范回 412/400。
    #[test]
    fn subscribe_lifecycle() {
        let created = handle_subscribe(
            AVT,
            None,
            Some("upnp:event"),
            Some("<http://127.0.0.1:1/lifecycle>"),
            Some("Second-60"),
        );
        assert_eq!(created.status, 201);
        let sid = created.sid.expect("新建订阅必须返回 SID");
        assert!(sid.starts_with("uuid:"));
        assert_eq!(created.timeout_sec, Some(60));

        // 续订。
        let renewed = handle_subscribe(AVT, Some(&sid), None, None, Some("Second-120"));
        assert_eq!(renewed.status, 200);
        assert_eq!(renewed.timeout_sec, Some(120));
        {
            let table = lock_table();
            assert!(table.contains_key(&sid));
        }

        // 缺 SID 的 UNSUBSCRIBE → 400；未知 SID → 412；已知 → 200。
        assert_eq!(handle_unsubscribe(None).status, 400);
        assert_eq!(handle_unsubscribe(Some("uuid:not-exist")).status, 412);
        assert_eq!(handle_unsubscribe(Some(&sid)).status, 200);
        assert_eq!(handle_unsubscribe(Some(&sid)).status, 412); // 已移除
    }

    #[test]
    fn rejects_subscribe_without_nt_or_callback() {
        assert_eq!(
            handle_subscribe(AVT, None, None, Some("<http://127.0.0.1:1/a>"), None).status,
            400 // 缺 NT
        );
        assert_eq!(
            handle_subscribe(AVT, None, Some("upnp:event"), None, None).status,
            412 // 缺 CALLBACK
        );
    }

    /// 新 SID 必须互不相同（uuid v4）。
    #[test]
    fn generates_unique_sids() {
        let a = handle_subscribe(AVT, None, Some("upnp:event"), Some("<http://127.0.0.1:1/u1>"), None);
        let b = handle_subscribe(AVT, None, Some("upnp:event"), Some("<http://127.0.0.1:1/u2>"), None);
        assert_ne!(a.sid, b.sid);
        handle_unsubscribe(a.sid.as_deref());
        handle_unsubscribe(b.sid.as_deref());
    }

    /// 每服务订阅上限：CM 服务在测试中独占使用，避免与并行用例互扰。
    #[test]
    fn enforces_per_service_capacity() {
        {
            let mut table = lock_table();
            table.retain(|_, s| s.service != CM);
        }
        for _ in 0..MAX_SUBSCRIBERS_PER_SERVICE {
            let r = handle_subscribe(
                CM,
                None,
                Some("upnp:event"),
                Some("<http://127.0.0.1:1/cap>"),
                Some("Second-30"),
            );
            assert_eq!(r.status, 201);
        }
        let overflow = handle_subscribe(
            CM,
            None,
            Some("upnp:event"),
            Some("<http://127.0.0.1:1/cap>"),
            Some("Second-30"),
        );
        assert_eq!(overflow.status, 503);
        // 清理，不影响其他用例。
        lock_table().retain(|_, s| s.service != CM);
    }
}
