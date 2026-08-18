/**
 * CI 触发面的**对称契约门**（CI-5，2026-08-18）。
 *
 * # 背景：一个已经咬过人的假门
 *
 * 本仓的判据面**横跨 Rust 与前端两侧**：`main.rs` 遍历整棵 `ui/src`、`i18n.rs`
 * `include_str!` 五个 locale、键覆盖断言读前端源码；反向地，前端契约门（约 20 个测试文件）
 * 读 `src-tauri/` 与 `crates/` 的 Rust 源码当判据。于是两个 workflow 的 push 过滤器**都不得
 * ignore 对侧的树**——只检查一个方向等于没检查。
 *
 * `ci.yml` 此前 ignore 了 `ui/**`（注释自辩「UI 改动不碰 Rust 链」——错，见上），实证：
 * `0742de0`（G1，纯 ui/ diff）push 后 CI workflow **零 run**。CI-5 删除该条；本门钉死
 * 对称规则不回潮。
 *
 * # 判据
 *
 * - `ci.yml`（Rust 门）的 push `paths-ignore` 不得含 `ui/**`、`src-tauri/**`、`crates/**`
 *   （后两条今天是天然 absent——防御性射程：Rust 门更没理由 ignore 自己的树）；
 * - `ui.yml`（前端门）不得含 ignore `src-tauri/**` 或 `crates/**` 的条目（其头注已写明
 *   「刻意不加 paths 过滤」；若将来加了 paths-ignore，对侧树同样禁入）。
 *
 * # 这门抓不到什么
 *
 * - `paths`（白名单）形态：若将来有人把 ignore 改成**白名单**，白名单漏掉对侧树同样造假门。
 *   白名单是会漂的枚举表（CI-5 评审时已弃），真要用请连同本门一起改判据。
 * - 端到端触发验证：过滤器是声明式，真触发要等下一次纯 UI push（CI-5 登记为待验）。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

function read(workflow: string): string {
  return readFileSync(join(REPO_ROOT, '.github/workflows', workflow), 'utf8');
}

/** 取 push 段的 paths-ignore 列表条目（无该段 ⇒ 空表 = 不忽略任何路径）。 */
function pushIgnoreEntries(src: string): string[] {
  // `push:` 是 `on:` 下的缩进键，不能按列 0 找；从它起截到下一个同级触发键或 `jobs:`。
  const pushMatch = /^(\s*)push:/m.exec(src);
  if (!pushMatch) return [];
  const pushAt = pushMatch.index;
  const nextTrigger = src
    .slice(pushAt + pushMatch[0].length)
    .search(/\n\s*(pull_request|workflow_dispatch|workflow_call|schedule|jobs):/);
  const pushBlock = src.slice(
    pushAt,
    nextTrigger >= 0 ? pushAt + pushMatch[0].length + nextTrigger : src.length
  );
  const ignAt = pushBlock.indexOf('paths-ignore:');
  if (ignAt < 0) return [];
  const listBlock = pushBlock.slice(ignAt);
  return [...listBlock.matchAll(/^\s+- '([^']+)'/gm)].map((m) => m[1]);
}

describe('CI 触发面对称契约（判据面横跨两侧 ⇒ 过滤器不得 ignore 对侧树）', () => {
  it('ci.yml（Rust 门）不得 ignore ui/**（CI-5 的正主），也不得 ignore 自己的 Rust 树', () => {
    const entries = pushIgnoreEntries(read('ci.yml'));
    for (const banned of ['ui/**', 'src-tauri/**', 'crates/**']) {
      expect(
        entries.includes(banned),
        `ci.yml 的 paths-ignore 含 ${banned} —— Rust 判据面横跨 ui/（main.rs 遍历 ui/src、` +
          `i18n.rs include_str! locale），ignore 它就造出「纯 UI 改动 Rust 门零 run」的假门（CI-5 实证形态）`
      ).toBe(false);
    }
  });

  it('ui.yml（前端门）不得 ignore src-tauri/** 或 crates/**（对称的一侧）', () => {
    const entries = pushIgnoreEntries(read('ui.yml'));
    for (const banned of ['src-tauri/**', 'crates/**']) {
      expect(
        entries.includes(banned),
        `ui.yml 的 paths-ignore 含 ${banned} —— 前端契约门读 Rust 源码当判据（约 20 个测试文件），` +
          `ignore 它就造出反方向的同类假门（ui.yml 头注早写明这条禁令，这里给牙）`
      ).toBe(false);
    }
  });
});
