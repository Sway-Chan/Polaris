//! 图形兼容逃生门（D 类合成层白屏的用户自救手段）——纯判定 + 早期应用。
//!
//! 背景：白屏的 D 类向量（GPU 进程反复崩溃、合成层不出帧）用户无自救手段。`crates/store` 有
//! `hardwareAcceleration` / `windowEffects` 两个 schema 字段（`store.rs:199-200` 默认值、`sanitize.rs:74-75`
//! sanitize-not-throw、`config-engine/src/builder/orchestration.rs:124-125` 已排除出重启 norm）。
//! 本模块消费 `hardwareAcceleration`（GPU 环境变量）与 `windowEffects`（窗口特效门控），两个判定均为纯函数。
//!
//! **正向语义**（迁自 上游 `b180163` 的修正，别迁成反向开关）：字段默认 **true**（开），消费一律
//! `!= Some(false)`。即「默认开 = 行为逐字节不变」，用户手动关才自救。反向的 `disableX` + 默认 false
//! 是双否定，读者要绕两层。
//!
//! **容错第一**（逃生门自己崩了就没救了）：配置文件缺失 / 损坏 / 空 / 字段类型脏 → 一律**不禁**（回落默认开），
//! 绝不 panic、绝不因脏值误禁用户的硬件加速。判定是纯函数，全可单测。
//!
//! ## Tauri 下的正确形态（与 Electron 的差异）
//!
//! - **`hardwareAcceleration`**：Tauri **没有** `app.disableHardwareAcceleration()` 等价 API。webview 的
//!   GPU 开关由各平台 runtime 的**环境变量**控制，且必须在 webview 创建**之前**设好：
//!   - Linux（WebKitGTK）：`WEBKIT_DISABLE_DMABUF_RENDERER=1`（主修复：NVIDIA 白屏）
//!     + `WEBKIT_DISABLE_COMPOSITING_MODE=1`（兜底：resize 崩溃）
//!   - Windows（WebView2）：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--disable-gpu`
//!   - macOS（WKWebView）：**无受支持的开关** → 该项在 mac 上是 no-op（如实登记，不谎称生效）。
//!
//!   注：上游 因「Electron 在 Linux 无条件禁 HW accel」把该开关在 Linux 整卡隐藏（死开关）。**Polaris 不适用
//!   该前提** —— Tauri/WebKitGTK 的合成默认是开的，故 Linux 上这个开关是**活的**，不可照搬 上游的隐藏结论。
//!
//! - **`windowEffects`**：门控 `main.rs::create_main_window` 的 **macOS vibrancy / Windows Mica**（B6「窗口铬」
//!   已落地，走官方 `window-vibrancy` crate）。**本段曾记「无行为消费」，那是 B6 落地前的状态，已过时** ——
//!   现有真实消费方，判定见 [`should_apply_window_effects`]。
//!
//!   **Linux 是结构性 no-op**：WebKitGTK 无 vibrancy/Mica 等价物，建窗处的特效分支本就 `#[cfg(mac/win)]`
//!   编译隔离 → Linux 目标根本没有可关的特效。故 UI 侧同步隐藏该行（`components.css` / `prototype.css` 的
//!   `:root[data-os="lin"] #set-window-effects{display:none}`），不留「拨了没反应」的死开关。
//!
//!   **与 `transparent` 的耦合 —— 别只摘特效调用**：mac/win 建窗时把 conf 的 `transparent:false` 覆盖成 true、
//!   并把 backgroundColor 清成全透明，**唯一目的**就是让原生 vibrancy/Mica 透上来；前端 `.win` / `.side`
//!   在 `[data-os=mac|win]` 下同样是 CSS `background:transparent`（`.win` 见 `index.css` 的
//!   `:root[data-os="mac"][data-window-effects="on"] .win`，`.side` 见 `components.css` 的
//!   `:root[data-os="mac"] .side`）。**别写成 `.win-frame`** —— 那个 class 全仓无元素渲染，选择器恒打空，
//!   正是「mac 透明看着做了、真机却不生效」的老坑；`style-invariants.test.ts` 现有守卫禁它复发。故若只跳过特效
//!   调用而仍建透明窗，得到的是**半透明穿透窗**（侧栏直接透出桌面），而不是开关文案承诺的「纯色背景」。
//!   正确形态：`transparent` 与特效调用受**同一个** [`should_apply_window_effects`] 门控 —— 不上特效就建
//!   不透明窗，回落 conf 的 `backgroundColor:#0B0F14` 实色底。
//!
//!   **生效时机 = 必须重启**：`transparent` 是 builder-only、运行期不可改，故本项做不了运行期热切
//!   （`window-vibrancy::clear_vibrancy` 只能撤特效，救不了 transparent 那一半）。UI 文案已如实告知需重启。

use std::path::Path;

/// 从 config.json 原文本判定「是否该禁用硬件加速」。
///
/// 正向语义：仅当字段严格 `=== false` 才判禁。缺失 / 非 bool 脏值（`"false"` / `0` / null）→ 默认开 → 不禁。
pub fn should_disable_hardware_acceleration(raw: Option<&str>) -> bool {
    field_is_explicit_false(raw, "hardwareAcceleration")
}

/// 从 config.json 原文本判定「建主窗时是否该上窗口特效」（macOS vibrancy / Windows Mica）。
///
/// **两个否决位，任一显式 `false` 即不上特效**：
///  - `windowEffects === false`：用户显式关特效（本项的直接开关）。
///  - `hardwareAcceleration === false`：图形逃生门已开。vibrancy/Mica 本身就是合成层负载（正是 D 类白屏
///    向量），逃生门开着还上特效自相矛盾 —— 用户是来自救的，不该被特效再拖回白屏。
///
/// 正向语义与 [`should_disable_hardware_acceleration`] 同口径：两字段默认 true，缺失 / 脏值 / 配置损坏
/// 一律回落「默认开」→ 上特效（存量配置行为逐字节不变）。
///
/// **调用方注意**：`transparent` 建窗参数必须与本判定同门控，理由见模块头「与 `transparent` 的耦合」。
pub fn should_apply_window_effects(raw: Option<&str>) -> bool {
    !field_is_explicit_false(raw, "windowEffects") && !should_disable_hardware_acceleration(raw)
}

/// 公共判定：顶层对象的 `key` 是否**显式**为 JSON `false`。
///
/// 任何异常（非字符串 / 空串 / JSON 解析失败 / 顶层非对象 / 字段缺失 / 类型非 bool）一律 false，绝不抛。
fn field_is_explicit_false(raw: Option<&str>, key: &str) -> bool {
    let Some(text) = raw else { return false };
    if text.trim().is_empty() {
        return false;
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        // JSON 损坏/截断：视为默认开，不禁（早期抛 = 整个启动失败）。
        return false;
    };
    parsed.get(key) == Some(&serde_json::Value::Bool(false))
}

/// 同步读 config.json 原文本。任何 IO 失败 → None（→ 全部判定回落默认开）。
///
/// 刻意读原文本而非走 `crates/store`：本判定发生在 webview 创建前的极早期，此时 store 尚未装配；且
/// 逃生门必须在「配置损坏到 store 都加载不了」时仍能工作。
pub fn read_config_raw(config_dir: &Path) -> Option<String> {
    std::fs::read_to_string(config_dir.join("config.json")).ok()
}

/// 应用硬件加速逃生门：设平台环境变量。**必须在首个 webview 创建之前调用**（各平台 runtime 在创建
/// webview 时才读这些变量）。
///
/// 不覆盖用户已显式设置的同名变量（用户手动设的优先级更高，且覆盖会打断排障者的临时实验）。
pub fn apply_hardware_acceleration_escape(disable: bool) {
    if !disable {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        // 两个变量分工不同，缺一不可（官方 https://v2.tauri.app/develop/debug/linux-graphics/）：
        //  - DMABUF_RENDERER：**主修复向量**。NVIDIA 专有驱动上 WebKitGTK 的 DMABUF framebuffer 导入失败
        //    （Error 71 / "Failed to create GBM buffer"）→ 整窗白屏。官方把它列为 NVIDIA 白屏的首选修复。
        //  - COMPOSITING_MODE：官方定位是「resize 时静默崩溃的最后手段」，**不是**白屏主修复。
        // 故两者并设：DMABUF 治白屏主因，COMPOSITING 兜合成层崩溃。均 set-if-absent。
        set_env_if_absent("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        set_env_if_absent("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        log::warn!(
            "图形兼容逃生门：hardwareAcceleration=false → 已设 WEBKIT_DISABLE_DMABUF_RENDERER=1 + WEBKIT_DISABLE_COMPOSITING_MODE=1（软件渲染）"
        );
    }
    #[cfg(target_os = "windows")]
    {
        set_env_if_absent("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--disable-gpu");
        log::warn!("图形兼容逃生门：hardwareAcceleration=false → 已设 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--disable-gpu");
    }
    #[cfg(target_os = "macos")]
    {
        // 如实登记：WKWebView 无受支持的禁 GPU 开关。不谎称生效（上游的教训：Linux 死开关 ON 态
        // 谎称「硬件加速已启用」，误导用户白跑一轮自救）。
        log::warn!("图形兼容逃生门：hardwareAcceleration=false，但 macOS(WKWebView) 无受支持的禁用开关 → 本项在 macOS 无效");
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn set_env_if_absent(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        // SAFETY: 在 Tauri Builder 启动前的单线程 main 早期调用，无并发读者。
        unsafe { std::env::set_var(key, value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_false_disables() {
        let raw = r#"{"hardwareAcceleration":false}"#;
        assert!(should_disable_hardware_acceleration(Some(raw)));
    }

    #[test]
    fn explicit_true_keeps_default_on() {
        let raw = r#"{"hardwareAcceleration":true}"#;
        assert!(!should_disable_hardware_acceleration(Some(raw)));
    }

    /// 正向语义核心：字段缺失 = 默认开 = 不禁。存量配置（升级前落盘的）必须逐字节行为不变。
    #[test]
    fn missing_field_keeps_default_on() {
        let raw = r#"{"language":"zh-CN"}"#;
        assert!(!should_disable_hardware_acceleration(Some(raw)));
    }

    /// 容错第一：绝不因脏值误禁用户的硬件加速（"false"/0/null 都不是 JSON false）。
    #[test]
    fn dirty_values_never_disable() {
        for raw in [
            r#"{"hardwareAcceleration":"false"}"#,
            r#"{"hardwareAcceleration":0}"#,
            r#"{"hardwareAcceleration":null}"#,
            r#"{"hardwareAcceleration":[]}"#,
            r#"{"hardwareAcceleration":{}}"#,
        ] {
            assert!(
                !should_disable_hardware_acceleration(Some(raw)),
                "脏值 {raw} 不得触发禁用"
            );
        }
    }

    /// 容错第一：文件缺失 / 空 / 损坏 / 顶层非对象 → 一律不禁，且绝不 panic。
    #[test]
    fn broken_config_never_disables_and_never_panics() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("{"),
            Some("not json at all"),
            Some("[1,2,3]"),
            Some("null"),
            Some(r#"{"hardwareAcceleration":fals"#),
        ] {
            assert!(!should_disable_hardware_acceleration(raw));
        }
    }

    /// 单向依赖：`windowEffects` 现已有行为消费（门控 vibrancy/Mica），但它**不得反向**影响
    /// hardwareAcceleration 的 GPU 环境变量判定 —— 关特效不该连带禁掉硬件加速。
    #[test]
    fn window_effects_field_does_not_affect_hardware_acceleration() {
        let raw = r#"{"hardwareAcceleration":true,"windowEffects":false}"#;
        assert!(!should_disable_hardware_acceleration(Some(raw)));
        let raw = r#"{"hardwareAcceleration":false,"windowEffects":true}"#;
        assert!(should_disable_hardware_acceleration(Some(raw)));
    }

    /// 直接开关：`windowEffects === false` → 不上特效。
    #[test]
    fn window_effects_explicit_false_skips_effects() {
        assert!(!should_apply_window_effects(Some(
            r#"{"windowEffects":false}"#
        )));
    }

    /// 正向语义：显式 true / 字段缺失 → 上特效（存量配置逐字节不变）。
    #[test]
    fn window_effects_true_or_missing_applies_effects() {
        assert!(should_apply_window_effects(Some(
            r#"{"windowEffects":true}"#
        )));
        assert!(should_apply_window_effects(Some(r#"{"language":"zh-CN"}"#)));
    }

    /// 第二个否决位：逃生门开着（hardwareAcceleration=false）→ 即便 windowEffects 为 true / 缺失也不上特效
    /// （vibrancy/Mica 本身就是合成层负载，正是用户要躲的白屏向量）。
    #[test]
    fn hardware_acceleration_off_also_skips_effects() {
        assert!(!should_apply_window_effects(Some(
            r#"{"hardwareAcceleration":false,"windowEffects":true}"#
        )));
        assert!(!should_apply_window_effects(Some(
            r#"{"hardwareAcceleration":false}"#
        )));
    }

    /// 真值表全枚举：两个否决位的 4 种组合，仅「两个都不是 false」才上特效。
    /// 覆盖所有逃逸路径 —— 把实现改成任一单字段判定、或把 && 换成 ||，都会有格子翻红。
    #[test]
    fn window_effects_truth_table() {
        for (hw, we, expect) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            let raw = format!(r#"{{"hardwareAcceleration":{hw},"windowEffects":{we}}}"#);
            assert_eq!(
                should_apply_window_effects(Some(&raw)),
                expect,
                "hardwareAcceleration={hw} windowEffects={we} 应 apply_effects={expect}"
            );
        }
    }

    /// 容错第一：配置缺失 / 空 / 损坏 / 顶层非对象 → 回落默认开 = 上特效，且绝不 panic。
    /// （逃生门自己崩了就没救了；脏值绝不误关用户的窗口特效。）
    #[test]
    fn broken_config_keeps_effects_on_and_never_panics() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("{"),
            Some("not json at all"),
            Some("[1,2,3]"),
            Some("null"),
            Some(r#"{"windowEffects":"false"}"#),
            Some(r#"{"windowEffects":0}"#),
            Some(r#"{"windowEffects":null}"#),
        ] {
            assert!(should_apply_window_effects(raw), "{raw:?} 不得关掉窗口特效");
        }
    }

    #[test]
    fn read_config_raw_missing_file_is_none() {
        let dir = std::env::temp_dir().join("polaris-graphics-compat-absent");
        assert!(read_config_raw(&dir).is_none());
    }
}
