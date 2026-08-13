//! BTM（Background Task Management）三级探测（system-design §D 维度7 mac 行）。
//!
//! ## 背景
//!
//! macOS 13 (Ventura) 起，系统引入 BTM —— 对「会持久化的后台项」（LaunchAgents/LaunchDaemons、
//! 登录项等）做探测与提示。**未签名/未公证 app** 的后台项会受到更严格的处理：
//!
//! - **macOS 13 (Ventura) — BTM v1**：LaunchDaemon 首次加载时，系统设置 → 通用 → 登录项与扩展
//!   会列出该项并弹「后台项已添加」。用户可手动禁用。helper 仍会运行（除非用户主动禁用）。
//! - **macOS 14 (Sonoma) — BTM v2**：增强对未签名项的处理 —— 未签名 LaunchDaemon 可能被默认
//!   置为「不允许」，需用户在设置中显式启用。KeepAlive 行为受影响（系统可能延迟重启）。
//! - **macOS 15 (Sequoia) — BTM v3**：周期性重新扫描 + 对未签名项的增强提示（更频繁的「允许后台项」
//!   弹窗）。未签名 helper 的常驻稳定性下降。
//!
//! ## Polaris 现状（`helper.go:8` 注释）
//!
//! Polaris **未签名**（无法做 SMJobBless 客户端证书校验）。helper 作为 root LaunchDaemon 部署，
//! BTM 会在三个版本上分别表现。Go helper 本体**没有显式 BTM 探测代码**（它只靠 launchd + token 协议
//! 常驻）；BTM「探测规避」是**安装期策略** —— 本模块固化这套策略判断，供安装器查询。
//!
//! ## 移植纪律
//!
//! Go 源无对应代码（BTM 是安装期而非运行期）。本模块为 Polaris 新增的 day-1 能力：把「当前 macOS
//! 版本 → BTM 行为 → 安装建议」固化为纯逻辑（跨平台可测），消除「策略散在 osascript 脚本/TS」的 drift。
//!
//! 地雷（B3 真机复验）：13/14/15 的 BTM 行为差异需在各版本真机各跑一次确认（尤其 15 的周期扫描频率）。

/// macOS 主版本号（用于 BTM 分级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MacosVersion(pub u32);

impl MacosVersion {
    pub const VENTURA: Self = Self(13);
    pub const SONOMA: Self = Self(14);
    pub const SEQUOIA: Self = Self(15);
    pub const TAHOE: Self = Self(26); // macOS 26 (Tahoe) - 2025 年命名重构
}

/// BTM 行为分级（对应 macOS 版本的三级探测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtmTier {
    /// macOS < 13：无 BTM，LaunchDaemon 自由加载。
    None,
    /// macOS 13 (Ventura)：BTM v1，首次加载弹「后台项已添加」，用户可禁用。
    V1,
    /// macOS 14 (Sonoma)：BTM v2，未签名项可能默认禁用，需显式启用。
    V2,
    /// macOS 15 (Sequoia) 及更新：BTM v3，周期重扫 + 增强提示。
    V3,
}

/// BTM 探测结果（携带该版本的安装建议）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtmAssessment {
    /// BTM 分级。
    pub tier: BtmTier,
    /// 是否会对未签名 helper 弹「后台项」提示。
    pub prompts_user: bool,
    /// 是否建议对 helper 二进制做 adhoc 签名（降低 BTM 严格度）。
    pub recommends_adhoc_sign: bool,
    /// 是否建议用户在系统设置中显式「允许」该后台项。
    pub requires_explicit_allow: bool,
    /// 该版本 BTM 行为的人读描述（供安装器 UI 展示）。
    pub description: &'static str,
}

/// 按 macOS 主版本号评估 BTM 行为（纯逻辑，跨平台可测）。
///
/// 对照三级探测：
/// - `< 13` → [`BtmTier::None`]，无提示。
/// - `== 13` → [`BtmTier::V1`]，首次弹「后台项已添加」。
/// - `== 14` → [`BtmTier::V2`]，未签名项可能默认禁用，建议 adhoc 签名 + 用户显式允许。
/// - `>= 15` → [`BtmTier::V3`]，周期重扫 + 增强提示，强烈建议 adhoc 签名。
#[must_use]
pub fn assess_btm(version: MacosVersion) -> BtmAssessment {
    if version < MacosVersion::VENTURA {
        BtmAssessment {
            tier: BtmTier::None,
            prompts_user: false,
            recommends_adhoc_sign: false,
            requires_explicit_allow: false,
            description: "macOS < 13: 无 BTM，LaunchDaemon 自由加载",
        }
    } else if version == MacosVersion::VENTURA {
        BtmAssessment {
            tier: BtmTier::V1,
            prompts_user: true,
            recommends_adhoc_sign: false,
            requires_explicit_allow: false,
            description: "macOS 13 (Ventura): BTM v1，首次加载弹「后台项已添加」，用户可手动禁用",
        }
    } else if version == MacosVersion::SONOMA {
        BtmAssessment {
            tier: BtmTier::V2,
            prompts_user: true,
            recommends_adhoc_sign: true,
            requires_explicit_allow: true,
            description:
                "macOS 14 (Sonoma): BTM v2，未签名项可能默认禁用，建议 adhoc 签名 + 用户显式允许",
        }
    } else {
        // >= 15（含 Sequoia 15 与 Tahoe 26）
        BtmAssessment {
            tier: BtmTier::V3,
            prompts_user: true,
            recommends_adhoc_sign: true,
            requires_explicit_allow: true,
            description:
                "macOS 15+ (Sequoia/Tahoe): BTM v3，周期重扫 + 增强提示，强烈建议 adhoc 签名",
        }
    }
}

/// adhoc 签名命令（移植自 `helper.go:196` 的 install-core 内签名步骤）。
///
/// 对照 Go：`/usr/bin/codesign --force --deep -s - <sing-box>`。`-s -` 即 adhoc 签名
/// （无身份证书，仅标记二进制「已签名」降低 Gatekeeper/BTM 严格度）。
///
/// 返回 (程序, 参数) 二元组，供 handler 经 CommandRunner 执行。
#[must_use]
pub fn adhoc_sign_cmd(target_binary: &str) -> (&'static str, Vec<String>) {
    (
        "/usr/bin/codesign",
        vec![
            "--force".into(),
            "--deep".into(),
            "-s".into(),
            "-".into(),
            target_binary.to_owned(),
        ],
    )
}

/// 清 quarantine 命令（移植自 `helper.go:195` 的 install-core 内 xattr 步骤）。
///
/// 对照 Go：`/usr/bin/xattr -cr <core_dir>`。`-cr` 递归清 com.apple.quarantine 扩展属性，
/// 防 Gatekeeper 对新放入未签名二进制 SIGKILL（`helper.go:193` 注释）。
#[must_use]
pub fn clear_quarantine_cmd(target_dir: &str) -> (&'static str, Vec<String>) {
    ("/usr/bin/xattr", vec!["-cr".into(), target_dir.to_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== assess_btm 三级分派 =====

    #[test]
    fn btm_none_before_ventura() {
        let a = assess_btm(MacosVersion(12));
        assert_eq!(a.tier, BtmTier::None);
        assert!(!a.prompts_user);
        assert!(!a.recommends_adhoc_sign);
        // macOS 11 Big Sur 也无 BTM
        let a11 = assess_btm(MacosVersion(11));
        assert_eq!(a11.tier, BtmTier::None);
    }

    #[test]
    fn btm_v1_ventura() {
        let a = assess_btm(MacosVersion::VENTURA);
        assert_eq!(a.tier, BtmTier::V1);
        assert!(a.prompts_user, "Ventura 首次加载会弹提示");
        assert!(!a.recommends_adhoc_sign, "V1 不强制签名");
        assert!(!a.requires_explicit_allow, "V1 不需显式允许");
    }

    #[test]
    fn btm_v2_sonoma() {
        let a = assess_btm(MacosVersion::SONOMA);
        assert_eq!(a.tier, BtmTier::V2);
        assert!(a.recommends_adhoc_sign, "Sonoma 未签名项建议 adhoc 签");
        assert!(a.requires_explicit_allow, "Sonoma 未签名项可能需显式允许");
    }

    #[test]
    fn btm_v3_sequoia_and_above() {
        let a = assess_btm(MacosVersion::SEQUOIA);
        assert_eq!(a.tier, BtmTier::V3);
        assert!(a.recommends_adhoc_sign);

        // Tahoe (26) 也归 V3
        let a26 = assess_btm(MacosVersion::TAHOE);
        assert_eq!(a26.tier, BtmTier::V3);
        assert!(a26.prompts_user);

        // 未来版本 (30+) 也归 V3（兜底）
        let a30 = assess_btm(MacosVersion(30));
        assert_eq!(a30.tier, BtmTier::V3);
    }

    #[test]
    fn btm_assessment_description_nonempty() {
        for v in [12, 13, 14, 15, 26] {
            let a = assess_btm(MacosVersion(v));
            assert!(!a.description.is_empty(), "v{v} description empty");
        }
    }

    // ===== 签名/quarantine 命令对照 helper.go =====

    #[test]
    fn adhoc_sign_cmd_matches_go_source() {
        // helper.go:196: /usr/bin/codesign --force --deep -s - <sb>
        let (prog, args) = adhoc_sign_cmd("/core/sing-box");
        assert_eq!(prog, "/usr/bin/codesign");
        assert_eq!(args, vec!["--force", "--deep", "-s", "-", "/core/sing-box"]);
    }

    #[test]
    fn clear_quarantine_cmd_matches_go_source() {
        // helper.go:195: /usr/bin/xattr -cr <coreDir>
        let (prog, args) = clear_quarantine_cmd("/Library/Application Support/Polaris/core");
        assert_eq!(prog, "/usr/bin/xattr");
        assert_eq!(
            args,
            vec![
                "-cr".to_owned(),
                "/Library/Application Support/Polaris/core".to_owned()
            ]
        );
    }

    #[test]
    fn macos_version_ordering() {
        assert!(MacosVersion::VENTURA < MacosVersion::SONOMA);
        assert!(MacosVersion::SONOMA < MacosVersion::SEQUOIA);
        assert!(MacosVersion(12) < MacosVersion::VENTURA);
    }
}
