//! 4 态弹窗更新 UI 状态机（移植自 上游 `UpdateService` 的 popup/progress 状态）。
//!
//! Polaris 的更新 UI 状态散在两处：
//!   - `UpdateProgress.status`：`'idle' | 'checking' | 'downloading' | 'downloaded' | 'no-update' |
//!     'update-available' | 'error'`（`UpdateService.ts:77-81` + `updateProgress(...)` 各调用点）。
//!   - `UpdatePopupState.phase`：`'remind' | 'progress' | 'done' | 'error'`（4 态 Conduit mini 窗，
//!     `UpdateService.ts:591-644`，含进度镜像 / done 自动关窗 / error 重试循环）。
//!
//! 本 crate 把这两套运行时态规约到一个枚举，驱动 staged 更新 UI 反馈：用户可见的「下载中 → 已就绪 →
//! 验证中 → 安装中」全周期 + 错误态。**纯逻辑**：状态转移函数不触碰 UI/IPC，由上层（Tauri command）
//! 把 [`UpdateState`] 经 serde 推到 renderer。
//!
//! 移植纪律：保留 Polaris「错误可重试」「done 后清状态」语义；状态机非法转移显式报错（Polaris 靠运行时
//! 弹窗控制隐式约束，此处编译期固化）。

use std::fmt;

use thiserror::Error;

/// 弹窗阶段（移植自 `UpdatePopupState.phase`，`UpdateService.ts:591-644`）。
///
/// 供 renderer 决定弹窗布局/高度（上游 `popupHeightFor(phase)`）；与 [`UpdateState`] 的活跃态对应。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
// 前端契约：phase 恒小写（对齐 上游 `UpdatePopupState.phase` 的 'remind'|'progress'|'done'|'error'）。
// 与本枚举的 Display 实现同口径；serde 与 Display 双通道必须一致，勿只改其一。
#[serde(rename_all = "lowercase")]
pub enum PopupPhase {
    /// 提醒态：发现新版本，待用户决定（update / later / skip）。= 上游 `'remind'`。
    Remind,
    /// 进度态：下载/校验中，显示百分比。= 上游 `'progress'`。
    #[default]
    Progress,
    /// 完成态：下载/校验/落位完成（800ms 后自动关窗，对齐 Polaris done 态）。= 上游 `'done'`。
    ///
    /// **「完成」= 盘上真有一份包**：载荷必带落位路径（[`crate::popup::UpdatePopupState::done`]
    /// 的参数是必填的），故「什么都没下」在类型上就构造不出本态。成因见下面 [`Self::NoUpdate`]。
    Done,
    /// 无更新可下态：用户点了「更新」，复查回来却没有任何可下载的包。**上游没有这一档**。
    ///
    /// # 为什么必须独立成一档（而不是塞进 `Done` 或 `Error`）
    ///
    /// 复查判 `NoUpdate` 有五条可达成因（`crate::github::check_app_update`：无正式发布 / 不比当前新 /
    /// 该版本已被跳过 / 无适配本平台的资产；外加宿主层的平台不受支持），后端**分辨不出**是哪一条。
    /// 塞进现有四态只有两条路，两条都是假话：
    ///  - `Done` ⇒ 弹窗渲染「下载完成」+ 满格进度条，而一个字节都没下（2026-08-17 前的现状）；
    ///  - `Error` ⇒ 把「用户自己按过跳过」「已经是最新版」说成故障。
    ///
    /// 本态只陈述后端**确实知道**的那一件事：这次复查没有找到可下载的更新包。不猜成因，
    /// 也就没有任何一条成因会让这句话变假。
    NoUpdate,
    /// 错误态：失败，可重试/手动下载/关闭（Polaris error 态重试循环）。= 上游 `'error'`。
    Error,
}

impl PopupPhase {
    /// 全部阶段（**遍历用**）。
    ///
    /// 存在理由是让「加了一档但某处没跟上」由测试而不是人眼发现：判据面一旦写成手抄的字面量清单，
    /// 失效方向就是「加了变体忘了加进清单」——所有遍历型断言会**静默少跑一格**（绿，且无信息量）。
    /// 故本清单与真实变体表由 [`tests::all_lists_every_popup_phase_exactly_once`] 从 `Display` 的
    /// 穷尽 match 反查对账，清单落后于枚举必转红。
    pub const ALL: [Self; 5] = [
        Self::Remind,
        Self::Progress,
        Self::Done,
        Self::NoUpdate,
        Self::Error,
    ];
}

impl fmt::Display for PopupPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remind => f.write_str("remind"),
            Self::Progress => f.write_str("progress"),
            Self::Done => f.write_str("done"),
            Self::NoUpdate => f.write_str("noupdate"),
            Self::Error => f.write_str("error"),
        }
    }
}

/// 4 态更新状态机（核心）。
///
/// 对应任务规约：Checking（下载/检查中）/ Ready（已就绪待安装）/ Verifying（SHA256 校验中）/
/// Installing（原子替换中）/ Error（失败）。Idle 为初始/空闲态（= 上游 `'idle'` + `'no-update'` 的合并：
/// 尚未启动或确认无更新）。
///
/// 转移图（合法路径，其余返回 [`UpdateStateError::InvalidTransition`]）：
/// ```text
/// Idle ──start_check──▶ Checking ──found──▶ Ready
///  ▲                       │                   │
///  │                  (err/not_found)         │
///  │                       ▼                   ▼
///  └──── reset ◀── Idle ◀── Error ◀── start_install ◀─ start_verify ◀─ Verifying
///                                        │                                          │
///                                   (retry)                                  (err)
///                                        ▼                                          │
///                               Verifying ◀──────────────────────────── installed ─┘
///                                                                             ▲
///                                                                  Installing ─┘
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
pub enum UpdateState {
    /// 空闲/初始态：尚未检查或已重置（= 上游 `'idle'` + `'no-update'`）。
    #[default]
    Idle,
    /// 检查/下载中：检查更新或下载新核进行中（= 上游 `'checking'` + `'downloading'` + `'update-available'`）。
    Checking,
    /// 已就绪：新核已下载完成，待校验/安装（= 上游 `'downloaded'`）。对应 popup `Done`（短停留）/`Remind`（待用户）。
    Ready,
    /// 验证中：SHA256 校验进行中（staged 更新周期内的子阶段，Polaris 无独立态、合并在 progress 内；
    /// 本 crate 显式分出以驱动 UI「验证中」文案，便于区分下载完成 vs 校验通过）。
    Verifying,
    /// 安装中：原子替换 tmp→rename + 版本簿记进行中（= Polaris installCoreFromDir 落位窗口；
    /// 对齐 popup `Progress`，但本 crate 独立分出以标记「不可中断」窗口）。
    Installing,
    /// 错误态：失败，可重试或重置回 Idle（= 上游 `'error'`）。携带的 message 由 [`UpdateState::with_error`] 设置。
    Error,
}

impl UpdateState {
    /// 是否处于活跃（非空闲、非错误）态。
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Ready | Self::Verifying | Self::Installing
        )
    }

    /// 是否为终态（需要用户介入或已完成的稳定态）。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Error)
    }

    /// 映射到弹窗阶段（移植自 Polaris popup phase 选择逻辑）。
    ///
    /// [`PopupPhase::NoUpdate`] **不在本映射的值域里**，这是刻意的：它描述的是「复查回来没有可下的
    /// 东西」，那不是 staged 更新周期里的任何一步，本枚举里没有与之对应的态（`Idle` 是「尚未开始
    /// 或已重置」，含义不同）。它由 `commands::updater::update_popup_action` 直接产出。
    #[must_use]
    pub fn popup_phase(self) -> PopupPhase {
        match self {
            Self::Idle | Self::Checking => PopupPhase::Progress,
            Self::Ready => PopupPhase::Done,
            Self::Verifying | Self::Installing => PopupPhase::Progress,
            Self::Error => PopupPhase::Error,
        }
    }

    /// 开始检查/下载：Idle/Error → Checking（= Polaris checkForUpdate / downloadUpdate 入口）。
    ///
    /// # Errors
    ///
    /// 活跃态（Checking/Ready/Verifying/Installing）再触发检查 → 非法转移（Polaris 靠 `isUpdating` 重入闸
    /// 隐式防并发，此处编译期固化）。
    pub fn start_check(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Idle | Self::Error => Ok(Self::Checking),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Checking,
            }),
        }
    }

    /// 发现新版本（检查/下载完成）：Checking → Ready。
    ///
    /// # Errors
    ///
    /// 非 Checking 态调用 → 非法转移。
    pub fn found(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Checking => Ok(Self::Ready),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Ready,
            }),
        }
    }

    /// 未发现/不可用：Checking → Idle（= 上游 `'no-update'` 收口）。
    ///
    /// # Errors
    ///
    /// 非 Checking 态调用 → 非法转移。
    pub fn not_found(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Checking => Ok(Self::Idle),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Idle,
            }),
        }
    }

    /// 开始 SHA256 校验：Ready → Verifying（staged 周期内的校验子阶段）。
    ///
    /// # Errors
    ///
    /// 非 Ready 态调用 → 非法转移。
    pub fn start_verify(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Ready => Ok(Self::Verifying),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Verifying,
            }),
        }
    }

    /// 校验通过，开始安装：Verifying → Installing（原子替换窗口）。
    ///
    /// # Errors
    ///
    /// 非 Verifying 态调用 → 非法转移。
    pub fn start_install(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Verifying => Ok(Self::Installing),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Installing,
            }),
        }
    }

    /// 安装完成：Installing → Idle（落位成功，回到空闲待下次检查；对齐 Polaris done 后关窗）。
    ///
    /// # Errors
    ///
    /// 非 Installing 态调用 → 非法转移。
    pub fn installed(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Installing => Ok(Self::Idle),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Idle,
            }),
        }
    }

    /// 失败：任意活跃态 → Error（对齐 Polaris handleError → error 态；错误详情由调用方记录到日志/UI）。
    ///
    /// 注意：Idle → Error 也允许（= Polaris 启动检查失败直接 error 态），但 Error → Error 是 no-op。
    #[must_use]
    pub fn with_error(self) -> Self {
        Self::Error
    }

    /// 重置回空闲：任意态 → Idle（= Polaris 关窗/重置进度）。
    #[must_use]
    pub fn reset(self) -> Self {
        Self::Idle
    }

    /// 重试：Error → Checking（= Polaris error 态「重试」按钮 → 重新下载/校验）。
    ///
    /// # Errors
    ///
    /// 非 Error 态调用 → 非法转移（活跃态不能用 retry，应直接推进）。
    pub fn retry(self) -> Result<Self, UpdateStateError> {
        match self {
            Self::Error => Ok(Self::Checking),
            other => Err(UpdateStateError {
                from: other,
                to: Self::Checking,
            }),
        }
    }
}

impl fmt::Display for UpdateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Checking => f.write_str("checking"),
            Self::Ready => f.write_str("ready"),
            Self::Verifying => f.write_str("verifying"),
            Self::Installing => f.write_str("installing"),
            Self::Error => f.write_str("error"),
        }
    }
}

/// 状态机非法转移错误。
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid state transition: {from} -> {to}")]
pub struct UpdateStateError {
    pub from: UpdateState,
    pub to: UpdateState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        assert_eq!(UpdateState::default(), UpdateState::Idle);
    }

    #[test]
    fn happy_path_full_cycle() {
        // Idle → Checking → Ready → Verifying → Installing → Idle
        let s = UpdateState::Idle;
        let s = s.start_check().unwrap();
        assert_eq!(s, UpdateState::Checking);
        let s = s.found().unwrap();
        assert_eq!(s, UpdateState::Ready);
        let s = s.start_verify().unwrap();
        assert_eq!(s, UpdateState::Verifying);
        let s = s.start_install().unwrap();
        assert_eq!(s, UpdateState::Installing);
        let s = s.installed().unwrap();
        assert_eq!(s, UpdateState::Idle);
    }

    #[test]
    fn not_found_returns_idle() {
        let s = UpdateState::Idle
            .start_check()
            .unwrap()
            .not_found()
            .unwrap();
        assert_eq!(s, UpdateState::Idle);
    }

    #[test]
    fn error_can_retry() {
        let s = UpdateState::Idle.start_check().unwrap().with_error();
        assert_eq!(s, UpdateState::Error);
        let s = s.retry().unwrap();
        assert_eq!(s, UpdateState::Checking);
    }

    #[test]
    fn any_state_can_reset() {
        for s in [
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::Ready,
            UpdateState::Verifying,
            UpdateState::Installing,
            UpdateState::Error,
        ] {
            assert_eq!(s.reset(), UpdateState::Idle, "reset from {s:?}");
        }
    }

    #[test]
    fn any_state_can_error() {
        for s in [
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::Ready,
            UpdateState::Verifying,
            UpdateState::Installing,
        ] {
            assert_eq!(s.with_error(), UpdateState::Error, "error from {s:?}");
        }
    }

    #[test]
    fn invalid_transitions_rejected() {
        // Idle 不能直接 found（须先 start_check）。
        assert_eq!(
            UpdateState::Idle.found(),
            Err(UpdateStateError {
                from: UpdateState::Idle,
                to: UpdateState::Ready,
            })
        );
        // Ready 不能再 start_check（活跃态防重入）。
        assert_eq!(
            UpdateState::Ready.start_check(),
            Err(UpdateStateError {
                from: UpdateState::Ready,
                to: UpdateState::Checking,
            })
        );
        // Checking 不能直接 start_install（须先 found → start_verify）。
        assert_eq!(
            UpdateState::Checking.start_install(),
            Err(UpdateStateError {
                from: UpdateState::Checking,
                to: UpdateState::Installing,
            })
        );
        // Verifying 不能直接 installed（须先 start_install）。
        assert_eq!(
            UpdateState::Verifying.installed(),
            Err(UpdateStateError {
                from: UpdateState::Verifying,
                to: UpdateState::Idle,
            })
        );
        // 非 Error 态不能 retry。
        assert_eq!(
            UpdateState::Idle.retry(),
            Err(UpdateStateError {
                from: UpdateState::Idle,
                to: UpdateState::Checking,
            })
        );
    }

    #[test]
    fn is_active_and_terminal() {
        assert!(!UpdateState::Idle.is_active());
        assert!(UpdateState::Checking.is_active());
        assert!(UpdateState::Ready.is_active());
        assert!(UpdateState::Verifying.is_active());
        assert!(UpdateState::Installing.is_active());
        assert!(!UpdateState::Error.is_active());

        assert!(UpdateState::Ready.is_terminal());
        assert!(UpdateState::Error.is_terminal());
        assert!(!UpdateState::Idle.is_terminal());
        assert!(!UpdateState::Checking.is_terminal());
    }

    #[test]
    fn popup_phase_mapping() {
        // 移植自 Polaris popup phase 选择：done 态对应 Ready，error 对应 Error，其余 progress。
        assert_eq!(UpdateState::Idle.popup_phase(), PopupPhase::Progress);
        assert_eq!(UpdateState::Checking.popup_phase(), PopupPhase::Progress);
        assert_eq!(UpdateState::Ready.popup_phase(), PopupPhase::Done);
        assert_eq!(UpdateState::Verifying.popup_phase(), PopupPhase::Progress);
        assert_eq!(UpdateState::Installing.popup_phase(), PopupPhase::Progress);
        assert_eq!(UpdateState::Error.popup_phase(), PopupPhase::Error);
    }

    #[test]
    fn display_strings() {
        assert_eq!(UpdateState::Idle.to_string(), "idle");
        assert_eq!(UpdateState::Checking.to_string(), "checking");
        assert_eq!(UpdateState::Ready.to_string(), "ready");
        assert_eq!(UpdateState::Verifying.to_string(), "verifying");
        assert_eq!(UpdateState::Installing.to_string(), "installing");
        assert_eq!(UpdateState::Error.to_string(), "error");
        assert_eq!(PopupPhase::Remind.to_string(), "remind");
        assert_eq!(PopupPhase::Progress.to_string(), "progress");
        assert_eq!(PopupPhase::Done.to_string(), "done");
        assert_eq!(PopupPhase::NoUpdate.to_string(), "noupdate");
        assert_eq!(PopupPhase::Error.to_string(), "error");
    }

    /// 🟡 **[`PopupPhase::ALL`] 必须逐个覆盖真实变体表。**
    ///
    /// `ALL` 是所有遍历型判据（弹窗载荷的随行事实门、镜像闸门、跨语言 phase 对拍）的取材源。
    /// 它一旦落后于枚举，那些门就**静默少跑一格** —— 新加的那一档没有任何断言碰过，而全部转绿。
    /// 故此处不比对手抄的清单，而是拿 `Display` 那个**编译器强制穷尽**的 match 当变体数的真值源。
    ///
    /// # 判据是「`Self::<Ident> =>` 这样的匹配臂」，不是「出现过 `Self::`」
    ///
    /// 裸数 `Self::` 会漂：同一个 impl 块里任何别的写法（`matches!(self, Self::Done)`、
    /// `Self::from_str`、辅助分支）都会把计数顶高，而顶高之后本门要么误红、要么在 `ALL` 恰好也
    /// 多一项时**假绿**。故只认「`Self::` + 标识符 + 可选空白 + `=>`」这一形状。
    ///
    /// 剩下的射程边界（如实登记，逐条都不是静默的）：
    ///  1. **or-pattern 臂**（`Self::A | Self::B => …`）：只有末项后面跟着 `=>` ⇒ 计数**偏低**
    ///     ⇒ 与 `ALL` 不等 ⇒ 转红。方向安全（宁可误红，不放过没被遍历到的变体）。
    ///  2. **同 impl 块里出现第二个 `match self`**：计数偏高 ⇒ 同样转红。今天该块只有 `fmt` 一个
    ///     方法，不可达。
    ///  3. **`_ => …` 通配臂**：计数停在通配那一刻，且 match 不再被编译器强制穷尽 ⇒ 新变体既不
    ///     编译红、也不被本门抓到 —— **这一条是真的哑**。今天无通配臂；谁要加，请连同本门一起
    ///     重新设计判据（通配臂本身就是「新变体不必表态」的宣言）。
    ///
    /// **变异探针**：从 `ALL` 里删掉任意一项 ⇒ 数量不等，转红；把两项写成同一个变体 ⇒ 去重后数量
    /// 不等，转红；给枚举加一档而不加进 `ALL` ⇒ 转红；在本 impl 块里加一行
    /// `let _ = Self::Done;` ⇒ 裸数版本会转红，本版本纹丝不动。
    #[test]
    fn all_lists_every_popup_phase_exactly_once() {
        const SRC: &str = include_str!("state.rs");
        const ANCHOR: &str = "impl fmt::Display for PopupPhase {";
        let at = SRC
            .find(ANCHOR)
            .expect("锚点消失：`Display for PopupPhase` 被改形，本门已失去变体数的真值源");
        let rest = &SRC[at..];
        let body = &rest[..rest.find("\n}\n").expect("找不到该 impl 块的列 0 右花括号")];
        // 「`Self::` + 标识符 + 可选空白 + `=>`」= 一条匹配臂。本 crate 无 regex 依赖，手写扫描。
        let variants = body
            .match_indices("Self::")
            .filter(|(i, m)| {
                let tail = &body[i + m.len()..];
                let ident_len = tail
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(tail.len());
                ident_len > 0 && tail[ident_len..].trim_start().starts_with("=>")
            })
            .count();
        assert!(
            variants >= 4,
            "`Display` 的 match 一条臂都没解析到（实得 {variants}）—— 取材器塌了"
        );
        assert_eq!(
            PopupPhase::ALL.len(),
            variants,
            "`PopupPhase::ALL` 有 {} 项，而枚举有 {variants} 个变体 —— 所有遍历型门都会漏掉那一格",
            PopupPhase::ALL.len()
        );
        let distinct: std::collections::BTreeSet<String> =
            PopupPhase::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            distinct.len(),
            PopupPhase::ALL.len(),
            "`PopupPhase::ALL` 里有重复项 —— 数量对得上，却有一格从没被遍历到"
        );
    }
}
