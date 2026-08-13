/**
 * 订阅更新进度 reducer 的纯逻辑门。
 *
 * 覆盖的是本模块**唯一**有分支的地方：终态该删还是该留。这条规则错了的后果是不对称的 ——
 * 「该删没删」= 更新成功后订阅栏永远挂着一个转圈，「该留删了」= 失败彻底静默（回到本次改动之前的
 * 那个缺陷）。故两个方向各钉一条。
 *
 * 接线形态由文件下半段的源码守卫钉住；文案与呈现由组件门 `sub-info-bar-progress.test.tsx` 钉住。
 *
 * **明确不在射程**（如实标注，不假装覆盖）：真事件在 Tauri 运行时里是否真的送达（`listen` 的
 * 投递面无法在 node 里复演）；后端真拉取时各阶段的时序与耗时；CSS 在浅/深两档下的实际观感。
 * 这三样归真机门。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import {
  reduceSubscriptionProgress,
  type SubscriptionProgressMap,
} from './use-subscription-progress-store';
import type { SubscriptionUpdateProgress } from '@/contracts/subscription-progress';

const frame = (p: Partial<SubscriptionUpdateProgress> & { phase: SubscriptionUpdateProgress['phase'] }) =>
  ({ subscriptionId: 'sub-a', ...p }) as SubscriptionUpdateProgress;

describe('reduceSubscriptionProgress', () => {
  it('过程帧写入并覆盖前一帧', () => {
    let m: SubscriptionProgressMap = {};
    m = reduceSubscriptionProgress(m, frame({ phase: 'fetching' }));
    expect(m['sub-a'].phase).toBe('fetching');
    m = reduceSubscriptionProgress(m, frame({ phase: 'providers', done: 1, total: 3 }));
    expect(m['sub-a']).toMatchObject({ phase: 'providers', done: 1, total: 3 });
    m = reduceSubscriptionProgress(m, frame({ phase: 'applying' }));
    expect(m['sub-a'].phase).toBe('applying');
  });

  it('done / unchanged 清空条目', () => {
    for (const phase of ['done', 'unchanged'] as const) {
      const running = reduceSubscriptionProgress({}, frame({ phase: 'fetching' }));
      const after = reduceSubscriptionProgress(running, frame({ phase }));
      // 变异锁：把终态一律写进 map → 更新成功后订阅栏永远挂着一个转圈的徽标。
      expect(after['sub-a'], `${phase} 必须清空`).toBeUndefined();
    }
  });

  it('failed 留在 map 里（不与其它终态同处理）', () => {
    const running = reduceSubscriptionProgress({}, frame({ phase: 'fetching' }));
    const after = reduceSubscriptionProgress(running, frame({ phase: 'failed', error: '连接超时' }));
    // 变异锁：把 failed 也当成「终态 → 删」→ 失败彻底静默，退回本次改动之前的形态
    //（toast 2.2s 散掉后屏幕上再无痕迹；后台自动更新失败时用户根本不在场）。
    expect(after['sub-a']).toMatchObject({ phase: 'failed', error: '连接超时' });
  });

  it('新一轮的 fetching 顶掉上一次残留的失败徽标', () => {
    const failed = reduceSubscriptionProgress({}, frame({ phase: 'failed', error: 'x' }));
    const retry = reduceSubscriptionProgress(failed, frame({ phase: 'fetching' }));
    expect(retry['sub-a'].phase).toBe('fetching');
    // 且这一轮成功后彻底清干净（失败徽标不该跨轮存活）。
    expect(reduceSubscriptionProgress(retry, frame({ phase: 'done' }))['sub-a']).toBeUndefined();
  });

  it('只影响自己那条订阅', () => {
    const m = reduceSubscriptionProgress(
      { 'sub-b': frame({ phase: 'failed', error: 'b 坏了' }) },
      frame({ phase: 'fetching' })
    );
    // 变异锁：写入时整表替换 / 删除时清空全表 → 多订阅场景下互相抹掉状态。
    expect(m['sub-b']).toMatchObject({ phase: 'failed' });
    expect(m['sub-a'].phase).toBe('fetching');
    const cleared = reduceSubscriptionProgress(m, frame({ phase: 'done' }));
    expect(cleared['sub-b']).toMatchObject({ phase: 'failed' });
  });

  it('无归属的帧被丢弃', () => {
    const m: SubscriptionProgressMap = {};
    // 空 subscriptionId 写进去只会以空键挂住，没有任何一条订阅栏读得到。
    expect(reduceSubscriptionProgress(m, { subscriptionId: '', phase: 'fetching' })).toBe(m);
  });

  it('对空条目的终态帧不制造新引用（省一次全表重渲染）', () => {
    const m: SubscriptionProgressMap = { 'sub-b': frame({ phase: 'fetching' }) };
    expect(reduceSubscriptionProgress(m, frame({ phase: 'done' }))).toBe(m);
  });
});

/**
 * 三段接线守卫 —— 「reducer 全绿但整条链没接」是本仓反复踩过的那类假绿
 * （先例 `store/latency-wiring-invariants.test.ts`，同样是源码不变量守卫）。
 *
 * 本模块的链是：**App.tsx 订事件 → store → NodesScreen 读 store 并传 prop → SubInfoBar 渲染**。
 * 断的是任意一环，屏幕上的净效果都一样（什么都不显示），且三层的单测**一条都不会红**。
 * 守的是形态不是措辞：断言的都是「调了哪个函数 / 传了哪个 prop」这类结构事实。
 */
describe('订阅进度接线不变量', () => {
  const SRC = resolve(__dirname, '..');
  const read = (rel: string): string => readFileSync(resolve(SRC, rel), 'utf8');

  it('App.tsx 挂窗口级持久订阅（挂进业务组件即退回「切屏即丢」）', () => {
    const src = read('App.tsx');
    expect(src).toContain('subscribeSubscriptionProgressEvents');
    // 必须在 useEffect 里挂，且不带依赖数组之外的条件——与既有 subscribeLatencyEvents 同形。
    expect(src).toMatch(/useEffect\(\s*\(\)\s*=>\s*subscribeSubscriptionProgressEvents\(\),\s*\[\]\s*\)/);
  });

  it('NodesScreen 读 store 并把当前订阅的进度传给 SubInfoBar', () => {
    const src = read('components/screens/nodes/NodesScreen.tsx');
    expect(src).toContain('useSubscriptionProgress(');
    expect(src).toMatch(/progress=\{activeSubProgress\}/);
  });

  it('SubInfoBar 真的消费 progress（而不是只声明了 prop）', () => {
    const src = read('components/screens/nodes/SubInfoBar.tsx');
    expect(src).toMatch(/progress\?\.phase === 'failed'/);
    expect(src).toContain("progress.phase !== 'failed'");
  });

  it('后端通道名与前端常量逐字一致（跨语言字符串派发，编译器管不到）', () => {
    const rust = readFileSync(
      resolve(SRC, '../../src-tauri/src/events.rs'),
      'utf8'
    );
    const ts = read('domain/ipc-channels.ts');
    // 变异锁：任一侧改了字面量而另一侧没跟 → 事件永远送不到，两侧各自的单测全绿。
    expect(rust).toContain('EVENT_SUBSCRIPTION_UPDATE_PROGRESS: &str = "event:subscriptionUpdateProgress"');
    expect(ts).toContain("EVENT_SUBSCRIPTION_UPDATE_PROGRESS: 'event:subscriptionUpdateProgress'");
  });
});
