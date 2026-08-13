/**
 * 规则复制载荷（G5）的行为门 + 调用点接线守卫。
 *
 * 两条不变式都会**静默**出错：带 id 的载荷被后端当成「已存在」，同形 remarks 让用户分不出副本。
 * 行为门测载荷本身；接线段防「函数在、按钮没接」——本仓已栽过多次（见
 * `nodes/nodes-speedtest-wiring.test.ts` 头注）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { duplicateRulePayload } from './rule-duplicate';
import type { Rule } from '@/contracts/types';

const base: Rule = {
  id: 'r-1',
  type: 'domainSuffix',
  values: ['example.com'],
  action: 'proxy',
  enabled: true,
  targetServerId: 'srv-9',
  bypassFakeIP: true,
};

describe('duplicateRulePayload', () => {
  /** 变异对照：把解构剔除换成直接 `{...rule}` → 本条转红（载荷带着原 id 走 rules.add）。 */
  it('去掉 id', () => {
    expect('id' in duplicateRulePayload(base, '副本')).toBe(false);
  });

  /**
   * 除 id / remarks 外**逐字段照搬** —— 复制丢字段（如漏掉 targetServerId）会让副本静默换一条出口。
   *
   * 变异对照：改成只挑几个字段构造 → 本条转红。
   */
  it('其余字段逐字段照搬', () => {
    const out = duplicateRulePayload(base, '副本');
    expect(out).toMatchObject({
      type: 'domainSuffix',
      values: ['example.com'],
      action: 'proxy',
      enabled: true,
      targetServerId: 'srv-9',
      bypassFakeIP: true,
    });
  });

  /** 变异对照：删掉后缀拼接 → 两条转红（列表里两行完全同形，用户分不出副本）。 */
  it('remarks 有值时追加后缀，无值时就是后缀本身', () => {
    expect(duplicateRulePayload({ ...base, remarks: '公司内网' }, '副本').remarks).toBe(
      '公司内网 (副本)',
    );
    expect(duplicateRulePayload(base, '副本').remarks).toBe('副本');
  });

  /** 原规则不得被就地改动（`delete` 实现就会踩这条）。 */
  it('不改动入参', () => {
    const input = { ...base, remarks: '原备注' };
    duplicateRulePayload(input, '副本');
    expect(input).toEqual({ ...base, remarks: '原备注' });
  });
});

describe('调用点接线', () => {
  const code = (src: string): string =>
    src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
  const SCREEN = code(
    readFileSync(
      fileURLToPath(new URL('../components/screens/rules/RulesScreen.tsx', import.meta.url)),
      'utf8',
    ),
  );
  const ITEM = code(
    readFileSync(
      fileURLToPath(new URL('../components/screens/rules/RuleItem.tsx', import.meta.url)),
      'utf8',
    ),
  );

  /** 变异对照：把 `onDuplicate={handleDuplicate}` 从 `<RuleItem>` 上摘掉 → 转红（按钮整个不渲染）。 */
  it('RulesScreen 把 handleDuplicate 接到 RuleItem 上', () => {
    expect(SCREEN).toContain('onDuplicate={handleDuplicate}');
    expect(SCREEN).toContain('duplicateRulePayload(rule,');
    expect(SCREEN).toContain('api.rules.add(');
  });

  /**
   * 写完必须刷 store —— `store.rules` 只由 `loadConfig`/`saveConfig` 写，不刷则列表看不到新增那条，
   * 用户以为复制失败又点一次，落出两条副本。
   *
   * 变异对照：删掉 `loadConfig(true)` → 转红。
   */
  it('复制成功后刷新 store', () => {
    expect(SCREEN).toContain('loadConfig(true)');
  });

  /** 变异对照：把按钮改成无条件渲染 → 转红（只读消费点会多出一个点了没反应的按钮）。 */
  it('RuleItem 仅在传了 onDuplicate 时渲染复制按钮', () => {
    expect(ITEM).toContain('{onDuplicate && (');
    expect(ITEM).toContain('onClick={() => onDuplicate(rule)}');
  });
});
