import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';

const root = fileURLToPath(new URL('..', import.meta.url));
const bundleDir = mkdtempSync(join(tmpdir(), 'echo-dlna-content-bundle-'));
const outfile = join(bundleDir, 'bundle.mjs');
await build({
  stdin: {
    contents: `
      export { classifyCastUri } from ${JSON.stringify(join(root, 'src/shared/dlnaCastContent.ts'))};
    `,
    resolveDir: root,
    sourcefile: 'dlna-cast-content-entry.ts',
    loader: 'ts',
  },
  bundle: true,
  format: 'esm',
  platform: 'node',
  outfile,
});
const { classifyCastUri } = await import(pathToFileURL(outfile).href);

test('支持常规音频/视频直链', () => {
  assert.equal(classifyCastUri('http://media.example/song.mp3').supported, true);
  assert.equal(classifyCastUri('https://cdn.example/a.flac?token=x#frag').supported, true);
  assert.equal(classifyCastUri('HTTP://HOST/Track.M4A').supported, true);
  // 无扩展名的直链不判失败（格式未知交给引擎探测）。
  assert.equal(classifyCastUri('http://media.example/stream').supported, true);
  assert.equal(classifyCastUri('https://media.example/get?id=1&fmt=mp3').supported, true);
});

test('拒绝播放列表/流媒体清单', () => {
  for (const uri of [
    'http://media.example/playlist.m3u8',
    'http://media.example/list.M3U',
    'http://media.example/manifest.mpd',
    'http://media.example/list.pls',
    'http://media.example/stream.smil',
  ]) {
    const verdict = classifyCastUri(uri);
    assert.equal(verdict.supported, false, uri);
    assert.match(verdict.reason, /播放列表/);
  }
});

test('拒绝图片/字幕/文档类内容', () => {
  for (const uri of [
    'http://media.example/cover.jpg',
    'http://media.example/photo.PNG?size=large',
    'http://media.example/subs.srt',
    'http://media.example/doc.pdf',
    'http://media.example/page.html#anchor',
  ]) {
    assert.equal(classifyCastUri(uri).supported, false, uri);
  }
});

test('拒绝非 http(s) 协议与非直链', () => {
  assert.equal(classifyCastUri('file:///home/user/a.mp3').supported, false);
  assert.equal(classifyCastUri('rtsp://media.example/stream').supported, false);
  assert.equal(classifyCastUri('/local/path/a.flac').supported, false);
  assert.equal(classifyCastUri('C:\\music\\a.flac').supported, false);
  assert.equal(classifyCastUri('').supported, false);
  assert.equal(classifyCastUri('   ').supported, false);
});

test('扩展名相似但不在黑名单的直链放行', () => {
  // xml 在黑名单（文档），但 xmls/其他相似后缀不误伤；m4a/mp4 正常放行。
  assert.equal(classifyCastUri('http://media.example/a.m4a').supported, true);
  assert.equal(classifyCastUri('http://media.example/a.opus').supported, true);
});
