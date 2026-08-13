//! JS 挑战 / 防火墙拒绝识别 —— unlock 族**通用**分类器（纯函数，读 [`UnlockResponse`]）。
//!
//! 消费方：`check_chatgpt` / `check_gemini`（挑战页会误 Ok/Blocked，故判定前先短路为 `Restricted`）。
//! 判据来自 CF 官方 `cf-mitigated` 契约 + cloudscraper 强 body marker，本机实测某 CF 前置站点 200 页背书反例。
//!
//! ## 判据（按优先级短路）
//! 1. **主判据 S1**：响应头 `cf-mitigated: challenge`（CF 官方契约，所有挑战类型都设此头，`challenge` 是唯一
//!    合法值）→ 一票定案，与 status 无关。用 [`UnlockResponse::header`] 大小写不敏感取。
//! 2. **辅判据门**：无 S1 时，`status ∈ {403,429,503}` **且** `server` 以 `cloudflare` 开头（大小写不敏感）。
//!    过门后按 body 强 marker 细分：
//!    - S5 `cf-error-code">1020` → [`ChallengeKind::FirewallBlock`]（**非挑战**：IP/规则直接拒，无 JS 可过 = L2 信号）。
//!    - S4 Turnstile marker → [`ChallengeKind::Turnstile`]。
//!    - S2/S3 JS/managed/captcha marker → [`ChallengeKind::CfChallenge`]。
//! 3. 其余 → `None`（走 checker 原逻辑）。
//!
//! ## 实测排除的两个误报陷阱（务必不当信号；本机实测某 CF 前置站点正常 200 页验过）
//! - **反例1** `set-cookie: __cf_bm=...`：bot-management cookie 在**正常 200 页也下发** → 非信号（不入 marker）。
//! - **反例2** 孤立子串 `challenge-platform`：正常页被动注入 `/cdn-cgi/challenge-platform/scripts/jsd/main.js`
//!   （JS detections）→ 非信号（marker 表刻意不含裸 `challenge-platform`）。
//!
//! marker 常量集中此文件常量段（`challenge.rs` 自持），CF 改版漂移只改一处。
//!
//! `ChallengeKind` 仅 crate 内部用（debug 日志 / 未来 hover 细分）——**不进 IPC**，对外统一折叠为
//! [`crate::UnlockStatus::Restricted`]，避免前端契约膨胀。

use crate::http::UnlockResponse;

/// 命中的拦截类型（crate 内部；对外折叠为 `Restricted`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    /// S1/S2/S3：JS / managed / captcha 挑战（真浏览器多半可过；裸 HTTP 检测器是墙）。
    CfChallenge,
    /// S4：Turnstile 挑战。
    Turnstile,
    /// S5：CF 1020 防火墙拒绝——IP/规则直接拒，无 JS 可过（L2 信号，非 L3 挑战）。
    FirewallBlock,
}

/// S2/S3 强 body marker（JS 挑战 IUAM / managed / captcha；来源 cloudscraper `is_IUAM/is_Captcha_Challenge`）。
/// 每条独立命中即判 [`ChallengeKind::CfChallenge`]。**刻意不含裸 `challenge-platform`**（反例2）。
const CF_CHALLENGE_MARKERS: &[&str] = &[
    "/cdn-cgi/images/trace/jsch/",    // S3 JS 挑战（IUAM）背景图
    "/cdn-cgi/images/trace/captcha/", // S2 captcha 挑战背景图
    "/cdn-cgi/images/trace/managed/", // S2 managed 挑战背景图
    "__cf_chl_f_tk",                  // S2 challenge-form token
];

/// S4 Turnstile marker（来源 cloudscraper `is_Turnstile_Challenge`）。
const TURNSTILE_MARKERS: &[&str] = &[
    "challenges.cloudflare.com/turnstile/v0/api.js",
    "class=\"cf-turnstile\"",
];

/// S5 CF 1020 防火墙拒绝 marker（来源 cloudscraper `is_Firewall_Blocked`：`<span class="cf-error-code">1020</span>`）。
const FIREWALL_1020_MARKER: &str = "cf-error-code\">1020";

/// 纯函数分类器：读 status / headers / body，返回命中的拦截类型（无命中 → `None`）。
///
/// 不触网、不 panic。判据与优先级见模块文档。
pub fn classify(resp: &UnlockResponse) -> Option<ChallengeKind> {
    // S1 主判据：cf-mitigated: challenge（一票定案，与 status 无关）。
    if resp
        .header("cf-mitigated")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("challenge"))
    {
        return Some(ChallengeKind::CfChallenge);
    }

    // 辅判据门：status ∈ {403,429,503} 且 server 以 cloudflare 开头。
    // 反例3（保守）：非 CF 源站自有 403 不判挑战——server 门挡住。
    if !matches!(resp.status, 403 | 429 | 503) {
        return None;
    }
    let is_cloudflare = resp.header("server").is_some_and(|s| {
        s.trim_start()
            .to_ascii_lowercase()
            .starts_with("cloudflare")
    });
    if !is_cloudflare {
        return None;
    }

    let body = resp.body.as_str();
    // S5 1020 防火墙拒绝优先（无 JS 可过，与挑战正交）→ FirewallBlock。
    if body.contains(FIREWALL_1020_MARKER) {
        return Some(ChallengeKind::FirewallBlock);
    }
    // S4 Turnstile。
    if TURNSTILE_MARKERS.iter().any(|m| body.contains(m)) {
        return Some(ChallengeKind::Turnstile);
    }
    // S2/S3 CfChallenge。
    if CF_CHALLENGE_MARKERS.iter().any(|m| body.contains(m)) {
        return Some(ChallengeKind::CfChallenge);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 构造响应（headers 键原样存，[`UnlockResponse::header`] 大小写不敏感取）。
    fn resp(status: u16, headers: &[(&str, &str)], body: &str) -> UnlockResponse {
        let mut h = BTreeMap::new();
        for (k, v) in headers {
            h.insert(k.to_string(), v.to_string());
        }
        UnlockResponse {
            status,
            body: body.to_string(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: None,
            headers: h,
        }
    }

    /// `server: cloudflare` 头（辅判据门必需）。
    const CF: (&str, &str) = ("server", "cloudflare");

    // ── 主判据 S1：cf-mitigated 一票定案（打断 S1 → 转红）────────────────────────
    #[test]
    fn s1_cf_mitigated_challenge_even_on_200_without_body_marker() {
        // 仅主判据成立：200 + 无 body marker + cf-mitigated:challenge → CfChallenge。
        // 若打断 S1，本例落 None（200 过不了辅判据门）→ 转红。
        let r = resp(200, &[("cf-mitigated", "challenge")], "<html>ok</html>");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }

    #[test]
    fn s1_header_name_and_value_case_insensitive() {
        let r = resp(200, &[("CF-Mitigated", "Challenge")], "");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }

    #[test]
    fn s1_non_challenge_value_not_flagged() {
        // "challenge" 是唯一合法挑战值——其它值不判（防 `.is_some()` 退化误判）。
        let r = resp(403, &[("cf-mitigated", "block"), CF], "");
        assert_eq!(classify(&r), None);
    }

    // ── 每条辅 marker 各一测（打断该 marker → None → 转红）───────────────────────
    #[test]
    fn s3_jsch_marker_is_cf_challenge() {
        let r = resp(503, &[CF], "x /cdn-cgi/images/trace/jsch/ x");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }
    #[test]
    fn s2_captcha_marker_is_cf_challenge() {
        let r = resp(403, &[CF], "x /cdn-cgi/images/trace/captcha/ x");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }
    #[test]
    fn s2_managed_marker_is_cf_challenge() {
        let r = resp(403, &[CF], "x /cdn-cgi/images/trace/managed/ x");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }
    #[test]
    fn s2_chl_form_token_marker_is_cf_challenge() {
        let r = resp(429, &[CF], "<form ...> __cf_chl_f_tk=abc123 </form>");
        assert_eq!(classify(&r), Some(ChallengeKind::CfChallenge));
    }
    #[test]
    fn s4_turnstile_api_js_marker() {
        let r = resp(
            403,
            &[CF],
            "<script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script>",
        );
        assert_eq!(classify(&r), Some(ChallengeKind::Turnstile));
    }
    #[test]
    fn s4_turnstile_widget_class_marker() {
        let r = resp(
            403,
            &[CF],
            r#"<div class="cf-turnstile" data-sitekey="x"></div>"#,
        );
        assert_eq!(classify(&r), Some(ChallengeKind::Turnstile));
    }
    #[test]
    fn s5_firewall_1020_is_firewall_block_not_challenge() {
        // S5 单独归 FirewallBlock（L2），非 CfChallenge——打断 S5 分支 → 落 CfChallenge/None → 转红。
        let r = resp(
            403,
            &[CF],
            r#"<span class="cf-error-code">1020</span> Ray ID"#,
        );
        assert_eq!(classify(&r), Some(ChallengeKind::FirewallBlock));
    }

    // ── 辅判据门有牙：status / server 各打断一次 ────────────────────────────────
    #[test]
    fn gate_status_must_be_challenge_range() {
        // body 有强 marker + server cloudflare，但 status=200 → 非挑战（防正常页误判）。
        let r = resp(200, &[CF], "/cdn-cgi/images/trace/managed/");
        assert_eq!(classify(&r), None);
    }
    #[test]
    fn gate_server_must_be_cloudflare() {
        // 403 + body marker 但 server=nginx（源站自有 403 透传）→ 不判（反例3 保守）。
        let r = resp(
            403,
            &[("server", "nginx")],
            "/cdn-cgi/images/trace/managed/",
        );
        assert_eq!(classify(&r), None);
    }

    // ── 两个误报陷阱：正常 200 页不误判（实测背书，反例1/反例2）─────────────────────
    #[test]
    fn trap_cf_bm_cookie_on_normal_page_not_challenge() {
        // 反例1：__cf_bm bot-management cookie 在正常 200 页也下发 → 非信号。
        let r = resp(
            200,
            &[
                CF,
                ("set-cookie", "__cf_bm=Ab.Cd-Ef; path=/; HttpOnly; Secure"),
            ],
            "<html><body>normal app page</body></html>",
        );
        assert_eq!(classify(&r), None);
    }
    #[test]
    fn trap_isolated_challenge_platform_on_normal_page_not_challenge() {
        // 反例2：正常页被动注入 /cdn-cgi/challenge-platform/scripts/jsd/main.js（JS detections）→ 非信号。
        let r = resp(
            200,
            &[CF],
            r#"<script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script><div>419KB app</div>"#,
        );
        assert_eq!(classify(&r), None);
    }
    #[test]
    fn trap_both_traps_on_403_still_none_without_strong_marker() {
        // 加固：即便 403+cloudflare，仅陷阱串（无强 marker）→ 仍 None（防陷阱在受限状态下误触）。
        let r = resp(
            403,
            &[CF, ("set-cookie", "__cf_bm=x")],
            r#"<script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>"#,
        );
        assert_eq!(classify(&r), None);
    }
}
