/**
 * AppAddDialog 图标设定即缓存 —— iconProxySrc 的路由纯逻辑单测（vitest, node 环境）。
 *
 * 隐私第一性验证：本地缓存 ref（自定义应用设定图标后 preset.iconUrl 持有形态）必须原样透传路由到
 * 缓存服务（读本地副本，渲染零出站），绝不被二次包裹成远端代理 ref。
 * 真机行为（scheme handler 读盘 / 下载）在 Rust 层单测覆盖（`src-tauri/src/icon_cache.rs`）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ICON_PROXY_SCHEME, iconProxySrc } from '../../domain/icon-proxy';

describe('iconProxySrc — 本地缓存 ref 与远端 URL 路由', () => {
  it('本地缓存 ref 原样透传（渲染零网络，不二次包裹）', () => {
    const cacheRef = `${ICON_PROXY_SCHEME}://c/custom-abc.png`;
    expect(iconProxySrc(cacheRef)).toBe(cacheRef);
  });

  it('已是远端代理 ref 也原样透传（防重复包裹）', () => {
    const remoteRef = `${ICON_PROXY_SCHEME}://i/https%3A%2F%2Fa.com%2Fx.png`;
    expect(iconProxySrc(remoteRef)).toBe(remoteRef);
  });

  it('http(s) URL 包成远端代理 ref（预览 / 未缓存路径）', () => {
    const url = 'https://cdn.example.com/x.png';
    expect(iconProxySrc(url)).toBe(`${ICON_PROXY_SCHEME}://i/${encodeURIComponent(url)}`);
  });

  it('空 / null / 非网络 URL 处理不变', () => {
    expect(iconProxySrc('')).toBe('');
    expect(iconProxySrc(null)).toBe('');
    expect(iconProxySrc(undefined)).toBe('');
    expect(iconProxySrc('data:image/png;base64,AAAA')).toBe('data:image/png;base64,AAAA');
  });
});

/**
 * 「刷新」接线门（源码扫描）。
 *
 * 为什么只能扫源码：刷新的效果分布在三处——后端两层缓存作废（Rust 侧已有单测）、前端换 IPC、
 * 以及给每个 `<img src>` 拼 bust 段。中间那两处是 React 组件内部的一次性接线，没有可导出的纯函数
 * 可测；而任何一处漏接，用户看到的都是同一句「点了刷新没变化」。故在源码层把这三根线钉住。
 * 变异对照：删掉 `${bustSuffix}` / 把 `loadGalleries(true)` 改回 `loadGalleries()` / 把
 * `refreshIconGalleries` 换回 `fetchIconGalleries`，任一条即转红。
 */
describe('AppAddDialog 在线图标「刷新」接线', () => {
  const SRC = readFileSync(
    fileURLToPath(new URL('./AppAddDialog.tsx', import.meta.url)),
    'utf-8',
  );

  it('自检：确实读到了组件源码（读空文件会让下面全部恒绿）', () => {
    expect(SRC.length).toBeGreaterThan(1000);
    expect(SRC).toContain('export function AppAddDialog');
  });

  it('刷新走 refreshIconGalleries（清后端两层缓存），普通加载仍走 fetchIconGalleries', () => {
    expect(SRC).toContain('api.ruleResources.refreshIconGalleries()');
    expect(SRC).toContain('api.ruleResources.fetchIconGalleries()');
    // 刷新按钮必须以 force 调用；漏了 true 就退化成一次普通加载（命中 1h TTL + 磁盘缓存 = 纹丝不动）。
    expect(SRC).toContain('loadGalleries(true)');
  });

  it('图库每个 <img> 的 src 拼上 bust 段（否则 webview 那层缓存不会重新请求）', () => {
    expect(SRC).toContain('${iconProxySrc(g.url)}${bustSuffix}');
    // bust 只在刷新过之后才拼，首次渲染保持裸 URL（不给缓存键平白加噪声）。
    expect(SRC).toContain("galleryBust > 0 ? `?r=${galleryBust}` : ''");
  });
});
