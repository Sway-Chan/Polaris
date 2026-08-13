import { describe, it, expect } from 'vitest';
import { createTopicSubscription, type TopicPort } from './topic-subscription';

/** 记录调用顺序的假 port；subscribe/unsubscribe 的 promise 由测试手动 resolve（模拟慢 IPC）。 */
function fakePort() {
  const calls: string[] = [];
  let emit: ((d: number) => void) | null = null;
  let resolveSub: (() => void) | null = null;
  const port: TopicPort<number> = {
    onFrame(cb) {
      calls.push('listen');
      emit = cb;
      return () => calls.push('unlisten');
    },
    subscribe() {
      calls.push('subscribe');
      return new Promise<void>((res) => {
        resolveSub = res;
      });
    },
    unsubscribe() {
      calls.push('unsubscribe');
      return Promise.resolve();
    },
  };
  return {
    port,
    calls,
    emit: (d: number) => emit?.(d),
    resolveSubscribe: () => resolveSub?.(),
  };
}

describe('createTopicSubscription', () => {
  it('监听在订阅之前挂上（否则后端首拍那一帧会被丢）', () => {
    const f = fakePort();
    createTopicSubscription(f.port, () => {}).setWanted(true);
    expect(f.calls).toEqual(['listen', 'subscribe']);
    // 变异：把 onFrame 挪到 subscribe 之后 → 顺序变 ['subscribe','listen'] → 本条转红。
  });

  it('subscribe 尚未 resolve 就 dispose，仍然退订', async () => {
    // 守的缺陷：订阅态若等 .then() 才置位，快速进出页面 → 退订永不发出 → 后端计数漏一个、
    // poller 不停机且签名残留 → 下次进页面等到内容变化才有帧（真机表现：拓扑不显示/滞后）。
    const f = fakePort();
    const sub = createTopicSubscription(f.port, () => {});
    sub.setWanted(true);
    sub.dispose(); // IPC 还在飞
    expect(f.calls).toEqual(['listen', 'subscribe', 'unlisten', 'unsubscribe']);
    f.resolveSubscribe();
    await Promise.resolve();
    expect(f.calls.filter((c) => c === 'unsubscribe')).toHaveLength(1);
  });

  it('同值重复 setWanted 不产生额外 IPC', () => {
    const f = fakePort();
    const sub = createTopicSubscription(f.port, () => {});
    sub.setWanted(true);
    sub.setWanted(true);
    sub.setWanted(false);
    sub.setWanted(false);
    expect(f.calls).toEqual(['listen', 'subscribe', 'unsubscribe']);
  });

  it('dispose 之后 setWanted 不再发 IPC，dispose 幂等', () => {
    const f = fakePort();
    const sub = createTopicSubscription(f.port, () => {});
    sub.setWanted(true);
    sub.dispose();
    sub.dispose();
    sub.setWanted(true);
    expect(f.calls).toEqual(['listen', 'subscribe', 'unlisten', 'unsubscribe']);
  });

  it('未订阅就 dispose：只注销监听，不发空退订', () => {
    const f = fakePort();
    createTopicSubscription(f.port, () => {}).dispose();
    expect(f.calls).toEqual(['listen', 'unlisten']);
  });

  it('帧经监听回调透传', () => {
    const f = fakePort();
    const got: number[] = [];
    const sub = createTopicSubscription(f.port, (d) => got.push(d));
    sub.setWanted(true);
    f.emit(42);
    expect(got).toEqual([42]);
  });
});
