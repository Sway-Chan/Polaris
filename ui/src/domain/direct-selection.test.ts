/**
 * 全局出口哨兵（direct / block）的判据门。
 *
 * 为什么值得单独一道门：`__block__` 的落地面横跨 ~10 处消费点（首页出口选单/状态栏/托盘三处 + 生成侧
 * 4 处 Rust），而**所有这些消费点都只认这三个纯函数**。哨兵值写错、两个哨兵撞值、或 resolveGlobalExitTag
 * 漏认 block，都会在这里先红，而不是等到真机上表现为「选了阻断但流量照走」这种沉默故障。
 */
import { describe, expect, it } from 'vitest';
import {
  BLOCK_SERVER_ID,
  DIRECT_SERVER_ID,
  isBlockSelection,
  isDirectSelection,
  isSentinelSelection,
  resolveGlobalExitTag,
} from './direct-selection';

describe('哨兵值本身', () => {
  /**
   * 两个哨兵必须互异 —— 撞值会让「阻断」静默退化成「直连」（流量照走，用户以为断了）。
   * 同时钉死字面量：Rust 侧 `dns_constants.rs` 是独立的第二份定义，跨语言靠字面量对齐，
   * 改这里不改那边 = 前端写入的出口后端不认 ⇒ 起核报 "Selected server not found"。
   */
  it('direct / block 互异且与 Rust 侧字面量一致', () => {
    expect(DIRECT_SERVER_ID).toBe('__direct__');
    expect(BLOCK_SERVER_ID).toBe('__block__');
    expect(DIRECT_SERVER_ID).not.toBe(BLOCK_SERVER_ID);
  });
});

describe('isBlockSelection', () => {
  it('只认 block 哨兵', () => {
    expect(isBlockSelection(BLOCK_SERVER_ID)).toBe(true);
    expect(isBlockSelection(DIRECT_SERVER_ID)).toBe(false);
    expect(isBlockSelection('srv-1')).toBe(false);
    expect(isBlockSelection(null)).toBe(false);
    expect(isBlockSelection(undefined)).toBe(false);
  });
});

describe('isSentinelSelection', () => {
  it('两个哨兵都认，真实节点 id / 空值都不认', () => {
    expect(isSentinelSelection(DIRECT_SERVER_ID)).toBe(true);
    expect(isSentinelSelection(BLOCK_SERVER_ID)).toBe(true);
    expect(isSentinelSelection('srv-1')).toBe(false);
    expect(isSentinelSelection('')).toBe(false);
    expect(isSentinelSelection(null)).toBe(false);
    expect(isSentinelSelection(undefined)).toBe(false);
  });

  /**
   * 不变式：isSentinelSelection ≡ isDirectSelection ∨ isBlockSelection。
   *
   * 这条挡的是「以后加第三个哨兵时只改了 isSentinelSelection、忘了它是两个具体谓词的并」——
   * 那会让 currentServer/serverConfigured（用 sentinel）与状态栏文案（用具体谓词）分叉：
   * 出口被判「已配置」但显示成「请配置服务器」。
   */
  it('恒等于两个具体谓词的并', () => {
    for (const id of [DIRECT_SERVER_ID, BLOCK_SERVER_ID, 'srv-1', '', null, undefined]) {
      expect(isSentinelSelection(id)).toBe(isDirectSelection(id) || isBlockSelection(id));
    }
  });
});

describe('resolveGlobalExitTag', () => {
  const map = new Map([['srv-1', 'HK']]);

  it('direct 哨兵 → direct 出站，不查 map', () => {
    expect(resolveGlobalExitTag(DIRECT_SERVER_ID, null)).toBe('direct');
  });

  /**
   * block 哨兵 → block 出站 tag，同样不查 map。
   *
   * 变异锁：删掉 direct-selection.ts 里 `if (isBlockSelection(...)) return 'block'` 那行 →
   * 落到 `idToTagMap?.get('__block__')` → undefined → 转红。
   */
  it('block 哨兵 → block 出站，不查 map', () => {
    expect(resolveGlobalExitTag(BLOCK_SERVER_ID, null)).toBe('block');
    expect(resolveGlobalExitTag(BLOCK_SERVER_ID, map)).toBe('block');
  });

  it('真实节点走 map；未知 id / 空值 → undefined', () => {
    expect(resolveGlobalExitTag('srv-1', map)).toBe('HK');
    expect(resolveGlobalExitTag('ghost', map)).toBeUndefined();
    expect(resolveGlobalExitTag(null, map)).toBeUndefined();
  });

  /** 两个哨兵解析出的 tag 必须互异，否则热切换会 PUT 到同一个成员、阻断与直连不可分。 */
  it('两个哨兵解析出的 tag 互异', () => {
    expect(resolveGlobalExitTag(DIRECT_SERVER_ID, null)).not.toBe(
      resolveGlobalExitTag(BLOCK_SERVER_ID, null)
    );
  });
});
