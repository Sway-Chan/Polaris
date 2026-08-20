/** 日志流自动跟随的滚动来源判据。纯函数，避免 DOM 事件接线与 follow 语义混在组件里。 */

/** 距底不超过此值仍视为贴底，容忍子像素、字体重排与一行高度抖动。 */
export const LOG_AT_BOTTOM_PX = 30;

/** 用户输入到 scroll 事件的归因窗口；只覆盖同一轮输入派发，不把很久后的布局滚动算给用户。 */
export const USER_SCROLL_INTENT_WINDOW_MS = 250;

export interface LogScrollMetrics {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
}

export interface FollowPauseInput {
  follow: boolean;
  metrics: LogScrollMetrics;
  /** 最近一次滚轮/触摸/拖动/键盘滚动意图；从未发生则为 null。 */
  lastUserIntentAt: number | null;
  now: number;
}

/** 当前滚动盒是否仍贴着底部。 */
export function isLogViewAtBottom(metrics: LogScrollMetrics): boolean {
  return (
    metrics.scrollHeight - metrics.scrollTop - metrics.clientHeight <= LOG_AT_BOTTOM_PX
  );
}

/** 键盘动作是否明确表达“往历史方向滚”。End/PageDown 等向底部动作不应暂停。 */
export function isUpwardLogScrollKey(key: string, shiftKey = false): boolean {
  return (
    key === 'ArrowUp' ||
    key === 'PageUp' ||
    key === 'Home' ||
    (key === ' ' && shiftKey)
  );
}

/**
 * scroll 事件是否应暂停 follow。
 *
 * DOM 在首次水合（缓冲可达 500 行、DOM 按页）、字体/layout 更新、以及 `scrollTop = scrollHeight`
 * 时也会发 scroll；这些
 * 事件没有近期用户滚动意图，必须忽略。只有“正在跟随 + 用户刚操作 + 已离底”三项同时成立才暂停。
 */
export function shouldPauseLogFollow(input: FollowPauseInput): boolean {
  if (!input.follow || isLogViewAtBottom(input.metrics)) return false;
  if (input.lastUserIntentAt === null) return false;
  const age = input.now - input.lastUserIntentAt;
  return age >= 0 && age <= USER_SCROLL_INTENT_WINDOW_MS;
}
