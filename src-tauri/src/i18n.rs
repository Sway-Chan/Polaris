//! Rust 侧**用户可见文案**的 i18n —— 原生文件对话框、Linux 托盘原生菜单 / 托盘 tooltip、
//! 提权引导消息框、应用菜单（⌘Q）、系统通知。
//!
//! ══ 补的是什么缺口 ══
//!
//! 产品出 5 语种（`ui/src/domain/language.ts` 的 `SUPPORTED_LANGUAGES`），前端有 i18next +
//! 5 份 locale JSON，**Rust 侧一个字都没有**：文件对话框标题 / 过滤器名、提权引导消息框
//! （标题 / 正文 / 按钮）、应用菜单的「退出 Polaris」一律硬编码中文；托盘原生菜单与 tooltip
//! 稍好一档，但只有 zh/en **二态**（旧 `TrayLang`）。于是俄语用户看到的是俄语按钮 + 中文标题
//! （macOS `AppleLanguages` 对账让 `NSOpenPanel` 的按钮/边栏跟随了语言，标题却是我们自己传的）。
//! Linux 上更要紧：AppIndicator 不递送可靠点击事件时，**原生菜单是主交互面**。
//!
//! ══ 为什么必须有 Rust 侧体系（而不是「前端把译好的文案当参数传下来」）══
//!
//! 「前端传参」对**由前端 invoke 发起**的那几个对话框确实成立（导出备份 / 导入订阅 / 选内核
//! 都是 `#[tauri::command]`，全仓无 Rust 内部调用方）。但它对下面两类**结构性覆盖不到**：
//!
//!  1. **托盘原生菜单 / tooltip**：菜单在 Rust 侧由 `build_tray_menu` 构建，由
//!     `reconcile_tray_menu` 的 30s 自愈轮询驱动，**没有任何前端调用方**；
//!  2. **提权引导消息框**（`runtime/proxy.rs::prompt_helper_gate`）：它挂在
//!     `run_helper_gate` ← `start_inner` ← `ProxyRuntime::start`，而起核的发起方包含
//!     `runtime/startup_tasks.rs::spawn_auto_connect`（启动 2s 后**Rust 自己**调
//!     `commands::proxy_start`）与托盘原生菜单的 `tray_toggle` —— 两条都没有前端在场，
//!     前端手上那份 i18next 递不进来。
//!
//! 两类都要弹给用户看，故 Rust 侧的文案表**无法回避**。既然它无论如何要存在，剩下的
//! 5 个前端发起的对话框也走它 —— 反过来给 5 个 command 加 title/filter 参数 + 改 5 处前端调用点
//! 的改动面更大，且会长出**两套**文案真值源（同一个「所有文件」在两处各写一遍，迟早分叉）。
//!
//! ══ 文案住哪：复用 `ui/src/i18n/locales/auxiliary/`，新增 `native.*` 命名空间 ══
//!
//! `locales/auxiliary/` 这个分区的定义就是「**主窗 i18next 不加载**、由别的消费方按命名空间具名导入」
//! （见 `ui/src/i18n/auxiliary.ts`）。此前的消费方是托盘浮层与更新弹窗两个辅助 webview，Rust 进程是
//! **第三个**这样的消费方，形状完全吻合：
//!
//!  · **不用主分区** `locales/*.json`：那是 i18next 的全量包，en-US 单份 159 kB、五份合计
//!    ~870 kB。`include_str!` 主分区 = 把 870 kB 常量烧进二进制，只为取二十来条串。
//!    aux 分区五份合计 ~14 kB。
//!  · **不另起 `locales/native/`**：那要把 `locale-parity.test.ts` 的键集/形态/棘轮门、
//!    `text-fit.test.ts` 的语料装配各复制一份。aux 分区已被这两道门覆盖（parity 把 aux
//!    合进主分区一起判，缺译会转红），新命名空间**零门禁成本**地继承。
//!  · **托盘那批键直接复用 `tray.*`，不另起一份**：`tray.rs` 的旧注释写着「文案与浮层
//!    `TrayMenu.tsx` 的 `TAKEOVERS` 表逐字一致（同一概念在两个入口不得措辞分叉）」——
//!    那是一条**靠人守**的散文约束。改成读同一个键之后，它变成结构性的：两个入口取的是
//!    同一个字符串，想分叉都分叉不了。
//!  · 辅助窗的 bundle **不会**因此变大：`labels.ts` / `update-popup/main.ts` 走的是
//!    `import { tray } from '.../aux/en-US.json'` 具名导入，Rollup 只保留那一棵子树，
//!    `native` 被 tree-shake 掉（这正是 `aux.ts` 选具名导入的理由，实测 3.2 kB）。
//!
//! ══ `include_str!` 而不是运行期读文件 ══
//!
//! 五份 JSON 在**编译期**嵌进二进制（下方 [`Lang::catalog_json`]）。
//!
//!  · **不能放 `resources/`**：该目录被 `.gitignore` 整体排除（`/resources/*`）、由
//!    `scripts/` 在构建期 fetch 填充。翻译是源码不是下载物，放进去等于「翻译不入库」。
//!  · **不做运行期读盘**：那要多一条「文件没跟着装进包」的失效腿，而它的症状是**静默**的
//!    （读不到 → 回落键名 → 用户看到 `native.allFiles`），且三平台各有一套资源目录布局。
//!  · **改 JSON 会不会不重编**（最容易埋的雷：静默用旧文案）：不会。`include_str!` 读到的文件
//!    由 rustc 写进 dep-info，cargo 据此判定重编 —— 与 `build.rs` 的 `cargo:rerun-if-changed`
//!    是两套机制，后者管的是 build script 自己的输入，`include_str!` 用不上它，**故本模块
//!    不需要也不应该往 `build.rs` 加 rerun-if-changed**。
//!
//!    这是实测的，不是推断（2026-07-31，本工作树）：
//!      · `target/debug/polaris.d` 里逐行列出了五份 `ui/src/i18n/locales/auxiliary/*.json`；
//!      · 无改动连跑两次 `cargo build -p polaris` ⇒ `Compiling polaris` 出现 **0** 次；
//!      · `touch ui/src/i18n/locales/auxiliary/ru.json` 后再跑 ⇒ 出现 **1** 次；其后再跑又回 **0** 次。
//!    复现命令记在 handoff 里。哪天换构建方式（自定义 build script 生成 locale、或改成运行期读盘），
//!    重跑这三步即可判定这条论断是否仍成立。
//!  · **路径跨出 crate**（`../../ui/...`）：这是本模块唯一的跨界依赖，且是**单向、只读、
//!    编译期**的。`crates/` 下的子 crate 不受影响 —— 它们没有 tauri 依赖，也就没有任何
//!    用户可见的原生表面（对话框 / 菜单 / 通知全在 `src-tauri/` 内，已实测），
//!    没有复用本模块的需求。
//!
//! ══ 语言从哪来 ══
//!
//! [`app_lang`] 读 `config.language`（`ConfigManager` 缓存投影），`auto` / 空 / 不认识的码
//! 回落系统 locale（`tauri_plugin_os::locale()`）。这与前端 `i18n/index.ts`（「语言选择真值源
//! = config.language」）和 `app_language.rs`（macOS `AppleLanguages` 对账）**同一个真值源**。
//! 解析规则是 `ui/src/domain/language.ts` 的 `resolveEffectiveLanguage` + `migrateLanguageCode`
//! 的逐条移植（见 [`resolve_effective`]），三处口径不得分叉。
//!
//! ══ 回落链：`当前语种 → en-US → 键名`，**刻意不回落 zh** ══
//!
//! 1. `en-US` 是前端 `DEFAULT_LANGUAGE`、i18next 的 `fallbackLng`、也是
//!    `locale-parity.test.ts` 的 `REFERENCE`（zh-CN/zh-TW 对它严格全等，ru/fa 走精确棘轮）
//!    ⇒ 它是**结构上唯一被保证完整**的一份。
//! 2. 回落 zh 会让「某个键漏译」的症状变成**波斯语用户看到中文**——那正是本模块要消灭的形态，
//!    而且它比英文更难被用户/我们辨认成 bug。
//! 3. `en-US` 也缺 → 返回**键名本身**（`native.allFiles` 这样的裸串），显式坏相、不静默显示
//!    别的语言。这一档不该发生：本文件的键覆盖门（`every_declared_key_resolves_in_all_five_locales`）
//!    与 `locale-parity.test.ts` 会先转红。口径与 `ui/src/i18n/auxiliary.ts` 逐条相同。
//!
//! ══ ⚠️ 本门射程之外的用户可见出口（**显式待办**，2026-08-17 登记）══
//!
//! **门绿 ≠ 全仓没有硬编码文案。** 下方 `tests::SINKS` 只枚举了 10 个**原生**出口
//! （对话框 / 菜单 / tooltip / 通知）。用户可见的文案还有第二条路：Rust 侧构造中文串 → 经 IPC
//! 递给前端 → 前端**原样显示**。这条路一个字都不在 `no_hardcoded_cjk_in_user_facing_native_sinks`
//! 的射程里，故那条门恒绿也说明不了这两个出口的情况。
//!
//! **六**个已知出口（**刻意不加进 `SINKS`**：全仓命中量级在数百条，加进去会让门当场大面积
//! 转红，属独立批次的工作量。此处如实登记，不假装不存在）。前两条随 U1 复审登记，后四条是
//! 2026-08-17 全仓复核补上的 —— 原文写「两个已知出口」是**错的**，那不是保守估计而是漏检：
//!
//! | # | 出口 | 载荷 | 显示点（前端原样展示） |
//! |---|---|---|---|
//! | 1 | `commands/updater.rs::ProgressStage::Failed(msg)`（经 `emit_progress` 广播） | `update:progress` 的 `error` | `SettingsUpdate.tsx` 的 `setErrMsg(patch.error …)`；同一真值经 `popup_state_for` 镜像进 mini 更新弹窗 error 态 |
//!
//! **#1 的改造量级（2026-08-17 复审校准，别照抄成「只需换个载荷」）**：`Failed(&str)` →
//! `Failed { code, params }` 要动 **变体定义 1 处 + 构造点 9 处**（每处都要挑 code 与 params）
//! **+ `stage_facts` 返回元组的第三格 + `popup_state_for` 的 `error: &str` 形参**；且那 9 处的
//! `msg` **同时**喂给 `ApiResponse::err(msg)`（本表 #2 那条通道），两条出口共用同一个串，
//! 拆不掉 —— 要么两条一起改，要么在那 9 处各造两份文案。
//!
//! 净判断仍是「W5 之后更好修」，理由是三条而不是「量小」：受影响调用点 13 → 9（进度事件的
//! 产地从 13 个平行实参调用点收敛成一个枚举 + 一个 `emit` 闭包）；改载荷会在那 9 处**编译红**
//! 而不是静默沿用旧字符串；「哪一格是用户可见文案」由类型在**一处**声明（`Failed` 的那个
//! 字段），不再散在每个调用点的第 4 个实参里。
//! | 2 | `response::ApiResponse::err(msg)` / `err_with_code(msg, _)` | 响应**信封**的 `msg` | `ipc-client.ts` 抛 `IpcError(msg)`，各调用点多以 `e.message` 直落 toast / 错误行 |
//! | 3 | `commands/subscription.rs::update_failure(...)`（`:330`；文案来自 `:813`「订阅不存在」/ `:821`「订阅缺少 URL」/ `:861` / `:917` 等 9 处调用点） | `event:subscriptionUpdateProgress` 终态帧的 `error` | `SubInfoBar.tsx:372` 的 `data-tip={failure.error \|\| t('nodes.subRefreshFail')}` |
//! | 4 | `runtime/subscription_scheduler.rs:302`（兜底串 `"订阅更新失败"`，其余透传 #3 的文案） | `event:subscriptionAutoUpdate` 的 `error` | `App.tsx:679` 的 `toast.error(t('nodes.subAutoUpdateFail'), data.error)` —— 第 2 参数是 toast 描述行 |
//! | 5 | **`ApiResponse::ok` 的载荷内嵌 `error` 字段**（≠ #2 的信封通道）：`commands/proxy.rs:522` / `:532`、`commands/misc.rs:1035` / `:1036` | `{ ok:false, error }` / `{ error }` | `components/dialogs/node-spec.ts:1050` 的 `message: r.error ?? ''`（NodeDialog 探测结果条）；misc 那两条落进诊断导出正文 |
//! | 6 | `runtime/proxy.rs:752` 的 `emit_proxy_error(message, error_code)` 的 `message` | `event:proxyError` / `event:proxyLifecycle` 的 `message` | `domain/proxy-error-text.ts` **三段式的第 2 段**（`STARTUP_FAILED` / `ROOT_ORPHAN_BLOCKED` 等无键码走这一段）→ `PendingChangesBar` 的「应用失败：{{reason}}」 |
//!
//! **#6 已有前端侧对账文档与门**，不必在此重复：判据写在 `ui/src/domain/proxy-error-text.ts`
//! 的头注（含「为什么不把 Rust message 也翻译了」的取舍），覆盖门是
//! `ui/src/contracts/proxy-error-key-coverage.test.ts`（读 Rust 源码对账 `pub mod code`）。
//! 本表只留指针。
//!
//! 后果是具体的：俄语/波斯语用户在更新失败时看到的是**俄语按钮 + 整段中文正文**
//! （如「更新包校验失败（可能被截断或篡改）: expected …」）。
//!
//! # 两个**做对了**的反例（修法不用另行发明，仓内已有两处可抄）
//!
//!  · `commands/proxy.rs:286` 的 indeterminate 腿同样构造了一句中文，但前端**刻意不采信**
//!    （`node-spec.ts:1038` 明写「`indeterminate` 腿不采信后端 `error` 文案」），改由前端出键 ——
//!    同一个文件里紧挨着的两条腿，一条对一条错，说明这不是「做不到」。
//!  · `SubDialog.tsx:142` 走 `r.errorKind` → `SUBSCRIPTION_ERROR_I18N_KEY` 查表，
//!    与 `proxy-error-text.ts` 的 `errorCode` 三段式同形：**跨进程只传分类、不传文案**。
//!
//! 为什么不在本批一起改：改造要动各通道的**载荷契约**（`error: string` → 结构化
//! `{code, params}`，否则前端无从翻译）、六个显示点、以及五份 locale。那是一次跨 Rust/TS
//! 契约的改动，与「把一条文案搬进 JSON」不是同一量级。规模估算见交接单（U3 批次）。
//!
//! 本节的存在本身就是判据：谁要把这些 sink 加进 `SINKS`，先读这一节再决定批次边界；
//! 谁要给上面任一通道**新加**一条中文文案，本表就是它欠下的账。

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;
use tauri::{AppHandle, Manager};

// ────────────────────────────────────────────────────────────────────────────
// 语言
// ────────────────────────────────────────────────────────────────────────────

/// 界面语言 —— 逐项等于 `ui/src/domain/language.ts` 的 `SUPPORTED_LANGUAGES`。
///
/// 顺序即 [`SUPPORTED`] 的顺序，无语义。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Lang {
    ZhCN,
    ZhTW,
    EnUS,
    Ru,
    Fa,
}

/// 全部受支持语言。`ui/src/domain/language.ts::SUPPORTED_LANGUAGES` 的 Rust 侧对应物
/// （**两侧由 `ui/src/contracts/rust-i18n-coverage.test.ts` 对账**）。
pub const SUPPORTED: [Lang; 5] = [Lang::ZhCN, Lang::ZhTW, Lang::EnUS, Lang::Ru, Lang::Fa];

/// 回落语言。= 前端 `DEFAULT_LANGUAGE`（理由见模块文档「回落链」一节）。
pub const DEFAULT: Lang = Lang::EnUS;

impl Lang {
    /// i18n 资源键（= locale 文件名，= `config.language` 的取值）。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Lang::ZhCN => "zh-CN",
            Lang::ZhTW => "zh-TW",
            Lang::EnUS => "en-US",
            Lang::Ru => "ru",
            Lang::Fa => "fa",
        }
    }

    /// 该语种的 aux 分区 JSON 原文（编译期嵌入，理由见模块文档）。
    ///
    /// 路径写死而不是 `concat!` 拼 —— `include_str!` 的实参必须是字面量才能让 rustc 把它
    /// 记进 dep-info（改 JSON 才会重编）。
    const fn catalog_json(self) -> &'static str {
        match self {
            Lang::ZhCN => include_str!("../../ui/src/i18n/locales/auxiliary/zh-CN.json"),
            Lang::ZhTW => include_str!("../../ui/src/i18n/locales/auxiliary/zh-TW.json"),
            Lang::EnUS => include_str!("../../ui/src/i18n/locales/auxiliary/en-US.json"),
            Lang::Ru => include_str!("../../ui/src/i18n/locales/auxiliary/ru.json"),
            Lang::Fa => include_str!("../../ui/src/i18n/locales/auxiliary/fa.json"),
        }
    }
}

/// 旧语言码迁移：`fa-IR` → `fa`（其余原样）。与 `domain/language.ts::migrateLanguageCode`
/// 同口径 —— 不迁移的话波斯语存量用户在这条腿上恒回落系统语言。
fn migrate_code(code: &str) -> &str {
    if code == "fa-IR" {
        "fa"
    } else {
        code
    }
}

/// 单个 BCP47 码 → 受支持语言；无匹配 `None`。
///
/// 移植 `domain/language.ts::matchSupported`：按主语言子标签 + 脚本/地区消歧。
/// 繁体判据 = `Hant` 脚本**或** tw/hk/mo 地区段（原文正则 `/(^|[-_])(tw|hk|mo)([-_]|$)/`
/// 在此实现为「按 `-`/`_` 切段后整段相等」——同一语义，且不为一个正则给 src-tauri 加 `regex` 依赖）。
fn match_supported(raw: &str) -> Option<Lang> {
    let l = raw.trim().to_ascii_lowercase();
    if l.is_empty() {
        return None;
    }
    let mut segs = l.split(['-', '_']);
    let primary = segs.next().unwrap_or_default();
    match primary {
        "zh" => {
            let hant =
                l.contains("hant") || l.split(['-', '_']).any(|s| matches!(s, "tw" | "hk" | "mo"));
            Some(if hant { Lang::ZhTW } else { Lang::ZhCN })
        }
        "fa" => Some(Lang::Fa),
        "ru" => Some(Lang::Ru),
        "en" => Some(Lang::EnUS),
        _ => None,
    }
}

/// OS 偏好语言有序列表 → 受支持语言；命中即止，全不匹配 → [`DEFAULT`]。
/// 移植 `domain/language.ts::resolveAutoLanguage`。
fn resolve_auto(preferred: &[String]) -> Lang {
    preferred
        .iter()
        .find_map(|p| match_supported(p))
        .unwrap_or(DEFAULT)
}

/// 解析有效界面语言。移植 `domain/language.ts::resolveEffectiveLanguage`。
///
/// - `choice` 为 `auto` / 空 / **不在受支持集合里**（含 `de-DE`、大小写不符的 `ZH-CN`）→ 按系统偏好解析；
/// - `choice` 是受支持的具体码（`fa-IR` 先迁移成 `fa`）→ 用它。
///
/// ⚠️ 与旧 `resolve_tray_lang` 的**行为差异**（刻意）：旧实现把「显式的非中文码」一律判英文
/// （`de-DE` → En），新实现按前端口径回落系统偏好（德语系统 + `de-DE` 选择 → 系统里若有俄语
/// 就取俄语）。分叉的那一版没有理由，只是二态解析的副产物。
#[must_use]
pub fn resolve_effective(choice: &str, system: &[String]) -> Lang {
    let c = migrate_code(choice.trim());
    if c.is_empty() || c == "auto" {
        return resolve_auto(system);
    }
    SUPPORTED
        .into_iter()
        .find(|l| l.code() == c)
        .unwrap_or_else(|| resolve_auto(system))
}

/// 本进程当前应显示的语言：`config.language` → `auto`/空/未知码回落系统 locale。
///
/// 走 [`ConfigManager::with_current`](crate::runtime::ConfigManager::with_current) **投影**而非
/// `current()`：本函数在托盘两个汇流点（tooltip 语言 + 菜单语言）里各调一次，而那两个汇流点挂着
/// **30s 自愈轮询**（`TRAY_ICON_POLL`）—— 用 `current()` 则核不动、用户不动，进程也会每 30s
/// 因为一个语言标签把整份配置（含 200 节点级 `servers`）深拷贝两遍。闭包内只取字段，不回调任何子系统。
///
/// ⚠️ 调用方**不得**把本函数塞进另一个 `with_current` 闭包里：闭包内持着 `ConfigManager` 的读锁，
/// 而本函数自己还要再读一次，递归读在有写者排队时永久阻塞。`main.rs` 的
/// `tray_reconcile_reads_config_by_projection_not_full_clone` 在源码层面钉着这两条。
#[must_use]
pub fn app_lang(app: &AppHandle) -> Lang {
    let choice = app
        .try_state::<crate::runtime::AppRuntime>()
        .and_then(|rt| {
            rt.config()
                .with_current(|c| {
                    c.get("language")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .ok()
                .flatten()
        })
        .unwrap_or_default();
    let sys = tauri_plugin_os::locale()
        .map(|l| vec![l])
        .unwrap_or_default();
    resolve_effective(&choice, &sys)
}

// ────────────────────────────────────────────────────────────────────────────
// 文案表
// ────────────────────────────────────────────────────────────────────────────

/// 一个语种的扁平文案表（`"tray.connect"` → 译文）。
type Catalog = HashMap<String, String>;

/// 把 aux JSON（两层：命名空间 → 键 → 串）压成扁平表。
///
/// **解析失败即 panic，不回落空表**：入参是 `include_str!` 嵌进来的**编译期常量**，不是用户
/// 可写的运行期文件 —— 它坏掉是我们自己提交了破 JSON，不是用户输入异常。回落空表的后果是
/// 每一条文案都退化成裸键名（`native.allFiles` 显在对话框标题上），比早失败更糟且更难归因。
/// 这与 `app_language.rs` 「读 config.json 绝不 panic」并不矛盾：那边读的是**用户可写的磁盘文件**。
/// 本函数被下方键覆盖门对五个语种各跑一遍，破 JSON 进不了 CI。
fn flatten(json: &str, lang: Lang) -> Catalog {
    let root: Value = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("locale {} 不是合法 JSON：{e}", lang.code()));
    let obj = root
        .as_object()
        .unwrap_or_else(|| panic!("locale {} 顶层不是对象", lang.code()));
    let mut out = Catalog::new();
    for (ns, sub) in obj {
        let leaves = sub
            .as_object()
            .unwrap_or_else(|| panic!("locale {}：命名空间 {ns} 不是对象", lang.code()));
        for (k, v) in leaves {
            let s = v
                .as_str()
                .unwrap_or_else(|| panic!("locale {}：{ns}.{k} 不是字符串", lang.code()));
            out.insert(format!("{ns}.{k}"), s.to_owned());
        }
    }
    out
}

/// 五份文案表（首次取用时解析一次）。
static CATALOGS: LazyLock<HashMap<&'static str, Catalog>> = LazyLock::new(|| {
    SUPPORTED
        .into_iter()
        .map(|l| (l.code(), flatten(l.catalog_json(), l)))
        .collect()
});

/// 取某语种的文案表。语种恒在表内（由 [`SUPPORTED`] 构造），故 `expect` 不可达。
fn catalog(lang: Lang) -> &'static Catalog {
    CATALOGS
        .get(lang.code())
        .expect("CATALOGS 由 SUPPORTED 构造，不可能缺项")
}

/// 取文案。回落链 `lang → en-US → 键名`（理由见模块文档「回落链」一节）。
///
/// `key` 用 [`key`] 模块里的常量，别写裸串 —— 键覆盖门只认那个模块里声明的常量。
#[must_use]
pub fn t(lang: Lang, key: &str) -> String {
    catalog(lang)
        .get(key)
        .or_else(|| catalog(DEFAULT).get(key))
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

// ────────────────────────────────────────────────────────────────────────────
// 键
// ────────────────────────────────────────────────────────────────────────────

/// Rust 侧消费的全部 i18n 键。
///
/// 两类：
///  · `tray.*` —— 与托盘浮层 `TrayMenu.tsx` **共用**的键（同一概念在原生菜单与浮层不得措辞分叉，
///    见模块文档）。这些键归浮层所有，`text-fit.test.ts` 的槽位穷尽性断言也盯着它们，
///    **不要往 `tray` 命名空间加只有 Rust 用的键**（浮层没有消费点 ⇒ 那道门会红，且它红得对）。
///  · `native.*` —— webview 里**没有对应表面**的文案（文件对话框、提权引导、应用菜单、
///    托盘检查更新的系统通知）。新增 Rust 侧文案往这里加。
///
/// 本模块内每一条 `pub const` 都被 `every_declared_key_resolves_in_all_five_locales` 逐个查表
/// 验证（五语种齐备），反向由 `every_native_key_in_locale_is_declared_here` 查死键。
pub mod key {
    // ── 托盘原生菜单（与浮层共用）──
    /// 「连接代理」。
    pub const TRAY_CONNECT: &str = "tray.connect";
    /// 「断开代理」。
    pub const TRAY_DISCONNECT: &str = "tray.disconnect";
    /// 接管方式子菜单标题。
    pub const TRAY_GROUP_TAKEOVER: &str = "tray.groupTakeover";
    /// 分流策略子菜单标题。
    pub const TRAY_GROUP_MODE: &str = "tray.groupMode";
    /// 「打开设置」。
    pub const TRAY_OPEN_SETTINGS: &str = "tray.openSettings";
    /// 「检查更新」。
    pub const TRAY_CHECK_UPDATE: &str = "tray.checkUpdate";
    /// 「打开主窗口」。
    pub const TRAY_OPEN_MAIN: &str = "tray.openMain";
    /// 「退出 Polaris」（托盘菜单 + 应用菜单 ⌘Q 共用）。
    pub const TRAY_QUIT: &str = "tray.quit";

    // ── 接管方式三档（`config.proxyModeType` 值域）──
    /// 系统代理。
    pub const TRAY_TAKEOVER_SYSTEM_PROXY: &str = "tray.takeoverSystemProxy";
    /// TUN 模式。
    pub const TRAY_TAKEOVER_TUN: &str = "tray.takeoverTun";
    /// 仅本机。
    pub const TRAY_TAKEOVER_MANUAL: &str = "tray.takeoverManual";

    // ── 分流策略三档（`config.proxyMode` 值域）──
    /// 智能分流。
    pub const TRAY_MODE_SMART: &str = "tray.modeSmart";
    /// 全局。
    pub const TRAY_MODE_GLOBAL: &str = "tray.modeGlobal";
    /// 直连。
    pub const TRAY_MODE_DIRECT: &str = "tray.modeDirect";

    // ── tooltip 四态 ──
    /// 已连接。
    pub const TRAY_STATUS_CONNECTED: &str = "tray.statusConnected";
    /// 连接中。
    pub const TRAY_STATUS_CONNECTING: &str = "tray.statusConnecting";
    /// 连接异常。
    pub const TRAY_STATUS_ERROR: &str = "tray.statusError";
    /// 未连接。
    pub const TRAY_STATUS_DISCONNECTED: &str = "tray.statusDisconnected";

    // ── 检查更新结果（浮层 notice 行与原生通知共用）──
    /// 已是最新版本。
    pub const TRAY_UP_TO_DATE: &str = "tray.upToDate";
    /// 检查更新失败。
    pub const TRAY_UPDATE_CHECK_FAILED: &str = "tray.updateCheckFailed";

    // ── 原生文件对话框 ──
    /// 导出配置备份：保存框标题。
    pub const NATIVE_BACKUP_EXPORT_TITLE: &str = "native.backupExportTitle";
    /// 导入配置备份：打开框标题。
    pub const NATIVE_BACKUP_IMPORT_TITLE: &str = "native.backupImportTitle";
    /// `.polaris-backup` 过滤器显示名。
    pub const NATIVE_BACKUP_FILE_TYPE: &str = "native.backupFileType";
    /// `.json` 过滤器显示名。
    pub const NATIVE_JSON_FILE_TYPE: &str = "native.jsonFileType";
    /// 导出诊断报告：保存框标题。
    pub const NATIVE_DIAGNOSTIC_EXPORT_TITLE: &str = "native.diagnosticExportTitle";
    /// 导出日志：保存框标题。
    pub const NATIVE_LOGS_EXPORT_TITLE: &str = "native.logsExportTitle";
    /// 本地导入配置：打开框标题。
    pub const NATIVE_CONFIG_PICK_TITLE: &str = "native.configPickTitle";
    /// 配置文件过滤器显示名。
    pub const NATIVE_CONFIG_FILE_TYPE: &str = "native.configFileType";
    /// 手动替换内核：打开框标题。
    pub const NATIVE_CORE_PICK_TITLE: &str = "native.corePickTitle";
    /// 「所有文件」过滤器显示名。
    pub const NATIVE_ALL_FILES: &str = "native.allFiles";
    /// Taildrop 取件：保存框标题。
    pub const NATIVE_TAILDROP_SAVE_TITLE: &str = "native.taildropSaveTitle";

    // ── 提权引导消息框 ──
    /// 未装 helper：标题。
    pub const NATIVE_HELPER_INSTALL_TITLE: &str = "native.helperInstallTitle";
    /// 未装 helper：正文。
    pub const NATIVE_HELPER_INSTALL_BODY: &str = "native.helperInstallBody";
    /// 未装 helper：确认按钮。
    pub const NATIVE_HELPER_INSTALL_CONFIRM: &str = "native.helperInstallConfirm";
    /// 已装但不可用：标题。
    pub const NATIVE_HELPER_REPAIR_TITLE: &str = "native.helperRepairTitle";
    /// 已装但不可用：正文。
    pub const NATIVE_HELPER_REPAIR_BODY: &str = "native.helperRepairBody";
    /// 已装但不可用：确认按钮。
    pub const NATIVE_HELPER_REPAIR_CONFIRM: &str = "native.helperRepairConfirm";
    /// 取消按钮。
    pub const NATIVE_CANCEL: &str = "native.cancel";

    // ── 托盘「检查更新」的系统通知 ──
    /// 通知标题。
    pub const NATIVE_UPDATE_NOTIFY_TITLE: &str = "native.updateNotifyTitle";
    /// `hasUpdate` 为真却缺 version（后端契约破损）。
    pub const NATIVE_UPDATE_INFO_INCOMPLETE: &str = "native.updateInfoIncomplete";
    /// 提醒窗弹不出来。
    pub const NATIVE_UPDATE_POPUP_FAILED: &str = "native.updatePopupFailed";
    /// 兜底错误串。
    pub const NATIVE_UNKNOWN_ERROR: &str = "native.unknownError";
}

#[cfg(test)]
mod tests {
    use super::*;

    // ════════════════════════════════════════════════════════════════════════
    // 语言解析（口径必须与 ui/src/domain/language.ts 一致）
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn explicit_choice_wins_over_system() {
        let sys = vec!["zh-CN".to_owned()];
        assert_eq!(resolve_effective("ru", &sys), Lang::Ru);
        assert_eq!(resolve_effective("fa", &sys), Lang::Fa);
        assert_eq!(resolve_effective("en-US", &sys), Lang::EnUS);
        assert_eq!(
            resolve_effective(" zh-TW ", &sys),
            Lang::ZhTW,
            "两侧空白应 trim"
        );
    }

    /// 存量 `fa-IR` 必须与前端 `migrateLanguageCode` 同口径迁移；不迁移的症状是波斯语老用户
    /// 的原生对话框恒回落系统语言，而应用内一切正常 —— 查不出来。
    #[test]
    fn legacy_fa_ir_migrates_to_fa() {
        assert_eq!(resolve_effective("fa-IR", &[]), Lang::Fa);
    }

    #[test]
    fn auto_and_unknown_fall_back_to_system_preference() {
        for choice in ["auto", "", "   ", "de-DE", "ZH-CN"] {
            assert_eq!(
                resolve_effective(choice, &["ru-RU".to_owned()]),
                Lang::Ru,
                "{choice} 应回落系统偏好"
            );
        }
        // 系统偏好也认不出 → DEFAULT（en-US），**不是**中文。
        assert_eq!(resolve_effective("auto", &["de-DE".to_owned()]), Lang::EnUS);
        assert_eq!(resolve_effective("auto", &[]), Lang::EnUS);
    }

    /// 中文的简繁消歧：`Hant` 脚本或 tw/hk/mo 地区 → 繁体，其余（含裸 `zh` / `Hans` / `sg`）→ 简体。
    /// 弄反的症状是全体繁体用户看到简体（或反之），而门若只测 `zh-CN`/`zh-TW` 两个规范码测不出来。
    #[test]
    fn chinese_script_and_region_disambiguation_matches_frontend() {
        for (sys, want) in [
            ("zh-Hant", Lang::ZhTW),
            ("zh-TW", Lang::ZhTW),
            ("zh-Hant-HK", Lang::ZhTW),
            ("zh_MO", Lang::ZhTW),
            ("zh-Hans-CN", Lang::ZhCN),
            ("zh", Lang::ZhCN),
            ("zh-SG", Lang::ZhCN),
        ] {
            assert_eq!(
                resolve_effective("auto", &[sys.to_owned()]),
                want,
                "系统 locale {sys} 解析错了"
            );
        }
    }

    /// 系统偏好是**有序**列表，命中即止（前端 `resolveAutoLanguage` 同款）。
    #[test]
    fn system_preference_list_is_ordered_first_match_wins() {
        assert_eq!(
            resolve_effective(
                "auto",
                &["de-DE".to_owned(), "ru".to_owned(), "fa".to_owned()]
            ),
            Lang::Ru
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // 文案表 + 回落链
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn five_catalogs_are_embedded_and_non_trivial() {
        assert_eq!(
            CATALOGS.len(),
            5,
            "语种数不对 —— SUPPORTED 与 include_str! 分叉了"
        );
        for l in SUPPORTED {
            let c = catalog(l);
            assert!(
                c.len() >= 50,
                "{} 只解析出 {} 条文案 —— aux JSON 结构变了？",
                l.code(),
                c.len()
            );
        }
    }

    /// 回落链 `lang → en-US → 键名`。第三档必须是**键名本身**，不得是中文
    /// （回落中文 = 波斯语用户在缺译时看到中文，正是本模块要消灭的形态）。
    #[test]
    fn fallback_chain_is_lang_then_en_then_key_name() {
        assert_eq!(t(Lang::Ru, key::NATIVE_CANCEL), "Отмена");
        // 不存在的键：五个语种都必须原样回键名。
        for l in SUPPORTED {
            assert_eq!(t(l, "native.__no_such_key__"), "native.__no_such_key__");
        }
    }

    /// 每一条声明过的键都必须在**五个**语种里各有真译文（不靠回落）。
    ///
    /// 这是「加了 Rust 文案却只补了中文」的直接判据。反向（locale 里有、Rust 没消费）见下一条。
    #[test]
    fn every_declared_key_resolves_in_all_five_locales() {
        let keys = declared_keys();
        assert!(
            keys.len() >= 30,
            "只解析出 {} 个键常量 —— `mod key` 的写法变了？门已失去判据",
            keys.len()
        );
        let mut missing = Vec::new();
        for k in &keys {
            for l in SUPPORTED {
                match catalog(l).get(k.as_str()) {
                    Some(v) if !v.trim().is_empty() => {}
                    Some(_) => missing.push(format!("  {} 的 {k} 是空串", l.code())),
                    None => missing.push(format!("  {} 缺 {k}", l.code())),
                }
            }
        }
        assert!(
            missing.is_empty(),
            "Rust 侧消费的键没有五语种齐备（补进 ui/src/i18n/locales/auxiliary/*.json）：\n{}",
            missing.join("\n")
        );
    }

    /// 反向对差：`native.*` 命名空间里的每一条都必须被 `mod key` 声明。
    ///
    /// `native.*` 的**唯一**消费方是 Rust（前端不加载它，`i18n-coverage.test.ts` 的 G4 还禁止
    /// TS 侧消费）⇒ 没有 Rust 常量指向它 = 死翻译，会一直被翻译者维护却没人显示。
    /// `tray.*` 不在本条射程内：它归浮层所有，Rust 只是共用其中一部分。
    #[test]
    fn every_native_key_in_locale_is_declared_here() {
        let declared = declared_keys();
        let dead: Vec<_> = catalog(Lang::EnUS)
            .keys()
            .filter(|k| k.starts_with("native.") && !declared.contains(*k))
            .cloned()
            .collect();
        assert!(
            dead.is_empty(),
            "aux 的 native.* 里有没人消费的死键（删掉，或在 `mod key` 里登记消费点）：{dead:?}"
        );
    }

    /// 从本文件源码抽出 `mod key` 里声明的全部键值。
    ///
    /// 用源码扫描而不是另维护一张 `ALL: &[&str]` 表：两张表必然漂移，而漂移的方向恰好是
    /// 「新键忘了登记 ⇒ 门看不见它 ⇒ 门恒绿」。
    fn declared_keys() -> Vec<String> {
        let src = include_str!("i18n.rs");
        let start = src
            .find("pub mod key {")
            .expect("锚点消失：`pub mod key {` —— 门已失去判据");
        let body = &src[start..];
        let end = body
            .find("\n}\n")
            .expect("锚点消失：`mod key` 的收尾 —— 门已失去判据");
        let mut out = Vec::new();
        for line in body[..end].lines() {
            let l = line.trim();
            if !l.starts_with("pub const ") {
                continue;
            }
            let Some(rest) = l.split_once(" = \"") else {
                continue;
            };
            let Some((v, _)) = rest.1.split_once('"') else {
                continue;
            };
            out.push(v.to_owned());
        }
        out
    }

    // ════════════════════════════════════════════════════════════════════════
    // 门：Rust 侧用户可见文案不得裸写中文
    // ════════════════════════════════════════════════════════════════════════
    //
    // # 为什么是「按 sink 收口」而不是「全仓禁裸中文字符串」
    //
    // `src-tauri/src` 里有 **3538** 条含中文的字符串字面量（实测）：日志、单测断言消息、
    // 诊断报告正文、panic 文案。它们**不是**缺陷 —— 本仓的写作约定就是中文注释 + 中文日志。
    // 一刀切禁掉等于要求把全仓日志改英文，那是另一件事。真正的缺陷面是「**送到用户眼前的
    // 原生表面**」：文件对话框、消息框、菜单项、tooltip、系统通知、窗口标题。这些出口是
    // **可枚举的**（下方 `SINKS`），且新增一个出口必然要写出这些 API 名之一。
    //
    // # 注释里的中文怎么排除（本门最大的假阳性源）
    //
    // 不靠正则「跳过以 `//` 开头的行」——那对块注释、行尾注释、`///` 文档注释里带引号的例子
    // 全部失效。改成**词法切分**（[`tokenize`]）：单行/块注释（Rust 块注释可嵌套）、普通串、
    // 原始串（`r#"…"#`）、字节/C 串、字符字面量（并与生命周期 `'a` 区分）各按语法走一遍，
    // 产出两样东西：
    //   ① **代码骨架**：与原文**等长**，注释字节与字符串**内容**字节一律换成空格（保留换行与引号）。
    //      sink 模式只在骨架上匹配 ⇒ 注释里写 `.set_title("导出…")` 当例子不会触发；
    //      括号配对也在骨架上做 ⇒ 串里的括号不会把配对带跑偏。
    //   ② **字面量表**：每条串在原文里的字节区间 + 是否含 CJK。
    // 一条 CJK 字面量落在某个 sink 调用的实参括号内 ⇒ 转红。
    //
    // # 读不到就抛
    //
    // 扫不到文件、某个 sink 模式在全仓一次都没匹配上（= 被改名 / 被删干净）⇒ **panic**，
    // 不是静默跳过。「扫到 0 处于是 0 条断言全绿」是假门。

    /// 用户可见的原生表面 —— 每条模式的实参里都不得出现裸中文字面量。
    ///
    /// 收录判据 = 「这个调用的字符串实参会**原样显示给用户**」。刻意**不收** `.body(`：
    /// 全仓 5 处里 3 处是 HTTP 请求体（`icon_cache.rs` / `runtime/http.rs`），语义完全不同；
    /// 通知那一处由唯一漏斗 `notify_user(` 覆盖。
    const SINKS: &[&str] = &[
        ".set_title(",                           // 文件对话框标题 / 窗口标题
        ".add_filter(",                          // 文件对话框过滤器显示名
        ".set_file_name(",                       // 文件对话框默认文件名
        ".message(",                             // 消息框正文
        ".title(",                               // 消息框标题 / 通知标题 / 建窗标题
        ".set_tooltip(",                         // 托盘 tooltip
        "MessageDialogButtons::OkCancelCustom(", // 消息框自定义按钮
        "MenuItem::with_id(",                    // 菜单项（同时命中 CheckMenuItem::with_id）
        "Submenu::with_items(",                  // 子菜单标题
        "notify_user(",                          // 系统通知（本仓唯一漏斗）
    ];

    /// 一条字符串字面量在原文里的位置与成分。
    #[derive(Debug)]
    struct Lit {
        /// 内容起始字节偏移（不含起始引号）。
        start: usize,
        /// 内容结束字节偏移（不含结束引号）。
        end: usize,
        has_cjk: bool,
    }

    /// CJK 统一表意文字 + 扩展 A + 兼容 + CJK 标点 + 全角。与
    /// `ui/src/i18n/i18n-coverage.test.ts` 的 `CJK` 同口径（假名/谚文不算，本仓不出这两个语种）。
    fn is_cjk(c: char) -> bool {
        matches!(c,
            '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{3000}'..='\u{303F}'
            | '\u{FF00}'..='\u{FFEF}')
    }

    /// 词法切分：返回（与原文等长的代码骨架, 字符串字面量表）。详见上方段落。
    fn tokenize(src: &str) -> (String, Vec<Lit>) {
        let b = src.as_bytes();
        let n = b.len();
        let mut sk = b.to_vec();
        let mut lits = Vec::new();
        // 抹掉 [s, e)：换行保留（骨架的行号要与原文对得上），其余换空格（保持等长）。
        let blank = |sk: &mut Vec<u8>, s: usize, e: usize| {
            for byte in &mut sk[s..e] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        };
        let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut i = 0usize;
        while i < n {
            // 行注释（含 /// 与 //!）
            if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
                let e = src[i..].find('\n').map_or(n, |p| i + p);
                blank(&mut sk, i, e);
                i = e;
                continue;
            }
            // 块注释（Rust 可嵌套）
            if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                let s = i;
                let mut depth = 1usize;
                i += 2;
                while i < n && depth > 0 {
                    if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                assert_eq!(
                    depth, 0,
                    "块注释未闭合（偏移 {s}）—— 词法器读不下去，不静默跳过"
                );
                blank(&mut sk, s, i);
                continue;
            }
            // 原始串 r"…" / r#"…"# / br#"…"# / cr#"…"#
            let prefix_ok = i == 0 || !is_ident(b[i - 1]);
            if prefix_ok {
                let mut j = i;
                if b[j] == b'b' || b[j] == b'c' {
                    j += 1;
                }
                if j < n && b[j] == b'r' {
                    let mut h = j + 1;
                    while h < n && b[h] == b'#' {
                        h += 1;
                    }
                    if h < n && b[h] == b'"' {
                        let hashes = h - (j + 1);
                        let term = format!("\"{}", "#".repeat(hashes));
                        let cs = h + 1;
                        let ce = src[cs..]
                            .find(&term)
                            .map(|p| cs + p)
                            .unwrap_or_else(|| panic!("原始串未闭合（偏移 {i}）"));
                        lits.push(Lit {
                            start: cs,
                            end: ce,
                            has_cjk: src[cs..ce].chars().any(is_cjk),
                        });
                        blank(&mut sk, cs, ce);
                        i = ce + term.len();
                        continue;
                    }
                }
            }
            // 普通串 "…" / b"…" / c"…"
            let str_start = if b[i] == b'"' {
                Some(i + 1)
            } else if prefix_ok && (b[i] == b'b' || b[i] == b'c') && i + 1 < n && b[i + 1] == b'"' {
                Some(i + 2)
            } else {
                None
            };
            if let Some(cs) = str_start {
                let mut j = cs;
                loop {
                    assert!(j < n, "字符串未闭合（偏移 {i}）");
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                lits.push(Lit {
                    start: cs,
                    end: j,
                    has_cjk: src[cs..j].chars().any(is_cjk),
                });
                blank(&mut sk, cs, j);
                i = j + 1;
                continue;
            }
            // 字符字面量 vs 生命周期：`'a` / `'static` 不是字面量，`'x'` / `'\n'` / `'中'` 是。
            if b[i] == b'\'' {
                let rest = &src[i + 1..];
                let lit_len = if let Some(after_backslash) = rest.strip_prefix('\\') {
                    // 转义形：`'\n'` / `'\''` / `'\u{4e2d}'` —— 长度 = 反斜杠 + 转义体 + 收尾引号。
                    after_backslash.find('\'').map(|p| p + 2)
                } else {
                    rest.chars()
                        .next()
                        .map(char::len_utf8)
                        .filter(|&l| rest.as_bytes().get(l) == Some(&b'\''))
                };
                if let Some(l) = lit_len {
                    blank(&mut sk, i + 1, i + 1 + l);
                    i += l + 2;
                    continue;
                }
                i += 1;
                continue;
            }
            i += 1;
        }
        (
            String::from_utf8(sk).expect("骨架只把注释/串内容换成 ASCII 空格，必然仍是合法 UTF-8"),
            lits,
        )
    }

    /// 一条命中：sink 名 + 行号 + 文案。
    #[derive(Debug, PartialEq, Eq)]
    struct Finding {
        line: usize,
        sink: &'static str,
        text: String,
    }

    /// 扫一份源码：落在 sink 实参括号内的裸 CJK 字面量。
    ///
    /// 返回值第二项是「本文件里每个 sink 各命中几处调用」，供全仓自检（模式失效即全 0）。
    fn scan(src: &str) -> (Vec<Finding>, HashMap<&'static str, usize>) {
        let (skeleton, lits) = tokenize(src);
        let sk = skeleton.as_bytes();
        let mut hits = Vec::new();
        let mut counts: HashMap<&'static str, usize> = SINKS.iter().map(|s| (*s, 0)).collect();
        for sink in SINKS {
            let mut from = 0usize;
            while let Some(p) = skeleton[from..].find(sink) {
                let at = from + p;
                from = at + sink.len();
                *counts.get_mut(sink).expect("counts 由 SINKS 构造") += 1;
                // 实参区间 = 模式末尾那个 '(' 到与之配对的 ')'。骨架里串内容已抹空 ⇒ 不会被串里的括号带跑。
                let open = at + sink.len() - 1;
                let mut depth = 0i32;
                let mut close = None;
                for (k, byte) in sk.iter().enumerate().skip(open) {
                    match byte {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(k);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let close = close.unwrap_or(sk.len());
                for l in lits
                    .iter()
                    .filter(|l| l.has_cjk && l.start > open && l.end <= close)
                {
                    hits.push(Finding {
                        line: src[..l.start].matches('\n').count() + 1,
                        sink,
                        text: src[l.start..l.end].chars().take(40).collect(),
                    });
                }
            }
        }
        (hits, counts)
    }

    /// 递归收 `src-tauri/src` 下的 `.rs`。
    fn rust_sources() -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, acc: &mut Vec<std::path::PathBuf>) {
            let rd = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("读不到目录 {}：{e}", dir.display()));
            for ent in rd {
                let p = ent.expect("目录项读取失败").path();
                if p.is_dir() {
                    walk(&p, acc);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    acc.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut acc = Vec::new();
        walk(&root, &mut acc);
        acc.sort();
        acc
    }

    /// **本门的自证**：词法器把注释里的中文排除掉、把代码里的中文抓出来。
    ///
    /// 这两条是磁盘变异测试（往真文件塞一条裸中文 ⇒ 红；塞进注释 ⇒ 不红）的自动化对应物 ——
    /// 磁盘变异是一次性的人工判据，这两条永久钉在 CI 上，防的是「哪天有人把注释剥离改坏了，
    /// 门从此对注释假阳性/对代码假阴性」。
    #[test]
    fn gate_flags_code_literals_and_ignores_comments() {
        // ① 代码里的裸中文 ⇒ 抓到
        let (bad, _) = scan("fn f() { d().set_title(\"导出备份\"); }");
        assert_eq!(bad.len(), 1, "代码里的裸中文没被抓到：{bad:?}");
        assert_eq!(bad[0].text, "导出备份");

        // ② 各种形态的注释里的中文 ⇒ 一条都不抓
        let commented = r##"
            // 行注释：`.set_title("导出备份")` 这样写就错了
            /// 文档注释：.add_filter("所有文件", &["*"])
            //! 内层文档注释：.message("需要修复提权助手")
            /* 块注释
               .set_title("导入配置备份")
               /* 嵌套块注释 .title("未知错误") */
            */
            fn f() { let s = "日志里的中文不算用户可见"; log::info!("托盘：{s}"); }
        "##;
        let (clean, _) = scan(commented);
        assert!(
            clean.is_empty(),
            "注释/日志里的中文被误判成用户文案：{clean:?}"
        );

        // ③ 原始串 / 字节串 / 转义引号 / 生命周期 都不能把词法器带跑
        let tricky = r####"
            fn f<'a>(x: &'a str) { let _ = '中'; let _ = '\''; let _ = "带\"引号\"的中文";
                let _ = r#"原始串里的 "引号" 与中文"#;
                d().set_title(r"原始串标题"); }
        "####;
        let (t3, _) = scan(tricky);
        assert_eq!(t3.len(), 1, "原始串/转义/生命周期把词法器带跑了：{t3:?}");
        assert_eq!(t3[0].text, "原始串标题");
    }

    /// 全仓门：任何 sink 的实参里都不得出现裸中文。
    #[test]
    fn no_hardcoded_cjk_in_user_facing_native_sinks() {
        let files = rust_sources();
        assert!(
            files.len() >= 30,
            "只扫到 {} 个 .rs —— 目录布局变了？门已失去判据",
            files.len()
        );
        let mut findings = Vec::new();
        let mut totals: HashMap<&'static str, usize> = SINKS.iter().map(|s| (*s, 0)).collect();
        for f in &files {
            let src = std::fs::read_to_string(f)
                .unwrap_or_else(|e| panic!("读不到 {}：{e}", f.display()));
            let (hits, counts) = scan(&src);
            for (k, v) in counts {
                *totals.get_mut(k).expect("totals 由 SINKS 构造") += v;
            }
            let rel = f.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(f);
            for h in hits {
                findings.push(format!(
                    "  {}:{} 经 `{}` 显示裸中文「{}」",
                    rel.display(),
                    h.line,
                    h.sink,
                    h.text
                ));
            }
        }
        // 自检：任何一条模式在全仓一次都没匹配上 = API 被改名/该出口被删 ⇒ 这条断言从此恒真。
        let dead: Vec<_> = totals
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(s, _)| *s)
            .collect();
        assert!(
            dead.is_empty(),
            "这些 sink 模式在全仓一处都没匹配上（被改名了？删了？）——留着等于门的这几档恒绿：{dead:?}"
        );
        assert!(
            findings.is_empty(),
            "Rust 侧的用户可见文案硬编码了中文（非中文用户会看到中文）。\
             修法：把文案加进 `ui/src/i18n/locales/auxiliary/*.json` 的 `native` 命名空间（五语种齐补），\
             在 `i18n::key` 登记常量，调用点改 `i18n::t(lang, key::X)`：\n{}",
            findings.join("\n")
        );
    }
}
