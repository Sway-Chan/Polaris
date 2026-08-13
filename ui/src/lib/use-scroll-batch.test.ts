/**
 * 分批渲染的门 —— 钉住那条**真机实证**：不许用 `loading="lazy"` / IntersectionObserver。
 *
 * # 为什么这条只能靠源码门
 *
 * 真机（macOS/WKWebView）症状是：请求根本没到 scheme handler，却触发了 img 的 `onerror`
 * —— 屏幕上是一片白方块，本机（Linux/Chromium）与任何单测里都**复现不了**。
 * 换回 `loading="lazy"` 类型对、构建过、全部测试绿，只有真机会坏。
 * 判据只写在注释里挡不住下一个人「顺手加个 lazy 优化一下」，故落成门。
 *
 * # 射程（如实记账）
 *
 * 扫的是**滚动分批的两个消费方文件**里有没有出现那两个词。抓不到：
 *  · 别的文件里用 IntersectionObserver（本仓其它地方若真有正当用途，不该被这道门连坐 ——
 *    坑的成因是「top-layer `<dialog>` + 小高度滚动容器」这个组合，不是这两个 API 本身）；
 *  · 动态构造属性名（`el.setAttribute('loading', 'lazy')`）；
 *  · 第三个消费方（新文件不在 CONSUMERS 里 —— 加消费方时要手动登记，这是刻意的）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { SCROLL_BATCH_PAGE, SCROLL_BATCH_AHEAD_PX } from './use-scroll-batch';

/** 登记的消费方（新增一个就加一行 —— 手动登记是为了让「谁在分批」这件事留在灯下）。 */
const CONSUMERS = [
  '../components/dialogs/AppAddDialog.tsx',
  '../components/dialogs/RuleDialog.tsx',
] as const;

const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');

const sources = CONSUMERS.map((rel) => ({
  rel,
  src: code(readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8')),
}));

describe('滚动分批：唯一实现 + 禁 lazy/IntersectionObserver', () => {
  it('自检：两个消费方都读到了，且都真的在用这个 hook（否则下面是空跑）', () => {
    for (const { rel, src } of sources) {
      expect(src.length, `${rel} 读空了`).toBeGreaterThan(2000);
      expect(src, `${rel} 不再消费 useScrollBatch —— 是不是又抄了一份内联实现？`).toContain(
        'useScrollBatch('
      );
    }
  });

  it('消费方不得出现 `loading="lazy"` / IntersectionObserver（真机白块的元凶）', () => {
    const offenders: string[] = [];
    for (const { rel, src } of sources) {
      if (/loading\s*=\s*["'{]?\s*["']?lazy/.test(src)) offenders.push(`${rel}: loading="lazy"`);
      if (/IntersectionObserver/.test(src)) offenders.push(`${rel}: IntersectionObserver`);
    }
    expect(
      offenders,
      '真机（macOS/WKWebView）实测：在 top-layer <dialog> + 小高度滚动容器里，请求到不了 ' +
        'scheme handler 却触发 onerror ⇒ 一片白方块。IntersectionObserver 与 lazy 同族，' +
        '不拿它做替代。分批用纯 scroll 事件（lib/use-scroll-batch.ts）。'
    ).toEqual([]);
  });

  it('消费方不得再自持分批常量（第二份常量 = 两处滚动手感会漂）', () => {
    for (const { rel, src } of sources) {
      expect(src, `${rel} 又自持了一份每批数`).not.toMatch(/GALLERY_PAGE|GALLERY_LOAD_AHEAD_PX/);
    }
  });

  it('常量在合理量级（写成 0 会让分批永不推进 = 列表只剩空白）', () => {
    expect(SCROLL_BATCH_PAGE).toBeGreaterThan(10);
    expect(SCROLL_BATCH_AHEAD_PX).toBeGreaterThan(0);
  });
});
