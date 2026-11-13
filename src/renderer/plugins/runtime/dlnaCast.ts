import type {
  EchoPluginDescriptor,
  PluginDlnaCastHandler,
  PluginDlnaCastRegisterOptions,
  PluginDlnaCastStateUpdate,
} from '../../../shared/plugins';
import { registerDlnaCastInterceptor } from '../dlnaCast';
import { runWithRequestOrigin as markRequestOrigin } from '@/utils/serverInterceptors';

interface DlnaCastApiDeps {
  addDisposable: (dispose: () => void) => () => void;
  reportPluginRuntimeError: (pluginId: string, error: unknown, source?: string) => void;
  /** 插件接管会话时回推播放状态给发送端（主进程直通 native 缓存） */
  pushState: (state: PluginDlnaCastStateUpdate) => void;
}

export interface PluginDlnaCastApi {
  /**
   * 注册投放事件拦截器。返回注销函数，并随插件 deactivate 自动清理。
   * 事件数据与拦截/放行语义见 shared/plugins.ts 的 PluginDlnaCastHandler 注释。
   */
  register: (handler: PluginDlnaCastHandler, options?: PluginDlnaCastRegisterOptions) => () => void;
  /**
   * 接管会话时回推播放状态给发送端（手机端轮询 GetTransportInfo/GetPositionInfo 读取）。
   * 宿主默认流程的状态回推由宿主自己维护，插件仅在拦截接管后调用。
   */
  updateState: (state: PluginDlnaCastStateUpdate) => void;
}

/**
 * fail-open 包装：插件处理器任何异常都不影响宿主投放流程。
 * 抛错/拒绝时上报并返回 undefined（工具层据此放行继续链）。
 */
const wrapSafe =
  (
    pluginId: string,
    handler: PluginDlnaCastHandler,
    reportError: DlnaCastApiDeps['reportPluginRuntimeError'],
  ): PluginDlnaCastHandler =>
  async (event, next) => {
    try {
      // 标记插件来源：处理器内经 ctx.kugou / request 发起的宿主 API 调用
      // 按既有惯例绕过 serverIntercept 拦截链（防递归）。
      return await markRequestOrigin({ type: 'plugin', pluginId }, () => handler(event, next));
    } catch (error) {
      reportError(pluginId, error, 'DLNA 投放接管处理器');
      return undefined;
    }
  };

const requireCapability = (descriptor: EchoPluginDescriptor): void => {
  if (descriptor.manifest.capabilities?.dlnaCast !== true) {
    throw new Error('插件未声明投放接管能力（capabilities.dlnaCast）');
  }
};

export const createDlnaCastApi = (
  descriptor: EchoPluginDescriptor,
  deps: DlnaCastApiDeps,
): PluginDlnaCastApi => ({
  register: (handler, options = {}) => {
    requireCapability(descriptor);
    if (typeof handler !== 'function') {
      throw new Error('DLNA 投放接管处理器必须是函数');
    }
    const safeHandler = wrapSafe(descriptor.id, handler, deps.reportPluginRuntimeError);
    const dispose = registerDlnaCastInterceptor(descriptor.id, safeHandler, options);
    return deps.addDisposable(dispose);
  },
  updateState: (state) => {
    requireCapability(descriptor);
    if (!state || typeof state !== 'object' || typeof state.transportState !== 'string') {
      throw new Error('updateState 需要 transportState 字段');
    }
    deps.pushState({
      transportState: state.transportState,
      transportStatus:
        state.transportStatus === 'OK' || state.transportStatus === 'ERROR_OCCURRED'
          ? state.transportStatus
          : undefined,
      currentUri: typeof state.currentUri === 'string' ? state.currentUri : undefined,
      title: typeof state.title === 'string' ? state.title : undefined,
      relTimeSecs: typeof state.relTimeSecs === 'number' ? state.relTimeSecs : undefined,
      durationSecs: typeof state.durationSecs === 'number' ? state.durationSecs : undefined,
      volume: typeof state.volume === 'number' ? state.volume : undefined,
      mute: typeof state.mute === 'boolean' ? state.mute : undefined,
    });
  },
});
