/**
 * 根级 ErrorBoundary —— C 类白屏（renderer 活着但 DOM 空）的渲染期防线。
 *
 * 定位：React 的渲染期抛错若无边界捕获，React 18 会**卸载整棵树** → `#root` 变空 → 纯背景色空窗，
 * 且不发任何平台事件（进程没死、页面加载成功），主进程侧只能靠 mount 健康门超时侦测。
 *
 * **必须挂在根**（`main.tsx` 里包住 `<App/>` 及其所有 provider），而非 App 返回值内部：
 * 挂在内部护不住 App 函数体的 hooks 与 provider 自身的 render 抛错 —— 而那些正是 C 类白屏的高发点
 * （上游 `A3` 实证：ErrorBoundary 挂在 `App.tsx` 返回的 JSX 里，等于没挂）。
 *
 * fallback 刻意**零依赖**：纯内联样式 + 硬编码双语文案，不走 i18n / 不用设计 token / 不引任何组件 ——
 * 因为坏掉的那一环**可能正是** i18n、theme 或 token 本身。逃生门自己崩了就没救了。
 */

import { Component } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/** 尽力上报，绝不因上报失败再抛（逃生门必须比它保护的东西更可靠）。 */
function reportSafely(cmd: string, args?: Record<string, unknown>): void {
  try {
    void invoke(cmd, args).catch(() => {});
  } catch {
    /* 非 Tauri 环境 / IPC 不可用：静默 —— 此处再抛就把兜底 UI 也搭进去了 */
  }
}

export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // 转发到 Rust 日志：这条是排障的唯一线索来源（白屏时用户只能导出日志）。
    reportSafely('renderer_log', {
      level: 'error',
      message: `ErrorBoundary 捕获渲染期异常：${error.message}\n${error.stack ?? ''}\ncomponentStack:${info.componentStack ?? ''}`,
    });
  }

  componentDidUpdate(_prevProps: Props, prevState: State): void {
    // fallback 挂上 = renderer 活着且正在显示**可交互**的兜底 UI（非空窗）→ 回发 ready 抑制主进程门的
    // 终局升级：用户已有兜底页 + 「重新加载」按钮，再叠一层终局页是冗余覆盖。
    // 注：门此前可能已自动 reload 过一次（可能治瞬态），本信号只阻断终局升级，不回滚已发生的 reload。
    if (!prevState.error && this.state.error) {
      reportSafely('renderer_ready');
    }
  }

  render(): ReactNode {
    if (!this.state.error) return this.props.children;
    return (
      <div
        style={{
          minHeight: '100vh',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          textAlign: 'center',
          padding: 24,
          boxSizing: 'border-box',
          background: '#0B0F14',
          color: '#E6EDF3',
          fontFamily: 'system-ui,-apple-system,"Segoe UI",Roboto,sans-serif',
        }}
      >
        <div style={{ maxWidth: 520 }}>
          <h1 style={{ fontSize: 18, fontWeight: 600, margin: '0 0 12px' }}>
            界面初始化失败 / Failed to initialize
          </h1>
          <p style={{ fontSize: 13, lineHeight: 1.6, color: '#9DA7B3', margin: '0 0 20px' }}>
            界面加载遇到问题，请尝试重新加载。网络代理不受影响。
            <br />
            The interface failed to load. Please try reloading. Your proxy connection is unaffected.
          </p>
          <button
            type="button"
            onClick={() => window.location.reload()}
            style={{
              font: 'inherit',
              fontSize: 13,
              color: '#fff',
              background: '#2F81F7',
              border: 0,
              borderRadius: 8,
              padding: '9px 20px',
              cursor: 'pointer',
            }}
          >
            重新加载 / Reload
          </button>
        </div>
      </div>
    );
  }
}
