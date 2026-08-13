//! 应用内语言设置 → **进程有效本地化**（macOS `AppleLanguages`），仅 macOS 有行为。
//!
//! ══ 这条改动补的是 `src-tauri/Info.plist` 那条明确留下的缺口 ══
//!
//! `CFBundleLocalizations`（Info.plist）解决的是「**跟随系统**」：macOS 用
//! 「用户 `AppleLanguages` ∩ 应用声明的本地化集合」算应用的有效本地化，应用一个都不声明时
//! 交集恒空 → 一律发 `en`。声明之后，系统是 `zh-Hans-CN` 的机器就能拿到 `zh-Hans`。
//!
//! 但用户在设置里把语言**显式改成与系统不同的值**（系统中文、应用内选俄语）时，那个交集里
//! 的用户侧集合仍是**系统**的 `AppleLanguages`，与前端 i18next 的选择毫无关系。链路
//! `tauri-plugin-dialog → rfd → NSOpenPanel`，AppKit 取的是**进程的**有效本地化，
//! 而它在进程启动早期就定死了。故此处把应用内的选择写进**应用自己的** UserDefaults 域
//! （`~/Library/Preferences/<identifier>.plist` 的 `AppleLanguages`）—— 该域优先级高于
//! `NSGlobalDomain`，于是「用户侧集合」变成应用内的选择，交集随之改变。
//!
//! ══ 生效语义：**下次启动**（这是本机制的固有代价，不是实现偷懒）══
//!
//! AppKit / CFBundle 在进程内**只解析一次**本地化并缓存，NSOpenPanel 的按钮、边栏、
//! 「新建文件夹」等全部取自它。故改语言当场不可能变，必须重启 App。
//!
//! 本模块把写入放在 [`crate::main`] 的**最早**位置（`tauri::Builder` 之前）而不是 `setup`，
//! 理由是**重启次数**：Tauri 2 的 `setup` 由 `Builder::build()` 在 `R::new(runtime_args)`
//! **之后**调用（`tauri-2.11.5/src/app.rs:2344` 建 runtime、`:2531` 才 `(setup)(app)`），
//! 那时 tao 已经建过 `NSApplication`。若在 `setup` 写：
//!
//! | 时点 | 磁盘 `config.language` | 已写入的 `AppleLanguages` | 本次 AppKit 读到 |
//! |------|------------------------|---------------------------|------------------|
//! | 改语言为 ru | ru | en（上次写的） | —（进程没重启） |
//! | 重启①      | ru | en → setup 才改写 ru | **en**（写在它后面） |
//! | 重启②      | ru | ru | ru |
//!
//! 即「改语言 → 要重启**两次**」。写在 `main()` 开头则重启①就读到 ru，**一次**。
//!
//! 至于「写在本次 AppKit 初始化之前，本次是不是就生效了」——**未验证**，本机是 Linux，
//! 验不了；且它对判据没有影响：写入的值来自本次启动读到的磁盘配置，本次生效与否都不改变
//! 「改语言后重启一次」这个用户可见语义。UI 文案按**一次重启**承诺（`languageDescMac`）。
//!
//! ══ 为什么是「启动时按配置对账」而不是「改语言时顺手写一次」══
//!
//! 写在改语言那一刻同样只在下次启动生效，看似等价，但它是**钩子**：config 的写入面不止
//! 设置页（备份导入、staged 应用、外部编辑 config.json、配置回滚都能改 `language`），
//! 漏挂任何一条腿就静默漂移，且漂移后永不自愈。启动对账是**汇流点**：无论上一次是怎么
//! 改的，本次启动一定把 `AppleLanguages` 拉回与 `config.language` 一致，任意脏状态一步收敛。
//!
//! ══ `auto` 必须**删键**，不能留旧值 ══
//!
//! 用户从「俄语」改回「跟随系统」时若只是不写，应用域里那条 `["ru"]` 还在 → 应用永久卡在
//! 俄语，且症状表现为「跟随系统坏了」，与 Info.plist 修好的那条缺陷长得一模一样。
//! 故 `auto` / 配置缺失 / 脏值一律 `removeObjectForKey` 回落系统域。
//!
//! ══ 码表必须与 Info.plist 的 `CFBundleLocalizations` 逐项一致 ══
//!
//! 写进 `AppleLanguages` 的码若不在应用声明的本地化集合里，交集为空 → 回落英文，**静默**。
//! 三方（[`APPLE_LANGUAGES`] / `ui/src/domain/language.ts` 的 `SUPPORTED_LANGUAGES` /
//! Info.plist）由 `ui/src/contracts/app-language-coverage.test.ts` 三向锁死。
//!
//! ══ 射程：仅 macOS ══
//!
//! Linux 的 WebKitGTK 读环境 locale、Windows 的 WebView2 与 Win32 通用对话框读系统区域设置，
//! 都不经 `AppleLanguages`。两平台上本模块的两个入口是**空函数**（保留调用点，让 `main()`
//! 的顺序守卫在三平台上都有判据）。

// ────────────────────────────────────────────────────────────────────────────
// 纯判定（无平台 API，Linux 上照样单测）
//
// 这一段全部带 `#[cfg(any(target_os = "macos", test))]`：消费方只有 macOS 的 `apply_process_language`
// 与本模块单测，Linux/Windows 的非测试构建里它们无人调用 → `-D dead-code` 直接把三平台 clippy 打红。
// 用 cfg 而不是 `#[allow(dead_code)]`：allow 会把**真正**变成死代码（比如哪天 macOS 那侧不再调用）
// 也一并盖住，那正是本仓要 `-D warnings` 的理由。
// ────────────────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "macos", test))]
use std::path::Path;

/// 界面语言（i18n 资源键）→ macOS 本地化码。
///
/// **左列**必须等于 `ui/src/domain/language.ts` 的 `SUPPORTED_LANGUAGES`（少一项 = 该语种用户
/// 拿不到原生对话框本地化）；**右列**必须是 `src-tauri/Info.plist` `CFBundleLocalizations` 的
/// 子集（不在里面 = 交集为空 = 静默回落英文）。两条都由跨语言覆盖门守。
///
/// `en-US → en` / `zh-CN → zh-Hans` 不是随便挑的：macOS 的本地化码用**脚本**（`zh-Hans`/`zh-Hant`）
/// 而非地区消歧，与 i18next 的资源键口径不同，直接把 `zh-CN` 写进去在 macOS 侧不匹配任何声明项。
#[cfg(any(target_os = "macos", test))]
pub const APPLE_LANGUAGES: &[(&str, &str)] = &[
    ("zh-CN", "zh-Hans"),
    ("zh-TW", "zh-Hant"),
    ("en-US", "en"),
    ("ru", "ru"),
    ("fa", "fa"),
];

/// 配置里的语言选择 → macOS 本地化码。`auto` / 未知码 / 空 → `None`（= 跟随系统 = 删键）。
///
/// `fa-IR` 迁移到 `fa`：与 `ui/src/domain/language.ts::migrateLanguageCode` 同口径 —— 旧版本
/// 存过 `fa-IR`，Rust 侧不迁移的话波斯语存量用户在这条腿上恒回落系统语言。
#[cfg(any(target_os = "macos", test))]
pub fn apple_language_for(choice: &str) -> Option<&'static str> {
    let c = if choice == "fa-IR" { "fa" } else { choice };
    APPLE_LANGUAGES
        .iter()
        .find(|(key, _)| *key == c)
        .map(|(_, code)| *code)
}

/// 从 config.json **原文本**取语言选择并映射成 macOS 码。
///
/// 容错口径对齐 [`crate::graphics_compat`]：文件缺失 / JSON 损坏 / 顶层非对象 / 字段缺失 /
/// 类型非字符串一律 `None`（跟随系统），**绝不 panic** —— 这段跑在 `main()` 的第一屏，
/// 此处抛异常 = 整个 App 起不来，而它换来的收益不过是一个对话框的语言。
#[cfg(any(target_os = "macos", test))]
pub fn apple_language_from_raw(raw: Option<&str>) -> Option<&'static str> {
    let parsed = serde_json::from_str::<serde_json::Value>(raw?).ok()?;
    apple_language_for(parsed.get("language")?.as_str()?.trim())
}

/// 用户 config.json 的绝对路径（macOS：`$HOME/Library/Application Support/<identifier>/polaris/config.json`）。
///
/// **不经 `AppHandle`**：调用点在 `tauri::Builder` 之前，那时 `App` 还不存在，
/// `PathResolver` 也就拿不到。故这里按 Tauri 自己的规则重算一次：
/// `app_config_dir()` = `dirs::config_dir()/<identifier>`（`tauri-2.11.5/src/path/desktop.rs:238-242`），
/// 而 `dirs::config_dir()` 在 macOS 上 = `$HOME/Library/Application Support`（`dirs-6.0.0/src/mac.rs:7`）。
/// 本函数只在 macOS 上被调用，故这条等式是确定的、无平台分支。
///
/// 叶名不另写字面量，取 [`crate::runtime::uninstall::CONFIG_DIR_LEAF`] 同一个常量。
///
/// **失配的形态是静默的**：路径错 → 读不到 config → 一律当 `auto` → 原生对话框恒跟随系统，
/// 与本改动落地前一模一样，没有任何报错。故 [`log_startup_outcome`] 会把用到的路径打进日志，
/// 真机上一眼可辨（判据见交付清单）。
#[cfg(any(target_os = "macos", test))]
fn user_config_path(home: &Path, identifier: &str) -> std::path::PathBuf {
    home.join("Library")
        .join("Application Support")
        .join(identifier)
        .join(crate::runtime::uninstall::CONFIG_DIR_LEAF)
        .join("config.json")
}

// ────────────────────────────────────────────────────────────────────────────
// macOS 实现
// ────────────────────────────────────────────────────────────────────────────

/// 本次启动的写入结果，供 [`log_startup_outcome`] 在日志 sink 装好之后补报。
///
/// **存在的唯一理由是让「没执行」自曝**：本模块跑在 `logging::init` 之前，那时 `log::*` 是
/// 静默 no-op，直接写日志等于没写。而它的失败形态全是「悄悄什么都没做」（`HOME` 取不到、
/// config.json 读不出），真机上排查者手里将没有任何证据。
#[cfg(target_os = "macos")]
static STARTUP_OUTCOME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 按 `config.language` 对账进程的 `AppleLanguages`。**必须在 `tauri::Builder` 之前调用**
/// （理由见模块文档「生效语义」一节；`main.rs` 的顺序守卫钉着这一点）。
///
/// 幂等、best-effort：任何一步取不到就什么都不做（保持上次的值，而不是清成系统语言 ——
/// 读不出配置时把用户已选好的语言抹掉，比不动更糟）。
#[cfg(target_os = "macos")]
pub fn apply_process_language(identifier: &str) {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        let _ = STARTUP_OUTCOME.set("跳过：$HOME 未设，算不出配置路径".to_owned());
        return;
    };
    let path = user_config_path(&home, identifier);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        // 首次启动（配置尚未落盘）会走到这里，属正常。此时**不删键**也不写键：没配置 = 没选过，
        // 保持系统域即是正确行为，而删键与「什么都不做」在这一格等价，故直接早退。
        let _ = STARTUP_OUTCOME.set(format!("跳过：config.json 读不到（{}）", path.display()));
        return;
    };
    let code = apple_language_from_raw(Some(&raw));
    write_apple_languages(code);
    // 只报两种结局：写入 / 删键。**刻意没有「失败」一档** —— 类型化绑定这条路上没有可失败的
    // 步骤（见 `write_apple_languages` 文档末节），留一条永不触发的失败文案只会误导读日志的人。
    // 真出问题时的痕迹在上面两个早退分支（$HOME 取不到 / config.json 读不到）。
    let _ = STARTUP_OUTCOME.set(match code {
        Some(c) => format!("AppleLanguages=[{c}]（读自 {}）", path.display()),
        None => format!(
            "AppleLanguages 已删除，回落系统语言（language=auto 或未识别；读自 {}）",
            path.display()
        ),
    });
}

/// 把 `AppleLanguages` 写进**应用自己的** UserDefaults 域；`None` = 删键（回落系统域）。
///
/// `standardUserDefaults` 的 `setObject:forKey:` 落点就是应用域（`NSArgumentDomain` 之下、
/// `NSGlobalDomain` 之上），正是 `defaults write <bundle-id> AppleLanguages -array <code>`
/// 的代码等价形式 —— 也正因如此，真机判据可以直接用 `defaults read` 对（见交付清单）。
///
/// # 为什么用 `objc2-foundation` 的类型化绑定而不是裸 `objc2` 消息
///
/// 裸 `msg_send!` 能零依赖声明做完同样的事（`objc2` 本就是直接依赖），但它**只做类型检查、
/// 不校验选择器存在**：方法名拼错照样编译通过，到 macOS 上才变成运行期的「悄悄什么都没做」。
/// 而本函数的**全部**失败形态本来就是「悄悄没生效」（写不进去 = 原生对话框继续按系统语言，
/// 没有任何用户可见信号），再叠一层同方向的不可见风险，等于把唯一能提前发现问题的机会也交出去。
///
/// 换成类型化绑定后，这类拼错变成**编译期错误**——即在三平台的 Rust 门上硬失败，而不是等真机。
/// 这个收益是实测出来的，不是推断：对 `x86_64-apple-darwin` 交叉 `cargo check`，把
/// `standardUserDefaults` / `setObject:forKey:` / `removeObjectForKey:` 各拼错一次，
/// 裸 `msg_send!` 版三次**全部 exit 0**（编译通过），本版三次**全部 exit 101**。
///
/// 代价只是一条依赖**声明**：`objc2-foundation` 早已由 `objc2-app-kit` 拉进依赖图，
/// Cargo.lock 无任何 `[[package]]` 新增（详见 `Cargo.toml` 该行上方注释）。
///
/// # 没有失败腿，所以不返回失败
///
/// 类型化绑定这条路上**没有一步是可失败的**：`standardUserDefaults()` 直接给
/// `Retained<NSUserDefaults>`，`NSString::from_str` / `NSArray::from_slice` 都是全函数
/// （裸消息版那些 `AnyClass::get(...)?` 短路，本质是在补类型系统缺的那一层）。
/// 故本函数返回 `()`：**编造一条永远不会触发的失败分支，比没有分支更糟**——
/// 调用方那句「失败：…」日志会成为永不出现的死文案，读日志的人却会以为它在守着什么。
#[cfg(target_os = "macos")]
fn write_apple_languages(code: Option<&str>) {
    use objc2_foundation::{NSArray, NSString, NSUserDefaults};

    let defaults = NSUserDefaults::standardUserDefaults();
    // macOS 用户语言偏好键（应用域里这条盖过 `NSGlobalDomain` 的同名键）。
    let key = NSString::from_str("AppleLanguages");
    match code {
        Some(c) => {
            let value = NSString::from_str(c);
            // AppleLanguages 的契约是有序数组（首项即首选语言）。Polaris 只写一项：
            // 写多项等于替用户编造回落顺序，而回落本就该由系统域接手。
            let array = NSArray::from_slice(&[&*value]);
            // SAFETY: `setObject:forKey:` 的不安全性仅来自「值的类型对不对得上」。
            // `AppleLanguages` 的类型契约是 CFArray<CFString>，此处传的正是 `NSArray<NSString>`。
            unsafe { defaults.setObject_forKey(Some(&array), &key) };
        }
        None => defaults.removeObjectForKey(&key),
    }
}

/// 把 [`apply_process_language`] 的结果补报进应用日志。在 `logging::init` **之后**调用。
#[cfg(target_os = "macos")]
pub fn log_startup_outcome() {
    match STARTUP_OUTCOME.get() {
        Some(outcome) => log::info!("原生对话框语言对账：{outcome}"),
        // 调用点被挪到 `apply_process_language` 之前 / 那句被删掉都会打到这里。
        None => log::warn!(
            "原生对话框语言对账：本次未执行（调用点丢失？）—— 原生对话框将继续按系统语言"
        ),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 非 macOS：空实现（保留调用点，让 `main()` 的顺序守卫三平台同源）
// ────────────────────────────────────────────────────────────────────────────

/// 非 macOS 上无对应机制（WebKitGTK 读环境 locale、WebView2 读系统区域设置），空函数。
#[cfg(not(target_os = "macos"))]
pub fn apply_process_language(_identifier: &str) {}

/// 非 macOS 上无事可报，空函数。
#[cfg(not(target_os = "macos"))]
pub fn log_startup_outcome() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_language_maps_to_a_macos_code() {
        for (key, code) in APPLE_LANGUAGES {
            assert_eq!(
                apple_language_for(key),
                Some(*code),
                "{key} 的映射查不回来 —— 表里有重复键或查找逻辑坏了"
            );
        }
    }

    /// 存量 `fa-IR` 必须与前端 `migrateLanguageCode` 同口径迁移到 `fa`。
    /// 不迁移的症状：波斯语老用户的原生对话框恒回落系统语言，而应用内一切正常 —— 查不出来。
    #[test]
    fn legacy_fa_ir_migrates_to_fa() {
        assert_eq!(apple_language_for("fa-IR"), Some("fa"));
    }

    /// `auto` 与任何不认识的码都必须落到 `None`（= 删键 = 跟随系统），
    /// **不得**回落成某个具体语言 —— 那会让「跟随系统」永久失效。
    #[test]
    fn auto_and_unknown_choices_fall_back_to_system() {
        for choice in ["auto", "", "system", "de-DE", "zh", "ZH-CN"] {
            assert_eq!(
                apple_language_for(choice),
                None,
                "{choice} 不该映射出具体语言"
            );
        }
    }

    /// 从原始 JSON 文本取值的正常腿。
    #[test]
    fn reads_language_out_of_raw_config() {
        assert_eq!(
            apple_language_from_raw(Some(r#"{"language":"ru","uiTheme":"dark"}"#)),
            Some("ru")
        );
        assert_eq!(
            apple_language_from_raw(Some(r#"{"language":" zh-TW "}"#)),
            Some("zh-Hant"),
            "两侧空白应被 trim（sanitize 侧同样 trim，两边口径要一致）"
        );
    }

    /// 容错腿：这一堆输入里任何一个 panic 都会让 App 起不来（调用点在 `main()` 第一屏）。
    #[test]
    fn malformed_config_never_panics_and_falls_back_to_system() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("{"),
            Some("[]"),
            Some("null"),
            Some(r#"{"language":null}"#),
            Some(r#"{"language":42}"#),
            Some(r#"{"language":["ru"]}"#),
            Some(r#"{"uiTheme":"dark"}"#),
            Some(r#"{"language":"auto"}"#),
        ] {
            assert_eq!(
                apple_language_from_raw(raw),
                None,
                "脏输入 {raw:?} 应回落跟随系统"
            );
        }
    }

    /// 路径拼装：入参是 **`$HOME`**（不是 app support 目录），`Library/Application Support`
    /// 由本函数补，叶名取自 `uninstall::CONFIG_DIR_LEAF`，identifier 来自 tauri.conf.json。
    ///
    /// 拼错的形态是**恒读空 → 恒跟随系统 → 与本改动落地前完全一样**，没有任何报错，
    /// 所以这条断言必须写死整条绝对路径而不是只查末几段（本测试第一版就把 app support 目录
    /// 当 `$HOME` 喂进来，拼出了 `.../Application Support/Library/Application Support/...`，
    /// 只查叶名的话这条就绿了）。
    #[test]
    fn user_config_path_is_home_then_app_support_then_identifier_then_polaris_leaf() {
        let p = user_config_path(Path::new("/Users/x"), "com.polaris.app");
        assert_eq!(
            p,
            Path::new("/Users/x/Library/Application Support/com.polaris.app/polaris/config.json")
        );
    }
}
