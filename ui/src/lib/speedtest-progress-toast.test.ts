/**
 * 测速进度 toast 协调器门 —— 守「同 key、sticky、必有终态」三条。
 *
 * # 射程与不射程（如实记账）
 *
 * 本仓 vitest 是 `environment:'node'`、无 jsdom（`vite.config.ts:76`）⇒
 *  · **能测**：本文件几乎全部。协调器是纯 TS（无 React、无 DOM），事件流与 toast 出口都可注入，
 *    故下面用假事件流驱动一整轮测速、断言假 toast 收到的**每一次调用及其 options** —— 这不是
 *    正则扫描，是真行为；静默超时也用 vitest 假计时器真跑到点。
 *  · **测不到**：这些 options 传到 `Toaster` 之后屏幕上真的只有一条、真的没被 2.2s 收走、
 *    弹窗打开时真的没被 `::backdrop` 压住。队列语义那半由 `components/layout/toast-queue.test.ts`
 *    的纯逻辑接住，剩下的渲染/层叠语义 node 环境不可观测，靠真机验收。
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import {
  SPEEDTEST_IDLE_TIMEOUT_MS,
  SPEEDTEST_TOAST_KEY,
  initialSpeedTestToastState,
  planSpeedTestRun,
  reduceSpeedTestDone,
  reduceSpeedTestIdle,
  reduceSpeedTestProgress,
  subscribeSpeedTestProgressToast,
  type SpeedTestDonePayload,
  type SpeedTestProgress,
  type SpeedTestToastDeps,
} from './speedtest-progress-toast';

interface Call {
  level: 'info' | 'success' | 'warning';
  msg: string;
  opts?: {
    key?: string;
    sticky?: boolean;
    description?: string;
    actions?: Array<{ label: string; onClick: () => void }>;
    dismiss?: { label: string };
  };
}

/**
 * 假外部面：捕获两条事件流的监听器 + 记录每一次 toast 调用（含 options）。
 *
 * `servers` / `runs` 让恢复动作可测：前者是「点击那一刻的当前节点集」（测试可随时改），
 * 后者记录**实际发出去的测速请求**（一次都没发 = 空数组，正是「不自动执行」那条门的判据）。
 */
function harness(servers: string[] = []) {
  const calls: Call[] = [];
  const runs: string[][] = [];
  let alive = [...servers];
  let listener: ((p: SpeedTestProgress) => void) | null = null;
  let doneListener: ((p: SpeedTestDonePayload) => void) | null = null;
  let unsubscribed = 0;
  const deps: SpeedTestToastDeps = {
    subscribe: (l) => {
      listener = l;
      return () => {
        unsubscribed += 1;
      };
    },
    subscribeDone: (l) => {
      doneListener = l;
      return () => {
        unsubscribed += 1;
      };
    },
    toast: {
      info: (msg, opts) => calls.push({ level: 'info', msg, opts }),
      success: (msg, opts) => calls.push({ level: 'success', msg, opts }),
      warning: (msg, opts) => calls.push({ level: 'warning', msg, opts }),
    },
    // 假 t：把键与插值原样拼出来，断言里既看得到用了哪个键，也看得到数字有没有接对。
    t: (key, vars) => (vars ? `${key}(${JSON.stringify(vars)})` : key),
    currentServerIds: () => alive,
    run: (ids) => runs.push(ids),
  };
  const stop = subscribeSpeedTestProgressToast(deps);
  return {
    calls,
    runs,
    stop,
    emit: (tested: number, ok: number, total: number) => listener?.({ tested, ok, total }),
    emitDone: (p: SpeedTestDonePayload) => doneListener?.(p),
    setServers: (ids: string[]) => {
      alive = [...ids];
    },
    clickAction: (label: string) => {
      const last = calls[calls.length - 1];
      const action = last?.opts?.actions?.find((it) => it.label === label);
      if (!action) throw new Error(`最后一条 toast 没有动作：${label}`);
      action.onClick();
    },
    unsubCount: () => unsubscribed,
  };
}

const done = (p: Partial<SpeedTestDonePayload> = {}): SpeedTestDonePayload => ({
  outcome: 'interrupted',
  tested: 12,
  total: 50,
  serverIds: [],
  pending: [],
  ...p,
});

describe('reduce：纯状态机', () => {
  it('未跑完 ⇒ sticky 进度（这就是「持续状态」那一档）', () => {
    const { next, intent } = reduceSpeedTestProgress(initialSpeedTestToastState, {
      tested: 3,
      ok: 2,
      total: 10,
    });
    expect(next).toEqual({ live: true, tested: 3, total: 10 });
    expect(intent).toEqual({
      level: 'info',
      sticky: true,
      msgKey: 'nodes.speedTestingNodes',
      msgVars: { tested: 3, total: 10 },
    });
  });

  it('🔴 跑完 ⇒ 非 sticky 的结论 toast（这就是「收起」）', () => {
    const { next, intent } = reduceSpeedTestProgress(
      { live: true, tested: 9, total: 10 },
      { tested: 10, ok: 7, total: 10 },
    );
    expect(next.live).toBe(false);
    // 变异「测速结束不收起」= 这里继续给 sticky:true（或不给 intent）⇒ 转红。
    expect(intent).toEqual({ level: 'success', sticky: false, msgKey: 'nodes.speedTestDone' });
  });

  it('total<=0 / 非数 一律忽略（契约破了也不许起一条永不终止的 sticky）', () => {
    for (const total of [0, -1, Number.NaN]) {
      const r = reduceSpeedTestProgress(initialSpeedTestToastState, { tested: 0, ok: 0, total });
      expect(r.intent).toBeNull();
      expect(r.next.live).toBe(false);
    }
  });

  it('静默超时：在跑 ⇒ 收成「测速中断」并带已完成数；不在跑 ⇒ 什么都不做', () => {
    const live = reduceSpeedTestIdle({ live: true, tested: 12, total: 50 });
    expect(live.next.live).toBe(false);
    expect(live.intent).toEqual({
      level: 'warning',
      sticky: false,
      msgKey: 'nodes.speedTestInterrupted',
      descKey: 'nodes.speedTestInterruptedSummary',
      descVars: { tested: 12, total: 50 },
      dismissLabelKey: 'nodes.speedTestDismiss',
    });
    expect(reduceSpeedTestIdle({ live: false, tested: 12, total: 50 }).intent).toBeNull();
  });
});

describe('订阅接线：一整轮测速的真实调用序列', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('🔴 每一条进度都用**同一个 key** —— 没有它，50 个节点会刷 50 条', () => {
    const h = harness();
    for (let i = 1; i <= 50; i++) h.emit(i, i, 50);
    // 50 次调用是对的（每个节点一次更新），关键是它们必须都带同一个 key ⇒ Toaster 侧 upsert 成一条。
    expect(h.calls).toHaveLength(50);
    expect(new Set(h.calls.map((c) => c.opts?.key))).toEqual(new Set([SPEEDTEST_TOAST_KEY]));
    h.stop();
  });

  it('🔴 进度是 sticky、终态不是（变异任一条即转红）', () => {
    const h = harness();
    h.emit(1, 1, 3);
    h.emit(2, 1, 3);
    h.emit(3, 2, 3);
    expect(h.calls.map((c) => [c.level, c.opts?.sticky])).toEqual([
      ['info', true],
      ['info', true],
      ['success', false],
    ]);
    expect(h.calls[0].msg).toBe('nodes.speedTestingNodes({"tested":1,"total":3})');
    expect(h.calls[2].msg).toBe('nodes.speedTestDone');
    h.stop();
  });

  it('🔴 跑完之后不再有静默超时补刀（终态后布防 = 十几秒后凭空多一条「测速中断」）', () => {
    const h = harness();
    h.emit(1, 1, 2);
    h.emit(2, 2, 2);
    vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS * 3);
    expect(h.calls).toHaveLength(2);
    expect(h.calls[1].level).toBe('success');
    h.stop();
  });

  it('🔴 兜底：连终态事件都丢了 ⇒ 静默超时仍把 sticky 收成「测速中断」，不许挂死在屏上', () => {
    /* 2026-07-31 B 批后本条的语义变了（**兜底**，不是主路径）：中断/完成一律由 `EVENT_SPEED_TEST_DONE`
       立即收口（见下面 `终态事件` 那一组）。本条守的是「事件丢失 / 后端异常退出」这个残余形态 ——
       此时没有任何事件会再来，只按 tested>=total 判终态必然挂死在屏上。
       变异锁：删掉 armIdle / 把 SPEEDTEST_IDLE_TIMEOUT_MS 改成 Infinity → 本条转红。 */
    const h = harness();
    h.emit(12, 9, 50);
    vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS - 1);
    expect(h.calls).toHaveLength(1); // 还没到点，进度仍在
    vi.advanceTimersByTime(1);
    expect(h.calls).toHaveLength(2);
    expect(h.calls[1]).toEqual({
      level: 'warning',
      msg: 'nodes.speedTestInterrupted',
      opts: {
        key: SPEEDTEST_TOAST_KEY,
        sticky: false,
        description: 'nodes.speedTestInterruptedSummary({"tested":12,"total":50})',
        dismiss: { label: 'nodes.speedTestDismiss' },
      },
    });
    h.stop();
  });

  it('每个进度事件都重新布防（慢节点不会被误判成中断）', () => {
    const h = harness();
    // 单节点硬上限 10s（冷 6s + 复用 4s），故连着几个慢节点也不该触发中断。
    for (let i = 1; i <= 5; i++) {
      h.emit(i, i, 50);
      vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS - 1000);
    }
    expect(h.calls.every((c) => c.level === 'info')).toBe(true);
    h.stop();
  });

  it('退订会拆掉在飞的超时 + **两条**事件流（组件卸载后不该再冒出一条 toast）', () => {
    const h = harness();
    h.emit(1, 1, 9);
    h.stop();
    vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS * 2);
    expect(h.calls).toHaveLength(1);
    // 2 而不是 1：B 批加了终态事件流（`subscribeDone`）。漏退订它 = 卸载后仍会弹终态 toast，
    // 故这个数字必须跟着订阅数走 —— 变异「只退订 progress」→ 转红并点名。
    expect(h.unsubCount()).toBe(2);
  });

  it('下一轮测速能重新起 sticky（上一轮的终态不把状态机锁死）', () => {
    const h = harness();
    // 第一轮：两节点跑完即终态。**不能用 total=1** —— 单节点 ⚡ 整轮静音（陈先生 2026-07-31 裁定），
    // 拿它当 fixture 会让本条测的「终态」压根不发生，测试恒绿且测不到东西。
    h.emit(2, 2, 2);
    h.emit(1, 1, 4); // 第二轮开跑
    expect(h.calls.map((c) => [c.level, c.opts?.sticky])).toEqual([
      ['success', false],
      ['info', true],
    ]);
    h.stop();
  });
});

describe('静默超时常量：必须留有余量，且**与后端常量真的对得上**', () => {
  /*
   * # 这条门为什么改成去读 Rust 源文件（而不是继续写死一个数字）
   *
   * 原文是 `expect(SPEEDTEST_IDLE_TIMEOUT_MS).toBeGreaterThanOrEqual(12_000)` —— 12000 是把
   * 「2 × 后端单节点上限」的**结果**抄进来。后端一改超时，这里照样绿（协调器文件头当时如实写着
   * 「两者失配本仓没有门会转红，只有两边注释互指」）。2026-07-31 后端改成**两段**预算，
   * 这个抄来的数字当场过期 —— 正是补门的时机，而不是把 12000 换成 20000 再抄一遍。
   *
   * 改后判据是**算术关系**：`SPEEDTEST_IDLE_TIMEOUT_MS >= 2 × (cold + reuse)`，两个加数直接从
   * `commands/speedtest.rs` 正则抓。跨语言常量对拍在本仓已有先例（`ipc-channel-bypass-wiring.test.ts`
   * 同样读 `src-tauri/src` 原文）。
   */
  const RS = readFileSync(
    fileURLToPath(new URL('../../../src-tauri/src/commands/speedtest.rs', import.meta.url)),
    'utf8'
  );

  /** 抓 `const NAME: u64 = 6_000;` 的数值（允许 `_` 分隔）。抓不到即抛——空转恒绿比失配更危险。 */
  function rustConstMs(name: string): number {
    const m = new RegExp(`const\\s+${name}\\s*:\\s*u64\\s*=\\s*([0-9_]+)\\s*;`).exec(RS);
    if (!m) throw new Error(`[speedtest-idle] 后端常量 ${name} 没抓到 —— 本门已失去判据`);
    return Number(m[1].replace(/_/g, ''));
  }

  it('前提校验：两个后端常量都抓得到（抓不到就不是「宽松」而是没门）', () => {
    expect(rustConstMs('SPEED_TEST_COLD_TIMEOUT_MS')).toBeGreaterThan(0);
    expect(rustConstMs('SPEED_TEST_REUSE_TIMEOUT_MS')).toBeGreaterThan(0);
  });

  it('🔴 ≥ 2 ×（冷建链预算 + 复用请求预算）—— 直接读 Rust 常量算，失配即转红', () => {
    // 单节点最坏耗时 = 冷段 + 复用段（两段各自超时的极端叠加）⇒ 活跑时两次进度事件的间隔上界就是它。
    // 调到这个界以下 ⇒ 一个跑满上限的慢节点会被误判成中断，测速中途凭空弹「已完成 x/y」+ 一个白跑的「继续」。
    // 变异对照：把后端任一常量调大（如冷 6s → 12s）而不动本值 → 本条转红。
    const worstSingleNode =
      rustConstMs('SPEED_TEST_COLD_TIMEOUT_MS') + rustConstMs('SPEED_TEST_REUSE_TIMEOUT_MS');
    expect(SPEEDTEST_IDLE_TIMEOUT_MS).toBeGreaterThanOrEqual(2 * worstSingleNode);
  });

  it('也不许大到用户以为卡死（兜底归兜底，上界仍在）', () => {
    // 降级成兜底后取大的代价小，但不是没有上界：一条挂在屏上的 sticky 超过半分钟就是「卡死」的观感。
    expect(SPEEDTEST_IDLE_TIMEOUT_MS).toBeLessThanOrEqual(30_000);
  });
});

describe('终态事件（主路径）：中断当场收口，不再靠猜', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('🔴 interrupted ⇒ 立即转中断态，**不等**静默超时', () => {
    const h = harness();
    h.emit(12, 9, 50);
    h.emitDone(done({ tested: 12, total: 50, pending: ['n1'] }));
    // 一个 tick 都没推进就已经有终态了 —— 这就是「断开为什么还要等十几秒」那个缺陷的直接反面。
    expect(h.calls).toHaveLength(2);
    expect(h.calls[1].level).toBe('warning');
    expect(h.calls[1].msg).toBe('nodes.speedTestInterrupted');
    expect(h.calls[1].opts?.description).toBe(
      'nodes.speedTestInterruptedSummary({"tested":12,"total":50})'
    );
    h.stop();
  });

  it('🔴 已完成数取**后端载荷**，不是本地最后一帧（如实上报）', () => {
    const h = harness();
    h.emit(12, 9, 50);
    // 后端权威值与本地回声故意不同：末几个节点的进度事件可能还没到就已经中断了。
    h.emitDone(done({ tested: 14, total: 50, pending: ['n1'] }));
    expect(h.calls[1].opts?.description).toBe(
      'nodes.speedTestInterruptedSummary({"tested":14,"total":50})'
    );
    h.stop();
  });

  it('🔴 终态收口后拆掉兜底定时器（否则十几秒后凭空再来一条「测速中断」）', () => {
    const h = harness();
    h.emit(12, 9, 50);
    h.emitDone(done({ pending: ['n1'] }));
    vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS * 3);
    expect(h.calls).toHaveLength(2);
    h.stop();
  });

  it('🔴 正常跑完那一帧已收口 ⇒ 随后到达的 done 不再弹第二条', () => {
    const h = harness();
    h.emit(1, 1, 2);
    h.emit(2, 2, 2); // 这一帧就是终态
    h.emitDone({ outcome: 'completed', tested: 2, total: 2, serverIds: ['n1', 'n2'], pending: [] });
    expect(h.calls).toHaveLength(2);
    expect(h.calls[1].msg).toBe('nodes.speedTestDone');
    h.stop();
  });

  it('单节点 ⚡（total<=1 整轮静音）⇒ done 也必须静音', () => {
    const h = harness();
    h.emit(1, 1, 1);
    h.emitDone({ outcome: 'completed', tested: 1, total: 1, serverIds: ['n1'], pending: [] });
    expect(h.calls).toHaveLength(0);
    h.stop();
  });

  it('completed 但 tested 到不了 total（JoinError 漏账）⇒ done 照样收口成「完成」', () => {
    const h = harness();
    h.emit(4, 4, 5); // 第 5 个节点的测量任务 panic 了，永远不会有 5/5
    h.emitDone({
      outcome: 'completed',
      tested: 4,
      total: 5,
      serverIds: ['n1', 'n2', 'n3', 'n4', 'n5'],
      pending: ['n5'],
    });
    expect(h.calls[1].level).toBe('success');
    expect(h.calls[1].msg).toBe('nodes.speedTestDone');
    // completed 不给「继续」：后端已裁定本轮结束，pending 只是漏账的账面残留。
    expect(h.calls[1].opts?.actions).toBeUndefined();
    h.stop();
  });
});

describe('中断后的恢复动作：继续剩余 / 重测原范围 / 关闭', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('🔴 收到 interrupted **不会**自动发续测请求（要点才发）', () => {
    // 变异锁：把 `deps.run(...)` 从 onClick 挪进 done 分支（= 自动恢复）→ 第一条断言转红。
    // 判据见协调器文件头「不自动续」三条：抢后端单飞闸 / 用户此刻在做别的事 / 无收敛点。
    const h = harness(['n1', 'n2']);
    h.emit(1, 1, 3);
    h.emitDone(done({ tested: 1, total: 3, serverIds: ['n0', 'n1', 'n2'], pending: ['n1', 'n2'] }));
    vi.advanceTimersByTime(SPEEDTEST_IDLE_TIMEOUT_MS * 3);
    expect(h.runs, '中断本身绝不触发任何请求').toEqual([]);

    h.clickAction('nodes.speedTestResume');
    expect(h.runs, '点了才发').toEqual([['n1', 'n2']]);
    h.stop();
  });

  it('🔴 发的是**差集**，不是全集（这就是「续测」相对「重测」的全部价值）', () => {
    // 变异锁：把 pending 换成整轮请求集（后端「返回全集」）或前端自己拿全量节点重测 → 转红。
    const h = harness(['n1', 'n2', 'n3', 'n4']);
    h.emit(2, 2, 4);
    h.emitDone(done({ tested: 2, total: 4, serverIds: ['n1', 'n2', 'n3', 'n4'], pending: ['n3', 'n4'] }));
    h.clickAction('nodes.speedTestResume');
    expect(h.runs).toEqual([['n3', 'n4']]);
    h.stop();
  });

  it('🔴 发出前按**当前**节点集过滤（中断到点击之间用户删了节点）', () => {
    // 变异锁：点击处理里去掉 planSpeedTestRun 直接 `deps.run(pending)` → 转红。
    const h = harness(['n1', 'n2', 'n3']);
    h.emit(1, 1, 3);
    h.emitDone(done({ tested: 1, total: 3, serverIds: ['n1', 'n2', 'n3'], pending: ['n2', 'n3'] }));
    h.setServers(['n1', 'n3']); // n2 被删了 / 订阅换了
    h.clickAction('nodes.speedTestResume');
    expect(h.runs).toEqual([['n3']]);
    h.stop();
  });

  it('🔴 过滤后为空 ⇒ **不发空请求**，只收掉 toast', () => {
    // 空 serverIds 传到后端等于「测全部」——语义彻底反转，会把一次「继续」变成一整轮全量测速。
    const h = harness(['n1']);
    h.emit(1, 1, 3);
    h.emitDone(done({ tested: 1, total: 3, serverIds: ['n1', 'n2', 'n3'], pending: ['n2', 'n3'] }));
    h.setServers([]); // 全没了
    h.clickAction('nodes.speedTestResume');
    expect(h.runs).toEqual([]);
    const last = h.calls[h.calls.length - 1];
    expect(last.msg).toBe('nodes.speedTestTargetsGone');
    expect(last.opts?.key).toBe(SPEEDTEST_TOAST_KEY); // 同 key ⇒ 顶掉那条中断 toast
    expect(last.opts?.sticky).toBe(false);
    h.stop();
  });

  it('🔴 一个节点都没完成时两个范围相同 ⇒ 只给「重新测速」，不放两个同义按钮', () => {
    const h = harness(['n1', 'n2', 'n3']);
    h.emit(0, 0, 3);
    h.emitDone(done({ tested: 0, total: 3, serverIds: ['n1', 'n2', 'n3'], pending: ['n1', 'n2', 'n3'] }));
    expect(h.calls[1].opts?.actions?.map((action) => action.label)).toEqual([
      'nodes.speedTestRetry',
    ]);
    h.stop();
  });

  it('🔴 重新测速严格复用原请求范围，不扩成当前全部节点', () => {
    const h = harness(['n1', 'n2', 'n3', 'outside']);
    h.emit(1, 1, 3);
    h.emitDone(done({ tested: 1, total: 3, serverIds: ['n1', 'n2', 'n3'], pending: ['n2', 'n3'] }));
    h.clickAction('nodes.speedTestRetry');
    expect(h.runs).toEqual([['n1', 'n2', 'n3']]);
    h.stop();
  });

  it('🔴 中断态 toast **不是 sticky**（带按钮却永不消失 = 赖在屏上关不掉）', () => {
    // 形态判据的另一半在 `components/layout/toast-queue.test.ts`：那边钉死 autoDismissMs 对
    // 带 actions 的条目必须返回有限值（即便调用方写了 sticky:true 也压过去）。两条一起才封死这个面。
    const h = harness(['n1']);
    h.emit(1, 1, 3);
    h.emitDone(done({ tested: 1, total: 3, serverIds: ['n1', 'n2', 'n3'], pending: ['n2'] }));
    expect(h.calls[1].opts?.sticky).toBe(false);
    expect(h.calls[1].opts?.actions?.map((action) => action.label)).toEqual([
      'nodes.speedTestResume',
      'nodes.speedTestRetry',
    ]);
    expect(h.calls[1].opts?.dismiss?.label).toBe('nodes.speedTestDismiss');
    h.stop();
  });

  it('planSpeedTestRun：保序 + 去重 + 只留还在的', () => {
    expect(planSpeedTestRun(['c', 'a', 'c', 'zz'], ['a', 'b', 'c'])).toEqual(['c', 'a']);
    expect(planSpeedTestRun([], ['a'])).toEqual([]);
    expect(planSpeedTestRun(['a'], [])).toEqual([]);
  });

  it('reduceSpeedTestDone：不在跑就什么都不做（纯状态机层）', () => {
    expect(reduceSpeedTestDone({ live: false, tested: 0, total: 0 }, done()).intent).toBeNull();
  });
});

describe('测速 Toast 文案在五语种都存在（本模块的键绕过了可寻址性门）', () => {
  /*
   * `locale-parity.test.ts` 的可寻址性门只扫 `t('字面量')` 形态；本模块把键存成 `intent.msgKey`
   * 再交给注入的 `t`，那道门看不见它们 ⇒ 这里自带一份，否则「键写错一个字母」只会在运行期
   * 显示成裸键名（且五语全中，fallbackLng 也救不了）。
   */
  const KEYS = [
    'nodes.speedTestingNodes',
    'nodes.speedTestDone',
    'nodes.speedTestInterrupted',
    'nodes.speedTestInterruptedSummary',
    // 恢复动作与关闭入口——五语同批补齐，故 ru/fa 的 MISSING_KEY_DEBT 不动。
    'nodes.speedTestResume',
    'nodes.speedTestRetry',
    'nodes.speedTestDismiss',
    'nodes.speedTestTargetsGone',
  ];
  const dir = fileURLToPath(new URL('../i18n/locales', import.meta.url));
  const files = readdirSync(dir).filter((f) => f.endsWith('.json'));

  it('前提校验：确实读到了五份 locale（读空则下面恒绿空转）', () => {
    expect(files).toHaveLength(5);
  });

  for (const f of files) {
    it(`${f} 键齐全`, () => {
      const data = JSON.parse(readFileSync(join(dir, f), 'utf8')) as Record<
        string,
        Record<string, unknown>
      >;
      const missing = KEYS.filter((k) => {
        const [ns, leaf] = k.split('.');
        return typeof data[ns]?.[leaf] !== 'string';
      });
      expect(missing, `${f} 缺键 —— 用户会看到裸键名`).toEqual([]);
    });
  }

  it('reduce 产出的键确实落在这张表里（防改了 reduce 忘了改本表）', () => {
    const used = new Set<string>();
    const collect = (
      i: {
        msgKey: string;
        descKey?: string;
        actions?: Array<{ labelKey: string }>;
        dismissLabelKey?: string;
      } | null
    ) => {
      if (!i) return;
      used.add(i.msgKey);
      if (i.descKey) used.add(i.descKey);
      for (const action of i.actions ?? []) used.add(action.labelKey);
      if (i.dismissLabelKey) used.add(i.dismissLabelKey);
    };
    collect(reduceSpeedTestProgress(initialSpeedTestToastState, { tested: 1, ok: 1, total: 5 }).intent);
    collect(reduceSpeedTestProgress(initialSpeedTestToastState, { tested: 5, ok: 5, total: 5 }).intent);
    collect(reduceSpeedTestIdle({ live: true, tested: 1, total: 5 }).intent);
    collect(
      reduceSpeedTestDone(
        { live: true, tested: 1, total: 5 },
        {
          outcome: 'interrupted',
          tested: 1,
          total: 5,
          serverIds: ['n1', 'n2'],
          pending: ['n2'],
        }
      ).intent
    );
    // `speedTestTargetsGone` 不经 reduce（它是「点击恢复动作但目标已不存在」时由订阅层直接弹的），
    // 故这里手工补上——否则上面那张表会因为「集合不等」转红，而缺席的恰恰是没被 reduce 覆盖的那条。
    used.add('nodes.speedTestTargetsGone');
    expect([...used].sort()).toEqual([...KEYS].sort());
  });
});
