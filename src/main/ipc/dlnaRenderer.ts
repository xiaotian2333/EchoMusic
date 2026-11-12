/**
 * DLNA 接收端 IPC（issue #516）。
 * - dlnaRenderer:apply-config 启停/改名（渲染层设置界面调用）
 * - dlnaRenderer:get-status 查询运行状态与 addon 可用性
 */
import { ipcRegistry } from './registry';
import {
  applyDlnaRendererConfig,
  getDlnaRendererStatus,
  type DlnaRendererConfig,
} from '../mediaTransport/renderer';

export function registerDlnaRendererIpc(): void {
  ipcRegistry.registerHandler('dlnaRenderer:get-status', () => getDlnaRendererStatus());
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
