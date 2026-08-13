//! 🔴 **随包核里任意一个平台的构建面变了，本门就红** —— 构建面 = `GOOS` / `GOARCH` /
//! `CGO_ENABLED` + Go build tag 集。
//!
//! # 为什么需要这一道
//!
//! 本仓下发的配置里，有整整一批字段的可用性**不由配置决定，由核编译时的 build tag 决定**：
//!
//! | tag | 没了会怎样 | 本仓的消费点 |
//! |---|---|---|
//! | `with_quic` | hysteria2 / tuic / QUIC 传输整片被拒 | `singbox/outbound.rs` |
//! | `with_utls` | `tls.utls` 指纹字段被拒 | `singbox/outbound.rs` |
//! | `with_wireguard` / `with_tailscale` | 对应 endpoint 起不来 | `singbox/endpoint.rs` |
//! | `with_naive_outbound` | naive 出站被拒 | `singbox/outbound.rs` |
//! | `with_gvisor` | TUN 的 gVisor 栈不可选 | `runtime/proxy.rs` |
//! | `with_clash_api` | 面板 / 外部控制器整块失效 | `singbox/config.rs` |
//!
//! 关键在于**这件事是逐平台的**：官方发布矩阵完全可能只在某一个 GOOS 上改 tag 集。
//! 而本仓其余两道核相关的门（`core_dep_fingerprint` / `core_schema_surface`）读的都是
//! **本机那一个平台**的二进制 ⇒ 在 Linux 开发机上、在 CI 的 ubuntu 腿上，
//! 「mac 那份核这一版没编 `with_tailscale`」是**结构性看不见**的：
//! 生成的配置一字节不变、`sing-box check` 在 Linux 上照样 rc=0、全仓单测照样全绿，
//! 直到 mac 用户点开一个 Tailscale 节点。
//!
//! 本门把四份核**一起**读，且判据是纯字节扫描（见下），故在任意宿主上都能把四个平台都看一遍。
//!
//! # 它同时守着 `core_schema_surface` 的前提
//!
//! 那道门只落了**一份**夹具，而它在打包腿上是四条腿各跑各的核。单份夹具成立的依据是
//! 「schema 面四平台恒等」，该结论的一条腿正是**四份核的 tag 集只差 `with_purego`**
//! （2026-08-09 逐条核过上游：`option/` 零 build tag；`include/` 里三对 GOOS/cgo 门控
//! ——ccm / usbip / resolved——两侧注册的是同一批 option 类型与同一个 type 常量；
//! `with_purego` 在 sing-box 模块内零引用，是依赖层的 tag）。
//! tag 集一旦真的分叉，那份共享夹具的依据就断了 ⇒ 本门的 [`tag_sets_differ_only_by_the_documented_extras`]
//! 会先红一步，提示去看 schema 夹具要不要按平台拆。
//!
//! # 射程（自曝，别把绿读大）
//!
//! - tag 只说明「这块代码**编进来了**」，不说明「它真的能用」。运行期能力仍由起核时的
//!   `sing-box check`（`proxy.rs::generate_and_gate`）负责。
//! - `core-manifest.json` 的 `coreArchiveSha256` 已经逐字节钉住了每个平台的压缩包 ⇒
//!   tag 集**不可能在不动 sha 的情况下漂移**。所以本门只在**换核 bump 那一刻**才有机会红 ——
//!   那正是它存在的意义（与 `core_dep_fingerprint` / `core_schema_surface` 同一套「换核即自曝」）。
//! - 盘上缺哪个平台就少看哪个平台；`POLARIS_REQUIRE_KERNEL_GATE=1` 时四份缺一即红
//!   （打包腿的 `node scripts/fetch-core.mjs` **不传 `--platform` = 全平台**，四份必然在盘上）。
//! - 🔴 「跳过」是静默的：下面那句 `eprintln!` 归 libtest 捕获，只在失败时回放。
//!   `ci.yml` 的 ubuntu 腿不拉核 ⇒ 本门在那边恒为空跑。真正的执行只在打包腿。
//!
//! # 没有「重生夹具」这条路，是有意的
//!
//! 期望值直接写在下面的表里，改它必须手敲。`core_schema_surface` 那份 2631 行夹具不得不给重生器，
//! 而本门只有四行 × 十几个 tag ——留一个 `POLARIS_REGEN_*` 等于把「人看一眼」这一步删掉。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 四平台共有的 build tag（排序）。
///
/// 取自 1.14.0-beta.7 官方发布资产实测。**不是「应该有这些」，是「今天就是这些」** ——
/// 上游合法地增删 tag 时本门会红，读 diff 决定本仓要不要跟。
const SHARED_TAGS: &[&str] = &[
    "badlinkname",
    "tfogo_checklinkname0",
    "with_acme",
    "with_ccm",
    "with_clash_api",
    "with_cloudflared",
    "with_dhcp",
    "with_gvisor",
    "with_naive_outbound",
    "with_ocm",
    "with_openconnect",
    "with_openvpn",
    "with_quic",
    "with_tailscale",
    "with_usbip",
    "with_utls",
    "with_wireguard",
];

/// 逐平台额外的 tag。今天只有一条：`with_purego` 只出现在 linux / windows，mac 两份没有
/// （mac 走 `CGO_ENABLED=1`，用不着 purego 那套无 cgo 兜底）。
struct CoreBuild {
    /// 与 `scripts/fetch-core.mjs` 的 `TARGETS[].key` 同名。
    key: &'static str,
    rel: &'static str,
    goos: &'static str,
    goarch: &'static str,
    cgo: &'static str,
    extra_tags: &'static [&'static str],
}

const MATRIX: &[CoreBuild] = &[
    CoreBuild {
        key: "linux",
        rel: "resources/linux/sing-box",
        goos: "linux",
        goarch: "amd64",
        cgo: "0",
        extra_tags: &["with_purego"],
    },
    CoreBuild {
        key: "win",
        rel: "resources/win/sing-box.exe",
        goos: "windows",
        goarch: "amd64",
        cgo: "0",
        extra_tags: &["with_purego"],
    },
    CoreBuild {
        key: "mac-arm64",
        rel: "resources/mac-arm64/sing-box",
        goos: "darwin",
        goarch: "arm64",
        cgo: "1",
        extra_tags: &[],
    },
    CoreBuild {
        key: "mac-x64",
        rel: "resources/mac-x64/sing-box",
        goos: "darwin",
        goarch: "amd64",
        cgo: "1",
        extra_tags: &[],
    },
];

impl CoreBuild {
    fn expected_tags(&self) -> BTreeSet<String> {
        SHARED_TAGS
            .iter()
            .chain(self.extra_tags)
            .map(|s| (*s).to_owned())
            .collect()
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <repo>/crates/config-engine")
        .to_path_buf()
}

fn kernel_gate_required() -> bool {
    std::env::var("POLARIS_REQUIRE_KERNEL_GATE").is_ok_and(|v| v == "1")
}

/// 把 Go 二进制内嵌 buildinfo 的 `build` 设置区**一趟扫完**建成表。
///
/// 设置区是纯文本、制表符分隔、换行结尾，形如：
/// `build\tGOOS=linux\nbuild\t-tags=with_quic,with_utls\n`
///
/// 不走 `go version -m`：CI 的 Rust 腿**没有 Go 工具链**（`ci.yml` 开头明载），
/// 挂一个本机有、CI 没有的外部命令等于把本门在 CI 上变成静默跳过。
/// 判据与 `core_dep_fingerprint::extract_dep_version` 同源，只是换了记录类型。
///
/// 为什么一趟扫完而不是「每个键各扫一次」：随包核单份 ~50MB，逐键朴素子串搜索实测
/// 四份核共 14s；本实现每轮把游标推过已匹配位置 ⇒ 全程线性，同样四份 ~3s。
///
/// - 前缀 `build\t` 是判据的一部分 ⇒ 二进制里裸出现的 `GOOS=` 不会被收进来。
/// - 值到第一个 `\t` / `\n` / `\0` 为止；**截断的记录整条丢弃**，不留半截值。
/// - 同名键先到先得（buildinfo 里每个键只出现一次；重复即异常输入，取第一条更稳）。
fn build_settings(bin: &[u8]) -> BTreeMap<String, String> {
    const PREFIX: &[u8] = b"build\t";
    let mut out = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < bin.len() {
        let Some(off) = bin[cursor..]
            .windows(PREFIX.len())
            .position(|w| w == PREFIX)
        else {
            break;
        };
        let start = cursor + off + PREFIX.len();
        let rest = &bin[start..];
        let Some(end) = rest.iter().position(|b| matches!(*b, b'\t' | b'\n' | 0)) else {
            break; // 记录被截断：不收，也没有后续可扫
        };
        if let Ok(record) = std::str::from_utf8(&rest[..end]) {
            if let Some((k, v)) = record.split_once('=') {
                out.entry(k.to_owned()).or_insert_with(|| v.to_owned());
            }
        }
        cursor = start + end;
    }
    out
}

/// build tag 集。**顺序无关**：官方 windows 资产的 `-tags` 顺序与其余三份不同
/// （`with_purego` 的位置不一样），按序列比会得到一条纯噪音的红。
fn tag_set(settings: &BTreeMap<String, String>) -> BTreeSet<String> {
    settings
        .get("-tags")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 盘上真实存在的那几份核，连同各自解出的 buildinfo 设置表（字节读完即弃，不驻留 ~50MB×4）。
fn present_cores() -> Vec<(&'static CoreBuild, BTreeMap<String, String>)> {
    let root = repo_root();
    MATRIX
        .iter()
        .filter_map(|c| {
            let path = root.join(c.rel);
            if !path.is_file() {
                return None;
            }
            let bytes =
                std::fs::read(&path).unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()));
            Some((c, build_settings(&bytes)))
        })
        .collect()
}

/// 缺核时的统一处置：`POLARIS_REQUIRE_KERNEL_GATE=1` 下缺一即红，否则跳过。
fn require_all_present(present: &[(&'static CoreBuild, BTreeMap<String, String>)]) -> bool {
    if present.len() == MATRIX.len() {
        return true;
    }
    let missing: Vec<&str> = MATRIX
        .iter()
        .filter(|c| !present.iter().any(|(p, _)| p.key == c.key))
        .map(|c| c.key)
        .collect();
    assert!(
        !kernel_gate_required(),
        "POLARIS_REQUIRE_KERNEL_GATE=1 但盘上缺这些平台的随包核：{} —— \
         打包腿的 `node scripts/fetch-core.mjs`（不传 --platform = 全平台）是不是失败了？\
         （构建面矩阵门未完整执行）",
        missing.join(", ")
    );
    false
}

#[test]
fn every_bundled_core_matches_its_pinned_build_face() {
    let present = present_cores();
    let complete = require_all_present(&present);
    if present.is_empty() {
        eprintln!(
            "⚠ 跳过构建面矩阵门：盘上一份随包核都没有（`.gitignore` 的 /resources/*）。\
             跑 `node scripts/fetch-core.mjs` 后本门自动生效。"
        );
        return;
    }
    if !complete {
        eprintln!("⚠ 构建面矩阵门只看到部分平台，未覆盖的平台本轮没有被检查。");
    }

    for (core, settings) in &present {
        // GOOS 在任何 Go 二进制里都必然存在 ⇒ 拿它当**提取器活性探针**。
        // 提不出来 = 门失效（Go 换了 buildinfo 编码 / 核被 strip / 拿到的不是 Go 二进制），
        // 必须红而不是静静把它当成「没有 tag」。
        let goos = settings.get("GOOS").unwrap_or_else(|| {
            panic!(
                "在 {} 里提不到 `build\\tGOOS=` —— 本门已失效（不是「构建面没变」），先修门再谈结论",
                core.rel
            )
        });
        assert_eq!(
            goos, core.goos,
            "{}: GOOS 变了（{} → {goos}）—— 这一格拿到的不是它该是的那个平台的核",
            core.key, core.goos
        );

        let goarch = settings.get("GOARCH").map(String::as_str).unwrap_or("");
        assert_eq!(
            goarch, core.goarch,
            "{}: GOARCH 变了（{} → {goarch}）",
            core.key, core.goarch
        );

        let cgo = settings
            .get("CGO_ENABLED")
            .map(String::as_str)
            .unwrap_or("");
        assert_eq!(
            cgo, core.cgo,
            "{}: CGO_ENABLED 变了（{} → {cgo}）。darwin 侧靠 cgo 才走真 ccm/usbip 实现，\
             linux/win 侧靠 purego —— 这一格翻转会连带改 build tag 的有效分支",
            core.key, core.cgo
        );

        let actual = tag_set(settings);
        let expected = core.expected_tags();
        if actual != expected {
            let added: Vec<&str> = actual.difference(&expected).map(String::as_str).collect();
            let removed: Vec<&str> = expected.difference(&actual).map(String::as_str).collect();
            panic!(
                "\n{} 这一份随包核的 build tag 集变了：\n  新增：{}\n  消失：{}\n\
                 tag 决定的是「这块代码编没编进来」，配置侧完全看不出来：\
                 生成的配置一字节不变、本机那个平台的 `sing-box check` 照样 rc=0，\
                 而对应平台的用户会在点开某类节点时才发现它不认。\n\
                 ⇒ 先对着模块头那张表看消失的 tag 本仓有没有消费点，再更新 core_build_matrix.rs 的 MATRIX。\n",
                core.key,
                if added.is_empty() {
                    "（无）".to_owned()
                } else {
                    added.join(", ")
                },
                if removed.is_empty() {
                    "（无）".to_owned()
                } else {
                    removed.join(", ")
                },
            );
        }
    }
}

/// 与上面那条**互不依赖**：它读 `MATRIX` 的期望值，这条只读盘上四份核彼此的关系。
///
/// 为什么要两条：换核 bump 时人会成批更新 `MATRIX`，此时上面那条按定义会绿。
/// 而「四平台 tag 集只差 `with_purego`」是 `core_schema_surface` 只落**一份**共享夹具的依据，
/// 它断了必须有人知道 —— 本条不看 `MATRIX`，成批更新压不住它。
#[test]
fn tag_sets_differ_only_by_the_documented_extras() {
    let present = present_cores();
    require_all_present(&present);
    if present.len() < 2 {
        eprintln!(
            "⚠ 跳过跨平台 tag 集比对：盘上只有 {} 份核，比不出「平台间差异」。",
            present.len()
        );
        return;
    }

    // 允许出现平台间差异的 tag = MATRIX 里登记过的 extra 的并集。其余任何差异都是新情况。
    let sanctioned: BTreeSet<&str> = MATRIX
        .iter()
        .flat_map(|c| c.extra_tags.iter().copied())
        .collect();

    let sets: Vec<(&str, BTreeSet<String>)> =
        present.iter().map(|(c, s)| (c.key, tag_set(s))).collect();
    let (base_key, base) = &sets[0];

    for (key, other) in &sets[1..] {
        let diff: Vec<&str> = base
            .symmetric_difference(other)
            .map(String::as_str)
            .filter(|t| !sanctioned.contains(t))
            .collect();
        assert!(
            diff.is_empty(),
            "\n{base_key} 与 {key} 的 build tag 集出现了**未登记**的平台差异：{}\n\
             这条断言是 `core_schema_surface` 只落一份共享夹具的依据之一 ——\
             tag 集真的按平台分叉后，schema 面「四平台恒等」的论证就断了一条腿。\n\
             ⇒ 要么把新差异登记进 MATRIX 的 extra_tags 并说明为什么，\
             要么去确认 schema 夹具是不是也得按平台拆。\n",
            diff.join(", ")
        );
    }
}

/// 正向对照：证明提取器真的会「提不到」和「提错不了」，否则上面两条可能只是碰巧绿。
#[test]
fn the_extractor_has_teeth() {
    let ok = build_settings(
        b"...\0build\tGOOS=darwin\nbuild\t-tags=with_quic,with_utls\nbuild\tCGO_ENABLED=1\n",
    );
    assert_eq!(ok.get("GOOS").map(String::as_str), Some("darwin"));
    assert_eq!(ok.get("CGO_ENABLED").map(String::as_str), Some("1"));
    assert_eq!(
        tag_set(&ok),
        ["with_quic", "with_utls"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>()
    );

    // 只收 `build\t` 打头的记录：光有 `GOOS=` 不算（否则二进制里任何裸文本都能污染判据）
    assert!(build_settings(b"GOOS=linux\nCGO_ENABLED=9\n").is_empty());

    // 键名精确：`-tagsx` 不会被当成 `-tags`
    assert!(tag_set(&build_settings(b"build\t-tagsx=nope\n")).is_empty());

    // 截断（值后既无分隔符也无 NUL）→ 整条丢弃，不留半截值
    assert!(build_settings(b"build\tGOOS=linu").is_empty());

    // 提不到 `-tags` → 空集（不是 panic）：那一格由 GOOS 探针负责区分「门坏了」与「真没 tag」
    assert!(tag_set(&build_settings(b"build\tGOOS=linux\n")).is_empty());

    // 顺序无关：windows 资产的 -tags 顺序与其余三份不同
    let reordered = build_settings(b"build\t-tags=with_utls,with_quic\n");
    assert_eq!(tag_set(&ok), tag_set(&reordered));
}

/// 打包腿上本门必须真的在跑 —— 否则「缺核强制红」那条开关形同虚设。
#[test]
fn ci_step_still_wired() {
    let wf = repo_root().join(".github/workflows/package.yml");
    let raw =
        std::fs::read_to_string(&wf).unwrap_or_else(|e| panic!("读不到 {}: {e}", wf.display()));
    // 匹配 **run 命令本身**而不是裸词：本文件在 package.yml 里还留了一段说明注释，
    // 注释里就写着 `core_build_matrix`。用裸词做判据的话，把整个 step 删掉、注释留下，
    // 本断言照样绿（本仓两天内被同一形状骗过两次，见 handoff §4）。
    assert!(
        raw.contains("--test core_build_matrix"),
        "package.yml 里找不到 `--test core_build_matrix` —— 打包腿没在跑构建面矩阵门，\
         缺核时它会静静跳过而没人知道"
    );
}
