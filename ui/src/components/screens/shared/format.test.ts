/**
 * latDotClass 单测（vitest，node 环境）——`.nm-latdot`（首页节点选单）色阶。
 *
 * 覆盖原型 latClass（L3030）的语义转写：undefined=未测→none / null=超时→dead2 /
 * 数字按阈值分档（<80 fast，<150 mid，<300 slow2，其余 dead2）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fmtBytes, fmtDuration, fmtRate, latDotClass, latLevel } from './format';

describe('latDotClass', () => {
  it('未测（undefined）→ lat-none', () => {
    expect(latDotClass(undefined)).toBe('lat-none');
  });

  it('超时（null）→ lat-dead2', () => {
    expect(latDotClass(null)).toBe('lat-dead2');
  });

  it('分档边界：<80 fast / <150 mid / <300 slow2 / 其余 dead2', () => {
    expect(latDotClass(0)).toBe('lat-fast');
    expect(latDotClass(79)).toBe('lat-fast');
    expect(latDotClass(80)).toBe('lat-mid');
    expect(latDotClass(149)).toBe('lat-mid');
    expect(latDotClass(150)).toBe('lat-slow2');
    expect(latDotClass(299)).toBe('lat-slow2');
    expect(latDotClass(300)).toBe('lat-dead2');
    expect(latDotClass(9999)).toBe('lat-dead2');
  });
});

describe('latLevel', () => {
  it('区分未测与测速失败：undefined 无状态，null/负数是失败态', () => {
    expect(latLevel(undefined)).toBe('none');
    expect(latLevel(null)).toBe('dead');
    expect(latLevel(-1)).toBe('dead');
    expect(latLevel(Number.NaN)).toBe('dead');
  });

  it('有效延迟继续使用既有四档阈值', () => {
    expect(latLevel(0)).toBe('fast');
    expect(latLevel(79)).toBe('fast');
    expect(latLevel(80)).toBe('mid');
    expect(latLevel(150)).toBe('slow');
    expect(latLevel(300)).toBe('dead');
  });
});

/**
 * fmtBytes 的**单位自带**契约 + 全仓调用点不变量。
 *
 * 2026-07-28 真机可见缺陷：`SubInfoBar` 写成 `{fmtBytes(used)} / {fmtBytes(total)} GB` ——
 * fmtBytes 已按量级返回 `B/KB/MB/GB/TB`，调用点再拼一个死单位就渲染成「1.20 TB GB」。
 * 这里钉的是**根因**（调用点不得自带单位），不是那一处字面量：换个文件再犯同样错照样被抓。
 */
describe('fmtBytes —— 单位由函数给出，调用点不得再拼一个', () => {
  it('按量级返回自带单位的串', () => {
    expect(fmtBytes(512)).toBe('512 B');
    expect(fmtBytes(1024)).toBe('1.00 KB');
    expect(fmtBytes(1024 ** 3)).toBe('1.00 GB');
    expect(fmtBytes(1.2 * 1024 ** 4)).toBe('1.20 TB');
    expect(fmtBytes(undefined)).toBe('—');
  });

  /**
   * 小数位上限 2 —— 连接页速率列传进来的是 `(Δbytes/Δt)` 浮点，`< 1024` 那档原先裸拼 `${n} B`
   * 就渲染成 `833.3333333333334 B/s`（2026-07-29 真机）。整数仍须打印成整数，不补 `.00`。
   */
  it('B 档最多两位小数，整数不补零', () => {
    expect(fmtBytes(2500 / 3)).toBe('833.33 B');
    expect(fmtBytes(1023)).toBe('1023 B');
    expect(fmtBytes(0)).toBe('0 B');
    expect(fmtRate(2500 / 3)).toBe('833.33 B/s');
  });

  it('全仓无「fmtBytes(…) 后紧跟裸单位」的调用点', () => {
    const srcRoot = fileURLToPath(new URL('../../..', import.meta.url));
    // `fmtBytes(<无嵌套括号>)` 之后（可跨一个 JSX `}` 与空白）紧跟 B|KB|MB|GB|TB 词。
    const DOUBLE_UNIT = /fmtBytes\([^()]*\)\s*\}?\s*(?:B|KB|MB|GB|TB)\b/;
    const offenders: string[] = [];
    const walk = (dir: string) => {
      for (const e of readdirSync(dir, { withFileTypes: true })) {
        if (e.name === 'node_modules' || e.name === 'dist') continue;
        const p = join(dir, e.name);
        if (e.isDirectory()) walk(p);
        else if (/\.tsx?$/.test(e.name) && !/\.test\.tsx?$/.test(e.name)) {
          if (DOUBLE_UNIT.test(readFileSync(p, 'utf8'))) offenders.push(p);
        }
      }
    };
    walk(srcRoot);
    expect(offenders).toEqual([]);
  });
});

// ── M8：时长首档 5 秒量化（新生连接的时长格不再每秒换串）────────────────────────
describe('M8 fmtDuration 首档 5 秒量化', () => {
  it('<60s 显示整 5 秒档（floor 不超前）', () => {
    expect(fmtDuration(0)).toBe('0s');
    expect(fmtDuration(4)).toBe('0s');
    expect(fmtDuration(7)).toBe('5s');
    expect(fmtDuration(12)).toBe('10s');
    expect(fmtDuration(59)).toBe('55s');
  });

  it('同档内文本稳定（7s/9s 同显 5s —— 每秒泵被压到 1/5）', () => {
    expect(fmtDuration(7)).toBe(fmtDuration(9));
    expect(fmtDuration(23)).toBe(fmtDuration(24));
  });

  it('≥60s 语义不变（分钟级）', () => {
    expect(fmtDuration(60)).toBe('1m');
    expect(fmtDuration(125)).toBe('2m');
    expect(fmtDuration(3661)).toBe('1h 1m');
  });
});
