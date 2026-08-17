//! mini 更新弹窗的状态载荷 + 会话编排（移植自 上游 `UpdateService` 的 popup 族）。
//!
//! 移植来源（`src/main/services/UpdateService.ts` + `update-popup-layout.ts`）：
//!  - `createUpdatePopup:541-631`：建窗 + 复用分支 + 初始态下发。
//!  - `sendPopupState:636-647`：写 `lastPopupState` + 推 renderer + 按 phase 改窗高。
//!  - `showUpdateDialog:486-503`：互斥闸 + remind 态 + 动作等待。
//!  - `popupHeightFor:18-28`（`update-popup-layout.ts`）：四态窗高。
//!
//! # 本模块存在的理由：#300/#301 的不变式需要一个「结构上无法违反」的落点
//!
//! 上游 issue #300（v4.2.3 全平台 remind 态 100% 必现白屏挂死窗）的根因链
//! （取证见 vault 里「更新弹窗『稍后提醒』留白」那份修复记录）：
//!
//! ```text
//! createUpdatePopup(新建路径) —— 全程无 sendPopupState
//!   did-finish-load 只重放 lastPopupState，而 lastPopupState 仅在 sendPopupState 内写入
//!   → 新建路径从不调 sendPopupState → lastPopupState 恒 null → 重放条件 false
//!   → renderer onState 永不触发 → 页面永空 → 用户看到 frameless 实色底、无按钮、Esc 失效、无法关闭
//! ```
//!
//! PR #301 的修法是在建窗末尾补一行 `this.sendPopupState(state)`。**那一行是可以再次被删掉的** ——
//! #300 本身就是 PR #292 把「首次加载后下发」替换成「崩溃后重放」时丢掉初始下发引入的回归，
//! 讽刺的是那次改动的注释目的正是「避免空白挂死窗」。**同一个类别的 bug 在同一个文件上复发过一次，
//! 说明「记得调用某个方法」不是一条能长期成立的不变式。**
//!
//! 本移植不复制那条「记得调用」的约定，改为把它**编码进类型**：
//!
//!  1. 页面的初始态经 [`PopupSession::open`] 产出的 [`PopupBootstrap`] **注入文档本身**
//!     （Tauri `initialization_script`，页面 boot 时同步可读）——而非建窗后再 push IPC。
//!     于是「窗口存在但从未拿到状态」**不再是一个可达状态**：没有 bootstrap 就没有页面。
//!     这比 #301 的单行 seed 严格更强：#301 仍依赖一次 IPC push 及时送达（早发即丢，靠重放兜底），
//!     而 bootstrap 根本不经 IPC，**无竞态可言**。
//!  2. [`PopupSession::open`] 是产出 bootstrap 的**唯一**入口，且它必然写 `last_state`
//!     → `did-finish-load` 重放（[`PopupSession::replay`]）恒有料可放，覆盖 reload / renderer 崩溃重建。
//!  3. 于是宿主层「建窗时忘了下发初始态」在编译期就写不出来：建窗需要 script，script 只能来自 `open`。
//!
//! Polaris 的 push 通道仍保留（[`PopupSession::send_state`]）用于**后续**状态流转（progress 百分比等），
//! 语义与上游一致：先写 `last_state`、再推 renderer（对齐 `UpdateService.ts:637` 的「写在 destroyed 检查之前」）。

use crate::state::PopupPhase;

/// 弹窗宽度（= 上游 `UPDATE_POPUP_WIDTH`，`update-popup-layout.ts:11`）。
pub const POPUP_WIDTH: u32 = 380;

/// 按阶段取弹窗高度（移植自 上游 `popupHeightFor`，`update-popup-layout.ts:18-28`）。
///
/// 四态高度逐字对齐上游：`remind`=184 / `error`=152 / `progress`|`done`=116。
#[must_use]
pub fn popup_height_for(phase: PopupPhase) -> u32 {
    match phase {
        PopupPhase::Remind => 184,
        PopupPhase::Error => 152,
        PopupPhase::Progress | PopupPhase::Done => 116,
    }
}

/// `done` 态自动关窗延迟（ms）（= 上游 `UpdateService.ts:772` 的 800ms）。
pub const DONE_AUTO_CLOSE_MS: u64 = 800;

/// 弹窗状态载荷（主 → 弹窗）。
///
/// 移植自 上游 `UpdatePopupState`（`shared/types/update.ts`）。字段经 serde 转 camelCase
/// 与前端契约对齐（前端 `ui/src/shared/types/update.ts` 按本结构重建）。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePopupState {
    /// 当前阶段（决定布局与窗高）。
    pub phase: PopupPhase,
    /// 目标新版本号（remind 态展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 当前版本号（remind 态展示，Polaris 形如 `v4.2.3`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    /// 下载进度百分比（progress/done 态，0-100）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
    /// 已下载/总量文案（progress 态，形如 `3.2 MB / 48 MB`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_text: Option<String>,
    /// 是否走镜像下载（progress 态角标；= 上游 `mirror` 标记）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mirror: bool,
    /// 错误文案（error 态）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

impl UpdatePopupState {
    /// remind 态（唯一入口态——#300 恰好杀死的就是它）。
    #[must_use]
    pub fn remind(version: impl Into<String>, current_version: impl Into<String>) -> Self {
        Self {
            phase: PopupPhase::Remind,
            version: Some(version.into()),
            current_version: Some(current_version.into()),
            ..Self::default()
        }
    }

    /// progress 态（百分比 + 可选字节文案）。
    #[must_use]
    pub fn progress(percentage: u8) -> Self {
        Self {
            phase: PopupPhase::Progress,
            percentage: Some(percentage.min(100)),
            ..Self::default()
        }
    }

    /// done 态（下载 / 校验完成；宿主应在 [`DONE_AUTO_CLOSE_MS`] 后自动关窗）。
    ///
    /// `percentage` 恒 100：done 与 progress 共用同一条进度条 DOM，留 `None` 会让条子在最后一帧
    /// 掉回 0（上游 `done` 载荷同样带满值）。
    #[must_use]
    pub fn done() -> Self {
        Self {
            phase: PopupPhase::Done,
            percentage: Some(100),
            ..Self::default()
        }
    }

    /// error 态（携带本地化后的错误文案）。
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            phase: PopupPhase::Error,
            error_text: Some(text.into()),
            ..Self::default()
        }
    }

    /// 本状态对应的窗高。
    #[must_use]
    pub fn height(&self) -> u32 {
        popup_height_for(self.phase)
    }
}

/// 弹窗动作（弹窗 → 主）。
///
/// 移植自 上游 `UpdatePopupAction`（`shared/types/update.ts:46-54`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PopupAction {
    /// 立即更新（remind → progress）。
    Update,
    /// 稍后（关窗，本次不再提醒）。
    Later,
    /// 跳过此版本（写 skipped 版本号）。
    Skip,
    /// 查看发布说明（开浏览器；**不 resolve 等待**，弹窗停在 remind）。
    ViewLog,
    /// 取消下载（progress 态）。
    Cancel,
    /// 重试（error → progress）。
    Retry,
    /// 手动下载（开浏览器；仅 https URL 放行）。
    ManualDownload,
    /// 关闭弹窗。
    Close,
}

impl PopupAction {
    /// 该动作是否**不**结束等待（= 上游 `viewLog` 分支：开页面但不 resolve，弹窗停在 remind）。
    ///
    /// 移植自 `UpdateService.ts:683-686`。
    #[must_use]
    pub fn is_non_resolving(self) -> bool {
        matches!(self, Self::ViewLog)
    }

    /// 给定阶段下该动作是否合法（= 上游 `awaitPopupAction(valid)` 白名单，`UpdateService.ts:712-717`）。
    ///
    /// 非法动作**静默忽略**（不报错、不改状态），对齐上游语义。
    #[must_use]
    pub fn is_valid_for(self, phase: PopupPhase) -> bool {
        match phase {
            // remind：update / later / skip（+ viewLog 非 resolving）
            PopupPhase::Remind => matches!(
                self,
                Self::Update | Self::Later | Self::Skip | Self::ViewLog
            ),
            // progress：仅 cancel
            PopupPhase::Progress => matches!(self, Self::Cancel),
            // error：retry / manualDownload / close
            PopupPhase::Error => matches!(self, Self::Retry | Self::ManualDownload | Self::Close),
            // done：800ms 后自动关窗，用户无按钮可点（仅容 close 兜底）
            PopupPhase::Done => matches!(self, Self::Close),
        }
    }
}

/// 建窗引导载荷：注入页面文档的初始状态（**替代** Polaris 建窗后 push IPC 的做法）。
///
/// 由 [`PopupSession::open`] 唯一产出。宿主层把 [`PopupBootstrap::init_script`] 交给
/// Tauri `WebviewWindowBuilder::initialization_script`，页面 boot 时同步读
/// `window.__POLARIS_UPDATE_POPUP_INITIAL__` 即可渲染首帧 —— 无 IPC、无竞态、无「早发即丢」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupBootstrap {
    /// 注入文档的初始化脚本（定义 `window.__POLARIS_UPDATE_POPUP_INITIAL__`）。
    pub init_script: String,
    /// 建窗时应使用的窗口宽度。
    pub width: u32,
    /// 建窗时应使用的窗口高度（按初始 phase）。
    pub height: u32,
}

/// 状态推送通道（宿主注入真实 Tauri `emit_to`；测试注入记录器）。
///
/// 移植自 上游 `webContents.send(IPC_CHANNELS.UPDATE_POPUP_STATE, state)`。
pub trait PopupTransport {
    /// 推一条状态到弹窗 renderer。
    ///
    /// # Errors
    ///
    /// 窗口已销毁 / IPC 失败。上游对此**静默吞掉**（`sendPopupState` 先写 `lastPopupState` 再检查
    /// destroyed），本 trait 返回 [`Result`] 让宿主决定记日志与否——但
    /// [`PopupSession::send_state`] 保证**先写 `last_state` 再推**，故推送失败不影响重放兜底。
    fn send_state(&self, state: &UpdatePopupState) -> Result<(), String>;

    /// 按 phase 调整窗口内容高度（= 上游 `sendPopupState` 内的 `setContentSize`，`:641-645`）。
    ///
    /// # Errors
    ///
    /// 窗口已销毁 / 平台调用失败。
    fn set_content_height(&self, height: u32) -> Result<(), String>;
}

/// 弹窗会话：持有 `last_state` + 推送通道，编排「建窗初始态 / 状态流转 / 重放」。
///
/// **`last_state` 的写入点唯一**（[`Self::open`] 与 [`Self::send_state`]），且二者都在推送**之前**写——
/// 这正是 #300 的根因所在（上游 `lastPopupState` 只在 `sendPopupState` 内写，而建窗路径不调它）。
#[derive(Debug)]
pub struct PopupSession<T: PopupTransport> {
    transport: T,
    last_state: Option<UpdatePopupState>,
}

impl<T: PopupTransport> PopupSession<T> {
    /// 构造会话（尚未建窗，`last_state` 为空）。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            last_state: None,
        }
    }

    /// **新建窗口路径的唯一入口**：产出注入页面的 bootstrap，并落 `last_state`。
    ///
    /// #300/#301 不变式在此结构性成立 —— 宿主要建窗就必须拿 [`PopupBootstrap::init_script`]，
    /// 而拿它就必然经过本方法，本方法必然写 `last_state`。**「建窗但没下发初始态」写不出来。**
    ///
    /// 注意：本方法**不**经 [`PopupTransport::send_state`] 推 IPC —— 初始态走文档注入，不走 IPC。
    /// 这与上游 #301 的 `sendPopupState(state)` 单行 seed 语义等价（都让首帧有料 + 让重放有料），
    /// 但消灭了「push 早于 listener 注册」的整类竞态（= #301 文档里列为「可选 C，后续加固」的那条，
    /// 本移植直接做进建窗路径）。
    pub fn open(&mut self, state: UpdatePopupState) -> PopupBootstrap {
        let height = state.height();
        let init_script = Self::build_init_script(&state);
        // 先落 last_state：did-finish-load / renderer 崩溃重建时靠它重放（#300 的 lastPopupState 恒 null 即死在这）。
        self.last_state = Some(state);
        PopupBootstrap {
            init_script,
            width: POPUP_WIDTH,
            height,
        }
    }

    /// 复用已存在弹窗时的状态下发（= 上游 `createUpdatePopup:542-545` 复用分支）。
    ///
    /// # Errors
    ///
    /// 透传 [`PopupTransport::send_state`] 失败（`last_state` 已先写，重放仍可兜底）。
    pub fn reuse(&mut self, state: UpdatePopupState) -> Result<(), String> {
        self.send_state(state)
    }

    /// 推一条新状态：**先写 `last_state`，再推 renderer**（对齐 `UpdateService.ts:637` 的写入时序）。
    ///
    /// 窗高随 phase 变化时同步调整（= 上游 `:641-645` 的 `setContentSize` 差量更新）。
    ///
    /// # Errors
    ///
    /// 透传推送失败。**失败也已写 `last_state`** —— 这是上游把 `lastPopupState = state` 放在
    /// destroyed 检查之前的原因：推送失败不能让重放失去依据。
    ///
    /// # 邀请过的版本号是**会话级**事实，不是 phase 级事实
    ///
    /// [`UpdatePopupState::progress`] / [`UpdatePopupState::done`] / [`UpdatePopupState::error`]
    /// 三个构造点都只填自己那一档要用的字段（`..Self::default()` ⇒ `version: None`）。不继承的话，
    /// **一离开 remind，「这次弹窗邀请的是哪一版」就在会话里蒸发了** —— 而下游有两个消费者要它：
    ///
    ///  1. `commands/updater.rs` 的 `Update` / `Retry` 分支：复查回来要与邀请版本逐字对账。
    ///     `Retry` 按 [`PopupAction::is_valid_for`] **只在 `Error` 态合法**，那里 `version` 恒 `None`
    ///     ⇒ 对账恒判「变了」⇒ 退回 remind 而**一个字节都不下**，「重试」实际变成「返回」。
    ///  2. 同文件 `ManualDownload` 分支（同样只在 `Error` 态合法）：拿它去拼该版本的 release tag 页。
    ///     `None` 时回落泛列表页 —— #311 修的正是「找不到对应版本说明」，而 error 态恰好是最需要
    ///     它的一屏。
    ///
    /// 只继承 `version`，**不继承 `current_version`**：前者有决策依赖它（上面两条），后者今天在
    /// remind 之外无任何消费者，而 remind 恒自带两者。为对称而继承是给未来的猜测付税。
    ///
    /// 继承只在 `version.is_none()` 时发生 ⇒ [`UpdatePopupState::remind`] 恒显式带版本，覆盖关系
    /// 明确（新邀请永远压过旧记忆）。[`Self::open`] 不需要这一层：它只被 `update_popup_show` 以
    /// remind 态调用，且那时会话刚 `new` 出来、`last_state` 为空。
    pub fn send_state(&mut self, mut state: UpdatePopupState) -> Result<(), String> {
        if state.version.is_none() {
            state.version = self.last_state.as_ref().and_then(|s| s.version.clone());
        }
        let height_changed = self.last_state.as_ref().map(|s| s.height()) != Some(state.height());
        let height = state.height();
        // 先写：任何推送失败都不得让 last_state 失同步（#300 的核心教训）。
        self.last_state = Some(state);
        if height_changed {
            self.transport.set_content_height(height)?;
        }
        // unwrap 安全：上一行刚写入 Some。
        let s = self.last_state.as_ref().expect("last_state just written");
        self.transport.send_state(s)
    }

    /// 重放最后一次状态（= 上游 `did-finish-load` 的 `lastPopupState` 重放，`:596-599`）。
    ///
    /// 用 `.on` 而非 `.once` 语义：renderer 每次 reload / 崩溃重建都重放，覆盖崩溃自愈。
    /// 返回 `Ok(false)` = 无可重放状态（本移植下**不可达**：`open` 必然已 seed；保留返回值
    /// 供宿主断言，作为不变式被破坏时的哨兵）。
    ///
    /// # Errors
    ///
    /// 透传推送失败。
    pub fn replay(&self) -> Result<bool, String> {
        match &self.last_state {
            Some(s) => {
                self.transport.send_state(s)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 最后一次状态（宿主/测试断言用）。
    #[must_use]
    pub fn last_state(&self) -> Option<&UpdatePopupState> {
        self.last_state.as_ref()
    }

    /// 会话是否已 seed（#300 不变式的直接断言点）。
    #[must_use]
    pub fn is_seeded(&self) -> bool {
        self.last_state.is_some()
    }

    /// 清状态（关窗；= 上游 `closed` 事件后重置）。
    pub fn reset(&mut self) {
        self.last_state = None;
    }

    /// 取推送通道（宿主取回以关窗等）。
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// 构造注入脚本：把初始状态序列化进 `window.__POLARIS_UPDATE_POPUP_INITIAL__`。
    ///
    /// 序列化用 `serde_json`，故 JSON 内的 `<`/`</script>`/引号等已被转义为合法 JSON 字面量；
    /// 且本脚本经 Tauri `initialization_script` 注入（**不是**拼进 HTML 文本），不存在
    /// `</script>` 提前闭合的注入面。状态内容全部来自本地 manifest / 版本号 / 本地化文案，
    /// 但仍按不可信输入处理（`version` 取自远端 GitHub tag）。
    fn build_init_script(state: &UpdatePopupState) -> String {
        // serde_json 对本结构不可能失败（全 Plain Old Data，无 Map<非字符串键>/非有限浮点）；
        // 万一失败也必须给页面一个可渲染的初始态 —— 绝不产出「无 bootstrap 的窗」（那正是 #300）。
        let json = serde_json::to_string(state).unwrap_or_else(|_| {
            r#"{"phase":"error","errorText":"popup bootstrap serialization failed"}"#.to_string()
        });
        format!("window.__POLARIS_UPDATE_POPUP_INITIAL__ = {json};")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 记录型 transport：把推送过的状态与窗高留痕，供断言。
    #[derive(Debug, Default)]
    struct RecordingTransport {
        sent: RefCell<Vec<UpdatePopupState>>,
        heights: RefCell<Vec<u32>>,
        fail: bool,
    }

    impl RecordingTransport {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }
    }

    impl PopupTransport for RecordingTransport {
        fn send_state(&self, state: &UpdatePopupState) -> Result<(), String> {
            if self.fail {
                return Err("mock: window destroyed".into());
            }
            self.sent.borrow_mut().push(state.clone());
            Ok(())
        }

        fn set_content_height(&self, height: u32) -> Result<(), String> {
            if self.fail {
                return Err("mock: window destroyed".into());
            }
            self.heights.borrow_mut().push(height);
            Ok(())
        }
    }

    // ── #300/#301 不变式 ──

    #[test]
    fn open_seeds_last_state_the_300_invariant() {
        // #300 的根因：新建窗口路径从不下发初始态 → lastPopupState 恒 null → 重放条件 false → 页面永空。
        // 本移植：open 是 bootstrap 的唯一产地，必然 seed。
        let mut s = PopupSession::new(RecordingTransport::default());
        assert!(!s.is_seeded(), "建窗前不应有状态");

        let boot = s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));

        assert!(s.is_seeded(), "#300 不变式：open 后 last_state 必须有料");
        assert_eq!(s.last_state().unwrap().phase, PopupPhase::Remind);
        // bootstrap 必须真的带上初始态（页面首帧的唯一来源）。
        assert!(boot
            .init_script
            .contains("__POLARIS_UPDATE_POPUP_INITIAL__"));
        assert!(boot.init_script.contains("\"phase\":\"remind\""));
        assert!(boot.init_script.contains("\"version\":\"4.2.4\""));
    }

    #[test]
    fn open_bootstrap_carries_remind_geometry() {
        // remind 是 #300 唯一中招的入口态：窗高必须是 184，宽 380。
        let mut s = PopupSession::new(RecordingTransport::default());
        let boot = s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));
        assert_eq!(boot.width, POPUP_WIDTH);
        assert_eq!(boot.height, 184, "remind 态窗高（对齐 popupHeightFor）");
    }

    #[test]
    fn replay_after_open_always_has_payload() {
        // did-finish-load 重放：#300 里这一步恒 false（lastPopupState null）。本移植恒有料。
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));

        assert!(s.replay().unwrap(), "open 之后重放必须有料可放");
        let sent = s.transport().sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].phase, PopupPhase::Remind);
    }

    #[test]
    fn replay_without_open_reports_empty_rather_than_lying() {
        // 哨兵：未 open 就重放 → false（本移植下不可达；若为 true 说明 last_state 被别处写脏）。
        let s: PopupSession<RecordingTransport> = PopupSession::new(RecordingTransport::default());
        assert!(!s.replay().unwrap());
        assert!(s.transport().sent.borrow().is_empty());
    }

    #[test]
    fn send_state_writes_last_state_even_when_push_fails() {
        // 上游把 `lastPopupState = state` 放在 destroyed 检查之前（UpdateService.ts:637），
        // 正是为了「推送失败也不让重放失去依据」。此处钉住该时序。
        let mut s = PopupSession::new(RecordingTransport::failing());
        s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));

        let r = s.send_state(UpdatePopupState::progress(42));
        assert!(r.is_err(), "mock transport 应报失败");
        // 关键：推送失败，但 last_state 已推进到 progress。
        assert_eq!(s.last_state().unwrap().phase, PopupPhase::Progress);
        assert_eq!(s.last_state().unwrap().percentage, Some(42));
    }

    // ── 邀请过的版本号：会话级事实，跨 phase 不蒸发 ──

    /// 🟡 **不变量：一次弹窗会话邀请过的版本号，跨 phase 一直在。**
    ///
    /// `progress` / `error` / `done` 三个构造点都不填 `version`。不继承的话，会话一离开 remind 就
    /// 忘了自己邀请过谁，而 `Error` 态恰恰挂着**两个**需要它的动作（[`PopupAction::is_valid_for`]：
    /// `Retry` 与 `ManualDownload`）：
    ///  - `Retry`：宿主拿它与复查回来的版本对账。恒 `None` ⇒ 恒判「变了」⇒ 退回 remind、一个字节
    ///    都不下，「重试」退化成「返回」。
    ///  - `ManualDownload`：拿它拼该版本的 release tag 页；`None` 回落泛列表页（#311 修的就是这个）。
    ///
    /// **变异探针**：删掉 `send_state` 里那三行继承 ⇒ 本条转红（且 `error` 那格恰好复刻真实故障）。
    #[test]
    fn the_invited_version_survives_phase_changes() {
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("v1.2.0", "v1.1.0"));

        for (step, state) in [
            ("progress", UpdatePopupState::progress(30)),
            ("error", UpdatePopupState::error("网络中断")),
            ("done", UpdatePopupState::done()),
        ] {
            s.send_state(state).unwrap();
            assert_eq!(
                s.last_state().unwrap().version.as_deref(),
                Some("v1.2.0"),
                "{step} 态把邀请版本弄丢了 —— error 态丢它会让「重试」永不下载"
            );
        }

        // 推给 renderer 的载荷也得带着它（宿主读的是 `popup_state()`，而它就是 last_state 的克隆；
        // 这里连推送侧一起钉，免得将来有人只改 last_state 不改推送）。
        let sent = s.transport().sent.borrow();
        assert!(
            sent.iter().all(|p| p.version.as_deref() == Some("v1.2.0")),
            "推送出去的状态里版本号丢了：{sent:?}"
        );
    }

    /// 🟡 **新邀请压过旧记忆：`remind` 恒显式带版本，不被继承值污染。**
    ///
    /// 继承只在 `version.is_none()` 时发生。这条钉住「同一会话里换了个版本重新提醒」时不会举着
    /// 上一轮的版本号 —— 本批 `update_popup_action` 的对账不一致分支正是这么用的。
    #[test]
    fn a_fresh_remind_overrides_the_inherited_version() {
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("v1.2.0", "v1.1.0"));
        s.send_state(UpdatePopupState::progress(10)).unwrap();
        s.send_state(UpdatePopupState::remind("v1.3.0", "v1.1.0"))
            .unwrap();
        assert_eq!(
            s.last_state().unwrap().version.as_deref(),
            Some("v1.3.0"),
            "新一轮 remind 必须覆盖继承来的旧版本号"
        );
    }

    // ── 状态流转 / 窗高 ──

    #[test]
    fn send_state_adjusts_height_only_on_phase_height_change() {
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("4.2.4", "v4.2.3")); // 184
        s.send_state(UpdatePopupState::progress(10)).unwrap(); // 116 → 改高
        s.send_state(UpdatePopupState::progress(20)).unwrap(); // 116 → 同高，不改
        s.send_state(UpdatePopupState::error("boom")).unwrap(); // 152 → 改高

        let heights = s.transport().heights.borrow();
        assert_eq!(
            *heights,
            vec![116, 152],
            "仅在窗高真变化时调用 setContentSize"
        );
    }

    #[test]
    fn reuse_branch_sends_state_immediately() {
        // 复用分支（Polaris createUpdatePopup:542-545）：直接下发 + 早返回。
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));
        s.transport().sent.borrow_mut().clear();

        s.reuse(UpdatePopupState::progress(5)).unwrap();
        let sent = s.transport().sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].phase, PopupPhase::Progress);
    }

    #[test]
    fn reset_clears_state_on_close() {
        let mut s = PopupSession::new(RecordingTransport::default());
        s.open(UpdatePopupState::remind("4.2.4", "v4.2.3"));
        assert!(s.is_seeded());
        s.reset();
        assert!(!s.is_seeded());
    }

    // ── 布局 / 载荷 ──

    #[test]
    fn popup_heights_match_upstream_layout() {
        // 逐字对齐 update-popup-layout.ts:18-28。
        assert_eq!(popup_height_for(PopupPhase::Remind), 184);
        assert_eq!(popup_height_for(PopupPhase::Error), 152);
        assert_eq!(popup_height_for(PopupPhase::Progress), 116);
        assert_eq!(popup_height_for(PopupPhase::Done), 116);
        assert_eq!(POPUP_WIDTH, 380);
        assert_eq!(DONE_AUTO_CLOSE_MS, 800);
    }

    #[test]
    fn state_serializes_camel_case_lowercase_phase() {
        // 前端契约：phase 小写、字段 camelCase。
        let s = UpdatePopupState {
            phase: PopupPhase::Progress,
            percentage: Some(37),
            bytes_text: Some("3.2 MB / 48 MB".into()),
            mirror: true,
            ..UpdatePopupState::default()
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"phase\":\"progress\""), "phase 必须小写: {j}");
        assert!(
            j.contains("\"bytesText\":\"3.2 MB / 48 MB\""),
            "camelCase: {j}"
        );
        assert!(j.contains("\"mirror\":true"));
        // None 字段不出现（前端按可选处理）。
        assert!(!j.contains("errorText"));
    }

    #[test]
    fn progress_percentage_clamped() {
        assert_eq!(UpdatePopupState::progress(200).percentage, Some(100));
    }

    #[test]
    fn done_state_is_full_and_matches_layout() {
        // done 与 progress 同高（116），进度条留满值——否则最后一帧掉回 0。
        let d = UpdatePopupState::done();
        assert_eq!(d.phase, PopupPhase::Done);
        assert_eq!(d.percentage, Some(100));
        assert_eq!(d.height(), 116);
        // done 态用户无按钮可点，仅容 close 兜底（上游白名单）。
        assert!(PopupAction::Close.is_valid_for(PopupPhase::Done));
        assert!(!PopupAction::Cancel.is_valid_for(PopupPhase::Done));
    }

    // ── 动作白名单 ──

    #[test]
    fn action_whitelist_per_phase_matches_upstream() {
        use PopupAction::{Cancel, Close, ManualDownload, Retry, Skip, Update, ViewLog};
        // remind：update / later / skip / viewLog
        assert!(Update.is_valid_for(PopupPhase::Remind));
        assert!(Skip.is_valid_for(PopupPhase::Remind));
        assert!(ViewLog.is_valid_for(PopupPhase::Remind));
        assert!(!Retry.is_valid_for(PopupPhase::Remind));
        // progress：仅 cancel
        assert!(Cancel.is_valid_for(PopupPhase::Progress));
        assert!(!Update.is_valid_for(PopupPhase::Progress));
        // error：retry / manualDownload / close
        assert!(Retry.is_valid_for(PopupPhase::Error));
        assert!(ManualDownload.is_valid_for(PopupPhase::Error));
        assert!(Close.is_valid_for(PopupPhase::Error));
        assert!(!Cancel.is_valid_for(PopupPhase::Error));
    }

    #[test]
    fn view_log_is_non_resolving() {
        // Polaris UpdateService.ts:683-686：viewLog 开页面但不 resolve，弹窗停在 remind。
        assert!(PopupAction::ViewLog.is_non_resolving());
        assert!(!PopupAction::Update.is_non_resolving());
        assert!(!PopupAction::Skip.is_non_resolving());
    }

    #[test]
    fn action_serde_camel_case() {
        assert_eq!(
            serde_json::to_string(&PopupAction::ManualDownload).unwrap(),
            "\"manualDownload\""
        );
        assert_eq!(
            serde_json::from_str::<PopupAction>("\"viewLog\"").unwrap(),
            PopupAction::ViewLog
        );
    }
}
