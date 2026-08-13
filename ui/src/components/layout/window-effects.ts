/**
 * 「窗口特效是否开启」的前端判定 —— 供 CSS 决定 mac 下 `.stage`/`.win` 该不该让位给原生 vibrancy。
 *
 * 为什么前端非要知道这件事（`index.css` 旧注释曾判定「拿不到 ⇒ 不做」，此处修正该结论）：
 *   mac 的两条建窗支路对前端的要求**相反**（`src-tauri/src/main.rs:440-459`）：
 *     · 特效开 → 建 `transparent(true)` 窗 + 全透明 backgroundColor + 挂 vibrancy
 *       ⇒ 前端必须让位（`.stage`/`.win` 不能自绘不透明底），否则 vibrancy 被盖住 = 用户报的「特效未生效」；
 *     · 特效关 → 保持 conf 的 `transparent:false` + **实色 #0B0F14** 底
 *       ⇒ 前端必须自绘（那块实色是**深色**，浅色主题下透出来就是深底浅字，不可读）。
 *   一套无条件 CSS 满足不了两条相反要求，故这个信号是必需的，不是锦上添花。
 *
 * 为什么**不需要**新造 Rust→webview 的状态回传（旧注释担心的「新造一套状态机」）：
 *   Rust 的门控 `should_apply_window_effects`（`src-tauri/src/graphics_compat.rs:65-67`）是
 *   `!explicit_false(windowEffects) && !explicit_false(hardwareAcceleration)` —— **两个否决位都是
 *   config.json 字段**，前端 store 里本来就有（`SettingsDisplay.tsx:141,158` 已在读同样两个字段渲染开关）。
 *   故此判定是对既有配置的纯函数复述，零新增 IPC / 零新增 Rust 代码 / 零新增状态源。
 *
 * **射程边界（已知残留，须真机确认）**：Rust 侧真正的第三个合取项是「`apply_vibrancy` 未报错」，
 *   失败只 `log::warn`（`main.rs:488-495`）、从不回传 webview，故本判定覆盖不到。mac 上该调用实际
 *   ~恒成功（Windows 的 Mica 在非 Win11 恒失败才是这一项的现实场景，而本判定只驱动 mac 规则）。
 *   若真机确认存在 mac vibrancy 失败场景，那时才值得让 Rust 回传最终结果——在此之前不预造。
 */

/** 特效三态：`unknown` = config 尚未加载完，CSS 按「不让位」兜底（见下方 resolve 注释）。 */
export type WindowEffectsState = 'on' | 'off' | 'unknown';

/** 判定所需的配置切片（只取两个否决位，与 Rust 门控同口径）。 */
export interface WindowEffectsConfigSlice {
  windowEffects?: boolean;
  hardwareAcceleration?: boolean;
}

/**
 * 镜像 `graphics_compat.rs::should_apply_window_effects` 的正向语义：
 * **两个否决位任一显式 `false` 即关**；缺失 / undefined 一律回落「默认开」（与 Rust 的
 * 「缺失 / 脏值 / 配置损坏 → 默认开」逐字段同口径，存量配置行为一致）。
 *
 * `config` 为 `undefined`（store 尚未载入）→ `'unknown'`：**故意不猜**。猜「开」会在特效实际关闭的
 * 机器上闪一帧透明（透出 #0B0F14 深底，浅色主题尤其刺眼）；猜「关」只是多显示一帧现有的不透明外观，
 * 随后校正。可读性优先于少闪一帧，故取后者（CSS 侧靠「属性缺失即不让位」实现，无需额外规则）。
 */
export function resolveWindowEffectsState(
  config: WindowEffectsConfigSlice | undefined,
): WindowEffectsState {
  if (!config) return 'unknown';
  return config.windowEffects !== false && config.hardwareAcceleration !== false ? 'on' : 'off';
}
