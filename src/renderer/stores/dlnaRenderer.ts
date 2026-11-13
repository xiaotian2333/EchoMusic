/**
 * DLNA 接收端（issue #516）渲染层桥。
 * - 把设置 store 的开关/设备名同步给主进程（主进程再驱动 native 渲染端）
 * - 首次启用前生成稳定的设备 UDN 并持久化，避免手机端反复见到"新设备"
 * - 收到投播请求时构造伪曲目走 player store 正规播放链：打断当前播放、
 *   播放栏标题/进度/SMTC 自动生效，队列保持不动
 * - 所有投放事件先过插件拦截链（capabilities.dlnaCast）：任一插件拦截
 *   即跳过宿主默认处理（播放/控制/提示），由插件全权接管
 *
 * 仅主窗口挂载（迷你窗口不重复绑定）。
 */
import { watch } from 'vue';
import { useSettingStore } from './setting';
import { usePlayerStore } from './player';
import { useToastStore } from './toast';
import { runDlnaCastInterceptorChain } from '@/plugins/dlnaCast';
import { classifyCastUri } from '../../shared/dlnaCastContent';
import type { PluginDlnaCastEvent, PluginDlnaCastEventKind } from '../../shared/plugins';
import type { ResolvedAudioSource } from './player/types';
import type { Song } from '@/models/song';

/** 兜底生成 UUID v4（file:// 下 crypto.randomUUID 可能不可用）。 */
function generateUdn(): string {
  const uuid =
    globalThis.crypto?.randomUUID?.() ??
    'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (char) => {
      const random = (Math.random() * 16) | 0;
      const value = char === 'x' ? random : (random & 0x3) | 0x8;
      return value.toString(16);
    });
  return uuid;
}

/** 解析 REL_TIME（H:MM:SS / MM:SS）为秒。 */
function parseRelTime(target: unknown): number | null {
  if (typeof target !== 'string') return null;
  const parts = target.split(':').map((part) => Number(part));
  if (parts.some((part) => !Number.isFinite(part))) return null;
  if (parts.length === 3) return Math.max(0, parts[0] * 3600 + parts[1] * 60 + parts[2]);
  if (parts.length === 2) return Math.max(0, parts[0] * 60 + parts[1]);
  return null;
}

/** 当前活跃的投播伪曲目 id（null = 无投播会话）。 */
let castTrackId: string | null = null;
/**
 * 拒绝新投播（无插件接管且内容不支持）时会主动结束旧投播会话，
 * 由此产生的 takeover 事件不弹"本机播放已接管"提示，只弹拒绝提示。
 */
let suppressTakeoverToast = false;

/** 构造投播伪曲目：走正规播放链展示标题/进度，但不进目录与歌词匹配。 */
function buildCastSong(uri: string, title: string, durationSecs: number): Song {
  const name = title || '收到 DLNA 投放';
  return {
    id: `dlna-cast-${Date.now()}`,
    title: name,
    name,
    artist: 'DLNA 投播',
    duration: Math.round(durationSecs),
    coverUrl: '',
    audioUrl: '',
    hash: `dlna-cast:${uri}`,
    mixSongId: '',
    source: 'dlna-cast',
  };
}

/** 结束投播会话：恢复自动切歌；是否停止引擎由调用方决定。 */
function clearCastSession(): void {
  if (!castTrackId) return;
  castTrackId = null;
  usePlayerStore().setAutoNextSuppressed(false);
}

export function useDlnaRendererBridge(): () => void {
  const settingStore = useSettingStore();
  const player = usePlayerStore();

  // 设备 UDN 只生成一次并随设置持久化。
  if (!settingStore.dlnaRendererUdn) {
    settingStore.dlnaRendererUdn = generateUdn();
  }

  const api = window.electron?.dlnaRenderer;

  const apply = (): void => {
    if (!api) return;
    void api
      .applyConfig({
        enabled: settingStore.dlnaRendererEnabled,
        name: settingStore.dlnaRendererName,
        udn: settingStore.dlnaRendererUdn,
      })
      .catch(() => undefined);
  };

  const unwatchConfig = watch(
    () => [settingStore.dlnaRendererEnabled, settingStore.dlnaRendererName] as const,
    () => apply(),
    { immediate: true },
  );

  const handleCastRequest = (payload: { uri?: unknown; title?: unknown }): void => {
    const uri = typeof payload.uri === 'string' ? payload.uri : '';
    if (!uri) return;
    const title = typeof payload.title === 'string' ? payload.title.trim() : '';
    // 宿主默认拒绝发生在插件拦截链之后：走到这里说明没有插件接管本条投播。
    // 播放列表/图片等内容的接管由插件拦截 set-av-transport-uri 完成。
    const verdict = classifyCastUri(uri);
    if (!verdict.supported) {
      if (castTrackId && String(player.currentTrackId ?? '') === castTrackId) {
        // UPnP 语义：SetAVTransportURI 会替换当前曲目；宿主无法播放新 URI 时结束旧会话，
        // 停掉播放并让发送端看到失败（STOPPED + ERROR_OCCURRED）。
        suppressTakeoverToast = true;
        clearCastSession();
        player.stop();
      }
      void api?.pushState({ transportState: 'STOPPED', transportStatus: 'ERROR_OCCURRED' });
      useToastStore().show(
        `不支持该内容：${verdict.reason ?? '格式或协议不受支持'}`,
        'warning',
        5000,
      );
      return;
    }
    suppressTakeoverToast = false;
    const song = buildCastSong(uri, title, 0);
    castTrackId = song.id;
    // 投播接管：播完自然暂停不推进队列；恢复时机见 clearCastSession。
    player.setAutoNextSuppressed(true);
    const playing = player.playTrack(song.id, [song], {
      preResolved: {
        url: uri,
        quality: null,
        effect: 'none',
        loudness: null,
        sourceKind: 'plugin',
      } satisfies ResolvedAudioSource,
    });
    // playTrack 成功后才会记录本地历史，同步抢占标记让投播不入听歌历史。
    player.historyLocalRecorded = true;
    void playing.catch(() => undefined);
    useToastStore().show(title ? `收到投播：${title}` : '收到 DLNA 投播', 'success', 4000);
  };

  const handleCastControl = (payload: {
    action?: unknown;
    unit?: unknown;
    target?: unknown;
    value?: unknown;
  }): void => {
    // 仅在投播曲目仍是当前曲目时响应，避免发送端控制误伤本地播放。
    if (!castTrackId || String(player.currentTrackId ?? '') !== castTrackId) return;
    const action = typeof payload.action === 'string' ? payload.action : '';
    switch (action) {
      case 'play':
        if (!player.isPlaying) void player.togglePlay();
        break;
      case 'pause':
        if (player.isPlaying) void player.togglePlay();
        break;
      case 'stop':
        clearCastSession();
        player.stop();
        break;
      case 'seek': {
        // 仅支持 REL_TIME；ABS_COUNT/TRACK_NR 无单曲语义，忽略。
        const secs = parseRelTime(payload.target);
        if (secs !== null) void player.seek(secs);
        break;
      }
      case 'set-volume': {
        const volume = typeof payload.value === 'number' ? payload.value : NaN;
        if (Number.isFinite(volume)) {
          player.setVolume(Math.min(100, Math.max(0, Math.round(volume))));
        }
        break;
      }
      default:
        break;
    }
  };

  const unwatchTakeover = watch(
    () => player.currentTrackId,
    (trackId) => {
      // 本地切歌接管：当前曲目已不是投播伪曲目，会话自然结束。
      if (castTrackId && String(trackId ?? '') !== castTrackId) {
        clearCastSession();
      }
    },
  );

  /** 构造投放事件并过插件拦截链：拦截则宿主不做任何处理，放行走 fallback。 */
  const dispatchCastEvent = (event: PluginDlnaCastEvent, fallback: () => void): void => {
    runDlnaCastInterceptorChain(event, fallback);
  };

  const controlEventKinds: ReadonlySet<string> = new Set([
    'play',
    'pause',
    'stop',
    'seek',
    'set-volume',
    'next',
    'previous',
    'set-mute',
  ]);

  const off = api?.onEvent((event) => {
    const payload = (event?.payload ?? {}) as Record<string, unknown>;
    const str = (key: string): string | undefined =>
      typeof payload[key] === 'string' ? (payload[key] as string) : undefined;
    const toast = useToastStore();
    switch (event?.type) {
      case 'cast-request':
        dispatchCastEvent(
          {
            kind: 'set-av-transport-uri',
            uri: str('uri'),
            title: str('title'),
            metadata: str('metadata'),
          },
          () => handleCastRequest(payload),
        );
        break;
      case 'cast-control': {
        const action = typeof payload.action === 'string' ? payload.action : '';
        if (!controlEventKinds.has(action)) break;
        dispatchCastEvent(
          {
            kind: action as PluginDlnaCastEventKind,
            unit: str('unit'),
            target: str('target'),
            value: typeof payload.value === 'number' ? payload.value : undefined,
            mute: typeof payload.mute === 'boolean' ? payload.mute : undefined,
          },
          () => handleCastControl(payload),
        );
        break;
      }
      case 'cast-stopped':
        // 宿主会话状态清理无条件执行（不受拦截影响）；拦截链只决定是否弹宿主提示。
        clearCastSession();
        dispatchCastEvent({ kind: 'session-end', reason: str('reason') }, () => {
          if (payload.reason === 'takeover') {
            // 拒绝投播（内容不支持）引发的主动停止不算"本地接管"，不弹接管提示。
            if (suppressTakeoverToast) {
              suppressTakeoverToast = false;
              return;
            }
            toast.show('本机播放已接管，投播会话结束', 'info', 3500);
          } else if (payload.reason === 'sender-stop') {
            toast.show('投播已停止', 'info', 3000);
          }
        });
        break;
      case 'unsupported':
        dispatchCastEvent({ kind: 'unsupported', uri: str('uri'), reason: str('reason') }, () =>
          toast.show(
            `不支持该内容：${typeof payload.reason === 'string' ? payload.reason : '格式或协议不受支持'}`,
            'warning',
            5000,
          ),
        );
        break;
      case 'cast-failed':
        // 仅剩接收端启动失败场景（投播加载失败由 store 的 notice 通道提示）。
        // 启动失败不经拦截链：与投放会话无关，宿主直接提示。
        toast.show(
          `接收端异常：${typeof payload.reason === 'string' ? payload.reason : '未知错误'}`,
          'danger',
          5000,
        );
        break;
      default:
        break;
    }
  });

  return () => {
    unwatchConfig();
    unwatchTakeover();
    off?.();
    // 组件卸载（窗口关闭）时若投播仍在进行，保留播放状态不清会话：
    // 主窗口重建后 currentTrackId 仍是伪曲目，控制监听随 off 解除。
  };
}
