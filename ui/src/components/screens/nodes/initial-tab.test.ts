import { describe, it, expect } from 'vitest';
import { initialNodesTab } from './initial-tab';
import type { ServerGroup } from '@/domain/server-grouping';

const g = (id: string, ids: string[]): ServerGroup => ({
  id,
  name: id,
  isManual: id === 'manual',
  servers: ids.map((sid) => ({ id: sid }) as ServerGroup['servers'][number]),
});

describe('initialNodesTab', () => {
  const groups = [g('manual', ['n-local']), g('mesh', []), g('sub-a', ['n-hk', 'n-jp'])];

  it('落在选中节点所在的组，而不是常量 manual', () => {
    // 守的缺陷：原实现 useState('manual') 写死落地 tab，订阅用户每次进页面都落在空的自建组。
    // 变异：把返回值改回 groups[0].id → 本条转红。
    expect(initialNodesTab(groups, 'n-jp')).toBe('sub-a');
  });

  it('选中节点在自建组时仍落自建', () => {
    expect(initialNodesTab(groups, 'n-local')).toBe('manual');
  });

  it('选中 id 缺失 / 找不到（已删、__direct__）→ 回落首组，不空白', () => {
    expect(initialNodesTab(groups, null)).toBe('manual');
    expect(initialNodesTab(groups, '')).toBe('manual');
    expect(initialNodesTab(groups, '__direct__')).toBe('manual');
    expect(initialNodesTab(groups, 'n-gone')).toBe('manual');
  });

  it('groups 未水合 → null（调用方不得据此设 tab）', () => {
    // 返回 groups[0]?.id 会在空数组上得 undefined、被调用方当成合法值写进 state，
    // 等真 groups 到齐就再没机会定位了。故此处必须是显式 null。
    expect(initialNodesTab([], 'n-jp')).toBeNull();
  });
});
