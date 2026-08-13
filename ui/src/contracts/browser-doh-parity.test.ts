/**
 * 浏览器 DoH 内置起点清单的**跨语言双向锁**。
 *
 * # 为什么需要
 *
 * 这份清单存在两处：Rust 侧 `DEFAULT_BROWSER_DOH_SUFFIXES`（**真值**，生成配置用它）与前端镜像
 * （只用来在用户没编辑过清单时把内容显示出来）。两处漂了不会有任何报错 ——
 * 用户看到的是前端那份，实际下发的是 Rust 那份，**看到的和生效的不是一回事**，
 * 而这恰恰是一张用来判断「我要不要再加几条」的清单：显示错了，用户的编辑决策就是错的。
 *
 * 判据取**逐项同序相等**，不是集合相等：顺序也是内容的一部分（UI 按顺序渲染，用户按顺序核对）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { DEFAULT_BROWSER_DOH_SUFFIXES } from './browser-doh';

const RUST = '../../../crates/config-engine/src/builder/route.rs';

/** 从 Rust 源码里解析 `pub const DEFAULT_BROWSER_DOH_SUFFIXES: &[&str] = &[ … ];` 的字面量。 */
function rustList(): string[] {
  const src = readFileSync(fileURLToPath(new URL(RUST, import.meta.url)), 'utf8');
  const needle = 'pub const DEFAULT_BROWSER_DOH_SUFFIXES: &[&str] = &[';
  const i = src.indexOf(needle);
  if (i < 0) throw new Error('Rust 侧找不到 DEFAULT_BROWSER_DOH_SUFFIXES —— 改名或删了，先确认再动本门');
  const body = src.slice(i + needle.length);
  const end = body.indexOf('];');
  if (end < 0) throw new Error('Rust 常量没有收口');
  // 逐行剔注释再取引号内容：本仓那份清单是按厂商分组的，注释里就写着厂商名与子域说明。
  const stripped = body
    .slice(0, end)
    .split('\n')
    .map((l) => l.split('//')[0] ?? '')
    .join('\n');
  return [...stripped.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

describe('浏览器 DoH 内置清单 —— Rust ↔ 前端镜像', () => {
  it('解析器自检：Rust 侧解出的条目数合理（没解到必须自曝）', () => {
    const list = rustList();
    expect(list.length).toBeGreaterThanOrEqual(10);
    expect(list).toContain('dns.google');
  });

  it('两侧逐项同序相等', () => {
    expect([...DEFAULT_BROWSER_DOH_SUFFIXES]).toEqual(rustList());
  });

  it('全小写且无重复（下发前 Rust 会归一化，镜像里就该是归一化后的样子）', () => {
    const list = [...DEFAULT_BROWSER_DOH_SUFFIXES];
    expect(list).toEqual(list.map((d) => d.toLowerCase()));
    expect(new Set(list).size).toBe(list.length);
  });

  it('不含本应用自己的 DNS 上游（预填它们等于自伤）', () => {
    // doh.pub / alidns 是本应用 bootstrap 与 DoH 上游用的域名，拦掉会把自己的解析打死。
    for (const own of ['doh.pub', 'dns.alidns.com', 'alidns.com']) {
      expect(DEFAULT_BROWSER_DOH_SUFFIXES).not.toContain(own);
    }
  });
});
