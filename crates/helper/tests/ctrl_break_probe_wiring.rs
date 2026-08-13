//! G3 探针的接线门：源码与 workflow 之间那几处**各写一份**的常量必须一致。
//!
//! 探针有三处「两边各写一份、漂了就静默失效」的耦合，且失效形态都不是报错：
//!
//! | 耦合 | 漂了会怎样 |
//! |---|---|
//! | 服务名（Rust `SERVICE_NAME` ↔ workflow 的 `sc.exe create`） | `StartServiceCtrlDispatcherW` 报 1063，服务起不来，只表现为「结果文件没出现」 |
//! | bin 名（Cargo `[[bin]] name` ↔ workflow 里的 exe 路径） | 构建步骤就红，这条相对安全，但一起钉住成本为零 |
//! | feature 名（Cargo `required-features` ↔ CI 交叉编译门 + 探针 workflow） | 探针**永远没人编**，烂掉要等到真跑实验那天 |
//!
//! 本门在 Linux 上跑（纯文本对差），不需要 Windows。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/helper 之上应有仓根")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 从 `pub const SERVICE_NAME: &str = "…";` 里取字面量。
fn service_name_in_source() -> String {
    let src = read("crates/helper/probes/ctrl_break_probe.rs");
    let needle = "pub const SERVICE_NAME: &str = \"";
    let i = src
        .find(needle)
        .expect("探针里找不到 SERVICE_NAME 常量 —— 改名了，先确认再动本门");
    let rest = &src[i + needle.len()..];
    rest[..rest.find('"').expect("字面量没收口")].to_string()
}

#[test]
fn service_name_matches_the_workflow() {
    let name = service_name_in_source();
    assert!(!name.is_empty(), "SERVICE_NAME 解析成了空串");
    let wf = read(".github/workflows/probe-ctrl-break.yml");
    assert!(
        wf.contains(&format!("$svc = '{name}'")),
        "workflow 里 sc.exe 用的服务名与探针的 SERVICE_NAME (`{name}`) 对不上 —— \
         症状不是报错，是 SCM 分派器起不来、结果文件永远不出现"
    );
}

#[test]
fn bin_and_feature_names_match_cargo() {
    let cargo = read("crates/helper/Cargo.toml");
    assert!(
        cargo.contains("name = \"ctrl-break-probe\""),
        "Cargo.toml 里没有 ctrl-break-probe 这个 bin"
    );
    assert!(
        cargo.contains("required-features = [\"ctrl-break-probe\"]"),
        "探针 bin 没挂 required-features —— 它会跟着每次 helper 默认构建一起编"
    );
    assert!(
        cargo.contains("path = \"probes/ctrl_break_probe.rs\""),
        "探针不在 probes/ 下 —— 放回 src/bin/ 会被 autobins 自动发现，required-features 就白挂了"
    );

    let wf = read(".github/workflows/probe-ctrl-break.yml");
    assert!(
        wf.contains("--bin ctrl-break-probe --features ctrl-break-probe"),
        "workflow 的构建命令没同时点名 bin 与 feature"
    );
    assert!(
        wf.contains("target\\release\\ctrl-break-probe.exe"),
        "workflow 里的 exe 路径与 bin 名对不上"
    );
}

/// 🔴 探针必须被**某条常跑的门**编到，否则它会在无人察觉中烂掉。
///
/// 它被 `required-features` 挡在默认构建之外 —— 这是有意的（默认构建零代价），
/// 代价是「没人显式点名就永远不编」。CI 的交叉编译步骤是唯一常跑的编译点，
/// 这条断言就是钉住那一处。
#[test]
fn ci_cross_check_still_compiles_the_probe() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("--features polaris-helper/ctrl-break-probe"),
        "ci.yml 的交叉编译步骤不再编探针 —— 它会随 helper 的 Windows API 改动悄悄烂掉，\
         等到真要跑实验那天才发现编不过"
    );
}
