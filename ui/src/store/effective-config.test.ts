/**
 * `effectiveConfigOf` 的门 —— 暂存回显那一层的三条性质（引用稳定性 / 失效判据 / 不抛）。
 *
 * 为什么这三条各自都得有测，而不是「回显对了就行」：
 *  - **引用稳定性**是渲染开销的唯一保证。zustand selector 按引用比较，每次返回新对象 ⇒
 *    全应用每次 store 变更都重渲染。且 `STAGED_CONFIG_ENABLED === false` 时条目恒空，
 *    「空条目集返回入参本体」正是本轮改动**零变化**承诺的全部技术内容。
 *    等值断言（`toEqual`）验不出这条 —— 深拷贝也 `toEqual` 相等。**必须 `toBe`。**
 *  - **失效判据**要两侧都验：漏了 config 变更 ⇒ 磁盘侧改动被旧缓存盖住；漏了 entries 变更 ⇒
 *    用户刚撤销的编辑还留在界面上。单验一侧的测试会让另一侧的 bug 全程绿着过。
 *  - **不抛**：本函数在渲染路径上，抛 = 白屏。畸形条目只可能来自 Storage（进程外数据），
 *    `applyEntry` 已对它跳过，本层不得引入新的抛点。
 *
 * 走纯函数 `effectiveConfigOf` 而不是 `useEffectiveConfig`：后者是 hook，vitest 是
 * `environment: 'node'`（无 jsdom，有意为之）。hook 那层只是「两个订阅 + 调本函数」，
 * 它接线对不对由 `lib/config-read-wiring.test.ts` 的源码守卫钉，不靠渲染测。
 */
import { describe, it, expect } from 'vitest';

import type { UserConfig } from '@/contracts/types';
import type { StagedEntry } from '@/lib/staged-config';
import { effectiveConfigOf } from './app-store';

const CONFIG = {
  servers: [
    { id: 'n1', name: '香港 01', port: 443 },
    { id: 'n2', name: '东京 02', port: 8443 },
  ],
  selectedServerId: 'n1',
  mixedPort: 7890,
  dnsConfig: { enableFakeIp: false },
} as unknown as UserConfig;

const EMPTY: readonly StagedEntry[] = [];

const editN1: StagedEntry = {
  id: 'server:n1',
  kind: 'server',
  label: '编辑节点 香港 01',
  entityPath: ['servers', 'n1'],
  nextValue: { id: 'n1', name: '香港 01（改）', port: 9443 },
};

const editPort: StagedEntry = {
  id: 'setting:mixedPort',
  kind: 'setting',
  label: '修改设置 · mixedPort',
  entityPath: ['mixedPort'],
  nextValue: 1080,
};

describe('引用稳定性（今天的唯一实际路径：总开关关 ⇒ 条目恒空）', () => {
  it('条目为空 ⇒ 返回的**就是**入参那个对象，不是等值副本', () => {
    // 变异对照：把实现改成 `replay(config, entries)` 无条件走（即删掉 length===0 早退），
    // 本断言立刻红（replay 恒 `{...baseline}` 建新对象），而 toEqual 版本仍绿。
    expect(effectiveConfigOf(CONFIG, EMPTY)).toBe(CONFIG);
  });

  it('同一 (config, entries) 连续两次取值恒同一引用', () => {
    expect(effectiveConfigOf(CONFIG, EMPTY)).toBe(effectiveConfigOf(CONFIG, EMPTY));
    const entries = [editN1];
    expect(effectiveConfigOf(CONFIG, entries)).toBe(effectiveConfigOf(CONFIG, entries));
  });

  it('空数组的**不同引用**同样走早退（每次 render 现造 [] 也不该重算）', () => {
    expect(effectiveConfigOf(CONFIG, [])).toBe(CONFIG);
    expect(effectiveConfigOf(CONFIG, [])).toBe(CONFIG);
  });

  it('config 为 null ⇒ null（不新建对象、不抛）', () => {
    expect(effectiveConfigOf(null, EMPTY)).toBeNull();
    expect(effectiveConfigOf(null, [editN1])).toBeNull();
  });
});

describe('回显：条目非空 ⇒ 合成值', () => {
  it('集合实体按 id 就地替换，其余字段原样', () => {
    const eff = effectiveConfigOf(CONFIG, [editN1])!;
    expect(eff.servers?.find((s) => s.id === 'n1')?.name).toBe('香港 01（改）');
    expect(eff.servers?.find((s) => s.id === 'n2')?.name).toBe('东京 02');
    expect(eff.mixedPort).toBe(7890);
  });

  it('设置键按键路径替换', () => {
    expect(effectiveConfigOf(CONFIG, [editPort])!.mixedPort).toBe(1080);
  });

  it('不改入参（磁盘那份必须保持原样，否则暂存基准被就地污染）', () => {
    effectiveConfigOf(CONFIG, [editN1, editPort]);
    expect(CONFIG.servers?.find((s) => s.id === 'n1')?.name).toBe('香港 01');
    expect(CONFIG.mixedPort).toBe(7890);
  });
});

describe('顺序条目同样参与合成（`replay` 两趟：实体在前、顺序在后）', () => {
  const reorder: StagedEntry = {
    id: 'order:servers',
    kind: 'server',
    label: '调整节点顺序',
    entityPath: ['servers'],
    nextValue: ['n2', 'n1'],
  };

  it('只有一条顺序条目 ⇒ 合成结果照样变（不得被当成「没有实体变更」短路掉）', () => {
    // 变异对照：把早退判据写成「滤掉顺序条目后为空就返回 config」（一个看着合理的优化），
    // 本断言立刻红 —— 用户拖完顺序，列表纹丝不动。
    const eff = effectiveConfigOf(CONFIG, [reorder])!;
    expect(eff).not.toBe(CONFIG);
    expect(eff.servers?.map((s) => s.id)).toEqual(['n2', 'n1']);
    expect(CONFIG.servers?.map((s) => s.id)).toEqual(['n1', 'n2']); // 入参不动
  });

  it('顺序条目参与失效判据（entries 引用换了就重算）', () => {
    const first = effectiveConfigOf(CONFIG, [editPort]);
    const second = effectiveConfigOf(CONFIG, [editPort, reorder]);
    expect(second).not.toBe(first);
    expect(second!.servers?.map((s) => s.id)).toEqual(['n2', 'n1']);
  });
});

describe('记忆化失效判据 = 两个入参的引用', () => {
  it('config 换了 ⇒ 重算（旧缓存不得盖住磁盘侧新值）', () => {
    const entries = [editPort];
    const first = effectiveConfigOf(CONFIG, entries);
    const nextDisk = { ...CONFIG, selectedServerId: 'n2' } as UserConfig;
    const second = effectiveConfigOf(nextDisk, entries);
    // 变异对照：把失效判据只留 `entries === memoEntries`，本断言红（second 会拿到 first）。
    expect(second).not.toBe(first);
    expect(second!.selectedServerId).toBe('n2');
    expect(second!.mixedPort).toBe(1080);
  });

  it('entries 换了 ⇒ 重算（撤销/新增的编辑必须立刻回显）', () => {
    const first = effectiveConfigOf(CONFIG, [editPort]);
    const second = effectiveConfigOf(CONFIG, [editPort, editN1]);
    // 变异对照：把失效判据只留 `config === memoConfig`，本断言红。
    expect(second).not.toBe(first);
    expect(second!.servers?.find((s) => s.id === 'n1')?.name).toBe('香港 01（改）');
  });

  it('条目从非空回到空 ⇒ 直接回到磁盘那份本体（「重置」后 UI 与磁盘一致，FR-4 / S-2）', () => {
    effectiveConfigOf(CONFIG, [editPort]);
    expect(effectiveConfigOf(CONFIG, EMPTY)).toBe(CONFIG);
  });
});

describe('畸形条目不抛（渲染路径上抛 = 白屏）', () => {
  // 这些形态都能从 Storage 里读出来（用户手改 / 旧版本残留 / 半截写入）。
  const junk = [
    { id: '', kind: 'server', label: '', entityPath: ['servers', 'n1'], nextValue: {} },
    { id: 'x', kind: 'nope', label: '', entityPath: ['servers', 'n1'], nextValue: null },
    { id: 'x', kind: 'server', label: '', entityPath: [], nextValue: null },
    // 集合路径但 id 与路径不符 —— 会造成静默错位，必须跳过而不是写进去。
    {
      id: 'x',
      kind: 'server',
      label: '',
      entityPath: ['servers', 'n1'],
      nextValue: { id: 'OTHER' },
    },
    null,
    'not-an-entry',
  ] as unknown as StagedEntry[];

  it('整批畸形 ⇒ 不抛，且逐条被跳过（等值于磁盘那份）', () => {
    expect(() => effectiveConfigOf(CONFIG, junk)).not.toThrow();
    expect(effectiveConfigOf(CONFIG, junk)).toEqual(CONFIG);
  });

  it('畸形混在合法条目中间 ⇒ 合法的照常落，畸形的跳过', () => {
    const mixed = [junk[0], editPort, junk[3]] as StagedEntry[];
    const eff = effectiveConfigOf(CONFIG, mixed)!;
    expect(eff.mixedPort).toBe(1080);
    expect(eff.servers?.find((s) => s.id === 'n1')?.name).toBe('香港 01');
    expect(eff.servers?.some((s) => s.id === 'OTHER')).toBe(false);
  });
});
