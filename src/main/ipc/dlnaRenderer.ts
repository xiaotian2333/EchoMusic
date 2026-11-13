/**
 * DLNA 接收端 IPC（issue #516）。
 * - dlnaRenderer:apply-config 启停/改名（渲染层设置界面调用）
 * - dlnaRenderer:get-status 查询运行状态与 addon 可用性
 */
import { ipcRegistry } from './registry';
import {
  applyDlnaRendererConfig,
  getDlnaRendererStatus,
  pushDlnaCastState,
  type DlnaRendererConfig,
} from '../mediaTransport/renderer';

export function registerDlnaRendererIpc(): void {
  ipcRegistry.registerHandler('dlnaRenderer:get-status', () => getDlnaRendererStatus());
  // 插件接管投播会话时回推播放状态（capability 校验在渲染层 ctx.dlnaCast 完成）
  ipcRegistry.registerHandler('dlnaRenderer:push-state', (_event, update: unknown) => {
    if (!update || typeof update !== 'object') return;
    const raw = update as Record<string, unknown>;
    if (typeof raw.transportState !== 'string') return;
    pushDlnaCastState({
      transportState: raw.transportState,
      currentUri: typeof raw.currentUri === 'string' ? raw.currentUri : undefined,
      title: typeof raw.title === 'string' ? raw.title : undefined,
      relTimeSecs: typeof raw.relTimeSecs === 'number' ? raw.relTimeSecs : undefined,
      durationSecs: typeof raw.durationSecs === 'number' ? raw.durationSecs : undefined,
      volume: typeof raw.volume === 'number' ? raw.volume : undefined,
      mute: typeof raw.mute === 'boolean' ? raw.mute : undefined,
    });
  });
  ipcRegistry.registerHandler(
    'dlnaRenderer:apply-config',
    async (_event, config: Partial<DlnaRendererConfig> | null) => {
      return applyDlnaRendererConfig({
        enabled: Boolean(config?.enabled),
        name: typeof config?.name === 'string' ? config.name : 'EchoMusic',
        udn: typeof config?.udn === 'string' ? config.udn : '',
      });
    },
  );
}
