/**
 * `state-facts.ts` 的行为门 —— 弹窗那两行「随行事实」拼出来的到底是什么。
 *
 * 与源码级守卫互补：源码门守得住「`main.ts` 里读没读 `receivedBytes`」，守不住「读到之后拼出来
 * 的是 `19.2 MB / 52.0 MB` 还是 `NaN undefined`」。本批第二条缺陷（`bytesText` 是死字段）的
 * 用户可见面就落在后者上。
 */
import { describe, expect, it } from 'vitest';
import { bytesText, doneSubject } from './state-facts';

describe('bytesText —— 有分母才报分母，绝不凑一个', () => {
  it('两边都有 ⇒ 已收 / 总量', () => {
    expect(bytesText(19_240_000, 52_000_000)).toBe('18.3 MB / 49.6 MB');
  });

  it('分母缺失 / 为 0 ⇒ 只报已收量（不拿已收字节当分母，那会一路 100% 再跳回去）', () => {
    expect(bytesText(19_240_000, undefined)).toBe('18.3 MB');
    expect(bytesText(19_240_000, 0)).toBe('18.3 MB');
    expect(bytesText(19_240_000, Number.NaN)).toBe('18.3 MB');
  });

  it('已收量缺失/非法 ⇒ undefined（调用方回落百分比，不渲染「— / —」这种半截话）', () => {
    expect(bytesText(undefined, 52_000_000)).toBeUndefined();
    expect(bytesText(Number.NaN, 52_000_000)).toBeUndefined();
    expect(bytesText(-1, 52_000_000)).toBeUndefined();
  });

  it('单位由 fmtBytes 给出，调用点不再拼死单位（本仓渲染过「1.20 TB GB」）', () => {
    // 跨量级：小的按 KB、大的按 GB —— 写死 MB 的实现会在这条上转红。
    expect(bytesText(512, 2 * 1024 ** 3)).toBe('512 B / 2.00 GB');
    expect(bytesText(0, 1024)).toBe('0 B / 1.00 KB');
  });
});

describe('doneSubject —— 「完成」必须说得出下的是哪一版、落在哪儿', () => {
  it('两者都有 ⇒ 版本 · 路径', () => {
    expect(doneSubject('v1.2.0', '/tmp/updates/polaris.dmg')).toBe(
      'v1.2.0 · /tmp/updates/polaris.dmg',
    );
  });

  it('只有其一 ⇒ 不留孤零零的分隔符', () => {
    expect(doneSubject('v1.2.0', undefined)).toBe('v1.2.0');
    expect(doneSubject(undefined, '/tmp/updates/polaris.dmg')).toBe('/tmp/updates/polaris.dmg');
  });

  it('全缺 / 空白 ⇒ 空串（调用方据此整行不渲染）', () => {
    expect(doneSubject(undefined, undefined)).toBe('');
    expect(doneSubject('', '   ')).toBe('');
  });
});
