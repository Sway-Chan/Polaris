import { readFileSync } from 'node:fs';
import { describe, it, expect } from 'vitest';
import {
  pendingChangesCount,
  hasPendingWork,
  applyOutcome,
  applyStateOnLifecycle,
  isRestartFailureCode,
  composeBarView,
  stagedPhaseOf,
  pendingPhaseOf,
  type ApplyPhase,
  type ApplyStatus,
  type BarInput,
  type PendingPhase,
  type StagedPhase,
} from './pending-bar-logic';

const PENDING_NONE = { added: [], modified: [], removed: [], restartDeferred: false };
const PENDING_ONE = { ...PENDING_NONE, added: ['a'] };

/** 合成表的入参默认取「表第一行」（clean × none），逐格只覆盖它关心的那几项。 */
function barInput(over: Partial<BarInput> = {}): BarInput {
  return {
    stagedPhase: 'clean',
    pendingPhase: 'none',
    stagedCount: 0,
    pendingCount: 0,
    saveError: null,
    applyError: null,
    // 既有各格默认「核在跑」——它们量的是 A×B 合成，不是运行态闸门。
    // 闸门本身另有专门的 describe（见文件末「coreRunning 闸门」）。
    coreRunning: true,
    ...over,
  };
}

const ids = (input: Partial<BarInput>): string[] =>
  composeBarView(barInput(input)).actions.map((a) => a.id);

const disabledIds = (input: Partial<BarInput>): string[] =>
  composeBarView(barInput(input))
    .actions.filter((a) => a.disabled)
    .map((a) => a.id);

describe('pendingChangesCount', () => {
  // 三个集合都是「运行核尚未吃进去的差异」，都由「立即应用」解决 → 都计入。
  // 变异对照：从实现里去掉 removed 那一项 → 本条转红（条上数字会比 popover 行数少）。
  it('计数 = added + modified + removed', () => {
    expect(pendingChangesCount({ added: ['a', 'b'], modified: ['c'], removed: ['d'], restartDeferred: false })).toBe(4);
  });

  // 变异对照：去掉 modified 那一项 → 本条转红。这条同时钉住 P1 修的那个症状——
  // modified 曾恒空，导致「测速说已编辑未生效，而 bar 上没有那个节点」。
  it('只有 modified 时也必须计数（modified 不再恒空）', () => {
    expect(pendingChangesCount({ added: [], modified: ['a', 'b'], removed: [], restartDeferred: false })).toBe(2);
  });

  it('只有 removed 时也必须计数', () => {
    expect(pendingChangesCount({ added: [], modified: [], removed: ['a'], restartDeferred: false })).toBe(1);
  });

  it('全空 → 0（操作条不渲染）', () => {
    expect(pendingChangesCount({ added: [], modified: [], removed: [], restartDeferred: false })).toBe(0);
  });

  // 核未运行 / 畸形对象缺字段 → 不抛，按 0 降级
  it('畸形对象缺字段 → 不抛，恒 0', () => {
    expect(
      pendingChangesCount({} as unknown as Parameters<typeof pendingChangesCount>[0])
    ).toBe(0);
  });

  // 变异对照：把某一项的 `?? 0` 去掉 → 本条转红（后端降级/旧版载荷缺键即整条崩）。
  it('部分缺字段（只有 added）→ 按已有项计数，不抛', () => {
    expect(
      pendingChangesCount({ added: ['a'] } as unknown as Parameters<
        typeof pendingChangesCount
      >[0])
    ).toBe(1);
  });
});

describe('applyOutcome（spec §2.5 Q8：applied 不再报成功）', () => {
  // C-8：`applied` 只意味着 schedule_restart() 已排程，重启成没成走 proxyStarted/proxyError。
  // 变异对照：把 applied 腿改回 `{phase:'idle', toast:{kind:'success',…}}` → 本条转红。
  // 这正是 Q8 表第三行点名的洞：「排程了但核没起来」时旧实现会报「已应用」。
  it('applied / deferred → 进 applying 态，且不弹 toast', () => {
    expect(applyOutcome('applied')).toEqual({ phase: 'applying', toast: null });
    expect(applyOutcome('deferred')).toEqual({ phase: 'applying', toast: null });
  });

  // skipped = 核未运行，没有重启在飞 → 不进 applying（否则条会卡在「应用中…」等一个永不到来的
  // proxyStarted）。变异对照：把 skipped 并进 applied 腿 → 本条转红。
  it('skipped（核未运行）→ 回 idle + info toast', () => {
    expect(applyOutcome('skipped')).toEqual({
      phase: 'idle',
      toast: { kind: 'info', key: 'home.pendingSkippedNotRunning' },
    });
  });

  // IPC 失败（null）/ 未知态 → 失败 + error toast，绝不静默吞。
  // 变异对照：把 default 腿改成 `{phase:'idle', toast:null}` → 本条转红。
  it('null / undefined（IPC 失败或未知态）→ failed + error toast', () => {
    for (const s of [null, undefined]) {
      expect(applyOutcome(s)).toEqual({
        phase: 'failed',
        toast: { kind: 'error', key: 'home.pendingApplyFailed' },
      });
    }
  });

  /**
   * 「toast 说失败而条仍是琥珀」这种同一件事两个说法，此前靠 `isApplyFailure` 与
   * `applyResultToastKey` 同源来守。P4 把两者并成一个函数后，同源变成结构性成立，
   * 于是这条门改为跨 `applyOutcome` → `composeBarView` 断言：凡判 failed 的状态，条必红。
   *
   * 变异对照：把 `composeBarView` 的 `clean × applyFailed` 那格 `err` 改成 false → 本条转红。
   */
  it('凡 applyOutcome 判 failed 的状态，条必进 `.err` 红', () => {
    const ALL: (ApplyStatus | null | undefined)[] = [
      'applied',
      'deferred',
      'skipped',
      null,
      undefined,
    ];
    for (const s of ALL) {
      const phase = applyOutcome(s).phase;
      const view = composeBarView(
        barInput({ pendingPhase: pendingPhaseOf(phase, PENDING_ONE) })
      );
      expect(view.err, `状态 ${String(s)} 上「判失败」与「条转红」分叉`).toBe(phase === 'failed');
    }
  });
});

describe('isRestartFailureCode（哪些 proxyError 算「这次立即应用没落地」）', () => {
  // 这三个在 runtime/proxy.rs 的 code 模块逐条注明「非终态」，走 set_nonfatal_error（核仍在跑）。
  // 变异对照：把其中任一从 CORE_STILL_RUNNING_CODES 里删掉 → 本条转红（用户会看到一条
  // 「应用失败」而核其实好好地跑着新配置）。
  it('三个非终态码（核仍在跑）不算本次 apply 失败', () => {
    expect(isRestartFailureCode('SYSTEM_PROXY_FAILED')).toBe(false);
    expect(isRestartFailureCode('EXIT_MISMATCH')).toBe(false);
    expect(isRestartFailureCode('RULE_RESOURCES_MISSING')).toBe(false);
  });

  // 变异对照：把判据反过来写成「正列失败码」并漏掉 TUN_ROUTE_NOT_CAPTURED → 本条转红。
  // 漏判失败码的代价是条永远停在「应用中…」，用户没有任何出口，比多一次可点掉的红严重得多。
  it('终态码与未知码一律算失败（取补集，不正列）', () => {
    expect(isRestartFailureCode('PROCESS_EXITED')).toBe(true);
    expect(isRestartFailureCode('STARTUP_FAILED')).toBe(true);
    expect(isRestartFailureCode('TUN_ROUTE_NOT_CAPTURED')).toBe(true);
    expect(isRestartFailureCode('SOME_CODE_ADDED_NEXT_YEAR')).toBe(true);
    expect(isRestartFailureCode(undefined)).toBe(true);
  });
});

describe('hasPendingWork', () => {
  const none = { added: [], modified: [], removed: [], restartDeferred: false };

  // 变异对照：把实现改回 `pendingChangesCount(pending) > 0` → 本条转红。
  // 守的正是「保存不重启」在 UI 上完全无痕这个洞：改 mixedPort/TUN/DNS 一个节点都不动，
  // 三个数组恒空，条若只看计数就永不出现。
  it('节点差集全空但 restartDeferred → 条必须出现', () => {
    expect(hasPendingWork({ ...none, restartDeferred: true })).toBe(true);
  });

  it('全空且无欠账 → 条不出现', () => {
    expect(hasPendingWork(none)).toBe(false);
  });

  // 变异对照：把实现改成 `pending.restartDeferred === true`（丢掉计数腿）→ 本条转红。
  it('有节点差集就出现，与 restartDeferred 无关', () => {
    expect(hasPendingWork({ ...none, added: ['a'] })).toBe(true);
    expect(hasPendingWork({ ...none, added: ['a'], restartDeferred: true })).toBe(true);
  });

  // 后端降级 / 旧版载荷缺键 → 按「没有欠账」降级。**不得**因缺键把条恒亮：
  // 一条永远消不掉的「待应用」比不显示更糟（用户点「立即应用」也清不掉它）。
  it('畸形对象缺字段 → 不抛，判 false', () => {
    expect(hasPendingWork({} as unknown as Parameters<typeof hasPendingWork>[0])).toBe(false);
  });

  // 非布尔真值（后端将来若误发字符串）→ 严格比较挡住，不按 truthy 放行。
  it('restartDeferred 非严格 true 不算欠账', () => {
    expect(
      hasPendingWork({ ...none, restartDeferred: 'yes' as unknown as boolean })
    ).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// §2.4 合成呈现规则 —— 表逐行落地。表是 SoT，这里逐格钉住，改表必须同步改这里。
// ---------------------------------------------------------------------------
describe('composeBarView：A=clean 那四行', () => {
  // 变异对照：把 `case 'none'` 的 HIDDEN 换成任何 visible 视图 → 转红（条会在无事可做时常驻 36px）。
  it('clean × none → 隐藏', () => {
    expect(composeBarView(barInput()).visible).toBe(false);
  });

  // 变异对照：把 titleKey 改成 pendingStagedTitle → 转红（「待应用」被说成「待保存」，
  // 而这两句话对应的用户下一步动作完全不同）。
  it('clean × pending → 「N 项待应用」+ 仅[立即应用]', () => {
    const v = composeBarView(barInput({ pendingPhase: 'pending', pendingCount: 3 }));
    expect(v).toMatchObject({ visible: true, err: false, titleKey: 'home.pendingBarTitle' });
    expect(v.titleVars).toEqual({ count: 3 });
    expect(v.actions).toEqual([{ id: 'apply', disabled: false }]);
  });

  // restartDeferred-only：三个节点数组恒空但确有欠账。
  // 变异对照：删掉 `pendingCount === 0` 那个三元 → 条上出现「0 项待应用」→ 转红。
  it('clean × pending 且计数为 0（只有 restartDeferred）→ 不许说「0 项待应用」', () => {
    const v = composeBarView(barInput({ pendingPhase: 'pending', pendingCount: 0 }));
    expect(v.titleKey).toBe('home.pendingBarConfigOnly');
    expect(v.titleVars).toEqual({});
  });

  // 变异对照：把 apply 的 disabled 改成 false → 转红（用户能在重启在飞时再点一次，连点叠 force-restart）。
  it('clean × applying → 「应用中…」+ 按钮在位但禁用', () => {
    const v = composeBarView(barInput({ pendingPhase: 'applying' }));
    expect(v).toMatchObject({ visible: true, err: false, titleKey: 'home.pendingApplying' });
    expect(v.actions).toEqual([{ id: 'apply', disabled: true }]);
  });

  // 变异对照：把 err 改成 false → 转红（`.pending-bar.err` 又变回死 CSS，失败只剩 2.2s 的 toast）。
  it('clean × applyFailed → `.err` + [重试][忽略]', () => {
    const v = composeBarView(barInput({ pendingPhase: 'applyFailed' }));
    expect(v).toMatchObject({ visible: true, err: true, titleKey: 'home.pendingApplyFailed' });
    expect(v.actions).toEqual([
      { id: 'retryApply', disabled: false },
      { id: 'dismissApply', disabled: false },
    ]);
  });

  // 变异对照：把带原因那条腿删掉（恒用无原因 key）→ 转红。后端给了原因却不显示 = 用户只知道
  // 「失败了」，不知道下一步该干什么。
  it('applyFailed 有原因 ⇒ 换成带 {{reason}} 的那句', () => {
    const v = composeBarView(barInput({ pendingPhase: 'applyFailed', applyError: '核起不来' }));
    expect(v.titleKey).toBe('home.pendingApplyFailedReason');
    expect(v.titleVars).toEqual({ reason: '核起不来' });
  });
});

describe('composeBarView：A=staged 那三行 + 一格 spec 缺口', () => {
  // 变异对照：从 stagedActions 里去掉 'apply' → 转红。表里 staged×none 明写三颗按钮：
  // 有 staged 但差集为空时，「立即应用」= 保存 + 重启，仍是用户要的那条快路。
  it('staged × none → 「N 项待保存」+ [重置][保存][立即应用]', () => {
    const v = composeBarView(barInput({ stagedPhase: 'staged', stagedCount: 2 }));
    expect(v).toMatchObject({ visible: true, err: false, titleKey: 'home.pendingStagedTitle' });
    expect(v.titleVars).toEqual({ count: 2 });
    expect(ids({ stagedPhase: 'staged', stagedCount: 2 })).toEqual(['reset', 'save', 'apply']);
    expect(disabledIds({ stagedPhase: 'staged', stagedCount: 2 })).toEqual([]);
  });

  // 变异对照：把 titleVars 写成单个 count → 转红。这一格必须同时说出两个数，
  // 「5 项待保存 → 保存 → 2 项待应用」的收缩全靠它们对得上（S-7）。
  it('staged × pending → 两个数都要说出来', () => {
    const v = composeBarView(
      barInput({ stagedPhase: 'staged', pendingPhase: 'pending', stagedCount: 2, pendingCount: 3 })
    );
    expect(v.titleKey).toBe('home.pendingStagedAndPending');
    expect(v.titleVars).toEqual({ staged: 2, pending: 3 });
  });

  // 变异对照：删掉 pendingCount===0 的分支 → 条上出现「另有 0 项待应用」→ 转红。
  it('staged × pending 且待应用计数为 0 → 不许说「另有 0 项待应用」', () => {
    const v = composeBarView(
      barInput({ stagedPhase: 'staged', pendingPhase: 'pending', stagedCount: 2, pendingCount: 0 })
    );
    expect(v.titleKey).toBe('home.pendingStagedAndConfig');
    expect(v.titleVars).toEqual({ count: 2 });
  });

  // 表注：这一格真实可达（点完「立即应用」后在核重启完成前又改了别的）。
  // 变异对照：把 apply 的 disabled 去掉 → 转红；把 reset/save 也禁掉 → 转红
  //（重启在飞不该拦住用户继续保存别的编辑）。
  it('staged × applying → [重置][保存] 可用、[立即应用] 禁用', () => {
    const input = { stagedPhase: 'staged' as StagedPhase, pendingPhase: 'applying' as PendingPhase, stagedCount: 2 };
    expect(composeBarView(barInput(input)).titleKey).toBe('home.pendingApplyingWithStaged');
    expect(ids(input)).toEqual(['reset', 'save', 'apply']);
    expect(disabledIds(input)).toEqual(['apply']);
  });

  /**
   * **spec §2.4 表里没有这一格**（已在交付里单列）。可达路径：保存成功清空 staged → apply 失败 →
   * 用户又改了别的。落法按两维各自贡献推出：A 给三颗，B 额外给[忽略]并点红。
   * 变异对照：让它退回 `clean × applyFailed` 的两颗按钮 → 转红（用户手上的 N 项待保存
   * 会连同[重置][保存]一起从条上消失，只剩一个说不清的红）。
   */
  it('staged × applyFailed（spec 缺口，按两维贡献补）→ 红 + 三颗 + [忽略]', () => {
    const input = { stagedPhase: 'staged' as StagedPhase, pendingPhase: 'applyFailed' as PendingPhase, stagedCount: 2 };
    expect(composeBarView(barInput(input)).err).toBe(true);
    expect(ids(input)).toEqual(['reset', 'save', 'apply', 'dismissApply']);
  });
});

describe('composeBarView：A 的两个动作生命周期态盖住整行 B（表末两行的 `*`）', () => {
  const ALL_B: PendingPhase[] = ['none', 'pending', 'applying', 'applyFailed'];

  // 变异对照：把 saving 的分派挪到 clean/staged 之后（不再优先）→ 表末行的 `*` 失效 → 转红。
  it('saving × 任意 B → 「保存中…」且全禁用', () => {
    for (const b of ALL_B) {
      const input = { stagedPhase: 'saving' as StagedPhase, pendingPhase: b, stagedCount: 2 };
      const v = composeBarView(barInput(input));
      expect(v, `B=${b}`).toMatchObject({ visible: true, err: false, titleKey: 'home.pendingSaving' });
      expect(disabledIds(input), `B=${b} 保存在飞时还有可点的按钮`).toEqual([
        'reset',
        'save',
        'apply',
      ]);
    }
  });

  // NFR-1 的 UI 面：保存失败后条目一条不丢，条给的是 [重置][重试保存]。
  // 变异对照：在这一格加上 'dismissApply'/'apply' → 转红（「忽略」一次失败的保存
  // 会让用户以为改动还在路上，而它其实哪也没去）。
  it('saveFailed × 任意 B → `.err` + [重置][重试保存]', () => {
    for (const b of ALL_B) {
      const input = { stagedPhase: 'saveFailed' as StagedPhase, pendingPhase: b, stagedCount: 2 };
      expect(composeBarView(barInput(input)).err, `B=${b}`).toBe(true);
      expect(ids(input), `B=${b}`).toEqual(['reset', 'retrySave']);
    }
  });

  it('saveFailed 有原因 ⇒ 换成带 {{reason}} 的那句', () => {
    const v = composeBarView(barInput({ stagedPhase: 'saveFailed', saveError: 'EACCES' }));
    expect(v.titleKey).toBe('home.pendingSaveFailedReason');
    expect(v.titleVars).toEqual({ reason: 'EACCES' });
  });

  // 变异对照：给任一可见格漏写 titleKey（留空串）→ 转红。一条 36px 的空白琥珀条比不显示更糟。
  it('凡可见的格必有文案', () => {
    const ALL_A: StagedPhase[] = ['clean', 'staged', 'saving', 'saveFailed'];
    for (const a of ALL_A) {
      for (const b of ALL_B) {
        const v = composeBarView(barInput({ stagedPhase: a, pendingPhase: b, stagedCount: 1, pendingCount: 1 }));
        if (!v.visible) continue;
        expect(v.titleKey, `${a} × ${b} 是一条没文案的空条`).not.toBe('');
      }
    }
  });
});

describe('两个维度各自的取值（合成的入口，不得在组件里另写一套）', () => {
  // 变异对照：把 `saveStatus !== 'idle'` 改成只判 'saving' → saveFailed 格永不可达 → 转红。
  it('stagedPhaseOf：动作态优先于条目数派生', () => {
    expect(stagedPhaseOf('idle', 0)).toBe('clean');
    expect(stagedPhaseOf('idle', 2)).toBe('staged');
    expect(stagedPhaseOf('saving', 2)).toBe('saving');
    // 保存失败后条目仍在（NFR-1），此时必须是 saveFailed 而不是 staged。
    expect(stagedPhaseOf('saveFailed', 2)).toBe('saveFailed');
  });

  // 变异对照：把 hasPendingWork 换回 `pendingChangesCount>0` → 最后一条转红
  //（restartDeferred 那笔欠账在 UI 上完全无痕）。
  it('pendingPhaseOf：本次 apply 的生命周期优先于静态差集', () => {
    expect(pendingPhaseOf('idle', PENDING_NONE)).toBe('none');
    expect(pendingPhaseOf('idle', PENDING_ONE)).toBe('pending');
    expect(pendingPhaseOf('applying', PENDING_ONE)).toBe('applying');
    expect(pendingPhaseOf('failed', PENDING_ONE)).toBe('applyFailed');
    // 差集已清空但重启还在飞：条要继续说「应用中…」，不能提前隐身。
    expect(pendingPhaseOf('applying', PENDING_NONE)).toBe('applying');
    expect(pendingPhaseOf('idle', { ...PENDING_NONE, restartDeferred: true })).toBe('pending');
  });
});

/**
 * 「立即应用」的运行态闸门。
 *
 * 变异锁：把 `gateApply` 改成恒等（不过滤）→ 下面每一条都转红。
 * 判据是**不渲染**而非禁用：禁用读作「现在不能点」，而核没在跑时这件事根本不存在
 * ——改动本就会在下次起核时带上，摆一颗按钮只会让用户以为自己漏做了一步。
 */
describe('composeBarView —— coreRunning 闸门', () => {
  const ids = (v: ReturnType<typeof composeBarView>) => v.actions.map((a) => a.id);

  it('核没跑 + 有 staged：只剩 [重置][保存]，没有「立即应用」', () => {
    const v = composeBarView(barInput({ stagedPhase: 'staged', stagedCount: 2, coreRunning: false }));
    expect(ids(v)).toEqual(['reset', 'save']);
  });

  it('核在跑 + 有 staged：三颗齐（回归对照，证明上一条不是把按钮全删了）', () => {
    const v = composeBarView(barInput({ stagedPhase: 'staged', stagedCount: 2, coreRunning: true }));
    expect(ids(v)).toEqual(['reset', 'save', 'apply']);
  });

  it('核没跑 + applyFailed：连「重试应用」也不出，只留「忽略」', () => {
    const v = composeBarView(
      barInput({ stagedPhase: 'clean', pendingPhase: 'applyFailed', coreRunning: false })
    );
    expect(ids(v)).toEqual(['dismissApply']);
  });

  it('核没跑 + 保存中：禁用态的那颗「立即应用」同样不渲染', () => {
    const v = composeBarView(barInput({ stagedPhase: 'saving', coreRunning: false }));
    expect(ids(v)).toEqual(['reset', 'save']);
  });
});

/**
 * **停核不产生「未保存」状态**（陈先生 2026-07-30：「我手动停止内核之后，提示我保存」）。
 *
 * 「脏/未保存」（A 维度）与「待应用差集」（B 维度）是**两个状态、两个真值源**：
 * A = `staged-config-store.entries`（渲染端，磁盘之前）；B = 后端 `pending_changes()`（磁盘 vs 运行核）。
 * 停核只动 B —— 后端没了「运行核」这个分母，差集恒空（`runtime/proxy.rs` 的 `empty()` 腿），
 * 而 `entries` 一个字节都不碰。故停核**只能**让条从有变无，绝不可能凭空长出一个「保存」。
 *
 * 这两条把该不变式钉在合成层：B 侧无论取什么值、核跑不跑，A=clean 时都不许出现[保存]/[重试保存]，
 * 文案也不许说「待保存」。变异对照：在 `composeBarView` 任一 `stagedPhase === 'clean'` 行里
 * 加一颗 `act('save')`（例如「核没跑就把「立即应用」换成「保存」」这种想当然的落法）→ 立刻转红。
 */
/**
 * 「应用中…」的收场判据（`event:proxyLifecycle` → ApplyState）。
 *
 * 这条通道存在的**全部理由**：后端 `proxyStarted/Stopped` 只由命令层发，「立即应用」触发的是后端
 * 自驱的去抖重启，两条都不发 ⇒ 条此前只能等 12s 兜底轮询。而「差集变空」**不能**代替它 ——
 * 起核失败时差集同样为空，拿它判成功会把失败报成成功。
 */
describe('applyStateOnLifecycle —— 收场判据必须能区分成功与失败', () => {
  /** 与真实调用形一致（`t(key)`，无兜底参）；返键名即可，本组只关心「取的是键还是后端串」。 */
  const t = (key: string) => key;

  // 变异对照：去掉 `current !== 'applying'` 早退 → 本条转红。托盘启停 / 后台自愈的结局不该
  // 被算成「本次 apply 的收场」（会把一条与用户动作无关的失败画成红）。
  it('不在 applying 态 → 一律 null（别人的起停不是本次 apply 的事）', () => {
    for (const p of ['idle', 'failed'] as ApplyPhase[]) {
      expect(applyStateOnLifecycle(p, { phase: 'ready' }, t)).toBeNull();
      expect(applyStateOnLifecycle(p, { phase: 'failed', message: 'x' }, t)).toBeNull();
      expect(applyStateOnLifecycle(p, { phase: 'stopped' }, t)).toBeNull();
    }
  });

  // 变异对照：让 ready 落 'failed' 或返回 null → 转红（前者凭空报错，后者退回等 12s 轮询）。
  it('ready → 成功收场', () => {
    expect(applyStateOnLifecycle('applying', { phase: 'ready' }, t)).toEqual({
      phase: 'idle',
      reason: null,
    });
  });

  // **这一条是本通道相对「差集变空」的全部价值**。
  // 变异对照：把 failed 腿删掉（只按 ready/stopped 分派）→ 转红 ⇒ 起核失败会被当成功静默收场。
  //
  // 同时是三段式**第 2 段**的实测锚：本例无 `errorCode`（后端 `StartError.code` 为 `Option`，
  // 「无可诚实断言的分类」那类失败腿确实不带码），故必须原样交出后端那句**准确**的话，
  // 而不是换成一句无信息量的通用错误。变异对照：把第 2 段删掉（`errorCode` 未命中就直接 `null`）
  // → 本条转红，且真实后果是条上只剩「应用失败」、用户不知道栽在哪。
  it('failed 且无 errorCode → 落失败态并原样带上后端给的原因（三段式第 2 段）', () => {
    expect(
      applyStateOnLifecycle('applying', { phase: 'failed', message: '核起不来：端口被占' }, t)
    ).toEqual({ phase: 'failed', reason: '核起不来：端口被占' });
  });

  // 三段式**第 1 段**：有码且前端有键 ⇒ 取键，**压过**后端那串中文 message。
  // 变异对照：把优先级换回 `message || t(key)` → 本条转红（拿到的是中文串），
  // 而那正是 ru/fa 用户看到「应用失败：核崩了」的成因。
  it('failed 且 errorCode 有对应键 → 用键，压过后端中文 message（三段式第 1 段）', () => {
    expect(
      applyStateOnLifecycle(
        'applying',
        { phase: 'failed', errorCode: 'HELPER_NOT_INSTALLED', message: 'helper 尚未安装' },
        t
      )
    ).toEqual({ phase: 'failed', reason: 'errors.helperNotInstalledDesc' });
  });

  // 豁免码（`PROXY_ERROR_KEY_EXEMPT`）走第 2 段：它们**故意**不配键，因为 message 里那句
  // 才带得出「这一次」的可诊断信息。变异对照：给 STARTUP_FAILED 补一个键 → 本条转红。
  it('failed 且 errorCode 在豁免表里 → 回落 message（豁免不等于静默）', () => {
    expect(
      applyStateOnLifecycle(
        'applying',
        { phase: 'failed', errorCode: 'STARTUP_FAILED', message: 'sing-box 启动期退出: bad config' },
        t
      )
    ).toEqual({ phase: 'failed', reason: 'sing-box 启动期退出: bad config' });
  });

  // 后端「无可诚实断言的分类」那一类只有 message、甚至可能没有；两者都不许把 undefined 画到条上。
  // 变异对照：把实现改成 `reason: event.message ?? null`（不 trim）→ 空白串那条转红 ⇒ 条会显示
  // 「应用失败：」后面跟一片空白。
  it('failed 但没有可用原因 → reason 归 null（三段式第 3 段：落不带 {{reason}} 的那句文案）', () => {
    expect(applyStateOnLifecycle('applying', { phase: 'failed' }, t)).toEqual({
      phase: 'failed',
      reason: null,
    });
    expect(applyStateOnLifecycle('applying', { phase: 'failed', message: '   ' }, t)).toEqual({
      phase: 'failed',
      reason: null,
    });
  });

  // 变异对照：把 stopped 归到 failed 腿 → 转红。停核可能正是用户自己点的、也可能是重启的停核腿
  // 刚跑完（起核还在路上），判失败会红得毫无道理。
  it('stopped → 回 idle，绝不判失败', () => {
    expect(applyStateOnLifecycle('applying', { phase: 'stopped' }, t)).toEqual({
      phase: 'idle',
      reason: null,
    });
  });
});

/**
 * **接线守卫**（前端侧，与 Rust 的 `lifecycle_push_is_paired_with_the_diff_push` 同族）：
 * 纯函数测绿 ≠ 它真被挂上了。上面那批只证「判据对」，若组件压根没订阅该通道，判据就是死代码、
 * 缺陷原样复发 —— 这正是「测方法体 ≠ 测接线」那类假绿。
 *
 * 变异对照：删掉 `PendingChangesBar.tsx` 里那条 `api.proxy.onLifecycle(...)` 订阅 → 转红；
 * 订阅了但不走纯函数（就地手写 if/else）→ 第二条断言转红。
 */
describe('接线：条真的订阅了 lifecycle 通道并走纯函数判定', () => {
  const SRC = readFileSync(
    new URL('./PendingChangesBar.tsx', import.meta.url),
    'utf8'
  ).replace(/^\s*(\/\/|\*|\/\*).*$/gm, ''); // 去整行注释：注释里也逐字写着这些调用

  it('订阅了 api.proxy.onLifecycle', () => {
    expect(SRC).toContain('api.proxy.onLifecycle(');
  });

  it('收场判定走 applyStateOnLifecycle，而不是在组件里另写一套', () => {
    expect(SRC).toContain('applyStateOnLifecycle(');
  });

  it('订阅被卸载时解绑（漏了就是每次重挂多一个监听器）', () => {
    expect(SRC).toContain('offLifecycle()');
  });
});

describe('A/B 不混同 —— 停核（只动 B）不得产出「未保存」语义', () => {
  const ALL_B: PendingPhase[] = ['none', 'pending', 'applying', 'applyFailed'];
  /** A 维度专属的动作与文案：B 侧任何取值都不得产出它们。 */
  const A_ONLY_ACTIONS = ['save', 'retrySave', 'reset'];
  const A_ONLY_TITLES = [
    'home.pendingStagedTitle',
    'home.pendingStagedAndPending',
    'home.pendingStagedAndConfig',
    'home.pendingSaving',
    'home.pendingSaveFailed',
    'home.pendingSaveFailedReason',
    'home.pendingApplyingWithStaged',
  ];

  it('A=clean × B 全取值 × 核跑/不跑：没有「保存」类按钮，文案也不提「待保存」', () => {
    for (const b of ALL_B) {
      for (const coreRunning of [true, false]) {
        const v = composeBarView(
          barInput({ stagedPhase: 'clean', stagedCount: 0, pendingPhase: b, coreRunning })
        );
        for (const bad of A_ONLY_ACTIONS) {
          expect(
            v.actions.map((a) => a.id),
            `B=${b} coreRunning=${coreRunning}：B 侧凭空产出了 A 维度的「${bad}」`
          ).not.toContain(bad);
        }
        expect(
          A_ONLY_TITLES,
          `B=${b} coreRunning=${coreRunning}：B 侧的文案说成了「待保存」`
        ).not.toContain(v.titleKey);
      }
    }
  });

  // 停核后前端那次 pull 必得空集（后端无分母 → `empty()` 腿）⇒ 条整条隐身，而不是换成「保存」。
  // 变异对照：让 `pendingPhaseOf` 在核停时返回 'pending'（或让 clean × none 变 visible）→ 转红。
  it('停核后的真实入参（差集空 + 核没跑 + 无 staged）→ 条整条隐藏', () => {
    expect(pendingPhaseOf('idle', PENDING_NONE)).toBe('none');
    expect(
      composeBarView(barInput({ stagedPhase: 'clean', pendingPhase: 'none', coreRunning: false }))
        .visible
    ).toBe(false);
  });
});
