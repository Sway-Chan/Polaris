/**
 * 屏幕级格式化小工具（提取自原型 polaris-prototype.html 的 latLevel / parseSize / parseTime 等）。
 *
 * 纯展示函数，无副作用。home/connections/logs 三屏共用。
 * 行号注释指向原型源，便于对照修订。
 */

/** 延迟分级（原型 latLevel，L3008-3010）。fast<80 / mid<150 / slow<300 / dead 其余。 */
export type LatLevel = 'fast' | 'mid' | 'slow' | 'dead' | 'none';

export function latLevel(v: number | null | undefined): LatLevel {
  if (v === undefined) return 'none';
  if (v === null || !Number.isFinite(v) || v < 0) return 'dead';
  if (v < 80) return 'fast';
  if (v < 150) return 'mid';
  if (v < 300) return 'slow';
  return 'dead';
}

/**
 * `.nm-latdot`（首页节点选单）专用色阶（原型 latClass，L3030）。与 latLevel 同阈值，但类名前缀不同
 * （`.lat-fast`/`.lat-mid`/`.lat-slow2`/`.lat-dead2`/`.lat-none`，样式定义见 prototype.css 独立于 `.nm-lat`
 * 的纯色阶类名，两套选择器不互通）。undefined=未测（none）/ null=超时（dead2，对齐真实数据模型的
 * timeout 语义——原型用字符串 'dead' 表示超时、null 表示未测，与我们的 null/undefined 语义正相反，
 * 故此处按语义而非字面值转写）。
 */
export function latDotClass(v: number | null | undefined): string {
  if (v === undefined) return 'lat-none';
  if (v === null) return 'lat-dead2';
  if (v < 80) return 'lat-fast';
  if (v < 150) return 'lat-mid';
  if (v < 300) return 'lat-slow2';
  return 'lat-dead2';
}

/**
 * 字节 → 人类可读（原型 parseSize/sizeFmt 的逆 + 渲染合一）。1024 进制。
 *
 * `< 1024` 那档此前是裸 `${n} B`。累计字节恒为整数，看不出问题；但 [`fmtRate`] 传进来的是
 * `(Δbytes / Δt)` 的**浮点**，于是速率列出现 `833.3333333333334 B/s`
 * （陈先生 2026-07-29 真机：「速率列应该最多只保留小数点后两位」）。
 * 统一收敛到最多两位小数，`Number(...)` 再包一层是为了让整数仍打印成整数（1023 而非 1023.00）。
 */
export function fmtBytes(n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return '—';
  if (n < 1024) return `${Number(n.toFixed(2))} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}

/** 字节/秒速率（连接信息页上下行）。 */
export function fmtRate(bytesPerSec: number | null | undefined): string {
  return fmtBytes(bytesPerSec) + '/s';
}

/**
 * 秒 → 时长（连接时长）。<60s 显**整 5 秒档**（floor，不超前），否则 m / h m。
 *
 * M8（2026-08-20）：首档若显整秒，存活 <60s 的新生连接的时长格就每秒换串（"12s"→"13s"）——
 * 连接表恰由新生短命连接主导，这是速率/累计迟滞后剩下的唯一每秒泵（WebKit 为高频重绘区域
 * 持续新建 graphics surface 不回收，.152 归因）。5 秒档把该格写频压到 1/5，人眼对「连接
 * 建立多久」的精度预期本就在十秒级。全仓仅连接表一处调用，无跨屏外溢。
 */
export function fmtDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || Number.isNaN(seconds)) return '—';
  if (seconds < 60) return `${Math.floor(seconds / 5) * 5}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${(m % 60)}m`;
}

/** RFC3339 → 相对时长（秒）。用于 ConnectionEntry.start → 连接已建立多久。 */
export function ageFromStart(start?: string): number | null {
  if (!start) return null;
  const t = Date.parse(start);
  if (Number.isNaN(t)) return null;
  return Math.max(0, (Date.now() - t) / 1000);
}

/** 运行时长（秒）→ 「2h 14m」式短串（home phead meta）。 */
export function fmtUptime(seconds: number | null | undefined): string {
  if (!seconds || seconds <= 0) return '0m';
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}
