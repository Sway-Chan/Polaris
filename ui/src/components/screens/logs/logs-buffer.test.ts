/**
 * 日志水合/流式两条腿的合并语义门 —— 守 2026-07-28 复审抓出的「水合竞态丢历史」。
 *
 * 复现路径（用例 1 直接复述它）：订阅腿先送到一批 → 游标推到 N → `logs.get` 的快照全部 `_id ≤ N`
 * → 游标去重成空 → `setLogs(空)` **整体替换** ⇒ 已入列的行一并被清掉，进页历史区恒空。
 * 触发条件是「核在高频输出日志时进入日志页」，不是理论路径。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { mergeHydration, maxLogId, type LogRow } from './logs-buffer';

/** 造一行（只有 `_id` 参与合并语义，其余字段是渲染用的噪声）。 */
const row = (id?: number, message = `m${id ?? 'x'}`): LogRow =>
  ({ timestamp: '2026-07-28T00:00:00Z', level: 'info', message, ...(id === undefined ? {} : { _id: id }) }) as LogRow;

const ids = (rows: LogRow[]) => rows.map((r) => r._id);

describe('mergeHydration：水合必须合并，不得替换', () => {
  it('订阅腿先到的行不得被水合清掉（缺陷原状：整体替换 → 恒空）', () => {
    const streamed = [row(10), row(11)]; // 订阅腿先送到
    const snapshot = [row(8), row(9), row(10), row(11)]; // 后端快照（含重叠）
    expect(ids(mergeHydration(streamed, snapshot, 500))).toEqual([8, 9, 10, 11]);
  });

  it('重叠部分按 `_id` 去重（同一条日志不得出现两次）', () => {
    const merged = mergeHydration([row(3)], [row(1), row(2), row(3)], 500);
    expect(ids(merged)).toEqual([1, 2, 3]);
    expect(merged.filter((r) => r._id === 3)).toHaveLength(1);
  });

  it('交错到达后按 `_id` 升序恢复真实时序（历史在上、新行在下）', () => {
    expect(ids(mergeHydration([row(5), row(7)], [row(4), row(6)], 500))).toEqual([4, 5, 6, 7]);
  });

  it('快照全是已有行 → 返回**原数组引用**（不制造无谓重渲）', () => {
    const prev = [row(1), row(2)];
    expect(mergeHydration(prev, [row(1), row(2)], 500)).toBe(prev);
  });

  it('缓冲为空（正常首次进页）→ 就是快照本身', () => {
    expect(ids(mergeHydration([], [row(1), row(2)], 500))).toEqual([1, 2]);
  });

  it('超出上限时截尾保留最新（两条腿都进来之后仍守 MAX_BUFFER）', () => {
    const prev = [row(9), row(10)];
    const snapshot = [row(6), row(7), row(8)];
    expect(ids(mergeHydration(prev, snapshot, 3))).toEqual([8, 9, 10]);
  });

  it('空快照 → 原样返回（非 Tauri / 后端还没日志）', () => {
    const prev = [row(1)];
    expect(mergeHydration(prev, [], 500)).toBe(prev);
  });
});

describe('mergeHydration：缺 `_id` 一律放行（宁可重复，不可吞日志）', () => {
  it('快照里没有 `_id` 的行照收（旧后端 / mock）', () => {
    const merged = mergeHydration([], [row(undefined, 'a'), row(undefined, 'b')], 500);
    expect(merged.map((r) => r.message)).toEqual(['a', 'b']);
  });

  it('有 id 的行排在无 id 的行之前（无 id 视为「最新」，保持最新在下）', () => {
    const merged = mergeHydration([row(undefined, 'noid')], [row(1)], 500);
    expect(merged.map((r) => r.message)).toEqual(['m1', 'noid']);
  });
});

describe('maxLogId：游标快进（少了它快照里的新行会被流式批再收一次）', () => {
  it('取最大 `_id`', () => {
    expect(maxLogId([row(3), row(9), row(5)])).toBe(9);
  });

  it('全无 `_id` / 空数组 → null（调用方据此不动游标）', () => {
    expect(maxLogId([row(undefined)])).toBeNull();
    expect(maxLogId([])).toBeNull();
  });
});

describe('接线：LogsScreen 的水合腿真的换成了合并', () => {
  const src = readFileSync(fileURLToPath(new URL('./LogsScreen.tsx', import.meta.url)), 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:])\/\/.*$/gm, '$1');

  it('水合走 mergeHydration + maxLogId', () => {
    expect(src).toMatch(/setLogs\(\(prev\)\s*=>\s*mergeHydration\(prev,/);
    expect(src).toContain('maxLogId(snapshot)');
  });

  it('水合腿**不得**再调游标去重 `dedupe`（那正是把快照吃空的那一步）', () => {
    // `dedupe` 仍归流式腿用；这里锁的是「它不再出现在 logs.get 的 then 里」。
    const hydration = src.match(/api\.logs[\s\S]{0,600}?\}\)\s*\.catch/);
    expect(hydration, '水合腿的形状变了 —— 请同步本断言').not.toBeNull();
    expect(hydration![0]).not.toContain('dedupe');
  });

  it('监听真就绪后才水合，搜索由后端完整历史执行', () => {
    const ready = src.indexOf('api.logs.onReceivedBatchReady(onBatch)');
    const hydrate = src.indexOf('api.logs.get(subscriptionId, MAX_BUFFERED_ROWS)');
    expect(ready).toBeGreaterThan(0);
    expect(hydrate).toBeGreaterThan(ready);
    expect(src).toMatch(/api\.logs\s*\.search\(query, displayLevel, source, MAX_BUFFERED_ROWS\)/);
    expect(src).toContain('const MAX_BUFFERED_ROWS = 500');
  });
});
