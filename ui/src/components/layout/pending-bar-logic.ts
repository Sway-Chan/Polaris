import type { PendingNodeChanges } from '@/contracts/types';
import {
  proxyErrorReason,
  type ProxyErrorTranslate,
} from '@/domain/proxy-error-text';

/**
 * 待应用差集操作条纯逻辑（抽出以便 node 环境 vitest 直测，同 connect-button-state.ts / status-bar-display.ts）。
 *
 * 与组件同居 `components/layout/`：条已从 Home 内联卡片迁到 `AppShell` 的 docked 全局位（P2），
 * 本文件是它的唯一消费方。
 */

/**
 * 操作条计数「N 项待应用」= added + modified + removed —— 三个集合都是「运行核尚未吃进去的差异」，
 * 都能被「立即应用」那颗按钮解决，故都该计入；漏计哪一类，条上的数字就与明细 popover 的行数对不上。
 *
 * 缺字段（核未运行/IPC 降级时的畸形对象）→ 该项按 0 降级，绝不抛。
 */
export function pendingChangesCount(pending: PendingNodeChanges): number {
  return (
    (pending?.added?.length ?? 0) +
    (pending?.modified?.length ?? 0) +
    (pending?.removed?.length ?? 0)
  );
}

/**
 * 条该不该出现 —— **不等于** `pendingChangesCount(pending) > 0`。
 *
 * `restartDeferred` 是「保存不重启」降级下来的非节点结构性变更（`mixedPort` / TUN / DNS …）：
 * 它一个节点都不动，三个数组恒空，但确实需要重启才生效。只按计数判可见性，
 * 「保存」在 UI 上就完全无痕 —— 用户保存了、什么也没发生、也没人说还差一步。
 */
export function hasPendingWork(pending: PendingNodeChanges): boolean {
  return pendingChangesCount(pending) > 0 || pending?.restartDeferred === true;
}

/** `applyPendingChanges()` 的后端三态（真运行态判定：运行=applied / 在飞=deferred / 未运行=skipped）。 */
export type ApplyStatus = 'applied' | 'deferred' | 'skipped';

/** 条的 B 维度里由「这一次 apply」驱动的那部分（§2.4 维度 B 的 applying / applyFailed 两态）。 */
export type ApplyPhase = 'idle' | 'applying' | 'failed';

export interface ApplyOutcome {
  phase: ApplyPhase;
  /** 顺带的 toast；`null` = 不弹。 */
  toast: { kind: 'info' | 'error'; key: string } | null;
}

/**
 * apply 排程结果 → 条的下一态 + 是否顺带 toast。
 *
 * **`applied` 不再报「已应用」**（spec §2.5 Q8 的第三行）：C-8 —— `apply_pending` 的返回值只表达
 * **排程结果**，`applied` 仅意味着 `schedule_restart()` 已排程，重启到底成没成走的是另一条通道
 * （`event:proxyStarted` / `event:proxyError`）。此前直接报 success 的写法会在「排程了但核没起来」时
 * 说谎，而那正是最需要如实说的一格。故 applied/deferred 一律进 `applying` 态、**不弹 toast**：
 * 条本身持续显示「应用中…」，而 toast 2.2s 就消失，用它表达一个还在进行中的过程只会误导。
 *
 * `skipped`（核未运行）不是失败，也不进 applying —— 没有重启在飞，下次起核自然纳入，给一次 info 即可。
 * `null`/`undefined`（IPC 失败或未知态）→ 失败态 + error toast，绝不静默吞（点了「立即应用」没反应
 * 与按钮失灵不可区分）。
 */
/**
 * 「应用中…」等多久就去查一次真运行态（兜底出口）。
 *
 * 存在的理由不是「重启可能慢」，而是**有真实失败路径一个事件都不发** —— TUN 模式下重启撞上提权
 * 助手安装门、用户取消时，`runtime/proxy.rs` 那类极早期失败自陈「无 emitter」，分类只随 `invoke`
 * 返回值出栈，而 `applyPendingChanges` 早已返回。没有这条兜底，条会永远停在「应用中…」。
 *
 * 取 12s：起核正常路径（含 TUN 建栈 + 系统代理设置）实测在数秒内；给慢盘/首次授权留一倍余量，
 * 又不至于让用户对着转圈干等太久。到点后**查状态再判**，不盲目判失败。
 */
export const APPLY_CONFIRM_TIMEOUT_MS = 12_000;

export function applyOutcome(status: ApplyStatus | null | undefined): ApplyOutcome {
  switch (status) {
    case 'applied':
    case 'deferred':
      return { phase: 'applying', toast: null };
    case 'skipped':
      return { phase: 'idle', toast: { kind: 'info', key: 'home.pendingSkippedNotRunning' } };
    default:
      return { phase: 'failed', toast: { kind: 'error', key: 'home.pendingApplyFailed' } };
  }
}

/**
 * `event:proxyError` 里**哪些码代表核仍在跑** —— 这几个在 `runtime/proxy.rs` 的 `code` 模块里逐条
 * 注明「非终态」，走 `set_nonfatal_error`（保留 running/pid/端口），不是本次重启没起来。
 *
 * 判据取补集（不在表里的一律算失败）而非正列失败码：漏判一个失败码 ⇒ 条永远停在「应用中…」、
 * 用户没有任何出口；漏判一个非终态码 ⇒ 多一次可点掉的红。两害相权取后者。
 *
 * `CORE_BINARY_MISMATCH` 是**起核后**的内核自证告警（实跑二进制 ≠ 本次期望的核）——核这时已经
 * 起来并在服务流量，只是跑的是旧核。把它误判成「重启没落地」会让条卡死在「应用中…」，而实际上
 * 那次 apply 已经成功了。
 */
const CORE_STILL_RUNNING_CODES: ReadonlySet<string> = new Set([
  'SYSTEM_PROXY_FAILED',
  'EXIT_MISMATCH',
  'RULE_RESOURCES_MISSING',
  'CORE_BINARY_MISMATCH',
]);

/** 这条 `event:proxyError` 是否意味着「这次立即应用的重启没落地」。 */
export function isRestartFailureCode(code: string | undefined): boolean {
  return code === undefined || !CORE_STILL_RUNNING_CODES.has(code);
}

/** 条里由「这一次 apply」驱动的那部分状态（`phase` + 失败原因）。 */
export interface ApplyState {
  phase: ApplyPhase;
  /** 只在 `failed` 时有意义。 */
  reason: string | null;
}

/**
 * `event:proxyLifecycle` → 「应用中…」该怎么收场。**纯函数**，返回 `null` = 这一帧与本次 apply 无关。
 *
 * # 为什么它才是收场的权威判据（而不是「差集变空」）
 *
 * 后端起核**失败**时起核快照同样是空 ⇒ 差集同样为空。拿差集判成功会把失败报成成功，
 * 而这正是本条要堵的那种谎。本事件显式携带结局，三个 phase 各有确定去向：
 *
 * - `ready` —— 核按新配置起来了 ⇒ 成功收场（`idle`）。差集随后由后端同刻推的空差集清掉，条自然隐身。
 * - `failed` —— 这次起核没回来 ⇒ `failed` + 一句用户可见的原因，由 `proxyErrorReason` 三段式解出
 *   （`errorCode` 命中键 → 译文；否则回落后端 `message`；两者皆无 → `null`，
 *   由 `composeBarView` 落到不带 `{{reason}}` 的那句，绝不显示 "undefined"）。
 * - `stopped` —— 核停了就谈不上「正在应用」⇒ 回 `idle`。**不判失败**：停核可能正是用户自己点的
 *   （或重启的停核腿刚跑完、起核还在路上），把它算成「应用失败」会红得毫无道理。
 *
 * # 为什么 `reason` 必须经 `proxyErrorReason` 而不是直接用 `event.message`
 *
 * `message` 是 Rust 侧写死的中文串，直接塞进 `home.pendingApplyFailedReason`（「应用失败：{{reason}}」）
 * 就是半句本地化、半句中文 —— 俄语/波斯语用户实测如此。载荷里的 `errorCode` 才是可跨语种的分类。
 * **本条也是三段式唯一全段可达的消费点**：`errorCode` 在本通道是**真可选**的（`StartError.code`
 * 为 `Option`，「无可诚实断言分类」的失败腿不带码），而 `STARTUP_FAILED` / `ROOT_ORPHAN_BLOCKED`
 * 又刻意不配键（理由见 `contracts/proxy-error-key-coverage.test.ts` 的 `KEY_EXEMPT`）——
 * 这两类全靠第 2 段把后端那句**准确**的话交出去。
 *
 * # 只在 `applying` 时改态
 *
 * 与既有两条事件腿同一条纪律（`PendingChangesBar` 头注）：别的时候来的起停是别人的事
 * （托盘启停、后台自愈），不该把与本次 apply 无关的结局算进来。
 *
 * # 与 12s 兜底轮询的优先级
 *
 * 本函数是**先到先决**的一方，轮询是**兜底**的一方，两者不会打脸：轮询的 timer 只在
 * `phase === 'applying'` 期间存在，本函数一旦把 phase 推离 `applying`，那条 effect 的 cleanup
 * 立刻 `clearTimeout`。反过来若事件真丢了（或那条极早期失败连本通道都没走到），12s 到点由
 * `getStatus()` 这个更权威的真值收场。两条腿问的是同一件事（核在不在跑），不存在两个答案并存。
 */
export function applyStateOnLifecycle(
  current: ApplyPhase,
  event: { phase: 'ready' | 'stopped' | 'failed'; errorCode?: string; message?: string },
  t: ProxyErrorTranslate
): ApplyState | null {
  if (current !== 'applying') return null;
  if (event.phase === 'failed') {
    // 空串/全空白的 message 与缺字段同等对待（由 `proxyErrorReason` 统一处理）：都落到不带原因的那句文案。
    return { phase: 'failed', reason: proxyErrorReason(event, t) };
  }
  return { phase: 'idle', reason: null };
}

// ─────────────────────── §2.4 两个正交维度的合成（不是一个状态机）───────────────────────

/**
 * 维度 A：staged（前端，磁盘之前）。`clean` / `staged` 由条目数派生，另两个是「保存」这一次动作的生命周期。
 */
export type StagedPhase = 'clean' | 'staged' | 'saving' | 'saveFailed';

/** 维度 B：pending（磁盘 vs 运行核）。 */
export type PendingPhase = 'none' | 'pending' | 'applying' | 'applyFailed';

export type BarActionId = 'reset' | 'save' | 'apply' | 'retryApply' | 'dismissApply' | 'retrySave';

/** `disabled` 与「不出现」是两回事：表里的「禁用」要求按钮仍在位（否则条会在重启期间抖一下）。 */
export interface BarAction {
  id: BarActionId;
  disabled: boolean;
}

export interface BarView {
  visible: boolean;
  /** `.pending-bar.err` 红态。 */
  err: boolean;
  titleKey: string;
  titleVars: Record<string, string | number>;
  /** 副标题（`.pb-tx div`）。空串 = 该行不渲染。 */
  detailKey: string;
  actions: BarAction[];
}

export interface BarInput {
  stagedPhase: StagedPhase;
  pendingPhase: PendingPhase;
  /** staged 条目数（= stagedDiff 大小，Q5：不需要后端算）。 */
  stagedCount: number;
  /** 待应用节点差集计数。0 而 `pendingPhase==='pending'` ⇒ 只有 `restartDeferred` 那笔欠账。 */
  pendingCount: number;
  saveError: string | null;
  applyError: string | null;
  /**
   * 内核当前在跑没有。
   *
   * 决定「立即应用」这颗**出不出现**：它的语义是「保存 + 强制重启内核让改动即刻生效」。
   * 核没在跑时后半句无对象 —— 改动本来就会在下次起核时带上，此时把这颗摆出来，
   * 用户点它得到的要么是一次空转、要么是一次莫名其妙的起核（陈先生 2026-07-30 报）。
   * 故 `false` 时整颗**不渲染**（而非禁用）：禁用是「现在不能点」，不渲染才是「这件事此刻不存在」。
   * 「保存」不受影响 —— 落盘在两种运行态下语义完全相同。
   */
  coreRunning: boolean;
}

const HIDDEN: BarView = {
  visible: false,
  err: false,
  titleKey: '',
  titleVars: {},
  detailKey: '',
  actions: [],
};

const act = (id: BarActionId, disabled = false): BarAction => ({ id, disabled });

/**
 * §2.4 的「合成呈现规则」表逐行落地。
 *
 * **不能塞成一个状态机**（spec 原话）：A×B 是 16 格，硬塞会得到 8 个不可达态。这里按表分派：
 * A 的两个「动作生命周期」态（saving / saveFailed）盖住整行 B（表末两行的 `*`），其余按 (A,B) 取格。
 *
 * 表里没有 `staged × applyFailed` 那一格 —— 它可达（保存成功清空 staged → apply 失败 → 用户又改了别的），
 * 落法按两个维度**各自贡献**推出：A=staged 给 [重置][保存][立即应用]，B=applyFailed 额外给 [忽略]
 * 并把条点红（「立即应用」本身就是那格里的[重试]，不重复出一颗）。**此格属 spec 缺口，已在交付里单列。**
 */
export function composeBarView(input: BarInput): BarView {
  const { stagedPhase, pendingPhase, stagedCount, pendingCount, saveError, applyError, coreRunning } =
    input;

  /** 核没在跑 ⇒ 抹掉全部「重启核」类动作（见 `BarInput.coreRunning`）。收在一处，别逐格判。 */
  const gateApply = (actions: BarAction[]): BarAction[] =>
    coreRunning ? actions : actions.filter((a) => a.id !== 'apply' && a.id !== 'retryApply');

  // ── A 的动作生命周期态覆盖整行 B ──
  if (stagedPhase === 'saving') {
    return {
      visible: true,
      err: false,
      titleKey: 'home.pendingSaving',
      titleVars: {},
      detailKey: '',
      // 全禁用：保存在飞时点重置会丢掉正在落盘的那批意图，点应用会叠一次 force-restart。
      actions: gateApply([act('reset', true), act('save', true), act('apply', true)]),
    };
  }
  if (stagedPhase === 'saveFailed') {
    return {
      visible: true,
      err: true,
      // NFR-1：staged 一条不丢，故这里给的是 [重置][重试保存]，没有「忽略」——
      // 忽略一次失败的保存 = 让用户以为改动还在路上，而它其实哪也没去。
      titleKey: saveError === null ? 'home.pendingSaveFailed' : 'home.pendingSaveFailedReason',
      titleVars: saveError === null ? {} : { reason: saveError },
      detailKey: 'home.pendingDetailTipStaged',
      actions: [act('reset'), act('retrySave')],
    };
  }

  const applyFailedTitle: Pick<BarView, 'titleKey' | 'titleVars'> = {
    titleKey: applyError === null ? 'home.pendingApplyFailed' : 'home.pendingApplyFailedReason',
    titleVars: applyError === null ? {} : { reason: applyError },
  };

  if (stagedPhase === 'clean') {
    switch (pendingPhase) {
      case 'none':
        return HIDDEN;
      case 'pending':
        return {
          visible: true,
          err: false,
          // 计数为 0 却在 pending ⇒ 只有 `restartDeferred`：条上不能说「0 项待应用」。
          titleKey: pendingCount === 0 ? 'home.pendingBarConfigOnly' : 'home.pendingBarTitle',
          titleVars: pendingCount === 0 ? {} : { count: pendingCount },
          detailKey: 'home.pendingDetailTip',
          actions: gateApply([act('apply')]),
        };
      case 'applying':
        return {
          visible: true,
          err: false,
          titleKey: 'home.pendingApplying',
          titleVars: {},
          detailKey: 'home.pendingDetailTip',
          actions: gateApply([act('apply', true)]),
        };
      case 'applyFailed':
        return {
          visible: true,
          err: true,
          ...applyFailedTitle,
          detailKey: 'home.pendingDetailTip',
          actions: gateApply([act('retryApply'), act('dismissApply')]),
        };
    }
  }

  // ── A = staged ──
  const stagedActions = gateApply([act('reset'), act('save'), act('apply')]);
  switch (pendingPhase) {
    case 'none':
      return {
        visible: true,
        err: false,
        titleKey: 'home.pendingStagedTitle',
        titleVars: { count: stagedCount },
        detailKey: 'home.pendingDetailTipStaged',
        actions: stagedActions,
      };
    case 'pending':
      return {
        visible: true,
        err: false,
        // 「另有 0 项待应用」是假话：pendingCount===0 时那笔欠账是非节点结构性变更（restartDeferred）。
        titleKey:
          pendingCount === 0 ? 'home.pendingStagedAndConfig' : 'home.pendingStagedAndPending',
        titleVars:
          pendingCount === 0
            ? { count: stagedCount }
            : { staged: stagedCount, pending: pendingCount },
        detailKey: 'home.pendingDetailTipStaged',
        actions: stagedActions,
      };
    case 'applying':
      return {
        visible: true,
        err: false,
        titleKey: 'home.pendingApplyingWithStaged',
        titleVars: { count: stagedCount },
        detailKey: 'home.pendingDetailTipStaged',
        // 「立即应用」禁用：在飞的重启被第二次 force-restart 打断没有意义（后端 `gate.is_busy()`
        // 会返 deferred，功能上安全），禁用是为了挡住用户连点。
        actions: gateApply([act('reset'), act('save'), act('apply', true)]),
      };
    case 'applyFailed':
      return {
        visible: true,
        err: true,
        ...applyFailedTitle,
        detailKey: 'home.pendingDetailTipStaged',
        actions: [...stagedActions, act('dismissApply')],
      };
  }
}

/** A 维度取值：`saving`/`saveFailed` 来自「保存」这次动作，其余由条目数派生。 */
export function stagedPhaseOf(
  saveStatus: 'idle' | 'saving' | 'saveFailed',
  entryCount: number
): StagedPhase {
  if (saveStatus !== 'idle') return saveStatus;
  return entryCount > 0 ? 'staged' : 'clean';
}

/** B 维度取值：本次 apply 的生命周期优先于「磁盘 vs 运行核」的静态差集。 */
export function pendingPhaseOf(applyPhase: ApplyPhase, pending: PendingNodeChanges): PendingPhase {
  if (applyPhase === 'failed') return 'applyFailed';
  if (applyPhase === 'applying') return 'applying';
  return hasPendingWork(pending) ? 'pending' : 'none';
}
