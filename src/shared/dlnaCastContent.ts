/**
 * DLNA 投播内容类型判定（原 echo-dlna-renderer native 侧 uri_unsupported 逻辑的 TS 移植）。
 *
 * 语义：native 的 SetAVTransportURI 不再预拒绝任何内容（插件接管需要先收到事件），
 * 事件经渲染层插件拦截链（capabilities.dlnaCast）分发后——
 * - 插件拦截：插件全权接管（可自行解析 m3u8 取音轨、展示图片等）；
 * - 无插件拦截：宿主 fallback 用本模块判定，不支持的内容回报
 *   STOPPED + ERROR_OCCURRED，让发送端看到投播失败。
 */

/** 明确不支持的内容扩展名：播放列表/流媒体清单、图片、字幕、文档等。 */
export const UNSUPPORTED_CAST_EXTS: ReadonlySet<string> = new Set([
  'm3u8',
  'm3u',
  'mpd',
  'smil',
  'asx',
  'pls',
  'jpg',
  'jpeg',
  'png',
  'gif',
  'webp',
  'bmp',
  'svg',
  'srt',
  'ass',
  'ssa',
  'txt',
  'pdf',
  'html',
  'htm',
  'xml',
]);

export interface CastUriVerdict {
  /** 是否可由宿主默认流程直接播放。 */
  supported: boolean;
  /** 不支持时给用户/发送端看的原因。 */
  reason?: string;
}

/**
 * 判定投播 URI 是否属于宿主默认流程不支持的内容。
 * 插件接管（拦截 set-av-transport-uri）时不会走到该判定。
 */
export function classifyCastUri(uri: string): CastUriVerdict {
  const lower = uri.trim().toLowerCase();
  if (!lower) {
    return { supported: false, reason: '投播地址为空' };
  }
  // 协议白名单：仅 http/https 直链（发送端投本地路径的场景宿主默认不支持）。
  const schemeMatch = /^([a-z][a-z0-9+.-]*):\/\//.exec(lower);
  if (!schemeMatch) {
    return { supported: false, reason: '仅支持 http/https 直链投播' };
  }
  if (schemeMatch[1] !== 'http' && schemeMatch[1] !== 'https') {
    return { supported: false, reason: '仅支持 http/https 直链投播' };
  }
  // 扩展名检查（去掉 query 与 fragment）。
  const path = lower.split(/[?#]/)[0] ?? '';
  const filename = path.split('/').pop() ?? '';
  if (!filename.includes('.')) {
    return { supported: true };
  }
  const ext = filename.split('.').pop() ?? '';
  if (UNSUPPORTED_CAST_EXTS.has(ext)) {
    return { supported: false, reason: '该内容类型暂不支持（播放列表/图片/文档等）' };
  }
  return { supported: true };
}
