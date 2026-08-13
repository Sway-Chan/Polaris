/**
 * Toast 队列语义门 —— 守「同 key 更新 / sticky 不自散 / 溢出不误伤持续状态」三条。
 *
 * # 射程与不射程（如实记账）
 *
 * 本仓 vitest 是 `environment:'node'`、无 jsdom（`vite.config.ts:76`，有意为之）⇒
 *  · **能测**：本文件全部 —— 队列语义是纯数组变换，与 DOM 无关，下面每一条都真跑到底；
 *  · **测不到**：`Toaster.tsx` 真的把 `upsertToast` 的结果渲染出来、`autoDismissMs` 返回 `null`
 *    时真的没起定时器、React key 真的没让节点重挂。这三条属渲染层，node 环境不可观测；
 *    源码侧只由本文件末尾的接线扫描钉住「调用还在」（正则级，挡得住整段被删、挡不住写反），
 *    真值靠真机验收（切屏/开弹窗/测完自动收）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ACTION_VISIBLE_MS,
  MAX_STACK,
  VISIBLE_MS,
  autoDismissMs,
  toastListKey,
  upsertToast,
  type ToastEntry,
} from './toast-queue';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const stripComments = (src: string) =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

let seq = 0;
const entry = (over: Partial<ToastEntry> = {}): ToastEntry => ({
  id: ++seq,
  msg: 'm',
  kind: '',
  sticky: false,
  shown: false,
  leaving: false,
  ...over,
});

describe('upsertToast：同 key 更新那一条，不新增', () => {
  it('🔴 一轮 50 个节点的进度只占一条 —— 退回「无条件 append」立刻转红', () => {
    /* 这是本批最要紧的一条：`Toaster` 的 id 原本是自增流水号、没有按 key 更新的能力，
       直接拿它推进度会按事件条数刷屏（50 个节点 = 50 条 toast，栈上限只挡到 2 条、
       其余全在闪，用户什么也读不到）。 */
    let list: ToastEntry[] = [];
    for (let i = 1; i <= 50; i++) {
      list = upsertToast(list, entry({ dedupeKey: 'p', msg: `测速中 ${i}/50`, sticky: true }));
    }
    expect(list).toHaveLength(1);
    expect(list[0].msg).toBe('测速中 50/50');
  });

  it('更新保持栈内位置不变（进度不因自身刷新而跳到别人前面）', () => {
    const first = entry({ dedupeKey: 'p', msg: 'p1', sticky: true });
    let list = upsertToast([], first);
    list = upsertToast(list, entry({ msg: 'other' }));
    expect(list.map((it) => it.msg)).toEqual(['p1', 'other']);
    list = upsertToast(list, entry({ dedupeKey: 'p', msg: 'p2', sticky: true }));
    // 位置仍在 0：若实现写成「先删后追加」，进度条会在每个事件上与旁边那条对调位置。
    expect(list.map((it) => it.msg)).toEqual(['p2', 'other']);
  });

  it('更新沿用旧 shown（不重播进场动画），但换新 id（让旧定时器空转）', () => {
    const first = entry({ dedupeKey: 'p', msg: 'p1' });
    let list = upsertToast([], { ...first, shown: true });
    const next = entry({ dedupeKey: 'p', msg: 'p2' });
    list = upsertToast(list, next);
    // shown 沿用：新条目字面是 false，upsert 必须把屏上那条的 true 带过来。
    // 否则每个进度事件都会掉一次 `.show` → 下一帧再加回 ⇒ 200ms 闪一次。
    expect(list[0].shown).toBe(true);
    // id 换新：旧 id 上可能挂着在飞的淡出定时器（上一条是非 sticky 时），换号即让它按 id 匹配不到。
    expect(list[0].id).toBe(next.id);
    expect(list[0].id).not.toBe(first.id);
  });

  it('无 key 的一次性 toast 各自独立成条（不因文案相同而被合并）', () => {
    let list = upsertToast([], entry({ msg: '保存失败' }));
    list = upsertToast(list, entry({ msg: '保存失败' }));
    expect(list).toHaveLength(2);
  });

  it('不同 key 互不干扰', () => {
    let list = upsertToast([], entry({ dedupeKey: 'a', msg: 'a1', sticky: true }));
    list = upsertToast(list, entry({ dedupeKey: 'b', msg: 'b1', sticky: true }));
    list = upsertToast(list, entry({ dedupeKey: 'a', msg: 'a2', sticky: true }));
    expect(list.map((it) => it.msg)).toEqual(['a2', 'b1']);
  });
});

describe('溢出挤旧：既有行为逐字不变，且不误伤持续状态', () => {
  it('全非 sticky 时退化为原型的「挤掉最旧」', () => {
    // 前提：既有 20+ 处调用点全是非 sticky，本条锁死它们的行为零变化。
    let list = upsertToast([], entry({ msg: 'a' }));
    list = upsertToast(list, entry({ msg: 'b' }));
    list = upsertToast(list, entry({ msg: 'c' }));
    expect(list).toHaveLength(MAX_STACK);
    expect(list.map((it) => it.msg)).toEqual(['b', 'c']);
  });

  it('🔴 有 sticky 在场时先挤非 sticky —— 否则两条普通 toast 就能把进度挤没', () => {
    /* 测速期间「N 个节点未纳入」info + 某个 error 一到，最旧的恰是进度那条。
       按裸「挤最旧」会把进度挤掉：用户看到进度凭空消失，且再也不回来（它只在下个事件才重建）。 */
    let list = upsertToast([], entry({ dedupeKey: 'p', msg: '进度', sticky: true }));
    list = upsertToast(list, entry({ msg: 'info' }));
    list = upsertToast(list, entry({ msg: 'err' }));
    expect(list.map((it) => it.msg)).toEqual(['进度', 'err']);
  });

  it('全 sticky 时仍挤最旧（不会无限堆叠）', () => {
    let list = upsertToast([], entry({ msg: 's1', sticky: true }));
    list = upsertToast(list, entry({ msg: 's2', sticky: true }));
    list = upsertToast(list, entry({ msg: 's3', sticky: true }));
    expect(list.map((it) => it.msg)).toEqual(['s2', 's3']);
  });
});

describe('autoDismissMs：sticky 不自动消失', () => {
  it('🔴 sticky ⇒ null（不起淡出定时器）', () => {
    // 变异「sticky 失效」= 让本函数对 sticky 也返 VISIBLE_MS ⇒ 进度 toast 2.2s 后自散，
    // 而一轮测速远不止 2.2s：用户只看得到前两个节点，此后一片空白。
    expect(autoDismissMs({ sticky: true })).toBeNull();
  });

  it('非 sticky ⇒ 原型的 2200ms', () => {
    expect(autoDismissMs({ sticky: false })).toBe(VISIBLE_MS);
    expect(VISIBLE_MS).toBe(2200);
  });
});

describe('autoDismissMs：带 action 的 toast **必须有出路**（action 压过 sticky）', () => {
  const act = { label: '继续', onClick: () => {} };

  it('🔴 sticky + action ⇒ 仍返回**有限**值（不许「带按钮却永不消失」）', () => {
    // 这是本仓唯一封住「按钮点不到又关不掉」的地方：Toaster 没有 dismiss 通道，收口全靠
    // 「同 key 顶掉」+「自动淡出」两条，故 sticky+action 在结构上没有出路。
    // 变异锁：把 `if (entry.action) return ACTION_VISIBLE_MS;` 删掉（让 sticky 说了算）→ 拿到 null → 转红。
    const ttl = autoDismissMs({ sticky: true, action: act });
    expect(ttl).not.toBeNull();
    expect(Number.isFinite(ttl as number)).toBe(true);
  });

  it('🔴 停留明显长于 2.2s（否则按钮形同虚设，用户视线还没落过去就没了）', () => {
    // 变异锁：让带 action 的也走 VISIBLE_MS → 转红。
    expect(autoDismissMs({ sticky: false, action: act })).toBe(ACTION_VISIBLE_MS);
    expect(ACTION_VISIBLE_MS).toBeGreaterThanOrEqual(8_000);
  });

  it('也不许赖在屏上（栈只有 2 个位子，常驻会挤掉后续真正要看的通知）', () => {
    expect(ACTION_VISIBLE_MS).toBeLessThanOrEqual(20_000);
  });
});

describe('toastListKey：React key 必须随 dedupeKey 稳定', () => {
  it('同 key 的两次更新给出同一个 React key（id 变了也不变）', () => {
    // 用 id 作 React key 会让同一条进度 toast 每次刷新都卸载重挂 ⇒ 进场动画重播 ⇒ 闪。
    expect(toastListKey({ id: 1, dedupeKey: 'p' })).toBe(toastListKey({ id: 2, dedupeKey: 'p' }));
  });

  it('无 key 时按 id 区分，且两个命名空间不串台', () => {
    expect(toastListKey({ id: 3 })).not.toBe(toastListKey({ id: 4 }));
    expect(toastListKey({ id: 3 })).not.toBe(toastListKey({ id: 9, dedupeKey: '3' }));
  });
});

describe('接线还在：Toaster 用的是本模块的判定，没有就地复刻一份', () => {
  const toaster = stripComments(read('./Toaster.tsx'));

  it('入栈走 upsertToast（不是裸 append/slice）', () => {
    expect(toaster, 'Toaster 又自己拼数组了 —— 同 key 更新语义会随之丢失').toMatch(
      /setItems\(\(prev\) => upsertToast\(prev, entry\)\)/,
    );
  });

  it('🔴 淡出定时器由 autoDismissMs 把关，null 时直接返回', () => {
    expect(toaster).toMatch(/const ttl = autoDismissMs\(entry\)/);
    expect(toaster, 'sticky 的早退没了 —— 进度 toast 会被 2.2s 定时器收走').toMatch(
      /if \(ttl === null\) return;/,
    );
    // 定时器时长必须用 ttl，写回常数就等于绕过了上面那道判定。
    expect(toaster).toMatch(/\}, ttl\);/);
  });

  it('React key 走 toastListKey（不是 it.id）', () => {
    expect(toaster).toMatch(/key=\{toastListKey\(it\)\}/);
    expect(toaster, 'key 又回到 it.id —— 同 key 更新会重挂节点、动画重播').not.toMatch(
      /key=\{it\.id\}/,
    );
  });

  it('🔴 action 从 ToastOptions 接进 entry，并真的渲染成一个可点的按钮', () => {
    // 变异锁：删掉 `action: opts?.action` → 第一条转红（按钮永远建不出来，续测入口整段哑火）；
    // 删掉那个 `<button>` → 第二条转红；删掉 pointerEvents 收回 → 第三条转红
    // （栈整体 `pointer-events:none`，不收回来按钮点不着 —— 这是「看着有按钮、点不动」的经典假接线）。
    expect(toaster).toMatch(/action: opts\?\.action/);
    expect(toaster).toMatch(/className="toast-action"/);
    expect(toaster, '带按钮那条必须把 pointer-events 收回来，否则点不到').toMatch(
      /pointerEvents: 'auto'/,
    );
    expect(toaster).toMatch(/onClick=\{it\.action\.onClick\}/);
  });

  it('四个 level 都把 ToastOptions 透传下去（漏一个，那条通道就用不了 key/sticky）', () => {
    for (const re of [
      /success: \(m, o\) => push\(m, 'ok', undefined, o\)/,
      /error: \(m, d, o\) => push\(m, 'err', d, o\)/,
      /info: \(m, o\) => push\(m, '', undefined, o\)/,
      /warning: \(m, o\) => push\(m, '', undefined, o\)/,
    ]) {
      expect(toaster).toMatch(re);
    }
  });
});
