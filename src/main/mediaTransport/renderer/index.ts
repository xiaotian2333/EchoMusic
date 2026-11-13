/**
 * DLNA MediaRenderer 接收端桥接层（issue #516，基于 echo-dlna-renderer 原生模块）。
 *
 * 职责划分：
 * - 协议（SSDP/SOAP/SCPD）全部在 native 侧完成
 * - 本模块把 native 事件对接到 PlayerController（loadFile/play/seek 等），
 *   并把播放器状态回推给 native 以应答发送端的 Get* 查询
 * - 音频直接解码播放；视频文件由解码器自动提取音频轨；m3u8/图片等内容的
 *   拒绝在渲染层插件拦截链之后进行（shared/dlnaCastContent 判定），无插件
 *   接管时回推 STOPPED + ERROR_OCCURRED 让发送端看到投播失败
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
    transportStatus?: string;
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
  /** 最近一次启动失败的可见原因（null/undefined = 无）。 */
  lastError?: string | null;
}

let native: NativeRendererApi | null | undefined; // undefined 表示尚未探测
let started = false;
let startingPromise: Promise<void> | null = null;
/** 最近一次启动失败的可见原因（null = 无），随状态查询暴露给设置界面。 */
let lastStartError: string | null = null;
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

    // 状态映射（发送端视角）：
    // - 引擎瞬态（loading/buffering/output-reconfig 等载荷 playing=false && paused=false）
    //   映射为 TRANSITIONING，绝不映射 STOPPED——否则发送端会把投播会话当成已中断
    //   （表现为手机端一直"正在连接"或自动重新投放）；
    // - STOPPED 只由显式路径写入（stop 动作、曲目结束、本地接管），一旦进入保持粘性，
    //   避免随后的引擎 stop 回声把状态改回 TRANSITIONING。
    const transport = playing
      ? 'PLAYING'
      : paused
        ? 'PAUSED_PLAYBACK'
        : lastTransportState === 'STOPPED'
          ? 'STOPPED'
          : 'TRANSITIONING';
    pushState(transport, {
      durationSecs: duration,
      relTimeSecs: timePos,
      volume,
    });
  };
  const timeListener = (payload?: { time?: number }) => {
    if (typeof payload?.time === 'number') {
      // 直接沿用当前会话状态，不用 || 兜底：陈旧的 STOPPED/PLAYING 兜底值会污染投播状态。
      pushState(lastTransportState, { relTimeSecs: payload.time });
    }
  };
  const durationListener = (duration?: number) => {
    if (typeof duration === 'number') {
      pushState(lastTransportState, { durationSecs: duration });
    }
  };
  const endListener = () => {
    if (castUri) {
      pushState('STOPPED');
    }
  };
  const errorListener = (payload?: { message?: string }) => {
    if (!castUri) return;
    // 播放失败的提示由渲染层 player store 的 notice 通道负责，这里只让
    // 手机端看到投播会话已停止。
    log.warn('[DlnaRenderer] 投播内容播放出错:', payload?.message ?? payload);
    pushState('STOPPED');
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
      // 新会话起步：解除 STOPPED 粘性并推进 TRANSITIONING（与 native SetAVTransportURI
      // 的提交一致，diff 无变化不产生多余 NOTIFY），避免上一曲的 STOPPED 泄漏进新会话。
      pushState('TRANSITIONING', { currentUri: uri, title: castTitle });
      // 播放切换交给渲染层 player store：构造投播伪曲目走正规播放链，
      // 打断当前播放、播放栏标题/进度/SMTC 全部自动生效，队列保持不动。
      const activeController = getController?.() ?? null;
      if (activeController) attachControllerOnce(activeController);
      publishToRenderer('cast-request', {
        uri,
        title: castTitle,
        metadata: event.metadata,
      });
      return;
    }
    case 'play': {
      publishToRenderer('cast-control', { action: 'play' });
      return;
    }
    case 'pause': {
      publishToRenderer('cast-control', { action: 'pause' });
      return;
    }
    case 'stop': {
      publishToRenderer('cast-control', { action: 'stop' });
      castUri = null;
      castTitle = '';
      pushState('STOPPED', { currentUri: '', title: '' });
      publishToRenderer('cast-stopped', { reason: 'sender-stop' });
      return;
    }
    case 'seek': {
      publishToRenderer('cast-control', {
        action: 'seek',
        unit: event.unit,
        target: event.target,
      });
      return;
    }
    case 'set-volume': {
      publishToRenderer('cast-control', { action: 'set-volume', value: event.value });
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
    lastStartError = '未找到可用的局域网 IP';
    publishToRenderer('cast-failed', { reason: lastStartError });
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
      lastStartError = null;
      currentConfig = config;
      log.info(`[DlnaRenderer] 接收端已启动（${lanIp}，设备名：${config.name}）`);
    } catch (error) {
      // 启动失败不再向上抛（IPC handler 抛错只会静默）；落到状态里并提示，UI 可重试。
      const reason = error instanceof Error ? error.message : String(error);
      lastStartError = reason;
      log.warn(`[DlnaRenderer] 接收端启动失败: ${reason}`);
      publishToRenderer('cast-failed', { reason });
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
  lastStartError = null;
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
    lastError: lastStartError,
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

/** 插件接管投播会话时回推播放状态（native 未运行时忽略）。 */
export function pushDlnaCastState(update: {
  transportState: string;
  transportStatus?: 'OK' | 'ERROR_OCCURRED';
  currentUri?: string;
  title?: string;
  relTimeSecs?: number;
  durationSecs?: number;
  volume?: number;
  mute?: boolean;
}): void {
  if (!started) return;
  try {
    native?.updateRendererState(update);
  } catch (error) {
    log.warn('[DlnaRenderer] 插件状态回推失败:', error);
  }
}
