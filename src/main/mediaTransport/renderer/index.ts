/**
 * DLNA MediaRenderer 接收端桥接层（issue #516，基于 echo-dlna-renderer 原生模块）。
 *
 * 职责划分：
 * - 协议（SSDP/SOAP/SCPD）全部在 native 侧完成
 * - 本模块把 native 事件对接到 PlayerController（loadFile/play/seek 等），
 *   并把播放器状态回推给 native 以应答发送端的 Get* 查询
 * - 音频直接解码播放；视频文件由解码器自动提取音频轨；m3u8/图片等在
 *   native 侧直接 SOAP fault 714 并推送 unsupported 事件
 */
import { app } from 'electron';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import log from '../../logger';
import { getMainWindow } from '../../window';
import { pickLanIpv4 } from '../mediaServer';
import { registerNativeLogHandler } from '../../native/logging';
import { getPersistedRendererSettings } from '../../storage/persistedStores';
import type { PlayerController } from '../../player/controller';

const nativeRequire = createRequire(path.join(process.cwd(), 'package.json'));

/** echo-dlna-renderer 事件载荷（与 addon 的 index.d.ts 对应）。 */
interface NativeRendererEvent {
  kind: string;
  uri?: string;
  metadata?: string;
  title?: string;
  unit?: string;
  target?: string;
  value?: number;
  mute?: boolean;
  port?: number;
  ip?: string;
  reason?: string;
}

/** echo-dlna-renderer addon 暴露的接口。 */
interface NativeRendererApi {
  startRenderer(
    options: {
      friendlyName: string;
      uuid: string;
      lanIp: string;
      httpPort: number;
    },
    callback: (error: Error | null, event: NativeRendererEvent) => void,
  ): Promise<void>;
  stopRenderer(): Promise<void>;
  updateRendererState(update: {
    transportState: string;
    currentUri?: string;
    title?: string;
    relTimeSecs?: number;
    durationSecs?: number;
    volume?: number;
    mute?: boolean;
  }): void;
  getRendererStatus(): {
    running: boolean;
    port?: number;
    ip?: string;
    friendlyName?: string;
  } | null;
  registerLogHandler?(
    callback: (error: Error | null, entry: { level: string; message: string }) => void,
  ): void;
}

export interface DlnaRendererConfig {
  enabled: boolean;
  name: string;
  udn: string;
}

export interface DlnaRendererStatusView {
  supported: boolean;
  running: boolean;
  port?: number;
  ip?: string;
  friendlyName?: string;
}

let native: NativeRendererApi | null | undefined; // undefined 表示尚未探测
let started = false;
let startingPromise: Promise<void> | null = null;
let currentConfig: DlnaRendererConfig | null = null;
let getController: (() => PlayerController | null) | null = null;
/** 当前投播中的 URI（null = 无投播会话），用于识别本地切歌接管。 */
let castUri: string | null = null;
let castTitle = '';
/** 上次推送给 native 的状态，避免重复推送。 */
let lastTransportState = '';
/** 注销 thunk：PlayerController.on 返回 this，需自行记录引用后用 off() 注销。 */
let detachFns: Array<() => void> = [];

function addonPath(name: string): string | null {
  const candidates = app.isPackaged
    ? [path.join(process.resourcesPath, 'native', `${name}.node`)]
    : [
        path.join(__dirname, `../../native/${name}/${name}.node`),
        path.join(process.cwd(), `native/${name}/${name}.node`),
      ];
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? null;
}

function loadRendererAddon(): NativeRendererApi | null {
  if (native !== undefined) return native;
  const candidate = addonPath('echo-dlna-renderer');
  if (!candidate) {
    log.info('[DlnaRenderer] echo-dlna-renderer addon 未找到，接收端不可用');
    native = null;
    return native;
  }
  try {
    const raw = nativeRequire(candidate) as Record<string, unknown>;
    if (
      typeof raw.startRenderer !== 'function' ||
      typeof raw.stopRenderer !== 'function' ||
      typeof raw.updateRendererState !== 'function'
    ) {
      log.warn('[DlnaRenderer] addon 缺少必要函数，接收端不可用');
      native = null;
      return native;
    }
    native = raw as unknown as NativeRendererApi;
    registerNativeLogHandler(native, 'DlnaRenderer');
    log.info('[DlnaRenderer] echo-dlna-renderer 加载成功');
  } catch (error) {
    log.warn(
      `[DlnaRenderer] 未能加载 echo-dlna-renderer: ${error instanceof Error ? error.message : String(error)}`,
    );
    native = null;
  }
  return native;
}

/** 向渲染层推送提示（渲染层转 toast）。 */
function publishToRenderer(type: string, payload?: Record<string, unknown>): void {
  getMainWindow()?.webContents.send('dlnaRenderer:event', { type, payload });
}

/** 解析 REL_TIME（H:MM:SS / MM:SS）为秒。 */
function parseRelTime(target: string): number | null {
  const parts = target.split(':').map((part) => Number(part));
  if (parts.some((part) => !Number.isFinite(part))) return null;
  if (parts.length === 3) return Math.max(0, parts[0] * 3600 + parts[1] * 60 + parts[2]);
  if (parts.length === 2) return Math.max(0, parts[0] * 60 + parts[1]);
  return null;
}

/** 推送状态到 native 缓存（native 用它应答发送端的 Get* 查询）。 */
function pushState(
  transportState: string,
  patch?: {
    currentUri?: string;
    title?: string;
    relTimeSecs?: number;
    durationSecs?: number;
    volume?: number;
    mute?: boolean;
  },
): void {
  if (transportState === lastTransportState && !patch) return;
  lastTransportState = transportState;
  native?.updateRendererState({ transportState, ...patch });
}

/** 订阅 PlayerController 事件，桥接为 native 状态缓存与接管检测。 */
function attachController(controller: PlayerController): void {
  detachController();

  const stateListener = (state: {
    playing?: boolean;
    paused?: boolean;
    duration?: number;
    timePos?: number;
    volume?: number;
    path?: string;
  }) => {
    const playing = Boolean(state?.playing);
    const paused = Boolean(state?.paused);
    const duration = typeof state?.duration === 'number' ? state.duration : undefined;
    const timePos = typeof state?.timePos === 'number' ? state.timePos : undefined;
    const volume = typeof state?.volume === 'number' ? state.volume : undefined;

    // 本地切歌接管：投播会话进行中且当前路径已不是投播 URI。
    if (castUri && typeof state?.path === 'string' && state.path !== castUri) {
      log.info('[DlnaRenderer] 本地播放接管，投播会话结束');
      castUri = null;
      castTitle = '';
      pushState('STOPPED', { currentUri: '', title: '' });
      publishToRenderer('cast-stopped', { reason: 'takeover' });
      return;
    }

    const transport = playing ? 'PLAYING' : paused ? 'PAUSED_PLAYBACK' : 'STOPPED';
    pushState(transport, {
      durationSecs: duration,
      relTimeSecs: timePos,
      volume,
    });
  };
  const timeListener = (payload?: { time?: number }) => {
    if (typeof payload?.time === 'number') {
      pushState(lastTransportState || 'PLAYING', { relTimeSecs: payload.time });
    }
  };
  const durationListener = (duration?: number) => {
    if (typeof duration === 'number') {
      pushState(lastTransportState || 'STOPPED', { durationSecs: duration });
    }
  };
  const endListener = () => {
    if (castUri) {
      pushState('STOPPED');
    }
  };
  const errorListener = (payload?: { message?: string }) => {
    if (!castUri) return;
    log.warn('[DlnaRenderer] 投播内容播放出错:', payload?.message ?? payload);
    pushState('STOPPED');
    publishToRenderer('cast-failed', {
      uri: castUri,
      title: castTitle,
      reason: payload?.message,
    });
  };

  controller.on('state-change', stateListener);
  controller.on('time-update', timeListener);
  controller.on('duration-change', durationListener);
  controller.on('playback-end', endListener);
  controller.on('error', errorListener);
  detachFns = [
    () => controller.off('state-change', stateListener),
    () => controller.off('time-update', timeListener),
    () => controller.off('duration-change', durationListener),
    () => controller.off('playback-end', endListener),
    () => controller.off('error', errorListener),
  ];
}

function detachController(): void {
  detachFns.forEach((detach) => {
    try {
      detach();
    } catch {
      // 忽略注销时的异常
    }
  });
  detachFns = [];
}

/** 处理 native 事件。 */
async function handleNativeEvent(event: NativeRendererEvent): Promise<void> {
  const controller = getController?.() ?? null;
  switch (event.kind) {
    case 'started': {
      log.info(
        `[DlnaRenderer] 渲染端就绪 http://${event.ip}:${event.port}（设备名：${currentConfig?.name}）`,
      );
      return;
    }
    case 'stopped': {
      castUri = null;
      castTitle = '';
      return;
    }
    case 'unsupported': {
      publishToRenderer('unsupported', {
        uri: event.uri,
        reason: event.reason,
      });
      return;
    }
    case 'set-av-transport-uri': {
      const uri = event.uri ?? '';
      castUri = uri;
      castTitle = event.title ?? '';
      if (!controller) {
        log.warn('[DlnaRenderer] 收到投播但播放引擎不可用');
        pushState('STOPPED', { currentUri: uri, title: castTitle });
        publishToRenderer('cast-failed', { uri, title: castTitle, reason: '播放引擎不可用' });
        return;
      }
      attachControllerOnce(controller);
      try {
        // SetAVTransportURI 后自动起播；发送端随后发的 Play 调用是幂等确认。
        await controller.loadFile(uri);
        await controller.play();
        pushState('PLAYING', { currentUri: uri, title: castTitle });
        publishToRenderer('cast-received', { uri, title: castTitle });
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        log.warn(`[DlnaRenderer] 投播加载失败（${uri}）: ${reason}`);
        pushState('STOPPED', { currentUri: uri, title: castTitle });
        publishToRenderer('cast-failed', { uri, title: castTitle, reason });
      }
      return;
    }
    case 'play': {
      await controller?.play().catch(() => undefined);
      return;
    }
    case 'pause': {
      controller?.pause();
      return;
    }
    case 'stop': {
      controller?.stop();
      castUri = null;
      castTitle = '';
      pushState('STOPPED', { currentUri: '', title: '' });
      publishToRenderer('cast-stopped', { reason: 'sender-stop' });
      return;
    }
    case 'seek': {
      if (!controller) return;
      if (event.unit === 'REL_TIME') {
        const secs = parseRelTime(event.target ?? '');
        if (secs !== null) await controller.seek(secs).catch(() => undefined);
        return;
      }
      // ABS_COUNT（单曲目恒为 1）与 TRACK_NR（无队列语义）阶段一不做处理。
      const secs = Number(event.target);
      if (event.unit === 'ABS_COUNT' && Number.isFinite(secs) && secs > 0) {
        await controller.seek(secs).catch(() => undefined);
      }
      return;
    }
    case 'set-volume': {
      if (typeof event.value === 'number' && controller) {
        controller.setVolume(Math.min(100, Math.max(0, event.value)));
      }
      return;
    }
    case 'set-mute':
    case 'next':
    case 'previous':
      // 阶段一不处理：静音缓存由 native 维护；队列语义暂不支持。
      return;
    default:
      log.debug(`[DlnaRenderer] 忽略未知事件: ${event.kind}`);
  }
}

/** 控制器事件监听只在首次投播时挂载（本地播放期间无需桥接开销）。 */
function attachControllerOnce(controller: PlayerController): void {
  if (detachFns.length > 0) return;
  attachController(controller);
}

async function startRenderer(config: DlnaRendererConfig): Promise<void> {
  const nativeApi = loadRendererAddon();
  if (!nativeApi) {
    publishToRenderer('unsupported', { reason: '本机缺少 echo-dlna-renderer 原生模块' });
    return;
  }
  if (startingPromise) return startingPromise;
  const lanIp = pickLanIpv4();
  if (!lanIp) {
    log.warn('[DlnaRenderer] 未找到可用的局域网 IPv4，接收端启动失败');
    publishToRenderer('cast-failed', { reason: '未找到可用的局域网 IP' });
    return;
  }
  startingPromise = (async () => {
    try {
      await nativeApi.startRenderer(
        {
          friendlyName: config.name,
          uuid: config.udn,
          lanIp,
          httpPort: 0,
        },
        (_error, event) => {
          // 事件回调串行处理；异常只记日志，不拖垮 native 循环。
          handleNativeEvent(event).catch((err) => {
            log.warn(`[DlnaRenderer] 事件处理异常: ${err instanceof Error ? err.message : err}`);
          });
        },
      );
      started = true;
      currentConfig = config;
      log.info(`[DlnaRenderer] 接收端已启动（${lanIp}，设备名：${config.name}）`);
    } finally {
      startingPromise = null;
    }
  })();
  return startingPromise;
}

async function stopRendererInternal(): Promise<void> {
  const nativeApi = native ?? null;
  if (!started && !startingPromise) return;
  if (startingPromise) await startingPromise.catch(() => undefined);
  try {
    await nativeApi?.stopRenderer();
  } catch (error) {
    log.warn(
      `[DlnaRenderer] 停止接收端失败: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  started = false;
  castUri = null;
  castTitle = '';
  lastTransportState = '';
  detachController();
}

/** 配置变更入口：按需启动/停止/重启。 */
async function applyConfig(config: DlnaRendererConfig): Promise<DlnaRendererStatusView> {
  const normalized: DlnaRendererConfig = {
    enabled: Boolean(config?.enabled),
    name: (typeof config?.name === 'string' && config.name.trim()) || 'EchoMusic',
    udn: (typeof config?.udn === 'string' && config.udn.replace(/^uuid:/, '').trim()) || '',
  };
  if (!normalized.enabled) {
    if (started) await stopRendererInternal();
    currentConfig = normalized;
    return statusView();
  }
  if (!normalized.udn) {
    log.warn('[DlnaRenderer] 缺少设备 UDN，无法启用接收端');
    return statusView();
  }
  const needRestart =
    started &&
    currentConfig !== null &&
    (currentConfig.name !== normalized.name || currentConfig.udn !== normalized.udn);
  if (needRestart) {
    await stopRendererInternal();
  }
  if (!started) {
    await startRenderer(normalized);
  } else {
    currentConfig = normalized;
  }
  return statusView();
}

function statusView(): DlnaRendererStatusView {
  const nativeStatus = started ? (native?.getRendererStatus() ?? null) : null;
  return {
    supported: loadRendererAddon() !== null,
    running: started && Boolean(nativeStatus?.running),
    port: nativeStatus?.port,
    ip: nativeStatus?.ip,
    friendlyName: nativeStatus?.friendlyName,
  };
}

/**
 * 初始化：持有 PlayerController 引用，并按持久化设置自动启动。
 * 在 initPlayer 之后调用。
 */
export function initDlnaRenderer(getControllerRef: () => PlayerController | null): void {
  getController = getControllerRef;
  loadRendererAddon();
  const saved = getPersistedRendererSettings();
  const enabled = saved?.dlnaRendererEnabled === true;
  const udn = typeof saved?.dlnaRendererUdn === 'string' ? saved.dlnaRendererUdn : '';
  const name = typeof saved?.dlnaRendererName === 'string' ? saved.dlnaRendererName : 'EchoMusic';
  if (enabled && udn) {
    applyConfig({ enabled, name, udn }).catch((error) => {
      log.warn('[DlnaRenderer] 按持久化配置自动启动失败:', error);
    });
  }
}

/** 应用新配置（IPC 入口）。 */
export async function applyDlnaRendererConfig(
  config: DlnaRendererConfig,
): Promise<DlnaRendererStatusView> {
  return applyConfig(config);
}

/** 查询状态（IPC 入口）。 */
export function getDlnaRendererStatus(): DlnaRendererStatusView {
  return statusView();
}

/** 退出清理（before-quit）。 */
export async function shutdownDlnaRenderer(): Promise<void> {
  await stopRendererInternal();
}
