/**
 * staged-only 行内动作策略（`ENTITY_ACTION_TABLE` + `splitStagedOnly` + `entryForEntity`）的门。
 *
 * # 这里测的是「分法」，不是「谁调它」
 *
 * 哪个调用点该走哪条腿，由 `lib/entity-action-wiring.test.ts` 的源码守卫钉（那是结构事实）。
 * 本文件钉的是**分法本身**：给定一批 id 和一个 staged-only 集合，切出来的两半对不对。
 *
 * # 四条要求各自防什么
 *
 *  1. **策略只有一处**：调用点拿到的是查表结果，不是自己判的。表里改一条策略，行为立刻跟着变 ——
 *     若某个调用点把策略写死了，它就不会跟着变（下面有一条专测这个）。
 *  2. **删除 = 撤销，要有正向对照**：盘上真有的实体必须走**真删除**。只测「staged-only 走 revert」
 *     是半条断言 —— 一律 revert 也能让它绿，而那意味着用户删不掉任何一个已保存的节点。
 *  3. **开关关着逐字节不变**：`stagedOnly` 为空 ⇒ `backend` 是**入参本体**（`toBe`），
 *     另外两半是同一个空数组常量。等值断言验不出这条。
 *  4. **撤销的是条目、不是实体**：`revertEntryIds` 装的必须是条目 id；按 `entityPath` 反查而不是
 *     解析 `${kind}:${id}` 这个约定，否则换个铸 id 的入口就静默失配。
 */
import { describe, it, expect } from 'vitest';

import {
  ENTITY_ACTION_TABLE,
  entryForEntity,
  splitStagedOnly,
  stagedOnlyStrategyOf,
  type StagedEntry,
} from './staged-config';

/** 条目 id 刻意**不**取 `server:<实体 id>` 的约定形态：反查若靠解析 id，本文件会当场转红。 */
const addN3: StagedEntry = {
  id: 'entry-created-by-import-42',
  kind: 'server',
  label: '导入节点 新加坡 03',
  entityPath: ['servers', 'n3'],
  nextValue: { id: 'n3', name: '新加坡 03' },
};

const addN4: StagedEntry = {
  id: 'entry-created-by-clone-7',
  kind: 'server',
  label: '添加节点 首尔 04',
  entityPath: ['servers', 'n4'],
  nextValue: { id: 'n4', name: '首尔 04' },
};

const addR3: StagedEntry = {
  id: 'entry-rule-dup-9',
  kind: 'rule',
  label: '新建规则 副本',
  entityPath: ['customRules', 'r3'],
  nextValue: { id: 'r3', type: 'domain' },
};

/**
 * **盘上已有**的节点被编辑过 ⇒ 它有条目、但不是 staged-only。正向对照全靠它：
 * 若只拿一个「没有条目的盘上节点」当对照，`splitStagedOnly` 里「找不到条目就落回 backend」
 * 那条兜底会替真正的判据把测试蒙绿（实跑验证过：删掉 staged-only 判据后旧对照仍全绿）。
 */
const editN1: StagedEntry = {
  id: 'entry-edit-n1',
  kind: 'server',
  label: '编辑节点 香港 01',
  entityPath: ['servers', 'n1'],
  nextValue: { id: 'n1', name: '香港 01（改）' },
};

const ENTRIES: readonly StagedEntry[] = [addN3, addN4, addR3, editN1];
const NONE: ReadonlySet<string> = new Set();

describe('要求 4：开关关着 ⇒ 逐字节不变（stagedOnly 恒空）', () => {
  it('backend 是**入参本体**，另外两半是空的', () => {
    // 变异对照（已实跑）：删掉 `stagedOnly.size === 0` 早退，本断言立刻红 —— 那条腿会重建数组，
    // 让每次删除都新造一个 id 数组。`toEqual` 版本验不出。
    const ids = ['n1', 'n2'];
    const split = splitStagedOnly('server.delete', ids, NONE, ENTRIES, 'servers');
    expect(split.backend).toBe(ids);
    expect(split.revertEntryIds).toEqual([]);
    expect(split.blocked).toEqual([]);
  });

  it('三条已裁定的 op 在空集下给出同一结果（谁都不会因为策略不同而分叉）', () => {
    const ids = ['n1'];
    for (const op of ['server.delete', 'server.deleteBatch', 'server.speedTest']) {
      expect(splitStagedOnly(op, ids, NONE, ENTRIES, 'servers').backend).toBe(ids);
    }
  });
});

describe('要求 3：删除 = 撤销，且盘上的实体走真删除（正向对照）', () => {
  it('staged-only ⇒ 进 revertEntryIds，且装的是**条目 id**', () => {
    const split = splitStagedOnly('server.delete', ['n3'], new Set(['n3']), ENTRIES, 'servers');
    // 变异对照（已实跑）：把 `entryForEntity` 换成按 `server:${id}` 解析条目 id，本断言红 ——
    // 这几条条目的 id 刻意不是那个形态（真实入口铸 id 的方式可以变，entityPath 不会）。
    expect(split.revertEntryIds).toEqual(['entry-created-by-import-42']);
    expect(split.backend).toEqual([]);
  });

  it('**正向对照**：盘上真有、且被编辑过的实体必须走后端真删除，不得退化成一律 revert', () => {
    // n1 在盘上（不在 stagedOnly 里）**但有条目**（用户改过它）。这正是能戳穿「一律 revert」的那个用例：
    // 拿一个没有条目的 id 当对照会被「找不到条目就落回 backend」的兜底蒙混过去（实跑确认过）。
    // 变异对照（已实跑）：删掉 `if (!stagedOnly.has(id))` 那条判据，本断言红 ——
    // 那意味着用户「删除一个已保存但改过的节点」时只撤销了自己的编辑，节点还在盘上。
    const split = splitStagedOnly('server.delete', ['n1'], new Set(['n3']), ENTRIES, 'servers');
    expect(split.backend).toEqual(['n1']);
    expect(split.revertEntryIds).toEqual([]);
  });

  it('混合一批：两半各走各的腿，且互不吃掉对方', () => {
    const split = splitStagedOnly(
      'server.deleteBatch',
      ['n1', 'n3', 'n2', 'n4'],
      new Set(['n3', 'n4']),
      ENTRIES,
      'servers'
    );
    expect(split.backend).toEqual(['n1', 'n2']);
    expect(split.revertEntryIds).toEqual(['entry-created-by-import-42', 'entry-created-by-clone-7']);
  });

  it('规则用自己的集合寻址（拿 servers 去找规则条目必然落空）', () => {
    expect(
      splitStagedOnly('rule.delete', ['r3'], new Set(['r3']), ENTRIES, 'customRules')
        .revertEntryIds
    ).toEqual(['entry-rule-dup-9']);
    // 集合传错 ⇒ 找不到条目 ⇒ 落回后端，让后端如实报错，而不是静默吞掉这次删除。
    expect(
      splitStagedOnly('rule.delete', ['r3'], new Set(['r3']), ENTRIES, 'servers').backend
    ).toEqual(['r3']);
  });

  it('staged-only 却找不到条目 ⇒ 落回后端（不静默吞）', () => {
    const split = splitStagedOnly('server.delete', ['ghost'], new Set(['ghost']), ENTRIES, 'servers');
    expect(split.backend).toEqual(['ghost']);
    expect(split.revertEntryIds).toEqual([]);
  });
});

describe('要求 1：策略只写在表里 —— 分法跟着表走，不是跟着调用点走', () => {
  it('block 策略 ⇒ 进 blocked，既不下发也不撤销', () => {
    const split = splitStagedOnly('server.speedTest', ['n3'], new Set(['n3']), ENTRIES, 'servers');
    expect(split.blocked).toEqual(['n3']);
    expect(split.backend).toEqual([]);
    expect(split.revertEntryIds).toEqual([]);
  });

  it('同一批 id、同一 staged-only 集合，换个 op 就换一条腿（证明分法读的是表）', () => {
    // 变异对照（已实跑）：把 `splitStagedOnly` 里的 `stagedOnlyStrategyOf(op)` 换成写死的 'revert'，
    // 本断言红 —— 那时 speedTest 也会去撤销用户的条目（点一下测速把节点删了）。
    const args = [['n3'], new Set(['n3']), ENTRIES, 'servers'] as const;
    expect(splitStagedOnly('server.delete', ...args).revertEntryIds.length).toBe(1);
    expect(splitStagedOnly('server.speedTest', ...args).blocked.length).toBe(1);
  });

  it('每条裁定逐条对得上表（表被改动即在此处说话）', () => {
    expect(stagedOnlyStrategyOf('server.delete')).toBe('revert');
    expect(stagedOnlyStrategyOf('server.deleteBatch')).toBe('revert');
    expect(stagedOnlyStrategyOf('rule.delete')).toBe('revert');
    expect(stagedOnlyStrategyOf('server.speedTest')).toBe('block');
    expect(stagedOnlyStrategyOf('subscription.deleteNodeCount')).toBe('disk-only');
    // 组网单例三条：**预防性** block（今天走不到，前提由 config-write-wiring 的 T5 钉住）。
    // 显式登记而不是靠 `stagedOnlyStrategyOf` 的默认腿 —— 默认腿救得了行为，救不了登记表的完整性。
    expect(stagedOnlyStrategyOf('server.tailscaleLogout')).toBe('block');
    expect(stagedOnlyStrategyOf('warp.edit')).toBe('block');
    expect(ENTITY_ACTION_TABLE.length).toBe(7);
  });
});

describe('要求 4 之二：entryForEntity 按 entityPath 寻址', () => {
  it('命中集合 + 主键才算数', () => {
    expect(entryForEntity(ENTRIES, 'servers', 'n3')).toBe(addN3);
    expect(entryForEntity(ENTRIES, 'customRules', 'r3')).toBe(addR3);
    expect(entryForEntity(ENTRIES, 'servers', 'r3')).toBeUndefined();
    expect(entryForEntity(ENTRIES, 'servers', 'nope')).toBeUndefined();
  });

  it('单段路径（整集合顺序条目）不得被当成实体条目', () => {
    const order: StagedEntry = {
      id: 'order:customRules',
      kind: 'rule',
      label: '调整规则顺序',
      entityPath: ['customRules'],
      nextValue: ['r1'],
    };
    // 变异对照（已实跑）：把长度判据 `e.entityPath.length === 2` 去掉，本断言红 ——
    // 撤销一次删除会连带把用户的排序一起撤了。
    expect(entryForEntity([order], 'customRules', 'customRules')).toBeUndefined();
  });
});
