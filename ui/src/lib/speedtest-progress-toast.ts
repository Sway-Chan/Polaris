/**
 * 测速进度 → **全局** toast 的协调器（对齐 上游 `renderer/components/settings/speed-test-toast.ts`）。
 *
 * # 为什么必须是全局，而不是节点页里那行字
 *
 * 后端 `EVENT_SPEED_TEST_PROGRESS`（`runtime/speedtest.rs::record_measured`）逐节点广播
 * `{tested, ok, total}`，此前只有 `NodesScreen` 订它、渲染成屏内一行文本。而 `ScreenRouter` 是裸
 * `switch`（无 keep-alive）⇒ **切屏即卸载即没了**；测速本来就是「按下去等十几秒」的操作，用户在这十几秒里
 * 去首页看连接态、去设置调东西，是最常见的行为，恰恰这时进度消失。上游 是全局 toast，所以在哪都看得见
 * （陈先生 2026-07-30 指出的正是这个差）。
 *
 * # 四个终止口都收在一处（否则 sticky toast 会挂死在屏上）
 *
 * sticky toast 不自动消失 ⇒ **必须**保证每一轮测速都有终态。本模块把四条路径收敛到同一个状态机：
 *
 *  1. **终态事件**（`EVENT_SPEED_TEST_DONE`，[`reduceSpeedTestDone`]）—— **主路径**。后端三条腿各在
 *     唯一出口广播 `{outcome, tested, total, serverIds, pending}`，`interrupted` 当场转中断态并给出恢复动作；
 *  2. `tested >= total` —— 正常跑完那一帧就地收口，转 `nodes.speedTestDone`（不必等事件绕一圈）；
 *  3. **静默超时**（[`SPEEDTEST_IDLE_TIMEOUT_MS`]）—— 降级为**纯兜底**，见下一节；
 *  4. `total <= 1` —— 不起 toast（`0` 是防御性忽略，`1` 是单节点 ⚡ 的裁定，见下方 guard 处注释）。
 *
 * ## 第 3 条为什么从主路径降级为兜底（2026-07-31 B 批）
 *
 * 改前只有 2、3 两条：中断**没有任何信号**能到达本模块（后端 `"interrupted"` 只写在 `ApiResponse` 的
 * 返回值里，而托盘浮层是独立 JS 堆 ⇒ 主窗的 toast 结构上拿不到），于是只能靠「静默 12s」去**猜**。
 * 陈先生 2026-07-31 的质问正对这一点：「断开为什么还需要等 16s？都已经无核了不是？」——
 * 断开 ⇒ `!running` ⇒ `is_superseded` 立刻为真 ⇒ 后端**当场**就知道，前端却要等十几秒去猜。
 *
 * 改后中断由第 1 条**立即**接住，第 3 条只剩两个残余用途：事件在投递中丢失、后端异常退出（进程没了，
 * 谁也不会再发事件）。它不再决定体验，只决定「最坏情况下 sticky 不会挂死」，故取值可以放宽（见常量注释）。
 *
 * 第 1 条覆盖的真实中断源仍是那两条（它们此前只能靠猜）：
 *  · 核生命周期跃迁打断（`drive_waves` 的 `superseded()` → `abort_all` + 返回 `"interrupted"`）。
 *    ⚠️ 判据是 `is_superseded(gen_now, gen0, running) = gen_now != gen0 || !running`，即**核
 *    start/stop/restart/regen 或崩溃**。**纯热切不在其内**：切节点走 `select_outbound`，不 bump
 *    世代、不改 running；且主核池测速用的是 `probe-selector-k`，与用户出口的 `proxy-selector`
 *    是两个 selector。所以「测速时切节点」只在该切换导致内核重启时才打断（陈先生 2026-07-31 指出
 *    本处原文「点了连接/断开/切节点就会走到」不准确）；
 *  · 测量任务 `JoinError`（`runtime/speedtest.rs` 的 `Ok(Some(Err(_))) => {}`）——该节点**不落账**，
 *    于是即便 outcome 是 `"completed"`，`tested` 也永远到不了 `total`。此时第 1 条照样收口
 *    （`completed` + `pending` 非空），不再需要第 3 条兜。
 *
 * # 中断后的恢复动作：继续剩余 / 重测原范围 / 关闭，**不自动执行**
 *
 * 中断态 toast 提供两个数据动作：`继续剩余` 只测 `pending`，`重新测速` 重测后端随终态事件返回的
 * `serverIds`。二者都在点击后才执行 —— 三条理由：
 *  1. 后端有**进程级单飞闸**（`commands/speedtest.rs::SPEED_TEST_IN_FLIGHT`）。自动重试会和用户的
 *     下一个动作抢闸，表现成「我点了测速却报 in-flight」；
 *  2. 中断的常见来源正是**用户主动切了节点 / 断开**——他此刻多半在做别的事，自动占带宽跑测速反直觉；
 *  3. 无限重试没有天然收敛点（核不稳时会反复触发）。
 * 判定在 [`planSpeedTestRun`]（纯逻辑）：发出前按**当前节点集**过滤（用户可能在这中间删了节点 /
 * 换了订阅）。过滤后为空 → 不发空请求。若中断时一个节点都没完成，`pending === serverIds`，此时
 * 「继续剩余」与「重新测速」完全同义，只保留后者，避免两个按钮执行同一件事。
 *
 * ## 为什么终止信号不从调用点（`speedTest()` 的 finally）取
 *
 * [选 事件流自足]：本模块只依赖广播事件，**与谁发起的无关**。托盘浮层是**独立 webview / 独立 JS 堆**
 *   （`use-latency-store.ts` 文件头已登记这一事实），它那次 `speedTest()` 的 finally 在托盘的堆里跑，
 *   够不到主窗的 toast —— 调用点方案对托盘入口结构上不成立。且新增入口时不会漏接。
 * [不选 调用点 begin/end 引用计数]（上游的做法）：上游 需要它是因为 上游 允许多入口并发测速；
 *   Polaris 后端有**进程级单飞闸**（`commands/speedtest.rs::SPEED_TEST_IN_FLIGHT`，跨窗口收口，
 *   并发一律返 `SPEEDTEST_IN_FLIGHT`）⇒ 同一时刻至多一轮，引用计数无对象可数，照搬即是白造。
 *
 * # 不显示 ok（成功数）、不画进度条 —— 判据
 *
 *  · **运行中的 ok 是噪声**：用户此刻要的是「还要等多久」= `tested/total`；成功数只在终态才有决策含义。
 *  · **终态的 ok 有含义，但已经有更好的呈现**：节点页每张卡的延迟徽标**逐节点**给出通/超时，
 *    信息量严格大于一个汇总数字；toast 只有一行预算，重复一遍低精度版本不改变用户下一步动作。
 *  · **进度条是同一信息的低精度重复**：`12/50` 已经精确；且冻结的原型 CSS（`prototype/components.css`
 *    禁改）里 `.toast` 没有进度条元件，新造要在 `index.css` 补一套并同步浅/深两档主题，
 *    还会让 toast 宽度在测速期间跳动 —— 成本与收益不成比例。
 *  · 直接后果：进度/终态四条文案**零新增 i18n 键**，全部复用 `servers.*` 下**早已五语齐备、却零消费点**的
 *    键（上游 同款字面，1:1 移植时译文先落地、消费点没接上）。恢复动作与关闭入口的新增键
 *    已在五语同批补齐 ⇒ `MISSING_KEY_DEBT`（ru/fa 缺口）不动。
 *
 * # 为什么外部面全靠注入、本模块零运行时 import
 *
 * 只有 `import type`（编译期擦除）。真事件流 / 真 toast / 真 i18n 由唯一调用点 `App.tsx` 装配。
 * 不是为了「可测试」这个空话，而是因为**静态 import 一旦碰到 `../i18n`，本模块在 node 里就再也跑不起来**：
 * `i18n/index.ts:81` 在模块加载期就写 `document.documentElement.dir`，而本仓 vitest 无 jsdom。
 * 换句话说，把 `toast`/`i18n` 直接 import 进来 = 本模块的门整个消失。注入是让门存在的前提。
 */
import type { ToastImpl } from './error-handler';
import type { SpeedTestDonePayload } from '../contracts/speed-test';

/** 后端 `EVENT_SPEED_TEST_PROGRESS` 载荷。 */
export interface SpeedTestProgress {
  tested: number;
  ok: number;
  total: number;
}

export type { SpeedTestDonePayload };

/**
 * 进度 toast 的去重键（语义同 sonner 的 `{ id }` / 上游 `speed-test-toast.ts` 的 `TOAST_ID`）。
 * 全轮测速自始至终只有这一条 —— 没有它，50 个节点会刷出 50 条。
 */
export const SPEEDTEST_TOAST_KEY = 'speedtest-progress';

/**
 * 静默多久判定为「已中断」——**纯兜底**，不再是主路径（主路径是 `EVENT_SPEED_TEST_DONE`，见文件头）。
 *
 * # 取值判据（20s）
 *
 * 上界仍按老口径推导，只是被测量的那个「单节点最坏耗时」变了：后端 2026-07-31 改成**两段预算**
 * （`commands/speedtest.rs` 的 `SPEED_TEST_COLD_TIMEOUT_MS = 6_000` 冷建链 +
 * `SPEED_TEST_REUSE_TIMEOUT_MS = 4_000` 复用请求）⇒ 单节点最坏 **10s**（且这 10s 只在「隧道建起来了
 * 但复用请求卡住」的异常路径上才发生；不可达节点因「首段超时即返回、不发第二次」恒为 6s）。
 * 两次进度事件的间隔上界 = 单节点最坏耗时 ⇒ 2 × 10s = **20s**。
 *
 * # 为什么这次可以放心取大（降级带来的松绑）
 *
 * 改前它是**唯一**的中断信号，取值要在「误判活跑」与「挂死太久」之间走钢丝，故只能贴着 2× 走。
 * 现在中断由终态事件**立即**接住，本值只在事件丢失 / 后端异常退出时才生效 ⇒ 它唯一的失败模式变成
 * 「把一轮还活着的测速误判成中断」（弹一条假的「已完成 x/y」+ 一个点了会白跑的「继续」）—— 取大只会
 * 让这个误判更不可能，代价仅仅是「真的丢事件时多挂 8s」，而那是个本就不该发生的形态。
 *
 * 上界也不是没有：20s 仍明显短于「用户以为卡死」的量级，且期间 toast 一直显示着 `tested/total`
 * 的活进度（不是一个冻结的转圈），用户看得见它在动。
 *
 * ⚠️ 与后端两个常量的关系由 `speedtest-progress-toast.test.ts` 的一条门**直接读 Rust 源文件**做算术
 * 校验（`>= 2 × (cold + reuse)`）—— 改后端超时而不改这里会当场转红。此前只有两边注释互指、无门。
 */
export const SPEEDTEST_IDLE_TIMEOUT_MS = 20_000;

/**
 * 状态机内部态：`live` = 屏上有一条 sticky 进度 toast。
 *
 * `live` 同时是**终态去重闸**：`tested >= total` 那一帧已经收口过了，随后到达的 `done` 事件看到
 * `live === false` 就什么都不做 —— 否则正常跑完的一轮会连弹两条「测速完成」。
 */
export interface SpeedTestToastState {
  live: boolean;
  tested: number;
  total: number;
}

export const initialSpeedTestToastState: SpeedTestToastState = {
  live: false,
  tested: 0,
  total: 0,
};

/**
 * 「该弹什么样一条 toast」的完整描述 —— 含 `sticky`，故「sticky 失效」这个变异在纯逻辑层就能抓到。
 * 文案留成 i18n 键 + 插值（不在这层解析）：纯逻辑单测不该依赖 i18n 初始化。
 */
export interface SpeedTestToastIntent {
  level: 'info' | 'success' | 'warning';
  sticky: boolean;
  msgKey: string;
  msgVars?: Record<string, number>;
  descKey?: string;
  descVars?: Record<string, number>;
  /**
   * 行内动作按钮的**描述**（不是 handler）：`labelKey` = i18n 键，`serverIds` = 该测哪些节点。
   *
   * 刻意不在这里放 `onClick`：这层是纯数据（单测直接比对整个 intent 对象）。真 handler 由
   * [`subscribeSpeedTestProgressToast`] 的 `dispatch` 用 `serverIds` + 注入的外部面组装 ——
   * 「点了之后要按当前节点集再过滤一次」也发生在那里（见 [`planSpeedTestRun`]）。
   */
  actions?: Array<{ labelKey: string; serverIds: string[] }>;
  /** 显式关闭入口；值是已纳入五语校验的 i18n 键。 */
  dismissLabelKey?: string;
}

/** 收到一个进度事件：更新态并给出该弹的 toast（`null` = 什么都不做）。 */
export function reduceSpeedTestProgress(
  state: SpeedTestToastState,
  ev: SpeedTestProgress
): { next: SpeedTestToastState; intent: SpeedTestToastIntent | null } {
  // total<=0：后端不会这么发（`plan_speed_test` 的零可测走的是失败信封，根本不 emit 进度）。
  // 真收到就说明契约破了 —— 此时起一条永不终止的 sticky toast 比不起更糟，故忽略。
  //
  // total===1：**单节点 ⚡ 一并忽略**（陈先生 2026-07-31 裁定：「单节点 ⚡ 不应该弹一条『测速完成』」）。
  // 单节点那次点击的进度只有 0/1 → 1/1 两帧，sticky 那条一闪即被终态顶掉，屏幕上净效果就是
  // 凭空弹一条「测速完成」——而**该节点卡自己的延迟徽标已经把结果写在原地了**，toast 是纯噪音。
  // 本模块存在的理由是「批量测速时进度在别的屏上看不见」，单节点没有这个问题（结果就在手边）。
  if (!Number.isFinite(ev.total) || ev.total <= 1) return { next: state, intent: null };

  if (ev.tested >= ev.total) {
    return {
      next: { live: false, tested: ev.tested, total: ev.total },
      // 终态一律**非 sticky**：它是一次性结论，2.2s 后自散即「收起」——这就是进度 toast 的收口，
      // 同 key 顶掉上一条 sticky，不需要另开一个 dismiss 通道（全库仅此一处会用）。
      intent: { level: 'success', sticky: false, msgKey: 'nodes.speedTestDone' },
    };
  }

  return {
    next: { live: true, tested: ev.tested, total: ev.total },
    intent: {
      level: 'info',
      sticky: true,
      msgKey: 'nodes.speedTestingNodes',
      msgVars: { tested: ev.tested, total: ev.total },
    },
  };
}

/** 静默超时到点：有在跑的进度就收成「测速中断」，否则什么都不做。 */
export function reduceSpeedTestIdle(state: SpeedTestToastState): {
  next: SpeedTestToastState;
  intent: SpeedTestToastIntent | null;
} {
  if (!state.live) return { next: state, intent: null };
  return {
    next: { ...state, live: false },
    intent: {
      level: 'warning',
      sticky: false,
      msgKey: 'nodes.speedTestInterrupted',
      descKey: 'nodes.speedTestInterruptedSummary',
      dismissLabelKey: 'nodes.speedTestDismiss',
      // 兜底腿没有后端载荷可用（事件根本没到），只能报本地看到的最后一帧进度。
      descVars: { tested: state.tested, total: state.total },
    },
  };
}

/**
 * 收到后端终态事件（**主路径**）：把在跑的 sticky 收成结论。
 *
 * - `!state.live` ⇒ 什么都不做。两种到达形态都在这一条里收敛：①正常跑完那一帧已由
 *   [`reduceSpeedTestProgress`] 收口过（否则连弹两条「测速完成」）；②单节点 ⚡ 整轮静音
 *   （`total<=1` 从没起过 toast，此时弹任何东西都是凭空多一条）。
 * - `interrupted` ⇒ 「测速中断」+ 已完成数**取后端载荷**（`ev.tested/ev.total` 才是权威的
 *   「如实上报已完成数」；本地 state 只是最后一帧进度的回声），并按原范围与待测差集挂恢复动作。
 * - `completed` ⇒ 「测速完成」。能走到这里说明 `tested` 没到过 `total`（典型：测量任务 JoinError
 *   导致某节点不落账）—— 后端认为这一轮已经裁定完毕，故照 `completed` 收口，而不是硬等一个
 *   永远不会来的进度事件。
 */
export function reduceSpeedTestDone(
  state: SpeedTestToastState,
  ev: SpeedTestDonePayload
): { next: SpeedTestToastState; intent: SpeedTestToastIntent | null } {
  if (!state.live) return { next: state, intent: null };
  const next = { live: false, tested: ev.tested, total: ev.total };

  if (ev.outcome === 'interrupted') {
    const pending = ev.pending ?? [];
    const serverIds = ev.serverIds ?? [];
    const actions: Array<{ labelKey: string; serverIds: string[] }> = [];
    // 部分完成时「继续剩余」才与重测原范围不同；全未测时只留「重新测速」，避免重复动作。
    if (pending.length > 0 && pending.length < serverIds.length) {
      actions.push({ labelKey: 'nodes.speedTestResume', serverIds: pending });
    }
    if (serverIds.length > 0) {
      actions.push({ labelKey: 'nodes.speedTestRetry', serverIds });
    } else if (pending.length > 0) {
      // 防旧后端/异常载荷：拿不到原范围时仍保住已有的续测能力。
      actions.push({ labelKey: 'nodes.speedTestResume', serverIds: pending });
    }
    return {
      next,
      intent: {
        level: 'warning',
        // sticky:false 是**必须**的（不是随手写的默认）：带动作的 toast 一定要有出路，
        // 判据见 `components/layout/toast-queue.ts::autoDismissMs`。那边还有一道压过 sticky 的兜底。
        sticky: false,
        msgKey: 'nodes.speedTestInterrupted',
        descKey: 'nodes.speedTestInterruptedSummary',
        descVars: { tested: ev.tested, total: ev.total },
        dismissLabelKey: 'nodes.speedTestDismiss',
        ...(actions.length > 0 ? { actions } : {}),
      },
    };
  }

  return {
    next,
    intent: { level: 'success', sticky: false, msgKey: 'nodes.speedTestDone' },
  };
}

/**
 * 测速动作集合的裁定（纯逻辑）：`动作范围 ∩ 当前节点集`，**保序、去重**。
 *
 * # 为什么必须过滤（C4）
 *
 * 中断到用户点恢复动作之间可以隔很久（按钮停留 15s，但用户可能立刻点也可能最后一刻点），这期间他
 * 完全可能删了节点 / 换了订阅 / 节点 id 变了。把一批**已经不存在**的 id 发下去，后端会把它们判成
 * 「请求了但配置里查无此节点」（`missing` → `notInPool`），运气差些整批落空 ⇒ 后端返**失败信封** ⇒
 * 前端 throw。过滤保证恢复动作只在**还有可测节点**时发生。
 *
 * 返回空数组 = 没有可测节点了（调用方据此**不发请求**，直接收掉 toast —— 空请求在后端等于「测全部」，
 * 那是彻底的语义反转）。
 */
export function planSpeedTestRun(serverIds: string[], currentIds: string[]): string[] {
  const alive = new Set(currentIds);
  const seen = new Set<string>();
  return serverIds.filter((id) => {
    if (!alive.has(id) || seen.has(id)) return false;
    seen.add(id);
    return true;
  });
}

/** 可注入的外部面（生产由 `App.tsx` 装配；单测注入假的，故本模块整条链路在 node 环境可跑）。 */
export interface SpeedTestToastDeps {
  subscribe: (listener: (p: SpeedTestProgress) => void) => () => void;
  /** 终态事件流（`EVENT_SPEED_TEST_DONE`）—— 中断/完成的主路径。 */
  subscribeDone: (listener: (p: SpeedTestDonePayload) => void) => () => void;
  toast: Pick<ToastImpl, 'info' | 'success' | 'warning'>;
  t: (key: string, vars?: Record<string, number>) => string;
  /**
   * 点恢复动作那一刻的**当前节点 id 全集**（用于过滤已消失的节点，见 [`planSpeedTestRun`]）。
   * 必须是 getter 而不是快照数组：这条订阅活一辈子，捕获的数组在用户改订阅后就是陈旧的。
   */
  currentServerIds: () => string[];
  /** 发起恢复测速（生产 = `api.server.speedTest(ids)`）。**只在过滤后非空时才会被调用。** */
  run: (ids: string[]) => void;
}

/**
 * 把进度事件流接到全局 toast —— **窗口级持久订阅**，返回退订函数。
 *
 * 调用点必须是该窗口生命周期内只挂载一次的根（主窗 `App.tsx` 的全局订阅层）。挂进业务组件即退回
 * 「切屏即丢」，也就是本模块存在的那个缺陷本身。
 */
export function subscribeSpeedTestProgressToast(deps: SpeedTestToastDeps): () => void {
  let state = initialSpeedTestToastState;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;

  const clearIdle = () => {
    if (idleTimer !== undefined) {
      clearTimeout(idleTimer);
      idleTimer = undefined;
    }
  };

  /**
   * 「继续」被点下：**此刻**再按当前节点集过滤一次（中断到点击之间用户可能改过订阅），
   * 空了就不发请求、只把 toast 换成一句说明（同 key 顶掉，随后 2.2s 自散）。
   *
   * 🔴 **只有这里会调 `deps.run`** —— 收到 `interrupted` 不会自动触发任何请求（判据见文件头
   * 「不自动执行」三条）。把 `run` 挪进终态分支就是自动恢复，门会转红。
   */
  const onActionClicked = (serverIds: string[]) => {
    const ids = planSpeedTestRun(serverIds, deps.currentServerIds());
    if (ids.length === 0) {
      deps.toast.info(deps.t('nodes.speedTestTargetsGone'), {
        key: SPEEDTEST_TOAST_KEY,
        sticky: false,
      });
      return;
    }
    deps.run(ids);
  };

  const dispatch = (intent: SpeedTestToastIntent | null) => {
    if (!intent) return;
    const msg = deps.t(intent.msgKey, intent.msgVars);
    // key 恒为同一个 ⇒ Toaster 侧走 upsert（更新那一条，不新增）。sticky 由 intent 决定，本层不覆写。
    const opts = {
      key: SPEEDTEST_TOAST_KEY,
      sticky: intent.sticky,
      ...(intent.descKey ? { description: deps.t(intent.descKey, intent.descVars) } : {}),
      ...(intent.actions
        ? {
            actions: intent.actions.map((action) => ({
              label: deps.t(action.labelKey),
              // 闭包捕获中断终态给出的范围；节点是否仍存在则推迟到点击那一刻现取。
              onClick: () => onActionClicked(action.serverIds),
            })),
          }
        : {}),
      ...(intent.dismissLabelKey
        ? { dismiss: { label: deps.t(intent.dismissLabelKey) } }
        : {}),
    };
    if (intent.level === 'success') deps.toast.success(msg, opts);
    else if (intent.level === 'warning') deps.toast.warning(msg, opts);
    else deps.toast.info(msg, opts);
  };

  const armIdle = () => {
    clearIdle();
    // 只在还活着时布防；终态已经把 sticky 顶掉了，再布防会在十几秒后凭空补一条「测速中断」。
    if (!state.live) return;
    idleTimer = setTimeout(() => {
      idleTimer = undefined;
      const r = reduceSpeedTestIdle(state);
      state = r.next;
      dispatch(r.intent);
    }, SPEEDTEST_IDLE_TIMEOUT_MS);
  };

  const unsubscribe = deps.subscribe((ev) => {
    const r = reduceSpeedTestProgress(state, ev);
    state = r.next;
    dispatch(r.intent);
    armIdle();
  });

  // 终态事件（主路径）。`armIdle()` 在这之后照调：它先 `clearIdle()`，再因 `!state.live` 早退 ⇒
  // 净效果是**拆掉在飞的兜底定时器**，否则终态收口后十几秒还会再冒一条「测速中断」。
  const unsubscribeDone = deps.subscribeDone((ev) => {
    const r = reduceSpeedTestDone(state, ev);
    state = r.next;
    dispatch(r.intent);
    armIdle();
  });

  return () => {
    clearIdle();
    unsubscribe();
    unsubscribeDone();
  };
}
