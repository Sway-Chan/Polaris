/**
 * stats topic 订阅生命周期（`stats` / `aggregate` / `detail` **三条 topic 共用**；
 * 抽出以便 node 环境 vitest 直测）。
 *
 * 放 `lib/` 而不是某一屏的目录下：三个订阅点分属首页、连接页、状态栏，挂在其中任一屏下都会让
 * 另外两屏跨屏 import。本模块对 IPC 零耦合（端口由调用方注入），放中性目录即可。
 *
 * # 它守的两条不变式（都在真机上咬过人）
 *
 * 1. **监听先于订阅**：后端 poller 首拍不 sleep（`PollGate::next_tick` 的 `ticked` 分支），首帧常落在
 *    「订阅已生效、监听还没挂」的窗口里被丢掉 ⇒ 切回首页拓扑空着，要等下一次内容变化才出现。
 * 2. **订阅态同步置位**：`subscribe()` 是 async，若等 resolve 才把自己标成「已订阅」，快速进出页面时
 *    dispose 会以为没订阅过而不退订 ⇒ 后端订阅计数回不到 0、poller 不停机且内容签名残留 ⇒ 下次进
 *    页面被签名去重挡住，直到内容变化才有帧。
 *
 * 两条都不是「性能优化」，是「有没有画面」。
 */
export interface TopicPort<T> {
  /** 注册帧监听，返回同步的注销函数。 */
  onFrame(cb: (data: T) => void): () => void;
  subscribe(): Promise<void>;
  unsubscribe(): Promise<void>;
}

export interface TopicSubscription {
  /** 想不想要数据（可见且聚焦 = true）。幂等：同值重复调用不产生 IPC。 */
  setWanted(want: boolean): void;
  /** 卸载：注销监听 + 补发退订（**只要订过就退**，不看 IPC 有没有 resolve）。 */
  dispose(): void;
}

export function createTopicSubscription<T>(
  port: TopicPort<T>,
  onData: (data: T) => void
): TopicSubscription {
  // 先挂监听（不变式 1）。
  const off = port.onFrame(onData);
  let subscribed = false;
  let disposed = false;

  const setWanted = (want: boolean) => {
    if (disposed || want === subscribed) return;
    subscribed = want; // 同步置位（不变式 2）
    const p = want ? port.subscribe() : port.unsubscribe();
    // IPC 失败不回滚本地态：订阅态以本地为准，dispose 仍会补退订。非 Tauri 环境直接吞。
    p.catch(() => {});
  };

  const dispose = () => {
    if (disposed) return;
    disposed = true;
    off();
    if (subscribed) {
      subscribed = false;
      port.unsubscribe().catch(() => {});
    }
  };

  return { setWanted, dispose };
}
