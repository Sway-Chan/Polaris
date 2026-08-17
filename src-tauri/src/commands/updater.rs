//! 更新类 command（上游 `update-handlers.ts` + `core-update-handlers.ts`）。
//!
//! # 实现状态（**逐个 command 如实标注，禁假成功**）
//!
//! | command | 状态 |
//! |---|---|
//! | `version_get_info` / `update_check` / `update_download` / `update_install` / `update_skip` / `update_open_releases` | ✅ 真实现 |
//! | `update_popup_*` | ✅ 真实现 |
//! | `core_update_check` / `core_update_run` / `core_get_version_info` / `core_rollback` / `core_replace_manual` / `core_reset_factory` / `core_update_apply_staged` / `core_update_get_auto_status` / `core_update_ack_version_change` | ✅ 真实现 |
//! | `app_uninstall_all` | ✅ 真实现（编排见 [`crate::runtime::uninstall`]；三平台可行性逐项如实标注） |
//!
//! # ⚠️ 为什么「未接线」返 `success:false` 而非 `{ok:false}`
//!
//! 本文件原先的 placeholder 返回 `ApiResponse::ok(json!({"ok": false}))` —— **信封是 `success:true`**。
//! 前端 `ipc-client.ts` 在 `success:true` 时**不 throw**、原样返回 `{ok:false}`，于是「功能根本没实现」
//! 在调用侧长得和「一次正常的业务性失败」一模一样（审计 §N4 点名的「能解析、不报 not found、
//! 但功能是空的 —— 比 not found 更隐蔽」）。
//!
//! 未接线者一律用 [`ApiResponse::err_with_code`]（`success:false` + 结构化 `code`）：前端 throw `IpcError`
//! 带 code，UI 能把「未接线」与「失败」分开呈现。**这是反伪造的底线：没实现就不能返成功信封。**
//!
//! # 📌 注释订正记录（2026-07-20，#13/#14 接线批）
//!
//! 本文件有**注释滞后于实现**的既往史（模块文档自陈「2026-07-16 旧注与 2026-07-18 早注均已过时」）。
//! 本批又订正了下面这批**与事实不符**的断言 —— 它们直接误导了后续判断，故逐条留档：
//!
//! | 旧注（**错的**） | 事实 |
//! |---|---|
//! | 「App 自更新的下载+安装/重启需 `tauri-updater`(签名 key) 未接入」 | **完全错误**。参考实现 上游 **从头到尾没用过任何 updater 框架、没有任何应用级签名密钥对**（`UpdateService.ts` 1212 行手写 fetch + 平台脚本）。Polaris 同理不引 `tauri-plugin-updater` ⇒ **不需要 minisign 密钥对**。该断言把「Tauri 官方 updater 插件的要求」错当成了「实现自更新的要求」 |
//! | 「内核更新的下载+落位需 helper 特权写受保护核目录」 | **错误**。换核落位于 `<config_dir>/core_update/`（用户可写），三平台统一 ⇒ **全程零提权**。helper 的受保护核目录是**可选 hardening**，且它本来就已完整实现 |
//! | 「本批不得触碰 `proxy.rs`：有并发 agent 在接热切换」 | 该并发批已结束。且路径逻辑已抽到 `runtime/core_paths.rs`，`proxy.rs` 只留三级优先级的一处插入 |
//! | 「备份仅由更新成功产生，而更新链路 HTTP 阻塞 → 本机永无备份可回滚」 | 更新链路已接线；备份由**任何**换核（在线/手动）产生，`hasBackup` 现读真实 `.bak` 状态 |
//! | 「`update_download` 需 HTTPS 下载 + 超时/OOM 闸/限流分类/镜像回退/idle 看门狗……该编排须只写一份」 | 那份编排**早就写好了**：[`CoreDownloader`](crate::runtime::http::CoreDownloader)（`runtime/http.rs`，含端到端单测）。stub 之所以是 stub，只因注入的是 `UnavailableDownloader` ——**接线缺口，不是实现缺口** |
//!
//! # 供应链：真伪与完整性靠什么
//!
//! | 层 | 手段 |
//! |---|---|
//! | 传输 | HTTPS（rustls）+ `safe_redirect_fetch` 逐跳 SSRF 闸 |
//! | 完整性 | Content-Length 比对（`CoreDownloader`）**＋ sha256 强校验**（GitHub release asset 的 `digest` 字段；**上游 没有这一层**，补上「镜像回退把信任面扩到 gh-proxy 运营方」的洞） |
//! | 真伪 | OS 层。**用户已拍板走 ad-hoc 签名**（不买 Developer ID / Authenticode）⇒ macOS 必须在安装脚本里清 quarantine，Windows 必须提前告知 SmartScreen 的点法。判定逻辑见 [`crate::runtime::update_install::install_advisory`] |
//!
//! **不需要任何应用级签名密钥对。**

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use polaris_net_stack::safe_redirect::{safe_redirect_fetch, SafeRedirectFetchOptions};
use polaris_updater::core_build::{ComparableVersion, CoreBuildKind};
use polaris_updater::github::{
    check_app_update, find_suitable_singbox_asset, github_releases_api_url, AppUpdateCheck,
    AssetArch, AssetPlatform, GithubRelease, APP_UPDATE_REPO, CORE_UPDATE_REPO,
};
use polaris_updater::popup::{
    PopupAction, UpdatePopupState, DONE_AUTO_CLOSE_MS, NO_UPDATE_AUTO_CLOSE_MS,
};
use polaris_updater::state::PopupPhase;
use polaris_updater::traits::{UnavailableDownloader, UpdateDownloader};
use polaris_updater::verify::verify_bytes;
use polaris_updater::version::{compare_semver, same_major_minor};
use polaris_updater::{extract_version_token, parse_asset_digest};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_shell::ShellExt;

use crate::commands::helper::with_uninstall_core_guard;
use crate::response::{ok_void, ApiResponse};
use crate::runtime::http::{app_user_agent, CoreDownloader, SystemDnsLookup, MAX_DOWNLOAD_BYTES};
use crate::runtime::uninstall::UninstallReport;
use crate::runtime::update_popup::{close_update_popup, show_update_popup};
use crate::runtime::{core_paths, core_swap, uninstall, update_install, AppRuntime};

// ── 宿主指针宽度绊线（非 64 位直接编不过）────────────────────────────────────────
//
// 本文件的写入闸全程以 `u64` 表达（`fileSize` / [`APP_UPDATE_MAX_BYTES`]），而
// [`CoreDownloader::with_max_bytes`](crate::runtime::http::CoreDownloader::with_max_bytes) 吃
// `usize` ⇒ 中间必有一次 `u64 → usize` 换算。在 <64 位宿主上装不下的闸值只能退到 `usize::MAX`，
// 也就是**把闸悄悄变成「不设闸」**：一个报 100 GiB 的 `fileSize` 会一路写到 ENOSPC，
// 而这恰恰是 [`APP_UPDATE_MAX_BYTES`] 自陈要防的那件事。
//
// 这条降级腿从未被测过（Polaris 三平台均为 64 位构建，CI 的交叉编译矩阵也只有 64 位目标），
// 与其留一条**静默**失效的闸，不如让非 64 位目标当场编不过 —— 失效面从「用户的盘被写满」
// 降到「构建期一条明确的错误」。
//
// 将来真要支持 32 位：不是放宽本绊线，而是把这些闸改成**全程 u64 比较、不经 usize**
// （即 `with_max_bytes` 也收 `u64`，读侧比较在 u64 上做）。
#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "polaris 的 App 更新写入闸以 u64 表达，非 64 位宿主上 `u64 → usize` 会退化成 usize::MAX（闸形同不设）。\
     支持 32 位前须先把 http.rs 的体积闸改成全程 u64 比较。"
);

/// 单次 GitHub releases 响应体上限（16 MiB；= 上游 `MAX_GITHUB_JSON_BYTES`：被劫持/WAF 回灌 GB 级
/// 响应会撞堆，流式超限即中断）。
const MAX_GITHUB_JSON_BYTES: usize = 16 * 1024 * 1024;

/// GitHub API 拉取**逐跳**超时（连接 + 读取，每跳各算一次）；= 上游 `fetchReleases` 的 15s 兜底
/// （防连接被静默吞永不 settle）。**注意它不是请求级上限** —— 见 [`CORE_CHECK_TOTAL_TIMEOUT_MS`]。
const GITHUB_FETCH_TIMEOUT_MS: u64 = 15_000;

/// 内核更新检查的**请求级总超时**（契约要求的 20s 整体兜底）。
///
/// 逐跳的 [`GITHUB_FETCH_TIMEOUT_MS`] 管不住多跳叠加（`safe_redirect_fetch` 最多 5 跳 ⇒ 最坏 90s），
/// 故在 [`core_update_check`] 外面再包一层整体 `timeout`。
const CORE_CHECK_TOTAL_TIMEOUT_MS: u64 = 20_000;

/// 结构化错误码：下载后端不可用（仅在 [`UnavailableDownloader`] 真被注入时才可能出现；
/// 生产注入的是 [`CoreDownloader`]，故此码现只作为 trait 契约的映射目标保留）。
const CODE_HTTP_UNAVAILABLE: &str = UnavailableDownloader::CODE;
// `CODE_CORE_SWAP_UNWIRED`（"CORE_SWAP_NOT_WIRED"）随 `app_uninstall_all` 接线一并删除：
// 它是本文件最后一个「未接线」错误码，留着就是个没有生产调用方的死常量。
/// 结构化错误码：无可回滚的备份。
const CODE_NO_BACKUP: &str = "NO_CORE_BACKUP";
/// 结构化错误码：活核为第三方 fork，在线更新被硬闸拦下。
const CODE_FORK_BLOCKED: &str = "CORE_FORK_BLOCKED";
/// 结构化错误码：核基目录未注入（`init_base_dir` 未跑；理论上只在异常启动路径出现）。
const CODE_CORE_DIR_UNAVAILABLE: &str = "CORE_DIR_UNAVAILABLE";
/// 结构化错误码：检查更新**整体超时**（≠ 网络失败：重试大概率还是超时，UI 应引导配置加速）。
const CODE_CHECK_TIMEOUT: &str = "UPDATE_CHECK_TIMEOUT";

/// 一帧 App 更新进度**及其随行事实**。
///
/// # 为什么是枚举，而不是 `(status, percentage, path, bytes, …)` 一串平行形参
///
/// `update:progress` 走 [`crate::events::broadcast`] fan-out 给**所有**窗口，于是「别的窗口发起的
/// 下载」（启动腿 `startup_tasks::spawn_auto_download`、弹窗腿 `update_popup_action`）同样会把
/// **设置页**推进 downloading/downloaded/error。而在本类型出现之前，事件只搬「状态」、不搬状态
/// 所依赖的数据，设置页只能拿本页上一次检查的结果去描述别人刚下的那个包 —— 后果是三条已核实的
/// 缺陷：「重启并安装」按钮首行 `if (!downloadedPath) return` 恒早退（哑键）、「重试」按钮首行
/// `if (!updateInfo) return` 恒早退（哑键）、卡片上的版本号与体积写的是另一个版本。
///
/// 修法是让**态与其随行事实同行**，并且由**类型**保证同行：`Downloaded` 不给落位路径就构造不
/// 出来，`Downloading` 不给已收字节就构造不出来。平行形参表达不了这条 —— `("downloaded", 100,
/// None, None)` 照样编译通过，只能另加一道源码级门去数，而那种门是后加的、可被新写法绕开。
///
/// # ⚠️ 类型挡不住的三样（如实登记，2026-08-17 复审补）
///
/// 「必填」只保证**字段在**，不保证**值可用**。以下三条今天在生产上都不可达，方向也安全，
/// 但别把本类型读成「构造得出来就一定对」：
///
///  1. `Downloaded { path: Path::new("") }` —— 空串照样是合法的 `&Path`，而前端
///     `if (!downloadedPath) return` 对空串同样早退 ⇒ 哑键原样复发。生产的两个构造点传的都是
///     `&dest`（`dir.join(&file_name)`，`file_name` 经 `unwrap_or_else` 兜底非空），够不着。
///     要挡得住得上 newtype（`NonEmptyPath`），代价与收益不成比例。
///  2. `Downloading { percentage: 200 }` —— [`stage_facts`] 原样透传。生产只有两个来源：字面量
///     `0`，与 [`progress_percent`] 的 `clamp(1, 99)`。同上，挡它要 newtype。
///  3. **给已有变体加字段**曾经会被 `..` 静默吃掉（三处解构都写 `{ .., }` ⇒ 加字段三处全不红、
///     载荷里不出现、跨语言对拍也不红）。**这一条已修**：三处解构一律写成逐字段绑定
///     （不需要的写 `_`），加字段现在是编译错误。「加变体被挡住了、加字段没有」这个不对称
///     由此消掉 —— 留这段是为了让下一个想图省事改回 `..` 的人看见代价。
#[derive(Clone, Copy)]
enum ProgressStage<'a> {
    /// 下载中：本帧的百分比 + **已收字节**（下载刚开始那一发是 `0 / 0`）。
    Downloading { percentage: u8, received: u64 },
    /// 已落位：包在盘上的路径 + 摘要是否逐字节校验过（与 `update_download` 回包的 `verified`
    /// 同一真值）。**路径是必填的**：设置页的「重启并安装」拿的就是它。
    Downloaded { path: &'a Path, verified: bool },
    /// 失败：成因文案（今天仍是硬编码中文，见 `crate::i18n` 模块文档登记的既存出口 #1）。
    Failed(&'a str),
}

/// 纯派生：一帧的 `(status, percentage, message)`。事件载荷与弹窗镜像共用同一份派生 ——
/// 两处各写一遍必然漂出「设置页说 100%、弹窗说 99%」这种同源不同话。
///
/// `percentage` 在两个终态上是**由类型定死的**：`Downloaded` 恒 100、`Failed` 恒 0，
/// 调用点没有把它写错的余地（此前是每个调用点自己传一个数）。
///
/// **不用 `..` 通配**（本函数与 [`progress_payload`] 及其行为门的解构一律逐字段写）：`..` 会把
/// 「给已有变体新加一个字段」静默吃掉 —— 三处都不红、载荷里不出现、跨语言对拍也不红。写成
/// `received: _` 之后，加字段是编译错误。
const fn stage_facts<'a>(stage: ProgressStage<'a>) -> (&'static str, u8, &'a str) {
    match stage {
        ProgressStage::Downloading {
            percentage,
            received: _,
        } => ("downloading", percentage, ""),
        ProgressStage::Downloaded {
            path: _,
            verified: _,
        } => ("downloaded", 100, ""),
        ProgressStage::Failed(msg) => ("error", 0, msg),
    }
}

/// 进度帧**不带**的清单字段（其余一律原样带过）。
///
/// # 为什么是「剥两个」而不是「留几个」
///
/// 剥除表的失效方向是**多带**（上游将来加一个大字段没被剥 ⇒ 帧胖了，性能退化，正确性零损失）；
/// 白名单的失效方向是**漏带**（消费方要的字段没在表里 ⇒ 「重试」重新变哑键、卡片又开始显示别的
/// 版本）—— 后者正是本批立项要修的那三条缺陷。判据面同样是枚举，但两者的**失效代价不对称**，
/// 故取失效安全的那一侧。
///
/// 逐条依据（均已核实，非估算）：
///  - `releaseNotes` = GitHub release body 原文，**无截断**（GitHub 单 body 上限 125 KB）。它只在
///    「已发现新版本」那一屏渲染（`SettingsUpdate.tsx` 的 `available` 态，且本就写着
///    `{updateInfo.releaseNotes && …}`），而 `available` **不可能**由进度帧进入 ——
///    `PROGRESS_CARD_RULE` 的取值里没有它，两侧状态集合由跨语言门对拍。
///  - `title` 全仓**零消费点**（前端 grep 已核：`title:` 的命中全是 i18n 对话框标题）。
///
/// 不剥的后果是具体的：一次下载最多 ~100 帧 × 所有窗口，20 KB 的 release notes ⇒ 约 2 MB 在
/// webview 主线程反序列化。**这是及时性问题，不是省内存**：进度条本身就是要「现在」到。
const PROGRESS_MANIFEST_OMITTED: [&str; 2] = ["releaseNotes", "title"];

/// 取清单在进度帧里的投影：删掉 [`PROGRESS_MANIFEST_OMITTED`]，**其余一个字段都不动**。
///
/// 非对象（理论上不可达：三条调用腿传的都是 `update_check` 的 `updateInfo`）原样返回 ——
/// 宁可把说不清的东西原样递过去，也不伪造一个空对象。
fn progress_manifest(info: &Value) -> Value {
    let Some(obj) = info.as_object() else {
        return info.clone();
    };
    let mut out = obj.clone();
    for key in PROGRESS_MANIFEST_OMITTED {
        out.remove(key);
    }
    Value::Object(out)
}

/// 纯函数：把一帧 + 它描述的那份发布清单拼成 `update:progress` 的载荷。
///
/// 形状：`{status, percentage, message, updateInfo, error?, receivedBytes?, filePath?, verified?}`
/// （与前端 `UpdateProgress` 契约逐字对齐；两侧的字段集由
/// `ui/src/contracts/update-progress-payload.test.ts` 做**双向**对拍）。
///
/// `updateInfo` 恒在：它是形参而不是可选项，故不存在「发了个态却没说是哪份包」的帧。带的是
/// [`progress_manifest`] 的投影（剥掉两个只在 `available` 那一屏渲染、且体积无上限的字段），
/// 其余字段一律原样 —— 「不丢字段」在剩下的字段上仍逐字成立。
///
/// 抽成纯函数是为了**可测**：[`emit_progress`] 持 `AppHandle`，单测构造不出 Tauri 运行时，
/// 判据留在里面就只剩源码级守卫，而那守得住「写没写那一行」，守不住「写出来的是什么」。
fn progress_payload(info: &Value, stage: ProgressStage<'_>) -> Value {
    let (status, percentage, message) = stage_facts(stage);
    let mut payload = json!({
        "status": status,
        "percentage": percentage,
        "message": message,
        // 随行事实之一：这一帧描述的是**哪一份包**。设置页据此渲染版本号 / 体积 / 预发布档次，
        // 并在 error 态拿它重试 —— 没有它，那些数字说的是本页上一次检查的另一个版本。
        "updateInfo": progress_manifest(info),
    });
    if !message.is_empty() {
        payload["error"] = Value::String(message.to_string());
    }
    // 逐字段解构（不用 `..`）：给变体加字段时这里必须显式表态，否则编译红。成因见
    // [`ProgressStage`] 文档「类型挡不住的三样」第 3 条。
    match stage {
        ProgressStage::Downloading {
            received,
            percentage: _,
        } => {
            payload["receivedBytes"] = Value::from(received);
        }
        ProgressStage::Downloaded { path, verified } => {
            payload["filePath"] = Value::String(path.to_string_lossy().into_owned());
            payload["verified"] = Value::Bool(verified);
        }
        ProgressStage::Failed(_) => {}
    }
    payload
}

/// 广播 App 更新进度（`update:progress`）。
///
/// # 同一份进度也镜像进 mini 弹窗
///
/// 弹窗与设置页看的是**同一次下载**，各自维护一份进度必然漂移。故此处是唯一产地：广播事件之后，
/// 若弹窗会话存在就把同一真值转成对应的弹窗档位推过去（见 [`popup_state_for`]）。这也是
/// `progress`/`done`/`error` 三态**唯一**的产地 —— 在此之前全仓只有 `remind`
/// （`update_popup_show`）能产出，于是弹窗里点「更新」后窗内零反馈、`PopupAction::Cancel`
/// （仅 Progress 合法）结构性不可达。
fn emit_progress(app: &AppHandle, info: &Value, stage: ProgressStage<'_>) {
    crate::events::broadcast(
        app,
        crate::events::channel::EVENT_UPDATE_PROGRESS,
        progress_payload(info, stage),
    );
    push_popup_state(app, popup_state_for(info, stage));
}

/// 纯映射：一帧下载进度 → 弹窗状态。**与 [`progress_payload`] 同参同源**。
///
/// # 为什么吃 `stage` 而不是吃压平后的 `(status, percentage, message)`
///
/// 本函数此前的形参是那三个标量 —— 于是**落位路径与版本号在那层压平里丢了**，`done` 只剩一个
/// 「完成」的状态字：既说不出下的是哪一版、落在哪儿，也让「复查回来没有可下的东西」能借用同一个
/// `UpdatePopupState::done()` 收场（弹窗照画「下载完成」+ 满格进度条，而一个字节都没下）。
/// 改吃 [`ProgressStage`] 之后，弹窗镜像与 `update:progress` 载荷读的是**同一个**类型化的帧 +
/// 同一份清单，两屏不可能各说各话；「未知 status」也不再是一种可达输入 —— 故本函数不再返
/// `Option`，弹窗态是**总**有的。
///
/// `remind` / `noupdate` 不在此映射内：前者由 `update_popup_show` 产出，后者由
/// [`update_popup_action`] 产出，都不是进度事件的产物。
///
/// **逐字段解构、不用 `..`**：给变体加字段时这里必须显式表态（同 [`progress_payload`] 的理由）。
#[must_use]
fn popup_state_for(info: &Value, stage: ProgressStage<'_>) -> UpdatePopupState {
    match stage {
        ProgressStage::Downloading {
            percentage,
            received,
        } => UpdatePopupState::progress(
            percentage,
            Some(received),
            // 分母取本帧随行清单的 `fileSize`（与设置页下载卡右半边同源）。缺失/为 0 时
            // `UpdatePopupState::progress` 会把它当「分母未知」，渲染端只显示已收量。
            info.get("fileSize").and_then(Value::as_u64),
        ),
        ProgressStage::Downloaded {
            path,
            // `verified` 不进弹窗：它已在 `update:progress` 载荷里给设置页用了，而弹窗要为它多一句
            // 五语文案才说得清「校验过 / 没校验」——那是另一件事，本批不夹带。
            verified: _,
        } => UpdatePopupState::done(
            info.get("version")
                .and_then(Value::as_str)
                .map(str::to_string),
            path.to_string_lossy(),
        ),
        ProgressStage::Failed(msg) => UpdatePopupState::error(if msg.is_empty() {
            // 既存的硬编码中文出口（见 `crate::i18n` 模块文档登记的 #1），本批不扩大也不新增。
            "更新失败"
        } else {
            msg
        }),
    }
}

/// 纯判定：当前处于 `phase` 的弹窗，该不该跟随一次进度事件。
///
/// **只有 `Progress` 跟随。** 这条闸的存在理由是「后台下载不得吃掉用户面前的提示」：
///
/// | 弹窗当前态 | 场景 | 跟随会怎样 |
/// |---|---|---|
/// | `Remind` | 启动检查到新版弹了提醒；此时 `autoDownloadUpdate` 的后台下载正在跑 | 提示被进度条顶掉，用户**再也看不到**「要不要更新」那一屏 |
/// | `Done` / `Error` | 终态，等自动关窗 / 等用户点重试 | 被一次无关下载改写成别的态 |
/// | `Progress` | 用户**刚在弹窗里点了「更新」**（该分支先手动推 `progress(0)`） | 正是要跟随的那一次 |
///
/// 于是「弹窗跟不跟随」由**用户是否亲手把它推进 progress** 决定，而不是由「碰巧有没有下载在跑」
/// 决定。`Retry`（error → 重试）同样先推 `progress(0)`，故重试链照常跟随。
#[must_use]
const fn should_mirror_to_popup(phase: PopupPhase) -> bool {
    matches!(phase, PopupPhase::Progress)
}

/// 把状态推给弹窗（弹窗未开 / 不在 progress 态 → no-op，**绝不为了推状态而建窗**）。
fn push_popup_state(app: &AppHandle, state: UpdatePopupState) {
    push_popup_state_inner(app, state, true);
}

/// 强制推送（绕过 [`should_mirror_to_popup`] 闸）：仅用于用户动作的**入口**那一发
/// —— 那一发正是把弹窗从 remind/error 推进 progress 的动作本身，闸对它恒 false。
fn force_popup_state(app: &AppHandle, state: UpdatePopupState) {
    push_popup_state_inner(app, state, false);
}

fn push_popup_state_inner(app: &AppHandle, state: UpdatePopupState, gated: bool) {
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return;
    };
    let Ok(mut slot) = rt.updater().popup().lock() else {
        return;
    };
    let Some(session) = slot.as_mut() else {
        return; // 弹窗未开（用户只在设置页操作）→ 无处可推
    };
    if gated {
        let current = session.last_state().map(|s| s.phase);
        // `None` 不可达（`open` 必然 seed），保守按「不跟随」处理。
        if !current.is_some_and(should_mirror_to_popup) {
            return;
        }
    }
    if let Err(e) = session.reuse(state) {
        // 窗可能刚被关掉；`reuse` 已先写 last_state，重放兜底不受影响 → 只记 debug。
        log::debug!("弹窗状态推送失败（窗可能已关）: {e}");
    }
}

/// 纯函数：`(已收字节, Content-Length)` → **该发的百分比**（`None` = 本 chunk 不发事件）。
///
/// 三条规则，各有其因：
///  - 无 `Content-Length` / 为 0 → `None`：算不出分母就不发（**绝不**拿已收字节凑一个假分母，
///    那会让进度条一路 100% 再跳回去）。
///  - 结果被夹在 `1..=99`：`0` 由下载**开始前**那一发独占、`100` 由 `downloaded` 独占 ——
///    否则会出现「downloaded(100%) 之后又来一发 downloading(100%)」的倒退帧。
///  - 与 `last_pct` 相同 → `None`：**逐 chunk 发就是 IPC 洪水**（几 MB 的包上千个 chunk）。
///    按整数百分比去重后，一次下载最多 99 个事件。
#[must_use]
fn progress_percent(received: u64, expected: Option<u64>, last_pct: u8) -> Option<u8> {
    let total = expected.filter(|t| *t > 0)?;
    let pct = (received.min(total) * 100 / total) as u8;
    let pct = pct.clamp(1, 99);
    (pct != last_pct).then_some(pct)
}

/// 构造下载进度回调：把 [`progress_percent`] 的判定接到 [`emit_progress`] 上。
///
/// 回调在下载 task 线程跑，故用 `Arc` + 原子游标（不是 `&mut`）。
///
/// `info` 按值收下（下载 task 的生命周期与本次调用解耦，借不了栈上的那份）——**一次下载克隆一次**，
/// 不是一帧一次。中间帧带的是回调给的 `received` 原值，**不是从百分比反推**的估算：百分比被
/// [`progress_percent`] 夹在 `1..=99` 且按整数去重，反推出来的字节数在每一帧上都是错的。
fn download_progress_emitter(
    app: &AppHandle,
    info: Value,
) -> std::sync::Arc<crate::runtime::http::DownloadProgressFn> {
    use std::sync::atomic::{AtomicU8, Ordering};
    let app = app.clone();
    let last = std::sync::Arc::new(AtomicU8::new(0));
    std::sync::Arc::new(move |received, expected| {
        let prev = last.load(Ordering::Relaxed);
        if let Some(pct) = progress_percent(received, expected, prev) {
            last.store(pct, Ordering::Relaxed);
            emit_progress(
                &app,
                &info,
                ProgressStage::Downloading {
                    percentage: pct,
                    received,
                },
            );
        }
    })
}

/// 核可写基目录（未注入 → 结构化失败，**不猜路径**）。
fn core_base_dir<T>() -> Result<&'static std::path::Path, ApiResponse<T>> {
    core_paths::base_dir().ok_or_else(|| {
        ApiResponse::err_with_code(
            "内核可写目录未初始化（应用启动期 core_paths::init_base_dir 未执行）",
            CODE_CORE_DIR_UNAVAILABLE,
        )
    })
}

/// 构造生产下载器（真实 HTTP + 用户配置的 gh 加速前缀）。
///
/// gh 前缀取自通用 config（`ghProxyPrefix`）；读不到就空串 = 不用镜像（**只回退，不改写原址优先**）。
///
/// `pub(crate)`：内核自动更新调度器（`runtime/core_update_scheduler.rs`）复用**同一个**构造入口 ——
/// 各建一份必然在 gh 前缀读法上漂移。
///
/// # `max_bytes` 为什么是形参
///
/// 三条生产腿的体积闸**语义不同**：两条内核腿把整包收进 `Vec<u8>` 再解归档 ⇒ 闸是**内存闸**，
/// 恒传 [`MAX_DOWNLOAD_BYTES`]（16 MiB，与形参化之前逐字一致）；App 安装包腿改成流式落盘后
/// 内存不随包体积长 ⇒ 16 MiB 只会把所有正常安装包拒掉，闸改由「清单声明大小 + 裕度」注入
/// （见 [`app_update_size_limit`]）。
///
/// 选形参而非「再开一个构造入口」：gh 前缀读法只该有一份。两个入口意味着有一天 App 腿
/// 读不到用户配的镜像前缀，而没有任何测试会发现。
pub(crate) fn updater_downloader(state: &AppRuntime, max_bytes: usize) -> CoreDownloader {
    let prefix = state
        .config()
        .get_value("ghProxyPrefix")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    CoreDownloader::new(state.http().clone(), tokio::runtime::Handle::current())
        .with_gh_proxy(prefix)
        .with_max_bytes(max_bytes)
}

/// GitHub releases API 的**状态码 → 成因文案**（纯函数）。`None` = 2xx，调用方继续读 body。
///
/// 抽成自由函数只为**可测**：判定夹在真 HTTP 调用之后，留在 [`fetch_releases_json`] 里就只能起真
/// 网络才断言得了，而本仓禁跑触网测试 —— 那等于这段「哪个状态码是什么成因」一条断言都没有。
fn github_status_error(status: u16, owner: &str, repo: &str) -> Option<String> {
    match status {
        200..=299 => None,
        // 403 限流单独成因（= 上游 `fetchReleases`）：它与「403 无权限」表象相同但处置相反。
        403 => Some("GitHub API 访问频率限制 (403)，请稍后再试或配置 GitHub 加速".to_string()),
        // 404 单独成因。**它不是「该仓库还没有 release」**：本端点是 releases **列表**
        // （`/repos/{owner}/{repo}/releases`，不是 `/releases/latest`），零 release 的**可见**仓库
        // 返回的是 `200 []` —— 由 `check_app_update` 判 `NoUpdate`（见 updater crate 的
        // `check_app_update_empty_releases_is_no_update`）。所以这里的 404 只有一个含义：
        // **更新源仓库对本次（未鉴权）请求不可见** —— 不存在 / 已改名 / 或是私有仓。GitHub 对私有仓
        // 一律以 404 掩盖存在性而**不是** 403，故上面的 403 分支永远兜不到这类。
        //
        // 仍返 `Err` 而不伪装成「已是最新」：把「更新通道整条不通」显示成已最新会让它永久静默，
        // 违反本函数下方 [`fetch_releases_json`] 的反伪造契约。原文案只有裸状态码，分辨不出
        // 「URL 写错」还是「仓库不可见」，带上 owner/repo 才是可行动的诊断（两条检查腿共用本函数，
        // 不带仓名连是 app 腿还是 core 腿都看不出）。
        404 => Some(format!(
            "更新源仓库不可访问 (404)：{owner}/{repo} 不存在、已改名或为私有仓库"
        )),
        s => Some(format!("GitHub API 返回错误: {s}")),
    }
}

/// 拉 GitHub releases JSON（**单点**：app 与 core 两条检查腿共用同一条 SSRF 安全路径）。
///
/// 失败语义（反伪造）：网络/SSRF/超时/非 2xx 一律 `Err(消息)`，**绝不**把失败伪装成「已是最新」。
async fn fetch_releases_json(
    state: &AppRuntime,
    owner: &str,
    repo: &str,
) -> Result<String, String> {
    let http = state.http().clone();
    let url = github_releases_api_url(owner, repo);
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: http.as_ref(),
        url: &url,
        user_agent: app_user_agent(),
        headers: Some(vec![(
            "Accept".to_string(),
            "application/vnd.github.v3+json".to_string(),
        )]),
        exempt_fake_ip: false,
        max_redirects: Some(5),
        timeout_ms: Some(GITHUB_FETCH_TIMEOUT_MS),
        max_body_bytes: Some(MAX_GITHUB_JSON_BYTES),
        lookup: &SystemDnsLookup,
    })
    .await
    .map_err(|e| format!("请求 GitHub 失败: {}", e.message))?;

    if let Some(msg) = github_status_error(resp.status, owner, repo) {
        return Err(msg);
    }
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}

/// 上游 `VERSION_GET_INFO`：版本信息（app + core 版本）。
///
/// ✅ **已接线**：app 版本取自 Tauri `package_info`；core 版本经 `UpdaterRuntime` 双读法的**展示读法**
/// （探测失败回落随包基线，= 上游 `getCoreVersion` 语义）；基线取自编译期嵌入的 `core-manifest.json`。
#[tauri::command]
pub fn version_get_info(app: AppHandle, state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    let u = state.updater();
    ApiResponse::ok(json!({
        "appVersion": app.package_info().version.to_string(),
        "coreVersion": u.read_core_version(),
        "coreBaseline": u.bundled_core_version(),
    }))
}

// ── App 更新 ──

/// Windows 便携版的**形态标记文件名**（与 `polaris.exe` 同级，由 `.github/workflows/package.yml`
/// 的 `Build Windows portable zip` 步写进 zip 根，并在同一步开包核验确实打进去了）。
const PORTABLE_MARKER: &str = "portable.marker";

/// 当前是否跑在 Windows 便携（loose）形态：**exe 同目录存在 [`PORTABLE_MARKER`]**。
///
/// # 为什么不能用 env（`PORTABLE_EXECUTABLE_DIR` / `PORTABLE_EXECUTABLE_FILE`）
///
/// 这两个变量是 **electron-builder 便携目标特有**的注入：它的便携版是一个自解压 NSIS stub，
/// 由该 stub 在拉起真 exe 之前设好它们。上游 由 electron-builder 打包，故白拿这份真值。
///
/// **Polaris 的便携版是 `Compress-Archive` 打的纯 zip，没有任何 stub**（`package.yml`
/// `Build Windows portable zip`：拷 exe + resources 后直接压缩）⇒ 这两个 env 在真机上
/// **恒不存在** ⇒ 便携用户恒被判成 installed 形态。这与选包器那半边是**同一个缺陷的两层**：
/// 判定层恒 false，选包层的便携分支即便修好也永不被走到；修任一层都不足以让便携用户拿到便携包。
///
/// 标记文件是本仓能给出的**确定性**判据：产出侧写、运行侧读，零依赖、不探注册表、不猜安装路径。
///
/// ⚠️ **已知边界（如实登记，不假装守住了）**：用户手动删掉标记文件后本函数判为 installed，
/// 该用户会重新被推安装器。方向是失败安全的那一侧（安装器能装、不会砸掉便携副本），
/// 但**没有任何自动手段能守住它** —— 兜底只有标记文件内写明的「勿删」。
///
/// **纯函数**（exe 路径注入，不读全局态）⇒ 可单测。
///
/// `pub(crate)`：`runtime/startup_tasks.rs` 的「自动下载更新」腿要在下载前预判资产形态，
/// 用的必须是**同一个**便携判据（各写一份 ⇒ 预判与实际安装形态漂移）。
pub(crate) fn is_portable_layout(exe_path: &std::path::Path) -> bool {
    exe_path
        .parent()
        .is_some_and(|dir| dir.join(PORTABLE_MARKER).is_file())
}

/// 「推」腿的 App 更新预发布口径 —— **恒为 `false`（只看正式版）**。
///
/// # 为什么是一个共享常量，而不是各腿各写一个字面量
///
/// 下面三处不是三件独立的事，而是**同一次邀请的两段**：
///  - 产出邀请：`runtime/startup_tasks.rs::spawn_auto_check_update`（启动后自动检查）与
///    `tray.rs::tray_check_update`（托盘 / 原生菜单「检查更新」）—— 它们检出的版本号被
///    [`update_popup_show`] 原样写进 mini 更新窗；
///  - 兑现邀请：本文件 [`update_popup_action`] 的 `Update` / `Retry` 分支 —— 弹窗只持版本号、
///    不持下载地址，故必须**复查一次**才拿得到 `updateInfo`，然后把那份资产真下下来。
///
/// 两段的候选集口径必须逐字相同，否则「弹窗上写的版本」与「点下去真正下载的版本」是两个东西。
/// 2026-08-17 之前复查腿写死 `Some(true)`：只要上游在最新正式版**之后**又发过一条预发布，
/// [`polaris_updater::github::check_app_update`] 内部按 `published_at` 取最新 ⇒ 用户看着
/// 「发现新版本 v1.2.0」按下「更新」，下回来的是 `v1.3.0-beta.1` —— 那个版本号从未被写给他看过。
///
/// # ⚠️ 本常量只守住不变量的**一半**（射程自曝，别读成「TOCTOU 也守住了」）
///
/// 它统一的是候选集**规则**。**内容**不归它管：邀请与兑现是两次独立的 check，中间隔着用户的
/// 思考时间，上游随时可能再发一版 —— 两端都是正式版、口径完全相同，「按 A 邀请、下到 B」照样
/// 发生。那一半由 [`update_popup_action`] 里对 `popup.version` 的**逐字对账**守（不一致就退回
/// `remind(新版本)`）。两道门缺一，不变量就只成立一半：
///  - 只有常量：口径对了，目标仍会在你思考的那几分钟里被换掉；
///  - 只有对账：对账本身会拦住换目标，但拦下来那一刻推给用户的可能是条预发布线。
///
/// # 为什么值是 `false`，而不是读一个「App 更新通道」设置
///
/// 本仓的「更新通道」只有内核那一条（`config.coreUpdateChannel`，见
/// [`core_update_channel_is_prerelease`]），**App 腿没有**对应配置键、没有 store 缺省值、
/// 设置页也没有任何开关（2026-08-17 全仓反查：`coreUpdateChannel` 5 处消费点，无 `appUpdateChannel`
/// 同类物）。没有用户可见开关支撑的 `true` 不是「更激进的策略」，是**没有任何人同意过**的策略。
///
/// App 的预发布只经**一扇门**暴露：设置页那次手动「检查更新」（`SettingsUpdate.tsx` 的
/// `updateApi.check(true)`）—— 那是用户自己按的「拉」，且结果卡上带 `isPrerelease` 标记，
/// 用户看得见自己拿到的是什么。本常量守的是另一侧：我们主动「推」到用户脸上的东西，只能是正式版。
///
/// # 交接：什么条件下本设计要推翻（不是废话，是给下一个人的判据）
///
/// 今天全仓只有一种「推」口径，所以一个共享常量足够，且比「把这次邀请用的口径记进
/// [`UpdatePopupState`]、复查时回读」便宜一个数量级（后者要加字段、穿过 [`update_popup_show`]
/// 这个 command、穿过弹窗首帧的文档注入初始态、再动弹窗侧的 TS 契约）。
///
/// **一旦给 App 加上「更新通道」设置，这个判断就翻转**：那时「推」腿会有两种口径，弹窗从产出到
/// 兑现之间用户还可能把通道改掉，共享常量再也表达不了「这次邀请当时用的是哪种口径」。届时正确的
/// 做法是走**记录口径**那条路 —— 而**不是**把本常量改成一个读配置的函数（那只是把 TOCTOU 从
/// 「两处字面量不一致」换成「同一处前后两次读到不同值」，弹窗仍会按 A 邀请、下到 B）。
///
/// **同一次改动还会引爆一处今天豁免的消费点**：`SettingsUpdate.tsx` 的 `downloaded` / `manual`
/// 两条腿用 `updateInfo?.isPrerelease` 渲染预发布徽标，而**别的窗口**发起的下载（本文件
/// [`update_popup_action`] 的「更新/重试」、`startup_tasks::spawn_auto_download`）会把设置页推进
/// `downloaded`。那份 `updateInfo` 与刚落盘的包**没有任何因果关系**，两个方向都会出错：
///
///  - **漏报**（方向安全）：`updateInfo` 为 `null`（本页压根没查过）⇒ 徽标静默缺席；
///  - **误报**（方向相反，用户可见的假话）：`updateInfo` 是**上一次检查**留下的陈旧值 ⇒ 卡片会举着
///    那次查到的版本号与预发布徽标，去描述盘上这份**别人下的**包。
///
/// 误报那一格**只止住了徽标那半**：`SettingsUpdate.tsx` 的 `downloaded` / `manual` 两条腿把徽标的
/// 资格判据收窄成 `downloadedPath && updateInfo?.isPrerelease`。
///
/// `downloadedPath` 之所以等价于「盘上这份是本页下的」，靠**两条**不变量，缺一不可：
///  1. 唯一写点是本页 `downloadUpdate` 的成功分支（外部广播那条腿从不设它）⇒ 它为真说明本页
///     下载过**某个**包；
///  2. **它与当前 `updateInfo` 描述的是同一个包** —— 这条是**非局部**的，写成状态无关的蕴含式：
///
///     > **`us === 'idle'` ⟹ `downloadedPath === null`**
///
///     （写 `downloadedPath` 的唯一点在 `downloadUpdate` 成功分支，其后 `us` 恒非 idle；而进入
///     idle 的两个入口都来自 `downloadedPath` 为 null 的态。）而 `setUpdateInfo` 只在
///     `checkUpdate()` 里执行、「检查更新」按钮只挂在 idle 那一格 ⇒ 每次写 `updateInfo` 时
///     `downloadedPath` 必为 `null`。
///
///     **刻意不按「哪些态回不到 idle」来枚举**：那样写的警告面会短于它要守的范围 —— 除
///     `downloaded` / `manual` 外，`error` 同样可能带着非空 `downloadedPath`（外部腿广播
///     `error` 只置 `us`、不清 path），而「下载失败 → 给个重新检查」比「已下载 → 重新检查」是
///     **更自然**的增补，恰好落在枚举式警告面之外。
///
/// ⚠️ **第 2 条一旦被破坏，误报立刻复活，而守它的那道门拦不住**：旧的正式包路径 + 新查到的 beta
/// 同时在手，徽标又会贴回外部腿下的那份正式包。射程是**任何**新增的 `setUs('idle')` 与**任何**
/// 新增的 `checkUpdate()` 入口（不限于某张卡），加的时候要么随入口一起清 `downloadedPath`，
/// 要么改走 W5 的正解。
///
/// **今天在 `checkUpdate()` 首行清 `downloadedPath` 是恒等 no-op**（由上面那条蕴含式直接得出：
/// 能点到「检查更新」时 path 必已为 null），故本批不清它**纯粹是不做无用功**，不是权衡取舍。
/// 只有真加了「重新检查」入口之后，清 path 才开始有代价（丢掉已下载包的安装入口）—— 届时正确
/// 做法仍是随入口一起清、或走 W5，而**不是**以「清了有代价」为由不清。
///
/// **止血刻意选「收窄断言」而不是「清空 `updateInfo`」**：后者看着更彻底，实则会让 `us === 'error'`
/// 那格的「重试」（该分支唯一按钮，直通 `downloadUpdate` 首行 `if (!updateInfo) return`）变成哑键，
/// 且要靠一面认领旗去分辨事件归属 —— 而事件与 invoke 回包**走的是两条通道**（`eval_script` vs
/// 自定义协议），本仓自己在 `SettingsUpdate.tsx` 里就登记过这条乱序。用真缺陷换假话不划算。
///
/// 于是**徽标那半**的误报没了，代价是**漏报扩大**：外部腿下的包一律不显示徽标（即使它真是预发布）。
/// 方向安全，且与下面这条合流 —— 正解都是让那两条腿把随行事实随事件带过来（W5 射程）。
///
/// **版本号 / 体积那半仍是假话，本批未修（如实登记）**：`downloaded` 态照旧渲染
/// `{updateInfo?.version}`、`downloading` 态照旧渲染版本号与 `fileSize`。设置页查到
/// `v1.3.0-beta.1` 不下载、启动腿下了 `v1.2.0` 正式包 ⇒ 卡片写「下载完成 v1.3.0-beta.1」（无徽标），
/// 盘上却是 v1.2.0。**不给版本号也挂 `downloadedPath &&`**：`downloading` 态会因此留一个空版本号，
/// 且与 W5 的正解（把随行事实带过来重建，而不是藏起来）方向冲突。这半连同上面第 2 条不变量一起
/// 交给 W5。
///
/// 漏报那一格今天不构成缺陷：能推本页的那两条腿恰恰都由本常量钉死只下正式版，**豁免依赖的就是
/// 这里的 `false`**。通道一开，那两条腿能下预发布，安装屏就会对一份预发布只字不提。
pub(crate) const PUSH_UPDATE_INCLUDE_PRERELEASE: bool = false;

/// 上游 `UPDATE_CHECK`：检查应用更新。
///
/// ✅ **检查侧已接线**：经 `runtime/http.rs` 的真实 client（`state.http()`）走**同一条**
/// SSRF 安全路径 [`safe_redirect_fetch`]（首 URL + 每跳 Location 都过 `assert_host_allowed`）拉
/// `Sway-Chan/polaris` 的 releases → `polaris_updater::check_app_update` 纯逻辑转换（过滤 prerelease /
/// 按 `published_at` 挑最新 / `compare_semver` 判新 / 平台架构资产选择 / skip 判定）→ 返
/// `{ hasUpdate, updateInfo? }`（`updateInfo` 字段与前端 `UpdateInfo` 契约逐字对齐）。
///
/// **代理策略**：本批走 `state.http()`（`no_proxy` 直连 client）。proxy 未运行时直连是合法回退
/// （C19 `resolve_update_proxy_target`：`update-in` 端口为 0 → 强制直连，自举友好）；「经 update-in 口
/// 拉取」需 `proxy.rs` 端口访问器（禁区，另批），而 GitHub API 公网可达，直连对「检查」已足够。
///
/// **失败语义**（B5 反伪造）：网络/SSRF/超时/非 2xx → `success:false`（前端 error 态显因），**绝不**
/// 把失败伪装成「已是最新」；无更新 / 无适配资产 / 已跳过 → `{ hasUpdate:false }`（诚实无更新）。
#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    include_prerelease: Option<bool>,
) -> Result<ApiResponse<Value>, ()> {
    let include_pre = include_prerelease.unwrap_or(false);

    // 平台/架构：宿主真值注入纯逻辑（非三大目标平台 → 无适配包，如实报无更新）。
    let Some(platform) = AssetPlatform::from_os(std::env::consts::OS) else {
        return Ok(ApiResponse::ok(json!({ "hasUpdate": false })));
    };
    let arch = AssetArch::from_arch(std::env::consts::ARCH);
    // 运行形态（loose vs installed）：
    //  - Linux：`APPIMAGE` 由 AppImage 运行时注入（Electron/Tauri 通用，**真值**）。
    //  - Windows：exe 同级的便携标记文件（[`is_portable_layout`]）—— **不是** electron-builder
    //    的 `PORTABLE_EXECUTABLE_DIR`，那个在本仓恒不存在（成因见 [`is_portable_layout`] 文档）。
    //  - macOS：`.app` 恒 loose，但 mac 选包不看形态（只看架构），故传什么都不影响结果。
    let loose_form = match platform {
        AssetPlatform::Windows => std::env::current_exe().is_ok_and(|exe| is_portable_layout(&exe)),
        AssetPlatform::Linux => std::env::var_os("APPIMAGE").is_some(),
        AssetPlatform::Macos => false,
    };

    // await 前取出 owned 值（当前版本 / 跳过版本），fetch 不持 State 借用（同 icon.rs 纪律）。
    let current = app.package_info().version.to_string();
    let skipped = state.updater().state().skipped_version;

    let (owner, repo) = APP_UPDATE_REPO;
    let body = match fetch_releases_json(&state, owner, repo).await {
        Ok(b) => b,
        Err(e) => return Ok(ApiResponse::err(format!("检查更新失败: {e}"))),
    };
    match check_app_update(
        &body,
        &current,
        include_pre,
        skipped.as_deref(),
        platform,
        arch,
        loose_form,
    ) {
        Ok(AppUpdateCheck::Available(info)) => {
            let info_v = serde_json::to_value(&info).unwrap_or(Value::Null);
            Ok(ApiResponse::ok(json!({
                "hasUpdate": true,
                "updateInfo": info_v,
            })))
        }
        Ok(AppUpdateCheck::NoUpdate) => Ok(ApiResponse::ok(json!({ "hasUpdate": false }))),
        Err(e) => Ok(ApiResponse::err(format!("解析 GitHub 响应失败: {e}"))),
    }
}

/// 按**目标文件路径**的下载单飞闸（同一个 dest 同时只允许一条下载腿在飞）。
///
/// # 并发是默认流程，不是异常路径
///
/// `autoDownloadUpdate` 开启时，启动腿（`runtime/startup_tasks.rs` 的 `spawn_auto_download`）在后台
/// 下载的**同时**弹 remind 窗邀请用户点「更新」（[`update_popup_action`] 的 Update/Retry 分支）。
/// 两条腿写的是同一个 `<cache>/updates/<fileName>`。没有单飞时：
///  - 两次下载各自把字节写盘再 rename（tmp 唯一化后不会撕裂 dest，但仍是一份白下的流量）；
///  - **进度事件两个产地**互相镜像：后台腿完成时发的 `downloaded(100)` 会把用户正在看的弹窗提前推到
///    Done，百分比在两个游标之间来回跳。
///
/// 故后到者在此等待；等到之后若前一位已把**同一份**包落好（sha256 相符）就直接复用，不重下。
///
/// 表只持弱引用：并发下载共同持有强引用时，同一 dest 仍命中同一把锁；最后一个下载释放后，下一次
/// 取锁会驱逐失效项。这样容量只与**当前在途目标**相关，不与长会话里见过多少更新文件名相关。
type DownloadGate = tokio::sync::Mutex<()>;
type DownloadGateMap = HashMap<PathBuf, Weak<DownloadGate>>;

fn keyed_download_gate(map: &mut DownloadGateMap, dest: &Path) -> Arc<DownloadGate> {
    map.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = map.get(dest).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(DownloadGate::new(()));
    map.insert(dest.to_path_buf(), Arc::downgrade(&gate));
    gate
}

fn download_gate(dest: &Path) -> Arc<DownloadGate> {
    static GATES: OnceLock<Mutex<DownloadGateMap>> = OnceLock::new();
    let map = GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(PoisonError::into_inner);
    keyed_download_gate(&mut guard, dest)
}

// ── App 更新包：期望摘要的来源（D1）────────────────────────────────────────────

/// 期望 sha256 的**来源**。当前**只有一级**（GitHub asset `digest`）。
///
/// # 为什么是枚举而不是「读一个字段」
///
/// 让「摘要是谁给的」成为**结果契约的一部分**（`digestSource` 回给前端 / 进日志）：出事时能追责
/// 到具体信任根，而不是只知道「校验过了」。这一条与「将来好不好扩展」无关，今天就有价值。
///
/// # ⚠️ 它**不是**「加一级只改一处」的形状（2026-08-16 订正）
///
/// 本段原写「加一级只需在 [`EXPECTED_DIGEST_SOURCES`] 里插一行」。**那句话不成立**，因为
/// [`DigestSource::field`] + `info.get(field)` 已经把来源的取法**锁死**成「`update_info` 顶层的一个
/// 字符串字段」，而 U3 要加的随包 `SHA256SUMS` 是**另一次网络下载 + 按资产名查表**：既不在
/// `update_info` 里，也不是一个纯函数拿得到的东西。U3 落地时必然要动本枚举、[`Self::field`]、
/// [`resolve_expected_digest`] 的签名，以及钉住当前表的那条单测。
///
/// **刻意不预先抽象**（YAGNI）：把 `field()` 换成 `extract(self, info, ctx)` 这类「每个来源自带
/// 取法」的形状，等于今天就为一个尚未落地、形态还会变的需求付设计税。留一条诚实的注释比留一个
/// 猜出来的抽象更有用 —— 本条注释本身就是 U3 的交接单。
///
/// **U3 已落地，但只落发布侧**（2026-08-17）：`SHA256SUMS` 现在**随每个 release 产出并有门守着**
/// （`.github/workflows/package.yml` 的 `Generate SHA256SUMS` + `scripts/verify-packaging.mjs` 的摘要门），
/// 而**消费侧刻意未接** —— 本枚举因此仍是单变体。判断与依据登记在
/// [`resolve_expected_digest`] 文档的「第 1 级已产出、消费侧未接」段，不在此重复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestSource {
    /// GitHub release asset 的 `digest` 字段（经 `parse_asset_digest` 解析后放在 `updateInfo.sha256`）。
    GithubAssetDigest,
}

impl DigestSource {
    /// `updateInfo` 里承载该来源摘要的字段名。
    const fn field(self) -> &'static str {
        match self {
            Self::GithubAssetDigest => "sha256",
        }
    }

    /// 回给前端 / 日志的来源标识（**如实标注摘要是谁给的**，便于事后追责到具体信任根）。
    const fn as_str(self) -> &'static str {
        match self {
            Self::GithubAssetDigest => "githubAssetDigest",
        }
    }
}

/// 期望摘要来源的**优先级序**（靠前者优先）。当前只有一项：U3 的 `SHA256SUMS` 虽已在发布侧产出，
/// 消费侧**经判断不接**（依据见 [`resolve_expected_digest`] 文档末节），且它也不是「在这里插一行」
/// 就能接上的（成因见 [`DigestSource`] 文档的订正段）。
const EXPECTED_DIGEST_SOURCES: [DigestSource; 1] = [DigestSource::GithubAssetDigest];

/// 一条选定的期望摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedDigest {
    hex: String,
    source: DigestSource,
}

/// 按 [`EXPECTED_DIGEST_SOURCES`] 的优先级挑第一条可用的期望摘要。
///
/// `Ok(None)` = 一条都没有：**不拒装**，降级为「Content-Length + `fileSize` 完整性校验 + 结果里
/// 如实标记未校验」（否则所有旧 release 都更新不了）。字段**整个缺失**是旧 release 的正常形态
/// （`AppUpdateInfo.sha256` 带 `skip_serializing_if = "Option::is_none"`），空串/纯空白同理。
///
/// # 两类「拿不到摘要」必须分开（本函数的全部难点）
///
///  - **本来就没有**（字段缺失 / 空串）→ `Ok(None)`，降级放行；
///  - **有，但不是个字符串**（`123` / `null` / `["…"]`）→ `Err`，**显式早退**。
///
/// 原实现两类都走 `Value::as_str` ⇒ 后者被**静默丢弃**成前者 ⇒ `verified:false` 放行 ——
/// 而本函数自己的文档写的正是「静默丢弃会把『摘要写坏了』伪装成『本来就没摘要』而放行」。
/// 一个把 `sha256` 序列化成数字的发布流程，本该当场炸掉、而不是让全体用户静默地少一道校验。
///
/// hex **格式**仍不在此校验（非法 hex 走到校验步报 `VerifyError::InvalidExpectedHash`，
/// 那里才分得出「发布方写坏了」与「包被篡改」两种文案）。
///
/// **纯函数**（吃 `Value`，无 IO）⇒ 可单测。
///
/// # 第 1 级（随包 `SHA256SUMS`）已产出，消费侧**经判断不接**（2026-08-17 登记）
///
/// 发布侧已落地：每个 release 都产出 `SHA256SUMS`，缺失/覆盖面不符/摘要不符任一即红
/// （`.github/workflows/package.yml` + `scripts/verify-packaging.mjs`）。本函数**仍然只看 asset
/// digest**，是判断结果，不是没做完。三条依据：
///
///  1. **增量价值的射程接近空集**。它只在「GitHub 没给 asset `digest`」∩「该 release 有
///     `SHA256SUMS`」这一个交集里有用。而 `SHA256SUMS` 从本轮才开始产出 ⇒ 有它的 release 都是
///     此后新发的 ⇒ 那些 release 的 asset `digest` 由 GitHub 在资产上传时算好、随
///     `update_check` 的同一次响应返回。老 release 两样都没有，接了也救不了。
///  2. **把它排在 asset digest 之前是拿弱的换强的**。两者防的都只是传输损坏与截断（都不防账号或
///     TLS 被攻破 —— 那需要签名，公钥内置于应用，属独立决策，本轮不做，也不假装 SHA 等价于它），
///     差别在**取它的那条腿会不会跟安装包一起被同一个中间人经手**：
///
///     - asset `digest` 随 `update_check` 从 `api.github.com` 的 JSON 到手，**不与安装包走同一个
///       host**，且由 GitHub 侧对已存字节算出；
///     - `SHA256SUMS` 是同一个 release 里的另一个**资产**，URL 与安装包同 host。
///
///     这不是权衡，是**结构性**的：镜像回落的判定面 `GITHUB_ASSET_HOSTS`
///     （`runtime/http.rs`）与前端的 `GH_HOSTS`（`ui/src/domain/gh-proxy.ts`）两侧独立实现、
///     **都显式排除 `api.github.com`** ⇒ API 腿结构性走不到 gh-proxy；而 `SHA256SUMS` 命中
///     `is_github_asset` ⇒ 真启用镜像时它会**跟安装包一起经同一个代理**。即恰恰在「启用代理、
///     信任面最大」的那个场景里，asset `digest` 的价值最高而 `SHA256SUMS` 的价值归零。
///
///     （措辞注：`github.rs` 的 `digest` 字段文档说「摘要与资产**同源**故不必自建密钥」，那句里的
///     「同源」指**信任根**都是 GitHub；本条说的「同 host」指**取回路径**。两个词各指一件事，
///     不要混读。）
///  3. **成本不是「插一行」**：要多一次网络往返（多一条会超时/被截断/被限流的失败腿）、要把本函数
///     从纯函数变成带 IO 的异步函数（现有这套纯函数单测随之作废）、要把清单 URL 从
///     `check_app_update` 一路穿到这里、还要动前端 `digestSource` 的字面量联合。付这些去覆盖第 1 条
///     那个近乎空的交集，是负收益。
///
/// **若将来要接**：正确形状不是「第 1 级」，而是 asset `digest` **缺失时的惰性回落** ——
/// 只在真正没摘要那条腿上多花一次往返，好路径一行不动。
///
/// **重开这个决定的触发条件**（有探测器，不靠人肉撞见）：GitHub 不再给 asset `digest`
/// —— 那一刻第 1 条的前提不成立，且客户端会静默降级成无摘要弱校验。发布流程里
/// `Verify published asset digests against SHA256SUMS` 那一步会在**发布当场**把它判红
/// （`.assets[].digest` 为空即红，release 停在草稿态），故这条触发条件是被机器盯着的，
/// 不必等用户在 UI 上看到 `verified:false`。
///
/// # Errors
///
/// 某一级来源的字段存在、但不是字符串。
fn resolve_expected_digest(info: &Value) -> Result<Option<ExpectedDigest>, String> {
    for src in EXPECTED_DIGEST_SOURCES {
        let Some(raw) = info.get(src.field()) else {
            continue; // 字段缺失 = 旧 release 的正常形态。
        };
        let Some(hex) = raw.as_str() else {
            return Err(format!(
                "更新信息里的 {} 字段不是字符串（实得 {}）——摘要写坏了，绝不当成「本来就没摘要」放行",
                src.field(),
                raw
            ));
        };
        let hex = hex.trim();
        if hex.is_empty() {
            continue; // 空串 / 纯空白 = 等同没有。
        }
        return Ok(Some(ExpectedDigest {
            hex: hex.to_string(),
            source: src,
        }));
    }
    Ok(None)
}

// ── App 更新包：写入体积闸（D4）────────────────────────────────────────────────

// `APP_UPDATE_SIZE_MARGIN`（声明值之上的 8 MiB 裕度）已删（2026-08-17）。
//
// 它自陈的唯一作用是「给清单 `fileSize` 与真实资产差一点点留条活路（改包名/重传后忘了同步清单
// 之类），免得一次人为疏忽把全体用户的更新卡死」。而**同批**新增的 [`check_declared_size`] 对
// `received != declared` 是**零容差**的等值判据 ⇒ 落在 `(declared, declared + 8 MiB]` 里的任何
// 差异都只有一条归宿：预检放行 → 白下满整包 → 等值判据拒绝。裕度已不存在任何「成功放行」的输入，
// 留着只会让人误以为清单写偏一点还能更新得动。
//
// 删掉裕度不会把「大小恰好等于声明值」的正常包卡掉，理由见 [`app_update_size_limit`] 头注
// （三处体积闸都是**严格大于**才拒）。

/// App 安装包写入闸的**绝对上限**（512 MiB）—— 不只是「清单没声明时的回落值」。
///
/// Polaris 三平台安装包在几十 MiB 量级，512 MiB 留了一个数量级以上的余量；
/// 它的职责只是「别让一个撒谎的服务端把盘写满」，不是精确判定。
///
/// # 为什么它必须同时压住**有**声明值的那条分支（原名 `APP_UPDATE_FALLBACK_MAX_BYTES`）
///
/// 原实现只在 `None`/`0` 分支用它，`Some(n)` 分支是 `n + 裕度` 且**不与任何上限取 min**：
/// `fileSize` 若是 100 GiB（发布流程写错 / GitHub 异常 / 前端构造的 `updateInfo`），闸值就是
/// 100 GiB ⇒ Content-Length 预检放行 ⇒ 一路写到 ENOSPC，用户系统盘被写满 —— 而这恰恰是本常量
/// 文档自陈要防的那件事。「回落值」这个名字本身就是那个洞的成因：它读起来只管一条分支。
const APP_UPDATE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// App 安装包的写入闸取值。
///
/// # 陷阱：`fileSize` 为 0 不等于「包是空的」
///
/// [`AppUpdateInfo::file_size`](polaris_updater::github::AppUpdateInfo::file_size) 在 GitHub asset
/// 缺 `size` 字段时按 **0** 填（`github.rs` 的 `#[serde(default)]`）。直接拿它当闸 ⇒ 闸值 0，
/// **任何**包都过不去 —— 而且失败长得像「下载超限」，没人会想到成因是清单少了个字段。
/// 故声明值有效（`> 0`）才拿它当闸，为 0 / 缺失一律回落 [`APP_UPDATE_MAX_BYTES`]。
///
/// # 闸值就等于声明值，**不再加裕度**（2026-08-17 核实后删）
///
/// 「闸 == 声明值」不会把一个大小恰好等于声明值的正常包卡掉 —— 三处体积闸全是**严格大于**才拒
/// （实测源码，非推断）：
///  - Content-Length 预检：`runtime/http.rs` `open_download_response` 的 `if n > self.max_bytes`；
///  - 内存腿读侧：`read_body_capped_with_progress` 的 `if buf.len() + chunk.len() > limit`；
///  - 流式腿读侧：`read_body_to_sink_with_progress` 的 `if received + chunk.len() > limit`。
///
/// 这条边界本身有门钉着（`runtime::http` 的 `size_limit_boundary_admits_a_body_of_exactly_the_limit`
/// 逐处拿「恰好等于闸值」的响应体撞过），故三处任一被改成 `>=` 会先在那条门上转红，而不是等到
/// 真机上「更新永远差一个字节」。
///
/// 裕度被删的成因见上方注释块：同批的 [`check_declared_size`] 是零容差等值判据，
/// `(declared, declared + 裕度]` 区间里的输入无论如何都会被拒，裕度只是让它**多下满一个整包**。
///
/// # 两条分支都封在 [`APP_UPDATE_MAX_BYTES`] 之下
///
/// 声明值是**服务端给的数**，闸不能由它单方面顶到任意高（成因见该常量文档）。
///
/// **纯函数** ⇒ 三条路径各有单测。
fn app_update_size_limit(declared: Option<u64>) -> usize {
    let limit = match declared.filter(|n| *n > 0) {
        Some(n) => n.min(APP_UPDATE_MAX_BYTES),
        None => APP_UPDATE_MAX_BYTES,
    };
    // 本回落（装不下 → usize::MAX）在受支持的宿主上**不可达**：本文件顶部的
    // `compile_error!` 已把非 64 位目标挡在编译期之外。保留 `try_from` 而不写 `as usize`，
    // 是因为 `as` 会**静默截断**成一个小值（闸比声明值还紧 ⇒ 正常包被拒）。
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// 发布清单声明的 `fileSize` 当**等值判据**（无摘要腿的硬化）。
///
/// # 为什么不能只靠 Content-Length
///
/// [`update_download`] 在无摘要时「回落 Content-Length 完整性校验」—— 但 Content-Length 是
/// **撒谎方自己给的数**，对撒谎方零约束：服务端/镜像返 `Content-Length: 1000` 且真发 1000 字节，
/// 完整性校验就过了，然后「无摘要 ⇒ 不校验」⇒ 一个 1000 字节的假包被 promote，返
/// `{success:true, verified:false}`，UI 给出安装入口。极端版 `Content-Length: 0` ⇒
/// 0 字节文件「下载成功」。
///
/// `fileSize` 来自 GitHub release 清单（与 `downloadUrl` 同一次 API 响应），镜像**改不动**它。
/// 故 `declared > 0` 时它是这条腿唯一有牙的等值判据 —— 零成本，且不影响旧 release
/// （缺 `size` 字段 ⇒ 0 ⇒ 不判，与本判据引入之前逐字一致）。
///
/// 判定委托 [`check_content_length`](crate::runtime::http::check_content_length)（同一条「实收 vs
/// 声明」判据，不另写一份）；本函数只负责 App 腿特有的 `> 0` 过滤。
///
/// # Errors
///
/// [`DownloadError::Incomplete`](polaris_updater::traits::DownloadError::Incomplete)：实收字节数
/// 与清单声明不符。
fn check_declared_size(
    received: u64,
    declared: Option<u64>,
) -> Result<(), polaris_updater::traits::DownloadError> {
    crate::runtime::http::check_content_length(received, declared.filter(|n| *n > 0))
}

/// 流式下载的**部分写入残件**（tmp）的所有权凭证：**drop 即删**，落位成功才 [`Self::disarm`]。
///
/// 形态照 [`ExtractWorkDir`]（本文件既有的 RAII 清理守卫）：构造即持有、`Drop` 里尽力清、
/// 清理失败只记日志。差别只有一个 —— 本守卫多一个「解除」出口，因为落位成功后那个文件
/// **已经变成 dest 了**，再删就是把刚下好的包删掉。
///
/// # 为什么是类型，不是「数一数清理调用」的守卫
///
/// 改造之前下载失败**不产生任何磁盘残留**（字节只在内存里）。流式之后「网络失败 / 停滞 / 超限 /
/// 摘要不符 / 落位失败」每一条早退都会留下一个写了一半的 tmp，故 U1 曾配了一条源码级守卫
/// （`every_failure_path_after_staging_discards_the_partial_tmp`），数
/// 「`ApiResponse::err(` 出现次数 == `discard_partial_download(&tmp)` 出现次数」。
///
/// **那条守卫是假的**，三条独立反例，每条都能让「漏清理」照样全绿：
///  1. 把落位失败的早退降级成「只 log 不早退」⇒ 两侧计数**同减**，仍相等；
///  2. 给一条早退配**两次**清理即可把另一条漏掉的配平；
///  3. 它**不匹配** `ApiResponse::err_with_code(`（`err` 后面是 `_` 不是 `(`）—— tmp 之后新增
///     一条带 code 且漏清理的早退，守卫全盲。
///
/// 计数守卫守的是「代码里有没有写那一行」，而不变量是「控制流离开这个作用域时残件在不在」。
/// 后者是**作用域**的性质，只有类型（`Drop`）表达得了：`?`、panic、以及任何将来新增的早退
/// 都自动被覆盖，不需要任何人记得配一行清理，也就没有「配平」可言。
struct PartialDownload {
    /// `None` = 已解除（落位成功，那个 inode 现在叫 dest 了）⇒ `Drop` 不再删。
    tmp: Option<PathBuf>,
}

impl PartialDownload {
    fn new(tmp: PathBuf) -> Self {
        Self { tmp: Some(tmp) }
    }

    /// 残件路径。
    ///
    /// [`Self::disarm`] **消费 self** ⇒ 能调到本方法的实例必然还持着路径，`expect` 不可达。
    fn path(&self) -> &Path {
        self.tmp
            .as_deref()
            .expect("PartialDownload 在 disarm 之后被使用（不可达：disarm 消费 self）")
    }

    /// 解除清理（**仅落位成功后**调）：tmp 已被 rename 成 dest，再删就是删掉成品。
    fn disarm(mut self) {
        self.tmp = None;
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        use polaris_updater::traits::UpdateFs;
        let Some(tmp) = self.tmp.take() else {
            return; // 已解除：落位成功，什么都不该删。
        };
        // `StdFs::remove_file` 把「文件不存在」视作成功，故「还没来得及建文件就失败」是 no-op。
        // 清理失败只记日志不改变结论：本守卫恒在一条**已经失败**的路径上析构，
        // 让清理错误盖掉真正的失败成因是本末倒置。
        if let Err(e) = polaris_updater::traits::StdFs.remove_file(&tmp) {
            log::warn!("清理更新包临时文件失败 {}: {e}", tmp.display());
        }
    }
}

/// 落位结论（[`land_payload`] 的产物）。**不含任何事件语义** —— 发不发 `downloaded` 由调用方按
/// 本枚举分支决定，这正是把它抽出来的全部目的。
#[derive(Debug, PartialEq, Eq)]
enum LandingOutcome {
    /// rename 成功，dest 现在是一个完整文件。
    Landed,
    /// 落位失败（dest 未动）。载荷是给用户看的成因。
    Failed(String),
}

/// tmp → dest 的落位**纯编排**（吃 `&dyn UpdateFs` ⇒ 可注入失败，可单测）。
///
/// # 为什么这一步必须是可注入的运行时判据，而不是一条文本守卫
///
/// 「`downloaded` 只在 rename 成功之后发」此前由一条**文本下标比较**守着
/// （`promote_staged(` 的位置 < 发 `ProgressStage::Downloaded` 的位置）。那条守卫表达不了
/// 「rename **成功**才执行」：把落位失败的早退降级成「只 log 不早退」，文本序**照样成立**，
/// 而运行时会在 rename 失败时广播 `downloaded(100)` 外加一个**根本不存在**的 `filePath` ——
/// 设置页据此给出一个点不开的安装入口。
///
/// 分成「判定」与「发事件」两半之后，判定这一半吃 trait、可注入 `MockFailOp::Rename`，
/// 于是「rename 失败时 dest 必须不存在、结论必须是 Failed」变成一条**运行得起来**的断言。
///
/// # rename **之前**先 `fsync` 文件（2026-08-17 新增）
///
/// 此前这一步是纯「写完 → rename」，中间没有任何 `sync`。rename 只保证**目录项**的原子替换，
/// 不保证被替换的那个 inode 的**数据**已经离开 page cache ⇒ 断电 / 内核崩溃后完全可能出现
/// 「dest 这个名字在、内容是零或半截」。而 dest 一旦存在，[`cached_download_is_reusable`]
/// （摘要对不上 → 重下，还算好）与 [`update_install`]（**直接拿去装**）都会把它当成完整包。
/// sync 失败按落位失败处理：残件由 [`PartialDownload`] 守卫在调用方的早退上清掉。
///
/// # 已知限制：**不做目录级 fsync**（如实登记，本批刻意不做）
///
/// 严格的崩溃一致性还需要在 rename **之后** `fsync` 父目录，否则崩溃后可能丢失那条目录项。
/// 本批不做，因为两种失效的后果不对称：
///  - 少了**文件级** fsync ⇒ 可能产生「名字在、内容是半截」的 dest ⇒ 装一个坏包（**这是本次修的**）；
///  - 少了**目录级** fsync ⇒ 最坏只是那次 rename 没落盘，dest 干脆不存在 ⇒ 退化成「更新包不见了」，
///    下次进 [`update_download`] 会重下一遍。**不会**产生半截包。
///
/// 即目录级 fsync 换来的是「少重下一次」，而文件级 fsync 换来的是「不装坏包」。要补目录级
/// fsync 还得在 `UpdateFs` 上再开一个 `sync_dir`（Windows 上没有可移植的目录句柄 fsync 等价物，
/// 得走 `FlushFileBuffers` on volume handle 这类平台分支）—— 半径与收益不成比例。
fn land_payload(
    fs: &dyn polaris_updater::traits::UpdateFs,
    tmp: &Path,
    dest: &Path,
) -> LandingOutcome {
    // 先刷盘再改名：顺序不可换（换了就等于没做 —— 崩溃窗口正好在 rename 之后）。
    if let Err(e) = fs.sync_file(tmp) {
        return LandingOutcome::Failed(format!("刷盘更新包失败 {}: {e}", tmp.display()));
    }
    match polaris_updater::verify::promote_staged(fs, tmp, dest) {
        Ok(()) => LandingOutcome::Landed,
        Err(e) => LandingOutcome::Failed(format!("写入更新包失败 {}: {e}", dest.display())),
    }
}

/// 孤儿 tmp 的存活阈值（24h）：**跨进程**残件早于此才删。
///
/// 为什么是 mtime 而不是 pid 存活探测：跨平台判「这个 pid 还活着吗」不可靠（Windows 无
/// `kill(0)` 等价物、pid 复用），而判错的代价是删掉另一个实例正在写的文件。mtime 阈值判错的
/// 代价只是「多留一天」。
const ORPHAN_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// tmp 命名族的中缀 —— 与 [`verify::tmp_name`](polaris_updater::verify::tmp_name) 生成的
/// `{dest}.polaris-new-{pid}-{seq}` 逐字对齐。
const ORPHAN_TMP_INFIX: &str = ".polaris-new-";

/// 纯判定：这个文件名是不是 [`verify::tmp_name`](polaris_updater::verify::tmp_name) 的产物。
///
/// 判据 = 含 [`ORPHAN_TMP_INFIX`]，且其后是 `{pid}-{seq}` 形态（两段都非空、都是纯十进制数字）。
/// **不**只看中缀：一个用户自己放的 `notes.polaris-new-draft` 不该被当成在飞下载的残件删掉。
/// 取**最后**一次中缀出现（`rsplit_once`）：`tmp_name` 是往 dest 名末尾追加，故末段才是它写的那一截。
fn is_orphan_tmp_name(name: &str) -> bool {
    let Some((_, tail)) = name.rsplit_once(ORPHAN_TMP_INFIX) else {
        return false;
    };
    let Some((pid, seq)) = tail.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !seq.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && seq.bytes().all(|b| b.is_ascii_digit())
}

/// 清扫 `dir` 下的**孤儿** tmp 残件（best-effort，失败只记日志）。
///
/// # 主触发器不是崩溃，是「下载途中退出 App」
///
/// [`update_download`] 是 **async command**：tmp 建立之后唯一的 await 点是
/// `spawn_blocking(...).await`。用户在下载完成前退出 App ⇒ tauri runtime **drop 掉那个 future**
/// ⇒ 从 [`PartialDownload`] 的 `Drop` 到各条早退，**全部被绕过**（future 被 drop 时局部变量确实
/// 会析构，但 `spawn_blocking` 的 blocking 线程**不可取消**，它会继续把 tmp 写完 —— 于是残件在
/// 守卫析构之后才出现）。开着 `autoDownloadUpdate` 时，每个「启动 → 后台下载 → 用户在完成前
/// 关 App」周期必留一个几十 MiB 的孤儿，而全仓唯一会碰这个目录的回收点是**完全卸载**
/// （`app_uninstall_all`）—— 长期累积到 GB 级。
///
/// 注意 `verify::tmp_name` 文档里那条「换核暂存目录每次 stage 整目录重建会一并清掉」的兜底
/// **只对换核腿成立**：App 更新落在 `<cache>/updates/`，没有任何整目录重建（该注释已一并订正）。
///
/// # 匹配面是**命名族**，不是「本次资产名」（2026-08-17 订正）
///
/// 原实现的前缀是 `{file_name}.polaris-new-` —— 只收**本次资产名**的残件。而 App 更新的资产名
/// 里带版本号（`Polaris_0.3.0_x64.dmg`），版本一换前缀就变，于是上面那个主触发器留下的残件
/// （必然是**旧版本名**的：这次要下的是新版本）**永远收不回来**，直到用户完全卸载 ——
/// 本函数自陈要防的那件事，恰好落在它的射程之外。
///
/// 故匹配面改成整个 `.polaris-new-` 命名族（判据见 [`is_orphan_tmp_name`]），不限资产名。
///
/// # 判据分两档（都不探测别的进程死活）
///
///  - **本次资产名 + 本 pid**：调用点在**单飞闸之内**，而闸按 dest 加锁 ⇒ 此刻本进程绝无第二条腿
///    在写同一个 `file_name` 的 tmp ⇒ 直接删（不看 mtime）。
///  - **其余全部**（别的资产名 / 别的 pid / 上次运行留下的）：只按 [`ORPHAN_TMP_MAX_AGE`]（≥24h）
///    删。读不到 mtime 一律当「新鲜」保留（失败安全的那一侧）。
///
/// 两档的边界就是单飞闸的射程：**闸只按 dest 串行**，管不到别的资产名 —— 本进程完全可能有另一条腿
/// 正在下另一个资产（版本回退、或用户手动触发了别的包），把「别的资产名」也放进即时档就会删掉
/// 一个在飞的下载。反过来，超过 24h 的残件不可能还是在飞下载（下载腿自带 30s 停滞看门狗 +
/// 15s 逐跳超时），故陈旧档对**任何** pid、**任何**资产名都安全。
///
/// 目录遍历走 [`UpdateFs::list_files`](polaris_updater::traits::UpdateFs::list_files)（它只列文件、
/// 跳过子目录）而不是手搓 `read_dir`：本函数因此可注入 `MockFs` 测试，也不会在 FS 抽象层上开
/// 第二个口子。mtime 不在 trait 面上（那是 trait 刻意不管的东西），故只对**需要**它的那一档
/// 读一次 `std::fs::metadata`。
///
/// 零新增依赖；失败一律吞掉（清扫是附带收益，**绝不**改变本次下载的结论）。
fn sweep_orphan_downloads(fs: &dyn polaris_updater::traits::UpdateFs, dir: &Path, file_name: &str) {
    // 即时档的前缀：本次资产名 + 本进程 pid（两者都对上才走即时删）。
    let self_prefix = format!("{file_name}{ORPHAN_TMP_INFIX}{}-", std::process::id());
    let Ok(names) = fs.list_files(dir) else {
        return; // 目录不存在 / 读不动：不是本次下载的问题，静默跳过。
    };
    let now = std::time::SystemTime::now();
    for name in names {
        if !is_orphan_tmp_name(&name) {
            continue; // 不是 tmp 命名族（含落位好的成品）。
        }
        let path = dir.join(&name);
        if !name.starts_with(&self_prefix) {
            // 别的资产名 / 别的 pid：只按 mtime 阈值收。读不到时间 → 当新鲜 → 保留。
            let stale = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age >= ORPHAN_TMP_MAX_AGE);
            if !stale {
                continue;
            }
        }
        match fs.remove_file(&path) {
            Ok(()) => log::info!("已清理更新包孤儿残件: {}", path.display()),
            Err(e) => log::debug!("清理更新包孤儿残件失败 {}: {e}", path.display()),
        }
    }
}

/// 纯判定（吃真实 FS）：已落盘的更新包能否被单飞的**后到者**直接复用。
///
/// 判据只有一条：文件在 **且** sha256 与本次期望相符。
///
/// **缺 sha256 时一律不复用**（返 false）：没有判据就不能声称「磁盘上这份就是你要的包」——
/// 旧 release 的 asset 没有 `digest` 字段，那种情况老老实实重下一遍，不拿文件名当身份。
///
/// # 摘要走流式（与下载腿同一个根因）
///
/// 原实现是 `std::fs::read(dest)` —— 为了一个 64 字节的判定，把整个安装包搬进内存。
/// 这与「下载时整包入内存」是**同一条缺陷的另一条腿**：下载腿改流式后若不一并修，
/// 单飞的后到者仍会在复用探测这一步把内存顶到包体积。判定结论逐字不变
/// （格式非法 / 摘要不符 / 文件不在 → 一律 false）。
///
/// # 比对委托 [`verify_hex_digest`](polaris_updater::verify::verify_hex_digest)（全 crate 单点）
///
/// 末行原本是手搓的 `actual.eq_ignore_ascii_case(sha)` —— 那是绕开该单点的第 4 处判定，
/// 而它的文档明写着不许手搓（两个变体处置相反，手搓必然把分野压成一个 bool）。本函数只需要
/// bool，但「只需要 bool」不是各写一份比较逻辑的理由：分叉只会在「大小写敏不敏感」
/// 这类地方发生，且只在真机大包上暴露。
///
/// 前置的 [`is_valid_sha256_hex`](polaris_updater::verify::is_valid_sha256_hex) 早退**保留**，
/// 理由与 `verify_bytes` 里那句「先验格式再算摘要」相同：期望值本身非法时，不该为一个必然
/// 返 false 的判定，白读一遍几十 MiB 的文件算流式摘要。
fn cached_download_is_reusable(dest: &Path, expected_sha: Option<&str>) -> bool {
    let Some(sha) = expected_sha.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if !polaris_updater::verify::is_valid_sha256_hex(sha) {
        return false;
    }
    let Ok(file) = std::fs::File::open(dest) else {
        return false;
    };
    let Ok(actual) = polaris_updater::verify::sha256_reader_hex(std::io::BufReader::new(file))
    else {
        return false;
    };
    polaris_updater::verify::verify_hex_digest(&actual, sha).is_ok()
}

/// 上游 `UPDATE_DOWNLOAD`：下载更新包到本地缓存目录。
///
/// ✅ **已接线**：复用 [`CoreDownloader`]（**唯一**下载适配器：重定向跟随 / UA / Content-Length 完整性 /
/// 停滞看门狗 / 镜像回退 / 16MiB 闸 / 15s 超时 / 403 限流分类），**不在此复制第二份编排**
/// —— 上游的 `core-downloader.ts` 与 `UpdateService.ts` 各写 ~170 行同构编排正是前车之鉴。
///
/// # 流式落盘（**不再把整包读进内存**）
///
/// 字节从网络下来即写进 `dest` 旁的同目录临时文件（`verify::tmp_name`），sha256 由
/// `verify::Sha256Stream` **边写边算**，全部收完且摘要相符后经 `verify::promote_staged`
/// 一次原子 rename 提升为 dest。内存占用与包体积**解耦**（原实现的 `Vec<u8>` 峰值 = 包体积，
/// 几十 MiB 到上百 MiB 的安装包直接顶在堆上）。
///
/// 三条不变式：
///  - **全程只碰 tmp**：dest 从「不存在」瞬间变为「完整文件」，故并发单飞的后到者经
///    [`cached_download_is_reusable`] 绝不会读到半截包；
///  - **失败即清理**：由 [`PartialDownload`] 这个 RAII 守卫承担 —— 不变量是「控制流离开这个作用域时
///    残件不在」，那是**作用域**的性质，只有类型表达得了（原先那条「数一数清理调用」的守卫为什么是
///    假的，见 [`PartialDownload`] 文档的三条反例）；
///  - **`downloaded` 只在 rename 成功之后发**：下载完成 ≠ 校验完成 ≠ 落位完成，早发一步就是
///    广播一个 dest 尚不存在的假成功态。落位判定与发事件被拆成 [`land_payload`] + 分支
///    （文本序守不住「成功才发」，成因见该函数文档）。
///
/// 落盘：`app_cache_dir()/updates/<fileName>`。同目录下**整个 `.polaris-new-` 命名族的孤儿 tmp**
/// （不限资产名 —— 残件多半带着**旧版本**的资产名）在拿到单飞闸之后顺手清一遍
/// （[`sweep_orphan_downloads`]）——「下载途中退出 App」会绕过上面那个 RAII 守卫。
///
/// **体积闸**：按 `updateInfo.fileSize` 声明值注入（**无裕度**，成因见 [`app_update_size_limit`]），
/// 并封在 [`APP_UPDATE_MAX_BYTES`] 之下；声明缺失/为 0 时直接取该上限。
/// **不**再与两条内核腿共用 16 MiB 内存闸 —— 那个闸的语义是「别把堆撑爆」，对流式落盘腿既无必要
/// 也卡不住正常安装包。
///
/// **完整性**（三级，由强到弱，**逐级都在**）：
///  1. 有期望摘要（[`resolve_expected_digest`]，当前只有 GitHub asset `digest`；随包 `SHA256SUMS`
///     虽已在发布侧产出，消费侧经判断不接，依据见该函数文档末节）→ sha256 **强校验**，
///     不符即丢弃 tmp、绝不落位（防截断/镜像投毒）；
///  2. 清单声明了 `fileSize` → **等值判据**（[`check_declared_size`]）。这一级是无摘要腿的主防线：
///     `fileSize` 来自 GitHub release 清单，镜像改不动；
///  3. 兜底 `Content-Length` 完整性（`CoreDownloader` 内）。**它对撒谎方零约束**——那个数就是撒谎方
///     自己给的，故绝不可把它当作「无摘要也安全」的理由（原文档正是这么写的，已订正）。
///
/// 三级全无（旧 release + 清单无 `fileSize`）时**不拒装**，但结果里 `verified:false`
/// **如实标记未校验**——否则所有旧 release 都更新不了。
///
/// **进度事件**：走 [`CoreDownloader::download_to_sink_with_progress`] 的逐 chunk 回调，发
/// `downloading(0%)` → `downloading(n%)`（**按整数百分比去重**，见下）→ `downloaded(100%)`。
/// 服务端不给 `Content-Length` 时算不出分母 → 中间那段没有百分比可发，进度条停在 0% 走
/// indeterminate（**不拿已收字节瞎凑一个分母**）。同一份进度由 [`emit_progress`] 一并镜像进 mini 弹窗。
///
/// **单飞**：按 `dest` 加闸（见 [`download_gate`]）。后台自动下载腿与用户在弹窗点「更新」的腿写同一个
/// dest，是默认流程而非异常；后到者等待 + 按 sha256 复用，绝不出现两个进度游标互相顶。
///
/// **失败即发 `error`**：本命令的**每一条**失败早退都先发一发 [`ProgressStage::Failed`] 再返错误信封 ——
/// 弹窗被推进 progress 后只有 `error`/`downloaded` 能把它推出去，静默 return 会让它永远转圈。
/// 该不变式由单测 `every_failure_path_emits_an_error_progress_event` 按计数锁住
/// （计数用**前缀** `ApiResponse::err`，故带 code 的早退同样在射程内）。
#[tauri::command]
pub async fn update_download(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    update_info: Value,
) -> Result<ApiResponse<Value>, ()> {
    // 本次下载**唯一**的发事件入口：随行事实（这次要下的那份发布清单）由它一处附着，
    // 下面十余个调用点因此没有「漏带」或「带成另一个版本的清单」的余地 —— 而设置页正是靠
    // 这份清单渲染版本号/体积、并在失败时重试。射程与成因见 [`ProgressStage`]。
    let emit = |stage: ProgressStage<'_>| emit_progress(&app, &update_info, stage);

    let Some(url) = update_info
        .get("downloadUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        // 前置校验的早退**也必须发 error 进度**：弹窗被 `force progress(0)` 推进 Progress 后，
        // 只有 `error` / `downloaded` 能把它推出去 —— 静默 return 会让窗永远转圈（只剩 Cancel）。
        let msg = "update_download 需要 updateInfo.downloadUrl";
        emit(ProgressStage::Failed(msg));
        return Ok(ApiResponse::err(msg));
    };
    let file_name = update_info
        .get("fileName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "polaris-update".to_string());
    // 路径穿越防线：文件名来自 GitHub 资产名，但它经过前端往返 —— 只取末段，绝不让 `../` 逃出目录。
    let file_name = std::path::Path::new(&file_name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "polaris-update".to_string());
    // 期望摘要：按来源优先级挑（D1）。字段在但不是字符串 ⇒ **显式早退**，绝不静默降级成
    // 「本来就没摘要」（那等于把发布方的失误偷偷换成少一道校验）。
    let expected_digest = match resolve_expected_digest(&update_info) {
        Ok(d) => d,
        Err(msg) => {
            emit(ProgressStage::Failed(&msg));
            return Ok(ApiResponse::err(msg));
        }
    };
    let expected_sha = expected_digest.as_ref().map(|d| d.hex.clone());
    // 声明大小 → 写入闸（**新增读取点**：改造之前本 command 根本没读过 fileSize）。
    let declared_size = update_info.get("fileSize").and_then(Value::as_u64);

    let dir = match app.path().app_cache_dir() {
        Ok(d) => d.join("updates"),
        Err(e) => {
            let msg = format!("解析缓存目录失败: {e}");
            emit(ProgressStage::Failed(&msg));
            return Ok(ApiResponse::err(msg));
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let msg = format!("建更新缓存目录失败 {}: {e}", dir.display());
        emit(ProgressStage::Failed(&msg));
        return Ok(ApiResponse::err(msg));
    }
    let dest = dir.join(&file_name);

    // ── 单飞（见 [`download_gate`]）：后到者在此等待，等待期间**不发任何进度事件**
    //    （否则两个游标互相顶）。首发者跑完再决定复用还是重下。
    let gate = download_gate(&dest);
    let _in_flight = gate.lock().await;

    // ── 孤儿 tmp 清扫（best-effort）：位置必须在**闸之内、`tmp_name` 之前** ——
    //    闸保证本进程此刻无人在写同名 tmp；`tmp_name` 之前保证不会误删自己这一次的残件。
    //    主触发器是「下载途中退出 App」（见 `sweep_orphan_downloads` 文档），RAII 守卫覆盖不到。
    sweep_orphan_downloads(&polaris_updater::traits::StdFs, &dir, &file_name);

    // 复用先完成者的成果（有 sha256 判据时才敢认，见 [`cached_download_is_reusable`]）。
    let reuse_probe = {
        let dest = dest.clone();
        let sha = expected_sha.clone();
        tokio::task::spawn_blocking(move || cached_download_is_reusable(&dest, sha.as_deref()))
            .await
            .unwrap_or(false)
    };
    if reuse_probe {
        log::info!(
            "更新包已在本地且 sha256 复核通过，复用不重复下载: {}",
            dest.display()
        );
        // 复用腿的 `verified` 恒 `true`：它之所以敢认盘上这份，判据**就是** sha256 比中
        // （见 `cached_download_is_reusable`）—— 与下面回包里那个 `true` 同一条理由、同一个值。
        emit(ProgressStage::Downloaded {
            path: &dest,
            verified: true,
        });
        return Ok(ApiResponse::ok(json!({
            "success": true,
            "filePath": dest.to_string_lossy(),
            "verified": true,
            // 复用分支**最有资格**标注来源：它之所以敢认盘上这份，正是靠这条 asset digest 比中的
            // （`cached_download_is_reusable` 的唯一判据）。此前这里漏了 `digestSource`，于是
            // 「复用」与「刚下的」两条成功路径的响应形状不一致，前端分不出摘要是谁给的。
            "digestSource": expected_digest.as_ref().map(|d| d.source.as_str()),
        })));
    }

    emit(ProgressStage::Downloading {
        percentage: 0,
        received: 0,
    });

    // ── 落盘目标：**全程只碰 tmp**，dest 直到最后一次 rename 之前一个字节都不动。
    //    tmp 由 `verify::tmp_name` 生成 ⇒ 与 dest 同目录同卷（原子 rename 的前提），
    //    且每次调用唯一 ⇒ 并发的另一条腿写的是另一个 tmp，不会互相截断。
    //
    //    RAII：从这一行起，**任何**离开本作用域的方式（早退 / `?` / panic / future 被 drop）
    //    都会删掉残件；只有落位成功那一条路才 `disarm`。不需要任何人记得配一行清理。
    let partial = PartialDownload::new(polaris_updater::verify::tmp_name(&dest));
    let dl = updater_downloader(&state, app_update_size_limit(declared_size));
    // 写句柄经 `UpdateFs` trait 取（生产 StdFs / 测试 MockFs 的注入纪律）。
    // 传**工厂**而非句柄：镜像回退换候选重下时要一个截断过的干净句柄。
    let sink_path = partial.path().to_path_buf();
    let new_sink: Arc<crate::runtime::http::DownloadSinkFactory> = Arc::new(move || {
        use polaris_updater::traits::UpdateFs;
        polaris_updater::traits::StdFs.open_write(&sink_path)
    });
    // `CoreDownloader::download*` 是**同步**桥（其 doc 明令须在 blocking 线程调用：
    // 在 async 上下文直调会阻塞 executor，Tauri 同步 command 更是跑在主线程上会冻 UI）。
    let url_for_task = url.clone();
    // 中间帧的发事件入口在下载 task 线程上，接不到上面那个借栈的 `emit` ⇒ 清单按值克隆一份
    // （一次下载克隆一次，不是一帧一次）。带的仍是**同一个** `update_info`。
    let on_progress = download_progress_emitter(&app, update_info.clone());
    let streamed = match tokio::task::spawn_blocking(move || {
        dl.download_to_sink_with_progress(&url_for_task, new_sink, on_progress)
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let msg = format!("下载更新包失败: {e}");
            emit(ProgressStage::Failed(&msg));
            // 「后端未接线」与「下载失败」**必须**可区分（trait 契约；生产注入的是 CoreDownloader，
            // 故这条实际不可达，但映射保留——折叠进泛化失败会让上层无限重试一个永不成功的调用）。
            return Ok(match e {
                polaris_updater::traits::DownloadError::BackendUnavailable(_) => {
                    ApiResponse::err_with_code(msg, CODE_HTTP_UNAVAILABLE)
                }
                _ => ApiResponse::err(msg),
            });
        }
        Err(e) => {
            let msg = format!("下载任务异常终止: {e}");
            emit(ProgressStage::Failed(&msg));
            return Ok(ApiResponse::err(msg));
        }
    };

    // ── 完整性第 2 级：清单声明的 `fileSize` 等值判据（见 [`check_declared_size`]）。
    //    这一级专治无摘要腿：Content-Length 是撒谎方自己给的数，对撒谎方零约束。
    if let Err(e) = check_declared_size(streamed.bytes, declared_size) {
        let msg = format!("更新包大小与发布清单声明不符（可能被截断或掉包）: {e}");
        emit(ProgressStage::Failed(&msg));
        return Ok(ApiResponse::err(msg));
    }

    // ── 完整性第 1 级：sha256 强校验（有摘要才做）——摘要是**边下边算**的，零额外 IO、零额外内存。
    //    判定委托 `verify::verify_hex_digest`（全 crate 单点），**按变体分流文案**：
    //    「发布方 digest 写坏了」重下一万次也不会好，把它显示成「可能被截断或篡改」只会引导用户
    //    反复重下。原实现手搓 `!is_valid_sha256_hex(..) || !eq_ignore_ascii_case(..)`，
    //    正是把这条分野压成了一个 bool。
    if let Some(d) = expected_digest.as_ref() {
        if let Err(e) = polaris_updater::verify::verify_hex_digest(&streamed.sha256_hex, &d.hex) {
            let msg = match e {
                polaris_updater::verify::VerifyError::InvalidExpectedHash(_) => format!(
                    "更新信息里的 sha256 摘要不是合法的 64 位十六进制（发布方写坏了，重试无用）: {}",
                    d.hex
                ),
                polaris_updater::verify::VerifyError::HashMismatch { .. } => format!(
                    "更新包校验失败（可能被截断或篡改）: expected {}, actual {}",
                    d.hex, streamed.sha256_hex
                ),
            };
            emit(ProgressStage::Failed(&msg));
            return Ok(ApiResponse::err(msg));
        }
    }

    // ── 落位：tmp 已在盘 ⇒ **只做 rename**，绝不把刚写完的文件读回内存（那会抵消整个改造）。
    //    dest 从「不存在」瞬间变成「完整文件」，故并发单飞的后到者经
    //    `cached_download_is_reusable` 绝不会读到半截包。
    //
    //    判定与发事件**必须分开**：文本序表达不了「rename 成功才发 downloaded」，
    //    成因见 [`land_payload`]。先求值再 match，让 `partial` 的借用在 match 之前就结束。
    let landing = land_payload(&polaris_updater::traits::StdFs, partial.path(), &dest);
    match landing {
        LandingOutcome::Failed(msg) => {
            // `promote_staged` 自己已尽力删过 tmp；`partial` 在此 return 时析构再兜一次。
            emit(ProgressStage::Failed(&msg));
            return Ok(ApiResponse::err(msg));
        }
        LandingOutcome::Landed => {
            // tmp 这个 inode 现在叫 dest 了 —— 先解除守卫，再广播成功。
            partial.disarm();
            // 「下载完成 / 校验完成 / 落位完成」三点分离后，`downloaded` **只在这一支**发：
            // 早一步发就是广播一个 dest 尚不存在的假成功态，设置页会给出一个点不开的安装入口。
            // 随行的 `path` 就是刚落位的那个 dest，`verified` 与下面回包里那个字段同源同值。
            emit(ProgressStage::Downloaded {
                path: &dest,
                verified: expected_digest.is_some(),
            });
        }
    }
    log::info!(
        "更新包已落位: {}（{} 字节，校验来源 {}）",
        dest.display(),
        streamed.bytes,
        expected_digest
            .as_ref()
            .map_or("none", |d| d.source.as_str())
    );
    Ok(ApiResponse::ok(json!({
        "success": true,
        "filePath": dest.to_string_lossy(),
        // `verified` 特指**摘要**这一级。无摘要时如实标记未校验（不拒装）——此时仍过了
        // `fileSize` 等值判据与 Content-Length，但那两级都弱于摘要，不配叫 verified。
        "verified": expected_digest.is_some(),
        "digestSource": expected_digest.as_ref().map(|d| d.source.as_str()),
    })))
}

/// 上游 `UPDATE_INSTALL`：安装已下载的更新包（生成平台脚本 → 停代理 → detached 起脚本 → 退出应用）。
///
/// ✅ **已接线**。决策全在纯函数 [`update_install::decide_install_plan`] /
/// [`update_install::build_install_script`]（真值表 + 快照单测），本 command 只做薄编排。
///
/// # 两段式：先告知，后执行
///
/// **用户已拍板走 ad-hoc 签名**（不买 Developer ID / Authenticode），故 macOS/Windows 都会被 OS 拦一道；
/// Linux deb 还要弹 polkit 提权框。[`update_install::install_advisory`] 判定后，**未确认前一律早退**，
/// 返 `{ok:false, needConfirm:true, advisory}`，由 UI 弹说明框讲清用户可执行的下一步。
///
/// **顺序不可换**（= 上游 `UpdateService.ts:306-315` 的明确注释）：确认框必须在**停代理之前**，
/// 用户取消即真 no-op —— 否则会留下「代理被停了但没更新」的坏态。
///
/// # 形态错配 → 交系统，**绝不强制 root**
///
/// 资产形态与运行形态错配（如 AppImage 运行拿到 `.deb`）时不自动提权安装，回退 `shell.open`
/// 让系统处理（= 上游 `:427-436`）。
#[tauri::command]
pub async fn update_install(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    file_path: String,
    confirmed: Option<bool>,
) -> Result<ApiResponse<Value>, ()> {
    let installer = std::path::PathBuf::from(&file_path);
    if !installer.is_file() {
        return Ok(ApiResponse::err(format!(
            "更新包不存在（请先下载）: {}",
            installer.display()
        )));
    }
    let Ok(exe_path) = std::env::current_exe() else {
        return Ok(ApiResponse::err("无法解析当前可执行文件路径"));
    };

    let os = std::env::consts::OS;
    let appimage = std::env::var_os("APPIMAGE").map(std::path::PathBuf::from);
    // 便携形态判据与 [`update_check`] 同源（标记文件），**不是** electron-builder 的
    // `PORTABLE_EXECUTABLE_FILE`（本仓恒不存在，见 [`is_portable_layout`]）。
    // `portable_exe` 的语义是「原便携 exe 路径」⇒ 便携形态下即当前 exe 自身。
    let portable = is_portable_layout(&exe_path).then(|| exe_path.clone());
    let run_form = update_install::detect_run_form(os, appimage.as_deref(), portable.as_deref());

    let plan = match update_install::decide_install_plan(
        os,
        run_form,
        &installer,
        &exe_path,
        appimage.as_deref(),
        portable.as_deref(),
    ) {
        Ok(p) => p,
        Err(reject) => {
            // 形态错配 / 不认识的资产 → **交系统处理**（不强制 root、不瞎猜脚本）。
            log::warn!("安装计划被拒（{reject:?}）：回退交系统打开");
            #[allow(deprecated)]
            let opened = app.shell().open(installer.to_string_lossy(), None).is_ok();
            return Ok(ApiResponse::ok(json!({
                "ok": false,
                "handedToSystem": opened,
                "reason": "form-mismatch",
                "detail": format!("{reject:?}"),
            })));
        }
    };

    // ── 安装前告知（ad-hoc 签名 / 提权）——未确认一律早退，**此刻还没碰代理**。
    if let Some(advisory) = update_install::install_advisory(&plan) {
        if confirmed != Some(true) {
            return Ok(ApiResponse::ok(json!({
                "ok": false,
                "needConfirm": true,
                "advisory": advisory.key(),
            })));
        }
    }

    let texts = update_install::InstallTexts::default();
    let spec = update_install::build_install_script(&plan, &texts);

    // ── 停代理（必须在写脚本/退出**之前**：Windows 上核进程占着文件会让替换失败）。
    let proxy = state.proxy.clone();
    if proxy.status().running {
        if let Err(e) = proxy.stop().await {
            // 停不掉就**不装**（带着跑着的核去替换应用本体 = 半死不活的坏态），如实报错。
            return Ok(ApiResponse::err(format!("安装前停止代理失败: {e}")));
        }
    }

    let dir = app
        .path()
        .app_cache_dir()
        .map(|d| d.join("updates"))
        .unwrap_or_else(|_| std::env::temp_dir());
    if let Err(e) = update_install::spawn_detached_script(&dir, &spec) {
        return Ok(ApiResponse::err(format!("启动安装脚本失败: {e}")));
    }

    log::info!("安装脚本已起（{:?}），应用即将退出", plan.platform);
    // 脚本已 detached；退出本进程把文件占用交出去（脚本内 `sleep 2` 等这一步）。
    app.exit(0);
    Ok(ApiResponse::ok(json!({ "ok": true, "success": true })))
}

/// 上游 `UPDATE_SKIP`：跳过本次版本。
///
/// ✅ **已接线**：纯本地状态（`update-state.json` 的 `skippedVersion`，原子写 tmp+rename）。不依赖网络。
#[tauri::command]
pub fn update_skip(state: State<'_, AppRuntime>, version: Option<String>) -> ApiResponse<()> {
    let Some(v) = version.filter(|s| !s.trim().is_empty()) else {
        return ApiResponse::err("update_skip 需要非空 version");
    };
    match state
        .updater()
        .mutate_state(|s| s.skipped_version = Some(v))
    {
        Ok(()) => ok_void(),
        Err(e) => ApiResponse::err(e),
    }
}

/// 泛 releases 列表页（无版本号可用时的回落目标）。
const RELEASES_LIST_URL: &str = "https://github.com/Sway-Chan/polaris/releases";

/// 拼「查看更新日志」目标 URL（纯函数，可测）：有版本号 → 直达该版本 release 页；否则回落泛列表页。
///
/// # tag 前缀已核实：本仓 release tag 恒带 `v`（不能凭 上游的写法直接抄）
///
/// 证据（均在本仓，非臆测）：
///  - `.github/workflows/package.yml`「产物标识：tag 用 tag 名（ref_name，如 `v0.1.0`）」；
///  - 同文件发布 job：`tag="${GITHUB_REF_NAME}"` → `gh release create "$tag" ...`——release 的 tag
///    **就是**推送的 git tag 本身，未做任何改写；
///  - 触发条件注释「push tags v*」；
///  - `GithubRelease.tag_name` 字段文档「如 `v0.2.0`」（`crates/updater/src/github.rs`）。
///
/// 传入的 `version` 取自 [`AppUpdateInfo::version`](polaris_updater::github::AppUpdateInfo) /
/// 弹窗 `UpdatePopupState.version`，两者都保留**原始 tag（已含 `v`）**。此处仍先
/// `trim_start_matches('v')` 再补一次前缀（= [`core_update_check_inner`] 同一行代码的写法）——
/// 防止调用方将来改传裸 semver 时把 URL 拼成 `vv0.1.0`，函数对两种输入形态都幂等。
///
/// `version` 为空/仅空白 → 回落 [`RELEASES_LIST_URL`]：拼一个大概率 404 的直达链接不如给用户一个
/// 打得开的页面。
#[must_use]
fn releases_url_for(version: Option<&str>) -> String {
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => format!("{RELEASES_LIST_URL}/tag/v{}", v.trim_start_matches('v')),
        None => RELEASES_LIST_URL.to_string(),
    }
}

/// 上游 `UPDATE_OPEN_RELEASES`：用系统浏览器打开 releases 页（shell 插件）。
///
/// #311：此前恒打开泛列表页——用户点「查看更新日志」找不到对应版本说明，形同摆设。现按
/// `version` 直达该版本 release 页（`/releases/tag/v<version>`，前缀依据见 [`releases_url_for`]）；
/// `version` 缺失/空时回落泛列表页。
///
/// 注：`shell.open` 在 tauri-plugin-shell 2.x 标记 deprecated，推荐 tauri-plugin-opener；
/// 切换属独立依赖决策（opener 需新增 crate 依赖），此处暂用 shell 并抑制 deprecation。
#[tauri::command]
#[allow(deprecated)]
pub fn update_open_releases(app: AppHandle, version: Option<String>) -> ApiResponse<()> {
    if let Err(e) = app.shell().open(releases_url_for(version.as_deref()), None) {
        return ApiResponse::err(format!("{e}"));
    }
    ok_void()
}

/// 上游 `UPDATE_POPUP_STATE`：mini 更新窗状态载荷（弹窗 → 主，拉取当前态）。
///
/// ✅ **已接线**：返回会话的 `last_state`。
///
/// **弹窗未开时返 `null` 而非编造 `{phase:"idle"}`**：「弹窗不存在」与「弹窗处于某态」是两回事，
/// 后者会让渲染端以为有个 idle 弹窗在。注意本移植下弹窗**首帧不依赖本 command** ——
/// 初始态随文档注入（`window.__POLARIS_UPDATE_POPUP_INITIAL__`），本 command 只作重连/兜底查询。
#[tauri::command]
pub fn update_popup_state(state: State<'_, AppRuntime>) -> ApiResponse<Option<UpdatePopupState>> {
    ApiResponse::ok(state.updater().popup_state())
}

/// 弹窗「更新 / 重试」**复查阶段**失败时回给用户的文案。
///
/// 同一条分支里有两处早退共用它（复查请求本身失败 / 复查回包缺版本号）。提成常量不是洁癖：
/// 两处原本是逐字相同的字面量，改一处漏一处就会出现「同一件事两种说法」。
const POPUP_RECHECK_FAILED: &str = "检查更新失败";

/// 复查回来之后的三档处置（[`reconcile_recheck`] 的结论）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecheckVerdict {
    /// 复查得到的版本与弹窗上写着的**逐字相同** ⇒ 照下。
    Proceed,
    /// 变了 ⇒ 退回 `remind(新版本)`，请用户对当前这串字重新决定。
    Renegotiate,
    /// 复查回包根本没有版本号（契约破损）⇒ 既无法对账，也不能弹个空版本号 ⇒ 按检查失败收场。
    Unusable,
}

/// **纯判定**：拿弹窗上写着的版本（`advertised`）与复查回来的版本（`rechecked`）对账。
///
/// 抽成纯函数是为了它**可被真值表钉死**：[`update_popup_action`] 持 `AppHandle` / `State`，
/// 单测构造不出 Tauri 运行时，判定留在里面就只剩源码级守卫能守 —— 那守得住「有没有调用」，
/// 守不住「判对没判对」。
///
/// # `advertised == None` 的含义：**会话把邀请版本弄丢了 = 不变式被破坏**
///
/// 判 [`RecheckVerdict::Renegotiate`] 作**兜底**：不知道当初承诺过什么，就不能声称「对上了」——
/// 失败安全的方向是让用户再确认一次，不是闷头下。
///
/// ⚠️ **不要把它读成「理论不可达」**。本注释 2026-08-17 前正是那么写的，而它每次「重试」都发生：
/// `Retry` 按 `PopupAction::is_valid_for` **只在 `Error` 态合法**，而 `UpdatePopupState::error()`
/// 只填 `error_text`（`..Self::default()` ⇒ `version: None`）⇒ 对账恒判「变了」⇒ 退回 remind、
/// 一个字节都不下，「重试」退化成「返回」。根因不在本函数，在**会话没记住自己邀请过谁**；已由
/// `polaris_updater::popup::PopupSession::send_state` 的版本继承修掉（那里有它自己的门）。
/// 所以今天走到这一档 = 那条继承坏了，属**哨兵**，不是常规路径。
fn reconcile_recheck(advertised: Option<&str>, rechecked: &str) -> RecheckVerdict {
    if rechecked.is_empty() {
        return RecheckVerdict::Unusable;
    }
    if advertised == Some(rechecked) {
        RecheckVerdict::Proceed
    } else {
        RecheckVerdict::Renegotiate
    }
}

/// 上游 `UPDATE_POPUP_ACTION`：弹窗 → 主进程按钮/关闭动作。
///
/// ✅ **已接线**（动作路由）：按 phase 白名单校验 → 合法动作执行副作用。
///
/// 语义对齐 `UpdateService.onPopupAction:680-702`：
///  - **非法动作静默忽略**（不报错、不改状态）—— 上游 `awaitPopupAction` 的 `valid` 白名单语义。
///  - `viewLog` **不结束等待**：开该版本 release 页（无版本号回落泛列表页），弹窗停在 remind。
///  - `manualDownload` 开外链（上游 `:696-700` 只放行 https；此处走该版本 release 页/回落泛列表页，天然满足）。
///  - `update` / `retry` **真下载**：弹窗自身只持有版本号、不持有下载地址，故先复查一次拿
///    `updateInfo`，再走 [`update_download`]；进度经 `update:progress` 广播，并由
///    [`emit_progress`] 镜像成弹窗 `progress` / `done` / `error` 三态。
///    「弹窗写的版本 == 真正下载的版本」这句不变量在这条分支上由**两道**东西合起来守：
///    ① 候选集**规则**——复查与产出邀请的腿共用 [`PUSH_UPDATE_INCLUDE_PRERELEASE`]；
///    ② 候选集**内容**——复查回来的 `version` 与 `popup.version` 逐字对账，不一致就退回
///    `remind(新版本)` 请用户重新确认，绝不换个目标接着下。缺一道都只守住一半。
///
/// # 窗内反馈的两个边角（`emit_progress` 覆盖不到的）
///
///  1. **复查期**：`update_check` 可跑满 15s，其间 `emit_progress` 一次都不发 → 用户点完按钮
///     看着 remind 态发呆。故本分支在复查**之前**先手动推一发 `progress(0)`。
///  2. **done 后关窗**：`done` 是终态，上游 800ms（[`DONE_AUTO_CLOSE_MS`]）后自动关窗；
///     进度事件本身不含「关窗」语义，故收在这里。
///
/// # 已知边界（如实登记）
///
/// `PopupAction::Cancel` 现在可达了（progress 态真会出现），但它只关窗、**不中断在飞的下载**——
/// `CoreDownloader` 无取消令牌。关窗后下载继续跑完并落盘，不会留半截文件（原子写），
/// 用户的下一次「更新」会直接复用。真正的下载取消需给下载器加 cancel token，单列。
#[tauri::command]
pub async fn update_popup_action(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    action: String,
) -> Result<ApiResponse<Value>, ()> {
    let Ok(act) = serde_json::from_value::<PopupAction>(Value::String(action.clone())) else {
        return Ok(ApiResponse::err(format!("未知弹窗动作: {action}")));
    };

    // 白名单：动作必须对当前 phase 合法，否则静默忽略（对齐上游）。
    let Some(popup) = state.updater().popup_state() else {
        // 弹窗不存在 → 无 phase 可校验（= 上游 awaitPopupAction 的 '__closed__' 短路）。
        return Ok(ApiResponse::ok(
            json!({ "handled": false, "reason": "popup-closed" }),
        ));
    };
    if !act.is_valid_for(popup.phase) {
        return Ok(ApiResponse::ok(
            json!({ "handled": false, "reason": "invalid-for-phase" }),
        ));
    }

    match act {
        // viewLog 不结束等待（弹窗停在 remind）。
        PopupAction::ViewLog => {
            let _ = update_open_releases(app, popup.version.clone());
            Ok(ApiResponse::ok(
                json!({ "handled": true, "resolving": false }),
            ))
        }
        PopupAction::Skip => {
            let u = state.updater();
            let v = u.popup_state().and_then(|s| s.version);
            let r = u.mutate_state(|s| s.skipped_version = v);
            let _ = close_update_popup(&app, u.popup());
            Ok(match r {
                Ok(()) => ApiResponse::ok(json!({ "handled": true })),
                Err(e) => ApiResponse::err(e),
            })
        }
        PopupAction::Later | PopupAction::Close | PopupAction::Cancel => {
            let u = state.updater();
            Ok(match close_update_popup(&app, u.popup()) {
                Ok(()) => ApiResponse::ok(json!({ "handled": true })),
                Err(e) => ApiResponse::err(e),
            })
        }
        // 真下载（**不再假装未接线，也不假装已开始**：下面这条路径确实会把包下下来）。
        PopupAction::Update | PopupAction::Retry => {
            // 复查期没有进度事件（可达 15s）→ 先把弹窗推进 progress，否则窗内零反馈。
            // 用 force：这一发正是「用户点了更新」的动作本身，闸（只放行 Progress）对它恒 false。
            force_popup_state(&app, UpdatePopupState::progress(0, None, None));
            // 口径与「产出这次邀请」的那条腿同源（见 PUSH_UPDATE_INCLUDE_PRERELEASE）：
            // 弹窗上写着哪个版本，这里复查出来的就得是哪个版本。
            let checked = update_check(
                app.clone(),
                state.clone(),
                Some(PUSH_UPDATE_INCLUDE_PRERELEASE),
            )
            .await?;
            let Some(data) = checked.data.filter(|_| checked.success) else {
                // 复查请求本身失败（网络 / 超时 / 非 2xx）。**只推弹窗，不广播** —— 与下面
                // 「已是最新」那档同一条理由：这条路径一个字节都没下，而 `emit_progress` 会把
                // `update:progress` 广播出去，让**设置页**弹一条它从未发起过的下载错误。
                // 弹窗是本次动作的唯一相关方（`emit_progress` 的弹窗镜像正是 `error(msg)` 这一发，
                // 故行为对弹窗逐字不变，少掉的只有那次全局广播）。
                let msg = checked
                    .error
                    .unwrap_or_else(|| POPUP_RECHECK_FAILED.to_string());
                push_popup_state(&app, UpdatePopupState::error(&msg));
                return Ok(ApiResponse::err(msg));
            };
            let Some(info) = data.get("updateInfo").cloned().filter(|v| !v.is_null()) else {
                // 复查回来没有任何可下载的包：弹窗停在 progress 会永远转圈 → 推进 `noupdate` 终态
                // （随后自动关窗）。
                //
                // **只推弹窗，不广播 `update:progress`**：这条路径一个字节都没下、没有 filePath，
                // 而全局广播会让设置页的 `onProgress` 显示「已下载」并给出一个不存在的安装入口
                // （`SettingsUpdate` 据 status==='downloaded' 判定包已就位）。弹窗是本次动作的
                // 唯一相关方，故状态只推给它。
                //
                // # 这里 2026-08-17 前推的是 `done()`，那是一句谎话
                //
                // 弹窗 `done` 渲染的是「下载完成」+ 满格进度条，而这条路径一个字节都没下。它至少
                // 有两条已核实的到达方式：① 用户已经是最新版；② 用户此前对**同一个版本**按过
                // 「跳过此版本」，再回弹窗点「更新」，复查被 `skipped_version` 过滤成 NoUpdate。
                // 与下面那道版本对账**同源**（复查结果与弹窗承诺不符），但处置不同：那边有新版本
                // 号可以退回 `remind` 重新征求，这边没有任何可下载的目标可展示。
                //
                // **不区分成因**：`check_app_update` 判 NoUpdate 有五条路（无正式发布 / 不比当前新 /
                // 已跳过 / 无适配本平台的资产 / 平台不受支持），回包只有一个 `hasUpdate:false`，
                // 后端**分辨不出**是哪一条。挑一条说出来就是拿状态冒充事实 —— 那正是本批要修的病。
                // 故只陈述确实知道的那件事：这次检查没找到 `popup.version` 的更新包。
                //
                // 版本号取 `popup.version`（弹窗上写着的那串字），不是复查结果 —— 复查什么都没返回。
                push_popup_state(&app, UpdatePopupState::no_update(popup.version.clone()));
                schedule_popup_auto_close(&app, NO_UPDATE_AUTO_CLOSE_MS);
                return Ok(ApiResponse::ok(
                    json!({ "handled": true, "hasUpdate": false }),
                ));
            };
            // ── 对账：复查回来的版本必须就是弹窗上写着的那串字 ────────────────────────
            //
            // [`PUSH_UPDATE_INCLUDE_PRERELEASE`] 只让两次 check 的**候选集规则**相同，管不了
            // **候选集内容** —— 邀请与兑现之间隔着用户的思考时间，上游随时可能再发一版。那时
            // 两端都是正式版（无预发布风险），但「按 A 邀请、下到 B」原样成立：弹窗上仍写着
            // v1.2.0，下回来的是 v1.3.0。而弹窗真写着的那串字就在同一作用域里 —— `popup.version`，
            // Skip 与 ManualDownload 两条分支一直在读它，唯独这条从不比对。
            //
            // **不一致时退回 remind(新版本)，不是「改个标签接着下」**：`progress` 态根本不渲染
            // 版本号（`ui/src/update-popup/main.ts` 的 `case 'progress'` 只有标题 + 进度条 + 字节数），
            // 把新版本号写进去用户一个字都看不见 —— 那是名义上的告知。退回 remind 让用户对
            // **当前这串字**再按一次「更新」，守的与本批同一句不变量：用户点的是哪个版本，就下哪个。
            // 代价只是罕见竞态下多一次点击；不会打转（下一次复查即命中同一版本）。
            //
            // `popup.version` 为 `None` 同样判不一致（不知道承诺过什么就不能声称对上了）。注意
            // 它**不是**「理论不可达」：`Retry` 只在 error 态合法，而 error 态的版本号靠
            // `PopupSession::send_state` 的**会话级继承**才有——那条继承一坏，重试就永不下载。
            // 判据与门都在 `polaris_updater::popup`，此处只作兜底。
            let rechecked = info
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match reconcile_recheck(popup.version.as_deref(), rechecked) {
                RecheckVerdict::Unusable => {
                    // hasUpdate 为真却没有版本号 = 后端契约破损。既无法对账，也**不弹空版本号**
                    // （与 `startup_tasks` / `tray` 两条产出腿同一处置）⇒ 按检查失败收场，
                    // 弹窗停在 error 仍可重试。文案与上面那条早退共用 `POPUP_RECHECK_FAILED`。
                    // 同样**只推弹窗、不广播**（理由见上面那条早退）。
                    log::warn!("弹窗复查：hasUpdate 为真但 updateInfo 缺 version，放弃本次下载");
                    push_popup_state(&app, UpdatePopupState::error(POPUP_RECHECK_FAILED));
                    return Ok(ApiResponse::err(POPUP_RECHECK_FAILED.to_string()));
                }
                RecheckVerdict::Renegotiate => {
                    log::info!(
                        "弹窗复查：邀请的是 {:?}，复查得到 {rechecked} —— 退回提醒态让用户重新确认",
                        popup.version
                    );
                    let current = app.package_info().version.to_string();
                    // 走带闸的 push（当前 phase 恒为上面 force 进去的 Progress，闸放行）：
                    // 弹窗若已被用户关掉，这里就该是 no-op，而不是替他重新弹一个窗。
                    push_popup_state(&app, UpdatePopupState::remind(rechecked, current));
                    // 只回 `handled`：弹窗端 `sendAction` 是 `void invoke(...).catch(() => {})`，
                    // 返回值整体丢弃，用户看到的变化全部由上面那发状态推送承载。曾多回过
                    // `reason` / `version` 两个字段 —— 零消费者、零测试、零对拍门，删。
                    return Ok(ApiResponse::ok(json!({ "handled": true })));
                }
                RecheckVerdict::Proceed => {}
            }

            // 下载腿内部经 emit_progress 推 downloading(n%) / downloaded(100%) / error。
            let resp = update_download(app.clone(), state, info).await?;
            if resp.success {
                schedule_popup_auto_close(&app, DONE_AUTO_CLOSE_MS);
            }
            Ok(resp)
        }
        PopupAction::ManualDownload => {
            let _ = update_open_releases(app, popup.version.clone());
            Ok(ApiResponse::ok(json!({ "handled": true })))
        }
    }
}

/// 终态后延时自动关窗。
///
/// 延时**由调用点传**，不是写死的 [`DONE_AUTO_CLOSE_MS`]：两个终态对用户的要求不同 ——
/// `done` 那一屏（= 上游 `UpdateService.ts:772` 的 800ms）用户不必读字就知道发生了什么；
/// `noupdate` 是唯一要求把一句话读完才有信息量的一屏，故取 [`NO_UPDATE_AUTO_CLOSE_MS`]。
/// 写死一个值就等于让后者一闪而过 —— 说了等于没说。
///
/// 单独 spawn 而非在命令里 sleep：命令得立刻返回给渲染端，否则弹窗按钮多转这么久才复位。
fn schedule_popup_auto_close(app: &AppHandle, delay_ms: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let Some(rt) = app.try_state::<AppRuntime>() else {
            return;
        };
        // 延时期间用户若手动关了窗，`close_update_popup` 是幂等的（窗没了只清会话槽）。
        if let Err(e) = close_update_popup(&app, rt.updater().popup()) {
            log::debug!("done 态自动关窗失败: {e}");
        }
    });
}

/// 开 mini 更新弹窗（主动唤起入口；供检查到新版本后调用）。
///
/// ✅ **已接线**：建窗路径的初始态随文档注入 —— #300/#301 的「建窗但从未下发初始态」在此**结构性不可达**
/// （见 `updater::popup` 模块文档）。
#[tauri::command]
pub fn update_popup_show(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    version: String,
    current_version: String,
) -> ApiResponse<()> {
    let u = state.updater();
    let st = UpdatePopupState::remind(version, current_version);
    match show_update_popup(&app, u.popup(), st) {
        Ok(()) => ok_void(),
        Err(e) => ApiResponse::err(e),
    }
}

// ── 内核更新 / 管理 ── 上游 `core-update-handlers.ts` ──

/// 上游 `core-update:check`：检查内核更新。
///
/// ✅ **已接线**：fork 硬闸 → 拉 `SagerNet/sing-box` releases → [`find_suitable_singbox_asset`]
/// 选平台/架构资产 → 版本比较 + 跨带标注。
///
/// # fork 硬闸（顺序要紧）
///
/// 活核是第三方 fork 时**直接拒绝、根本不发这次 HTTP 请求**（= 上游 `CoreUpdateService.ts:167,274`）。
/// 理由：官方 release 覆盖 fork 会静默吃掉用户明确选择的特性分支。该闸**必须前置于网络调用** ——
/// 「先请求再判」在功能上等价，但会向 GitHub 泄露一次本可避免的请求，且让闸看起来可有可无。
///
/// # 请求级总超时（[`CORE_CHECK_TOTAL_TIMEOUT_MS`]）
///
/// [`GITHUB_FETCH_TIMEOUT_MS`] 是**逐跳**超时（连接 + 读取，每一跳各算一次），而
/// `safe_redirect_fetch` 最多跟 5 跳 ⇒ 最坏可叠到 90s，远超契约要求的 20s 兜底。
/// 故此处再包一层**整体** `timeout`：无论内部跑到哪一步，20s 到点即返。
///
/// 超时用**独立错误码** [`CODE_CHECK_TIMEOUT`]（不折叠进泛化网络失败）：二者处置不同 ——
/// 网络失败可立刻重试，超时说明这条链路当前就是慢/被墙，重试大概率还是超时，UI 该提示配置加速。
///
/// **`invoke` 无取消语义**：前端做的 UI 等待上限只是不再等结果，后端请求照跑；这一层是唯一
/// 真能让请求停下来的地方（`timeout` drop 掉 future ⇒ 连接随之关闭）。
#[tauri::command]
pub async fn core_update_check(state: State<'_, AppRuntime>) -> Result<ApiResponse<Value>, ()> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(CORE_CHECK_TOTAL_TIMEOUT_MS),
        core_update_check_inner(state),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Ok(ApiResponse::err_with_code(
            format!(
                "检查内核更新超时（超过 {}s）：网络不可达或需要配置 GitHub 加速",
                CORE_CHECK_TOTAL_TIMEOUT_MS / 1000
            ),
            CODE_CHECK_TIMEOUT,
        )),
    }
}

/// 读 `config.coreUpdateChannel`：`"prerelease"` → true，其余（含缺省 / 非法值）→ false。
///
/// 缺省即 `stable`，与本开关引入前的写死行为逐字一致 —— 存量用户不会因为升级被动切进测试通道。
/// 非法值不在此处纠错：`store::sanitize` 已把它删掉，这里读到的要么是两个合法值之一，要么不存在。
fn core_update_channel_is_prerelease(state: &AppRuntime) -> bool {
    state
        .config()
        .get_value("coreUpdateChannel")
        .ok()
        .and_then(|v| v.as_str().map(|s| s == "prerelease"))
        .unwrap_or(false)
}

/// [`core_update_check`] 的本体（被 20s 总超时包着；见其文档）。
async fn core_update_check_inner(state: State<'_, AppRuntime>) -> Result<ApiResponse<Value>, ()> {
    let u = state.updater();
    let current_line = u.read_core_version_line();
    let current = u.read_core_version();

    // ── fork 硬闸：前置，零网络。
    if u.core_build_kind() == CoreBuildKind::Fork {
        return Ok(ApiResponse::err_with_code(
            "当前为第三方（非官方）内核，已禁用在线更新；请先回滚或重置到出厂内核",
            CODE_FORK_BLOCKED,
        ));
    }

    let Some(platform) = AssetPlatform::from_os(std::env::consts::OS) else {
        return Ok(ApiResponse::ok(json!({
            "hasUpdate": false,
            "currentVersion": current,
        })));
    };
    let arch = AssetArch::from_arch(std::env::consts::ARCH);

    let (owner, repo) = CORE_UPDATE_REPO;
    let body = match fetch_releases_json(&state, owner, repo).await {
        Ok(b) => b,
        Err(e) => return Ok(ApiResponse::err(format!("检查内核更新失败: {e}"))),
    };
    let releases: Vec<GithubRelease> = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Ok(ApiResponse::err(format!("解析 sing-box release 失败: {e}"))),
    };

    // 候选集按**更新通道**取（`config.coreUpdateChannel`）：
    //   - `stable`（缺省，与改动前行为逐字一致）：只看正式版；
    //   - `prerelease`：alpha / beta / rc 一并纳入。
    //
    // 为什么必须有这个开关：sing-box 的 alpha/beta 在 GitHub 上就是 `prerelease=true`，写死过滤等于
    // 把**整条预发布线**从候选里删掉 —— 跑 `1.14.0-alpha.31` 的用户不但看不到 `1.14.0-beta.1`，
    // 连「有没有更新」都恒为否：剩下的正式版是 1.13.x，`compare_semver` 判它更旧
    // （陈先生 2026-07-30 报「同样 1.14.0，alpha 无法更新到 beta」）。**比对逻辑本身没问题**，
    // `compare_semver` 完整实现了 semver 预发布优先级（alpha.31 < alpha.32 < beta.1 < rc.1 < 1.14.0）。
    //
    // **不按 alpha/beta/rc 再细分档次**：GitHub 只给一个 `prerelease` 布尔，档次仅存在于 tag 文本里，
    // 靠字符串猜会在上游改命名的那天静默失效。
    //
    // 跨带闸（`same_major_minor`）与本通道**正交**：通道决定「看不看预发布」，跨带决定「跨不跨 minor」，
    // 两道各管各的，开预发布不会顺带放开跨带。
    let include_prerelease = core_update_channel_is_prerelease(&state);
    let Some(release) = releases
        .iter()
        .filter(|r| include_prerelease || !r.prerelease)
        .max_by(|a, b| {
            a.published_at
                .as_deref()
                .unwrap_or("")
                .cmp(b.published_at.as_deref().unwrap_or(""))
        })
    else {
        return Ok(ApiResponse::ok(json!({
            "hasUpdate": false,
            "currentVersion": current,
        })));
    };

    let latest = release.tag_name.trim_start_matches('v').to_string();
    // 版本闸：必须**严格新于**当前（不可解析 → 失败安全按无更新，绝不误报有更新）。
    let is_newer = compare_semver(&latest, &current)
        .map(|o| o > 0)
        .unwrap_or(false);
    let Some(asset) = find_suitable_singbox_asset(&release.assets, platform, arch) else {
        // 无适配资产 → 如实报无更新（而非报错：这不是失败，是这台机器没有对应构建）。
        return Ok(ApiResponse::ok(json!({
            "hasUpdate": false,
            "currentVersion": current,
        })));
    };
    // 跨带标注（UI 据此提示兼容性风险；**自动路径**的硬闸在 `core_update_run` 里，此处只标注）。
    let cross_band = !same_major_minor(&latest, &current).unwrap_or(true);

    Ok(ApiResponse::ok(json!({
        "hasUpdate": is_newer,
        "currentVersion": current,
        "currentVersionLine": current_line,
        "latestVersion": latest,
        "downloadUrl": asset.browser_download_url,
        "assetName": asset.name,
        "sha256": asset.digest.as_deref().and_then(parse_asset_digest),
        "releaseNotes": release.body.clone().unwrap_or_default(),
        "crossBand": cross_band,
    })))
}

/// 上游 `core-update:update`：下载并更新内核（下载 → sha256 → 解压 → 停核 → 换 → 起核）。
///
/// ✅ **已接线**，零提权（落位于 `<config_dir>/core_update/`）。落位顺序见 [`swap_core_with_restart`]。
///
/// # 闸门顺序（前置判定全在网络之前）
///
/// 1. fork 硬闸（零网络）→ 2. 版本闸（不比当前新即 no-op）→ 3. 跨带闸（自动路径拒绝，手动路径由
///    `core_replace_manual` 承担）→ 4. 才下载。
///
/// # 完整性
///
/// GitHub asset `digest` 存在 → sha256 **强校验**，不符即拒绝（**字节绝不落盘**）；缺失回落
/// `CoreDownloader` 的 Content-Length 校验。
#[tauri::command]
pub async fn core_update_run(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    value: Option<String>,
) -> Result<ApiResponse<Value>, ()> {
    let base = match core_base_dir::<Value>() {
        Ok(b) => b,
        Err(resp) => return Ok(resp),
    };
    let u = state.updater();
    if u.core_build_kind() == CoreBuildKind::Fork {
        return Ok(ApiResponse::err_with_code(
            "当前为第三方（非官方）内核，已禁用在线更新；请先回滚或重置到出厂内核",
            CODE_FORK_BLOCKED,
        ));
    }
    let current = u.read_core_version();

    // 前端传 downloadUrl（invokeScalar → `{ value }`）；缺省则自己先查一次。
    let (url, expected_sha, latest) = match value.filter(|s| !s.trim().is_empty()) {
        Some(u) => (u, None, String::new()),
        None => {
            let checked = core_update_check(state.clone()).await?;
            let Some(data) = checked.data.filter(|_| checked.success) else {
                return Ok(ApiResponse::err(
                    checked.error.unwrap_or_else(|| "检查内核更新失败".into()),
                ));
            };
            if !data
                .get("hasUpdate")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(ApiResponse::ok(json!({ "result": "noop", "ok": true })));
            }
            let Some(u) = data.get("downloadUrl").and_then(Value::as_str) else {
                return Ok(ApiResponse::err("内核更新缺下载地址"));
            };
            (
                u.to_string(),
                data.get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                data.get("latestVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        }
    };

    // 跨带硬闸（**自动路径**）：跨 major.minor 不自动换（= 上游 `sameMajorMinor` 闸）。
    // 手动换核路径（`core_replace_manual`）可绕过——那是用户明确的一次性选择。
    if !latest.is_empty()
        && !same_major_minor(&latest, &current).unwrap_or(true)
        && state
            .config()
            .get_value("restrictCoreUpdateToCompatibleMinor")
            .ok()
            .and_then(|v| v.as_bool())
            != Some(false)
    {
        return Ok(ApiResponse::ok(json!({
            "result": "deferred",
            "ok": false,
            "crossBand": true,
            "latestVersion": latest,
        })));
    }

    let asset_name = url.rsplit('/').next().unwrap_or("core-asset").to_string();
    // 内核腿整包入内存（解归档要用）⇒ 闸就是内存闸，逐字沿用形参化之前的 16 MiB。
    let dl = updater_downloader(&state, MAX_DOWNLOAD_BYTES);
    let url_for_task = url.clone();
    let bytes = match tokio::task::spawn_blocking(move || dl.download(&url_for_task)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Ok(ApiResponse::err(format!("下载内核失败: {e}"))),
        Err(e) => return Ok(ApiResponse::err(format!("下载任务异常终止: {e}"))),
    };
    if let Some(sha) = expected_sha.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = verify_bytes(&bytes, sha) {
            return Ok(ApiResponse::err(format!(
                "内核校验失败（可能被截断或篡改），已拒绝落位: {e}"
            )));
        }
    }

    // 归档 → 裸二进制。官方资产是 .tar.gz/.zip，解压走 OS 自带 tar（不引新依赖，见 core_swap 模块文档）。
    let core_bytes = match extract_core_bytes(base, &asset_name, &bytes) {
        Ok(b) => b,
        Err(e) => return Ok(ApiResponse::err(e)),
    };

    let version_line = if latest.is_empty() {
        String::new()
    } else {
        latest.clone()
    };
    Ok(swap_core_with_restart(
        &app,
        &state,
        base,
        &core_bytes,
        &version_line,
        core_swap::SwapSource::Update,
        false,
        &current,
        // 用户明确点了「更新」→ 允许为换核停/起核。
        SwapInterrupt::Allowed,
    )
    .await)
}

/// 把下载到的资产解成裸核字节（归档 → 解压 → 定位；已是裸二进制则原样返回）。
///
/// 解压落在 `<base>/core-staged/extract-<pid>-<seq>`（**每次调用唯一** + RAII 清理，见 [`ExtractWorkDir`]），
/// 不污染现役核目录、也不与并发的另一条解归档腿互踩。
///
/// `pub(crate)`：内核自动更新调度器（`runtime/core_update_scheduler.rs`）在暂存前走**同一条**
/// 解归档路径 —— 各写一份会在「哪些资产算裸二进制 / 解压到哪 / 何时清工作目录」上漂移。
pub(crate) fn extract_core_bytes(
    base: &std::path::Path,
    asset_name: &str,
    bytes: &[u8],
) -> Result<Vec<u8>, String> {
    if core_swap::is_raw_binary_asset(asset_name) {
        return Ok(bytes.to_vec());
    }
    let work = ExtractWorkDir::create(base)?;
    // 归档名来自资产名（可能含 `../`）→ 只取末段，绝不让它逃出工作目录。
    let archive_name = std::path::Path::new(asset_name).file_name().map_or_else(
        || "core-asset".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let archive = work.path().join(&archive_name);
    std::fs::write(&archive, bytes).map_err(|e| format!("写归档失败: {e}"))?;
    let out = work.path().join("x");
    core_swap::extract_archive(&archive, &out)?;
    let core = core_swap::pick_core_from_dir(&out, core_paths::core_filename())?;
    std::fs::read(&core).map_err(|e| format!("读解压产物失败: {e}"))
    // `work` 在此 drop → 无论成败（含上面任一 `?` 早退）都清掉工作目录。
}

/// 解归档工作目录：`<base>/core-staged/extract-<pid>-<seq>`（**每次调用唯一** + RAII 清理）。
///
/// # 为什么必须唯一
///
/// 原实现固定用 `core-staged/extract`，且入口就是 `rm -rf` 再重建。两条解归档腿并发时
/// （调度器的自动下载腿 vs 用户点的 `core_update_run` —— 调度器的 `busy` 闸**不覆盖手动命令**）
/// 它们互相 rm/写同一个目录：一方可能读到**对方**的核字节，并以自己的版本号 stage / 换入 ——
/// 簿记记的是 A 的版本，落盘的是 B 的二进制，且两边都「成功」。
///
/// RAII 清理同时修掉原实现的另一半：原来的 `let _ = remove_dir_all(&work)` 在 `?` 早退
/// （建目录后、写归档失败）和 panic 路径上都不执行，会留下 GB 级残件。
struct ExtractWorkDir {
    path: std::path::PathBuf,
}

impl ExtractWorkDir {
    fn create(base: &std::path::Path) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        /// 进程内单调序号（同一毫秒的两次调用也不会撞名）。
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            core_paths::staged_dir_in(base).join(format!("extract-{}-{seq}", std::process::id()));
        // 同名残件（上次进程硬崩留下的）先清；正常情况下这个路径不存在。
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|e| format!("建解压工作目录失败: {e}"))?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for ExtractWorkDir {
    fn drop(&mut self) {
        // 无论成败都清（本机 /tmp 与 config 目录都不该留 GB 级残件）。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 本次换核**允不允许**为换核停/起核。
///
/// 「绝不主动断流」是内核自动更新的硬不变量，而唯一持有 `proxy.stop()` 的地方是
/// [`swap_core_with_restart`] —— 判据必须收在那里，见 [`swap_blocked_by_no_interrupt`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapInterrupt {
    /// 用户明确发起（在线更新 / 回滚 / 手动替换 / 重置出厂 / 点「立即应用」）→ 可停可起。
    Allowed,
    /// 自动路径 → **绝不断流**：换核那一刻代理若在跑，一律放弃本次落位并保留 staged。
    Forbidden,
}

/// 纯判定：本次换核是否被「绝不主动断流」硬不变量拦下。
///
/// # 为什么这条判定必须在 [`swap_core_with_restart`] 里、而不能只留在调度器
///
/// 调度器的 `apply_staged_auto` 先判 `proxy.status().running == false` 才调落位命令，但从那次判定
/// 到 `swap_core_with_restart` 真正 `proxy.stop()` 之间，还隔着「读 `update-state.json` → 版本闸 →
/// **读几十 MB 的 staged 核文件** → sha 复核」几十~几百 ms（post-download 腿更是在下载完成的任意
/// 时刻触发）。用户在这个窗口里点「连接」，落位就会照常走 stop→swap→restart，**在用户从未同意的
/// 情况下断流一次**；新核起不来还会再走一遍回滚舞。
///
/// 判定放到读 `was_running` 的那一层后，判定与 `stop()` 之间不再有任何 `await`，窗口收敛到不可见。
#[must_use]
const fn swap_blocked_by_no_interrupt(interrupt: SwapInterrupt, was_running: bool) -> bool {
    matches!(interrupt, SwapInterrupt::Forbidden) && was_running
}

/// 取换核返回里的 `result` 字段。
///
/// `deferred` 的信封是 `success:true`（它不是错误，是一次合法的「本轮不落位」），故**必须**能与
/// 「真的换成功了」区分 —— 否则 [`apply_staged_inner`] 会把一个字节都没换的轮次当成功、清掉 staged。
fn swap_result_code(resp: &ApiResponse<Value>) -> Option<&str> {
    resp.data.as_ref()?.get("result")?.as_str()
}

/// 换核落位 + 停/起核编排（**唯一**的停起腿；四条换核路径共用）。
///
/// 顺序严格对齐规格 §7.3：
/// ```text
/// 1. 记录当前是否在跑 + 快照配置
/// 2. proxy.stop()                       ← 经 ProxyRuntime 公有 API，其内部走 LifecycleGate
/// 3. 备份现役核 → <core>.bak（skip_backup 时跳过）
/// 4. atomic_replace（tmp+rename）+ chmod +x +（macOS）xattr -cr / codesign
/// 5. proxy.start(config)
/// 6. 验证闩：起核失败 → 回滚 .bak → 再起 → 如实返 failed
/// 7. 成功 → 写 pendingChangeNotice{previous,current}
/// ```
///
/// **不绕过 `LifecycleGate` 自行发信号**：`ProxyRuntime::stop/start` 内部已持有 gate
/// （起停竞态单飞 + 世代守卫），此处调它们即是经 gate。
#[allow(clippy::too_many_arguments)]
async fn swap_core_with_restart(
    app: &AppHandle,
    state: &AppRuntime,
    base: &std::path::Path,
    core_bytes: &[u8],
    version_line: &str,
    source: core_swap::SwapSource,
    skip_backup: bool,
    previous_version: &str,
    interrupt: SwapInterrupt,
) -> ApiResponse<Value> {
    // ── wire 契约前置检查：**非随包核唯一有牙的地方** ──────────────────────────
    //
    // `crates/singbox-grpc/build.rs` 那道 release 硬门的取材面只有 `resources/*/sing-box` 四条路径，
    // 而在线换核 / 用户自带 fork 走的正是本函数 —— 该路径此前无任何 wire 对拍，
    // 是 2026-08-05 那类「上游插一个字段把后面全顶掉一位 ⇒ 整条流静默死掉」在本仓仍敞着的一格。
    //
    // 判据只拦「字段号对不上」这一档（prost 整帧解码失败 → 当断线无限重连 → 功能**静默**消失，
    // 用户看不出）；符号缺失 / 抠不出 descriptor 一律放行 + 告警 —— 没观测到 ≠ 观测到没问题，
    // 据一次读失败剥夺用户自选的核是更坏的错（判据分档见 `verdict_for_core_bytes` 文档）。
    //
    // 位置在**停核之前**（也在 `swap_blocked_by_no_interrupt` 之前）：拒绝时用户的代理毫发无损，
    // 且不必先把一份注定要拒的核落到盘上。本检查是纯 CPU 的字节扫描，不含 await。
    // **回滚豁免**：那份 `.bak` 正是用户此前一直在跑的核，而回滚的触发场景恰恰是「新核不可用」——
    // 在这里拒绝回滚 = 把用户困在坏核上，且没有任何一条更安全的路可走。故只对「换上一份新核」
    // 这个方向拦；`Rollback` 直接放行（它换回去的东西本来就是现状的上一态）。
    if !matches!(source, core_swap::SwapSource::Rollback) {
        match polaris_singbox_grpc::verdict_for_core_bytes(core_bytes) {
            polaris_singbox_grpc::WireVerdict::Match => {}
            polaris_singbox_grpc::WireVerdict::Unobservable(why) => {
                log::warn!(
                    "换核 wire 契约对拍取不到判据（{why}）→ 放行（没观测到 ≠ 观测到没问题）"
                );
            }
            polaris_singbox_grpc::WireVerdict::Mismatch(report) => {
                log::error!("换核被拒：目标内核的管理 API 字段布局与本应用不一致\n{report}");
                return ApiResponse::err(format!(
                    "目标内核的管理 API 字段布局与本应用不一致，已放弃换核 —— \
                     换上去会让日志、连接、组网状态等整块**静默**失效（不报错、只是没有下一帧）：\n{report}"
                ));
            }
        }
    }

    let proxy = state.proxy.clone();
    let was_running = proxy.status().running;
    // ── 「绝不主动断流」硬不变量：判在**拥有 stop 的这一层**（成因见 `swap_blocked_by_no_interrupt`）。
    //    此后到 `proxy.stop()` 之间不得再插入任何 await —— 那会把 TOCTOU 窗口重新撑开。
    if swap_blocked_by_no_interrupt(interrupt, was_running) {
        log::info!("换核前一刻代理已在运行 → 自动路径放弃本次落位，保留 staged 待下次安全窗口");
        return ApiResponse::ok(json!({
            "result": "deferred",
            "ok": false,
            "reason": "proxy-running",
        }));
    }
    // 起核用的配置在停核**之前**快照：停完再读若失败就没法把用户的代理恢复回去。
    let config = if was_running {
        match state.config().load_full() {
            Ok(c) => Some(c),
            Err(e) => return ApiResponse::err(format!("读取配置失败，已放弃换核: {e}")),
        }
    } else {
        None
    };

    if was_running {
        if let Err(e) = proxy.stop().await {
            // 停不掉就**不换**（在核跑着时替换二进制：Linux ETXTBSY / Windows 文件占用）。
            return ApiResponse::err(format!("换核前停止内核失败，已放弃换核: {e}"));
        }
    }

    let swap = match core_swap::install_core_bytes(
        base,
        std::env::consts::OS,
        core_bytes,
        version_line,
        source,
        skip_backup,
    ) {
        Ok(s) => s,
        Err(e) => {
            // 落位失败 → 现役核未动（atomic_replace 保证），把代理恢复回去再报错。
            if let Some(c) = config {
                let _ = proxy.start(c).await;
            }
            return ApiResponse::err(format!("换核失败: {e}"));
        }
    };

    // ── 验证闩：新核起不来就自动回退（= 上游 `:1401` 的 arm 验证闩；skipBackup 时不 arm）。
    if let Some(c) = config {
        if let Err(start_err) = proxy.start(c.clone()).await {
            log::error!("新内核起核失败（{start_err}），触发自动回退");
            if swap.backed_up {
                match core_swap::rollback_core(base, std::env::consts::OS, previous_version) {
                    Ok(_) => {
                        let restarted = proxy.start(c).await.is_ok();
                        return ApiResponse::err(format!(
                            "新内核启动失败（{start_err}），已自动回滚到原内核{}",
                            if restarted {
                                "并重新启动"
                            } else {
                                "，但重启失败，请手动启动"
                            }
                        ));
                    }
                    Err(e) => {
                        return ApiResponse::err(format!(
                            "新内核启动失败（{start_err}），且回滚也失败（{e}）——请到设置页「重置为出厂内核」"
                        ));
                    }
                }
            }
            return ApiResponse::err(format!(
                "新内核启动失败（{start_err}），且本次未留备份（重置出厂路径）——请重试或重装应用"
            ));
        }
    }

    // ── 簿记回写：以**盘上那个二进制**为版本真值 ──
    // `install_core_bytes` 写进簿记的是调用方声明的 `version_line`，而两条主路径给的是空串
    // （`core_update_run` 前端恒传 downloadUrl ⇒ latest 为空；`core_rollback` 直接传 ""）。
    // 空簿记 ⇒ 下次启动判 unknown ⇒ `decide_reseed` 恒 Keep ⇒ 这个核被**永久钉住**，之后
    // `bundledCoreVersion` 提到多高都不再播种，盘面与 UI 都看不出来。判据与取舍见
    // `core_swap::marker_rewrite_line`。
    //
    // 位置**必须在验证闩之后**：闩内失败会走 `rollback_core`（它自己按 `previous_version`
    // 重写簿记），此刻盘上已经是**旧核**，在闩前回写只会把旧核的版本盖到新核的簿记上。
    // 用 `read_core_version_line()`（探测失败返空串），**不用** `read_core_version()`
    // ——后者失败回落随包基线，会把「读不到」伪装成「就是基线」写进簿记。
    let probed_line = state.updater().read_core_version_line();
    // 回写失败**不致命**：核已换好且已起来，此刻中止只会丢掉一次成功的换核。但要如实告警。
    if let Err(e) = core_swap::rewrite_marker_from_probe(base, version_line, &probed_line, source) {
        log::warn!("换核簿记回写失败（{e}）：簿记仍为空，该核将不被后续随包基线重播种");
    }

    // 版本变更通知（push 型持久位；banner show→ack 弹一次，非每启动重弹）。
    let current_version = state.updater().read_core_version();
    let _ = state.updater().mutate_state(|s| {
        s.pending_change_notice = Some(crate::runtime::updater::PendingChangeNotice {
            previous_version: previous_version.to_string(),
            current_version: current_version.clone(),
        });
    });
    crate::events::broadcast(
        app,
        crate::events::channel::EVENT_CORE_VERSION_CHANGED,
        json!({
            "previousVersion": previous_version,
            "currentVersion": current_version,
            "hasBackup": core_swap::has_backup(base, std::env::consts::OS),
        }),
    );

    // ── 稳定观察窗：起核成功**不等于**换核成功（= 上游 `armPendingValidation` + `startStabilityWatch`）──
    // 上面那道验证闩是同步的：`proxy.start()` 返 Err 才回滚。新核「起得来、几十秒后崩」这一类
    // 它一概看不见。守护腿补的正是这一段，条件与 上游 一致：
    //   · `was_running` —— 没重启过就没有「首次运行」可观察（arm 的前提是刚起了一次新核）；
    //   · `swap.backed_up` —— 没备份就无处可回滚，arm 了也只能干看着；
    //   · 非 Rollback 源 —— 否则回滚本身又 arm 一次，形成来回换核的循环。
    //     （Rollback 走 `skip_backup=true` ⇒ `backed_up` 恒 false，本条是**第二道**闸，写明意图。）
    if was_running && swap.backed_up && source != core_swap::SwapSource::Rollback {
        arm_core_validation(app.clone(), current_version.clone());
    }

    ApiResponse::ok(json!({
        "ok": true,
        "result": "applied",
        "corePath": swap.core_path.to_string_lossy(),
        "hasBackup": swap.backed_up,
        "previousVersion": previous_version,
        "currentVersion": current_version,
        "restarted": was_running,
    }))
}

/// 换核后的**稳定观察窗守护腿**（上游 `startStabilityWatch` + `autoRollbackIfPendingUpdate` 的编排侧）。
///
/// 判据与常量在 [`core_validation`](crate::runtime::core_validation)（纯逻辑、可穷举单测）；
/// 本函数只做那三件带副作用的事：**置抑制位 → 轮询观察 → 回滚**。
///
/// # 时序
///
/// 1. 置 `auto_restart_suppressed(true)`：窗口内核意外退出**不自动重启**。没有这一步，Polaris 的
///    崩溃自愈会先退避重启 3 次再放弃 —— 把「新核有问题」这个信号淹掉，而那是本窗口唯一要采集的信息。
/// 2. 每 `POLL_INTERVAL` 看一次 `proxy.status()`，直到 `STABILITY_DWELL` 走完。
/// 3. 观察到 [`failure_warrants_rollback`](crate::runtime::core_validation::failure_warrants_rollback)
///    成立 → **先撤抑制位再回滚**（顺序不可反：回滚回去的老核必须立刻恢复崩溃自愈保护），
///    然后走 [`core_rollback`] 的整条编排（读备份 → 停/起核 → prune → 清陈旧「已更新」通知）。
///
/// # 已知边界（如实登记，与 上游 同）
///
/// - 用户在窗口内**主动停核** ⇒ `running=false` 但无错误码 ⇒ 判据不成立 ⇒ 窗口走完按「稳定」收尾。
///   即这次换核实际上没被真正验证过。上游的 `startStabilityWatch` 定时器同样照常到期，
///   本移植不额外收紧 —— 收紧要引入「用户意图」输入，那是新语义不是移植。
/// - 观察窗只覆盖**本次进程存活期**。窗口内应用退出 ⇒ 状态随进程消失，下次启动不续验（上游 亦然：
///   `pendingUpdateVersion` 是内存态）。
fn arm_core_validation(app: AppHandle, new_version: String) {
    use crate::runtime::core_validation::{
        failure_warrants_rollback, POLL_INTERVAL, STABILITY_DWELL,
    };

    tauri::async_runtime::spawn(async move {
        let proxy = app.state::<AppRuntime>().proxy.clone();
        proxy.set_auto_restart_suppressed(true);
        log::info!(
            "换核验证窗口开启（{}s）：新内核 {new_version}，窗口内核异常退出将自动回滚",
            STABILITY_DWELL.as_secs()
        );

        let mut waited = std::time::Duration::ZERO;
        let mut failed = false;
        while waited < STABILITY_DWELL {
            tokio::time::sleep(POLL_INTERVAL).await;
            waited += POLL_INTERVAL;
            let st = proxy.status();
            if failure_warrants_rollback(st.running, st.error_code.as_deref()) {
                log::error!(
                    "换核验证窗口内新内核 {new_version} 异常退出（code={:?}，距换核 {:?}）→ 自动回滚",
                    st.error_code,
                    waited
                );
                failed = true;
                break;
            }
        }

        // 撤抑制位**必须在回滚之前**：回滚会把老核起回来，那一刻起它就该受崩溃自愈保护；
        // 也覆盖「稳定收尾」这条路径，故放在分支之外无条件执行。
        proxy.set_auto_restart_suppressed(false);

        if !failed {
            log::info!("新内核 {new_version} 稳定运行 {STABILITY_DWELL:?}，换核验证通过");
            return;
        }

        // 复用 `core_rollback` 的整条编排（读备份 → 同一条停/起核 → prune → 清陈旧通知），
        // 不在此另写一份：那会变成第二个「怎么回滚」的真值源。
        match core_rollback(app.clone(), app.state::<AppRuntime>()).await {
            Ok(resp) if resp.success => {
                log::warn!("已自动回滚到原内核（新内核 {new_version} 未通过换核验证）");
            }
            Ok(resp) => {
                log::error!(
                    "自动回滚失败：{} —— 请到设置页手动回滚或「重置为出厂内核」",
                    resp.error.as_deref().unwrap_or("未知原因")
                );
            }
            Err(()) => log::error!("自动回滚调用失败 —— 请到设置页手动回滚"),
        }
    });
}

/// 上游 `core:getVersionInfo`：内核版本信息。
///
/// ✅ **已接线**（§C6 的主消费点）：`currentVersion` / `bundledVersion` / `build`
/// （official|fork|unknown）/ `pendingChangeNotice`。
///
/// `build` 经 `classify_core_build(原始版本行)` —— 喂的是**失败置空**的读法产物，
/// 故探测失败时如实报 `unknown`（**不回落基线伪装成 official**）。
#[tauri::command]
pub fn core_get_version_info(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    let u = state.updater();
    let st = u.state();
    let build = match u.core_build_kind() {
        CoreBuildKind::Official => "official",
        CoreBuildKind::Fork => "fork",
        CoreBuildKind::Unknown => "unknown",
    };
    // 备份状态现读**真实** `.bak`（换核链路已接线，备份由任何一次换核产生）。
    let os = std::env::consts::OS;
    let backup = core_paths::base_dir().filter(|b| core_swap::has_backup(b, os));
    // ⚠️ `backupVersion` 需跑 `<bak> version` 才知道 —— 那是**执行内核二进制**，属真机腿
    // （本机禁起核）。故这里如实返 `null` 而非编造：UI 只需 `hasBackup` 就能给出「回滚」按钮，
    // 版本号缺失不影响功能。**绝不**拿现役核版本冒充备份版本（那会让用户以为回滚到别的版本）。
    ApiResponse::ok(json!({
        "currentVersion": u.read_core_version(),
        "bundledVersion": u.bundled_core_version(),
        "build": build,
        "hasBackup": backup.is_some(),
        "backupVersion": Value::Null,
        "pendingChangeNotice": serde_json::to_value(st.pending_change_notice).unwrap_or(Value::Null),
    }))
}

/// 上游 `core:rollback`：回滚到备份内核。
///
/// ✅ **已接线**：`<core>.bak` → `<core>`（原子替换）+ 停/起核 + 清 `pendingChangeNotice`。
///
/// 无备份时**保留** [`CODE_NO_BACKUP`]——那是**正确语义**（真的没有可回滚的东西），不是 stub。
#[tauri::command]
pub async fn core_rollback(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    let base = match core_base_dir::<Value>() {
        Ok(b) => b,
        Err(resp) => return Ok(resp),
    };
    let os = std::env::consts::OS;
    if !core_swap::has_backup(base, os) {
        return Ok(ApiResponse::err_with_code(
            "没有可回滚的内核备份",
            CODE_NO_BACKUP,
        ));
    }
    let previous = state.updater().read_core_version();
    let backup_path = match core_paths::core_backup_path() {
        Some(p) => p,
        None => return Ok(ApiResponse::err("无法解析备份路径")),
    };
    // 空文件必须拒绝：落位一个 0 字节的核 = 直接 brick（起核必失败，且备份已被消费掉）。
    // `.bak` 由 `install_core_bytes` 从非空字节产出，故空只会来自外部截断/磁盘故障 ——
    // 概率低，但后果与另两条腿完全相同，判据不该只有两条腿有。
    let bytes = match std::fs::read(&backup_path) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return Ok(ApiResponse::err("内核备份文件为空（已损坏），放弃回滚")),
        Err(e) => return Ok(ApiResponse::err(format!("读取内核备份失败: {e}"))),
    };

    // 走同一条停/起核编排（`skip_backup=true`：备份正被消费，不该再拿它备份一次自己）。
    let resp = swap_core_with_restart(
        &app,
        &state,
        base,
        &bytes,
        "",
        core_swap::SwapSource::Rollback,
        true,
        &previous,
        // 用户明确点了「回滚」→ 允许为换核停/起核。
        SwapInterrupt::Allowed,
    )
    .await;
    if resp.success {
        // 备份已被消费 → 删掉，避免 UI 出现「回滚到自己」的假选项。
        core_swap::prune_backup(base, os);
        // 陈旧的版本变更通知一并清（防回滚后还弹「已更新到 X」）。
        let _ = state
            .updater()
            .mutate_state(|s| s.pending_change_notice = None);
    }
    Ok(resp)
}

/// 上游 `core:replaceManual`：手动上传替换内核（**无网络**）。
///
/// ✅ **已接线**，零提权。两段式（对齐前端契约 `{ok:true}` / `{ok:false, needConfirm}`）：
///  1. 无 `filePath` → 弹文件选择器；
///  2. 经 [`decide_core_override`](polaris_updater::decide_core_override) 的同款判据预判
///     fork / 跨基线风险 → 需确认则返 `{ok:false, needConfirm:true, …}`，**不落位**；
///  3. `force=true` 或无风险 → 备份现役核 → 停核 → 原子替换 → 起核。
///
/// **跨带在此路径放行**（与自动更新相反）：手动换核是用户明确的一次性选择，上游 同语义。
#[tauri::command]
pub async fn core_replace_manual(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    file_path: Option<String>,
    force: Option<bool>,
) -> Result<ApiResponse<Value>, ()> {
    let base = match core_base_dir::<Value>() {
        Ok(b) => b,
        Err(resp) => return Ok(resp),
    };

    // 1. 取文件：前端没给就弹系统文件选择器。
    let path = match file_path.filter(|s| !s.trim().is_empty()) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            use tauri_plugin_dialog::DialogExt;
            let picked = app
                .dialog()
                .file()
                .set_title(crate::i18n::t(
                    crate::i18n::app_lang(&app),
                    crate::i18n::key::NATIVE_CORE_PICK_TITLE,
                ))
                .blocking_pick_file();
            let Some(f) = picked else {
                // 用户取消 = 正常流程，不是错误（**不返 error 信封**，否则 UI 会弹红）。
                return Ok(ApiResponse::ok(json!({ "ok": false, "cancelled": true })));
            };
            match f.into_path() {
                Ok(p) => p,
                Err(e) => return Ok(ApiResponse::err(format!("解析所选文件路径失败: {e}"))),
            }
        }
    };
    if !path.is_file() {
        return Ok(ApiResponse::err(format!(
            "所选内核文件不存在: {}",
            path.display()
        )));
    }

    let u = state.updater();
    let bundled = u.bundled_core_version().to_string();
    let previous = u.read_core_version();

    // 2. 预判（**不执行上传的二进制**：本机禁起核，且执行来路不明的二进制本身就是风险面）。
    //    判据来自文件名里的版本 token —— 探测不到就当 unknown，走「需确认」而非静默放行。
    let name_token = extract_version_token(&path.file_name().unwrap_or_default().to_string_lossy());
    let forced = force == Some(true);
    if !forced {
        let comparable = ComparableVersion::normalize(&name_token);
        let baseline_override = compare_semver(comparable.as_str(), &bundled)
            .map(|o| o < 0)
            .unwrap_or(true); // 解析不出 = 未知 → 保守要求确认
        if baseline_override {
            return Ok(ApiResponse::ok(json!({
                "ok": false,
                "needConfirm": true,
                "baselineOverride": true,
                "uploadVersion": name_token,
                "bundledVersion": bundled,
                "filePath": path.to_string_lossy(),
            })));
        }
    }

    // 3. 落位。version_line 用文件名解析出的 token；解析不出即空串，随后由
    //    `swap_core_with_restart` 按 `core_swap::marker_rewrite_line` 用**盘上实读**补齐
    //    （连实读也失败才落 unknown）。
    //
    //    ⚠️ 「保护用户手放的核」**不靠这里的空串**，靠 `SwapSource::Manual` 那条显式豁免
    //    （`core_paths::decide_reseed`）。旧写法把两件事塞进一个空串，真实形状是：文件名解析
    //    得出版本 ⇒ 写进簿记 ⇒ official 且旧 ⇒ **照样被覆盖**；解析不出 ⇒ 永不覆盖。
    //    同一个用户动作、结果取决于文件名长什么样，那是不一致而非失败安全。现在拆开：
    //    **簿记如实记版本**（诚实，诊断/UI 看到真话）+ **决策尊重来源**（尊重意图）。
    let bytes = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return Ok(ApiResponse::err("所选内核文件为空")),
        Err(e) => return Ok(ApiResponse::err(format!("读取内核文件失败: {e}"))),
    };
    Ok(swap_core_with_restart(
        &app,
        &state,
        base,
        &bytes,
        &name_token,
        core_swap::SwapSource::Manual,
        false,
        &previous,
        // 用户明确选了文件替换内核 → 允许为换核停/起核。
        SwapInterrupt::Allowed,
    )
    .await)
}

/// 上游 `core:getAutoStatus`：内核自动更新状态。
///
/// ✅ **已接线**：纯本地状态读取（`autoUpdateCore` / `lastCheckAt` / `staged` /
/// `crossBandNotifiedVersion`），零网络零探测。
///
/// `autoUpdateCore` 现返**真值**（此前硬返 `null`）：判据是
/// [`auto_update_core_enabled`](crate::runtime::core_update_scheduler::auto_update_core_enabled)
/// —— 与调度器守门用的**同一个**纯函数，UI 显示的开关态与后台实际是否会跑不可能对不上。
/// 读配置失败 → `false`（失败安全：宁可显示「关」，也不显示一个不会生效的「开」）。
#[tauri::command]
pub fn core_update_get_auto_status(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    let st = state.updater().state();
    let enabled = state
        .config()
        .load_full()
        .map(|c| crate::runtime::core_update_scheduler::auto_update_core_enabled(&c))
        .unwrap_or(false);
    ApiResponse::ok(json!({
        "autoUpdateCore": enabled,
        "lastCheckAt": st.last_check_at,
        "staged": serde_json::to_value(st.staged).unwrap_or(Value::Null),
        "crossBandNotifiedVersion": st.cross_band_notified_version,
    }))
}

/// 上游 `core:applyStaged`：用户点「立即应用」已暂存的内核。
///
/// ✅ **已接线**：无 staged → `"noop"`；有 staged → 版本闸 → 落位（走同一条停/起核编排）。
///
/// 落位结果沿用 `ApplyOutcome` 的五态词汇（`applied` / `discarded` / `deferred` / `failed` / `noop`），
/// 与前端 `coreUpdateApi.applyStaged()` 的联合类型逐字对齐 —— **不用 void/boolean 折叠**
/// （上游 修 M1 的原因：布尔返回把 discarded/deferred/failed 一律误报成「已应用」）。
///
/// 另有 `discarded` 的新成因 `reason:"integrity"`：暂存核的 sha256 复核不过（位腐 / 篡改）。
#[tauri::command]
pub async fn core_update_apply_staged(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    // 用户亲手点的「立即应用」→ 允许为换核停/起核。
    Ok(apply_staged_inner(&app, &state, SwapInterrupt::Allowed).await)
}

/// 自动路径的落位入口（**仅**供 `runtime/core_update_scheduler.rs` 调用）。
///
/// 与 [`core_update_apply_staged`] 唯一的差别是 [`SwapInterrupt::Forbidden`]：换核那一刻代理若在跑，
/// **不停核**、返 `deferred`、保留 staged 等下一个安全窗口。调度器自己那道 `running == false` 的
/// gating 只是省一次白跑，**不是**不变量的守卫 —— 守卫在 [`swap_core_with_restart`]
/// （见 [`swap_blocked_by_no_interrupt`]）。
pub(crate) async fn core_update_apply_staged_auto(
    app: &AppHandle,
    state: &AppRuntime,
) -> ApiResponse<Value> {
    apply_staged_inner(app, state, SwapInterrupt::Forbidden).await
}

/// [`core_update_apply_staged`] / [`core_update_apply_staged_auto`] 的共同本体。
async fn apply_staged_inner(
    app: &AppHandle,
    state: &AppRuntime,
    interrupt: SwapInterrupt,
) -> ApiResponse<Value> {
    let Some(staged) = state.updater().state().staged else {
        return ApiResponse::ok(json!({ "result": "noop" }));
    };
    let base = match core_base_dir::<Value>() {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let current = state.updater().read_core_version();

    // 版本闸：staged 不再领先当前 → 作废（= 上游 `compareVersions <= 0 → clearStaged → discarded`）。
    if compare_semver(&staged.version, &current)
        .map(|o| o <= 0)
        .unwrap_or(true)
    {
        discard_staged_dir(state, &staged.dir);
        return ApiResponse::ok(json!({ "result": "discarded" }));
    }

    let staged_dir = std::path::Path::new(&staged.dir);
    let staged_core = staged_dir.join(core_paths::core_filename());
    let bytes = match std::fs::read(&staged_core) {
        Ok(b) if !b.is_empty() => b,
        // 暂存核缺失/为空 → 作废清理（= 上游「staged 暂存核心缺失，清理」）。
        _ => {
            discard_staged_dir(state, &staged.dir);
            return ApiResponse::ok(json!({ "result": "discarded" }));
        }
    };

    // ── 完整性复核（stage → apply 之间可隔多日）：暂存时自算的**裸核** sha256 在这里对账。
    //    起核验证闩只能拦「起不来」，拦不住「起得来但行为坏」的位腐/篡改核。
    let recorded = std::fs::read_to_string(staged_core_sha_path(staged_dir)).ok();
    match check_staged_integrity(&bytes, recorded.as_deref()) {
        StagedIntegrity::Mismatch => {
            log::error!(
                "暂存内核完整性复核失败（位腐或被篡改），已作废不落位：{}",
                staged.dir
            );
            discard_staged_dir(state, &staged.dir);
            return ApiResponse::ok(json!({ "result": "discarded", "reason": "integrity" }));
        }
        StagedIntegrity::Unrecorded => {
            log::debug!("暂存内核无 sha256 记录（旧版本 App 暂存的）→ 跳过完整性复核");
        }
        StagedIntegrity::Ok => {}
    }

    let resp = swap_core_with_restart(
        app,
        state,
        base,
        &bytes,
        &staged.version,
        core_swap::SwapSource::Update,
        false,
        &current,
        interrupt,
    )
    .await;
    // `deferred`（不断流硬不变量拦下）信封是 success:true 但**一个字节都没换** → 绝不能清 staged。
    if swap_result_code(&resp) == Some("deferred") {
        return resp;
    }
    if resp.success {
        discard_staged_dir(state, &staged.dir);
        return resp;
    }
    // 落位失败：**保留 staged 待重试**（= 上游 failed 语义），并如实返 failed。
    ApiResponse::ok(json!({
        "result": "failed",
        "error": resp.error,
    }))
}

/// 作废 staged：清簿记 + 删暂存目录（三条作废路径共用，避免各写一份漏删）。
fn discard_staged_dir(state: &AppRuntime, dir: &str) {
    let _ = state.updater().mutate_state(|s| s.staged = None);
    let _ = std::fs::remove_dir_all(dir);
}

/// 暂存核的 sha256 旁挂文件：`<staged_dir>/<core_filename>.sha256`（内容 = 64 字符 hex）。
///
/// # 为什么是旁挂文件而不是 `StagedRecord` 字段
///
/// 旁挂文件与被它校验的字节**同目录、同生命周期**：`CoreStagedUpdater::stage` 每次都
/// `remove_dir_all(staged_dir)` 重建，故不可能出现「摘要还是旧的、核已被换成别的」这种错配；
/// 而写进 `update-state.json` 的字段与暂存目录是两份独立状态，一致性得自己维护。
///
/// 文件名收在此一处：生产端（`runtime/core_update_scheduler.rs` 暂存后写）与消费端
/// （[`apply_staged_inner`] 落位前读）共用，各拼一份必然漂移。
pub(crate) fn staged_core_sha_path(staged_dir: &Path) -> PathBuf {
    staged_dir.join(format!("{}.sha256", core_paths::core_filename()))
}

/// 暂存核完整性复核的三态结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedIntegrity {
    /// 没有摘要记录（旧版本 App 暂存的核）→ **放行**：拿不到基准不等于字节坏了，
    /// 「没记录就拒绝落位」会让跨版本升级的用户白丢一次已下好的核。
    Unrecorded,
    /// 复核通过。
    Ok,
    /// 与暂存时记录的摘要不符 → 位腐 / 被篡改 → 必须作废，绝不换入。
    Mismatch,
}

/// 纯判定：暂存核字节 vs 暂存时记录的 sha256。
///
/// 空串 / 全空白的记录按「没有记录」处理（旁挂文件写了一半不该把一个好核毙掉）。
/// 非法 hex（长度不对 / 非 hex 字符）走 [`verify_bytes`] 判成错误 → [`StagedIntegrity::Mismatch`]：
/// 那说明旁挂文件本身已损坏，与它同目录的核不值得信任。
#[must_use]
pub(crate) fn check_staged_integrity(bytes: &[u8], recorded_sha: Option<&str>) -> StagedIntegrity {
    let Some(sha) = recorded_sha.map(str::trim).filter(|s| !s.is_empty()) else {
        return StagedIntegrity::Unrecorded;
    };
    if verify_bytes(bytes, sha).is_ok() {
        StagedIntegrity::Ok
    } else {
        StagedIntegrity::Mismatch
    }
}

/// 上游 `core:ackVersionChange`：banner 展示版本变更通知后 ack 清除。
///
/// ✅ **已接线**：清 `pendingChangeNotice`（show→ack，弹一次非每启）。
///
/// 语义要点（= 上游 `ackPendingChangeNotice:1171-1175`）：这是「推送式一次性通知」的消费端，
/// 取代了旧的「last-known-version !== current」推断式判定（后者每次启动都重弹）。
#[tauri::command]
pub fn core_update_ack_version_change(state: State<'_, AppRuntime>) -> ApiResponse<()> {
    match state
        .updater()
        .mutate_state(|s| s.pending_change_notice = None)
    {
        Ok(()) => ok_void(),
        Err(e) => ApiResponse::err(e),
    }
}

/// 上游 `core-update:reset-factory`：把内核恢复为随 App 出厂的版本。
///
/// ✅ **已接线**：≡ `replaceManual({filePath: 随包核, force: true, skipBackup: true})` + `pruneBackup`。
///
/// # `skip_backup=true` 的语义（严格照 上游 `CoreUpdateService.ts:1272-1278, 1475-1484`）
///
/// 不备份、不 arm 验证闩、不自动回退，末尾清残留 `.bak`。理由：现役核正是用户**主动要丢弃**的那个，
/// 出厂核已知稳定。若照常备份，用户「重置到出厂」后 UI 会冒出一个「回滚到刚被丢弃的核」的选项，
/// 语义正好倒错。
#[tauri::command]
pub async fn core_reset_factory(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    let base = match core_base_dir::<Value>() {
        Ok(b) => b,
        Err(resp) => return Ok(resp),
    };
    // **随包出厂核**（绕过可写核优先级与 env 逃生门——要的正是「出厂那一份」）。
    let bundled_path = match core_paths::bundled_core_path() {
        Ok(p) => p,
        Err(e) => return Ok(ApiResponse::err(format!("未找到随包出厂内核: {e}"))),
    };
    let bytes = match std::fs::read(&bundled_path) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return Ok(ApiResponse::err("随包出厂内核文件为空（打包异常）")),
        Err(e) => return Ok(ApiResponse::err(format!("读取随包出厂内核失败: {e}"))),
    };
    let u = state.updater();
    let previous = u.read_core_version();
    let bundled_version = u.bundled_core_version().to_string();

    let resp = swap_core_with_restart(
        &app,
        &state,
        base,
        &bytes,
        &bundled_version,
        core_swap::SwapSource::ResetFactory,
        true, // skip_backup
        &previous,
        // 用户明确点了「重置为出厂内核」→ 允许为换核停/起核。
        SwapInterrupt::Allowed,
    )
    .await;
    // 末尾清残留（install_core_bytes 在 skip_backup 分支已 prune 一次；此处覆盖失败路径）。
    core_swap::prune_backup(base, std::env::consts::OS);
    Ok(resp)
}

/// 上游 `APP_UNINSTALL_ALL`：完全卸载（提权 helper / 受保护目录内核 / 用户配置 / 应用本体）。
///
/// ✅ **已接线**。本函数是**薄壳**：编排、顺序、失败传播、三平台可行性判定全在
/// [`crate::runtime::uninstall`] 的纯函数里（可穷举单测），这里只做注入 + 信封。
///
/// # 六步的因果序（详表见 [`crate::runtime::uninstall`] 模块文档）
///
/// 停核 → 取消开机自启 → 卸 helper（其 root 脚本一并清受保护目录中的内核）→ 删用户配置
/// → 删更新缓存 → 删应用本体。
///
/// 几处先后**不可换**：取消自启最便宜最可逆故排最前（失败时零损失，且它删的登录项在配置目录之外）；
/// `HelperRuntime::uninstall` 要往配置目录写提权脚本、从那里读 token，先删配置就等于把 helper
/// 永久焊死在系统里；应用本体必须最后 —— 它是当前进程的载体。
///
/// # 任一步失败即中止（红线）
///
/// [`run_uninstall`](crate::runtime::uninstall::run_uninstall) 做 fail-fast：失败步之后的每一项都记
/// `NotAttempted` 如实上报，绝不「反正已经删一半了不如删完」。
///
/// # 为什么外层仍是 `ApiResponse::ok`（而不是失败时返 `err`）
///
/// 与 [`helper_uninstall`](crate::commands::helper_uninstall) 同口径：**IPC 层不失败，业务结果在
/// 载荷里**。这里是刚需而非风格 —— 前端 `ipc-client.ts` 在 `success:false` 时 throw 且**只带
/// `error`/`code`，`data` 会被丢掉**；用失败信封就等于把「哪项成了、哪项没成、为什么」全扔了，
/// 而那恰恰是本功能必须逐项呈现的东西。真值收在
/// [`UninstallReport::verdict`](crate::runtime::uninstall::UninstallReport)：只有 `Complete` 才是
/// 「卸干净了」，前端据此选文案与配色 —— 半成品绝不会显示成「已卸载」。
/// 只有命令本身崩了（`spawn_blocking` join 失败）才走 `err`。
///
/// # 真机门
///
/// 卸 helper 那一步会弹提权框（osascript / pkexec / UAC）。本机无 bundled helper ⇒
/// `installed()` 为 false ⇒ 该步 `Skipped`，**不弹框**。
/// [`AutostartOps`](crate::runtime::uninstall::AutostartOps) 的生产实现：包一层插件的
/// `AutoLaunchManager`。**只做转接，不加判定** —— 判定在纯函数侧。
struct PluginAutostart(AppHandle);

impl uninstall::AutostartOps for PluginAutostart {
    fn is_enabled(&self) -> bool {
        self.0
            .state::<tauri_plugin_autostart::AutoLaunchManager>()
            .is_enabled()
            .unwrap_or(false)
    }
    fn disable(&self) -> Result<(), String> {
        self.0
            .state::<tauri_plugin_autostart::AutoLaunchManager>()
            .disable()
            .map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub async fn app_uninstall_all(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<UninstallReport>, ()> {
    let helper = state.helper.clone();
    let config_dir = state.config().dir().to_path_buf();
    // `app_cache_dir()/updates` = 应用更新包的唯一落点（见 `update_download`）。它在**配置目录之外**，
    // 删配置带不走 —— 漏掉就是卸载完还剩几百 MB 安装包。
    let cache_updates_dir = app
        .path()
        .app_cache_dir()
        .ok()
        .map(|d| d.join(uninstall::CACHE_UPDATES_LEAF));
    let autostart = PluginAutostart(app.clone());

    // 停核腿 + 全程看门狗与 `helper_uninstall` 共用同一层壳（见 `with_uninstall_core_guard`）。
    // 整条链（提权框 + 两次 remove_dir_all）跑在 spawn_blocking 上：提权框是分钟级原生模态，
    // 直调会把一个 tokio worker 占死。
    let joined = with_uninstall_core_guard(&state, |stop| async move {
        tokio::task::spawn_blocking(move || {
            let ops = uninstall::SystemUninstallOps {
                helper: helper.as_ref(),
                autostart: &autostart,
                os: std::env::consts::OS,
                config_dir,
                cache_updates_dir,
                // UserDefaults 域名 = 应用 identifier（`tauri.conf.json`，编译期常量）。
                // 与 `app_language::apply_process_language` 写入时用的是同一个来源，
                // 两处取不同的值就会「写进 A 域、清掉 B 域」。
                bundle_identifier: app.config().identifier.clone(),
                // 路径**全部由本进程自己算**，没有一段来自前端入参（本命令零参数）。
                exe: std::env::current_exe().ok(),
                appimage: std::env::var_os("APPIMAGE").map(PathBuf::from),
            };
            uninstall::run_uninstall(&ops, stop)
        })
        .await
    })
    .await;

    Ok(match joined {
        Ok(report) => {
            log::info!(
                "完全卸载结束：verdict={:?}，逐项 {:?}",
                report.verdict,
                report.steps
            );
            ApiResponse::ok(report)
        }
        Err(e) => ApiResponse::err(format!("完全卸载任务异常终止: {e}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本文件源码（调用点守卫共用）。
    const SRC: &str = include_str!("updater.rs");

    /// 独占的临时目录（无 tempfile dev-dep；用完即删）。
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "polaris-updater-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ── GitHub releases API 状态码 → 成因 ────────────────────────────────────

    /// 🟡 **变异锁：404 = 更新源仓库不可见，且必须仍是错误。**
    ///
    /// 真机成因（2026-07-28 Mac）：`update_check` 恒报「检查更新失败: GitHub API 返回错误: 404」。
    /// 实测 `https://api.github.com/repos/Sway-Chan/polaris/releases` 未鉴权 404，而
    /// `Sway-Chan/Polaris` 是 `private:true` 且零 release —— GitHub 对私有仓以 404 掩盖存在性。
    /// 裸状态码文案把这一成因与「URL 写错 / owner-repo 大小写错」混为一谈（排查时正是先怀疑了
    /// 后者，实测 `sagernet/SING-BOX` 直接 200 才证伪：GitHub 仓名大小写不敏感）。
    ///
    /// **变异探针**：删掉 `404 =>` 分支让它落回 `s =>` 兜底 ⇒ 第 2、3 条转红；
    /// 把 404 改成返回 `None`（伪装成 2xx ⇒ 前端显示「已是最新」）⇒ 第 1 条 `expect` 转红；
    /// 从文案里去掉 `{owner}/{repo}` ⇒ 第 3 条转红。
    #[test]
    fn github_404_reports_unreachable_repo_not_a_generic_status() {
        let e = github_status_error(404, "Sway-Chan", "polaris")
            .expect("404 必须是错误：更新通道整条不通，绝不能伪装成「无更新 / 已是最新」");
        assert!(e.contains("404"), "状态码要留在文案里供日志比对：{e}");
        assert!(
            e.contains("不存在") && e.contains("私有"),
            "releases **列表**端点上的 404 只意味着仓库不可见（零 release 是 `200 []`），\
             文案必须说准成因而不是甩一个裸状态码：{e}"
        );
        assert!(
            e.contains("Sway-Chan/polaris"),
            "app 腿与 core 腿共用同一个取数函数，不带 owner/repo 的日志分不清是哪条腿挂了：{e}"
        );
    }

    /// 🟡 **变异锁：2xx 放行 / 403 限流成因保留 / 其余状态码走兜底。**
    ///
    /// 403 与 404 的处置**相反**（前者等一会儿或配加速就好，后者是仓库根本不可见），合并成一条
    /// 文案会把用户引去做无用的重试。
    ///
    /// **变异探针**：把 `200..=299` 改窄成 `200` ⇒ 第 2 条转红；删 `403 =>` 分支 ⇒ 第 3 条转红；
    /// 把兜底 `s =>` 改成 `None` ⇒ 第 4 条 `expect` 转红。
    #[test]
    fn github_status_error_truth_table() {
        assert!(github_status_error(200, "o", "r").is_none(), "2xx 放行");
        assert!(
            github_status_error(204, "o", "r").is_none(),
            "放行的是整个 2xx 段（原实现即 `200..300`），不是只有 200"
        );
        let e403 = github_status_error(403, "o", "r").expect("403 必须是错误");
        assert!(
            e403.contains("频率限制"),
            "403 的处置是等/配加速，与 404 的「仓库不可见」不可合并：{e403}"
        );
        let e500 = github_status_error(500, "o", "r").expect("5xx 必须是错误");
        assert!(
            e500.contains("500"),
            "未特殊化的状态码走兜底并保留码值：{e500}"
        );
    }

    // ── #311：「查看更新日志」直达当前版本 release 页 ──────────────────────────

    /// 🟡 **变异锁：有版本号 → 拼 `/releases/tag/v<version>`，且不重复补 `v`。**
    ///
    /// **变异探针**：把 `format!` 里的 `v` 删掉 ⇒ 第 1 条转红；去掉
    /// `trim_start_matches('v')` ⇒ 第 2 条转红（拼出 `vv0.1.0`）。
    #[test]
    fn releases_url_for_version_targets_tag_page() {
        assert_eq!(
            releases_url_for(Some("v0.2.0")),
            "https://github.com/Sway-Chan/polaris/releases/tag/v0.2.0"
        );
        // 调用方将来若改传裸 semver（无 `v`），也不能拼错——两种输入形态幂等。
        assert_eq!(
            releases_url_for(Some("0.2.0")),
            "https://github.com/Sway-Chan/polaris/releases/tag/v0.2.0"
        );
    }

    /// 🟡 **变异锁：version 为空/仅空白/`None` → 回落泛列表页，绝不拼出可能 404 的链接。**
    ///
    /// **变异探针**：把 `filter(|v| !v.is_empty())` 删掉 ⇒ 第 2、3 条转红
    /// （会拼出 `.../tag/v`）。
    #[test]
    fn releases_url_for_missing_version_falls_back_to_list_page() {
        assert_eq!(
            releases_url_for(None),
            "https://github.com/Sway-Chan/polaris/releases"
        );
        assert_eq!(
            releases_url_for(Some("")),
            "https://github.com/Sway-Chan/polaris/releases"
        );
        assert_eq!(
            releases_url_for(Some("   ")),
            "https://github.com/Sway-Chan/polaris/releases"
        );
    }

    // ── 「绝不主动断流」硬不变量（H1）─────────────────────────────────────────

    /// 🟡 **变异锁：自动路径 + 代理在跑 ⇒ 必须拦下。**
    ///
    /// **变异探针**：把 [`swap_blocked_by_no_interrupt`] 改成恒 false / 只看 `interrupt` /
    /// 只看 `was_running` ⇒ 逐条转红。
    #[test]
    fn no_interrupt_invariant_truth_table() {
        assert!(
            swap_blocked_by_no_interrupt(SwapInterrupt::Forbidden, true),
            "自动路径遇到运行中的代理 → 必须放弃换核（绝不无同意断流）"
        );
        assert!(
            !swap_blocked_by_no_interrupt(SwapInterrupt::Forbidden, false),
            "代理没跑 → 自动路径照常落位（这是唯一的安全窗口，不能一并堵死）"
        );
        assert!(
            !swap_blocked_by_no_interrupt(SwapInterrupt::Allowed, true),
            "用户亲手点的换核 → 允许停/起核，否则「立即应用」永远应用不了"
        );
        assert!(!swap_blocked_by_no_interrupt(SwapInterrupt::Allowed, false));
    }

    /// 🟡 **调用点守卫：不断流判定必须夹在「读 `was_running`」与「`proxy.stop()`」之间。**
    ///
    /// 判定放在别处（比如只留在调度器里）就会重新撑开 TOCTOU 窗口：从调度器判 `running == false`
    /// 到这里真 stop，中间隔着读簿记 + 读几十 MB 暂存核 + sha 复核，用户点一下连接就断流了。
    ///
    /// **变异探针**：删掉 `swap_blocked_by_no_interrupt(` / 把它挪到 `proxy.stop()` 之后 /
    /// 在判定与 stop 之间插入一个 `.await` ⇒ 逐条转红。
    #[test]
    fn no_interrupt_check_precedes_any_proxy_stop() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn swap_core_with_restart(");
        let read_at = body
            .find("proxy.status().running")
            .expect("锚点消失：守卫已失去判据");
        let gate_at = body
            .find("swap_blocked_by_no_interrupt(")
            .expect("不断流硬不变量的校验被删了 —— 自动换核会在用户未同意时停掉正在跑的代理");
        // 找 `proxy.stop().await`（带 await）而非裸 `proxy.stop()`：后者在本函数的注释里也出现。
        let stop_at = body
            .find("proxy.stop().await")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            read_at < gate_at && gate_at < stop_at,
            "顺序必须是 读 was_running → 判定 → stop（实得 {read_at} / {gate_at} / {stop_at}）"
        );
        // 判定与 stop 之间不得有 await：有的话 `was_running` 就又是一张过期快照了。
        assert!(
            !body[gate_at..stop_at].contains(".await"),
            "判定与 proxy.stop() 之间出现了 await —— TOCTOU 窗口被重新撑开"
        );
    }

    /// 🟡 **不变量：空核绝不落位（落位 0 字节的核 = 直接 brick，起核必失败）。**
    ///
    /// 三条读字节的腿都必须在把字节交给 `swap_core_with_restart` 之前拒绝空文件。
    ///
    /// **为什么这道门长在这里**：此前这条不变量的唯一书面证据是
    /// `core_swap::install_core_from_file` 的单测 —— 而那个函数**没有任何生产调用点**
    /// （2026-08-09 全仓反查：定义 + 它自己两条单测，无第三处）。删它时若顺手把测试一并删掉，
    /// 不变量就一处都不剩了；故把门搬到**活路径**上。删函数不该连带删掉它守着的东西。
    ///
    /// 顺带补齐：回滚腿此前**没有**这道校验（`.bak` 由非空字节产出，空只会来自外部截断），
    /// 而后果与另两条腿完全相同。
    ///
    /// **变异探针**：任一腿的 `Ok(b) if !b.is_empty()` 退回 `Ok(b)` ⇒ 该腿转红。
    #[test]
    fn empty_core_bytes_never_reach_the_swap() {
        for leg in [
            "pub async fn core_replace_manual(",
            "pub async fn core_reset_factory(",
            "pub async fn core_rollback(",
        ] {
            let body = crate::commands::guard_scan::top_level_fn_body(SRC, leg);
            let guard_at = body.find("Ok(b) if !b.is_empty()").unwrap_or_else(|| {
                panic!("{leg} 少了空文件校验 —— 0 字节的核会被落位，起核必失败且旧核已被覆盖")
            });
            let swap_at = body
                .find("swap_core_with_restart(")
                .unwrap_or_else(|| panic!("{leg} 锚点消失：守卫已失去判据"));
            assert!(
                guard_at < swap_at,
                "{leg} 的空文件校验必须早于落位（实得 校验={guard_at} / 落位={swap_at}）"
            );
        }
    }

    /// 🟡 **调用点守卫：wire 契约对拍必须在换核路径上、且早于停核与落盘。**
    ///
    /// `verdict_for_core_bytes` 的单测测的是**判据本身**；判据再对，不被调用就什么也守不到 ——
    /// 而这条路径（在线换核 / 用户自带 fork）恰恰是 `build.rs` 那道 release 硬门够不着的一格
    /// （它只看 `resources/*/sing-box` 四条路径）。故此处立源码级守卫。
    ///
    /// 顺序判据不是形式主义：拦在 `proxy.stop()` 之前 ⇒ 拒绝时用户的代理毫发无损；
    /// 拦在 `install_core_bytes(` 之前 ⇒ 不会先把一份注定要拒的核落到盘上。
    ///
    /// **变异探针**：删掉调用 / 把它挪到 `proxy.stop().await` 之后 / 挪到 `install_core_bytes(`
    /// 之后 ⇒ 逐条转红。
    #[test]
    fn wire_contract_check_precedes_stop_and_install() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn swap_core_with_restart(");
        let check_at = body.find("verdict_for_core_bytes(core_bytes)").expect(
            "换核路径没有对拍 wire 契约 —— 非随包核的字段号漂移会让管理 API 整条流静默死掉",
        );
        let stop_at = body
            .find("proxy.stop().await")
            .expect("锚点消失：守卫已失去判据");
        let install_at = body
            .find("install_core_bytes(")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            check_at < stop_at && check_at < install_at,
            "对拍必须早于停核与落盘（实得 对拍={check_at} / stop={stop_at} / 落盘={install_at}）"
        );
        // 只有 Mismatch 一档拦。判据不能只查「两个分支名在不在」—— 那样把 Unobservable 分支体
        // 改成 return 也照样绿（写第一版时正是这个形态，变异编译不过才暴露出来）。
        // 故直接查 **Unobservable 分支体里不得有 return**。
        let unobs_at = body
            .find("WireVerdict::Unobservable(")
            .expect("Unobservable 分支没了 —— 取不到判据时会走进拒绝档");
        let mismatch_at = body
            .find("WireVerdict::Mismatch(")
            .expect("Mismatch 分支没了 —— 真正该拦的那一档没人拦");
        assert!(
            unobs_at < mismatch_at,
            "分支顺序变了，下面这段取不准 Unobservable 的分支体"
        );
        assert!(
            !body[unobs_at..mismatch_at].contains("return"),
            "Unobservable 分支里出现了 return —— 把「没观测到」当成「观测到有问题」，\
             一次读失败就会剥夺用户装自己那份核的能力"
        );
        // 回滚必须豁免：`core_rollback` 走的是同一条编排，在这里拦住等于把用户困在坏核上
        // （回滚的触发场景恰恰是「新核不可用」）。
        let exempt_at = body
            .find("SwapSource::Rollback")
            .expect("回滚豁免没了 —— 备份核若对不上号，用户将无路可退");
        assert!(
            exempt_at < check_at,
            "回滚豁免必须在对拍之前生效（实得 豁免={exempt_at} / 对拍={check_at}）"
        );
    }

    /// 🟡 **调用点守卫：换核成功后必须挂上稳定观察窗，且挂在起核之后。**
    ///
    /// 观察窗是 上游 `armPendingValidation` + `startStabilityWatch` 的对等物，补的是同步验证闩
    /// 看不见的那一类失败：新核**起得来**、几十秒后才崩。删掉 `arm_core_validation(` 这一行，
    /// 全仓没有任何其它测试会红 —— 判据（`core_validation`）的单测测的是判据本身，
    /// 而这条腿一旦不被调用，判据再对也不会被执行到。故此处立源码级守卫。
    ///
    /// **变异探针**：删掉 `arm_core_validation(` / 把它挪到 `proxy.start(` 之前 /
    /// 去掉 `swap.backed_up` 前置 ⇒ 逐条转红。
    #[test]
    fn stability_watch_is_armed_after_a_successful_restart() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn swap_core_with_restart(");
        let arm_at = body.find("arm_core_validation(").expect(
            "换核后的稳定观察窗没被挂上 —— 新核「起得来、几十秒后崩」将无人回滚，\
             备份原样躺在盘上，用户得自己发现并手动回滚",
        );
        // 起核在前：没起过核就没有「首次运行」可观察。
        let start_at = body.find("proxy.start(").expect("锚点消失：守卫已失去判据");
        assert!(
            start_at < arm_at,
            "观察窗必须挂在起核之后（实得 start={start_at} / arm={arm_at}）"
        );
        // 两个前置条件必须与 arm 同处一个判定：漏掉 backed_up 会在无备份时白挂一个
        // 「观察到失败也回滚不了」的窗口，且窗口内抑制了自愈重启 = 纯的负收益。
        let gate = &body[..arm_at];
        let cond_at = gate
            .rfind("if was_running && swap.backed_up")
            .expect("观察窗的前置条件（was_running + backed_up）被改了或删了");
        let between = gate[cond_at..].lines().count();
        assert!(
            between <= 3,
            "前置条件与 arm 调用之间隔了 {between} 行 —— 守卫已无法确认二者仍是同一个判定"
        );
    }

    /// 🟡 **调用点守卫：簿记回写必须接在验证闩之后，且喂的是「探测失败返空」那个读法。**
    ///
    /// 纯逻辑由 `core_swap::marker_rewrite_line` 的真值表锁；这里锁**接线**——
    /// 逻辑再对，没人调它就等于没修。三条各锁一个真实退化：
    ///  1. 回写整段被删 ⇒ 空簿记原样回归 ⇒ 从设置页更新过内核的机器被永久钉住（静默）。
    ///  2. 回写挪到验证闩**之前** ⇒ 闩内失败会 `rollback_core` 把盘上换回旧核，而此刻已按
    ///     「新核」的实读写过簿记 ⇒ 簿记记的是新版本、盘上是旧核，判据与实况反向。
    ///  3. 喂 `read_core_version()` 而非 `read_core_version_line()` ⇒ 探测失败时它**回落随包
    ///     基线**，于是「读不出版本」被伪装成「版本就是基线」写进簿记 ⇒ 后续升级判同版不播种。
    ///
    /// **变异探针**：删 `rewrite_marker_from_probe(` / 把它挪到 `if let Some(c) = config` 之前 /
    /// 把 `read_core_version_line()` 改成 `read_core_version()` ⇒ 逐条转红。
    #[test]
    fn marker_rewrite_is_wired_after_verify_latch_with_nonfallback_read() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn swap_core_with_restart(");
        let latch_at = body
            .find("proxy.start(c.clone()).await")
            .expect("锚点消失：守卫已失去判据（验证闩变形了）");
        let rewrite_at = body.find("rewrite_marker_from_probe(").expect(
            "换核后的簿记回写被删了 —— 声明值为空的两条主路径（core_update_run 前端传 \
             downloadUrl / core_rollback 传 \"\"）会写空簿记，这个核从此不再被随包基线重播种",
        );
        assert!(
            latch_at < rewrite_at,
            "簿记回写必须在验证闩之后（实得 latch={latch_at} / rewrite={rewrite_at}）：\
             闩内失败会回滚成旧核，闩前回写会把新核版本写到旧核的簿记上"
        );
        // 取材必须是「探测失败返空串」那个读法。`read_core_version(` 是 `read_core_version_line(`
        // 的前缀，故先把带 `_line` 的出现全部剔掉再找裸的，避免自己骗自己。
        let probe_seg = &body[..rewrite_at];
        assert!(
            probe_seg.contains("read_core_version_line()"),
            "簿记回写的取材必须是 read_core_version_line()（探测失败返空串）"
        );
        assert!(
            !probe_seg
                .replace("read_core_version_line()", "")
                .contains("read_core_version()"),
            "回写取材段出现了 read_core_version() —— 它探测失败会回落随包基线，\
             把「读不到」写成「就是基线」"
        );
    }

    /// 🟡 **调用点守卫：自动落位入口必须传 `Forbidden`，手动入口必须传 `Allowed`。**
    #[test]
    fn auto_entry_forbids_interruption_manual_entry_allows_it() {
        let auto = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub(crate) async fn core_update_apply_staged_auto(",
        );
        assert!(
            auto.contains("SwapInterrupt::Forbidden"),
            "自动落位入口必须是「绝不断流」档"
        );
        let manual = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn core_update_apply_staged(",
        );
        assert!(
            manual.contains("SwapInterrupt::Allowed"),
            "用户点「立即应用」必须允许停/起核，否则该按钮永远无效"
        );
    }

    /// 🟡 **调用点守卫：`deferred` 绝不清 staged。**
    ///
    /// `deferred` 的信封是 `success: true`（它是一次合法的「本轮不落位」，不是错误），
    /// 落在 `resp.success` 那条分支上就会把一个字节都没换的轮次当成功、把已下好的核删掉。
    ///
    /// **变异探针**：删掉 `swap_result_code(&resp) == Some("deferred")` 那段早退 /
    /// 把它挪到 `if resp.success` 之后 ⇒ 转红。
    #[test]
    fn deferred_outcome_never_clears_staged() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn apply_staged_inner(");
        let deferred_at = body
            .find(r#"swap_result_code(&resp) == Some("deferred")"#)
            .expect("deferred 分支被删了 —— 自动路径被拦下时会把已下好的 staged 核误删");
        let success_at = body
            .find("if resp.success {")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            deferred_at < success_at,
            "deferred 早退必须在 `resp.success` 分支**之前**（deferred 的信封正是 success:true）"
        );
    }

    // ── 暂存核完整性复核（L9）────────────────────────────────────────────────

    #[test]
    fn check_staged_integrity_truth_table() {
        let core = b"fake-sing-box";
        let good = polaris_updater::verify::sha256_hex(core);
        assert_eq!(
            check_staged_integrity(core, Some(&good)),
            StagedIntegrity::Ok
        );
        // 大小写不敏感（verify_bytes 的既有语义）。
        assert_eq!(
            check_staged_integrity(core, Some(&good.to_uppercase())),
            StagedIntegrity::Ok
        );
        // 字节被改（位腐 / 篡改）→ 必须拦。
        assert_eq!(
            check_staged_integrity(b"tampered", Some(&good)),
            StagedIntegrity::Mismatch
        );
        // 旁挂文件本身坏了（非法 hex）→ 同目录的核不可信 → 也拦。
        assert_eq!(
            check_staged_integrity(core, Some("not-a-hash")),
            StagedIntegrity::Mismatch
        );
        // 无记录 / 空记录（旧版本 App 暂存的核、或旁挂文件写了一半）→ 放行，不倒退。
        assert_eq!(
            check_staged_integrity(core, None),
            StagedIntegrity::Unrecorded
        );
        assert_eq!(
            check_staged_integrity(core, Some("  \n")),
            StagedIntegrity::Unrecorded
        );
    }

    /// 旁挂文件必须与被校验的核**同目录**（`stage` 重建目录时一起被清掉 ⇒ 不会错配）。
    #[test]
    fn staged_sha_sidecar_sits_next_to_the_core() {
        let dir = std::path::Path::new("/some/core-staged");
        let p = staged_core_sha_path(dir);
        assert_eq!(p.parent(), Some(dir), "摘要必须与核同目录");
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            format!("{}.sha256", core_paths::core_filename())
        );
    }

    /// 🟡 **调用点守卫：完整性复核必须发生在换核之前。**
    #[test]
    fn staged_integrity_is_checked_before_the_swap() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn apply_staged_inner(");
        let check_at = body.find("check_staged_integrity(").expect(
            "暂存核完整性复核被删了 —— 位腐/篡改的核会被原样换入（起核闩拦不住「起得来但行为坏」）",
        );
        let swap_at = body
            .find("swap_core_with_restart(")
            .expect("锚点消失：守卫已失去判据");
        assert!(check_at < swap_at, "复核必须在换核**之前**");
    }

    // ── 下载单飞（H2）───────────────────────────────────────────────────────

    /// 同一个 dest 拿到同一把闸；不同 dest 互不影响（否则两个不相干的下载会互相排队）。
    #[test]
    fn download_gate_is_keyed_by_destination() {
        let a = std::path::Path::new("/cache/updates/a.dmg");
        let b = std::path::Path::new("/cache/updates/b.dmg");
        assert!(Arc::ptr_eq(&download_gate(a), &download_gate(a)));
        assert!(
            !Arc::ptr_eq(&download_gate(a), &download_gate(b)),
            "不同目标文件必须各有各的闸"
        );
    }

    /// 单飞表只拥有在途锁：旧目标的最后一个强引用释放后，下次取锁必须驱逐旧键。
    #[test]
    fn download_gate_map_prunes_finished_destinations() {
        let mut map = DownloadGateMap::new();
        let first_path = std::path::Path::new("/cache/updates/old.dmg");
        let first = keyed_download_gate(&mut map, first_path);
        assert_eq!(map.len(), 1);
        assert!(Arc::ptr_eq(
            &first,
            &keyed_download_gate(&mut map, first_path)
        ));

        drop(first);
        let current_path = std::path::Path::new("/cache/updates/current.dmg");
        let current = keyed_download_gate(&mut map, current_path);
        assert_eq!(map.len(), 1, "已结束目标不应在进程内永久占位");
        assert!(!map.contains_key(first_path));
        assert!(Arc::ptr_eq(
            &current,
            &keyed_download_gate(&mut map, current_path)
        ));
    }

    /// 🟡 **变异锁：同一个 dest 同时只允许一条下载腿在临界区内。**
    ///
    /// 复现的是默认流程：`autoDownloadUpdate` 的后台下载腿 + 用户在 mini 弹窗点「更新」。
    /// **变异探针**：把 `download_gate(...).lock().await` 从 `update_download` 里拿掉
    /// （或让 [`download_gate`] 每次返回新 Arc）⇒ 本条转红。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_gate_serialises_concurrent_downloads_of_one_destination() {
        use std::sync::atomic::AtomicUsize;
        let dest = std::path::PathBuf::from("/cache/updates/concurrent.pkg");
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (dest, inside, peak) = (dest.clone(), inside.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                let gate = download_gate(&dest);
                let _permit = gate.lock().await;
                let n = inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                peak.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同一个 dest 同时有多条下载腿在写 —— 两个进度游标互相顶，且白下一份流量"
        );
    }

    /// 复用只认 sha256：有摘要且相符才复用；不符 / 无摘要 / 文件不在 → 老老实实重下。
    #[test]
    fn cached_download_is_reusable_only_with_a_matching_digest() {
        let dir = scratch("reuse");
        let dest = dir.join("update.pkg");
        std::fs::write(&dest, b"package-bytes").unwrap();
        let good = polaris_updater::verify::sha256_hex(b"package-bytes");

        assert!(cached_download_is_reusable(&dest, Some(&good)));
        assert!(
            !cached_download_is_reusable(&dest, Some(&"a".repeat(64))),
            "摘要不符必须重下（磁盘上那份可能是上次被截断的）"
        );
        assert!(
            !cached_download_is_reusable(&dest, None),
            "没有摘要判据就不能声称「磁盘上这份就是你要的包」——不拿文件名当身份"
        );
        assert!(
            !cached_download_is_reusable(&dest, Some("")),
            "空摘要等同没有摘要"
        );
        assert!(!cached_download_is_reusable(
            &dir.join("missing.pkg"),
            Some(&good)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── App 更新包：写入体积闸（D4）─────────────────────────────────────────

    /// 🟡 **`fileSize` 为 0 / 缺失时必须回落宽松上限，绝不能拿 0 当闸。**
    ///
    /// `AppUpdateInfo.file_size` 在 GitHub asset 缺 `size` 字段时按 **0** 填
    /// （`github.rs` 的 `#[serde(default)]`）。直接拿它当闸 ⇒ 闸值 = 0，
    /// **任何包都过不去**，且失败长得像「下载超限」，成因（清单少个字段）无从追起。
    ///
    /// **变异探针**：把 `declared.filter(|n| *n > 0)` 改回 `declared` ⇒ 后三条转红。
    #[test]
    fn app_update_size_limit_falls_back_when_the_declared_size_is_absent_or_zero() {
        // 声明值有效 → 闸就等于声明值（**无裕度**，2026-08-17 删）。
        //
        // 本条原是一对断言：`== declared + MARGIN` 与 `> declared`（「闸不得小于等于声明值本身」）。
        // 随裕度一并订正为**等值**：三处体积闸都是**严格大于**才拒（预检 `n > limit`、两条读侧
        // `已收 + 本 chunk > limit`），故 `limit == declared` 时一个恰好 `declared` 字节的包
        // **仍然过得去**。那条边界不是推断，由 `runtime::http` 的
        // `size_limit_boundary_admits_a_body_of_exactly_the_limit` 逐处拿等长响应体撞过。
        let declared = 40 * 1024 * 1024;
        assert_eq!(
            app_update_size_limit(Some(declared)),
            declared as usize,
            "有声明值时闸 = 声明值本身：等长包靠三处闸的严格大于语义放行，不靠裕度"
        );

        // 声明为 0（GitHub asset 缺 size 的真实形态）→ 回落宽松绝对上限。
        assert_eq!(
            app_update_size_limit(Some(0)),
            APP_UPDATE_MAX_BYTES as usize,
            "fileSize=0 是「清单没给」，不是「包是空的」——拿 0 当闸会拒掉一切"
        );
        // 字段整个缺失 → 同上。
        assert_eq!(app_update_size_limit(None), APP_UPDATE_MAX_BYTES as usize);
        // 回落上限必须容得下真实量级的安装包（几十 MiB）。
        assert!(
            app_update_size_limit(None) > 200 * 1024 * 1024,
            "回落上限卡得比真实安装包还紧 = 换了个姿势拒掉一切"
        );
    }

    /// 🟡 **声明分支必须有天花板：闸值绝不由服务端单方面顶到任意高。**
    ///
    /// 本条**推翻了**它的前身（`app_update_size_limit_saturates_instead_of_wrapping`）：那条断言
    /// `Some(u64::MAX) → usize::MAX`，即**把洞当成期望固化了下来** —— 只防「回绕成一个极小的闸」，
    /// 完全不防「闸大到形同不设」。而 `APP_UPDATE_MAX_BYTES` 的文档写的正是「别让一个撒谎的
    /// 服务端把盘写满」：`fileSize` 报 100 GiB ⇒ 闸值 100 GiB ⇒ Content-Length 预检放行 ⇒
    /// 一路写到 ENOSPC，用户的系统盘被写满。
    ///
    /// **变异探针**（2026-08-17 实测订正，原写「前三条逐条转红」是错的）：去掉
    /// `.min(APP_UPDATE_MAX_BYTES)` ⇒ **第 1 条**转红（断言在此中止，故一次运行只看得到这一条）；
    /// 单独摘掉第 1、2 条后重跑，**第 3、4 条仍绿** —— 第 3 条的声明值恰好等于天花板，`min`
    /// 在那一点是恒等映射，本就落不到差异上；第 4 条在天花板之下同理。
    /// 即本条真正的判据是**第 1、2 条**，第 3 条只钉 `min` 的等值分支不越界。
    #[test]
    fn app_update_size_limit_is_capped_by_the_absolute_ceiling() {
        // 极端声明值（前身守的那一半，保留）。裕度删掉后 `saturating_add` 也一并没了 ⇒
        // 「回绕成极小值」这条失效形态结构性不存在，只剩「顶到天上」要防。
        assert_eq!(
            app_update_size_limit(Some(u64::MAX)),
            APP_UPDATE_MAX_BYTES as usize,
            "u64::MAX 的声明值必须被压回绝对上限"
        );
        // 撒谎的服务端：100 GiB 的声明值必须被压回天花板。
        assert_eq!(
            app_update_size_limit(Some(100 * 1024 * 1024 * 1024)),
            APP_UPDATE_MAX_BYTES as usize,
            "声明值再大也不得把闸顶上去——那正是本闸要防的那件事"
        );
        // 边界：声明值恰好等于天花板 → 就取天花板（`min` 的等值分支）。
        assert_eq!(
            app_update_size_limit(Some(APP_UPDATE_MAX_BYTES)),
            APP_UPDATE_MAX_BYTES as usize
        );
        // 天花板之下的正常量级不受影响（封顶不得把正常包一起卡掉）。
        let normal = 40 * 1024 * 1024;
        assert_eq!(app_update_size_limit(Some(normal)), normal as usize);
    }

    // ── U2：打包体积门 ↔ 客户端写入闸的一致性（D5）───────────────────────────

    /// 打包体积门源码（`scripts/verify-packaging.mjs`），只为**逐字读出它那行常量**。
    const PACKAGING_GATE_JS: &str = include_str!("../../../scripts/verify-packaging.mjs");

    /// 打包体积门阈值在本仓的**第二份**（单位 MiB），真值在
    /// `scripts/verify-packaging.mjs` 的 `MAX_UPDATE_ASSET_BYTES`。
    ///
    /// D5 拍板的形态是「维护两份 + 一条一致性测试钉死」。两份不是冗余：Rust 侧这一份让本文件能
    /// 提出一条 JS 侧**提不出**的断言 —— 打包阈值必须待在 [`APP_UPDATE_MAX_BYTES`] 之下。
    ///
    /// 为什么不反过来「让 JS 去读 Rust 那份、只留一份」：本文件对 [`APP_UPDATE_MAX_BYTES`] 是
    /// **按符号引用**的（改名 / 改类型 / 删掉都在编译期红），而 JS 侧只能拿正则去扒 Rust 源码，
    /// 那种引用会**静默漂移**（正则扒不着就退化成「没变」）。把跨语言那一跳放在有编译器的这一侧，
    /// 漂移面才最小 —— 也正因为如此，下面这个解析函数必须把「扒不准」一律判成失败。
    const PACKAGING_MAX_UPDATE_ASSET_MIB: u64 = 96;

    /// 从打包门源码里读出 `MAX_UPDATE_ASSET_BYTES = N * 1024 * 1024` 的 `N`。
    ///
    /// 取不到就返回 `None` ⇒ 调用方**转红**。这是刻意的失败方向：判据取不到时若默认「没变」，
    /// 一次 JS 侧改写形态（比如换成裸字面量）就会让这条一致性测试**静默失效**，
    /// 而它看起来还是绿的 —— 那正是本测试要防的那类事。
    ///
    /// # 「出现多次」同样是不可判定，必须一并判成失败（2026-08-17 复审修）
    ///
    /// 原实现取**首个**匹配（`split_once`），于是「解析失败会红」防不住「解析到错的那一行」：
    /// 本仓的注释风格就是大段引用代码 —— 调门时在常量文档里补一行
    /// `const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;  // 旧值` 作沿革记录、同时把真值升到 256，
    /// 首个匹配读到注释里那个 96、与镜像相等 ⇒ **绿**，而两份常量已经漂开 160 MiB。
    /// 故先数命中数，`!= 1` 直接 `None`：与 `checkSha256Sums` 对同名资产的处置同一条纪律
    /// ——**不可判定就必须红，不能挑一个继续**。
    ///
    /// # 数的是「以 marker 开头的**行**」，不是子串出现次数（2026-08-17 二次复审修）
    ///
    /// 只数子串的话，「恰好 1 次、但那 1 次不是真声明」照样会被取值 —— 复审逐字复刻本函数跑了
    /// 14 组输入，三组假绿全长一个样：**真声明因换行/改名不再匹配（0 次）+ 注释里那处同形（1 次）
    /// = 合计 1 次**。最现实的一种是有人按更窄行宽把常量折成
    /// `= \n  256 * 1024 * 1024;`，而文件里某处注释还留着一句一行形态的引用 ⇒ 读到注释里的旧值 96、
    /// 与镜像相等 ⇒ 绿，两份实际已漂开 160 MiB。
    ///
    /// 按行判把两侧一起堵上：注释行（`//` / ` * ` 开头）结构上不以 marker 起头，不再参与计数；
    /// 真声明一旦折行就 0 命中 ⇒ `None` ⇒ 红（fail-closed，方向正确）。
    /// `trim_start_matches("export ")` 是给「将来这行改成 `export const`」留的等价形态，不是通配。
    fn packaging_gate_mib_from_js(src: &str) -> Option<u64> {
        const MARKER: &str = "const MAX_UPDATE_ASSET_BYTES = ";
        let hits: Vec<&str> = src
            .lines()
            .map(|l| l.trim_start().trim_start_matches("export "))
            .filter(|l| l.starts_with(MARKER))
            .collect();
        if hits.len() != 1 {
            return None;
        }
        hits[0]
            .strip_prefix(MARKER)?
            .split_once(';')?
            .0
            .trim()
            .strip_suffix(" * 1024 * 1024")?
            .trim()
            .parse()
            .ok()
    }

    /// 🟡 **解析函数对「扒不准」的三类输入都必须交白卷，而不是猜一个数出来。**
    ///
    /// 上一条测试的全部效力都建立在「`js` 这个数确实是 CI 上拦包的那个数」之上。本条把这个前提
    /// 本身钉住，覆盖三类：
    ///  - **形态变了**（裸字面量 / 改名 / 折行）⇒ `None`，调用方 `expect` 转红；
    ///  - **真声明出现多次**⇒ `None`（读哪一处不确定）；
    ///  - **注释里有同形文本**⇒ 不得被当成声明取值 —— 这一类是二次复审逐字复刻跑出来的假绿：
    ///    真声明折行后 0 命中、注释那处 1 命中，只数子串的实现合计得 1 ⇒ 静默读注释里的旧值。
    ///
    /// **变异探针**（2026-08-17 实测）：把命中判据换回 `src.matches(MARKER).count()` ⇒ 注释组
    /// ①②③ **三条各自都会转红**（断言在首条中止，故一次运行只看得到 ①）。三组在旧判据下逐字返回
    /// `Some(96)` —— 即注释里那份旧值被当成了真值，而真值其实是 256。
    /// 两条正向对照（唯一真声明 / `export` + 缩进）在新旧实现下都返回 `Some(96)`，
    /// 故本条不是靠「把什么都判红」换来的。
    #[test]
    fn packaging_gate_parser_refuses_ambiguous_or_reshaped_sources() {
        // 正向对照：唯一一处、形态如约 ⇒ 读得出（否则下面几条「读不出」毫无信息量）。
        assert_eq!(
            packaging_gate_mib_from_js("const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;\n"),
            Some(96)
        );
        // 形态变了 ⇒ 交白卷。
        assert_eq!(
            packaging_gate_mib_from_js("const MAX_UPDATE_ASSET_BYTES = 100663296;\n"),
            None,
            "裸字面量读不出，必须让调用方红"
        );
        assert_eq!(
            packaging_gate_mib_from_js("const MAX_UPDATE_ASSET_LIMIT = 96 * 1024 * 1024;\n"),
            None,
            "常量改名 = 判据消失"
        );
        // 真声明出现两次 ⇒ 读哪一处不确定，交白卷。
        assert_eq!(
            packaging_gate_mib_from_js(concat!(
                "const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;\n",
                "const MAX_UPDATE_ASSET_BYTES = 256 * 1024 * 1024;\n"
            )),
            None,
            "两条真声明 ⇒ 不可判定；挑首个会把「已漂开」判成绿"
        );
        // ── 注释里的同形文本不得被当成声明（只数子串的实现在这三组上全是假绿）──
        // ① 真声明折行 ⇒ 应 0 命中；注释那处不参与计数 ⇒ 合计 0 ⇒ None。
        assert_eq!(
            packaging_gate_mib_from_js(concat!(
                "// 沿革：const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;（旧值）\n",
                "const MAX_UPDATE_ASSET_BYTES =\n  256 * 1024 * 1024;\n"
            )),
            None,
            "真声明折行 ⇒ 判据取不到，绝不能回落去读注释里的旧值"
        );
        // ② 真声明改名 + 注释留旧形态。
        assert_eq!(
            packaging_gate_mib_from_js(concat!(
                " * 旧写法：const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;\n",
                "const MAX_UPDATE_ASSET_LIMIT = 256 * 1024 * 1024;\n"
            )),
            None,
            "块注释里的同形行同样不得顶替判据"
        );
        // ③ 真声明换裸字面量并改名 + 注释留旧形态。
        assert_eq!(
            packaging_gate_mib_from_js(concat!(
                "// const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;\n",
                "const ASSET_CEILING = 268435456;\n"
            )),
            None,
            "注释掉的旧行 + 换形态的新声明 ⇒ 必须交白卷"
        );
        // 反向对照：真声明前有缩进 / 将来改成 `export const` 仍读得出（本条不是「越严越好」，
        // 把等价形态一并判红会让门在无害改动上假红）。
        assert_eq!(
            packaging_gate_mib_from_js(
                "  export const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;\n"
            ),
            Some(96)
        );
    }

    /// 🟡 **打包体积门的两份常量不得漂开，且它必须待在客户端绝对写入闸之下。**
    ///
    /// 两条断言各防一件事：
    ///  - **漂开**：只改 JS 那一份 ⇒ 本条转红。没有它，「两份」就是两个各说各话的数；
    ///  - **越顶**：把打包门调到 [`APP_UPDATE_MAX_BYTES`] 之上 ⇒ 本条转红。那种配置下打包门会
    ///    放行一个**客户端结构性下不动**的包（写入闸在 512 MiB 处拒，用户侧表现为更新永远失败），
    ///    等于把 U1 那个缺陷换了个数量级重来一遍。
    ///
    /// 还有一条**下界**：门不得低于真实产物量级（实测最大 51.72 MiB，mac-arm64 dmg），
    /// 否则每次发布都假红 —— 一道恒红的门与一道没有的门，信息量一样是零。
    ///
    /// **变异探针**：把 `MAX_UPDATE_ASSET_BYTES` 改成 `128 * 1024 * 1024` 而不动这里 ⇒ 第 1 条转红；
    /// 改成 `1024 * 1024 * 1024` 并同步改这里 ⇒ 第 2 条转红；同步改成 `32` ⇒ 第 3 条转红。
    #[test]
    fn packaging_size_gate_is_mirrored_and_stays_under_the_client_write_gate() {
        let js = packaging_gate_mib_from_js(PACKAGING_GATE_JS).expect(
            "读不出 verify-packaging.mjs 的 MAX_UPDATE_ASSET_BYTES —— \
             判据取不到时必须红，不能默认「没变」（形态要求：`N * 1024 * 1024`）",
        );
        assert_eq!(
            js, PACKAGING_MAX_UPDATE_ASSET_MIB,
            "打包体积门的两份常量漂开了：JS 侧 {js} MiB / 本文件 {PACKAGING_MAX_UPDATE_ASSET_MIB} MiB。\
             D5 的形态是「维护两份 + 本条钉死」，改一处必须同步改另一处"
        );
        // 下面两条一律拿**从 JS 读出来的那个数**去判，不拿本文件的镜像：真正在 CI 上拦包的是
        // JS 那一份，镜像只是用来发现漂开。拿镜像判等于自己判自己（clippy 也会直接指出这是
        // 一条常量断言）。
        assert!(
            js * 1024 * 1024 < APP_UPDATE_MAX_BYTES,
            "打包门（{js} MiB）越过了客户端绝对写入闸（{} MiB）：\
             那样打包门会放行一个客户端根本下不动的包，用户侧表现为更新永远失败",
            APP_UPDATE_MAX_BYTES / 1024 / 1024
        );
        assert!(
            js >= 64,
            "打包门（{js} MiB）卡到了真实产物量级（实测最大 51.72 MiB）附近：\
             那会让每次发布都假红，与没有这道门等价"
        );
    }

    // ── App 更新包：期望摘要的来源（D1）─────────────────────────────────────

    /// 摘要来源按优先级挑；全无 → `Ok(None)`（**不拒装**，降级为弱校验并如实标记未校验）。
    #[test]
    fn expected_digest_is_resolved_by_source_priority_and_degrades_honestly() {
        let sha = "a".repeat(64);
        let got = resolve_expected_digest(&json!({ "sha256": sha }))
            .expect("合法字符串不该报错")
            .expect("应挑出 asset digest");
        assert_eq!(got.hex, sha);
        assert_eq!(got.source, DigestSource::GithubAssetDigest);
        assert_eq!(got.source.as_str(), "githubAssetDigest");

        // 无摘要 / 空串 / 纯空白 → Ok(None)（旧 release 的真实形态：`AppUpdateInfo.sha256` 带
        // `skip_serializing_if = "Option::is_none"`，故缺摘要时**字段整个不出现**）。
        assert_eq!(resolve_expected_digest(&json!({})), Ok(None));
        assert_eq!(resolve_expected_digest(&json!({ "sha256": "" })), Ok(None));
        assert_eq!(
            resolve_expected_digest(&json!({ "sha256": "   " })),
            Ok(None)
        );

        // 格式非法的摘要**不得**在此静默丢弃 —— 那会把「摘要写坏了」伪装成「本来就没摘要」而放行。
        // 它要一路走到校验步，才分得出「发布方写坏了」与「包被篡改」两种文案。
        let bad = resolve_expected_digest(&json!({ "sha256": "not-a-hash" }))
            .expect("非法 hex 是字符串，不在本函数报错")
            .expect("格式非法也要挑出来，交给校验步报错");
        assert_eq!(bad.hex, "not-a-hash");
    }

    /// 🟡 **字段在、但不是字符串 ⇒ 显式早退，绝不静默降级成「本来就没摘要」。**
    ///
    /// 原实现三种形态（`123` / `null` / `["…"]`）全走 `Value::as_str` → `None` → 与「字段缺失」
    /// 合流 → `verified:false` 放行。即一个把 `sha256` 序列化成数字的发布流程，会让**全体用户**
    /// 静默地少一道校验，而返回体上只显示 `verified:false`（长得和旧 release 一模一样）。
    /// 这与 [`resolve_expected_digest`] 自己文档写的「静默丢弃会把『摘要写坏了』伪装成
    /// 『本来就没摘要』而放行」正相反。
    ///
    /// **变异探针**：把非字符串分支改回 `continue`（或退回 `and_then(Value::as_str)`）⇒ 三条转红。
    #[test]
    fn a_non_string_digest_field_is_rejected_not_silently_dropped() {
        for bad in [json!(123), json!(null), json!(["a".repeat(64)]), json!({})] {
            let err = resolve_expected_digest(&json!({ "sha256": bad }))
                .expect_err("非字符串的 sha256 必须显式早退");
            assert!(err.contains("sha256"), "错误必须点名是哪个字段坏了：{err}");
        }
        // 反向对照：字段**整个缺失**仍是合法的「本来就没摘要」，不得被一起拒掉
        // （否则所有旧 release 立刻更新不了）。
        assert_eq!(resolve_expected_digest(&json!({ "other": 1 })), Ok(None));
    }

    /// 摘要来源表：**当前只有一级**，且每一级都必须自报字段名与对外标识。
    ///
    /// # 本条不再声称「加一级只改一处」（2026-08-16 订正）
    ///
    /// 前身叫 `digest_source_table_is_ordered_and_extensible`，配的文档说「U3 只需在表最前面
    /// 插一行」。那是**自相矛盾**的：本条自己就 `assert_eq!` 死了当前表，U3 必然要改它 ——
    /// 一个声称「只改一处」的断言，本身就是必然要改的第二处。真正的成因是
    /// [`DigestSource::field`] 把取法锁死成「`update_info` 顶层字符串字段」，而 `SHA256SUMS`
    /// 是另一次网络下载 + 按资产名查表（详见 [`DigestSource`] 文档）。
    ///
    /// 所以本条现在只钉两件**今天为真**的事：表是单元素的（不留假接线），以及每一级都自报身份。
    ///
    /// # U3 落地后本条**仍然为真**（2026-08-17）
    ///
    /// 前身预期「U3 落地时本条会被改」。实际落地的是「发布侧产出 `SHA256SUMS` + 门」，
    /// 消费侧经判断**不接**（三条依据见 [`resolve_expected_digest`] 文档末节），故表仍是单元素。
    /// 这条断言因此从「本批还没做」变成「做过判断后的现状」—— 将来真要接第 2 个来源时它才会红。
    #[test]
    fn digest_source_table_is_the_single_current_source() {
        assert_eq!(
            EXPECTED_DIGEST_SOURCES,
            [DigestSource::GithubAssetDigest],
            "消费侧只认 GitHub asset digest 一级（随包 SHA256SUMS 已产出但经判断不接，不留假接线）"
        );
        // 每个来源都必须自报字段名与标识（新增一级时忘了补 → 编译期就红）。
        for src in EXPECTED_DIGEST_SOURCES {
            assert!(!src.field().is_empty());
            assert!(!src.as_str().is_empty());
        }
    }

    /// 🟡 **清单声明的 `fileSize` 是等值判据（无摘要腿的主防线）。**
    ///
    /// 无摘要腿此前**只有** Content-Length 兜底，而那个数是撒谎方自己给的：服务端/镜像返
    /// `Content-Length: 1000` 且真发 1000 字节 ⇒ 完整性校验过 ⇒ 无摘要不校验 ⇒ 1000 字节的
    /// 假包被 promote，返 `{success:true, verified:false}`，UI 给出安装入口。
    /// 极端版 `Content-Length: 0` ⇒ 0 字节文件「下载成功」。
    ///
    /// `fileSize` 来自 GitHub release 清单，镜像改不动 —— 零成本堵上这条。
    ///
    /// **变异探针**：把 [`check_declared_size`] 改成恒 `Ok(())` ⇒ 第 2、3 条转红；
    /// 去掉 `.filter(|n| *n > 0)` ⇒ 第 4 条转红（缺 size 的旧 release 会被全部拒掉）。
    #[test]
    fn declared_file_size_is_enforced_as_an_equality_criterion() {
        use polaris_updater::traits::DownloadError;

        // 声明值匹配 → 放行。
        assert!(check_declared_size(1000, Some(1000)).is_ok());

        // 不匹配 → Incomplete（结构化，带两个数）。收得少（截断）与收得多（掉包/注入）都算。
        assert!(matches!(
            check_declared_size(1000, Some(52_000_000)),
            Err(DownloadError::Incomplete {
                received: 1000,
                expected: 52_000_000
            })
        ));
        assert!(
            check_declared_size(0, Some(52_000_000)).is_err(),
            "`Content-Length: 0` + 真发 0 字节的假包必须被这一级拦下"
        );
        assert!(check_declared_size(52_000_001, Some(52_000_000)).is_err());

        // 声明为 0 / 缺失 = 「清单没给」，**不判**（旧 release 的正常形态；判了就全拒）。
        assert!(check_declared_size(1000, Some(0)).is_ok());
        assert!(check_declared_size(1000, None).is_ok());
    }

    /// 🟡 **调用点守卫：单飞闸必须早于「发进度」与「真下载」。**
    ///
    /// 闸拿在后面 = 等待中的那条腿照样先发一发 `downloading(0)`，把已在跑的另一条腿的百分比顶回 0。
    #[test]
    fn update_download_takes_the_gate_before_emitting_or_downloading() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        let gate_at = body
            .find("download_gate(&dest)")
            .expect("下载单飞闸被删了 —— 后台自动下载与弹窗「更新」会同时写同一个 dest");
        let emit_at = body
            .find("emit(ProgressStage::Downloading {")
            .expect("锚点消失：守卫已失去判据");
        // 锚点随 U1（整包入内存 → 流式落盘）从 `download_with_progress(` 换成
        // `download_to_sink_with_progress(`：守的东西**一个字没变**（闸必须早于发进度与真下载），
        // 只是「真下载」那一句的名字变了。
        let dl_at = body
            .find("download_to_sink_with_progress(")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            gate_at < emit_at && gate_at < dl_at,
            "单飞闸必须在「发 downloading(0)」与「真下载」之前拿到（实得 {gate_at} / {emit_at} / {dl_at}）"
        );
    }

    /// 🟡 **调用点守卫：落位只许 rename，绝不许把刚流式写完的文件读回内存。**
    ///
    /// `atomic_replace` 吃 `&[u8]`。谁要是图省事把落位写回 `atomic_replace(&StdFs, &dest, &bytes)`，
    /// 就等于在流式下载之后又做一次「整包读回内存 + 整包重写」—— 本次改造的收益**当场归零**，
    /// 而且四条 gate 全绿、行为完全正确，只有真机上几十 MiB 的内存峰值会告诉你。
    /// 源码扫描是这条不变式**唯一**够得着的判据。
    ///
    /// **变异探针**：把 `promote_staged(...)` 换回 `atomic_replace(...)` ⇒ 两条断言同时转红。
    #[test]
    fn payload_is_promoted_by_rename_not_re_read_into_memory() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        assert!(
            body.contains("land_payload("),
            "落位必须走 `land_payload` → `verify::promote_staged`（tmp 已在盘 → 只 rename）"
        );
        // ── 「整包回内存」按**族**禁，不按单个字面量禁 ──────────────────────────
        //
        // 前身只禁 `atomic_replace(` 与 `verify_bytes(` 两个字面量，而被守的语义面远大于这两项：
        // 在落位前插一行 `let bytes = std::fs::read(&tmp)?;`（动机很自然 —— 落位前补一次大小复核）
        // 内存峰值就回到包体积、整个改造收益归零，而两条断言**全绿**。
        // 故改为禁掉「先把字节攒齐」这一族的全部入口。
        for banned in [
            "std::fs::read(",
            ".read(&tmp",
            "read_to_end(",
            "atomic_replace(",
            "verify_bytes(",
        ] {
            assert!(
                !body.contains(banned),
                "`{banned}` 属「整包入内存」族 —— 流式下载之后再把文件读回内存，本次改造的收益当场归零，\
                 而四条 gate 会全绿、行为完全正确，只有真机上几十 MiB 的内存峰值会告诉你"
            );
        }
    }

    /// 🟡 **调用点守卫：全程只碰 tmp，dest 只在最后一次 rename 时出现。**
    ///
    /// dest 一旦被中途写入（哪怕只是 `open_write(&dest)` 建个空文件），并发单飞的**后到者**
    /// 就会经 [`cached_download_is_reusable`] 读到一个半截包 —— 而它的判据是 sha256，
    /// 半截包会被判「不可复用」从而重下，看起来还挺正常；真正的坏形态是 dest 上留下一个
    /// 长度不对的文件，`update_install` 拿它去装。
    ///
    /// **变异探针**：把 sink 的目标从 tmp 改成 dest ⇒ 白名单计数对不上 ⇒ 转红。
    #[test]
    fn streaming_download_only_ever_touches_the_tmp_path() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        assert!(
            body.contains("verify::tmp_name(&dest)"),
            "tmp 必须由 `verify::tmp_name` 生成（同目录同卷 = 原子 rename 的前提）"
        );
        assert!(
            body.contains("open_write(&sink_path)")
                && body.contains("let sink_path = partial.path().to_path_buf()"),
            "写句柄必须指向 tmp（经 RAII 守卫持有的那条路径）"
        );

        // ── dest 侧改**白名单计数**（前身是单字面量黑名单 `!contains("open_write(&dest")`）──
        //
        // 黑名单挡不住同一语义的任何别的写法：`File::create(&dest)` / `StdFs.write(&dest, ..)` /
        // `open_write(dest.as_path())` 全都绕得过去，而后果一样 —— dest 上留下一个长度不对的文件，
        // `update_install` 拿它去装。故反过来：**列出 dest 的全部合法用法**，
        // 出现次数钉死；新增任何一处对 dest 的引用都必须显式改这张表，并在改的时候回答
        // 「它会不会在落位之前碰 dest」。
        const DEST_USES: [&str; 9] = [
            "let dest = dir.join(&file_name);",
            "let gate = download_gate(&dest);",
            "let dest = dest.clone();",
            "cached_download_is_reusable(&dest, sha.as_deref())",
            "dest.display()",
            "dest.to_string_lossy()",
            "verify::tmp_name(&dest)",
            "land_payload(&polaris_updater::traits::StdFs, partial.path(), &dest)",
            // 两处 `ProgressStage::Downloaded { path: &dest, .. }`（复用腿 / 落位成功腿）。
            // 逐条回答本白名单要求回答的那个问题：**都在落位之后**（复用腿的前提是
            // `cached_download_is_reusable` 已认下盘上那份完整包，落位腿在 `Landed` 臂内），
            // 且两处都只是**读**路径去拼事件载荷，一个字节都不写。
            "path: &dest,",
        ];
        /// dest 在 `update_download` 里的**总出现次数**（含上表每一项各自的出现次数之和）。
        const DEST_MENTIONS: usize = 13;
        let total = body.matches("dest").count();
        let covered: usize = DEST_USES
            .iter()
            .map(|p| body.matches(p).count() * p.matches("dest").count())
            .sum();
        assert_eq!(
            total, DEST_MENTIONS,
            "对 dest 的引用数变了（实得 {total}，钉死 {DEST_MENTIONS}）—— \
             新增/删除任何一处都必须显式改白名单，并复核它没有在落位之前碰 dest"
        );
        assert_eq!(
            covered, total,
            "出现了不在白名单里的 dest 用法（白名单覆盖 {covered} / 实得 {total}）—— \
             dest 必须从「不存在」瞬间变为「完整文件」，中途碰它就会让后到者读到半截包"
        );

        // ── `downloaded` 的位置：**降级为弱断言**（真正的门是运行时的
        //    `landing_reports_failure_and_leaves_dest_untouched_when_rename_fails`）。
        //
        // 文本下标比较表达不了「rename **成功**才发」：把落位失败的早退降级成「只 log 不早退」，
        // 文本序照样成立。故这里只钉一件文本层面**确实**表达得了的事：下载腿的 `downloaded`
        // 落在 `LandingOutcome::Landed` 那一支里。
        //
        // 注意 `downloaded` 在本函数里有**两个**合法产地：① 复用分支（文件已在盘且 sha256 复核过，
        // 此时根本没下载）② 本次下载落位成功之后。故不能拿首个匹配比大小 —— 那只会量到 ①。
        let landed_at = body
            .find("LandingOutcome::Landed =>")
            .expect("落位成功分支消失：守卫已失去判据");
        let download_began_at = body
            .find("emit(ProgressStage::Downloading {")
            .expect("锚点消失：守卫已失去判据");
        let downloaded_after_start: Vec<usize> = body
            .match_indices("ProgressStage::Downloaded {")
            .map(|(i, _)| i)
            .filter(|i| *i > download_began_at)
            .collect();
        assert!(
            !downloaded_after_start.is_empty(),
            "锚点消失：守卫已失去判据（下载腿一发 `downloaded` 都不发？）"
        );
        for at in downloaded_after_start {
            assert!(
                at > landed_at,
                "`downloaded` 必须落在 `LandingOutcome::Landed` 分支内（实得 Landed {landed_at} / downloaded {at}）"
            );
        }
    }

    /// 🟡 **调用点守卫：残件清理必须是**类型**（RAII），不是「数一数清理调用」。**
    ///
    /// # 前身（计数守卫）为什么是假的
    ///
    /// 它数「`ApiResponse::err(` 出现次数 == `discard_partial_download(&tmp)` 出现次数」。
    /// 三条独立反例，每条都能让「漏清理」照样全绿：
    ///  1. 把落位失败的早退降级成「只 log 不早退」⇒ 两侧计数**同减**，仍相等；
    ///  2. 给一条早退配**两次**清理即可把另一条漏掉的配平；
    ///  3. 它**不匹配** `ApiResponse::err_with_code(` ⇒ tmp 之后新增一条带 code 且漏清理的早退，
    ///     守卫全盲。
    ///
    /// 根因是它守错了对象：不变量是「控制流离开这个作用域时残件不在」——那是**作用域**的性质，
    /// 计数表达不了。换成 [`PartialDownload`] 之后，`?` / panic / 将来新增的任何早退都自动覆盖，
    /// 也就没有「配平」这回事。
    ///
    /// 本条因此只守**形态**（真正的行为门是运行时的
    /// [`partial_download_deletes_the_tmp_on_drop_and_keeps_it_after_disarm`]）：
    ///
    /// **变异探针**：删掉 `PartialDownload::new(` ⇒ 第 1 条转红；把 `partial.disarm()` 挪出
    /// `Landed` 分支（例如提到 match 之前，失败路径就不再清残件了）⇒ 第 3 条转红；
    /// 再引入一个手工清理函数 ⇒ 第 2 条转红。
    #[test]
    fn the_partial_tmp_is_owned_by_an_raii_guard_not_by_counted_cleanup_calls() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        assert!(
            body.contains("PartialDownload::new("),
            "残件必须由 RAII 守卫持有 —— 计数守卫守不住「离开作用域时残件不在」（三条反例见本测试文档）"
        );
        assert!(
            !body.contains("discard_partial_download("),
            "回到了手工清理 ⇒ 又要靠「每条早退都记得配一行」，而那正是被推翻的形态"
        );
        let disarms = body.matches("disarm()").count();
        assert_eq!(
            disarms, 1,
            "`disarm()` 只该有一处（落位成功那一支），实得 {disarms} 处 —— \
             多一处就意味着某条失败路径也把守卫解除了，残件从此无人清"
        );
        assert!(
            body.contains("LandingOutcome::Landed => {")
                && body[body
                    .find("LandingOutcome::Landed => {")
                    .expect("锚点消失：守卫已失去判据")..]
                    .contains("partial.disarm()"),
            "`disarm()` 必须在**落位成功**分支内：提到分支之外（哪怕只是 match 之前一行），\
             落位失败时残件就不再被清"
        );
    }

    /// 🟡 **调用点守卫：`update_download` 的每一条失败早退都必须先发 `error` 进度事件。**
    ///
    /// 弹窗被 `force progress(0)` 推进 Progress 后，只有 `error` / `downloaded` 能把它推出去；
    /// 静默 return 会让它永远转圈（只剩 Cancel 可点）。原实现的三条前置校验早退正是这样。
    ///
    /// 按**计数**锁而不是逐条锁：新增任何一条失败早退却忘了配一发 error 事件，
    /// 两个计数立刻对不上 ⇒ 转红。
    ///
    /// # 计数用**前缀** `ApiResponse::err`（2026-08-16 订正）
    ///
    /// 前身数的是 `ApiResponse::err(` —— 它**不匹配** `ApiResponse::err_with_code(`（`err` 后面是
    /// `_` 不是 `(`），于是新增一条带 code 的早退时守卫全盲。改成前缀后，代价是
    /// BackendUnavailable 那条分支（一个 `match` 里两个 `ApiResponse::err*` **共用同一发**
    /// error 事件）会被多数一次；把它作为**具名常数**扣掉，而不是把 `err_with_code` 排除在
    /// 计数之外 —— 后者等于把射程重新缩回去。
    #[test]
    fn every_failure_path_emits_an_error_progress_event() {
        /// 「一条早退里出现两个 `ApiResponse::err*`、但只发一发 error 事件」的已知分支数。
        /// 当前唯一一处：下载失败时按 `BackendUnavailable` 与否二选一造信封。
        const SHARED_ERR_BRANCHES: usize = 1;

        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        let errors = body.matches("ApiResponse::err").count();
        let emits = body.matches("emit(ProgressStage::Failed(").count();
        assert!(errors > 0, "锚点消失：守卫已失去判据");
        assert_eq!(
            errors,
            emits + SHARED_ERR_BRANCHES,
            "失败信封 {errors} 处、error 进度事件 {emits} 发（另计 {SHARED_ERR_BRANCHES} 处共用分支）\
             —— 对不上的那条会让弹窗永远转圈"
        );
        assert_eq!(
            body.matches("ApiResponse::err_with_code(").count(),
            SHARED_ERR_BRANCHES,
            "带 code 的失败早退数变了 → 请确认它也发了 error 进度事件，并更新 SHARED_ERR_BRANCHES"
        );
    }

    // ── 随行事实：态与它依赖的数据同帧同行 ──────────────────────────────────

    /// 🟡 **行为门：每一帧都带着它那个态所依赖的全部随行事实。**
    ///
    /// `update:progress` 走 `events::broadcast` fan-out 给**所有**窗口 ⇒ 把设置页推进
    /// downloading / downloaded / error 的路径大多**不是设置页发起的**（启动自动下载腿
    /// `startup_tasks::spawn_auto_download`、弹窗「更新·重试」腿 `update_popup_action`），
    /// 那几条腿上设置页拿不到任何 invoke 回包 ⇒ 这一帧是它**唯一**的事实来源。少一样就静默
    /// 少一样：已核实的三条后果是「重启并安装」哑键（无 `filePath`）、「重试」哑键
    /// （无 `updateInfo`）、卡片上的版本号与体积写的是上一次检查的另一个版本。
    ///
    /// # 判据由**类型**穷尽，不由夹具点名
    ///
    /// 「哪个变体该带哪些键」写在 `required` 的穷尽 `match` 里 ⇒ [`ProgressStage`] 新增变体
    /// **编译不过**，作者必须回到这里显式回答它带得起哪些事实。
    ///
    /// 「样本有没有覆盖到每个变体」则由**从源码派生的臂数**兜底，不是手写常数：前身写
    /// `assert_eq!(samples.len(), VARIANTS)`（两个手写数字互比），2026-08-17 复审实测 ——
    /// 加第 4 变体 + 补齐两处生产 match + **只**补一条 `required` 臂、不动样本 ⇒ 全量 4181 全绿，
    /// 那个变体的载荷一次都没跑过。现改为数 [`stage_facts`] 那个**编译器强制穷尽**的 match
    /// 有几条臂，臂数即变体数，两个数字都不再由人手维护。
    /// 每格还反向断言「白名单之外不得夹带」——两个方向合起来是**逐变体的集合相等**，
    /// 而不是「至少有这几个键」这种只挡得住删除的弱判据。
    ///
    /// 跨语言那一半（Rust 键集 ↔ TS `UpdateProgress` 字段集）由
    /// `ui/src/contracts/update-progress-payload.test.ts` 双向对拍，与本门正交。
    ///
    /// **变异探针**：删掉 `payload["filePath"] = …` ⇒ Downloaded 那格转红；把
    /// `"updateInfo": info` 换成 `Value::Null` ⇒ 三格同时转红；把 `Downloaded` 的百分比从
    /// 100 改成别的 ⇒ 转红；多写一个键 ⇒ 「夹带」那条转红。
    #[test]
    fn progress_frame_carries_the_facts_its_state_depends_on() {
        /// 每个变体的帧里**必须**存在、且不得多于此的键（穷尽 match ⇒ 新增变体即编译错误）。
        ///
        /// 逐字段解构（不写 `..`）：给已有变体加字段时这里也必须表态，否则「加变体被挡住了、
        /// 加字段没有」那个不对称会从生产代码搬到门里来。
        const fn required(stage: ProgressStage<'_>) -> &'static [&'static str] {
            match stage {
                ProgressStage::Downloading {
                    percentage: _,
                    received: _,
                } => &[
                    "status",
                    "percentage",
                    "message",
                    "updateInfo",
                    "receivedBytes",
                ],
                ProgressStage::Downloaded {
                    path: _,
                    verified: _,
                } => &[
                    "status",
                    "percentage",
                    "message",
                    "updateInfo",
                    "filePath",
                    "verified",
                ],
                ProgressStage::Failed(_) => {
                    &["status", "percentage", "message", "updateInfo", "error"]
                }
            }
        }

        /// [`ProgressStage`] 的变体数，**从源码派生**：[`stage_facts`] 的 match 由编译器强制
        /// 穷尽 ⇒ 它有几条臂就有几个变体。数 `ProgressStage::` 而不数 `=>`：签名里的
        /// `ProgressStage<'a>` 不含冒号，不会被误计。
        ///
        /// 射程边界（如实登记，两条）：
        ///  1. 臂头若写成 or-pattern（`A::X { .. } | A::Y { .. } =>`），本计数会**高于**臂数
        ///     ⇒ 断言转红（安全方向：宁可误红也不放过没样本的变体）。
        ///  2. **给 `stage_facts` 与 `required` 同时补一条 `_ => …` 通配臂时，本门静默失效**：
        ///     计数停在通配那一刻的臂数，而 `match` 也不再被编译器强制穷尽 ⇒ 新变体既不编译红、
        ///     也不被本门抓到。今天两处都是穷举、无通配臂，故不可达；谁要加通配臂，请连同本门
        ///     一起重新设计判据（通配臂本身就是「新变体不必表态」的宣言）。
        fn variant_count() -> usize {
            let body =
                crate::commands::guard_scan::top_level_fn_body(SRC, "const fn stage_facts<'a>(");
            let n = body.matches("ProgressStage::").count();
            assert!(
                n >= 3,
                "锚点消失：`stage_facts` 的 match 一条臂都没解析到 —— 本门已失去变体数的真值源"
            );
            n
        }

        let path = std::path::Path::new("/tmp/updates/polaris.dmg");
        let info = json!({
            "version": "v1.2.0",
            "fileSize": 52_000_000_u64,
            "isPrerelease": true,
            // 两个**必须被剥掉**的字段：样本里没有它们，下面那条「剥掉了」的断言就无信息量。
            "releaseNotes": "## v1.2.0\n- 大段更新说明……",
            "title": "Polaris v1.2.0",
            // 契约将来加字段时，这一格证明其余字段是**原样**带过去的，不是被逐字段抄了一遍。
            "futureField": "kept",
        });
        // 期望的投影：从样本清单里**逐个删掉**登记为剥除的键，其余一个不动。
        // 表驱动 ⇒ 剥除表加一项时，本断言的两个方向（该没的没了、该在的逐字还在）自动跟着长。
        let expected_manifest = {
            let mut m = info.as_object().expect("样本清单必须是对象").clone();
            for key in PROGRESS_MANIFEST_OMITTED {
                assert!(
                    m.remove(key).is_some(),
                    "样本清单里没有 `{key}` ⇒ 「它被剥掉了」这条断言无信息量（假绿）"
                );
            }
            Value::Object(m)
        };

        let samples = [
            ProgressStage::Downloading {
                percentage: 37,
                received: 19_240_000,
            },
            ProgressStage::Downloaded {
                path,
                verified: true,
            },
            ProgressStage::Failed("下载更新包失败"),
        ];
        let variants = variant_count();
        assert_eq!(
            samples.len(),
            variants,
            "样本表没跟上变体数（`stage_facts` 有 {variants} 条臂）—— 新增的那个变体的载荷一次都没跑过"
        );
        let tags: std::collections::BTreeSet<&str> =
            samples.iter().map(|s| stage_facts(*s).0).collect();
        assert_eq!(
            tags.len(),
            samples.len(),
            "样本里有重复变体 —— 有一格根本没被测到"
        );

        for stage in samples {
            let (status, percentage, message) = stage_facts(stage);
            let payload = progress_payload(&info, stage);
            let obj = payload.as_object().expect("载荷必须是 JSON 对象");
            for key in required(stage) {
                assert!(obj.contains_key(*key), "{status} 帧缺随行事实 `{key}`");
                assert!(!obj[*key].is_null(), "{status} 帧的 `{key}` 是 null");
            }
            let extra: Vec<&String> = obj
                .keys()
                .filter(|k| !required(stage).contains(&k.as_str()))
                .collect();
            assert!(extra.is_empty(), "{status} 帧夹带了未登记的键: {extra:?}");
            assert_eq!(obj["status"], json!(status));
            assert_eq!(obj["percentage"], json!(percentage));
            assert_eq!(obj["message"], json!(message));
            // 「剥掉的真没了」——先单独判，失败消息才指得出是哪个字段。
            for key in PROGRESS_MANIFEST_OMITTED {
                assert!(
                    obj["updateInfo"].get(key).is_none(),
                    "{status} 帧仍带着 `{key}` —— 它无上限且 progress 可达态一律不渲染，\
                     每帧广播给所有窗口是纯粹的主线程开销"
                );
            }
            // 「该在的逐字还在」——**整对象相等**，不是「有几个字段」：少带一个字段就是
            // 「重试」重新变哑键 / 卡片又开始显示别的版本，正是本批立项要修的那三条。
            assert_eq!(
                obj["updateInfo"], expected_manifest,
                "{status} 帧的清单投影与「原样减去剥除表」不符 —— 卡片会拿它渲染版本号/体积/档次，\
                 「重试」也要拿它重下"
            );
        }

        // 逐值对账（上面只管「在不在」，这里管「是不是那个数」）。
        let landed = progress_payload(
            &info,
            ProgressStage::Downloaded {
                path,
                verified: false,
            },
        );
        assert_eq!(landed["filePath"], json!(path.to_string_lossy()));
        assert_eq!(landed["verified"], json!(false));
        assert_eq!(landed["percentage"], json!(100), "落位帧的百分比由类型定死");
        let mid = progress_payload(
            &info,
            ProgressStage::Downloading {
                percentage: 37,
                received: 19_240_000,
            },
        );
        assert_eq!(
            mid["receivedBytes"],
            json!(19_240_000),
            "已收字节必须是回调原值 —— 从百分比反推的数每一帧都是错的"
        );
        let failed = progress_payload(&info, ProgressStage::Failed("下载更新包失败"));
        assert_eq!(failed["error"], json!("下载更新包失败"));
        assert_eq!(failed["message"], failed["error"], "两个字段在此同源同值");
        assert_eq!(failed["percentage"], json!(0), "失败帧的百分比由类型定死");
    }

    /// 🟡 **调用点守卫：本次下载的随行事实只由一处附着。**
    ///
    /// `update_download` 里有十余处发进度的地方（每条失败早退各一发 + 三处正常帧）。若每处
    /// 各自调 `emit_progress(&app, <某个清单>, …)`，失守形态有二：漏传（编译红，无所谓）与
    /// **传成另一个对象**（编译绿、gate 全绿，而设置页显示的版本号是别的包的）。故本函数体内
    /// `emit_progress(` 只许出现一次 —— 就是那条把 `update_info` 绑死的闭包定义本身。
    ///
    /// 中间帧（下载回调）跑在另一个线程上、借不了栈，故它经 `download_progress_emitter` 按值
    /// 收清单；本门连带钉住它收的是**同一份** `update_info`。
    ///
    /// **变异探针**：把任一处 `emit(ProgressStage::Failed(&msg))` 改回
    /// `emit_progress(&app, &Value::Null, ProgressStage::Failed(&msg))` ⇒ 计数变 2 ⇒ 转红；
    /// 把 `download_progress_emitter(&app, update_info.clone())` 的第二个实参换成
    /// `Value::Null` ⇒ 转红。
    #[test]
    fn every_progress_frame_of_this_download_carries_this_downloads_manifest() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        assert!(
            body.contains(
                "let emit = |stage: ProgressStage<'_>| emit_progress(&app, &update_info, stage);"
            ),
            "单一发事件入口没了 —— 十余个调用点各自带清单，迟早有一处带成另一个版本的"
        );
        let direct = body.matches("emit_progress(").count();
        assert_eq!(
            direct, 1,
            "`emit_progress(` 在本函数里出现 {direct} 次（只该是那条闭包定义）—— \
             绕过 `emit` 直发就等于给这一处附着的清单开了个后门"
        );
        // 正向对照：三种帧确实都还在发（否则上面那条在「一发都不发」时也绿）。
        for anchor in [
            "emit(ProgressStage::Downloading {",
            "emit(ProgressStage::Downloaded {",
            "emit(ProgressStage::Failed(",
        ] {
            assert!(
                body.contains(anchor),
                "锚点消失：守卫已失去判据（{anchor}）"
            );
        }
        assert!(
            body.contains("download_progress_emitter(&app, update_info.clone())"),
            "中间帧的清单必须是**同一份** `update_info` —— 另造一个对象会让下载中卡片的\
             版本号与首尾两帧不符"
        );
    }

    // ── 残件所有权：RAII（H1）───────────────────────────────────────────────

    /// 🟡 **运行时门：drop 即删；`disarm` 之后 drop 什么都不删。**
    ///
    /// 这是把「失败即清理」从**源码计数**换成**类型**之后，真正有牙的那条门 ——
    /// 它测的是行为（离开作用域时文件在不在），不是「代码里写没写那一行」。
    ///
    /// **变异探针**：删掉 `impl Drop for PartialDownload` ⇒ 第 1 条转红；
    /// 把 `disarm` 改成不置 `None`（或直接删掉这个方法体的赋值）⇒ 第 2 条转红
    /// （落位成功后会把刚下好的包删掉）。
    #[test]
    fn partial_download_deletes_the_tmp_on_drop_and_keeps_it_after_disarm() {
        let dir = scratch("partial-raii");

        // ① 未解除 → drop 即删（= 每一条失败早退）。
        let armed = dir.join("armed.pkg.polaris-new-1-0");
        std::fs::write(&armed, b"half-written").unwrap();
        {
            let guard = PartialDownload::new(armed.clone());
            assert_eq!(guard.path(), armed, "守卫必须原样交出它持有的那条路径");
            assert!(armed.is_file(), "守卫存活期间不得提前删");
        }
        assert!(
            !armed.exists(),
            "守卫 drop 后残件必须消失 —— 否则每次下载失败都在缓存目录里攒一个几十 MiB 的垃圾"
        );

        // ② 已解除 → drop 什么都不做（= 落位成功，那个 inode 现在叫 dest 了）。
        let landed = dir.join("landed.pkg");
        std::fs::write(&landed, b"complete").unwrap();
        PartialDownload::new(landed.clone()).disarm();
        assert!(
            landed.is_file(),
            "disarm 之后 drop 绝不能删 —— 那删掉的是刚落位成功的更新包本身"
        );
        assert_eq!(std::fs::read(&landed).unwrap(), b"complete");

        // ③ 文件本来就不存在（「还没来得及建就失败」）→ drop 不得 panic。
        drop(PartialDownload::new(dir.join("never-created")));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── 落位：可注入的运行时判据（H2）──────────────────────────────────────

    /// 🟡 **运行时门：rename 失败 ⇒ `Failed` 且 dest 不存在；成功 ⇒ `Landed` 且 tmp 消失。**
    ///
    /// 这条替掉的是一条**表达不了自己声称之事**的文本守卫（`promote_at < downloaded_at` 的下标
    /// 比较）：把落位失败的早退降级成「只 log 不早退」，文本序照样成立，而运行时会广播
    /// `downloaded(100)` 外加一个根本不存在的 `filePath`。
    ///
    /// [`land_payload`] 吃 `&dyn UpdateFs` ⇒ 可注入 `MockFailOp::Rename`，
    /// 于是「rename 成功才算落位」变成一条真跑得起来的断言。
    ///
    /// **变异探针**：把 [`land_payload`] 的 `Err(e) =>` 改成 `Ok(())` 一样返回
    /// `LandingOutcome::Landed`（即「只 log 不早退」那个降级）⇒ 第 1 条转红。
    #[test]
    fn landing_reports_failure_and_leaves_dest_untouched_when_rename_fails() {
        use polaris_updater::traits::{MockFailOp, MockFs, StdFs};

        let dir = scratch("landing");

        // ① rename 失败 → Failed(_)，且 dest **不存在**（绝不广播一个不存在的 filePath）。
        let dest_fail = dir.join("fail.pkg");
        let tmp_fail = polaris_updater::verify::tmp_name(&dest_fail);
        std::fs::write(&tmp_fail, b"streamed").unwrap();
        let mut fs = MockFs::new(&dir);
        fs.fail_next(MockFailOp::Rename);
        let outcome = land_payload(&fs, &tmp_fail, &dest_fail);
        assert!(
            matches!(outcome, LandingOutcome::Failed(_)),
            "rename 失败必须报 Failed，实得 {outcome:?}"
        );
        assert!(
            !dest_fail.exists(),
            "落位失败时 dest 必须仍不存在 —— 否则 `update_install` 会拿一个半截文件去装"
        );
        if let LandingOutcome::Failed(msg) = outcome {
            assert!(
                msg.contains("写入更新包失败"),
                "失败文案须与同文件其它早退一致：{msg}"
            );
        }

        // ② 成功 → Landed，dest 是完整内容，tmp 消失。
        let dest_ok = dir.join("ok.pkg");
        let tmp_ok = polaris_updater::verify::tmp_name(&dest_ok);
        std::fs::write(&tmp_ok, b"streamed-bytes").unwrap();
        assert_eq!(
            land_payload(&StdFs, &tmp_ok, &dest_ok),
            LandingOutcome::Landed
        );
        assert_eq!(std::fs::read(&dest_ok).unwrap(), b"streamed-bytes");
        assert!(!tmp_ok.exists(), "落位成功后 tmp 必须已被 rename 掉");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 🟡 **运行时门：rename **之前**必须先 `fsync`；刷盘失败 ⇒ `Failed` 且 dest 不存在。**
    ///
    /// 没有这一步时，rename 只保证目录项的原子替换，不保证那个 inode 的**数据**已离开 page
    /// cache ⇒ 断电/内核崩溃后可能出现「dest 名字在、内容是零或半截」，而 dest 一旦存在
    /// [`update_install`] 就会**直接拿去装**。
    ///
    /// **变异探针**（2026-08-17 逐条实测，两条各红一处、互不重叠）：
    ///  - 删掉 `land_payload` 里的 `sync_file` 调用（或把失败改成「只 log 不早退」）
    ///    ⇒ **第 1 条**转红（注入的刷盘失败无处可发，结论成了 `Landed`）；
    ///  - 把 `sync_file` 挪到 `promote_staged` **之后** ⇒ **第 2 条**转红（结论仍是 `Failed`，
    ///    但 rename 已经发生 ⇒ dest 已存在）。顺序因此也在射程内，不是只靠注释声明。
    ///
    /// **不是探针、是运行期后果**（本仓 CI 只跑 Linux/macOS/Windows 的 64 位构建，但**没有**
    /// 任何门会因它转红，故不列在上面）：把
    /// [`StdFs::sync_file`](polaris_updater::traits::StdFs) 的 `OpenOptions::write(true)`
    /// 改成 `File::open`（只读句柄），Linux 上照常绿，**Windows 上落位会全线失败** ——
    /// `FlushFileBuffers` 对只读句柄直接报错。这条只能靠 trait 文档里的那句约束守住。
    #[test]
    fn landing_fsyncs_the_payload_before_renaming_it_into_place() {
        use polaris_updater::traits::{MockFailOp, MockFs};

        let dir = scratch("landing-fsync");
        let dest = dir.join("synced.pkg");
        let tmp = polaris_updater::verify::tmp_name(&dest);
        std::fs::write(&tmp, b"streamed").unwrap();

        let mut fs = MockFs::new(&dir);
        fs.fail_next(MockFailOp::SyncFile);
        // 残件的清理归 RAII 守卫（`land_payload` 自己不删）——照生产的形态过一遍：
        // 失败分支**不** disarm，守卫在离开作用域时把 tmp 收掉。
        let outcome = {
            let partial = PartialDownload::new(tmp.clone());
            land_payload(&fs, partial.path(), &dest)
        };

        assert!(
            matches!(outcome, LandingOutcome::Failed(_)),
            "刷盘失败必须报 Failed（内容可能还在 page cache 里），实得 {outcome:?}"
        );
        assert!(
            !dest.exists(),
            "刷盘失败时绝不能已经 rename —— dest 一旦存在就会被 `update_install` 当成完整包"
        );
        assert!(
            !tmp.exists(),
            "失败路径上的残件必须由 RAII 守卫收掉（`land_payload` 不负责删）"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── 孤儿 tmp 清扫（H3）──────────────────────────────────────────────────

    /// 把文件的 mtime 往前拨 `age`（测跨进程残件的 24h 阈值；不引第三方 crate）。
    fn backdate(path: &std::path::Path, age: std::time::Duration) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - age).unwrap();
    }

    /// 🟡 **运行时门：本资产 + 本 pid 的孤儿直接收；其余一律只按 mtime 阈值收；无关文件不碰。**
    ///
    /// 主触发器是「下载途中退出 App」：`update_download` 是 async command，tmp 建立后唯一的
    /// await 点是 `spawn_blocking(...).await`，退出 App 会让 runtime **drop 掉那个 future** ⇒
    /// RAII 守卫连同三条早退全被绕过，而 blocking 线程不可取消、可能把 tmp 写完。开着
    /// `autoDownloadUpdate` 时每个「启动 → 后台下载 → 提前关 App」周期必留一个几十 MiB 的孤儿，
    /// 而唯一的回收点是完全卸载。
    ///
    /// # 第 ⑤ 条已**反向**（2026-08-17）
    ///
    /// 它原先钉的是「别的资产的残件不归本次清扫管」—— 而那正是缺陷本身：App 更新的资产名带版本号，
    /// 版本一换前缀就变，于是上面那个主触发器留下的残件（必然是**旧版本名**）永远收不回来。
    /// 现在钉的是「别资产残件：新鲜保留、陈旧收」，并由第 ⑥ 条做真实缺陷回放。
    ///
    /// **变异探针**（2026-08-17 逐条实测，末条原写「去掉它 ⇒ 第 4 条转红」是错的）：
    /// 把本资产+本 pid 那一档也改成走 mtime 阈值 ⇒ 第 1 条转红；
    /// 把匹配面缩回 `{file_name}.polaris-new-` ⇒ 第 6 条转红（旧版本名的陈旧残件又收不回来了）；
    /// 把即时档改成只看 pid、不看资产名 ⇒ 第 5 条转红；
    /// 把 `>= ORPHAN_TMP_MAX_AGE` 改成 `<` ⇒ 第 2、3、5、6 条同时转红。
    ///
    /// `is_orphan_tmp_name` 无论**放宽成「只看中缀」还是整个去掉**，转红的都是**第 7 条**，
    /// 不是第 4 条：第 4 条的 `dest` 夹具是新鲜写的，判据没了它只是落进 mtime 档 → 保留 → 仍绿。
    /// 真正被这条判据护住的是「陈旧的非 tmp 文件」，而夹具里那个正是 `not_a_tmp`。
    #[test]
    fn orphan_sweep_collects_only_what_it_can_prove_is_abandoned() {
        use polaris_updater::traits::StdFs;

        let dir = scratch("orphan-sweep");
        let file_name = "polaris-0.2.0.dmg";
        let pid = std::process::id();

        // ① 本资产 + 本进程留下的残件：调用点在单飞闸内 ⇒ 此刻绝无第二条腿在写它 ⇒ 直接收（不看 mtime）。
        let mine = dir.join(format!("{file_name}.polaris-new-{pid}-7"));
        // ② 其它进程 + 陈旧（> 24h）：跨进程兜底，收。
        let stale = dir.join(format!("{file_name}.polaris-new-{}-0", pid.wrapping_add(1)));
        // ③ 其它进程 + 新鲜：可能正有另一个实例在写 ⇒ **不收**（失败安全的那一侧）。
        let fresh = dir.join(format!("{file_name}.polaris-new-{}-1", pid.wrapping_add(2)));
        // ④ 落位好的成品：绝不能碰。
        let dest = dir.join(file_name);
        // ⑤ 别的资产名 + 新鲜：**保留**。单飞闸只按 dest 串行，管不到别的资产名 ——
        //    本进程完全可能有另一条腿正在下它，故别资产不许走即时档（哪怕 pid 是自己的）。
        let other_fresh = dir.join(format!("polaris-0.1.9.dmg.polaris-new-{pid}-0"));
        // ⑥ **真实缺陷回放**：上次运行下到一半被关掉，留下的是**旧版本名**的残件（资产名含版本号，
        //    这次要下的是新版本 ⇒ 前缀必然不同）。陈旧了就必须收，否则每次版本更迭攒一个几十 MiB
        //    的垃圾，直到完全卸载才回收。
        let stale_old_version = dir.join(format!(
            "polaris-0.1.9.dmg.polaris-new-{}-4",
            pid.wrapping_add(3)
        ));
        // ⑦ 含中缀但**形态不符**（`{pid}-{seq}` 不是纯数字）：不是 `tmp_name` 的产物，一律不碰。
        let not_a_tmp = dir.join("notes.polaris-new-draft");

        for p in [
            &mine,
            &stale,
            &fresh,
            &dest,
            &other_fresh,
            &stale_old_version,
            &not_a_tmp,
        ] {
            std::fs::write(p, b"x").unwrap();
        }
        let over_age = ORPHAN_TMP_MAX_AGE + std::time::Duration::from_secs(60);
        backdate(&stale, over_age);
        backdate(&stale_old_version, over_age);
        backdate(&not_a_tmp, over_age); // 陈旧也不该被收：判据是命名形态，不是年龄
        backdate(&mine, std::time::Duration::from_secs(1)); // 新鲜也照收（判据是 pid 不是时间）

        sweep_orphan_downloads(&StdFs, &dir, file_name);

        assert!(
            !mine.exists(),
            "本资产 + 本进程留下的残件必须被收（闸已保证无人在写它）"
        );
        assert!(!stale.exists(), "超过 24h 的跨进程残件必须被收");
        assert!(
            fresh.exists(),
            "新鲜的跨进程残件必须保留 —— 另一个实例可能正在写它，而 pid 存活探测跨平台不可靠"
        );
        assert!(dest.exists(), "落位好的更新包绝不能被当成残件删掉");
        assert!(
            other_fresh.exists(),
            "别的资产名的**新鲜**残件必须保留 —— 单飞闸只按 dest 串行，管不到别的资产名"
        );
        assert!(
            !stale_old_version.exists(),
            "旧版本名的陈旧残件必须被收 —— 资产名含版本号，只收本次资产名等于这条腿永不回收"
        );
        assert!(
            not_a_tmp.exists(),
            "含 `.polaris-new-` 但不是 `{{pid}}-{{seq}}` 形态的文件不是清扫的对象"
        );

        // 交叉核对：上面五个夹具的名字是**手搓**的，与 `sweep_orphan_downloads` 的前缀判据同出一人
        // ⇒ 二者可以互相印证却**双双跑偏于真实命名**（`verify::tmp_name` 改个后缀就全盲）。
        // 故再用真产物过一遍：这条是「判据被自己污染」的唯一报警。
        let real_tmp = polaris_updater::verify::tmp_name(&dest);
        std::fs::write(&real_tmp, b"x").unwrap();
        sweep_orphan_downloads(&StdFs, &dir, file_name);
        assert!(
            !real_tmp.exists(),
            "`verify::tmp_name` 的真实产物落在清扫的前缀判据之外 —— 手搓夹具全绿也没有意义"
        );
        assert!(dest.exists(), "第二次清扫同样不得碰成品");

        // 目录不存在也不得 panic（best-effort，绝不改变本次下载的结论）。
        sweep_orphan_downloads(&StdFs, &dir.join("nope"), file_name);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 🟡 **调用点守卫：清扫必须夹在「拿到单飞闸」与「生成本次 tmp」之间。**
    ///
    /// 两侧都是硬要求：
    ///  - 早于闸 ⇒ 本进程可能有另一条腿正在写同名 tmp，那一档是**不看 mtime 直接删**的，会删掉在飞的下载；
    ///  - 晚于 `tmp_name` ⇒ 本次自己的 tmp 也带着本进程 pid，会被当场删掉。
    ///
    /// **变异探针**：删掉调用 / 挪到 `gate.lock()` 之前 / 挪到 `verify::tmp_name(&dest)` 之后
    /// ⇒ 逐条转红。
    #[test]
    fn orphan_sweep_runs_inside_the_gate_and_before_this_download_stages_its_tmp() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "pub async fn update_download(");
        let lock_at = body
            .find("gate.lock().await")
            .expect("锚点消失：守卫已失去判据");
        let sweep_at = body.find("sweep_orphan_downloads(").expect(
            "孤儿清扫被删了 —— 下载途中退出 App 会绕过 RAII 守卫，每个周期攒一个几十 MiB 的残件",
        );
        let tmp_at = body
            .find("verify::tmp_name(&dest)")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            lock_at < sweep_at,
            "清扫必须在单飞闸**之内**（实得 lock={lock_at} / sweep={sweep_at}）：\
             闸外清扫会删掉本进程另一条腿正在写的 tmp"
        );
        assert!(
            sweep_at < tmp_at,
            "清扫必须在生成本次 tmp **之前**（实得 sweep={sweep_at} / tmp={tmp_at}）：\
             之后清扫会把自己这一次的 tmp 当孤儿删掉"
        );
    }

    /// 🟡 **调用点守卫：复查回来「没有可下的东西」时，既不广播假的 `downloaded`，也不对弹窗
    /// 自己谎称「已下载」。**
    ///
    /// # 这道门此前只守住了一半
    ///
    /// 旧判据是「函数体里必须出现 `push_popup_state(&app, UpdatePopupState::done())`」+「函数体里
    /// 不得出现 `ProgressStage::Downloaded`」。它守的是**广播给设置页的那一份**（后半条），而
    /// 前半条**要求**把 `done()` 推给弹窗自己 —— 于是「一个字节都没下」在发起本次动作的那一屏上
    /// 被渲染成 `updatePopup.downloaded`（「下载完成」）+ 满格进度条。门在，守的方向漏了发起方。
    ///
    /// 新判据把「推出 progress」与「推的是哪一档」拆开：仍必须把弹窗推出 progress（否则永远转圈），
    /// 但那一档必须是 `no_update`；`done` 在本臂**一次都不许出现**。`UpdatePopupState::done` 现在
    /// 是必填落位路径的构造函数，本臂根本没有路径可传 —— 源码门与类型闸在此重合，两道都留着：
    /// 类型挡「拿不出路径」，本门挡「随手编一个路径喂给它」。
    ///
    /// # 判据必须收到**分支**，臂一级还不够
    ///
    /// 射程收紧走过三级，每一级都是被同一形态咬出来的（「取材面宽于意图面」）：
    ///  1. **整函数**：把推送搬进别的臂就能替本臂作证 —— 本仓在同一个函数上栽过两次
    ///     （见 `guard_scan::match_arm_body` 文档）。
    ///  2. **臂**（`match_arm_body`）：仍不够。`Update | Retry` 臂内有 **5 条早退分支**，而
    ///     `arm.contains(…)` 只问「臂内某处有没有这句话」。实测：把本分支的推送**整个删掉**
    ///     （⇒ 弹窗永远停在 progress 转圈，正是本门自称要防的那件事）、把它挪进下面的
    ///     `Renegotiate` 分支 ⇒ **全仓 1428 passed / 0 failed，本门全绿**。
    ///  3. **分支**（现判据）：用**位置区间**钉死 —— 那两发推送的 offset 必须落在
    ///     「`let Some(info) = data.get("updateInfo")` 这一行」与「本分支自己的
    ///     `"hasUpdate": false` 早退」之间。同款先例见本文件
    ///     [`the_user_action_forces_the_popup_into_progress_before_rechecking`] 的 `force_at < check_at`。
    ///
    /// 「我要判的是什么位置的什么形状」：形状 = 那两发推送，位置 = **那一条 `let-else` 的 else 体内**。
    /// 计数不是位置（把推送搬进 helper 里计数纹丝不动），故这里判的是区间不是次数；但两个锚点
    /// 各自的**唯一性**要单独断言，否则「区间」本身可以被第二处同名锚点重新划定。
    ///
    /// 负向断言（不得广播 `ProgressStage::Downloaded`）**故意保持全函数**：任何一条臂都不许广播，
    /// 那是更强的形态，与 `recheck_failures_settle_the_popup_without_broadcasting` 同一取向。
    /// `done(` 的负向断言保持**臂级**：本臂任何一条分支都不许推 done，同样严于分支级。
    ///
    /// **变异探针**：把 `no_update(...)` 改回 `done(popup.version.clone(), "")` ⇒ 转红；
    /// 把两发推送挪进 `Renegotiate` 分支 ⇒ 区间判定转红（臂级判据在此**全绿**，见上面第 2 条）；
    /// 把它挪进 `ViewLog` 臂 ⇒ 臂内找不到，转红。
    #[test]
    fn the_no_download_path_never_claims_a_download() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn update_popup_action(",
        );
        let arm = crate::commands::guard_scan::match_arm_body(
            &body,
            "PopupAction::Update | PopupAction::Retry =>",
            "PopupAction::",
        );

        // ── 区间的两个端点：先证明它们各自唯一，否则「落在区间内」可以被第二处锚点重新划定 ──
        const BRANCH_HEAD: &str = "let Some(info) = data.get(\"updateInfo\")";
        const BRANCH_EXIT: &str = "\"hasUpdate\": false";
        for anchor in [BRANCH_HEAD, BRANCH_EXIT] {
            let n = arm.matches(anchor).count();
            assert_eq!(
                n, 1,
                "锚点 {anchor:?} 在本臂里出现 {n} 次（须恰好 1）—— 区间端点不唯一，\
                 「落在分支内」这句话就不再指同一段代码"
            );
        }
        let head = arm.find(BRANCH_HEAD).expect("锚点消失：守卫已失去判据");
        let exit = arm.find(BRANCH_EXIT).expect("锚点消失：守卫已失去判据");

        // ── 那两发推送必须落在这条 `let-else` 的 else 体内 ──
        for (needle, why) in [
            (
                "push_popup_state(&app, UpdatePopupState::no_update(",
                "「没有可下的东西」仍须把弹窗推出 progress（否则永远转圈），且必须推 `no_update` 那一档",
            ),
            (
                "schedule_popup_auto_close(&app, NO_UPDATE_AUTO_CLOSE_MS)",
                "`noupdate` 终态没有排自动关窗，或沿用了 done 的 800ms —— 后者一闪而过，等于没说",
            ),
        ] {
            let at = arm.find(needle).unwrap_or_else(|| panic!("{why}（本臂里根本找不到）"));
            assert!(
                head < at && at < exit,
                "{why}。实得 offset：分支头={head} / 本句={at} / 分支早退={exit} —— \
                 它不在「复查回来没有 updateInfo」那条分支里。搬到臂内别的分支上，\
                 这条路径就一发都不推，而弹窗停在 progress 永远转圈"
            );
        }

        assert!(
            !arm.contains("UpdatePopupState::done("),
            "本臂一个字节都没下 —— 推 `done` 就是在发起本次动作的那一屏上谎称「下载完成」\
             （2026-08-17 前的现状）。落位路径是 `done` 的必填参数，本臂拿不出真的那一个"
        );
        assert!(
            !body.contains("ProgressStage::Downloaded"),
            "「没有可下的东西」不得广播 downloaded —— 无文件、无 filePath，设置页会显示假的「已下载」"
        );
    }

    // ── 弹窗邀请 ↔ 真正下载：同一个版本 ────────────────────────────────────

    /// 🟡 **共享口径选出来的是最新正式版，不是发布时间更晚的那条预发布线。**
    ///
    /// ⚠️ **本测试只守「弹窗写的版本 == 真正下载的版本」的一半 —— 候选集*规则*那一半。**
    /// 别把它读成「TOCTOU 也守住了」：内容那一半（两次 check 之间上游又发了一版）由
    /// [`popup_recheck_reconciles_the_version_against_what_the_popup_showed`] 守着的**逐字对账**
    /// 负责，与本测试正交。整条不变量由三道门合起来才成立：
    ///  1. 口径**规则**共享 —— [`every_update_check_call_site_uses_the_shared_prerelease_scope`]；
    ///  2. 这个规则选出来的**是什么** —— 本测试（常量共享得再好，值错了照样把用户推到预发布上）；
    ///  3. 规则相同但**内容**变了怎么办 —— 那条对账门。
    ///
    /// 样本刻意造成「最新正式版之后又发过一条预发布」：这正是 2026-08-17 之前复查腿写死
    /// `Some(true)` 时会翻车的形状 —— `select_update_release` 按 `published_at` 取最新，
    /// 于是弹窗写 `v0.2.0`、点下去下的是 `v0.3.0-beta.1`。
    ///
    /// **变异探针**：把 [`PUSH_UPDATE_INCLUDE_PRERELEASE`] 改成 `true` ⇒ 三条正向断言同时转红。
    #[test]
    fn the_shared_scope_picks_the_stable_line_not_a_later_prerelease() {
        const RELEASES: &str = r#"[
          {"tag_name":"v0.3.0-beta.1","prerelease":true,"published_at":"2024-06-01T00:00:00Z",
           "assets":[{"name":"Polaris-0.3.0-mac-arm64.dmg","browser_download_url":"https://x/beta","size":1}]},
          {"tag_name":"v0.2.0","prerelease":false,"published_at":"2024-05-01T00:00:00Z",
           "assets":[{"name":"Polaris-0.2.0-mac-arm64.dmg","browser_download_url":"https://x/stable","size":1}]}
        ]"#;
        let pick = |include_pre: bool| match check_app_update(
            RELEASES,
            "0.1.0",
            include_pre,
            None,
            AssetPlatform::Macos,
            AssetArch::Arm64,
            false,
        )
        .expect("样本 JSON 必须可解析")
        {
            AppUpdateCheck::Available(i) => i,
            AppUpdateCheck::NoUpdate => panic!("样本里有比 0.1.0 新的版本，不该判成无更新"),
        };

        // 产出邀请（startup_tasks / tray → update_popup_show 写进弹窗的那个版本号）与兑现邀请
        // （update_popup_action 复查 → update_download 真下的那份资产）读的是同一个常量。
        let advertised = pick(PUSH_UPDATE_INCLUDE_PRERELEASE);
        assert_eq!(
            advertised.version, "v0.2.0",
            "推给用户的必须是最新**正式版**，而不是发布时间更晚的那条预发布线"
        );
        assert!(
            !advertised.is_prerelease,
            "「推」腿不得把预发布推到用户脸上 —— App 侧没有任何通道开关支撑这个选择"
        );
        assert_eq!(
            advertised.download_url, "https://x/stable",
            "复查后真正下载的资产必须属于弹窗写着的那个版本"
        );
        // 反证：口径一放开，选中的就是另一个版本。没有这一条，上面三条在「样本里只有一个候选」
        // 时同样全绿 —— 那是恒真断言，等于没门。
        assert_eq!(
            pick(true).version,
            "v0.3.0-beta.1",
            "样本自检：放开口径必须指向另一个版本，否则本测试对该常量不敏感"
        );
    }

    /// 递归收集 `src-tauri/src` 下的全部 `.rs`（路径 + 源码）。
    ///
    /// 读不到目录/文件一律 panic —— 判据面读空时必须转红，而不是「扫到 0 个文件于是 0 个断言全绿」。
    fn rust_sources_under(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("读不到目录 {}: {e}", dir.display()));
        for entry in entries {
            let p = entry.expect("目录项读取失败").path();
            if p.is_dir() {
                rust_sources_under(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let s = std::fs::read_to_string(&p)
                    .unwrap_or_else(|e| panic!("读不到 {}: {e}", p.display()));
                out.push((p, s));
            }
        }
    }

    /// 从 `update_check` 的左括号处取**实参列表**：括号配平定边界，行内 `//` 注释到行尾整段丢弃，
    /// 顶层逗号切分，每个实参去掉全部空白（⇒ 断言不随 rustfmt 的换行位置漂移）。
    ///
    /// # 射程自曝
    ///
    /// 这是词法上的简化，不是 Rust parser：实参里若出现含 `//` 的字符串字面量、或含逗号的泛型
    /// （`Foo<A, B>`）会被切错。`update_check` 的三个实参恒为 `AppHandle` / `State` / `Option<bool>`，
    /// 两者都不出现；真出现了，调用点那边「必须恰好 3 个实参」那条断言会**转红**（fail-loud），
    /// 不会静默放行。
    fn call_args(src: &str, open: usize) -> Vec<String> {
        let mut depth = 0usize;
        let mut args: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut in_comment = false;
        let mut it = src[open..].chars().peekable();
        while let Some(c) = it.next() {
            if in_comment {
                if c == '\n' {
                    in_comment = false;
                }
                continue;
            }
            if c == '/' && it.peek() == Some(&'/') {
                in_comment = true;
                it.next();
                continue;
            }
            match c {
                '(' | '[' | '{' => {
                    depth += 1;
                    if depth > 1 {
                        cur.push(c);
                    }
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if !cur.trim().is_empty() {
                            args.push(cur.split_whitespace().collect());
                        }
                        return args;
                    }
                    cur.push(c);
                }
                ',' if depth == 1 => {
                    args.push(cur.split_whitespace().collect());
                    cur.clear();
                }
                _ => {
                    if depth >= 1 {
                        cur.push(c);
                    }
                }
            }
        }
        panic!("`update_check(` 的括号没配平 —— 取材器判据已过期");
    }

    /// 🟡 **调用点守卫：`update_check` 的**每一处**调用都必须把预发布口径写成那个共享常量本身。**
    ///
    /// 「弹窗邀请的版本 == 点下去下载的版本」不是靠一处判断实现的，而是靠两条产出腿 + 一条兑现腿
    /// 读同一个值。三处各写一个 `Some(false)` 在今天等价，但正是 2026-08-17 那次缺陷的温床：
    /// 兑现腿被单独改成 `Some(true)`，另两处纹丝不动，没有任何门会红。
    ///
    /// # 覆盖面由**判据**定，不由夹具定
    ///
    /// 本门**遍历 `src-tauri/src` 全部 `.rs`**（`CARGO_MANIFEST_DIR`），不列 `include_str!` 清单，
    /// 也不按函数名点腿：新加一条腿、甚至新加一个文件，都自动进判据面。上一版照着三个函数名硬编码
    /// ⇒ 明天加第四条腿写 `Some(true)`，三条断言全绿；且它只看函数体内**第一处** `update_check(`，
    /// 同一腿里再加一个坏调用同样全绿。两个洞都由「遍历 + 全部出现处」堵掉。
    ///
    /// # 「总数 ≥ 3」不等于「三条腿都还在」
    ///
    /// 计数是可被别处补齐的：某条腿改用别名 import 后不再被识别，另两条腿加上任意新增的合规调用
    /// 照样把计数顶回 3。故除计数外**按文件点名**三条已知腿。上一版（只有计数）实测被这个形态
    /// 骗过：别名改写 `tray.rs` + 一个含合规调用的探针文件 ⇒ 编译通过、全仓测试全绿、本门 ok。
    ///
    /// # 射程自曝：只扫 `src-tauri/src`
    ///
    /// 根目录写死 `CARGO_MANIFEST_DIR/src`，`crates/*` 不在射程。今天结构性不可达 —— 依赖方向是
    /// `crates → src-tauri`（`polaris_updater` 等被 `src-tauri` 依赖，反过来不成立），而
    /// `update_check` 是 `src-tauri` 的 `#[tauri::command]`，crates 侧引用不到它。哪天依赖方向变了
    /// （或 command 层下沉进 crate），这里要跟着扩根目录。
    ///
    /// # 判据是**精确等于**，不是「包含常量名」
    ///
    /// 只接受 `Some(PUSH_UPDATE_INCLUDE_PRERELEASE)`（或带完整路径那一形）。`contains` 式判据
    /// 实测放行 `Some(!PUSH_UPDATE_INCLUDE_PRERELEASE)` 与 `Some(PUSH_UPDATE_INCLUDE_PRERELEASE || true)`
    /// ——**口径被反转而门不红**；反过来，行尾同行注释里出现 `Some(false)` 字样会顶红
    /// （`guard_scan::strip_line_comments` 只剥整行）。改成解析实参 + 精确比对后，取反/或运算不再
    /// 通过，注释也不再误伤（[`call_args`] 在括号内自行剥行尾注释）。
    ///
    /// # 一条**有意为之**的拒收（别当成误报来「修」）
    ///
    /// 先绑局部变量再传（`let scope = Some(PUSH_UPDATE_INCLUDE_PRERELEASE); update_check(a, b, scope)`）
    /// 同样会被拒。本门是词法的，追不了数据流：放行 `scope` 就等于放行任何名字，那时门只剩
    /// 「第三个实参是个标识符」这点信息。代价是调用点必须把常量写在明面上 —— 这正是本门要的
    /// 「口径肉眼可查」。要重构成绑变量，请连同本门的判据一起改，别把判据改宽了事。
    ///
    /// **变异探针**：任一调用点改成 `Some(true)` / `Some(false)` / `Some(!C)` / `Some(C || true)`
    /// ⇒ 该点转红；实参行加个含 `Some(false)` 字样的**行尾注释** ⇒ 仍然全绿（不误红）；
    /// 在任意文件（哪怕全新、未挂进模块树的文件）里新增一个坏调用 ⇒ 转红；
    /// 把调用整个删光 ⇒ 「至少 3 个调用点」自检转红。
    #[test]
    fn every_update_check_call_site_uses_the_shared_prerelease_scope() {
        /// 唯一放行的两种写法（裸名 + 完整路径）。
        const EXPECTED: [&str; 2] = [
            "Some(PUSH_UPDATE_INCLUDE_PRERELEASE)",
            "Some(crate::commands::updater::PUSH_UPDATE_INCLUDE_PRERELEASE)",
        ];
        const NEEDLE: &str = "update_check(";

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources_under(&root, &mut files);
        assert!(
            files.len() >= 20,
            "取材自检：{} 下只扫到 {} 个 .rs —— 判据面塌了",
            root.display(),
            files.len()
        );

        let mut sites = 0usize;
        let mut hit_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (path, raw) in &files {
            // 整行注释（含 `///` 文档）先抹平，保留行数与行序 ⇒ 下面的行号仍然对得上。
            let src = crate::commands::guard_scan::strip_line_comments(raw);
            let mut from = 0usize;
            while let Some(rel) = src[from..].find(NEEDLE) {
                let at = from + rel;
                from = at + NEEDLE.len();
                let before = &src[..at];
                // `core_update_check(` 这类同尾名：前一个字符仍是标识符字符 ⇒ 不是本函数。
                if before
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                // 字符串字面量里的提及 —— **本门自己的判据串与 panic 文案就是这一类**
                // （第一版只查「前一个字符是不是 `"`」，结果被自己那句
                // ``panic!("`update_check(` 的括号没配平…")`` 顶红：那里前一个字符是反引号）。
                // 判据换成「本行到此处为止的引号个数是奇数 ⇒ 正处在字面量内部」，对本仓这些**单行**
                // 字面量足够；跨行字面量里若再出现它，下面「必须恰好 3 个实参」会转红而非静默放行。
                let line_start = before.rfind('\n').map_or(0, |i| i + 1);
                let mut escaped = false;
                let mut quotes = 0usize;
                for ch in before[line_start..].chars() {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quotes += 1;
                    }
                }
                if quotes % 2 == 1 {
                    continue;
                }
                // 定义本身（`pub async fn update_check(`）。
                if before.trim_end().ends_with("fn") {
                    continue;
                }
                let where_ = format!("{}:{}", path.display(), before.matches('\n').count() + 1);
                let args = call_args(&src, at + "update_check".len());
                assert_eq!(
                    args.len(),
                    3,
                    "{where_} 的 update_check 实参不是 3 个（实得 {args:?}）—— 签名变了或取材器判据过期"
                );
                assert!(
                    EXPECTED.contains(&args[2].as_str()),
                    "{where_} 的预发布口径不是共享常量**本身**（实得 `{}`）。只接受 {EXPECTED:?} —— \
                     字面量、取反 `Some(!C)`、或运算 `Some(C || true)` 一律拒收，否则\
                     「弹窗按 A 邀请、点下去下到 B」会重新长出来",
                    args[2]
                );
                sites += 1;
                hit_files.insert(
                    path.strip_prefix(&root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        assert!(
            sites >= 3,
            "只扫到 {sites} 个 update_check 调用点 —— 至少应有 3（启动自动检查 / 托盘检查 / 弹窗复查）"
        );
        // **具名覆盖**：光有「总数 ≥ 3」不够 —— 那是个可被别处补齐的计数。某条腿改用别名 import
        // （`use crate::commands::update_check as check_app_update;`）后就不再被 NEEDLE 识别，
        // 而另外两条腿 + 任意新增的合规调用照样把计数顶到 3 ⇒ 那条腿悄悄脱离判据面而全绿。
        // 实测过这个形态：别名改写 + 一个合规探针文件 ⇒ 编译通过、全仓测试全绿、本门 ok。
        // 故按**文件**点名三条已知腿（文件级即可，不下探到函数级：函数改名是重构，换文件才是搬家）。
        for leg in ["tray.rs", "runtime/startup_tasks.rs", "commands/updater.rs"] {
            assert!(
                hit_files.contains(leg),
                "{leg} 里一个可识别的 update_check 调用点都没有 —— 这条腿要么被删了，\
                 要么改成了本门认不出的写法（别名 import / 中转函数）。命中文件：{hit_files:?}"
            );
        }
    }

    /// 🟡 **真值表：对账判定本身（`reconcile_recheck`）。**
    ///
    /// 与它配套的接线由 [`popup_recheck_reconciles_the_version_against_what_the_popup_showed`] 守：
    /// 那条守「有没有调用、结论有没有被执行」，本条守「判对没判对」。缺任一条都只有一半。
    ///
    /// **变异探针**：把 `advertised == Some(rechecked)` 改成 `!=`、或把空串那条早退删掉
    /// ⇒ 逐条转红。
    #[test]
    fn reconcile_recheck_only_proceeds_on_a_verbatim_match() {
        // 逐字相同 ⇒ 照下（唯一放行档）。
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), "v1.2.0"),
            RecheckVerdict::Proceed
        );
        // 复查期间上游又发了一版（两端都是正式版，口径常量对此无能为力）⇒ 重新征求同意。
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), "v1.3.0"),
            RecheckVerdict::Renegotiate
        );
        // 大小写 / 前缀 / 空白差异一律不算「相同」——版本号是标识符，不是自然语言。
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), "V1.2.0"),
            RecheckVerdict::Renegotiate
        );
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), "1.2.0"),
            RecheckVerdict::Renegotiate
        );
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), " v1.2.0"),
            RecheckVerdict::Renegotiate
        );
        // 不知道当初承诺了什么 ⇒ 不能声称对上了（失败安全，不是 Proceed）。
        assert_eq!(
            reconcile_recheck(None, "v1.3.0"),
            RecheckVerdict::Renegotiate
        );
        // 复查回包没有版本号 = 契约破损：既不下也不弹空版本号。
        assert_eq!(
            reconcile_recheck(Some("v1.2.0"), ""),
            RecheckVerdict::Unusable
        );
        assert_eq!(reconcile_recheck(None, ""), RecheckVerdict::Unusable);
    }

    /// 🟡 **调用点守卫：兑现腿必须拿复查回来的版本与弹窗上写着的那串字逐字对账。**
    ///
    /// 上面那道门统一的是候选集**规则**；这道守的是**内容**。两次 check 之间隔着用户的思考时间，
    /// 上游随时可能再发一版 —— 那时两端都是正式版、口径完全相同，「按 A 邀请、下到 B」照样发生。
    /// 而弹窗上真写着的那串字就在同一作用域里（`popup.version`，Skip / ManualDownload 一直在读它）。
    ///
    /// 顺序判据不是形式主义：对账必须夹在**复查之后、下载之前**，且不一致时必须**提前返回** ——
    /// 只推个 `remind` 却继续往下走 = 弹窗回到提醒态、包照下，比不改还糟。
    ///
    /// **变异探针**：删掉比较 / 删掉 `remind` / 把 `return` 去掉让它继续往下走 / 把对账挪到
    /// `update_download` 之后 ⇒ 逐条转红。
    #[test]
    fn popup_recheck_reconciles_the_version_against_what_the_popup_showed() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn update_popup_action(",
        );
        // 锚点限定在 `Update | Retry` 臂内：在整个函数体上 find 时，别的臂里出现同形代码就会
        // 替本臂作证（同文件两道门都栽在这个形态上，见 `guard_scan::match_arm_body` 头注）。
        let arm = crate::commands::guard_scan::match_arm_body(
            &body,
            "PopupAction::Update | PopupAction::Retry =>",
            "PopupAction::",
        );
        let check_at = arm.find("update_check(").expect("锚点消失：守卫已失去判据");
        let cmp_at = arm
            .find("reconcile_recheck(popup.version.as_deref(), rechecked)")
            .expect("版本对账被删了 —— 复查换了目标也会照下，而弹窗上仍写着旧版本号");
        let remind_at = arm.find("UpdatePopupState::remind(rechecked").expect(
            "不一致时没有退回 remind —— 换个目标接着下只是「告知」，不是「征求同意」\
             （何况 progress 态根本不渲染版本号，连告知都不成立）",
        );
        let download_at = arm
            .find("update_download(")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            check_at < cmp_at && cmp_at < download_at,
            "对账必须夹在复查与下载之间（实得 check={check_at} / cmp={cmp_at} / download={download_at}）"
        );
        assert!(
            cmp_at < remind_at && remind_at < download_at,
            "退回 remind 必须在下载之前（实得 cmp={cmp_at} / remind={remind_at} / download={download_at}）"
        );
        assert!(
            arm[remind_at..download_at].contains("return Ok("),
            "退回 remind 之后没有 return —— 弹窗回到提醒态，下载却照跑"
        );
    }

    /// 🟡 **调用点守卫：`ManualDownload` 必须把会话记住的版本喂给 release 页。**
    ///
    /// 这条钉的是一次**用户可见行为变更**：`PopupSession::send_state` 开始跨 phase 继承版本号后，
    /// error 态的「手动下载」从「回落 GitHub 泛 releases 列表页」变成「直达该版本 tag 页」
    /// （`releases_url_for(Some(v))`）。改进是真的，但它**不能顺带发生、也不能顺带消失** ——
    /// 有人把这里改成传 `None`，用户又会掉回列表页而没有任何门说话。
    ///
    /// 分工：数据侧（版本号跨 phase 还在不在）由 `polaris_updater::popup` 的
    /// `the_invited_version_survives_phase_changes` 守；映射侧（版本号 → tag 页 URL）由
    /// [`releases_url_for_version_targets_tag_page`] 守；本条只守中间那根线还接着。
    ///
    /// **变异探针**：把 `popup.version.clone()` 改成 `None` ⇒ 转红。
    #[test]
    fn manual_download_opens_the_release_page_of_the_invited_version() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn update_popup_action(",
        );
        const NEEDLE: &str = "update_open_releases(app, popup.version.clone())";
        const HEAD: &str = "PopupAction::ManualDownload =>";
        // 切片封到下一条臂头（[`guard_scan::match_arm_body`]，那里有它自己的合成样本单测）：
        // 切到函数体尾时，`ViewLog` 臂里有一句**逐字相同**的调用会替本条作证 —— 实测把
        // ManualDownload 臂整体移到 ViewLog 之前、实参保持 `None`，编译通过、全仓测试全绿，
        // 而用户行为退回 #311 原形。
        let arm = crate::commands::guard_scan::match_arm_body(&body, HEAD, "PopupAction::");
        assert!(
            arm.contains(NEEDLE),
            "ManualDownload 没有把会话记住的版本喂给 release 页 —— 用户会掉回泛列表页（#311 的原形）"
        );
        // 位置判据 + 计数判据合起来才闭合：位置判据挡「臂被搬走」，计数判据挡「某一臂的调用被删」
        // （ViewLog 与 ManualDownload 各一处，两者都得在）。单用计数挡不住「两处都在同一臂里」，
        // 单用位置挡不住「另一臂悄悄丢了」。
        //
        // ⚠️ **登记一处今天等价、将来不等价的形态**：`ManualDownload` 是末臂，切片因而一路延伸到
        // 函数的右花括号。今天无害（`match act { … }` 是尾表达式，其后没有代码）；一旦改成
        // `let resp = match act { … };` 再加后续处理，match **之后**的代码就落进本切片 ⇒ 上面那条
        // `contains(NEEDLE)` 可被它喂饱。封顶挡不住（那里没有臂头）、`match_arm_body` 的输出自检
        // 也挡不住（没有 `PopupAction::`），届时只剩下面的 `count == 2` 部分设防。
        assert_eq!(
            body.matches(NEEDLE).count(),
            2,
            "`{NEEDLE}` 应恰好两处（ViewLog + ManualDownload），实得 {}",
            body.matches(NEEDLE).count()
        );
    }

    /// 🟡 **调用点守卫：复查之前必须先把弹窗**强制**推进 progress(0)。**
    ///
    /// 两件事都压在这一行上：
    ///  1. **窗内反馈**：复查可跑满 15s，其间没有任何进度事件 ⇒ 用户点完「更新」看着 remind 发呆；
    ///  2. **后续所有 `push_popup_state` 的前提**：闸 [`should_mirror_to_popup`] 只放行 `Progress`。
    ///     这一发若换成带闸的 `push_popup_state`，phase 会停在 `Remind` ⇒ 闸对之后每一发都判否
    ///     ⇒ 对账退回 remind、两条复查失败早退推 error、下载进度镜像**全部变成 no-op**，
    ///     而 `PopupAction::Cancel`（仅 Progress 合法）结构性不可达。
    ///
    /// 本批删掉全局广播之后，这条的后果被放大了：改动前设置页至少还会亮一条（虽然那条本身是误报），
    /// 现在两条复查失败腿在任何地方都不再有反馈。
    ///
    /// **此前唯一拦住它的是偶然**：改成 `push_popup_state` 会让 `force_popup_state` 变成零调用点
    /// ⇒ clippy `-D warnings` 报 never used。那是**夹具级**保护 —— 再多一处 `force_popup_state`
    /// 调用它就消失。实测：直接改成 `push_popup_state` ⇒ 编译通过、全仓测试全绿。
    ///
    /// **变异探针**：改成 `push_popup_state` / 删掉这一行 / 把它挪到 `update_check(` 之后 ⇒ 逐条转红。
    #[test]
    fn the_user_action_forces_the_popup_into_progress_before_rechecking() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn update_popup_action(",
        );
        // **两个锚点都必须限定在 `Update | Retry` 臂内**。在整个函数体上 `find()` 时，把这发
        // `force_popup_state` 挪进 `ViewLog` 臂（位置更靠前）再从本臂删掉 ⇒ 两个 `find` 依旧命中、
        // 顺序依旧成立、本门全绿，而上面列的后果一条不少地发生。实测：全仓 4178 全绿。
        // 这与同文件 `manual_download_...` 那道门踩的是同一形态（别的臂替本臂作证）。
        let arm = crate::commands::guard_scan::match_arm_body(
            &body,
            "PopupAction::Update | PopupAction::Retry =>",
            "PopupAction::",
        );
        let force_at = arm
            .find("force_popup_state(&app, UpdatePopupState::progress(0, None, None))")
            .expect(
                "复查前那发强制 progress(0) 没了 —— 窗内 15s 零反馈，且之后每一发状态推送都会被闸拦掉",
            );
        let check_at = arm.find("update_check(").expect("锚点消失：守卫已失去判据");
        assert!(
            force_at < check_at,
            "强制 progress(0) 必须在复查**之前**（实得 force={force_at} / check={check_at}）：\
             之后才推等于那 15s 里窗内仍是零反馈"
        );
    }

    /// 🟡 **不变量：复查阶段的失败只推弹窗，绝不广播 `update:progress`。**
    ///
    /// 这条路径一个字节都没下、没有 filePath。`emit_progress` 会把事件**全局广播**，让设置页
    /// 弹一条它从未发起过的下载错误（`SettingsUpdate` 的 `onProgress` 直接置 error 态 + 错误文案）。
    /// 同函数「已是最新」那档早就写着「只推弹窗、不广播」，两条复查失败腿此前却在广播 ——
    /// 同一个函数里两种取向。行为对**弹窗**逐字不变：`emit_progress` 的弹窗镜像就是
    /// `push_popup_state(UpdatePopupState::error(msg))` 这一发（见 `popup_state_for`）。
    ///
    /// **变异探针**：任一条早退改回 `emit_progress(&app, &info, ProgressStage::Failed(..))` ⇒ 转红。
    #[test]
    fn recheck_failures_settle_the_popup_without_broadcasting() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            SRC,
            "pub async fn update_popup_action(",
        );
        assert!(
            !body.contains("emit_progress("),
            "复查腿又开始广播 update:progress —— 设置页会显示一条它从未发起过的下载错误"
        );
        // 正向对照：两条早退确实各自把弹窗推进了 error（不广播 ≠ 什么都不做）。
        // 负向断言（上面那条）**故意保持全函数**：任何一条臂都不许广播，那是更强的形态。
        // 正向计数则限定到本臂 —— 否则把某条早退搬去别的臂，计数照样是 2。
        let arm = crate::commands::guard_scan::match_arm_body(
            &body,
            "PopupAction::Update | PopupAction::Retry =>",
            "PopupAction::",
        );
        assert_eq!(
            arm.matches("push_popup_state(&app, UpdatePopupState::error(")
                .count(),
            2,
            "复查失败的两条早退（请求失败 / 回包缺版本号）必须各自把弹窗推进 error，否则窗内永远转圈"
        );
    }

    // ── 解归档工作目录（M7）─────────────────────────────────────────────────

    /// 🟡 **变异锁：并发的解归档腿必须各占各的工作目录，且退出即自清。**
    ///
    /// 固定名 `core-staged/extract` 时，调度器的自动下载腿与用户点的 `core_update_run` 会互相
    /// `rm -rf` / 覆盖，一方可能读到**对方**的核字节并以自己的版本号 stage/换入。
    ///
    /// **变异探针**：把 [`ExtractWorkDir::create`] 的唯一后缀去掉 ⇒ 「路径互不相同」转红；
    /// 删掉 `Drop` 实现 ⇒ 「退出即自清」转红。
    #[test]
    fn extract_work_dirs_are_unique_and_self_cleaning() {
        let base = scratch("extract");
        let paths: Vec<std::path::PathBuf> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let base = base.clone();
                    s.spawn(move || {
                        let w = ExtractWorkDir::create(&base).unwrap();
                        let p = w.path().to_path_buf();
                        // 还持着守卫时目录必须存在（并发的另一条腿不得把它删掉）。
                        assert!(p.is_dir(), "工作目录被别人删了：{}", p.display());
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        assert!(p.is_dir(), "并发期间工作目录被 rm 掉了：{}", p.display());
                        p
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "并发的解归档腿拿到了同一个工作目录"
        );
        for p in &paths {
            assert!(!p.exists(), "守卫 drop 后必须清掉工作目录：{}", p.display());
            assert_eq!(
                p.parent(),
                Some(core_paths::staged_dir_in(&base).as_path()),
                "工作目录必须落在 core-staged 下（不污染现役核目录）"
            );
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 便携形态判定：**有标记 = loose，无标记 = installed**。
    ///
    /// 这条是修复的另一半。修好选包器却让本函数恒返回 `false`（旧实现读的
    /// `PORTABLE_EXECUTABLE_DIR` 在本仓恒不存在），便携用户照样被推安装器 ——
    /// 变异探针：把判据改回任何一个 env 读取 ⇒ 「有标记」那条转红。
    #[test]
    fn portable_layout_is_decided_by_marker_file_next_to_exe() {
        let base =
            std::env::temp_dir().join(format!("polaris-portable-probe-{}", std::process::id()));
        let portable_dir = base.join("portable");
        let installed_dir = base.join("installed");
        std::fs::create_dir_all(&portable_dir).unwrap();
        std::fs::create_dir_all(&installed_dir).unwrap();
        std::fs::write(portable_dir.join(PORTABLE_MARKER), b"portable\n").unwrap();

        // 有标记 ⇒ 便携。
        assert!(
            is_portable_layout(&portable_dir.join("polaris.exe")),
            "exe 同级有 {PORTABLE_MARKER} 时必须判成便携形态，否则便携用户会被推 NSIS 安装器"
        );
        // 无标记 ⇒ 安装态（NSIS 装出来的目录里没有这个文件）。
        assert!(!is_portable_layout(&installed_dir.join("polaris.exe")));
        // 标记是**目录**不算数（`is_file` 而非 `exists`）。
        std::fs::create_dir_all(installed_dir.join(PORTABLE_MARKER)).unwrap();
        assert!(!is_portable_layout(&installed_dir.join("polaris.exe")));
        // 无父目录的退化路径不得 panic。
        assert!(!is_portable_layout(std::path::Path::new("polaris.exe")));

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 标记文件名是**跨文件契约**（`package.yml` 写它、本模块读它），钉死字面值。
    #[test]
    fn portable_marker_name_matches_packaging_contract() {
        assert_eq!(PORTABLE_MARKER, "portable.marker");
    }

    // ── 下载进度（此前只有 0% / 100% 两点）───────────────────────────────────

    /// 🟡 **进度百分比的三条规则各有牙**：无分母不发 / 夹在 1..=99 / 同值去重。
    ///
    /// **变异探针**：去掉 `clamp(1, 99)` ⇒ 「0 与 100 不由中段发」两条断言转红；
    /// 去掉 `pct != last_pct` ⇒ 「同百分比不重发」转红；
    /// 把 `expected` 缺失时回落成 `received` ⇒ 「无 Content-Length 不发」转红。
    #[test]
    fn progress_percent_rules() {
        // 无 Content-Length / 为 0 → 一律不发（不拿已收字节凑假分母）。
        assert_eq!(progress_percent(5_000, None, 0), None);
        assert_eq!(progress_percent(5_000, Some(0), 0), None);

        // 正常中段。
        assert_eq!(progress_percent(50, Some(100), 0), Some(50));
        // 同百分比不重发（IPC 洪水防线）。
        assert_eq!(progress_percent(50, Some(100), 50), None);
        assert_eq!(progress_percent(509, Some(1000), 50), None, "50.9% 仍是 50");
        assert_eq!(progress_percent(510, Some(1000), 50), Some(51));

        // 下界：0% 不由中段发（那一发由下载开始前独占）。
        assert_eq!(
            progress_percent(0, Some(1000), 0),
            Some(1),
            "首个 chunk 前的 0 会被夹到 1；last_pct=0 故仍发一次"
        );
        assert_eq!(progress_percent(3, Some(1000), 1), None, "夹到 1，与上次同");
        // 上界：100% 不由中段发（那一发由 downloaded 独占，否则出现倒退帧）。
        assert_eq!(progress_percent(1000, Some(1000), 50), Some(99));
        // received 超过 total（服务端 Content-Length 撒谎）→ 仍夹在 99，不溢出。
        assert_eq!(progress_percent(9_999, Some(1000), 50), Some(99));
    }

    // ── 弹窗各档（此前只有 remind 一档可达）─────────────────────────────────

    /// 🟡 **弹窗镜像与广播载荷读的是同一帧：三种帧各自带齐自己那一屏要的事实。**
    ///
    /// 本函数此前吃的是压平后的 `(status, percentage, error)`，落位路径与版本号在那层压平里丢了
    /// ⇒ `done` 只剩一个状态字。改吃 [`ProgressStage`] + 同一份清单后，两屏结构上不可能各说各话。
    ///
    /// **变异探针**：把 `Downloaded` 那臂的 `path.to_string_lossy()` 换成常量串 ⇒ 落位路径那条转红；
    /// 把 `info.get("fileSize")` 换成 `None` ⇒ 分母那条转红；
    /// 把 `Downloading` 的 `Some(received)` 换成 `None` ⇒ 已收字节那条转红。
    #[test]
    fn progress_frames_map_to_popup_phases_with_their_facts() {
        let info = json!({ "version": "v1.2.0", "fileSize": 52_000_000_u64 });
        let path = std::path::Path::new("/tmp/updates/polaris.dmg");

        let downloading = popup_state_for(
            &info,
            ProgressStage::Downloading {
                percentage: 42,
                received: 19_240_000,
            },
        );
        assert_eq!(downloading.phase, PopupPhase::Progress);
        assert_eq!(downloading.percentage, Some(42));
        assert_eq!(
            downloading.received_bytes,
            Some(19_240_000),
            "已收字节必须是回调原值 —— 从百分比反推的数每一帧都是错的"
        );
        assert_eq!(
            downloading.total_bytes,
            Some(52_000_000),
            "分母取本帧随行清单的 fileSize（与设置页下载卡同源）"
        );

        let done = popup_state_for(
            &info,
            ProgressStage::Downloaded {
                path,
                verified: true,
            },
        );
        assert_eq!(done.phase, PopupPhase::Done);
        assert_eq!(
            done.file_path.as_deref(),
            Some("/tmp/updates/polaris.dmg"),
            "「完成」得说得出包落在哪儿 —— 没有它，它与「什么都没下」长得一模一样"
        );
        assert_eq!(
            done.version.as_deref(),
            Some("v1.2.0"),
            "得说得出下的是哪一版"
        );

        let err = popup_state_for(&info, ProgressStage::Failed("网络不可达"));
        assert_eq!(err.phase, PopupPhase::Error);
        assert_eq!(err.error_text.as_deref(), Some("网络不可达"));
        // 错误文案缺失 → 兜底而非空串（空 error 态弹窗上是一片空白）。
        assert_eq!(
            popup_state_for(&info, ProgressStage::Failed("")).error_text,
            Some("更新失败".to_string())
        );

        // 清单缺 `fileSize` / 给 0 ⇒ 分母未知，只报已收量，**不拿已收字节凑假分母**。
        for blind in [json!({}), json!({ "fileSize": 0 })] {
            let s = popup_state_for(
                &blind,
                ProgressStage::Downloading {
                    percentage: 42,
                    received: 19_240_000,
                },
            );
            assert_eq!(s.received_bytes, Some(19_240_000));
            assert_eq!(s.total_bytes, None, "分母未知时不得编一个出来：{blind}");
        }
        // 清单缺 `version` ⇒ 不编版本号（会话继承会补上弹窗邀请的那一版，见 PopupSession）。
        assert_eq!(
            popup_state_for(
                &json!({}),
                ProgressStage::Downloaded {
                    path,
                    verified: false,
                }
            )
            .version,
            None
        );
    }

    /// 🟡 **后台下载不得顶掉用户面前的 remind 提示。**
    ///
    /// `autoDownloadUpdate` 开启时，启动检查会「弹 remind 窗」+「后台下载」并行；若进度事件无条件
    /// 镜像进弹窗，用户**再也看不到**「要不要更新」那一屏。闸只放行 `Progress`（= 用户亲手点过
    /// 「更新」的弹窗）。
    ///
    /// 判据面**遍历 [`PopupPhase::ALL`]**，不点名几个 phase：新加一档而闸忘了表态时，点名式清单
    /// 静默漏掉那一格（且全绿），遍历式必判。`ALL` 自己与枚举由 `state.rs` 的门对账。
    ///
    /// **变异探针**：把闸改成恒 true / 放行任意一个非 Progress 档 ⇒ 本条转红并点名那一档。
    #[test]
    fn popup_only_follows_progress_it_was_put_into_by_the_user() {
        for phase in PopupPhase::ALL {
            let expected = phase == PopupPhase::Progress;
            assert_eq!(
                should_mirror_to_popup(phase),
                expected,
                "{phase} 档的跟随判定错了 —— 只有用户亲手推进 progress 的弹窗才跟随后台下载；\
                 remind 被顶掉用户就再也看不到「要不要更新」那一屏，终态被顶掉则是拿一次无关\
                 下载改写一个已经落定的结论"
            );
        }
        // 正向对照：闸不是恒 false（那样弹窗里点「更新」后窗内零反馈）。
        assert!(should_mirror_to_popup(PopupPhase::Progress));
    }

    // ── 请求级总超时 ─────────────────────────────────────────────────────────

    /// 🟡 **总超时必须严格小于「逐跳超时 × 最大跳数」，否则它不是兜底而是装饰。**
    ///
    /// `safe_redirect_fetch` 最多跟 5 跳（`max_redirects: Some(5)`），逐跳 15s ⇒ 最坏 90s。
    /// 契约要求 20s 整体兜底。**变异探针**：把总超时调到 ≥ 90s ⇒ 本条转红。
    #[test]
    fn core_check_total_timeout_actually_caps_multi_hop_worst_case() {
        const MAX_HOPS: u64 = 5 + 1; // 5 次重定向 + 最终一跳
        assert_eq!(CORE_CHECK_TOTAL_TIMEOUT_MS, 20_000, "契约要求 20s");
        // 读进局部再断言：clippy 的 assertions-on-constants 不许直接比较两个常量，
        // 但这两个值本来就是**编译期契约**，比的就是它们。
        let (total, per_hop) = (CORE_CHECK_TOTAL_TIMEOUT_MS, GITHUB_FETCH_TIMEOUT_MS);
        assert!(
            total < per_hop * MAX_HOPS,
            "总超时 {total}ms 没有比逐跳叠加的最坏值 {}ms 更紧 —— 它就没在兜任何底",
            per_hop * MAX_HOPS
        );
        // 超时码必须与其它失败码可区分（处置不同：超时该引导配加速，网络失败该重试）。
        for other in [
            CODE_HTTP_UNAVAILABLE,
            CODE_NO_BACKUP,
            CODE_FORK_BLOCKED,
            CODE_CORE_DIR_UNAVAILABLE,
        ] {
            assert_ne!(CODE_CHECK_TIMEOUT, other);
        }
    }

    /// 🟡 **调用点守卫**：`core_update_check` 必须被总超时包着。
    ///
    /// 纯单测测不到（命令持 `State<'_, AppRuntime>`，且真超时要 20s 挂钟）。
    /// **变异探针**：把 `tokio::time::timeout(...)` 从命令体里删掉 ⇒ 本条转红。
    #[test]
    fn core_update_check_is_wrapped_in_a_total_timeout() {
        let src = include_str!("updater.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "pub async fn core_update_check(");
        assert!(
            body.contains("tokio::time::timeout"),
            "总超时被摘掉了 —— 逐跳 15s × 6 跳可跑到 90s，契约要求 20s 兜底"
        );
        assert!(
            body.contains("CORE_CHECK_TOTAL_TIMEOUT_MS"),
            "超时时长必须取自具名常量（写死字面量会与常量/测试漂移）"
        );
        assert!(
            body.contains("CODE_CHECK_TIMEOUT"),
            "超时必须返回可辨识错误码，不得折叠进泛化网络失败"
        );
    }
}
