/**
 * 解锁服务集的**跨语言双向锁** —— 前端三个数组 ↔ Rust `ServiceId::ALL` / `PENDING_CALIBRATION`。
 *
 * # 这条门补的缺口（本轮 review MED）
 *
 * 此前「双向锁」只锁了一个方向：Rust 侧 `crates/unlock/src/types.rs` 的契约测试比对的是**测试体内
 * 硬编码的前端数组副本**，而前端零测试钉 `SERVICE_IDS` / `ENABLED_SERVICE_IDS` /
 * `PENDING_CALIBRATION_SERVICE_IDS`。于是「只改前端」这个方向完全裸奔：
 * 从 `PENDING_CALIBRATION_SERVICE_IDS` 删掉 `'grok'` → 首页徽章开始渲染 grok，而后端
 * `ServiceId::ALL` 里没有它 ⇒ **一次探测都不会发**（`detector::detect_all` 只遍历 `ALL`）⇒ 徽章恒 idle，
 * 且**全部 gate 照绿**。两侧文档当时都声称「只改一侧会转红」，与事实不符。
 *
 * # 为什么是「读 Rust 源码」而非再抄一份镜像常量
 *
 * 抄镜像只是把同一个漂移面往后挪一格：改前端 + 顺手改镜像，两侧照样能分叉。本门直接把 Rust 源码当
 * 真值读进来解析，**任一侧单独改动都会转红**——这才是「双向」。读源码断言是本仓既有手法
 * （见 `src/styles/style-invariants.test.ts` 的同款理由：node 环境无 DOM，但结构是纯文本可断言的）。
 *
 * # 自曝纪律
 *
 * 解析器先自检（数组非空、变体→字面量映射齐全）。Rust 侧改名/重构导致**解析不到**时必须转红，
 * 而不是拿到空数组让后面的断言恒真——「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { SERVICE_IDS, ENABLED_SERVICE_IDS, PENDING_CALIBRATION_SERVICE_IDS } from './unlock-detection';

/** Rust 侧 SoT（上线集/停飞集 + 变体→字面量映射的唯一出处）。 */
const RUST_TYPES = readFileSync(
  fileURLToPath(new URL('../../../crates/unlock/src/types.rs', import.meta.url)),
  'utf8'
);

/**
 * `as_str()` 的 match 臂 → 变体名到字面量的映射（**不假设** `lowercase(变体) === 字面量`：
 * 那是 `rename_all` 的当前行为，不是契约；真契约就写在 `as_str()` 里）。
 */
function variantLiterals(src: string): Map<string, string> {
  const map = new Map<string, string>();
  for (const m of src.matchAll(/ServiceId::(\w+)\s*=>\s*"([a-z0-9]+)"/g)) {
    map.set(m[1], m[2]);
  }
  return map;
}

/** 取某个 `pub const <NAME>: &'static [ServiceId] = &[ … ];` 数组里的变体字面量（保序）。 */
function rustServiceConst(src: string, name: string, literals: Map<string, string>): string[] {
  const block = new RegExp(`pub const ${name}: &'static \\[ServiceId\\] = &\\[([\\s\\S]*?)\\];`).exec(src);
  expect(block, `Rust 侧 ${name} 解析失败（改名/重构了？）—— 解析不到必须转红，不得静默放行`).not.toBeNull();
  return [...block![1].matchAll(/ServiceId::(\w+)/g)].map((m) => {
    const literal = literals.get(m[1]);
    expect(literal, `ServiceId::${m[1]} 在 as_str() 里没有对应字面量 —— 前端拿到的键会恒 undefined`).toBeDefined();
    return literal!;
  });
}

const LITERALS = variantLiterals(RUST_TYPES);
const RUST_ALL = rustServiceConst(RUST_TYPES, 'ALL', LITERALS);
const RUST_PENDING = rustServiceConst(RUST_TYPES, 'PENDING_CALIBRATION', LITERALS);

describe('解析器自检（没解析到必须自曝，不得让后面的断言恒真）', () => {
  it('变体→字面量映射与两个常量数组都非空', () => {
    expect(LITERALS.size).toBeGreaterThanOrEqual(SERVICE_IDS.length);
    expect(RUST_ALL.length).toBeGreaterThan(0);
    expect(RUST_PENDING.length).toBeGreaterThan(0);
  });
});

describe('前端服务集 ↔ Rust ServiceId 双向锁', () => {
  /**
   * 上线集**逐一同序**相等。
   *
   * 牙（两个方向各一）：
   *  · 只改前端（从 `PENDING_CALIBRATION_SERVICE_IDS` 删 `'grok'`）→ `ENABLED_SERVICE_IDS` 多出 grok，
   *    Rust `ALL` 没有 → 转红（**这正是本门补的那个缺口**：徽章渲染而后端永不探测，恒 idle）。
   *  · 只改 Rust（把 `Grok` 移回 `ALL`）→ Rust 多一项 → 同样转红（后端探测了但徽章不显）。
   */
  it('ENABLED_SERVICE_IDS === ServiceId::ALL（顺序即展示序）', () => {
    expect([...ENABLED_SERVICE_IDS]).toEqual(RUST_ALL);
  });

  /** 停飞集必须两侧一致：只翻一侧 = 一边不渲染、一边不探测，两种半吊子形态都在这里转红。 */
  it('PENDING_CALIBRATION_SERVICE_IDS === ServiceId::PENDING_CALIBRATION', () => {
    expect([...PENDING_CALIBRATION_SERVICE_IDS].sort()).toEqual([...RUST_PENDING].sort());
  });

  /** 已实现全集 = 上线集 ⊎ 停飞集（无重叠、无遗漏）——防服务被**静默**弄丢成孤儿。 */
  it('SERVICE_IDS === ALL ⊎ PENDING_CALIBRATION', () => {
    expect([...SERVICE_IDS].sort()).toEqual([...RUST_ALL, ...RUST_PENDING].sort());
    expect(RUST_ALL.filter((id) => RUST_PENDING.includes(id))).toEqual([]);
  });

  /** 上线集必须是 `SERVICE_IDS` 的**子序列**（少几项可以，乱序不行——展示序即编排序）。 */
  it('ALL 是 SERVICE_IDS 的子序列', () => {
    const order = SERVICE_IDS.filter((id) => RUST_ALL.includes(id));
    expect(order).toEqual(RUST_ALL);
  });
});
