/**
 * 内存治理的跨层接线门。
 *
 * 纯函数测试能证明“裁剪 / 对账怎么算”，本文件负责证明这些能力真的接在页面、IPC 与窗口销毁路径上；
 * 否则下一次重构很容易留下一个测试全绿、生产所有权已断开的孤立 helper。
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

function source(relative: string): string {
  return readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');
}

describe('长期内存所有权接线', () => {
  it('日志页以 mount token 成对登记与退订，且监听先于水合', () => {
    const logs = source('./components/screens/logs/LogsScreen.tsx');
    const listenAt = logs.indexOf('api.logs.onReceivedBatchReady(onBatch)');
    const getAt = logs.indexOf('.get(subscriptionId, MAX_RENDERED_ROWS)');
    expect(listenAt).toBeGreaterThan(0);
    expect(getAt).toBeGreaterThan(listenAt);
    expect(logs).toContain('api.logs.unsubscribe(subscriptionId)');
    expect(logs).toContain('followRef.current');
  });

  it('窗口 reload 与销毁都兜底清理日志订阅', () => {
    const main = source('../../src-tauri/src/main.rs');
    const tray = source('../../src-tauri/src/tray.rs');
    expect(main).toContain('commands::misc::clear_log_stream_window("main")');
    expect(tray).toContain('crate::commands::misc::clear_log_stream_window("main")');
  });

  it('主窗重建、销毁和后台可见性探针共用跨平台生命周期边界', () => {
    const main = source('../../src-tauri/src/main.rs');
    const tray = source('../../src-tauri/src/tray.rs');
    const stats = source('../../src-tauri/src/runtime/stats.rs');
    const misc = source('../../src-tauri/src/commands/misc.rs');

    // W18b：唤出漏斗先 spawn 脱帧再排回主线程（接收者名会漂移，钉「点调用+实参」形态）。
    expect(main).toContain('tauri::async_runtime::spawn(async move {');
    expect(main).toContain('.run_on_main_thread(move ||');
    expect(main).toContain('rt.stats().mark_main_window_created()');
    expect(tray).toContain('rt.stats().mark_main_window_destroying()');
    expect(stats).toContain('window_alive: AtomicBool');
    expect(misc).toContain('runtime.stats().window_visible(app)');

    const visibleLogWindows = misc.slice(
      misc.indexOf('fn visible_log_windows('),
      misc.indexOf('\n/// 取尾部最多', misc.indexOf('fn visible_log_windows('))
    );
    expect(visibleLogWindows).not.toContain('.is_visible(');
    expect(visibleLogWindows).not.toContain('.is_minimized(');
  });

  it('权威配置水合后统一对账全部节点与订阅缓存', () => {
    const app = source('./App.tsx');
    expect(app).toContain('if (!config) return;');
    for (const call of [
      'useLatencyStore.getState().retainServerIds(serverIds)',
      'useAppStore.getState().retainServerIds(serverIds)',
      'useTailscaleLoginCacheStore.getState().retainServerIds(serverIds)',
      'useSubscriptionProgressStore.getState().retainSubscriptionIds(subscriptionIds)',
    ]) {
      expect(app).toContain(call);
    }
    expect(app).not.toContain('useRef<Set<string>>(new Set())');
    expect(app).toContain('useRef<Map<string, string>>(new Map())');
  });

  it('日志 API 的 get/search/unsubscribe 跨层通道成对存在', () => {
    const channels = source('./domain/ipc-channels.ts');
    const client = source('./ipc/api-client.ts');
    const main = source('../../src-tauri/src/main.rs');
    expect(channels).toContain("LOGS_UNSUBSCRIBE: 'logs_unsubscribe'");
    expect(channels).toContain("LOGS_SEARCH: 'logs_search'");
    expect(client).toContain('invoke(IPC_CHANNELS.LOGS_UNSUBSCRIBE, { subscriptionId })');
    expect(client).toContain('invoke(IPC_CHANNELS.LOGS_SEARCH, { query, level, source, limit })');
    expect(main).toContain('logs_unsubscribe,');
    expect(main).toContain('logs_search,');
  });
});
