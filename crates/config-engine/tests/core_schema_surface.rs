//! 🔴 **随包核换一版、它的配置 schema 面变了，本门就红 —— 逼人回来看一眼本仓的结构体要不要跟。**
//!
//! # 为什么需要这一道
//!
//! 本仓的 `crates/config-engine/src/singbox/` 是**手写**的一套下发结构体，只建模了内核 schema 的
//! 一个子集（2026-08-08 那次全量审计人工算过一次差集）。这是**有意的**：不该把内核每个字段都开出来。
//! 问题不在差集存在，在于**差集只被人工算过那一次**：
//!
//! - 随包核 bump 之后，内核新增/改名/删除的字段**不会让任何东西变红** ——
//!   生成的配置一个字节不变、`sing-box check` 照样 rc=0、全仓单测照样全绿。
//! - 于是「我们没建模的那部分」到底有多大、变成了什么，**换核之后无人知晓**，
//!   直到下一次有人再手工跑一遍全量审计。上一次是 2026-08-08，下一次没有排期。
//!
//! 这正是本仓反复吃亏的那类「绿没有信息量」：门在、绿着，但它从没看过这件事。
//!
//! # 判据：钉 schema 面本身，不钉「差集为零」
//!
//! 差集**本来就不该是零**（我们只建模需要的那部分），所以「无缺口」不是可断言的东西。
//! 本门钉的是**内核那一侧的形状**：把 `sing-box schema` 输出里每个 `$defs` 下的属性路径
//! 排序落成夹具。核一换，这份清单必变 ⇒ 本门红，且 diff 直接告诉你**哪个字段进来了、哪个走了**，
//! 人据此决定本仓结构体要不要跟。红不等于有 bug，红等于「该看一眼了」。
//!
//! 这与 `core_dep_fingerprint.rs` 是同一套思路的两个方向：那道钉**依赖版本**（我们靠的默认值有没有变），
//! 这道钉**配置面**（我们要建模的东西有没有变）。
//!
//! # 射程（自曝，别把绿读大）
//!
//! - 本门**不**验本仓结构体的完整性 —— 它只保证「内核那侧没变过」。结构体该不该补，是人看 diff 之后的判断。
//!   把「我们的字段集」也自动算进来需要解析 Rust 源码，那层脆性比它挡住的风险更大，故不做。
//! - schema 由 Go 侧**编译期类型反射**产出 ⇒ 原理上可能随 GOOS 不同。本门只能跑**本机那一个平台**
//!   的二进制（外平台的核在本机执行不了），但打包腿是四条腿各跑各的核 ⇒ CI 上四平台都被看过。
//!   单份共享夹具的依据是 2026-08-09 逐条核过的上游源码：`option/` 零 build tag；`include/` 里
//!   三对 GOOS/cgo 门控（ccm / usbip / resolved）两侧注册的是**同一批 option 类型与同一个 type 常量**；
//!   option 字段类型闭包里唯一的 GOOS 门控类型（`miekg/dns.SessionUDP`）不可达。
//!   该论证的一条腿由 `core_build_matrix` 守着（四份核 tag 集只差 `with_purego`），断了会先在那边红。
//! - 看**属性路径的存在性 + 取值域**（`enum` / `const`），不看类型与数值上下界。
//!   取值域这一维是 2026-08-09 补的，回放对照见 [`the_domain_dimension_catches_value_only_narrowing`]。
//! - 🔴 **schema 会「多报」**：它描述的是**结构体形状**，不是**这个构建收不收**。
//!   现成反例：`ExperimentalOptions` 恒含 `v2ray_api`，而随包核的 build tag 里没有 `with_v2ray_api`
//!   ⇒ 字段在 schema 里、下发下去照样被拒。「这个构建到底编了什么」由 `core_build_matrix` 回答，
//!   「这份配置这个核收不收」由起核时的 `sing-box check` 回答（`proxy.rs::generate_and_gate`）。
//!   三者各管一格，别拿其中任一条的绿去读另外两格。
//! - 缺核时跳过；`POLARIS_REQUIRE_KERNEL_GATE=1` 时缺核直接红（与 `core_dep_fingerprint` /
//!   `kernel_accepts_outbounds` 同一套接线，打包腿强制生效，`ci_step_still_wired` 守着）。
//! - 🔴 **「跳过」是静默的**：下面那句 `eprintln!` 归 libtest 捕获，**只在测试失败时才回放**。
//!   CI 的 ubuntu 腿不拉核 ⇒ 那条绿只说明「编得过 + 抽取器自检过」，没有比对过任何 schema。
//!
//! # 夹具怎么重生
//!
//! `POLARIS_REGEN_SCHEMA_SURFACE=1 cargo test -p polaris-config-engine --test core_schema_surface`
//! 重生成夹具后**必须人工读 diff**再提交 —— 重生器与校验器是同一段代码，无脑重生等于把门关掉。
//! 为此另设两道**独立于夹具**的下界（`$defs` 数与路径数），抽取器一旦失效（返回空/半份）当场红，
//! 而不是安静地把一份空夹具写回去。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 夹具：`sing-box schema` 输出里 `<$defs 名>.<属性名>` 的排序去重清单。
const FIXTURE: &str = "tests/fixtures/core-schema-surface.txt";

/// 抽取器自检下界 —— **独立于夹具**，故夹具被重生成空/半份时仍会红。
///
/// 取值来自 1.14.0-beta.7 实测（93 个 `$defs` / 2631 条路径）后**向下留出余量**：
/// 不是「等于今天的数」（那由夹具负责），是「抽取器还在干活」的地板。留够余量是因为上游
/// 完全可能合法地砍掉一批协议分支，那时该红的是夹具（有 diff 可读），不是这两条地板。
const MIN_DEFS: usize = 60;
const MIN_PATHS: usize = 1500;
/// 取值域行（`… = [ … ]`）的地板，同样**独立于夹具**。1.14.0-beta.7 实测 204 条，向下留余量
/// （与 `MIN_PATHS` 同口径：地板管「抽取器还在干活」，具体数由夹具管）。
const MIN_DOMAIN_LINES: usize = 150;

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

/// 一个属性自身的**取值域**：`enum` 的全部取值 + `const` 的单值，按 JSON 字面量排序去重。
///
/// 沿 `anyOf` / `oneOf` / `allOf` / `items` 下钻（同一个字段常写成「标量或标量数组」两副面孔，
/// 取值域挂在里层），但**不进 `properties`** —— 那是下一级字段的事，粒度与本门的路径一致。
///
/// 用 JSON 字面量而不是裸值渲染：上游确有 `enum: ["default", ""]` 这种含空串的取值域，
/// 裸拼会把它变成一个看不见的差异。
fn collect_domain(node: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
    match node {
        serde_json::Value::Array(items) => {
            for v in items {
                collect_domain(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(vals)) = map.get("enum") {
                out.extend(vals.iter().map(|v| v.to_string()));
            }
            if let Some(c) = map.get("const") {
                out.insert(c.to_string());
            }
            for (k, v) in map {
                if k != "properties" && k != "enum" && k != "const" {
                    collect_domain(v, out);
                }
            }
        }
        _ => {}
    }
}

/// 递归收集一个 `$defs` 条目下所有 `properties` 的键，**按协议分支分域**，并给每个属性附上取值域。
///
/// 三处不能省：
///
/// 1. **通用递归**而不是只看顶层 `properties`：上游用 `oneOf` / `allOf` 表达协议分支，
///    只看顶层会漏掉绝大部分协议字段（`Inbound` 这一个 def 下就挂着 19 个分支）。
/// 2. **分支判别符入作用域**：每个 `oneOf` 分支带 `properties.type.const`（如 `"tun"`），
///    据此把作用域记成 `Inbound[tun]`。不这么做的话所有入站类型会被压成同一个 `Inbound.<字段>`：
///    字段从一个协议挪到另一个协议、或某协议整支被删而别处恰好同名，**清单一个字都不变**。
///    用 `const` 的值而不是分支下标，是因为下标会随上游注册顺序漂，值不会。
/// 3. **取值域另出一行**（`{scope}.{k} = [...]`，见 [`collect_domain`]）：只钉存在性的话，
///    「字段还在、但某个 enum 取值没了」结构性看不见 —— 而那正是最难查的一类
///    （配置照常生成、没覆盖到该取值的 `check` 照样 rc=0，用户恰好选中它时才炸）。
fn collect_props(node: &serde_json::Value, scope: &str, out: &mut Vec<String>) {
    if let serde_json::Value::Array(items) = node {
        for v in items {
            collect_props(v, scope, out);
        }
        return;
    }
    let serde_json::Value::Object(map) = node else {
        return;
    };
    let branch = map
        .get("properties")
        .and_then(|p| p.get("type"))
        .and_then(|t| t.get("const"))
        .and_then(|c| c.as_str());
    let scoped: String;
    let scope = match branch {
        Some(c) => {
            scoped = format!("{scope}[{c}]");
            scoped.as_str()
        }
        None => scope,
    };
    if let Some(serde_json::Value::Object(props)) = map.get("properties") {
        for (k, sub) in props {
            out.push(format!("{scope}.{k}"));
            // 分支判别符（`type` 的 const）已经写进作用域名了，再收一遍是纯重复。
            // 但 `type` 也可能挂**真的** enum（如 `DNSRule[…].type = ["default",""]`），
            // 故只在「这个 const 就是本节点的判别符」时跳过，不按字段名一刀切。
            if branch.is_some() && k == "type" {
                continue;
            }
            let mut domain = std::collections::BTreeSet::new();
            collect_domain(sub, &mut domain);
            if !domain.is_empty() {
                out.push(format!(
                    "{scope}.{k} = [{}]",
                    domain.into_iter().collect::<Vec<_>>().join(",")
                ));
            }
        }
    }
    for (k, v) in map {
        // `properties` 的值本身是「字段名 → 子 schema」的映射，字段名不是 schema 关键字；
        // 已在上面收过，再递归进去会把子 schema 里的嵌套字段挂到同一个作用域名下。
        // 那不是本门要钉的粒度（会把 diff 噪声放大一个量级），故跳过。
        if k != "properties" {
            collect_props(v, scope, out);
        }
    }
}

/// 跑 `sing-box schema` 并抽出排序去重的属性路径清单。
///
/// **纯本地**：该子命令不读配置、不联网（`cmd/sing-box/cmd_schema.go` 由 `include.Context()`
/// 的注册表反射产出）。故本门不违反「禁跑触碰网络的测试」。
fn schema_surface(core: &Path) -> (usize, Vec<String>, usize) {
    let out = Command::new(core)
        .arg("schema")
        .output()
        .unwrap_or_else(|e| panic!("跑 `{} schema` 失败：{e}", core.display()));
    assert!(
        out.status.success(),
        "`{} schema` 退出码非零：{:?}\nstderr: {}",
        core.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("`sing-box schema` 的输出不是合法 JSON");
    let defs = doc
        .get("$defs")
        .and_then(|v| v.as_object())
        .expect("`sing-box schema` 输出里没有 `$defs`");
    let mut paths = Vec::new();
    for (name, body) in defs {
        collect_props(body, name, &mut paths);
    }
    paths.sort_unstable();
    paths.dedup();
    let domains = paths.iter().filter(|p| p.contains(" = [")).count();
    (defs.len(), paths, domains)
}

#[test]
fn bundled_core_config_schema_surface_is_unchanged() {
    let Some(core) = bundled_core() else {
        let required = std::env::var("POLARIS_REQUIRE_KERNEL_GATE").is_ok_and(|v| v == "1");
        assert!(
            !required,
            "POLARIS_REQUIRE_KERNEL_GATE=1 但盘上没有随包核 —— \
             打包腿的 `node scripts/fetch-core.mjs` 是不是失败了？（schema 面门未执行）"
        );
        eprintln!(
            "⚠ 跳过 schema 面门：盘上没有随包核（`.gitignore` 的 /resources/*）。\
             跑 `node scripts/fetch-core.mjs` 后本门自动生效。"
        );
        return;
    };

    let (n_defs, paths, n_domains) = schema_surface(&core);

    // 抽取器自检（独立于夹具）：抽空/抽半份时先在这里红，而不是与一份同样残缺的夹具「对上」。
    assert!(
        n_defs >= MIN_DEFS,
        "抽取器疑似失效：只抽到 {n_defs} 个 $defs（下界 {MIN_DEFS}）"
    );
    assert!(
        paths.len() >= MIN_PATHS,
        "抽取器疑似失效：只抽到 {} 条属性路径（下界 {MIN_PATHS}）",
        paths.len()
    );
    // 取值域是**后加的一维**，必须有自己的地板：只靠上面那条的话，取值域抽取整个坏掉
    // （collect_domain 返回空）时总行数仍远在 MIN_PATHS 之上 ⇒ 夹具被重生成「只剩路径」的样子，
    // 而门保持绿。这正是本仓反复吃亏的「两扇门之间的缝」。
    assert!(
        n_domains >= MIN_DOMAIN_LINES,
        "取值域抽取器疑似失效：只抽到 {n_domains} 条取值域（下界 {MIN_DOMAIN_LINES}）"
    );

    let fixture_path = repo_root().join("crates/config-engine").join(FIXTURE);
    let rendered = paths.join("\n") + "\n";

    if std::env::var("POLARIS_REGEN_SCHEMA_SURFACE").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(fixture_path.parent().expect("夹具目录")).expect("建夹具目录失败");
        std::fs::write(&fixture_path, &rendered).expect("写夹具失败");
        eprintln!(
            "已重生夹具 {}（{} 个 $defs / {} 条路径）—— 提交前必须人工读 diff。",
            fixture_path.display(),
            n_defs,
            paths.len()
        );
        return;
    }

    let expected = std::fs::read_to_string(&fixture_path).unwrap_or_else(|e| {
        panic!(
            "读不到夹具 {}：{e}\n首次落地请跑 \
             `POLARIS_REGEN_SCHEMA_SURFACE=1 cargo test -p polaris-config-engine --test core_schema_surface`",
            fixture_path.display()
        )
    });

    if rendered == expected {
        return;
    }

    // 不 diff 整份（2600+ 行），只报增删两侧 —— 红的时候要一眼看出「哪个字段进来了」。
    let have: std::collections::BTreeSet<&str> = rendered.lines().collect();
    let want: std::collections::BTreeSet<&str> = expected.lines().collect();
    let added: Vec<&&str> = have.difference(&want).collect();
    let removed: Vec<&&str> = want.difference(&have).collect();
    panic!(
        "随包核的配置 schema 面变了（新增 {} 条 / 消失 {} 条）。\n\
         这不一定是 bug，但**必须有人看一眼**本仓 `src/singbox/` 的结构体要不要跟。\n\
         新增：{:?}\n消失：{:?}\n\
         看过并决定之后重生夹具：\
         `POLARIS_REGEN_SCHEMA_SURFACE=1 cargo test -p polaris-config-engine --test core_schema_surface`",
        added.len(),
        removed.len(),
        added.iter().take(40).collect::<Vec<_>>(),
        removed.iter().take(40).collect::<Vec<_>>(),
    );
}

/// 打包腿上本门必须真的在跑 —— 否则「缺核强制红」那条开关形同虚设，
/// 而 ci.yml 那边的绿只说明「编得过」（不拉核 ⇒ 恒走跳过分支）。
/// 回放本门自己的历史失效面：**字段名一个不变、只有 enum 取值域收窄**。
///
/// 加取值域这一维之前，本门对这种变化恒为绿 —— 而它的用户侧后果是最难查的一种：
/// 配置照常生成、`sing-box check` 在没覆盖到那个取值的场景下照样 rc=0，
/// 只有当用户恰好选了被删掉的那个值（如 TUN 栈选 `system`）才在起核时炸。
///
/// 本测试**不碰随包核**，纯喂合成 schema 片段 ⇒ 缺核时也照跑，是本门唯一恒有牙的一条。
#[test]
fn the_domain_dimension_catches_value_only_narrowing() {
    let collect = |v: &serde_json::Value| {
        let mut out = Vec::new();
        collect_props(v, "Inbound", &mut out);
        out.sort();
        out
    };
    let only_paths = |v: &[String]| -> Vec<String> {
        v.iter().filter(|s| !s.contains(" = [")).cloned().collect()
    };

    let wide = serde_json::json!({
        "properties": {
            "type": { "const": "tun" },
            "stack": { "enum": ["system", "gvisor", "mixed"] }
        }
    });
    let narrow = serde_json::json!({
        "properties": {
            "type": { "const": "tun" },
            "stack": { "enum": ["gvisor", "mixed"] }
        }
    });
    let (a, b) = (collect(&wide), collect(&narrow));

    // 路径维一模一样 —— 这正是旧判据看不见它的原因，也是本条存在的理由。
    assert_eq!(
        only_paths(&a),
        only_paths(&b),
        "两份片段的路径维本就该相同，不同说明这条对照没打在取值域上"
    );
    // 取值域维必须把它抓出来。
    assert_ne!(
        a, b,
        "enum 取值域收窄没有被抽取器看见 —— 新加的这一维没有牙"
    );
    assert!(a.contains(&r#"Inbound[tun].stack = ["gvisor","mixed","system"]"#.to_owned()));

    // 判别符 `type` 的 const 已写进作用域名，不重复收；但**真的** enum 仍要收。
    let discriminator_only = collect(&serde_json::json!({
        "properties": { "type": { "const": "tun" }, "listen": { "type": "string" } }
    }));
    assert!(!discriminator_only
        .iter()
        .any(|s| s.starts_with("Inbound[tun].type = ")));
    let real_type_enum = collect(&serde_json::json!({
        "properties": { "type": { "enum": ["default", ""] } }
    }));
    assert!(real_type_enum.contains(&r#"Inbound.type = ["","default"]"#.to_owned()));

    // 取值域挂在 anyOf/items 里层（「标量或标量数组」两副面孔）也要下钻到。
    let nested = collect(&serde_json::json!({
        "properties": {
            "network": { "anyOf": [ { "enum": ["tcp", "udp"] }, { "items": { "enum": ["tcp", "udp"] } } ] }
        }
    }));
    assert!(nested.contains(&r#"Inbound.network = ["tcp","udp"]"#.to_owned()));

    // 但不得钻进下一级字段的 `properties` —— 那是它们自己作用域的事。
    let nested_object = collect(&serde_json::json!({
        "properties": { "tls": { "properties": { "min_version": { "enum": ["1.2", "1.3"] } } } }
    }));
    assert!(
        !nested_object
            .iter()
            .any(|s| s.starts_with("Inbound.tls = [")),
        "把下一级字段的取值域挂到了父字段名下：{nested_object:?}"
    );
}

#[test]
fn ci_step_still_wired() {
    let wf = repo_root().join(".github/workflows/package.yml");
    let raw =
        std::fs::read_to_string(&wf).unwrap_or_else(|e| panic!("读不到 {}: {e}", wf.display()));
    // 匹配 **run 命令本身**而不是裸词：本文件在 package.yml 里还留了一段说明注释，注释里就写着
    // `core_schema_surface`。用裸词做判据的话，把整个 step 删掉、注释留下，本断言照样绿 ——
    // 本仓两天内因此踩空两次（见 memory `feedback_gate_exists_but_toothless`）。
    assert!(
        raw.contains("--test core_schema_surface"),
        "package.yml 里找不到 `--test core_schema_surface` —— 打包腿没在跑 schema 面门，\
         缺核时它会静静跳过而没人知道"
    );
}
