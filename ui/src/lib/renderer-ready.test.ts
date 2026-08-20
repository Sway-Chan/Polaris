/**
 * W27：主窗 ready 必须描述“真实页面已提交”，不能描述 App 外壳或 Suspense spinner 已提交。
 *
 * 行为门锁文档级去重；接线门锁三条可见结果：首屏 chunk 与语言包并行预取、ready 位于 Suspense 内容
 * 腿、两类错误兜底仍能让隐藏窗立即上屏。
 */

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

function read(relative: string): string {
  return readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  vi.resetModules();
});

describe('renderer-ready 文档级出口', () => {
  it('同一文档的重复 commit 只回报一次', async () => {
    const { reportRendererReady } = await import('./renderer-ready');
    reportRendererReady();
    reportRendererReady();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('renderer_ready');
  });

  it('IPC 失败后解除去重，后续兜底仍可补发', async () => {
    invokeMock.mockRejectedValueOnce(new Error('transport down'));
    const { reportRendererReady } = await import('./renderer-ready');
    reportRendererReady();
    await Promise.resolve();
    reportRendererReady();
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });
});

describe('W27 首个可交互帧接线', () => {
  const router = read('../components/screens/ScreenRouter.tsx');
  const app = read('../App.tsx');
  const main = read('../main.tsx');
  const boundary = read('../components/ErrorBoundary.tsx');

  it('ready 从 App 外壳下沉到 Suspense 内容提交边界', () => {
    expect(app).not.toContain('IPC_CHANNELS.RENDERER_READY');
    expect(app).not.toContain('reportRendererReady');
    expect(router).toContain('function RendererReadyBoundary');
    expect(router).toContain('useLayoutEffect(() => reportRendererReady(), [])');
    expect(router).toMatch(
      /<Suspense fallback=\{loadingScreen\(\)\}>\s*<RendererReadyBoundary>[\s\S]*<\/RendererReadyBoundary>\s*<\/Suspense>/
    );
  });

  it('模块求值时只预取 nav-store 指向的首屏，打平 i18n 后的串行瀑布', () => {
    expect(router).toContain('const initialRoute = useNavStore.getState()');
    expect(router).toContain("initialRoute.scope === 'settings'");
    expect(router).toContain('screenLoaders[initialRoute.mainScreen]');
    expect(router).toContain('void initialLoader().catch');
  });

  it('React 同步失败与根 ErrorBoundary 都复用 ready 出口', () => {
    expect(main).toContain('reportRendererReady();');
    expect(boundary).toContain('reportRendererReady();');
    expect(boundary).not.toContain("reportSafely('renderer_ready')");
  });
});
