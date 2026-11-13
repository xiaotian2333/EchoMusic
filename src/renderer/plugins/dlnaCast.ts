import type {
  PluginDlnaCastEvent,
  PluginDlnaCastHandler,
  PluginDlnaCastNext,
  PluginDlnaCastRegisterOptions,
} from '../../shared/plugins';

/**
 * DLNA 投放事件拦截器注册表与洋葱链执行器。
 *
 * 本模块是纯工具层：不 import 任何 plugins 运行时代码，不感知插件概念之外的逻辑。
 * 插件侧的能力校验与异常隔离（fail-open 上报）在 plugins/runtime/dlnaCast.ts 完成
 * 后再把安全 handler 注册到这里。模式与 utils/serverInterceptors.ts 保持一致。
 */

interface Registration {
  /** 单调递增的注册序号，同优先级时稳定排序 */
  seq: number;
  pluginId: string;
  name?: string;
  priority: number;
  handler: PluginDlnaCastHandler;
}

let seqSeed = 0;
let registrations: Registration[] = [];

/**
 * 注册一个投放事件拦截器。返回注销函数。
 * 注册表采用不可变更新：在途分发持有的数组快照不受后续注册/卸载影响。
 */
export const registerDlnaCastInterceptor = (
  pluginId: string,
  handler: PluginDlnaCastHandler,
  options: PluginDlnaCastRegisterOptions = {},
): (() => void) => {
  const item: Registration = {
    seq: ++seqSeed,
    pluginId,
    name: options.name,
    priority: options.priority ?? 0,
    handler,
  };
  registrations = [...registrations, item];
  return () => {
    registrations = registrations.filter((registration) => registration !== item);
  };
};

export const hasDlnaCastInterceptors = (): boolean => registrations.length > 0;

/** deactivate 兜底清理：按插件 id 扫除全部残留拦截器。 */
export const removeDlnaCastInterceptorsByPlugin = (pluginId: string): void => {
  registrations = registrations.filter((registration) => registration.pluginId !== pluginId);
};

/** 优先级降序、注册序号升序：高优先级先收到事件，同优先级先注册先收 */
const sortedChain = (): Registration[] =>
  registrations.slice().sort((a, b) => b.priority - a.priority || a.seq - b.seq);

/**
 * 洋葱链执行投放事件分发：
 * - handler 调用 next() → 放行，继续内层（幂等，重复调用无效）；
 * - 未调用 next 且返回 true → 拦截，链终止（fallback 不执行）；
 * - 未调用 next 且返回非 true / 抛出异常 → 放行（fail-open 兜底，异常上报在 ctx 层）；
 * - 链走完仍未拦截 → 调用 fallback（宿主默认处理）。
 *
 * 异步 handler 会等待其结果；next 已被调用时以 next 为准（先到先得）。
 */
export const runDlnaCastInterceptorChain = (
  event: PluginDlnaCastEvent,
  fallback: () => void,
): void => {
  const chain = sortedChain();

  const dispatch = (depth: number): void => {
    if (depth >= chain.length) {
      fallback();
      return;
    }
    const current = chain[depth];
    // settled：本层已经放行或短路，之后的返回值/迟到的 next 一律无效
    let settled = false;
    const next: PluginDlnaCastNext = () => {
      if (settled) return;
      settled = true;
      dispatch(depth + 1);
    };
    let result: boolean | void | Promise<boolean | void>;
    try {
      result = current.handler(event, next);
    } catch {
      // ctx 层的 wrapSafe 通常已兜住异常；这里兜底放行保证宿主不断链
      next();
      return;
    }
    if (result instanceof Promise) {
      result
        .then((intercepted) => {
          if (settled) return;
          if (intercepted === true) {
            settled = true; // 拦截：链终止
            return;
          }
          next();
        })
        .catch(() => {
          if (!settled) next();
        });
      return;
    }
    if (settled) return;
    if (result === true) {
      settled = true; // 拦截：链终止
      return;
    }
    next();
  };

  dispatch(0);
};
