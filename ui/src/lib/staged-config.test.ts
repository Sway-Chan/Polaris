/**
 * 配置暂存纯逻辑的门 —— 重放 / 撤销 / 路由判定 / 持久化编解码。
 *
 * 每条给出**变异对照**（改坏哪一行会让它转红）；无变异对照的断言视为无信息量。
 */
import { describe, it, expect } from 'vitest';

import {
  BYPASS_TABLE,
  STAGED_CONFIG_ENABLED,
  configBaseVersion,
  conflictingEntries,
  decodeStagedPayload,
  editRoute,
  encodeStagedPayload,
  entitySnapshot,
  isAbsentSnapshot,
  isBypassedConfigKey,
  isBypassedOp,
  isValidStagedEntry,
  replay,
  revertEntry,
  stageEntry,
  type StagedEntry,
} from './staged-config';
import { isStagedExempt } from '@/contracts/user-config-fields';

function serverEntry(id: string, patch: Record<string, unknown> = {}): StagedEntry {
  return {
    id: `server:${id}`,
    kind: 'server',
    label: `编辑节点 ${id}`,
    entityPath: ['servers', id],
    nextValue: { id, name: id, protocol: 'vless', address: 'a.example', port: 443, ...patch },
  };
}

const BASE = {
  servers: [
    { id: 'n1', name: 'n1', protocol: 'vless', address: 'a.example', port: 443 },
    { id: 'n2', name: 'n2', protocol: 'vless', address: 'b.example', port: 443 },
  ],
  selectedServerId: 'n1',
  mixedPort: 7890,
  dnsConfig: { enableFakeIp: true, servers: ['1.1.1.1'] },
};

describe('replay —— 从 baseline 重放条目', () => {
  /** 牙：把 `replay` 的 reduce 初值从 `{...baseline}` 改成 `baseline` 本体 → 入参被改 → 转红。 */
  it('不可变：不改入参', () => {
    const snapshot = JSON.parse(JSON.stringify(BASE));
    replay(BASE, [serverEntry('n1', { port: 8443 })]);
    expect(BASE).toEqual(snapshot);
  });

  /** 空集恒等（FR-4 / S-2：「重置」后 UI 与磁盘逐字段相等的依据）。 */
  it('空条目集 ⇒ 与 baseline 逐字段相等', () => {
    expect(replay(BASE, [])).toEqual(BASE);
  });

  /**
   * T1-1 重放幂等。
   *
   * 牙：把某条 `nextValue` 从「整体替换」改成读旧值再改（`port + 1` 那种增量语义）→
   * 连跑两次结果不同 → 转红。这条钉住 Q2 理由 3「apply 必须是幂等的整体替换」。
   */
  it('T1-1 幂等：同一输入重放两次结果相同', () => {
    const entries = [serverEntry('n1', { port: 8443 }), serverEntry('n2', { address: 'c.example' })];
    expect(replay(BASE, entries)).toEqual(replay(replay(BASE, entries), entries));
  });

  it('集合实体：按 id 整体替换，不动同集合的其它元素', () => {
    const next = replay(BASE, [serverEntry('n1', { port: 8443 })]);
    expect(next.servers[0]).toEqual({
      id: 'n1',
      name: 'n1',
      protocol: 'vless',
      address: 'a.example',
      port: 8443,
    });
    expect(next.servers[1]).toEqual(BASE.servers[1]);
  });

  it('集合实体：id 不存在 ⇒ 追加（新增节点）', () => {
    const next = replay(BASE, [serverEntry('n3')]);
    expect(next.servers).toHaveLength(3);
    expect(next.servers[2].id).toBe('n3');
  });

  /** 集合实体上 `nextValue: null` = 删除。 */
  it('集合实体：nextValue=null ⇒ 删除该实体', () => {
    const next = replay(BASE, [
      { id: 'server:n1', kind: 'server', label: '删除节点 n1', entityPath: ['servers', 'n1'], nextValue: null },
    ]);
    expect(next.servers.map((s) => s.id)).toEqual(['n2']);
  });

  /**
   * 牙：把 `upsertById` 的「集合不存在就原样返回」删掉 → 会凭空补一个 `ruleResources: []` 键 →
   * 转红。重放必须不改变无关形状，否则「重置 ≡ 磁盘现值」这条不变式在删除路径上破功。
   */
  it('集合实体：删一个不存在的集合 ⇒ 形状零变化', () => {
    const next = replay(BASE, [
      {
        id: 'rule:x',
        kind: 'rule',
        label: '删除规则 x',
        entityPath: ['ruleResources', 'x'],
        nextValue: null,
      },
    ]);
    expect(next).toEqual(BASE);
    expect(Object.keys(next)).toEqual(Object.keys(BASE));
  });

  it('键路径：顶层键与嵌套键都按整体替换写入', () => {
    const next = replay(BASE, [
      { id: 'setting:mixedPort', kind: 'setting', label: '混合端口', entityPath: ['mixedPort'], nextValue: 1080 },
      {
        id: 'setting:dnsConfig.enableFakeIp',
        kind: 'setting',
        label: 'FakeIP',
        entityPath: ['dnsConfig', 'enableFakeIp'],
        nextValue: false,
      },
    ]);
    expect(next.mixedPort).toBe(1080);
    expect(next.dnsConfig).toEqual({ enableFakeIp: false, servers: ['1.1.1.1'] });
  });

  /** 非法条目跳过而非抛（渲染路径上抛 = 白屏）。 */
  it('非法条目：跳过，不抛，不污染结果', () => {
    const bogus = { id: '', kind: 'server', label: 'x', entityPath: [], nextValue: 1 } as unknown as StagedEntry;
    expect(replay(BASE, [bogus, serverEntry('n1', { port: 8443 })]).servers[0].port).toBe(8443);
  });
});

describe('stageEntry / revertEntry', () => {
  /**
   * T1-3 同 id 覆盖。
   *
   * 牙：把 `stageEntry` 的「找到同 id 就替换」改成无条件 push → 条目数虚高（一次表单连改两次
   * 就显示「2 项待保存」）→ 转红。
   */
  it('T1-3 同一实体重复编辑不产生第二条，且保序', () => {
    const one = stageEntry([], serverEntry('n1', { port: 1 }));
    const two = stageEntry(one, serverEntry('n2'));
    const three = stageEntry(two, serverEntry('n1', { port: 2 }));
    expect(three).toHaveLength(2);
    expect(three[0].id).toBe('server:n1');
    expect((three[0].nextValue as { port: number }).port).toBe(2);
  });

  it('非法条目不入表', () => {
    const bogus = { id: 'x', kind: 'nope', label: '', entityPath: ['servers', 'a'], nextValue: {} } as unknown as StagedEntry;
    expect(stageEntry([], bogus)).toHaveLength(0);
  });

  /**
   * T1-2 撤销等价（S-5）：`replay(base, entries \ {k})` ≡ `replay(base, 从未含 k 的 entries)`。
   *
   * 牙：把 `revertEntry` 实现成「追加一条反向 patch」而非「移除后重放」→ 同一实体被改过两次时
   * 中间态会残留 → 两侧不等 → 转红。
   */
  it('T1-2 撤销第 k 条 ≡ 从未加入第 k 条', () => {
    const a = serverEntry('n1', { port: 8443 });
    const b = serverEntry('n2', { address: 'c.example' });
    const withAll = stageEntry(stageEntry([], a), b);
    expect(replay(BASE, revertEntry(withAll, b.id))).toEqual(replay(BASE, [a]));
  });

  it('撤销同一实体的第二次编辑后，整条条目消失（不回到第一次的值）', () => {
    const first = stageEntry([], serverEntry('n1', { port: 1 }));
    const second = stageEntry(first, serverEntry('n1', { port: 2 }));
    expect(replay(BASE, revertEntry(second, 'server:n1'))).toEqual(BASE);
  });

  it('撤销不存在的 id ⇒ 原样', () => {
    const entries = stageEntry([], serverEntry('n1'));
    expect(revertEntry(entries, 'server:zzz')).toEqual(entries);
  });
});

describe('isValidStagedEntry', () => {
  it('集合实体的 nextValue.id 必须与路径末段一致', () => {
    expect(isValidStagedEntry(serverEntry('n1'))).toBe(true);
    expect(
      isValidStagedEntry({ ...serverEntry('n1'), nextValue: { id: 'n2' } })
    ).toBe(false);
  });

  /**
   * 「集合键当单段路径一律拒收」这条**收窄**成了「只准是主键序列」——因由：单段集合路径现在有了
   * 唯一合法语义（顺序条目，见下方「顺序条目」组）。它原本要挡的那件事**没有放松**：
   * 拿元素数组去整份替换集合仍然被拒（下面第 4 条），那才是「整份替换不是暂存粒度」的实质。
   */
  it('拒收空路径 / 空 id / 未知 kind / 集合键当单段路径却塞整份集合', () => {
    const ok = serverEntry('n1');
    expect(isValidStagedEntry({ ...ok, entityPath: [] })).toBe(false);
    expect(isValidStagedEntry({ ...ok, id: '' })).toBe(false);
    expect(isValidStagedEntry({ ...ok, kind: 'nope' })).toBe(false);
    expect(
      isValidStagedEntry({ ...ok, entityPath: ['servers'], nextValue: [{ id: 'n1' }] })
    ).toBe(false);
    expect(isValidStagedEntry({ ...ok, entityPath: ['servers'], nextValue: {} })).toBe(false);
    // 反向对照：同一条路径给主键序列 ⇒ 是合法的顺序条目（否则上面那条断言就成了空跑）。
    expect(isValidStagedEntry({ ...ok, entityPath: ['servers'], nextValue: ['n1'] })).toBe(true);
    expect(isValidStagedEntry(null)).toBe(false);
    expect(isValidStagedEntry('{}')).toBe(false);
  });
});

describe('editRoute —— 总开关 + 两张表的唯一闸门', () => {
  /**
   * **总开关关掉 = 今天行为**的证据就是这一条：无论键属哪一类、操作是不是绕过，
   * 一律 `'direct'`（提交即落盘），暂存层不产生任何条目、不读写任何存储。
   *
   * 牙：在任一编辑入口里绕开 `editRoute` 自己写第二处 `if (staged)` → 该入口不受开关管辖 →
   * 本条测不到它（所以入口侧必须只经这一个函数，这条纪律写在 `editRoute` 的文档里）。
   * 就本函数而言：把 `if (!enabled) return 'direct'` 挪到豁免判定之后 → 关着开关时
   * `servers` 仍返 `'staged'` → 转红。
   */
  it('总开关关 ⇒ 一切走 direct', () => {
    for (const key of ['servers', 'customRules', 'dnsConfig', 'autoStart', 'selectedServerId']) {
      expect(editRoute(key, false), key).toBe('direct');
      expect(editRoute(key, false, 'switchServer'), key).toBe('direct');
    }
  });

  /**
   * 默认值本身是产品行为的一部分。2026-07-29 由 `false` 翻成 `true`（陈先生拍板），
   * 前置四条（P4 三动作 + P5 乐观并发 + P6 双向接线守卫 + 回显闭环）见常量本身的文档。
   *
   * 牙的方向随之反转：现在把它翻回 `false` → 转红。反向同样是产品行为变更 ——
   * 关掉之后用户在条上看到的「N 项待保存」会连同暂存条目一起消失，
   * 而 localStorage 里那批 staged 载荷仍在（`hydrate` 在 `enabled=false` 时早退不读它），
   * 下次再开又会冒出来。任一方向的翻动都必须显式改本断言。
   */
  it('编译期默认值为开（改默认值必须显式改本断言）', () => {
    expect(STAGED_CONFIG_ENABLED).toBe(true);
  });

  it('开关开：Class B 键进暂存', () => {
    expect(editRoute('servers', true)).toBe('staged');
    expect(editRoute('dnsConfig.enableFakeIp', true)).toBe('staged');
    expect(editRoute('singboxDashboard', true)).toBe('staged');
    // 日志两轴喂 sing-box `log.*`，改了要重启核才生效 ⇒ 与「第四类重启」收口同批归入 Class B。
    expect(editRoute('logLevel', true)).toBe('staged');
  });

  it('开关开：W-0 豁免键走 direct', () => {
    expect(editRoute('autoStart', true)).toBe('direct');
    expect(editRoute('hardwareAcceleration', true)).toBe('direct');
  });

  it('开关开：W-1/2/3 绕过（按键、按操作各一条）走 direct', () => {
    expect(editRoute('selectedServerId', true)).toBe('direct');
    expect(editRoute('servers', true, 'deleteTailscaleNode')).toBe('direct');
  });
});

describe('W-0 豁免表 与 W-1/2/3 绕过表互斥（T1-6）', () => {
  /**
   * E-2：`selectedServerId` **在** Rust `UserConfig` 里（故不豁免），但切节点必须立刻生效（故绕过）。
   *
   * 牙：把 `selectedServerId` 从绕过表挪进豁免表（或反过来）→ 转红。两者磁盘行为相同、语义不同，
   * 混同会让「Rust 加/删字段时豁免集自动伸缩」这条性质悄悄作用到绕过集上。
   */
  it('selectedServerId 在绕过表、不在豁免表', () => {
    expect(isBypassedConfigKey('selectedServerId')).toBe(true);
    expect(isStagedExempt('selectedServerId')).toBe(false);
  });

  it('绕过表里所有 configKey 都不是豁免键（否则该行是空操作）', () => {
    for (const rule of BYPASS_TABLE) {
      if (rule.configKey === undefined) continue;
      expect(isStagedExempt(rule.configKey), `${rule.op} 的 ${rule.configKey}`).toBe(false);
    }
  });

  it('不经 config 的操作按 op 命中（起停核 / 隐私锁 / 删组网节点 / 刷订阅）', () => {
    for (const op of [
      'proxyStartStop',
      'privacyLock',
      'deleteTailscaleNode',
      'deleteWarpNode',
      'refreshSubscription',
    ]) {
      expect(isBypassedOp(op), op).toBe(true);
    }
    expect(isBypassedOp('editServer')).toBe(false);
  });
});

describe('持久化编解码（Q1-b，载荷带 baseline 本体）', () => {
  it('编码 → 解码往返保真', () => {
    const payload = { baseline: BASE, entries: [serverEntry('n1'), serverEntry('n2')] };
    expect(decodeStagedPayload(encodeStagedPayload(payload))).toEqual(payload);
  });

  /**
   * `baseline` 必须原样往返 —— 它是冲突检出的基准，掉一层或被改形就等于换了基准。
   * 变异对照：让 `encodeStagedPayload` 只写 `configBaseVersion(baseline)`（回到旧契约）→ 本条转红。
   */
  it('baseline 本体逐字段往返，不退化成版本 hash', () => {
    const decoded = decodeStagedPayload(encodeStagedPayload({ baseline: BASE, entries: [] }));
    expect(decoded?.baseline).toEqual(BASE);
  });

  /**
   * 畸形一律 null：「解析不了」与「没存过」对调用方是同一种情况。
   *
   * **`{baseVersion, entries}` 是老形态**（无 `baseline`）：同样判 null ⇒ 调用方退回今天的丢弃
   * 行为。没有基准就判不了冲突，此时丢弃是唯一诚实的选择。
   * 变异对照：把 `!isRecord(baseline)` 放宽成 `baseline ?? {}` → 老载荷会带着一个空基准被恢复，
   * 之后每个实体都判成「磁盘侧变了」⇒ 恒冲突弹窗 → 本条转红。
   */
  it('畸形输入 / 老形态（无 baseline）一律 null，不抛', () => {
    for (const raw of [
      null,
      undefined,
      '',
      'not json',
      '[]',
      '{}',
      '{"baseline":{}}',
      '{"baseline":{},"entries":3}',
      '{"baseline":3,"entries":[]}',
      '{"baseline":null,"entries":[]}',
      '{"baseline":[],"entries":[]}',
      '{"baseVersion":"deadbeef","entries":[]}',
    ]) {
      expect(decodeStagedPayload(raw as string | null), String(raw)).toBeNull();
    }
  });

  /** 一条坏条目不该让用户其余编辑陪葬。 */
  it('逐条丢弃非法条目，剩余条目仍可恢复', () => {
    const raw = JSON.stringify({
      baseline: BASE,
      entries: [serverEntry('n1'), { id: 'x' }, { id: 'y', kind: 'server', label: '', entityPath: ['servers', 'n9'], nextValue: { id: 'n8' } }],
    });
    const decoded = decodeStagedPayload(raw);
    expect(decoded?.entries).toHaveLength(1);
    expect(decoded?.entries[0].id).toBe('server:n1');
  });
});

describe('configBaseVersion（T1-17 的版本源）', () => {
  /**
   * 牙：把 `stableStringify` 换成 `JSON.stringify` → 键序不同的等值 config 得到不同版本 →
   * 每次 `loadConfig` 都被判「盘上变了」→ 恢复腿恒失配、staged 永远恢复不回来 → 转红。
   */
  it('键序无关：等值 config 版本相同', () => {
    expect(configBaseVersion({ a: 1, b: { c: 2, d: 3 } })).toBe(
      configBaseVersion({ b: { d: 3, c: 2 }, a: 1 })
    );
  });

  /**
   * 数组顺序**是**语义（节点列表顺序会显示给用户）。
   * 牙：让 `stableStringify` 顺手把数组也排序 → 顺序变化测不出 → 转红。
   */
  it('数组顺序敏感', () => {
    expect(configBaseVersion({ servers: [1, 2] })).not.toBe(configBaseVersion({ servers: [2, 1] }));
  });

  it('内容变了版本就变（改一个端口）', () => {
    const changed = { ...BASE, servers: [{ ...BASE.servers[0], port: 8443 }, BASE.servers[1]] };
    expect(configBaseVersion(changed)).not.toBe(configBaseVersion(BASE));
  });

  it('输出是定长 8 位十六进制', () => {
    expect(configBaseVersion(BASE)).toMatch(/^[0-9a-f]{8}$/);
  });
});

// ---------------------------------------------------------------------------
// P5 冲突检出（Q8-b）—— 实体粒度、比对基准、缺席与 null 的区分。
// ---------------------------------------------------------------------------

describe('entitySnapshot —— 实体子树取值', () => {
  const CFG = {
    servers: [
      { id: 'n1', name: '香港', port: 443 },
      { id: 'n2', name: '东京', port: 8443 },
    ],
    subscriptions: [{ id: 's1', url: 'https://x', etag: 'W/"v1"' }],
    selectedServerId: null,
    dnsConfig: { enableFakeIp: true },
  };

  it('集合实体按 id 寻址，取的是那一个元素而不是整个集合', () => {
    expect(entitySnapshot(CFG, ['servers', 'n1'])).toContain('香港');
    expect(entitySnapshot(CFG, ['servers', 'n1'])).not.toContain('东京');
  });

  /**
   * 键路径实体走 setPath 的同一套寻址（顶层键 / 嵌套键都算实体）。
   * 牙：把键路径腿改成只认顶层键 → `dnsConfig.enableFakeIp` 恒返「缺席」→ 该实体的任何改动
   * 都会与「磁盘上也没有」相等 ⇒ 永远判不出冲突 → 转红。
   */
  it('键路径实体按路径逐层取', () => {
    expect(entitySnapshot(CFG, ['dnsConfig', 'enableFakeIp'])).toBe('true');
    expect(entitySnapshot(CFG, ['selectedServerId'])).toBe('null');
  });

  /**
   * **「值是 null」与「实体不存在」必须可区分**（Q8 最后一行：staged 引用的实体在磁盘侧已被删
   * ⇒ 属冲突，要弹窗问）。两者若都返 `'null'`，「别人把这个节点删了」会被判成「没变」，
   * 于是重放把它悄悄重建出来 —— 用户删了个节点，一保存又活了。
   *
   * 牙：把缺席腿的返回值改成 `'null'` → 后两个断言转红。
   */
  it('缺席与 null 不是同一件事', () => {
    const absent = entitySnapshot(CFG, ['servers', 'nope']);
    expect(isAbsentSnapshot(absent)).toBe(true);
    expect(isAbsentSnapshot(entitySnapshot(CFG, ['selectedServerId']))).toBe(false);
    expect(absent).not.toBe(entitySnapshot(CFG, ['selectedServerId']));
  });

  it('集合本身缺席 ⇒ 也算实体缺席，不抛', () => {
    expect(isAbsentSnapshot(entitySnapshot({}, ['servers', 'n1']))).toBe(true);
    expect(isAbsentSnapshot(entitySnapshot(null, ['mixedPort']))).toBe(true);
  });

  /** 键序无关（与 configBaseVersion 同源的 stableStringify）：同值不同键序不得判成「变了」。 */
  it('键序无关', () => {
    const a = { servers: [{ id: 'n1', name: 'x', port: 443 }] };
    const b = { servers: [{ port: 443, id: 'n1', name: 'x' }] };
    expect(entitySnapshot(a, ['servers', 'n1'])).toBe(entitySnapshot(b, ['servers', 'n1']));
  });
});

describe('conflictingEntries —— 冲突集（T1-14 / T1-15）', () => {
  const BASELINE = {
    servers: [
      { id: 'n1', name: '香港', port: 443 },
      { id: 'n2', name: '东京', port: 8443 },
    ],
    subscriptions: [{ id: 's1', url: 'https://x', etag: 'W/"v1"' }],
    mixedPort: 7890,
  };
  const editN1 = serverEntry('n1', { name: '香港', port: 9443 });

  it('磁盘没变 ⇒ 冲突集为空（自动合并腿）', () => {
    expect(conflictingEntries(BASELINE, BASELINE, [editN1])).toEqual([]);
  });

  /**
   * **T1-14 的正面**：磁盘只动了**别的**实体 ⇒ 不冲突。
   *
   * 这一条是「无弹窗噪音」的守门人（Q8-b 闸 3）：订阅调度器写 `subscriptions[].etag`、
   * 规则资源调度器写 `updatedAt`、后端权威字段写托盘 MRU —— 这些后台写盘每天发生很多次，
   * 若它们能把用户的节点编辑判成冲突，弹窗会变成骚扰，用户第一反应是把这个功能关掉。
   *
   * 牙：把实体粒度换成整份 config 比对（`stableStringify(baseline) !== stableStringify(disk)`）
   * → 本条转红（etag 变了就弹窗）。
   */
  it('只有别的实体变了 ⇒ 不进冲突集（实体粒度，不是整份比对）', () => {
    const disk = {
      ...BASELINE,
      subscriptions: [{ id: 's1', url: 'https://x', etag: 'W/"v2"' }],
    };
    expect(conflictingEntries(BASELINE, disk, [editN1])).toEqual([]);
  });

  it('同一实体两边都动过 ⇒ 进冲突集', () => {
    const disk = {
      ...BASELINE,
      servers: [{ id: 'n1', name: '香港 IEPL', port: 443 }, BASELINE.servers[1]],
    };
    expect(conflictingEntries(BASELINE, disk, [editN1]).map((e) => e.id)).toEqual(['server:n1']);
  });

  /**
   * **T1-14 的边界**：字段级 diff 会把「两人改同一节点的不同字段」判成可合并 —— 而那正是最该问
   * 用户的情形（半个旧节点 + 半个新节点是谁都没要过的第三种东西）。
   *
   * 牙：把判据改成「逐字段比对、只有同一字段两边都改了才算冲突」→ 本条转红。
   */
  it('同一实体的不同字段 ⇒ 仍是冲突（实体粒度是语义，不是实现便利）', () => {
    const disk = {
      ...BASELINE,
      // 磁盘侧改的是 name，staged 改的是 port —— 字段级 diff 会判「不冲突」。
      servers: [{ id: 'n1', name: '香港 IEPL 01', port: 443 }, BASELINE.servers[1]],
    };
    expect(conflictingEntries(BASELINE, disk, [editN1])).toHaveLength(1);
  });

  /** staged 引用的实体在磁盘侧已被删 ⇒ 冲突（Q8 最后一行）。 */
  it('磁盘侧把该实体删了 ⇒ 冲突', () => {
    const disk = { ...BASELINE, servers: [BASELINE.servers[1]] };
    expect(conflictingEntries(BASELINE, disk, [editN1])).toHaveLength(1);
  });

  /**
   * **T1-15：比对基准必须是 baseline，不能是 effectiveConfig。**
   *
   * `effectiveConfig = replay(baseline, entries)` 已经含着用户自己的改动，拿它当基准等于问
   * 「磁盘和我改完之后的样子一不一样」—— 答案恒为「不一样」⇒ 恒冲突 ⇒ 自动合并腿永不可达、
   * 每次保存都弹窗。这条把那个错误配置直接演出来。
   *
   * 牙：把 store 里传给 `conflictingEntries` 的第一个实参从 `baseline` 换成
   * `replay(baseline, entries)` → 第二个断言那种「本该静默合并」的情形会变成冲突 → 转红。
   */
  it('用 effectiveConfig 当基准会恒冲突（故基准只能是 baseline）', () => {
    const effective = replay(BASELINE, [editN1]);
    // 正确用法：磁盘没变 ⇒ 无冲突。
    expect(conflictingEntries(BASELINE, BASELINE, [editN1])).toEqual([]);
    // 错误用法：同样「磁盘没变」，却被判成冲突。
    expect(conflictingEntries(effective, BASELINE, [editN1])).toHaveLength(1);
  });

  it('多条条目各自独立判定，只回冲突的那些', () => {
    const editPort = {
      id: 'setting:mixedPort',
      kind: 'setting' as const,
      label: '改混合端口',
      entityPath: ['mixedPort'],
      nextValue: 1080,
    };
    const disk = { ...BASELINE, mixedPort: 7891 };
    expect(conflictingEntries(BASELINE, disk, [editN1, editPort]).map((e) => e.id)).toEqual([
      'setting:mixedPort',
    ]);
  });
});

// ─────────────────────── 集合 → 主键映射（appRules 以 appId 寻址）───────────────────────

/**
 * 共同牙：把 `ID_ADDRESSED_COLLECTIONS` 退回 `Set` / 把 `primaryKey()` 写死成 `'id'`
 * → 本组每一条都转红（`AppRule` 结构里没有 `id` 字段，寻址会全部落空）。
 */
describe('主键寻址 —— 集合按自己的主键字段寻址，不恒是 id', () => {
  const APP_BASE = {
    appRules: [
      { appId: 'netflix', action: 'proxy', enabled: true },
      { appId: 'openai', action: 'direct', enabled: true },
    ],
    customAppPresets: [
      { id: 'custom-a', name: 'A', emoji: '🌐', geositeTags: ['a'] },
      { id: 'custom-b', name: 'B', emoji: '🌐', geositeTags: ['b'] },
    ],
  };
  const appRuleEntry = (appId: string, nextValue: unknown): StagedEntry => ({
    id: `appRule:${appId}`,
    kind: 'appRule',
    label: `应用策略 ${appId}`,
    entityPath: ['appRules', appId],
    nextValue,
  });

  it('校验按 appId 比对：主键相符放行、写成 id 拒收', () => {
    expect(isValidStagedEntry(appRuleEntry('netflix', { appId: 'netflix', action: 'block' }))).toBe(
      true,
    );
    // 最难查的一类静默错位：id 对了但主键（appId）对不上 ⇒ 重放会写到「另一个实体」上。
    expect(isValidStagedEntry(appRuleEntry('netflix', { id: 'netflix', action: 'block' }))).toBe(
      false,
    );
    expect(isValidStagedEntry(appRuleEntry('netflix', { appId: 'openai' }))).toBe(false);
  });

  it('重放按 appId 覆盖 / 追加 / 删除，不动同集合其它元素', () => {
    const edited = replay(APP_BASE, [
      appRuleEntry('netflix', { appId: 'netflix', action: 'block', enabled: true }),
    ]);
    expect(edited.appRules).toEqual([
      { appId: 'netflix', action: 'block', enabled: true },
      { appId: 'openai', action: 'direct', enabled: true },
    ]);
    const added = replay(APP_BASE, [appRuleEntry('spotify', { appId: 'spotify', action: 'direct' })]);
    expect(added.appRules.map((r) => r.appId)).toEqual(['netflix', 'openai', 'spotify']);
    const removed = replay(APP_BASE, [appRuleEntry('netflix', null)]);
    expect(removed.appRules.map((r) => r.appId)).toEqual(['openai']);
  });

  it('entitySnapshot 同样按 appId 取那一条（否则冲突检出恒 ABSENT ⇒ 恒冲突）', () => {
    expect(entitySnapshot(APP_BASE, ['appRules', 'openai'])).toBe(
      JSON.stringify({ action: 'direct', appId: 'openai', enabled: true }),
    );
    expect(isAbsentSnapshot(entitySnapshot(APP_BASE, ['appRules', 'nope']))).toBe(true);
  });

  it('同表里主键仍是 id 的集合不受影响（映射不是全局改名）', () => {
    const p = replay(APP_BASE, [
      {
        id: 'appPreset:custom-a',
        kind: 'appPreset',
        label: '移除应用 A',
        entityPath: ['customAppPresets', 'custom-a'],
        nextValue: null,
      },
    ]);
    expect(p.customAppPresets.map((x) => x.id)).toEqual(['custom-b']);
  });

  /**
   * 牙：把 `AppPolicyScreen.removeCustomApp` 的第二条条目（删 appRules）去掉 → 重放后盘上留下
   * 指向已删预设的孤儿规则 → 两侧不等 → 转红。这条钉的正是「整族一起进暂存」的那个理由。
   */
  it('一次删除产生的两条条目，重放结果 ≡ 直接删（预设 + 其分流规则）', () => {
    const staged = replay(APP_BASE, [
      {
        id: 'appPreset:custom-a',
        kind: 'appPreset',
        label: '移除应用 A',
        entityPath: ['customAppPresets', 'custom-a'],
        nextValue: null,
      },
      appRuleEntry('netflix', null),
    ]);
    // 直落盘那条腿做的两件事：filter 掉预设 + filter 掉它的规则。
    const direct = {
      appRules: APP_BASE.appRules.filter((r) => r.appId !== 'netflix'),
      customAppPresets: APP_BASE.customAppPresets.filter((p) => p.id !== 'custom-a'),
    };
    expect(staged).toEqual(direct);
  });
});

// ─────────────────────────── 原子撤销组（groupId）───────────────────────────

/**
 * 一次用户动作产生的**跨集合多条条目**必须一起撤。只撤一半会留下「预设还在、规则没了」——
 * 与 Q8-b 拒绝字段级 diff 同型的「谁都没要过的第三种东西」。
 */
describe('原子撤销组 —— 同组条目撤销时连坐', () => {
  const GROUP = 'appRemove:custom-a';
  const presetDel: StagedEntry = {
    id: 'appPreset:custom-a',
    kind: 'appPreset',
    label: '移除应用 A',
    entityPath: ['customAppPresets', 'custom-a'],
    nextValue: null,
    groupId: GROUP,
  };
  const ruleDel: StagedEntry = {
    id: 'appRule:custom-a',
    kind: 'appRule',
    label: '移除应用规则 A',
    entityPath: ['appRules', 'custom-a'],
    nextValue: null,
    groupId: GROUP,
  };
  const loner = serverEntry('n1', { port: 8443 });

  /** 牙：把 `revertEntry` 的组腿删掉（恒按 id 过滤）→ 只走一条 → 转红。 */
  it('撤组内任一条 ⇒ 全组消失（两个方向都试）', () => {
    const staged = stageEntry(stageEntry([], presetDel), ruleDel);
    expect(revertEntry(staged, presetDel.id)).toEqual([]);
    expect(revertEntry(staged, ruleDel.id)).toEqual([]);
  });

  /**
   * 组内条目**跨集合**（`customAppPresets` / `appRules`，路径无共同前缀）⇒ 分组只能是显式字段。
   * 牙：把 `groupId` 换成「按 `entityPath[1]` 推断同组」→ 下面那条 `appRules` 里 id 相同但不同组的
   * 条目会被误连坐 → 转红。
   */
  it('跨集合仍连坐，且不牵连同集合的别组条目', () => {
    const otherGroup: StagedEntry = {
      id: 'appRule:other',
      kind: 'appRule',
      label: '别的应用规则',
      entityPath: ['appRules', 'custom-a'],
      nextValue: null,
      groupId: 'appRemove:custom-b',
    };
    const staged = [presetDel, ruleDel, otherGroup, loner];
    expect(revertEntry(staged, ruleDel.id)).toEqual([otherGroup, loner]);
  });

  /** 牙：把「无组」腿也改成按组过滤（`undefined` 当成一个组）→ 无组条目会互相连坐 → 转红。 */
  it('无组条目：撤一条只走一条，其余无组条目原样', () => {
    const other = serverEntry('n2');
    const staged = stageEntry(stageEntry([], loner), other);
    expect(revertEntry(staged, loner.id)).toEqual([other]);
    // 无组与有组混排：撤无组那条不碰组。
    const mixed = [presetDel, ruleDel, loner];
    expect(revertEntry(mixed, loner.id)).toEqual([presetDel, ruleDel]);
  });

  /**
   * 牙：把校验里的 `groupId` 那行删掉（畸形被静默当「无组」）→ 下面三条 `false` 断言转红。
   * 静默接受的后果不是「少一个字段」，是这一组退化成可拆撤销 —— 恰好还原出本字段要消灭的状态。
   */
  it('畸形分组标识 ⇒ 整条条目非法（不是悄悄剥掉字段）', () => {
    expect(isValidStagedEntry({ ...presetDel, groupId: 123 })).toBe(false);
    expect(isValidStagedEntry({ ...presetDel, groupId: '' })).toBe(false);
    expect(isValidStagedEntry({ ...presetDel, groupId: ['a'] })).toBe(false);
    // 正向对照：缺席与非空串都合法（否则上面三条可能只是「这条条目本来就非法」）。
    expect(isValidStagedEntry(presetDel)).toBe(true);
    expect(isValidStagedEntry({ ...presetDel, groupId: undefined })).toBe(true);
    expect(isValidStagedEntry(loner)).toBe(true);
  });

  /** 牙：`stageEntry` 若把同组当同一条（按 groupId 去重）→ 长度变 1 → 转红。 */
  it('同组不同 id 是两条独立条目；同 id 覆盖语义不变', () => {
    const staged = stageEntry(stageEntry([], presetDel), ruleDel);
    expect(staged).toHaveLength(2);
    const again = stageEntry(staged, { ...ruleDel, label: '改了文案' });
    expect(again).toHaveLength(2);
    expect(again[1].label).toBe('改了文案');
  });

  /**
   * 牙：`encodeStagedPayload` 若逐字段挑着写（漏掉 `groupId`）→ 恢复后连坐失效 → 转红。
   * 这条落在**持久化**上：staged 跨重启存活，组丢了等于重启后又能拆撤销。
   */
  it('持久化往返保住分组，且恢复出来的条目仍连坐', () => {
    const staged = [presetDel, ruleDel, loner];
    const back = decodeStagedPayload(encodeStagedPayload({ baseline: BASE, entries: staged }));
    expect(back?.entries.map((e) => e.groupId)).toEqual([GROUP, GROUP, undefined]);
    expect(revertEntry(back!.entries, presetDel.id)).toEqual([loner]);
  });

  /** 牙：校验放行畸形 `groupId` ⇒ 这条坏条目会被 decode 保留 → 转红。 */
  it('载荷里带畸形分组标识的条目在解码时被丢弃，其余条目仍恢复', () => {
    const raw = JSON.stringify({
      baseline: BASE,
      entries: [{ ...presetDel, groupId: 7 }, loner],
    });
    expect(decodeStagedPayload(raw)?.entries).toEqual([loner]);
  });
});

// ─────────────────────────── 顺序条目（整集合主键序列）───────────────────────────

describe('顺序条目 —— entityPath 单段 = 集合本身', () => {
  const ORDER_BASE = {
    customRules: [
      { id: 'r1', type: 'domain', enabled: true },
      { id: 'r2', type: 'domain', enabled: true },
      { id: 'r3', type: 'domain', enabled: true },
    ],
  };
  const order = (ids: string[]): StagedEntry => ({
    id: 'order:customRules',
    kind: 'rule',
    label: '调整规则顺序',
    entityPath: ['customRules'],
    nextValue: ids,
  });
  const ids = (cfg: { customRules: { id: string }[] }) => cfg.customRules.map((r) => r.id);

  /** 牙：把 `isValidStagedEntry` 的顺序腿删掉（回到「集合键当首段只准两段」）→ 条目被丢 → 转红。 */
  it('校验：主键序列放行，非字符串数组 / 空串拒收', () => {
    expect(isValidStagedEntry(order(['r3', 'r1', 'r2']))).toBe(true);
    expect(isValidStagedEntry(order([]))).toBe(true); // 空序列 = 不动任何元素（全落 rest）
    expect(isValidStagedEntry({ ...order([]), nextValue: null })).toBe(false);
    expect(isValidStagedEntry({ ...order([]), nextValue: [1, 2] })).toBe(false);
    expect(isValidStagedEntry({ ...order([]), nextValue: ['r1', ''] })).toBe(false);
  });

  /** 牙：把 `reorderCollection` 改成「按序列 splice 插入」这类增量实现 → 重放两次结果不同 → 转红。 */
  it('T1-1 幂等：同一顺序条目重放两次结果相同', () => {
    const once = replay(ORDER_BASE, [order(['r3', 'r1', 'r2'])]);
    expect(ids(once)).toEqual(['r3', 'r1', 'r2']);
    expect(replay(once, [order(['r3', 'r1', 'r2'])])).toEqual(once);
  });

  /** 牙：把「序列里查不到的 id 跳过」改成 `list[rank.get(id)]` 直取 → undefined 混进数组 / 抛 → 转红。 */
  it('序列引用了已被删除的 id ⇒ 忽略，不抛、不产生空洞', () => {
    expect(ids(replay(ORDER_BASE, [order(['r9', 'r3', 'r1'])]))).toEqual(['r3', 'r1', 'r2']);
    // 集合本身不存在 ⇒ 形状零变化（不凭空补一个空数组键）。
    expect(replay({ mixedPort: 7890 }, [order(['r1'])])).toEqual({ mixedPort: 7890 });
  });

  /**
   * 牙：把 `replay` 的两趟合回单趟 reduce → 「先排序后新增」与「先新增后排序」结果不同 → 转红。
   *
   * 语义：顺序条目描述的是用户在列表里看到的那个排列，而那个列表含同批的新增/删除。
   */
  it('与同批增/删条目可交换（两趟：实体在前、顺序在后）', () => {
    const addR4: StagedEntry = {
      id: 'rule:r4',
      kind: 'rule',
      label: '新建规则 r4',
      entityPath: ['customRules', 'r4'],
      nextValue: { id: 'r4', type: 'domain', enabled: true },
    };
    const delR2: StagedEntry = {
      id: 'rule:r2',
      kind: 'rule',
      label: '删除规则 r2',
      entityPath: ['customRules', 'r2'],
      nextValue: null,
    };
    const o = order(['r4', 'r3', 'r1']);
    const expected = ['r4', 'r3', 'r1'];
    expect(ids(replay(ORDER_BASE, [o, addR4, delR2]))).toEqual(expected);
    expect(ids(replay(ORDER_BASE, [addR4, delR2, o]))).toEqual(expected);
    expect(ids(replay(ORDER_BASE, [addR4, o, delR2]))).toEqual(expected);
  });

  it('序列没提到的元素落末尾并保持原相对序（同批新增未被拖过的情形）', () => {
    expect(ids(replay(ORDER_BASE, [order(['r3'])]))).toEqual(['r3', 'r1', 'r2']);
  });

  /**
   * 「排序 + 同批改了其中一条规则」—— 两件事互不吃掉对方。
   * 牙：把两趟顺序倒过来（先顺序后实体）→ 仍绿；把顺序条目的重放改成整份替换 `customRules`
   * （拿 id 序列当集合写回去）→ 那条规则的字段全没了 → 转红。
   */
  it('排序 + 同批改了其中一条规则：顺序按序列、内容按实体条目', () => {
    const editR1: StagedEntry = {
      id: 'rule:r1',
      kind: 'rule',
      label: '编辑规则 r1',
      entityPath: ['customRules', 'r1'],
      nextValue: { id: 'r1', type: 'domainSuffix', enabled: false },
    };
    const out = replay(ORDER_BASE, [editR1, order(['r2', 'r1', 'r3'])]);
    expect(ids(out)).toEqual(['r2', 'r1', 'r3']);
    expect(out.customRules[1]).toEqual({ id: 'r1', type: 'domainSuffix', enabled: false });
  });

  /**
   * 顺序实体的快照 = **主键序列**，不是元素内容。
   * 牙：把 `entitySnapshot` 的顺序腿删掉（退回整份集合序列化）→ 第一条断言转红
   * （别人只改了 enabled 就被判成顺序冲突，弹窗噪音）。
   */
  it('冲突检出按主键序列判：改元素内容不冲突、改次序才冲突', () => {
    const o = order(['r3', 'r2', 'r1']);
    const contentChanged = {
      customRules: [
        { id: 'r1', type: 'domain', enabled: false },
        { id: 'r2', type: 'domain', enabled: true },
        { id: 'r3', type: 'domain', enabled: true },
      ],
    };
    expect(conflictingEntries(ORDER_BASE, contentChanged, [o])).toEqual([]);
    const orderChanged = { customRules: [...ORDER_BASE.customRules].reverse() };
    expect(conflictingEntries(ORDER_BASE, orderChanged, [o])).toHaveLength(1);
  });
});

/** 牙：把 `deleteSubscription` 从 `BYPASS_TABLE` 删掉 → 转红（它会掉回默认腿进暂存）。 */
describe('W-3 补充：删订阅是级联原子操作，绕过暂存', () => {
  it('deleteSubscription 按 op 命中绕过表', () => {
    expect(isBypassedOp('deleteSubscription')).toBe(true);
    expect(BYPASS_TABLE.find((r) => r.op === 'deleteSubscription')?.predicate).toBe('W-3');
  });
});
