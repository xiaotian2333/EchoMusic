/**
 * DLNA 接收端（issue #516）渲染层桥。
 * - 把设置 store 的开关/设备名同步给主进程（主进程再驱动 native 渲染端）
 * - 首次启用前生成稳定的设备 UDN 并持久化，避免手机端反复见到"新设备"
 * - 主进程事件转 toast 提示
 *
 * 仅主窗口挂载（迷你窗口不重复绑定）。
 */
import { watch } from 'vue';
import { useSettingStore } from './setting';
import { useToastStore } from './toast';

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

export function useDlnaRendererBridge(): () => void {
  const settingStore = useSettingStore();

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

  const unwatch = watch(
    () => [settingStore.dlnaRendererEnabled, settingStore.dlnaRendererName] as const,
    () => apply(),
    { immediate: true },
  );

  const off = api?.onEvent((event) => {
    const payload = (event?.payload ?? {}) as { uri?: string; title?: string; reason?: string };
    const toast = useToastStore();
    const title = (payload.title || '').trim();
    switch (event?.type) {
      case 'cast-received':
        toast.show(title ? `收到投播：${title}` : '收到 DLNA 投播', 'success', 4000);
        break;
      case 'unsupported':
        toast.show(`不支持该内容：${payload.reason || '格式或协议不受支持'}`, 'warning', 5000);
        break;
      case 'cast-failed':
        toast.show(`投播播放失败${payload.reason ? `：${payload.reason}` : ''}`, 'danger', 5000);
        break;
      case 'cast-stopped':
        if (payload.reason === 'takeover') {
          toast.show('本机播放已接管，投播会话结束', 'info', 3500);
        }
        break;
      default:
        break;
    }
  });

  return () => {
    unwatch();
    off?.();
  };
}
