//! 🔴 **随包核里那些「我们靠它的默认值」的依赖，版本一动就红。**
//!
//! # 为什么需要这一道（本门的第一个也是唯一的现存消费点：`sing-tun` 的 DNS 模式默认值）
//!
//! 本仓的 DNS 截获**完全建立在一个上游默认值上**，而那个默认值我们从不写进配置：
//!
//! - `route.rules` 里一条 `port:[53] + action:"hijack-dns"`（`builder/route.rs`），
//!   配一个哨兵 IP `CONTROLLED_TUN_DNS_IP = 8.8.8.8`（`user_config/dns_constants.rs`，
//!   **故意排除出 `BOOTSTRAP_DIRECT_DNS_IPS`**，否则被直连放行就逃逸了劫持）。
//! - tun inbound 里**不发** `dns_mode` ⇒ 用的是 sing-tun 的默认值。
//!   定版源码（`sing-tun@v0.8.12-0.20260805001200-da24acaf4de3` `tun.go:127-132`）：
//!   `func (o *Options) DNSModeOrDefault() string { if o.DNSMode == "" { return DNSModeHijack } … }`
//!
//! 即：**默认是 `hijack`，我们整套 DNS 分流就靠它**。上游哪天把默认改成 `native`，
//! 生成的配置一个字节都不变、`sing-box check` 照样 rc=0、全仓单测照样全绿，而用户侧的表现是
//! **能上网但分流失效**（DNS 不再进规则引擎 ⇒ FakeIP / 国内外分流 / dns-race 全部旁路）。
//! 这正是本仓反复吃亏的那类「看起来正常」的失败。
//!
//! # 为什么是「钉依赖版本」而不是「把 `dns_mode: hijack` 写进配置」
//!
//! 写进配置看着更直接，但代价在金样：`fixtures/config-snapshot.json` 是 **上游 4.2.6 冻结的
//! parity 基线**（`golden_config_snapshot.rs` 开头明文「只重生、不手改」），而 上游侧不发这个键。
//! 加一个 Polaris-only 的键 ⇒ 每个 TUN 场景当场 delta，且**永远无法靠重生对齐**（重生一次回来一次）。
//! 为一个假想风险换掉一道真门的可信度，不划算。
//!
//! 于是判据挪到**依赖指纹**上：核一 bump，`sing-tun` 版本串必变 ⇒ 本门红 ⇒ 人被迫回去看一眼
//! `DNSModeOrDefault` 的默认值还是不是 `hijack`，确认后才更新下面的常量。
//! **判据独立于被检查的那个值** —— 这是 `28f5d46` 修 cronet go.mod 对拍时立下的同一条：
//! 那次的坏味道正是「以 `cronetModuleVersion` 为新鲜度判据，而判据本身就是校验要去发现的值」。
//!
//! # 射程（自曝，别把绿读大）
//!
//! - 本门**不**验默认值本身是什么 —— 二进制里读不到（Go 符号表被剥，`go tool nm` 报
//!   `no symbol section`）。它只保证「依赖没换过」，换了就把人拦下来。
//! - 只读**本机存在的那一个**平台二进制。三平台同一个 sing-box release ⇒ 依赖版本一致，
//!   读哪个都行；真出现不一致，那本身就是该红的事。
//! - 缺核时跳过，`POLARIS_REQUIRE_KERNEL_GATE=1` 时缺核直接红（与 `kernel_accepts_outbounds`
//!   同一套接线，打包腿强制生效，`ci_step_still_wired` 守着）。
//! - 🔴 **「跳过」是静默的**：下面那句 `eprintln!` 归 libtest 捕获，**只在测试失败时才回放**。
//!   实测本门首跑的 CI ubuntu 腿（不拉核）日志里只有 `bundled_core_still_uses_the_pinned_sing_tun
//!   ... ok`，提示语零命中 ⇒ **那条绿只说明「编得过 + 提取器单测过」，没有比对过任何版本串**。
//!   真正的比对只发生在打包腿（核已 fetch + 硬化开关）。本地要看见提示语得显式 `-- --nocapture`。

use std::path::{Path, PathBuf};

/// 随包核当前使用的 `sing-tun` 版本。
///
/// 🔴 **改这个常量之前，先做这件事**：去读该版本 `tun.go` 的 `DNSModeOrDefault()`，
/// 确认 `o.DNSMode == ""` 时返回的仍是 `DNSModeHijack`。取源码的路径（符号表被剥时仍可用）：
///
/// ```text
/// go version -m <随包核>                     # 读出 sing-tun 的确切版本（伪版本带 commit）
/// curl -sfL https://proxy.golang.org/github.com/sagernet/sing-tun/@v/<版本>.zip
/// ```
///
/// 若默认值变了 —— **不要**只更新这里就放行：本仓整套 DNS 分流依赖它，得先决定是显式下发
/// `dns_mode` 还是改机制，详见 vault `polaris-singbox-1.14-adoption-matrix-2026-07-30` 的 I3 判定块。
// 2026-08-11 随 beta.14 上抬。放行前已按本常量文档的指引核对：
// https://raw.githubusercontent.com/SagerNet/sing-tun/d67734281390/tun.go 的
// `DNSModeOrDefault()`（tun.go:127-132）仍是 `if o.DNSMode == "" { return DNSModeHijack }` ——
// 前提未变，故只更新 pin，不改机制。
const SING_TUN_PINNED: &str = "v0.8.12-0.20260810140529-d67734281390";

/// 被钉的依赖模块路径。
const SING_TUN_MODULE: &str = "github.com/sagernet/sing-tun";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <repo>/crates/config-engine")
        .to_path_buf()
}

/// 本机可用的随包核。缺核 → `None`（调用方决定跳过还是红）。
fn bundled_core() -> Option<PathBuf> {
    let root = repo_root();
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["resources/win/sing-box.exe"]
    } else if cfg!(target_os = "macos") {
        &["resources/mac-arm64/sing-box", "resources/mac-x64/sing-box"]
    } else {
        &["resources/linux/sing-box"]
    };
    candidates
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
}

/// 从 Go 二进制内嵌的 modinfo 里提某个依赖的版本串。
///
/// modinfo 是纯文本、制表符分隔、换行结尾，形如：
/// `dep\tgithub.com/sagernet/sing-tun\tv0.8.12-…\th1:LhMorA53…=\n`
///
/// 不走 `go version -m`：CI 的 Rust 腿**没有 Go 工具链**（`ci.yml` 开头明载），
/// 挂一个本机有、CI 没有的外部命令等于把本门在 CI 上变成静默跳过。
///
/// 前后都用 `\t` 定界 ⇒ 模块名是**精确匹配**，`…/sing-tun-extra` 这类前缀撞名不会命中。
fn extract_dep_version(bin: &[u8], module: &str) -> Option<String> {
    let needle = format!("dep\t{module}\t").into_bytes();
    let at = bin
        .windows(needle.len())
        .position(|w| w == needle.as_slice())?;
    let rest = &bin[at + needle.len()..];
    let end = rest.iter().position(|b| *b == b'\t' || *b == b'\n')?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_owned)
}

#[test]
fn bundled_core_still_uses_the_pinned_sing_tun() {
    let Some(core) = bundled_core() else {
        let required = std::env::var("POLARIS_REQUIRE_KERNEL_GATE").is_ok_and(|v| v == "1");
        assert!(
            !required,
            "POLARIS_REQUIRE_KERNEL_GATE=1 但盘上没有随包核 —— \
             打包腿的 `node scripts/fetch-core.mjs` 是不是失败了？（依赖指纹门未执行）"
        );
        eprintln!(
            "⚠ 跳过依赖指纹门：盘上没有随包核（`.gitignore` 的 /resources/*）。\
             跑 `node scripts/fetch-core.mjs` 后本门自动生效。"
        );
        return;
    };

    let bytes = std::fs::read(&core).unwrap_or_else(|e| panic!("读不到 {}: {e}", core.display()));

    // 提不出来 = 本门失效（Go 换了 modinfo 编码 / 核被 strip 得更狠 / 拿到的不是 Go 二进制）。
    // 这种情况必须**红**而不是跳过 —— 否则本门会从「守着依赖」静静退化成「永远绿」。
    let actual = extract_dep_version(&bytes, SING_TUN_MODULE).unwrap_or_else(|| {
        panic!(
            "在 {} 里找不到 `{SING_TUN_MODULE}` 的 modinfo 条目 —— \
             本门已失效（不是「依赖没变」），先修门再谈结论",
            core.display()
        )
    });

    assert_eq!(
        actual, SING_TUN_PINNED,
        "\n随包核的 sing-tun 版本变了：{SING_TUN_PINNED} → {actual}\n\
         本仓的 DNS 截获依赖 sing-tun 的 `DNSModeOrDefault()` 默认值为 `hijack`\
         （tun inbound 刻意不下发 `dns_mode`）。默认值一变，生成的配置一字节不动、\
         `sing-box check` 照样 rc=0，而用户侧表现是「能上网但分流失效」。\n\
         ⇒ 先读新版 `tun.go` 的 `DNSModeOrDefault()` 确认默认仍是 `DNSModeHijack`，\
         再更新 core_dep_fingerprint.rs 的 SING_TUN_PINNED。取源码路径见该常量的文档注释。\n"
    );
}

/// 正向对照：证明提取器真的会「提不到」和「提错不了」，否则上面那条断言可能只是碰巧绿。
#[test]
fn the_extractor_has_teeth() {
    let ok = b"...\ndep\tgithub.com/sagernet/sing-tun\tv1.2.3\th1:AAAA=\ndep\tother\tv9\n";
    assert_eq!(
        extract_dep_version(ok, SING_TUN_MODULE).as_deref(),
        Some("v1.2.3")
    );

    // 依赖不在 → None（而不是返回旁边那条的版本）
    let absent = b"...\ndep\tgithub.com/sagernet/sing-box\tv1.14.0\th1:BBBB=\n";
    assert_eq!(extract_dep_version(absent, SING_TUN_MODULE), None);

    // 前缀撞名不得命中：`sing-tun-extra` 与 `sing-tun` 只差后缀，靠尾部 `\t` 定界分开
    let confusable = b"...\ndep\tgithub.com/sagernet/sing-tun-extra\tv7.7.7\th1:CCCC=\n";
    assert_eq!(extract_dep_version(confusable, SING_TUN_MODULE), None);

    // 截断（版本串后既无 \t 也无 \n）→ None，不返回半截版本
    let truncated = b"dep\tgithub.com/sagernet/sing-tun\tv1.2.3";
    assert_eq!(extract_dep_version(truncated, SING_TUN_MODULE), None);
}

/// 打包腿上本门必须真的在跑 —— 否则「缺核强制红」那条开关形同虚设。
#[test]
fn ci_step_still_wired() {
    let wf = repo_root().join(".github/workflows/package.yml");
    let raw =
        std::fs::read_to_string(&wf).unwrap_or_else(|e| panic!("读不到 {}: {e}", wf.display()));
    // 匹配 **run 命令本身**而不是裸词：本文件在 package.yml 里还留了一段说明注释，
    // 注释里就写着 `core_dep_fingerprint`。用裸词做判据的话，把整个 step 删掉、注释留下，
    // 本断言照样绿 —— 实测过，这正是「绿没有信息量」的一个新鲜样本。
    assert!(
        raw.contains("--test core_dep_fingerprint"),
        "package.yml 里找不到 `--test core_dep_fingerprint` —— 打包腿没在跑依赖指纹门，\
         缺核时它会静静跳过而没人知道"
    );
}
