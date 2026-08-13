/**
 * 「需重启 App 才生效」的配置键集合 —— U-7 的**第三类重启**（单一可替换点）。
 *
 * # 三类重启（本产品的重启语义分类，勿混）
 *
 * | 类别 | 谁需要重启 | 呈现通路 |
 * |---|---|---|
 * | 重启**内核** | sing-box 子进程 | pending-bar「待应用」差集（`proxy:getPendingChanges`） |
 * | 重启 **App** | Polaris 进程本身 | **本模块 + 保存后的确认弹窗**（U-7） |
 * | 无需重启 | — | 落盘即生效 |
 *
 * 三者互不相交：本集合的成员**结构性地不进** pending 差集（差集的定义是「核没吃进去」，
 * 而这些键根本不参与内核配置生成 —— 它们不是 `UserConfig` 的 Rust 侧字段，恒 `norm_equal` → NoOp 腿）。
 *
 * # 成员判据（唯一判据，新增成员必须逐条按它核实）
 *
 * **该键的消费点在 webview / 进程建立之前，且它描述的是「本次运行」的行为**。
 * 两个合取项缺一不可：
 *  - 前者 ⇒ 运行期改它对当前进程无任何作用（环境变量已被 runtime 读走 / builder 参数已定死 /
 *    插件注册窗口已过）。
 *  - 后者把「本来就只描述下次启动」的键排除掉（见下方 `silentStart`）—— 那类键不存在
 *    「以为生效了其实没生效」，弹窗只会是噪音。
 *
 * # ⚠️ 「三类重启」是当前全集，但它不是结构性保证
 *
 * 曾存在**第四类** = 「改了要重启内核、但差集看不见」的键，实例是 `logLevel` / `disableLogFile`：
 * `runtime/proxy.rs` 的 `log_axes_from_config` 从**原始 config JSON** 读这两键喂 `GenerateConfigDeps`
 * 注入 sing-box `log.*`，而两键都不在 `UserConfig::FIELD_NAMES` ⇒ `config_generation_norm` 恒相等
 * ⇒ 落 NoOp 腿 ⇒ 永不进 pending 差集：核在跑时关掉「关闭日志写盘」，sing-box 继续按旧值写盘，
 * pending-bar 不出现、本文件的弹窗也不出现，全程无提示。
 *
 * **2026-07-29 已收口**：两键已建模进 `UserConfig`（`app_config.rs`，用 `serde_json::Value` 保持解析
 * 宽容），改它们现在正常进 pending 差集。判据由 `runtime/proxy.rs` 的
 * `every_generation_input_key_is_visible_to_norm` 钉住：**凡被生成侧读到的 config 键都必须在
 * `FIELD_NAMES` 里**，再有人从裸 JSON 读一个新键去喂生成就会转红。
 *
 * 留这段的原因：那条守卫盯的是 `generate_deps` 这一个入口，**不是**「第四类不可能再出现」的证明。
 * 别因为本文件说了「三类」就以为结构上已经穷尽。
 *
 * # 成员逐条证据（2026-07-28 于磁盘核实）
 *
 * - **`hardwareAcceleration`** —— `src-tauri/src/main.rs:1196-1202` 在 setup 里、**首个 webview 创建之前**
 *   调 `graphics_compat::apply_hardware_acceleration_escape()` 设 GPU 环境变量
 *   （`WEBKIT_DISABLE_DMABUF_RENDERER` / `WEBKIT_DISABLE_COMPOSITING_MODE` / `--disable-gpu`，
 *   `graphics_compat.rs:92-123`）。各平台 runtime 只在建 webview 那一刻读这些变量 ⇒ 之后改无效。
 * - **`windowEffects`** —— `main.rs:887-888` + `:932-951` + `:982-1010`：`transparent` / `background_color`
 *   是 **builder-only 参数**（运行期不可改），vibrancy/Mica 也只在 `builder.build()` 之后挂一次。
 *   更糟的是前端 `resolveWindowEffectsState`（`components/layout/window-effects.ts:42`）是**实时**读这两个键的：
 *   运行期把它从关拨到开 → 前端立刻让位（`.stage`/`.win` 转透明）而原生窗仍是不透明 `#0B0F14`
 *   ⇒ 浅色主题下透出深底浅字。即「不重启」不是「没变化」，是**看得见的坏**。
 * - **`rememberWindowSize`** —— `main.rs:1302-1322`：`tauri-plugin-window-state` 是**按本键 gate 注册**的，
 *   且必须早于 `create_main_window`（插件靠 `on_window_ready` 恢复几何）。运行期拨开 → 插件没注册 ⇒
 *   本次会话的几何压根不会被记录，下次启动无从还原；运行期拨关 → 插件仍在 ⇒ 照样记录并还原。
 *   两个方向都是**静默无效**。
 *
 * # 刻意排除（同样在启动期读原文本，但不满足第二个合取项）
 *
 * - **`silentStart`**（`main.rs:88-92` / `:1330`）—— 它的语义本就是「**下次**启动时隐藏主窗」，
 *   本次运行不该有任何变化。为它弹「立即重启」会让应用重启后自己藏起来，是伤害不是修复。
 * - **`autoStart` / `autoConnect` / `autoCheckUpdate`** —— 同上，语义即「下次启动/启动期做什么」；
 *   且 `autoStart` 经 `autoStartApi` 立即写 OS launch agent，不存在延迟生效。
 * - **`builtinGeoMeta`**（`main.rs:1217-1227`）—— 迁移/播种元数据，不由设置页编辑，无 UI 入口。
 *
 * # 反面对照（都在启动期被读过，但**有**运行期消费者，故不属本集合）
 *
 * `uiTheme`（`tray.rs:430` 播种 + `AppShell` 运行期接管）、`language`（`tray.rs:90`，托盘菜单 live 重建）、
 * `logLevel`（`logging.rs:301` 启动读一次，运行期 `set_level` 改）、
 * `minimizeToTray`（`main.rs:143` 每次关窗**现读**）。
 */

import type { UserConfig } from '@/contracts/types';

/**
 * 集合本体。**单一可替换点**：新增/删除成员只改这一行 + 上方证据段，
 * 判定与呈现（`appRestartRequiredChanges` / 确认弹窗）无需同改。
 */
export const APP_RESTART_REQUIRED_KEYS = [
  'hardwareAcceleration',
  'windowEffects',
  'rememberWindowSize',
] as const;

export type AppRestartRequiredKey = (typeof APP_RESTART_REQUIRED_KEYS)[number];

/**
 * 归一到**后端启动期真正读到的判定值**。
 *
 * 三个成员在 Rust 侧是同一口径的「缺省为开、仅显式 `false` 才关」
 * （`graphics_compat.rs:72-82` 的 `field_is_explicit_false` / `main.rs:102-109` 的 `unwrap_or(true)`），
 * 故 `undefined` 与 `true` 是**同一个判定结果**，二者互换不触发弹窗（它确实什么也没改变）。
 *
 * ⚠️ 新增**非 bool 或缺省为关**的成员时必须先改这里，否则会漏报/误报。
 */
function effectiveValue(v: boolean | undefined): boolean {
  return v !== false;
}

/**
 * 本次保存里，哪些「需重启 App」的键**值真的变了**。
 *
 * - 只看 `patch` 里**出现过**的键：设置页每次写都是局部 patch，不出现即本次没碰。
 * - 只在归一后的值**不等**时命中：同值重复保存（受控组件回声 / 表单整份提交）不该弹窗。
 * - 返回顺序恒为 [`APP_RESTART_REQUIRED_KEYS`] 的声明顺序，便于文案稳定与断言。
 */
export function appRestartRequiredChanges(
  prev: Partial<UserConfig> | null | undefined,
  patch: Partial<UserConfig>,
): AppRestartRequiredKey[] {
  if (!prev) return [];
  return APP_RESTART_REQUIRED_KEYS.filter(
    (key) => key in patch && effectiveValue(patch[key]) !== effectiveValue(prev[key]),
  );
}

/**
 * 两份**完整** config 之间，哪些「需重启 App」的键值真的变了。
 *
 * 与 [`appRestartRequiredChanges`] 的差别只有一处，但不可合并：**没有 `key in` 守卫**。
 * 那条守卫的前提是「入参是局部 patch，键没出现 = 本次没碰」；而在整份 config 的比对里，
 * 键缺席不是「没碰」而是**取默认值**（三个成员均缺省为开）。沿用带守卫的版本会漏掉
 * 「把显式 `false` 抹回缺省」这类真实变更 —— 那恰恰是备份导入最常见的形态
 * （旧备份根本没有这些键 ⇒ 整类替换后键消失 ⇒ 值实际从关变回开）。
 *
 * 用于**非设置页发起**的变更：备份导入整类替换 `generalSettings`（`commands/misc.rs::backup_import_apply`
 * 在 Rust 侧直接落盘 + 广播，不经 `useConfig.update`）、托盘写入、后端自愈。
 */
export function appRestartRequiredDiff(
  prev: Partial<UserConfig> | null | undefined,
  next: Partial<UserConfig>,
): AppRestartRequiredKey[] {
  if (!prev) return [];
  return APP_RESTART_REQUIRED_KEYS.filter(
    (key) => effectiveValue(next[key]) !== effectiveValue(prev[key]),
  );
}

/**
 * 在「本次改动碰到的键」里，筛出**重启真的会改变什么**的那些。
 *
 * 上面两个函数回答的是「值变了没有」，判据基线是**上一次的值**。但「要不要提示重启」问的是另一件事：
 * 磁盘现值与**本次进程启动时后端真正读到的值**是否不同 —— 只有不同，重启才会带来变化。
 *
 * 少了这一层会误报：进程以 `hardwareAcceleration=true` 起来 → 用户关掉（弹窗，点「稍后」）→ 又打开
 * ⇒ 「值变了」成立，但磁盘值已回到启动值，重启什么都不会发生。而重启**会断代理**，
 * 用户要么白断一次，要么学会无视这个弹窗 —— 后者直接废掉 U-7 的全部价值。
 *
 * 两层是**交集**而不是替换：光看「≠ 启动值」会在每次无关保存的回声里反复弹（差异一直存在，
 * 直到用户重启为止）。必须「这次碰了」且「碰完之后确实与启动值不同」才提示。
 *
 * `startup` 取不到（IPC 失败 / 旧后端）→ 原样返回，退回只看「值变了」的旧行为：
 * 宁可多提示一次，也不静默——静默正是 U-7 要修的病。
 */
export function restartKeysStillPending(
  changed: readonly AppRestartRequiredKey[],
  startup: Partial<UserConfig> | null | undefined,
  next: Partial<UserConfig>,
): AppRestartRequiredKey[] {
  if (!startup) return [...changed];
  return changed.filter((key) => effectiveValue(next[key]) !== effectiveValue(startup[key]));
}
