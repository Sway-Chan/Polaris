//! §C6 金样对拍：`core_build` 移植 vs 上游 `src/shared/__tests__/core-build.test.ts`。
//!
//! **这扇门覆盖什么**（§K7 三问，逐条回答）：
//!  1. **射程**：`core-build.ts` 全部 7 个导出的**全部断言**，逐条 1:1 移植自 上游的 jest 测试文件
//!     （2026-07-16 对拍 上游 `@18d35bd` 的 `core-build.test.ts:1-240`）。每个 `#[test]` 的名字与
//!     上游 `describe/it` 一一对应，便于漂移时定位。
//!  2. **前置缺失时失败还是静默跳过**：本文件**无夹具、无前置、无 IO**——纯函数直调，
//!     不存在「夹具缺失 → `return` 静默跳过」的形态（§K7 点名的 `golden_config_snapshot.rs:124-125` 病灶）。
//!     断言恒执行。
//!  3. **它没覆盖的部分谁在守**：本文件只守**纯逻辑**。「谁来调这些函数、喂什么参数」由
//!     `src-tauri/src/runtime/updater.rs` 的接线守（见该文件的 `core_version_readers_are_asymmetric`
//!     等测试）；「真机换核后 reseed 是否真生效」**无门**——需真机换核，已如实登记为边界。
//!
//! 变异验证（`~/docs/polaris/roadmap` §K7.1「没做变异验证的门不能声称它在守」）：见任务报告，
//! 逐条改坏 `core_build.rs` 的关键分支确认本文件转红。

use polaris_updater::core_build::{
    classify_core_build, classify_reseed_result, decide_core_override, extract_version_token,
    parse_uploaded_core_version, reseed_applied, ComparableVersion, CoreBuildKind,
    CoreOverrideDecision, ReseedResult, UploadedCoreVersion,
};
use polaris_updater::version::compare_semver;

/// 便利：构造 `decide_core_override` 的期望值。
fn decision(reseed: bool, warn: bool) -> CoreOverrideDecision {
    CoreOverrideDecision { reseed, warn }
}

// ── describe('extractVersionToken') ── core-build.test.ts:18-32 ──

#[test]
fn extract_version_token_from_full_version_line() {
    // it('从完整 version 行提取 token')
    assert_eq!(extract_version_token("sing-box version 1.13.13"), "1.13.13");
    assert_eq!(
        extract_version_token("sing-box version 1.13.13-reF1nd"),
        "1.13.13-reF1nd"
    );
}

#[test]
fn extract_version_token_bare_token_and_v_prefix() {
    // it('裸 token / 前缀 v')
    assert_eq!(extract_version_token("1.13.13"), "1.13.13");
    assert_eq!(extract_version_token("v1.13.13"), "1.13.13");
}

#[test]
fn extract_version_token_empty_and_dirty_input() {
    // it('空/脏输入')。上游第 3 条 `undefined as any` 在 Rust 由 `&str` 类型排除，
    // 等价形态（空串）已由前两条覆盖。
    assert_eq!(extract_version_token(""), "");
    assert_eq!(extract_version_token("   "), "");
}

// ── describe('parseUploadedCoreVersion') ── core-build.test.ts:34-75 ──

#[test]
fn parse_uploaded_official_release_full_token_and_raw_line() {
    // it('官方 release：完整 token + 原始首行')
    assert_eq!(
        parse_uploaded_core_version("sing-box version 1.14.0\nEnvironment: go1.25"),
        UploadedCoreVersion {
            version: Some("1.14.0".into()),
            version_line: "sing-box version 1.14.0".into(),
        }
    );
}

#[test]
fn parse_uploaded_official_prerelease_keeps_alpha_suffix() {
    // it('官方 prerelease：保留 -alpha.N 后缀（B-1 病灶：旧正则会剥成 1.14.0）')
    assert_eq!(
        parse_uploaded_core_version("sing-box version 1.14.0-alpha.37").version,
        Some("1.14.0-alpha.37".into())
    );
}

#[test]
fn parse_uploaded_official_dev_keeps_commit_hex_tail() {
    // it('官方 dev：保留 base + 短 commit hex 完整尾段')
    assert_eq!(
        parse_uploaded_core_version("sing-box version 1.14.0-alpha.37-abcdef1").version,
        Some("1.14.0-alpha.37-abcdef1".into())
    );
}

#[test]
fn parse_uploaded_fork_keeps_compound_suffix() {
    // it('fork：保留完整复合后缀（供 classifyCoreBuild / B-3 识别）')
    let r = parse_uploaded_core_version("sing-box version 1.14.0-alpha.31-nekolsd-test");
    assert_eq!(r.version, Some("1.14.0-alpha.31-nekolsd-test".into()));
    assert_eq!(
        r.version_line,
        "sing-box version 1.14.0-alpha.31-nekolsd-test"
    );
}

#[test]
fn parse_uploaded_int_dirty_empty_never_lies() {
    // it('纯 int / 脏行 / 空：version=null 不谎报，versionLine 仍保留原始行')
    assert_eq!(
        parse_uploaded_core_version("sing-box version 20260101").version,
        None
    );
    assert_eq!(
        parse_uploaded_core_version("garbage output"),
        UploadedCoreVersion {
            version: None,
            version_line: "garbage output".into(),
        }
    );
    assert_eq!(
        parse_uploaded_core_version(""),
        UploadedCoreVersion {
            version: None,
            version_line: String::new(),
        }
    );
}

#[test]
fn parse_uploaded_only_first_line() {
    // it('只取第一行（version 恒在首行，避免 Environment 行 go1.25.x 误匹配）')
    assert_eq!(
        parse_uploaded_core_version(
            "sing-box version 1.14.0-alpha.37\nEnvironment: go1.25.10 linux/amd64"
        )
        .version,
        Some("1.14.0-alpha.37".into())
    );
}

// ── describe('comparableCoreVersion') ── core-build.test.ts:77-107 ──

#[test]
fn comparable_truncates_fork_tail() {
    // it('截断 fork 尾段（第二个 - 起）')
    assert_eq!(
        ComparableVersion::normalize("1.14.0-alpha.31-nekolsd-test").as_str(),
        "1.14.0-alpha.31"
    );
}

#[test]
fn comparable_truncates_official_dev_hex_tail() {
    // it('截断官方 dev 短 commit hex 尾段')
    assert_eq!(
        ComparableVersion::normalize("1.14.0-alpha.31-abcdef1").as_str(),
        "1.14.0-alpha.31"
    );
}

#[test]
fn comparable_keeps_prerelease_release_and_strips_v() {
    // it('保留官方 prerelease / release / 剥前缀 v')
    assert_eq!(
        ComparableVersion::normalize("1.14.0-alpha.37").as_str(),
        "1.14.0-alpha.37"
    );
    assert_eq!(ComparableVersion::normalize("1.14.0").as_str(), "1.14.0");
    assert_eq!(ComparableVersion::normalize("v1.13.13").as_str(), "1.13.13");
}

#[test]
fn comparable_non_version_returned_verbatim() {
    // it('非版本串（纯 int / 空）原样返回（unknown 核不参与 reseed 决策，无害）')
    assert_eq!(
        ComparableVersion::normalize("20260101").as_str(),
        "20260101"
    );
    assert_eq!(ComparableVersion::normalize("").as_str(), "");
}

#[test]
fn comparable_m1_contract_full_token_pollutes_comparison() {
    // it('M-1 契约：完整 dev/fork token 污染比较，comparable 修正为正确的「旧于基线」')
    //
    // 这是 §C6 最要紧的一条：它同时钉住 `compare_semver`（数字段 < 字母段）与 `normalize` 的截断口径。
    let bundled = "1.14.0-alpha.37";
    // 完整 dev token 的 `-abcdef1` 并入第二 prerelease 段 `"31-abcdef1"`（字母段）> `"37"`（数字段）→ 误判 +1（更新）
    assert_eq!(
        compare_semver("1.14.0-alpha.31-abcdef1", bundled).unwrap(),
        1
    );
    // comparable 截断后 → 1.14.0-alpha.31 < 1.14.0-alpha.37 → 正确 -1（更旧，触发预判/reseed）
    assert_eq!(
        compare_semver(
            ComparableVersion::normalize("1.14.0-alpha.31-abcdef1").as_str(),
            bundled
        )
        .unwrap(),
        -1
    );
    // fork 同理（这正是若收敛 getCoreVersion 会让 fork warn: cmp<=0 失效的回归根因）
    assert_eq!(
        compare_semver("1.14.0-alpha.31-nekolsd-test", bundled).unwrap(),
        1
    );
    assert_eq!(
        compare_semver(
            ComparableVersion::normalize("1.14.0-alpha.31-nekolsd-test").as_str(),
            bundled
        )
        .unwrap(),
        -1
    );
}

// ── describe('classifyCoreBuild — official') ── core-build.test.ts:109-132 ──

#[test]
fn classify_official_pure_semver_release() {
    // it('纯 semver release（含官方基线 1.13.13）')
    assert_eq!(
        classify_core_build("sing-box version 1.13.13"),
        CoreBuildKind::Official
    );
    assert_eq!(classify_core_build("1.13.13"), CoreBuildKind::Official);
    assert_eq!(classify_core_build("1.12.8"), CoreBuildKind::Official);
}

#[test]
fn classify_official_prerelease() {
    // it('官方预发布 -alpha/-beta/-rc.N')
    assert_eq!(classify_core_build("1.13.0-rc.5"), CoreBuildKind::Official);
    assert_eq!(
        classify_core_build("1.12.0-beta.15"),
        CoreBuildKind::Official
    );
    assert_eq!(
        classify_core_build("1.11.0-alpha.19"),
        CoreBuildKind::Official
    );
}

#[test]
fn classify_official_dev_build_hex_case_insensitive() {
    // it('官方 dev 自建（base + 短 commit hex，不误判 fork；大小写均可）')
    assert_eq!(
        classify_core_build("1.13.13-78b2e12"),
        CoreBuildKind::Official
    );
    assert_eq!(
        classify_core_build("1.13.13-78b2e12fbdd8"),
        CoreBuildKind::Official
    );
    assert_eq!(
        classify_core_build("1.13.0-rc.5-abcdef1"),
        CoreBuildKind::Official
    );
    // 大写 hex 不误判 fork
    assert_eq!(
        classify_core_build("1.13.13-78B2E12"),
        CoreBuildKind::Official
    );
}

#[test]
fn classify_official_cross_version_no_false_positive() {
    // it('边界：手动上传的官方跨版本 → 官方，零误报')
    assert_eq!(classify_core_build("1.14.0"), CoreBuildKind::Official); // 更新的官方
    assert_eq!(classify_core_build("1.11.5"), CoreBuildKind::Official); // 更旧的官方
    assert_eq!(classify_core_build("2.0.0"), CoreBuildKind::Official);
    assert_eq!(classify_core_build("v1.12.3"), CoreBuildKind::Official);
}

// ── describe('classifyCoreBuild — fork（非官方）') ── core-build.test.ts:134-144 ──

#[test]
fn classify_fork_ref1nd_suffix() {
    // it('reF1nd 后缀')
    assert_eq!(classify_core_build("1.13.13-reF1nd"), CoreBuildKind::Fork);
    assert_eq!(
        classify_core_build("sing-box version 1.13.13-reF1nd"),
        CoreBuildKind::Fork
    );
    assert_eq!(
        classify_core_build("1.14.0-alpha.29-reF1nd"),
        CoreBuildKind::Fork
    );
}

#[test]
fn classify_fork_nekolsd_suffix() {
    // it('nekolsd 后缀（含次级 -test）')
    assert_eq!(classify_core_build("1.13.3-nekolsd"), CoreBuildKind::Fork);
    assert_eq!(
        classify_core_build("1.14.0-alpha.31-nekolsd-test"),
        CoreBuildKind::Fork
    );
}

// ── describe('classifyCoreBuild — unknown（不硬判 fork）') ── core-build.test.ts:146-154 ──

#[test]
fn classify_unknown_never_hard_judges_fork() {
    // it('unknown / 空 / 脏输入')
    assert_eq!(classify_core_build("unknown"), CoreBuildKind::Unknown);
    assert_eq!(
        classify_core_build("sing-box version unknown"),
        CoreBuildKind::Unknown
    );
    assert_eq!(classify_core_build(""), CoreBuildKind::Unknown);
    assert_eq!(classify_core_build("sing-box"), CoreBuildKind::Unknown);
    assert_eq!(
        classify_core_build("garbage-output"),
        CoreBuildKind::Unknown
    );
}

// ── describe('decideCoreOverride') ── core-build.test.ts:156-184 ──
// 随包(内置)基线（上游用 const B = '1.14.0-alpha.32'）。
const B: &str = "1.14.0-alpha.32";

#[test]
fn decide_official_older_than_bundled_reseeds() {
    // it('官方 < 内置 → 内置替换(reseed)，不警告')
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Official,
            &ComparableVersion::normalize("1.13.13"),
            B
        ),
        decision(true, false)
    );
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Official,
            &ComparableVersion::normalize("1.14.0-alpha.30"),
            B
        ),
        decision(true, false)
    );
}

#[test]
fn decide_official_equal_to_bundled_keeps() {
    // it('官方 == 内置 → 保持(不重播种)，不警告')
    assert_eq!(
        decide_core_override(CoreBuildKind::Official, &ComparableVersion::normalize(B), B),
        decision(false, false)
    );
}

#[test]
fn decide_official_newer_than_bundled_keeps_no_downgrade() {
    // it('官方 > 内置 → 保持(不降级)，不警告（release > 同版 prerelease / 更高版）')
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Official,
            &ComparableVersion::normalize("1.14.0"),
            B
        ),
        decision(false, false)
    );
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Official,
            &ComparableVersion::normalize("1.15.0"),
            B
        ),
        decision(false, false)
    );
}

#[test]
fn decide_fork_never_reseeds_and_warns_when_not_newer() {
    // it('fork → 绝不重播种；≤ 内置 → 警告')
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Fork,
            &ComparableVersion::normalize("1.13.3"),
            B
        ),
        decision(false, true)
    );
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Fork,
            &ComparableVersion::normalize("1.14.0-alpha.31"),
            B
        ),
        decision(false, true)
    );
}

#[test]
fn decide_fork_newer_than_bundled_keeps_no_warn() {
    // it('fork > 内置 → 保持，不警告')
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Fork,
            &ComparableVersion::normalize("1.15.0"),
            B
        ),
        decision(false, false)
    );
}

#[test]
fn decide_unknown_same_as_fork() {
    // it('unknown → 同 fork：绝不重播种；≤ 内置 → 警告')
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Unknown,
            &ComparableVersion::normalize("1.13.0"),
            B
        ),
        decision(false, true)
    );
    assert_eq!(
        decide_core_override(
            CoreBuildKind::Unknown,
            &ComparableVersion::normalize("1.15.0"),
            B
        ),
        decision(false, false)
    );
}

// ── describe('reseedApplied') ── core-build.test.ts:186-203 ──
// 上游此 describe 用 const B = '1.14.0-alpha.33'（与上一节的 alpha.32 不同，勿混）。
const B33: &str = "1.14.0-alpha.33";

#[test]
fn reseed_applied_false_when_still_older_than_baseline() {
    // it('换核失败：版本仍 < 基线（旧核被占用 ETXTBSY 未替换）→ 未生效')
    // issue #150 实况：基线 1.14.0-alpha.33，活核仍 1.13.13 → 绝不能判成功
    assert!(!reseed_applied("1.13.13", B33));
    assert!(!reseed_applied("1.14.0-alpha.30", B33));
}

#[test]
fn reseed_applied_true_when_equal_to_baseline() {
    // it('换核成功：版本 == 基线 → 生效')
    assert!(reseed_applied(B33, B33));
}

#[test]
fn reseed_applied_true_when_newer_than_baseline() {
    // it('版本 > 基线（已是更新核）→ 视为生效')
    assert!(reseed_applied("1.14.0", B33)); // release > 同版 prerelease
    assert!(reseed_applied("1.15.0", B33));
}

// ── describe('classifyReseedResult') ── core-build.test.ts:205-240 ──
const BEFORE: &str = "1.13.13"; // 换核前旧官方核

#[test]
fn classify_reseed_f1_probe_failure_never_fakes_success() {
    // it('F1 核心：换核后重读探测失败（lineAfter=空）→ 保守保留旧版本、判未生效')
    // 绝不能因探测失败而回落基线 → 否则版本闸门误放行、带旧核硬跑退回死循环
    let expect = ReseedResult {
        version: BEFORE.into(),
        applied: false,
    };
    assert_eq!(classify_reseed_result("", BEFORE, B33), expect);
    assert_eq!(classify_reseed_result("   ", BEFORE, B33), expect);
    assert_eq!(classify_reseed_result("sing-box", BEFORE, B33), expect);
}

#[test]
fn classify_reseed_success_reports_baseline_version() {
    // it('换核成功：lineAfter 报随包基线版本 → 判生效、记录新版本')
    assert_eq!(
        classify_reseed_result("sing-box version 1.14.0-alpha.33 (go1.25)", BEFORE, B33),
        ReseedResult {
            version: "1.14.0-alpha.33".into(),
            applied: true,
        }
    );
}

#[test]
fn classify_reseed_not_applied_when_old_core_still_reports() {
    // it('换核未生效：旧核仍可跑、lineAfter 报旧版本 → 判未生效、记录旧版本（仍 < 基线）')
    // issue #150 主路径：ETXTBSY 只阻写不阻执行，重读拿到真实旧 1.13.13
    assert_eq!(
        classify_reseed_result("sing-box version 1.13.13", BEFORE, B33),
        ReseedResult {
            version: BEFORE.into(),
            applied: false,
        }
    );
}

#[test]
fn classify_reseed_applied_when_newer_official_core() {
    // it('换核后已是更新官方核（> 基线）→ 判生效')
    assert_eq!(
        classify_reseed_result("sing-box version 1.14.0", BEFORE, B33),
        ReseedResult {
            version: "1.14.0".into(),
            applied: true,
        }
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// 以下为 **Polaris 侧补测**（上游 core-build.test.ts 无对应条目）——
// 由 §C6 变异验证发现：上游金样套件对这两条**载荷分支无覆盖**，1:1 移植如实继承了这两个洞。
// 按 §K7「门只守它射程内的东西；两扇门之间的缝，正是生产路径」，在此堵上。
// 二者均已实测：改坏对应分支 → 本节转红（见任务报告变异验证表）。
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn decide_fork_equal_to_bundled_warns_polaris_addition() {
    // 洞 1：上游只测了 fork `<` B（1.13.3 / 1.14.0-alpha.31）与 `>` B（1.15.0），
    // **从未测 `==` B** → `warn: cmp <= 0` 改成 `cmp < 0` 时上游套件全绿。
    // 而 `core-build.ts:101` 的注释明写「≤ 基线时提醒兼容风险（**含同版 fork**——后缀分支可能缺新特性）」
    // → `<=` 里的 `=` 是载荷语义，不是笔误，必须有门守。
    assert_eq!(
        decide_core_override(CoreBuildKind::Fork, &ComparableVersion::normalize(B), B),
        decision(false, true),
        "同版 fork 必须 warn（后缀分支可能缺新特性），且绝不 reseed"
    );
    assert_eq!(
        decide_core_override(CoreBuildKind::Unknown, &ComparableVersion::normalize(B), B),
        decision(false, true),
        "同版 unknown 同 fork：warn 且绝不 reseed"
    );
}

#[test]
fn classify_official_dev_hex_lower_bound_is_seven_polaris_addition() {
    // 洞 2：上游 fork 样本（`-reF1nd` / `-nekolsd`）都含非 hex 字母（r/n/k/l/s），
    // 故 `[0-9a-fA-F]{7,}` 的 **7 位下限**无任何样本压着 → 把 `>= 7` 改成 `>= 1` 时上游套件全绿。
    // 下限是真语义：短 hex 后缀（<7）不足以认定为「官方 dev 短 commit」，应判 fork。
    assert_eq!(
        classify_core_build("1.13.13-abc123"), // 6 位 hex → 未达下限
        CoreBuildKind::Fork,
        "6 位 hex 后缀未达官方 dev 短 commit 下限（7）→ 应判 fork"
    );
    assert_eq!(
        classify_core_build("1.13.13-abc1234"), // 7 位 hex → 恰达下限
        CoreBuildKind::Official,
        "7 位 hex 后缀 = 官方 dev 短 commit 下限 → 应判 official"
    );
}
