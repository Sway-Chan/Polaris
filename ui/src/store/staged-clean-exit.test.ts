/**
 * Q1-b 清除时机 ④「主进程退出前清 staged」的渲染端半边。
 *
 * # 判据为什么在后端
 *
 * 要区分的是「用户关了 App」与「进程没退但 webview 换了一个」（`window_health` 的白屏自愈重载、
 * C16 轻量模式 `tray_enter_lightweight` 销毁主窗 webview 后按需重建）。渲染端能拿到的
 * `beforeunload`/`pagehide` 在**重载时同样触发**，结构性判不了；退出那一刻再发指令给 webview 是竞态，
 * 强杀更是没有那一刻。故后端在**真退出腿**落一个持久标记，渲染端下次启动**在 hydrate 之前**读一次。
 *
 * # 本文件钉什么、不钉什么
 *
 * 钉：拿到真 ⇒ 清、拿到假 ⇒ 恢复、一个 webview 只问一次、问不到 ⇒ 保守恢复，
 * 外加两条会让上面全部失效却不会让任何断言转红的源码事实（介质、以及渲染端不许自作主张判退出）。
 * 不钉：「标记只在真退出腿落」（要跑起来的 Tauri 进程）与「标记读即清」——后者由
 * `src-tauri/src/clean_exit.rs` 的单测钉，本文件只消费它的结果。
 *
 * # 为什么每条都 `resetModules` + 动态 import
 *
 * 「一个 webview 只问一次」是 module 级的 memo，跨用例不重置就等于第二条起全部拿到上一条的结果。
 * 一次 `resetModules` + `import()` = 一次「新 webview 起来了」，正是这些不变式的自然单位。
 * 本仓 vitest 是 node 环境无 jsdom，storage 一律立桩（同 `i18n/language-hydration.test.ts` 先例）。
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { STAGED_STORAGE_KEY, encodeStagedPayload, type StagedEntry } from '@/lib/staged-config';
import type { UserConfig } from '@/contracts/types';

const cleanExitMock = vi.fn(async () => false);
vi.mock('@/ipc', () => ({
  api: {
    config: { get: async () => CONFIG, save: async () => ({ status: 'saved', version: 'v2' }) },
    proxy: { applyPendingChanges: async () => ({ status: 'applied' }) },
    window: { takeCleanExitFlag: () => cleanExitMock() },
  },
}));

const CONFIG = {
  servers: [{ id: 'n1', name: 'n1', port: 443 }],
  mixedPort: 7890,
} as unknown as UserConfig;

const ENTRY: StagedEntry = {
  id: 'server:n1',
  kind: 'server',
  label: '编辑节点 n1',
  entityPath: ['servers', 'n1'],
  nextValue: { id: 'n1', name: 'n1', port: 8443 },
};

class FakeStorage implements Storage {
  private map = new Map<string, string>();
  get length(): number {
    return this.map.size;
  }
  clear(): void {
    this.map.clear();
  }
  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }
  key(index: number): string | null {
    return [...this.map.keys()][index] ?? null;
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
}

let fake: FakeStorage;

const flush = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

/**
 * 起一个「新 webview」：重置模块（连同那个只问一次的 memo）+ 预置一份上次会话留下的暂存，
 * 然后跑首次 hydrate（= `app-store.loadConfig` 拿到 config 后的那一步）。
 */
async function bootWithPersistedStaged(): Promise<
  typeof import('./staged-config-store')
> {
  fake.setItem(
    STAGED_STORAGE_KEY,
    encodeStagedPayload({
      baseline: CONFIG as unknown as Record<string, unknown>,
      entries: [ENTRY],
    })
  );
  vi.resetModules();
  const mod = await import('./staged-config-store');
  // 总开关生产为关；这些不变式描述的是**开关打开后**的行为，故显式置真（同 store 单测口径）。
  mod.useStagedConfigStore.setState({ enabled: true });
  mod.hydrateStagedConfig(CONFIG);
  await flush();
  return mod;
}

beforeEach(() => {
  fake = new FakeStorage();
  vi.stubGlobal('localStorage', fake);
  cleanExitMock.mockClear();
  cleanExitMock.mockImplementation(async () => false);
});

describe('Q1-b ④ 正常退出标记 —— 恢复腿的闸', () => {
  /**
   * **正常退出后再启动 ⇒ staged 被清**（NFR-3）。不清则「关掉 App 再打开，两天前的半截编辑还在」，
   * 且会参与下一次保存的整份合成 —— Q1-b 管这叫埋雷。
   *
   * 牙：把 `consumeCleanExitFlag` 里的 `persist(null, [])` 删掉 → 条目照旧恢复 → 转红。
   */
  it('正常退出后再启动 ⇒ 持久化的暂存被清，内存里也没有', async () => {
    cleanExitMock.mockImplementation(async () => true);
    const mod = await bootWithPersistedStaged();
    expect(mod.useStagedConfigStore.getState().entries).toEqual([]);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
    // 清干净之后 baseline 必须跟到当前盘值，否则下一批编辑会拿一份陈旧基准去比冲突。
    expect(mod.useStagedConfigStore.getState().baseline).toEqual(CONFIG);
  });

  /**
   * **强杀 / 断电 / 崩溃后再启动 ⇒ staged 保留**。标记没落下 ⇒ 判不出「正常退出」⇒ 恢复。
   * 与 Q1-b 原文同向：宁可多恢复一次让用户看见「N 项待保存」，也不静默吞掉。
   *
   * 牙：把闸改成无条件清（不看返回值）→ 转红。
   */
  it('强杀后再启动 ⇒ 暂存照常恢复', async () => {
    cleanExitMock.mockImplementation(async () => false);
    const mod = await bootWithPersistedStaged();
    expect(mod.useStagedConfigStore.getState().entries).toEqual([ENTRY]);
    expect(fake.getItem(STAGED_STORAGE_KEY)).not.toBeNull();
  });

  /**
   * **进程没退（自愈重载 / C16 轻量模式销毁重建）⇒ staged 保留**。后端不会在这些路径上落标记，
   * 渲染端这一侧的表现与强杀同形：拿到假 ⇒ 恢复。这条与上一条断言相同、**前提不同**，
   * 留着是因为它对应的是两个独立的回归方向（后端把标记落错地方 vs 前端不看标记）。
   *
   * 牙：同上；后端那一半（标记只在 `prevent_exit` 放行之后落）由 `main.rs` 的调用点位置保证，
   * 本环境判不了，已在交付里列为真机项。
   */
  it('轻量模式销毁重建 webview（进程没退）⇒ 暂存照常恢复', async () => {
    cleanExitMock.mockImplementation(async () => false);
    const mod = await bootWithPersistedStaged();
    expect(mod.useStagedConfigStore.getState().entries).toEqual([ENTRY]);
  });

  /**
   * **一个 webview 只问一次**。标记是读即清的一次性资源：**并发**的两次首次 hydrate
   * （`loadConfig` 与 `loadConfig(true)` 前后脚落地，`hydrated` 还没置上）若各问一次，
   * 「谁先谁后」就成了清不清的决定因素 —— 那是竞态不是判据。
   *
   * 两次调用必须**都在 flush 之前**：flush 之后 `hydrated` 已置真，第二次走的是同步快路径、
   * 根本不碰这个闸，测出来恒是 1，等于没测（本条最初就是这么写的，变异存活）。
   *
   * 牙：把 `cleanExitGate ??=` 改成每次新建 promise → 调用次数变 2 → 转红。
   */
  it('并发的首次 hydrate 只问后端一次（标记是一次性资源）', async () => {
    cleanExitMock.mockImplementation(async () => true);
    fake.setItem(
      STAGED_STORAGE_KEY,
      encodeStagedPayload({
        baseline: CONFIG as unknown as Record<string, unknown>,
        entries: [ENTRY],
      })
    );
    vi.resetModules();
    const mod = await import('./staged-config-store');
    mod.useStagedConfigStore.setState({ enabled: true });
    mod.hydrateStagedConfig(CONFIG);
    mod.hydrateStagedConfig(CONFIG);
    await flush();
    expect(cleanExitMock).toHaveBeenCalledTimes(1);
    expect(mod.useStagedConfigStore.getState().entries).toEqual([]);
  });

  /**
   * **问不到就恢复**（后端没这条命令 / IPC 抛）。失败方向必须是「多恢复一次」而不是「清掉」，
   * 且绝不能把异常抛穿到 `loadConfig`。
   *
   * 牙：删掉 `consumeCleanExitFlag` 的 try/catch → 未捕获 rejection + 条目丢失 → 转红。
   */
  it('IPC 抛 ⇒ 当作非正常退出：暂存保留、不抛穿', async () => {
    cleanExitMock.mockImplementation(() => {
      throw new Error('command not found');
    });
    const mod = await bootWithPersistedStaged();
    expect(mod.useStagedConfigStore.getState().entries).toEqual([ENTRY]);
    expect(fake.getItem(STAGED_STORAGE_KEY)).not.toBeNull();
  });
});

// ---------------------------------------------------------------------------
// 两条源码事实：改动它们会让上面全部失去意义，却不会让任何行为断言转红。
// ---------------------------------------------------------------------------

const STORE_SRC = readFileSync(
  fileURLToPath(new URL('./staged-config-store.ts', import.meta.url)),
  'utf8'
);

describe('介质与信号来源（源码守卫：行为断言够不着的两条）', () => {
  /**
   * 介质必须是 `localStorage`。换成 `sessionStorage` 时上面每一条**依然全绿**（单测里 storage 是同一个桩），
   * 生产里却会变成：C16 轻量模式 idle 到点销毁主窗 webview → 用户唤出 → 暂存的编辑没了。
   * 那是定时器驱动的静默数据丢失，比自愈重载更常发生，正是 NFR-1 禁止的伤害。
   */
  it('持久化走 localStorage，不走 sessionStorage', () => {
    expect(STORE_SRC).toContain('localStorage');
    expect(STORE_SRC).not.toMatch(/\bsessionStorage\s*[.=;)]/);
  });

  /**
   * 渲染端**不许自己判「App 要退了」**：`beforeunload` / `pagehide` / `unload` 在**重载**时同样触发，
   * 挂清除腿上去就等于在自愈重载里吃掉用户的编辑（NFR-1）。判据只能来自后端标记。
   *
   * 牙：给 store 加一句 `addEventListener('beforeunload', ...)` → 转红。
   */
  it('渲染端不监听 beforeunload / pagehide / unload 来清暂存', () => {
    expect(STORE_SRC).not.toMatch(/addEventListener\s*\(\s*['"](beforeunload|pagehide|unload)['"]/);
  });
});
