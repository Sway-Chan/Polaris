/**
 * 磁盘镜像合成层（`effectiveCollection` + `stagedOnlyIds`）的门 —— 「列表回显 staged 编辑、
 * 出口选单不回显」这条裁定的四条性质。
 *
 * # 为什么这一层必须单独有门，`effective-config.test.ts` 不覆盖它
 *
 * app-store 的 `servers` / `rules` 是从 `config.servers` / `config.customRules` 抄出来的**扁平镜像**，
 * 而节点页 / 规则页 / 首页出口选单渲染的是镜像、不是 `config`。上一批把 `config` 读点接上
 * `effectiveConfig` 之后，**列表依然不回显 staged 编辑** —— 缺的正是本层。
 *
 * # 四条性质各自防什么（缺一条就有一类 bug 全程绿着过）
 *
 *  1. **引用稳定性**：本层直接当 zustand selector 用 —— 条目为空时返回**入参本体**、非空时同一
 *     (镜像, 条目) 恒返同一引用。这是「本批改动零变化」承诺的全部技术内容。
 *     `toEqual` 验不出（等值副本照样过），必须 `toBe`。
 *  2. **两面分叉**：staged-only 实体进列表（effective）、不进可立即操作的集合（disk 镜像）。
 *     反向断言「它不在 disk 里」单独看是**恒真的废话**（disk 里本来就什么都没有也能过），
 *     故每条都配一个正向对照：盘上真有的那个实体在两面都必须出现。
 *  3. **标记判据**：「待保存」= effective − disk，不新造字段。正向对照 = 改过但盘上有的实体**不得**被标。
 *  4. **重放到镜像而非 `config.customRules`**：镜像会被 `RulesScreen` 的乐观重排先写一手，
 *     取 `effectiveConfig.customRules` 会把那次乐观更新丢掉 ⇒ 拖完弹回原位，**且总开关关着时也会发生**。
 *
 * 走纯函数而不是 `useEffectiveServers/Rules`：后者是 hook，vitest 是 `environment: 'node'`
 * （无 jsdom，有意为之）。hook 那层只是「两个订阅 + 调本函数」，
 * **哪个组件读哪一面**由 `lib/config-read-wiring.test.ts` 的 `MIRROR_SITES` 钉，不靠渲染测。
 */
import { describe, it, expect } from 'vitest';

import type { Rule, ServerConfig, UserConfig } from '@/contracts/types';
import { STAGED_CONFIG_ENABLED, stagedOnlyIds, type StagedEntry } from '@/lib/staged-config';
import { effectiveCollection, effectiveConfigOf } from './app-store';

const DISK_SERVERS = [
  { id: 'n1', name: '香港 01', port: 443 },
  { id: 'n2', name: '东京 02', port: 8443 },
] as unknown as ServerConfig[];

const DISK_RULES = [
  { id: 'r1', type: 'domain', action: 'proxy', enabled: true },
  { id: 'r2', type: 'ipCidr', action: 'direct', enabled: true },
] as unknown as Rule[];

const EMPTY: readonly StagedEntry[] = [];

/** 新建一个还没落盘的节点（NodeDialog / ImportDialog / 克隆三条腿都产出这个形态）。 */
const addN3: StagedEntry = {
  id: 'server:n3',
  kind: 'server',
  label: '添加节点 新加坡 03',
  entityPath: ['servers', 'n3'],
  nextValue: { id: 'n3', name: '新加坡 03', port: 443 },
};

/** 改一个**盘上已有**的节点 —— 标记判据的正向对照就靠它。 */
const editN1: StagedEntry = {
  id: 'server:n1',
  kind: 'server',
  label: '编辑节点 香港 01',
  entityPath: ['servers', 'n1'],
  nextValue: { id: 'n1', name: '香港 01（改）', port: 9443 },
};

const addR3: StagedEntry = {
  id: 'rule:r3',
  kind: 'rule',
  label: '新建规则 副本',
  entityPath: ['customRules', 'r3'],
  nextValue: { id: 'r3', type: 'domain', action: 'block', enabled: true },
};

const reorderRules: StagedEntry = {
  id: 'order:customRules',
  kind: 'rule',
  label: '调整规则顺序',
  entityPath: ['customRules'],
  nextValue: ['r2', 'r1'],
};

describe('性质 1：空条目集走引用快路径（返回入参本体，不建等值副本）', () => {
  /**
   * 原标题是「总开关关着 ⇒ 逐字节零变化」，那个框架已随 2026-07-29 翻开开关过时：
   * 条目不再恒空，本节测的也就不再是「产品零变化」。
   *
   * 保留下来的是那条**更基本**的纯函数性质 —— 空条目集必须返回入参本体。
   * 它现在守的是渲染开销：暂存为空是常态（用户大部分时间没有未保存的编辑），
   * 这条快路径一旦退化成防御性拷贝，每次 store 变更都会把整张列表重渲染一遍。
   */
  it('编译期开关为开（翻回 false 是产品行为变更，必须显式改本断言）', () => {
    expect(STAGED_CONFIG_ENABLED).toBe(true);
  });

  it('条目为空 ⇒ 返回的**就是**镜像那个数组，不是等值副本', () => {
    // 变异对照（已实跑）：把早退写成 `return [...mirror]`（一个看着无害的防御性拷贝），本断言
    // 立刻红，而 `toEqual` 版本仍绿 —— 拷贝会让每次 store 变更都把整张列表重渲染一遍。
    //
    // 两条**实跑过、确认不红**的改法，写在这里免得下一个人以为它们被门守着：
    //  · 删掉 `entries.length === 0` 早退 —— `replay` 对空条目集是结构共享的
    //    （`{...baseline}` 只换外层对象，`.servers` 仍是同一个数组）。早退省的是那次分配。
    //  · 在非空腿上拷贝（`[...next]`）—— 空条目集走不到那条腿；那条腿的引用稳定性由下面
    //    「连续两次取值恒同一引用」那条守。
    expect(effectiveCollection('servers', DISK_SERVERS, EMPTY)).toBe(DISK_SERVERS);
    expect(effectiveCollection('customRules', DISK_RULES, EMPTY)).toBe(DISK_RULES);
  });

  it('条目非空时同一 (镜像, 条目) 连续两次取值恒同一引用（记忆化 —— selector 不得抖）', () => {
    // 本函数**直接当 zustand selector 用**：不记忆化就每次返回新数组，
    // `useSyncExternalStore` 的快照永不稳定 ⇒ 列表持续重渲染。
    // 变异对照（已实跑）：删掉 `mirror === memo.mirror && entries === memo.entries` 那条命中判据，
    // 本断言立刻红。
    const entries = [addN3];
    expect(effectiveCollection('servers', DISK_SERVERS, entries)).toBe(
      effectiveCollection('servers', DISK_SERVERS, entries)
    );
  });

  it('记忆化失效判据取两个入参的引用（漏一侧就有一类 bug 全程绿着过）', () => {
    const entries = [addN3];
    const first = effectiveCollection('servers', DISK_SERVERS, entries);
    // 镜像换了 ⇒ 重算（否则磁盘侧的新节点被旧缓存盖住）。
    const otherMirror = [...DISK_SERVERS];
    expect(effectiveCollection('servers', otherMirror, entries)).not.toBe(first);
    // 条目换了 ⇒ 重算（否则刚撤销的编辑还留在列表上）。
    const again = effectiveCollection('servers', DISK_SERVERS, entries);
    expect(effectiveCollection('servers', DISK_SERVERS, [addN3, editN1])).not.toBe(again);
  });

  it('两个集合各有各的记忆化槽（互不顶掉）', () => {
    const se = [addN3];
    const re = [addR3];
    const s1 = effectiveCollection('servers', DISK_SERVERS, se);
    const r1 = effectiveCollection('customRules', DISK_RULES, re);
    expect(effectiveCollection('servers', DISK_SERVERS, se)).toBe(s1);
    expect(effectiveCollection('customRules', DISK_RULES, re)).toBe(r1);
  });

  it('每次 render 现造的空数组同样走早退（不同引用、同样返回本体）', () => {
    expect(effectiveCollection('servers', DISK_SERVERS, [])).toBe(DISK_SERVERS);
    expect(effectiveCollection('servers', DISK_SERVERS, [])).toBe(DISK_SERVERS);
  });

  it('条目非空 ⇒ 不改入参（磁盘镜像被就地污染的话，两面的差就没了）', () => {
    effectiveCollection('servers', DISK_SERVERS, [addN3, editN1]);
    expect(DISK_SERVERS.map((s) => s.id)).toEqual(['n1', 'n2']);
    expect(DISK_SERVERS[0].name).toBe('香港 01');
  });
});

describe('性质 2：staged-only 实体进列表、不进「可立即操作」的集合', () => {
  it('列表侧（effective）看得见新节点，磁盘镜像侧看不见', () => {
    const listed = effectiveCollection('servers', DISK_SERVERS, [addN3]);
    expect(listed.map((s) => s.id)).toEqual(['n1', 'n2', 'n3']);

    // **正向对照**：单说「n3 不在 disk 里」是恒真的废话（disk 空着也过）。
    // 真正的判据是「盘上有的两个必须在，盘上没有的那个必须不在」，两个方向一起说话。
    expect(DISK_SERVERS.map((s) => s.id)).toEqual(['n1', 'n2']);
    expect(DISK_SERVERS.some((s) => s.id === 'n3')).toBe(false);
  });

  it('出口选单读的就是那份磁盘镜像 —— 谁读哪一面由 MIRROR_SITES 钉', () => {
    // 本条只钉「两份集合确实不同」这半边（另半边 = HomeScreen 读的是裸镜像，
    // 由 `lib/config-read-wiring.test.ts` 的 M3 锚点钉；那是源码结构事实，这里测不了）。
    const listed = effectiveCollection('servers', DISK_SERVERS, [addN3]);
    expect(listed).not.toBe(DISK_SERVERS);
    expect(listed.length).toBe(DISK_SERVERS.length + 1);
  });

  it('规则侧同型：新规则进列表，磁盘镜像不动', () => {
    const listed = effectiveCollection('customRules', DISK_RULES, [addR3]);
    expect(listed.map((r) => r.id)).toEqual(['r1', 'r2', 'r3']);
    expect(DISK_RULES.map((r) => r.id)).toEqual(['r1', 'r2']);
  });
});

describe('性质 3：「待保存」标记 = effective − disk（不新造字段）', () => {
  it('只标 staged-only；**改过但盘上有**的实体不标（正向对照）', () => {
    const listed = effectiveCollection('servers', DISK_SERVERS, [addN3, editN1]);
    const marked = stagedOnlyIds(listed, DISK_SERVERS);
    // 变异对照（已实跑）：把判据改成「条目里提到的 id 都算」，本断言红 —— n1 被改过但在盘上，
    // 标成「待保存 · 尚未落盘」是撒谎（它能被选为出口）。
    expect([...marked]).toEqual(['n3']);
    expect(marked.has('n1')).toBe(false);
  });

  it('无条目 ⇒ 空集（总开关关着时列表上不会凭空多出角标）', () => {
    expect(stagedOnlyIds(effectiveCollection('servers', DISK_SERVERS, EMPTY), DISK_SERVERS).size).toBe(
      0
    );
  });

  it('删除条目不会让盘上的实体被标（它从 effective 里消失，不是「多出来」）', () => {
    const del: StagedEntry = {
      id: 'server:n2',
      kind: 'server',
      label: '删除节点 东京 02',
      entityPath: ['servers', 'n2'],
      nextValue: null,
    };
    const listed = effectiveCollection('servers', DISK_SERVERS, [del]);
    expect(listed.map((s) => s.id)).toEqual(['n1']);
    expect(stagedOnlyIds(listed, DISK_SERVERS).size).toBe(0);
  });
});

describe('性质 4：重放的基准是**镜像**，不是 `config.customRules`', () => {
  /** `RulesScreen` 的乐观重排先写镜像（等一轮 IPC 会先弹回再跳过去），config 侧此刻还是旧序。 */
  const OPTIMISTIC_MIRROR = [DISK_RULES[1], DISK_RULES[0]];
  const DISK_CONFIG = { customRules: DISK_RULES } as unknown as UserConfig;

  it('总开关关着时，乐观重排后的镜像原样交出（取 config.customRules 会把它丢掉）', () => {
    // 变异对照（已实跑）：把 `useEffectiveRules` 实现成 `useEffectiveConfig((c) => c?.customRules)`，
    // 本断言红 —— 顺序退回 ['r1','r2']，表现成「拖完弹回原位」，**且开关关着时也会发生**。
    expect(effectiveCollection('customRules', OPTIMISTIC_MIRROR, EMPTY).map((r) => r.id)).toEqual([
      'r2',
      'r1',
    ]);
    expect(effectiveConfigOf(DISK_CONFIG, EMPTY)!.customRules?.map((r) => r.id)).toEqual([
      'r1',
      'r2',
    ]);
  });

  it('顺序条目重放到镜像上是幂等的（乐观写过一次，再重放一次结果相同）', () => {
    const once = effectiveCollection('customRules', OPTIMISTIC_MIRROR, [reorderRules]);
    expect(once.map((r) => r.id)).toEqual(['r2', 'r1']);
    // 幂等：把结果再喂一次，仍是同一个排列（重放对条目入表顺序不敏感的前提）。
    expect(effectiveCollection('customRules', once, [reorderRules]).map((r) => r.id)).toEqual([
      'r2',
      'r1',
    ]);
  });

  it('顺序条目 + 新增实体：新实体落末尾（`replay` 两趟的可交换性）', () => {
    const listed = effectiveCollection('customRules', DISK_RULES, [addR3, reorderRules]);
    expect(listed.map((r) => r.id)).toEqual(['r2', 'r1', 'r3']);
    // 条目入表顺序反过来，结果必须一样（否则收敛结果取决于用户操作次序这个偶然）。
    expect(
      effectiveCollection('customRules', DISK_RULES, [reorderRules, addR3]).map((r) => r.id)
    ).toEqual(['r2', 'r1', 'r3']);
  });

  it('畸形条目不抛、不把集合换成非数组（渲染路径上抛 = 白屏）', () => {
    const junk = [
      { id: 'x', kind: 'nope', label: '', entityPath: ['servers', 'n1'], nextValue: null },
      null,
      'not-an-entry',
    ] as unknown as StagedEntry[];
    expect(() => effectiveCollection('servers', DISK_SERVERS, junk)).not.toThrow();
    expect(effectiveCollection('servers', DISK_SERVERS, junk)).toEqual(DISK_SERVERS);
  });
});
