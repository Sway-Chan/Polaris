//! 🔴 **随包内核真的收得下我们生成的出站吗** —— 把 Rust 生成的 `outbounds`/`endpoints` 喂给
//! `sing-box check`，看它加不加载得动。
//!
//! # 为什么必须有这一道
//!
//! 2026-08-07 之前，`config-engine` 有 **620 条单测全绿**，而它生成的配置喂给随包核是
//! `rc=1 FATAL`。同一类缺陷因此出货了**两次**：
//!
//! | 缺陷 | 单测怎么说的 | 真核怎么说的 |
//! |---|---|---|
//! | `Protocol::Http` 把 `http_settings` 塞进 `ob.transport` | 全绿 | `decode config: outbounds[0].transport: json: unknown field "transport"` |
//! | 传输层用黑名单（内核是白名单，只有 trojan/vless/vmess 有 `transport`） | 全绿 | 同上，且波及 14 个协议 |
//!
//! 两次都不是断言写错，是**没有任何一道门问过内核**。仓里所有 config-gen 的门都停在
//! 「我写的函数返回了什么」这一层：金样对拍问的是「跟冻结的 TS 输出一不一致」（两边一起错就一起绿），
//! serde 往返问的是「自己序列化自己反序列化对不对」。**没有一道问「这个东西真的能用吗」。**
//!
//! # 射程（自曝，别把绿读大）
//!
//! - **只喂 `outbounds` + `endpoints` + 它们引用的 `dns.servers`**，刻意不喂 `route`/`inbounds`：
//!   那两块会把 `.srs` 规则集文件缺失、TUN 需要权限之类**与出站正确性无关**的失败混进来，
//!   门一旦经常因无关原因红，就会被人加 `-- --skip` 绕过。要补 route/inbounds 那一层得另立一道，
//!   判据也不同（那边该在起核闸门 `core-supervisor::config_gate` 里，已在跑）。
//! - **`check` 只跑 decode + initialize，不跑 Start** —— 实测 `selector.outbounds` 指向不存在的 tag
//!   仍 `rc=0`，而真起核是 `dependency[X] not found`。悬空引用抓不到，那由
//!   `prune_detour_dead_references` / `fix_route_dead_references` 与起核闸门管。
//! - 不验行为，只验「内核肯把它加载进来」。节点连不连得上是真机的事。
//!
//! # 两个阶段，两种口径（这是本门最要紧的一条设计）
//!
//! `check` 的失败分两个阶段，**成因完全不同**：
//!
//! | 阶段 | 诊断形状 | 谁的责任 | 本门口径 |
//! |---|---|---|---|
//! | decode | `decode config at …: outbounds[0].transport: json: unknown field …` | **本 builder 的形状决策** | **零容忍** |
//! | initialize | `initialize outbound[1]: invalid uuid: …` | 节点**内容**（用户/订阅给的凭据） | 逐条登记的棘轮 |
//!
//! 两次出货的缺陷都在 decode —— 那正是「我们把字段摆在哪个容器里」这件事，builder 全权负责。
//! 而 initialize 阶段验的是凭据内容：金样夹具里的 `uuid-1` / 假 reality 公钥 / 假 ECH pem /
//! 假 ssh 私钥都过不了，**那是夹具的性质，不是缺陷**。
//!
//! 把 initialize 整个豁免掉是不行的（reality × `tls.engine` 那类冲突就发生在这一阶段）。故做成
//! **逐条登记的棘轮**：每一条允许的 initialize 失败都要写明场景名与内核原话，冒出没登记的立刻红；
//! 反向也锁 —— 登记了却不再发生的条目同样红，免得表里堆满没人敢删的陈年豁免。
//!
//! # 核不在盘上时（`.gitignore` 的 `/resources/*`，由 `scripts/fetch-core.mjs` 拉）
//!
//! 缺核时跳过，同时提供硬化开关：`POLARIS_REQUIRE_KERNEL_GATE=1` 时缺核直接**红**。
//! `package.yml` 的打包腿在 `fetch-core.mjs` 之后带着该变量跑本门 —— 于是「拉核失败」与
//! 「本门没跑」在**那条腿上**都会自曝，而不是静静变成一条绿。这条 CI 接线本身也有门守着，见
//! `ci_step_still_wired`：删掉那一步会转红。
//!
//! 🔴 **「跳过」这件事本身是静默的，别把它读成会自曝**（2026-08-07 实测更正，原文写的是
//! 「跳过并大声说」）：下面那句 `eprintln!` 归 libtest 捕获，**只在测试失败时才回放** ——
//! 通过时它一个字都不出现。实测 CI ubuntu 腿（不拉核）的日志里只有一行
//! `bundled_core_accepts_every_generated_outbound_set ... ok`，grep 提示语零命中。
//! ⇒ **ubuntu 腿上这条绿只说明「编得过」，没有比对过任何东西**；真正的执行只在打包腿。
//! 本地要看见提示语得显式 `-- --nocapture`。
//!
//! 分工与 `singbox-grpc/tests/bundled_core_wire.rs` 同构（那道门守 proto wire 漂移）：
//! 开发机上核已 fetch 就自动生效，打包腿上强制生效。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use polaris_config_engine::builder::{generate_sing_box_config, GenerateConfigDeps};
use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::server_config::ServerConfig;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
struct SnapshotFixture {
    cases: Vec<SnapshotCase>,
}

#[derive(Debug, Deserialize)]
struct SnapshotCase {
    name: String,
    #[serde(default = "default_platform")]
    platform: String,
    input: UserConfig,
}

fn default_platform() -> String {
    "win32".to_string()
}

fn repo_root() -> PathBuf {
    // crates/config-engine → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 应形如 <repo>/crates/config-engine")
        .to_path_buf()
}

/// 本机可用的随包核。缺核 → `None`（调用方决定是跳过还是红）。
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

/// `sing-box --disable-color check -c <path>` → `(rc==0, 诊断原文)`。
///
/// `--disable-color`：sing-box **不看 stdout/stderr 是不是 tty**，不加则诊断行恒带 ANSI 码，
/// 断言里做子串匹配会莫名其妙对不上（同 `core-supervisor::config_gate` 的处理，那边有实测记录）。
fn check(core: &Path, config_path: &Path) -> (bool, String) {
    let out = Command::new(core)
        .arg("--disable-color")
        .arg("check")
        .arg("-c")
        .arg(config_path)
        .output()
        .unwrap_or_else(|e| panic!("跑 {} 失败: {e}", core.display()));
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let diag = if stderr.is_empty() { stdout } else { stderr };
    (out.status.success(), diag)
}

/// 把一份完整配置削成「只剩出站面」的最小可 check 配置。
///
/// 留 `dns.servers` 是因为出站上的 `domain_resolver` 按 tag 指向它们，删了会变成
/// `domain resolver not found` —— 一个与出站正确性无关的失败。`dns.rules` 反而要删：
/// 它可能引用 `route.rule_set`，而 `route` 整块被删掉了。
fn outbound_surface(cfg: &Value) -> Value {
    let mut dns = json!({});
    if let Some(servers) = cfg.get("dns").and_then(|d| d.get("servers")) {
        dns["servers"] = servers.clone();
    }
    let mut out = json!({
        "log": { "disabled": true },
        "outbounds": cfg.get("outbounds").cloned().unwrap_or_else(|| json!([])),
    });
    if dns.get("servers").is_some() {
        out["dns"] = dns;
    }
    if let Some(eps) = cfg.get("endpoints") {
        if !eps.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            out["endpoints"] = eps.clone();
        }
    }
    out
}

fn deps_for(platform: &str) -> GenerateConfigDeps {
    GenerateConfigDeps {
        platform: platform.into(),
        arch: "x86_64".into(),
        race_server_port: 0,
        probe_direct_port: None,
        probe_proxy_port: None,
        update_in_port: None,
        probe_pool_ports: vec![],
        lan_resolver_for_dns: None,
        race_upstream_ips: vec![],
        race_upstream_ports: vec![],
        has_cronet: true,
        cronet_copy_failed: false,
        has_management_api: false,
        privacy_mode: false,
        log_level: polaris_config_engine::user_config::LogLevel::Info,
        disable_log_file: false,
        dashboard_serve_dir: None,
        tailscale_api_port: 0,
        cache_path: "/fake/userData/cache.db".into(),
        log_file_path: Some("/fake/userData/singbox.log".into()),
        runtime_rules_dir: "/fake/userData/rules".into(),
        rule_resources_path: "/fake/userData/rule-resource".into(),
        custom_rules_dir: "/fake/userData/custom-rules".into(),
        tailscale_state_dir_prefix: "/fake/userData/tailscale".into(),
        is_valid_srs_fn: |_| true,
        own_lan_cidrs: vec![],
        log: |_, _| {},
        on_degraded: || {},
    }
}

fn load_cases() -> Vec<SnapshotCase> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/config-snapshot.json");
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("fixtures/config-snapshot.json 读不到: {e}"));
    let fixture: SnapshotFixture =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture 解析失败: {e}"));
    assert!(!fixture.cases.is_empty(), "fixture 为空 —— 本门会变成空转");
    fixture.cases
}

/// 缺核时的统一处置：硬化开关开着就红，否则大声跳过。
fn core_or_skip(what: &str) -> Option<PathBuf> {
    if let Some(c) = bundled_core() {
        return Some(c);
    }
    let required = std::env::var("POLARIS_REQUIRE_KERNEL_GATE").is_ok_and(|v| v == "1");
    assert!(
        !required,
        "POLARIS_REQUIRE_KERNEL_GATE=1 但盘上没有随包核 —— \
         打包腿的 `node scripts/fetch-core.mjs` 是不是失败了？（{what} 未执行）"
    );
    eprintln!(
        "⚠ 跳过 {what}：盘上没有随包核（`.gitignore` 的 /resources/*）。\
         跑 `node scripts/fetch-core.mjs` 后本门自动生效；打包腿带 POLARIS_REQUIRE_KERNEL_GATE=1 强制生效。"
    );
    None
}

/// 允许的 **initialize 阶段**失败 —— 每条都是金样夹具的**占位凭据**，不是缺陷。
///
/// 判据是「内核验的是凭据**内容**，而内容不由 builder 决定」。夹具是 上游 4.2.6 期从 TS 导出的
/// 冻结快照，里面的凭据一律是占位串；给它们换成真凭据既不现实（reality/ECH 要现生成、ssh 要往仓里
/// 塞一把私钥），也不会提高本门对 builder 缺陷的检出力。
///
/// **两个方向都锁**：出现没登记的 initialize 失败 → 红；登记了却不再发生 → 也红。
const FIXTURE_CREDENTIAL_ARTIFACTS: &[(&str, &str)] = &[
    ("多协议 outbound", "invalid uuid"),
    (
        "vless reality + shadowsocks shadow-tls",
        "invalid public_key",
    ),
    (
        "抗封增强（ECH+TLS fragment+multiplex）",
        "invalid ECH configs pem",
    ),
    ("ssh outbound", "parse private key"),
];

/// 诊断行属于哪个阶段。decode 的键路径前缀是 sing-box 稳定的输出格式
/// （`decode config at <file>: <keypath>: <go msg>`），initialize 期的错误没有这个前缀。
fn is_decode_stage(diag: &str) -> bool {
    diag.contains("decode config at")
}

/// 🔴 **金样全部 37 个场景的出站面，随包核必须逐个收下。**
///
/// 覆盖面（按 fixture 实测）：vless 30 · selector 41 · direct/block 各 37 · trojan 3 · vmess 2 ·
/// shadowsocks 2 · snell 2 · hysteria2 / tuic / anytls / socks / http / shadowtls / naive / ssh 各 1，
/// 外加 1 个 wireguard endpoint —— 本仓支持的协议一个不落。
#[test]
fn bundled_core_accepts_every_generated_outbound_set() {
    let Some(core) = core_or_skip("出站面加载门") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("polaris-kgate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let mut decode_failures: Vec<String> = Vec::new();
    let mut unlisted_init_failures: Vec<String> = Vec::new();
    let mut artifacts_seen: Vec<(&str, &str)> = Vec::new();
    let mut checked = 0usize;
    for case in load_cases() {
        let deps = deps_for(&case.platform);
        let Ok(cfg) = generate_sing_box_config(&case.input, &BTreeMap::new(), &deps) else {
            // 生成失败属金样门的射程（那边会红），本门只管「生成得出来的能不能被内核收下」。
            continue;
        };
        let surface = outbound_surface(&serde_json::to_value(&cfg).expect("序列化"));
        let p = dir.join(format!("case-{checked}.json"));
        std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
        checked += 1;
        let (ok, diag) = check(&core, &p);
        if ok {
            continue;
        }
        if is_decode_stage(&diag) {
            decode_failures.push(format!("  · {} → {diag}", case.name));
            continue;
        }
        match FIXTURE_CREDENTIAL_ARTIFACTS
            .iter()
            .find(|(name, needle)| *name == case.name && diag.contains(needle))
        {
            Some(hit) => artifacts_seen.push(*hit),
            None => unlisted_init_failures.push(format!("  · {} → {diag}", case.name)),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        checked >= 30,
        "只检了 {checked} 个场景 —— 夹具缩水或生成大面积失败，本门的覆盖面已经不是它自称的那个"
    );
    // ① decode 阶段零容忍 —— 两次出货的缺陷都在这里。
    assert!(
        decode_failures.is_empty(),
        "随包核在 **decode 阶段**拒绝了 {} / {checked} 个场景的出站面。\
         这类失败是 builder 的形状决策错了（字段摆错容器 / 摆错协议），\
         在真机上会让**整个核起不来**，不止坏掉一个节点：\n{}",
        decode_failures.len(),
        decode_failures.join("\n")
    );
    // ② initialize 阶段：只放行逐条登记过的夹具凭据 artifact。
    assert!(
        unlisted_init_failures.is_empty(),
        "随包核在 initialize 阶段拒绝了 {} 个**未登记**的场景。\
         若确认是夹具占位凭据，补进 FIXTURE_CREDENTIAL_ARTIFACTS 并写明理由；\
         若不是，那就是真缺陷：\n{}",
        unlisted_init_failures.len(),
        unlisted_init_failures.join("\n")
    );
    // ③ 反向锁：登记了却不再发生 ⇒ 表已过期，必须删条目，否则豁免只增不减。
    let mut missing: Vec<&str> = FIXTURE_CREDENTIAL_ARTIFACTS
        .iter()
        .filter(|e| !artifacts_seen.contains(e))
        .map(|(name, _)| *name)
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "FIXTURE_CREDENTIAL_ARTIFACTS 里这些条目**没有再触发**（夹具换了？内核放宽了？）：{missing:?}。\
         请删掉它们 —— 留着等于给未来的真缺陷预留一条免检通道"
    );
}

/// 本仓节点表单支持的 13 协议里，**除 custom 外**的 12 个（custom 是 raw-JSON 逃生舱，
/// 形状由用户给，不由 builder 决定）。凭据用形状合法的值，让失败尽量落在 decode 而非 initialize。
const SWEEP_PROTOCOLS: &[(&str, &str)] = &[
    ("vless", r#""uuid":"6ba7b810-9dad-11d1-80b4-00c04fd430c8""#),
    ("vmess", r#""uuid":"6ba7b810-9dad-11d1-80b4-00c04fd430c8""#),
    ("trojan", r#""password":"pw""#),
    ("shadowsocks", r#""method":"aes-256-gcm","password":"pw""#),
    ("hysteria2", r#""password":"pw""#),
    (
        "tuic",
        r#""uuid":"6ba7b810-9dad-11d1-80b4-00c04fd430c8","password":"pw""#,
    ),
    ("socks", r#""username":"u","password":"pw""#),
    ("http", r#""username":"u","password":"pw""#),
    ("anytls", r#""password":"pw""#),
    ("naive", r#""username":"u","password":"pw""#),
    ("snell", r#""password":"pw""#),
    ("ssh", r#""username":"u","password":"pw""#),
];

/// `O_NET` 里除 `tcp` 外的全部传输档（`ui/src/components/dialogs/node-spec.ts`）。
const SWEEP_NETWORKS: &[&str] = &["ws", "grpc", "httpupgrade", "http"];

/// 🔴 **12 协议 × 4 种非 tcp 传输的全交叉，decode 阶段必须干净。**
///
/// # 为什么金样门不够，必须再来一遍
///
/// 本门第一版只喂金样的 37 个场景，而 **2026-08-07 真实出货的那个缺陷（传输层用黑名单）
/// 在那 37 个场景里一次都没被触发** —— 夹具里没有「非白名单协议 + 非 tcp 传输」这个组合。
/// 把黑名单改回去，金样那条腿照样全绿。
///
/// 那就是一条没有信息量的绿：门看起来在守，实际守不到缺陷所在的输入空间。夹具是按
/// **典型用法**导出的，而这类缺陷恰恰长在**组合边角**上。所以覆盖面必须由**判据**决定
/// （「哪些组合下发到内核会炸」），不能由「手上正好有什么夹具」决定。
///
/// # 这些组合真的会发生
///
/// UI 只给 vless/vmess/trojan 暴露传输选择器，但**导入侧不受这个限制**：xray 的 `streamSettings`
/// 可挂在任意出站上、clash 的 `network:` 同理，`net-stack` 那几个 parser 会照单写进 `server.network`。
///
/// # 只判 decode
///
/// 本 sweep 问的是「字段摆在哪个容器里」，那是 decode 的事。initialize 阶段验凭据内容
/// （ssh 没私钥、hysteria2 的 obfs 之类），与本 sweep 的判据无关，故只收集不判 —— 真要管
/// 凭据内容，那是导入/校验层的射程。
#[test]
fn every_protocol_times_every_transport_decodes_cleanly() {
    let Some(core) = core_or_skip("协议 × 传输交叉门") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("polaris-kgate-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let mut decode_failures: Vec<String> = Vec::new();
    let mut combos = 0usize;
    for (proto, cred) in SWEEP_PROTOCOLS {
        for net in SWEEP_NETWORKS {
            let raw = format!(
                r#"{{
                    "servers": [{{ "id":"n1","name":"N","protocol":"{proto}",
                        "address":"a.example.com","port":443,{cred},
                        "network":"{net}",
                        "wsSettings":{{"path":"/w","headers":{{"Host":"a.example.com"}}}},
                        "grpcSettings":{{"serviceName":"gs"}},
                        "httpSettings":{{"path":"/h"}} }}],
                    "selectedServerId":"n1","proxyMode":"global",
                    "proxyModeType":"manual","mixedPort":17899
                }}"#
            );
            let user_config: UserConfig = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("{proto}/{net} 夹具无效: {e}"));
            let deps = deps_for("linux");
            let Ok(cfg) = generate_sing_box_config(&user_config, &BTreeMap::new(), &deps) else {
                continue;
            };
            let surface = outbound_surface(&serde_json::to_value(&cfg).expect("序列化"));
            let p = dir.join(format!("sweep-{proto}-{net}.json"));
            std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
            combos += 1;
            let (ok, diag) = check(&core, &p);
            if !ok && is_decode_stage(&diag) {
                decode_failures.push(format!("  · {proto} × {net} → {diag}"));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        combos,
        SWEEP_PROTOCOLS.len() * SWEEP_NETWORKS.len(),
        "有组合在生成阶段就没走到 check —— 本门的交叉面缩水了，先查生成为什么失败"
    );
    assert!(
        decode_failures.is_empty(),
        "{} / {combos} 个「协议 × 传输」组合产出了内核 decode 不了的配置。\
         这类配置一旦下发，**整个核起不来**（不止这个节点）：\n{}",
        decode_failures.len(),
        decode_failures.join("\n")
    );
}

/// 🔴 **正向对照：这道门确实会红。**
///
/// 没有这一条，上面那条全绿既可能是「配置真的都合法」，也可能是「`check` 根本没在判、
/// 或者我把 rc 读反了」。这里喂一份**已知内核必拒**的出站（就是 2026-08-07 那次真实事故的形状：
/// `type:"http"` 挂 `transport`），断言它一定被拒、且诊断里点到了 `transport`。
///
/// 顺带钉住一件事：内核给的诊断**自带下标**（`outbounds[0]`），起核闸门 `classify_peel_target`
/// 正是靠它把「第几项」翻回「哪个节点」的。哪天内核不再给下标，这里会先红。
#[test]
fn the_gate_can_actually_fail() {
    let Some(core) = core_or_skip("出站面加载门的正向对照") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("polaris-kgate-neg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let p = dir.join("bad.json");
    std::fs::write(
        &p,
        serde_json::to_vec_pretty(&json!({
            "log": { "disabled": true },
            "outbounds": [{
                "type": "http",
                "tag": "bad",
                "server": "a.example.com",
                "server_port": 8080,
                // 内核 http 出站 schema 无 `transport` 且 additionalProperties:false。
                "transport": { "type": "http", "path": "/x" }
            }]
        }))
        .unwrap(),
    )
    .expect("写盘");
    let (ok, diag) = check(&core, &p);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !ok,
        "内核居然收下了 `type:http` + `transport` —— 上面那条全绿因此没有信息量，\
         先确认核版本与 schema 是不是变了"
    );
    assert!(
        diag.contains("transport"),
        "诊断里没提 transport，本对照可能撞上了别的失败原因；实得：{diag}"
    );
    assert!(
        diag.contains("outbounds[0]"),
        "内核诊断不再自带下标 —— 起核闸门的 `classify_peel_target` 靠它归因，那边会跟着失效；实得：{diag}"
    );
}

/// 🔴 **CI 接线自曝：打包腿必须带着硬化开关跑本门。**
///
/// 本门在缺核时跳过（开发机常态），故它的真正牙在 `package.yml` ——
/// 那条腿在 `fetch-core.mjs` 之后跑，核必然在盘上。若有人删掉那一步，本门就退化成
/// 「开发机上偶尔跑跑」，而**不会有任何东西报警**。这条断言就是那个警报。
///
/// 判据刻意宽（只认关键字共现，不认行号/缩进/步骤名），避免变成格式洁癖门。
#[test]
fn ci_step_still_wired() {
    let wf = repo_root().join(".github/workflows/package.yml");
    let raw =
        std::fs::read_to_string(&wf).unwrap_or_else(|e| panic!("读不到 {}: {e}", wf.display()));
    assert!(
        raw.contains("POLARIS_REQUIRE_KERNEL_GATE"),
        "package.yml 里找不到 POLARIS_REQUIRE_KERNEL_GATE —— \
         内核加载门在打包腿上没有强制执行，缺核会静静跳过"
    );
    assert!(
        // 匹配 run 命令本身而非裸词：本文件名一旦出现在 package.yml 的注释里（相邻的
        // core_dep_fingerprint 那步就踩过这个坑），裸词判据会在 step 被删后仍然绿。
        raw.contains("--test kernel_accepts_outbounds"),
        "package.yml 里找不到 `--test kernel_accepts_outbounds` —— 打包腿没在跑本门"
    );
}

/// 🔴 本门必须排在「Fetch cronet libs」**之后** —— 这个先后关系是承重的。
///
/// # 血证（2026-08-13）
///
/// 此前本门排在 cronet 拉取之前，Windows 腿红在：
/// `naive outbound → initialize outbound[0]: cronet: library not found.
///  Place libcronet.dll in the executable directory or PATH`
///
/// 随包核 **beta.14 在 initialize 阶段就硬要这个库**，而它由 `fetch-cronet.mjs` 落到
/// `resources/win/`（= 核的同目录）。2026-08-05 那次 Windows 腿是绿的，因为旧核不要它 ——
/// 抬核到 beta.14 之后才成立，而 **Windows 腿从 08-05 起整整一周没再跑过**，缺陷就潜伏在那里。
///
/// 排在之后同时更贴近生产：生产就是把 DLL 放在核旁边。故这不是「顺手挪一下」，
/// 挪回去会让 Windows 打包腿必红，且症状（一句 cronet 找不到）跟本门要守的东西毫无关系。
#[test]
fn ci_step_order_is_load_bearing() {
    let wf = repo_root().join(".github/workflows/package.yml");
    let raw =
        std::fs::read_to_string(&wf).unwrap_or_else(|e| panic!("读不到 {}: {e}", wf.display()));
    let cronet = raw
        .find("node scripts/fetch-cronet.mjs")
        .expect("package.yml 里找不到 fetch-cronet 步骤");
    let gate = raw
        .find("--test kernel_accepts_outbounds")
        .expect("package.yml 里找不到本门的 run 命令");
    assert!(
        cronet < gate,
        "内核门排到了 cronet 拉取**之前** —— Windows 腿会红在 `cronet: library not found`，\
         那是环境缺件，与本门要守的「生成的配置内核收不收」毫无关系"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// hysteria v1 / tor —— 2026-08-11 新进建模协议，两条腿都有「只有真核能判」的陷阱
// ─────────────────────────────────────────────────────────────────────────────

/// 用最小 `UserConfig` 造一个单节点配置，喂随包真核。
///
/// 不并进上面的 `SWEEP_PROTOCOLS`（那张表是 协议 × 非 tcp 传输 的全交叉）：
/// hysteria v1 走 QUIC、tor 自带传输层，两者都不接 ws/grpc/httpupgrade/http，
/// 硬塞进去只会得到一批语义上不成立的组合，红了也不知道该改哪。
#[test]
fn bundled_core_accepts_hysteria_v1_and_tor() {
    use polaris_config_engine::user_config::protocol_settings::{HysteriaSettings, TorSettings};
    use polaris_config_engine::user_config::server_config::Protocol;

    let Some(core) = core_or_skip("hysteria v1 / tor 出站门") else {
        return;
    };

    let hy = ServerConfig {
        id: "hy1".into(),
        name: "hy1".into(),
        protocol: Protocol::Hysteria,
        address: "h.example.com".into(),
        port: 443,
        hysteria_settings: Some(HysteriaSettings {
            auth_str: Some("secret".into()),
            up_mbps: Some(10),
            down_mbps: Some(50),
            obfs: Some("obfs-pw".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let tor = ServerConfig {
        id: "tor1".into(),
        name: "tor1".into(),
        protocol: Protocol::Tor,
        // 地址栏故意填上：Tor 是**无地址协议**，生成侧必须把它清掉。
        // 这条不是假设 —— 实测给随包核传 `server` 得
        // `outbounds[0].server: json: unknown field "server"`，**整个核起不来**。
        address: "should-be-dropped.example.com".into(),
        port: 9050,
        tor_settings: Some(TorSettings {
            executable_path: Some("/usr/bin/tor".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let input = UserConfig {
        servers: vec![hy, tor],
        selected_server_id: Some("hy1".into()),
        ..Default::default()
    };

    let deps = deps_for(&default_platform());
    let cfg = generate_sing_box_config(&input, &BTreeMap::new(), &deps).expect("生成配置");
    let value = serde_json::to_value(&cfg).expect("序列化");

    // ① 结构断言：tor 出站不得带 server/server_port。
    // 放在喂核之前 —— 核只会说「unknown field」，说不出是哪个节点漏清的。
    let outs = value["outbounds"].as_array().expect("outbounds 数组");
    let tor_ob = outs
        .iter()
        .find(|o| o["type"] == "tor")
        .expect("没生成 tor 出站");
    assert!(
        tor_ob.get("server").is_none() && tor_ob.get("server_port").is_none(),
        "tor 出站带上了 server/server_port —— 随包核会在 decode 阶段拒收整份配置：{tor_ob}"
    );
    let hy_ob = outs
        .iter()
        .find(|o| o["type"] == "hysteria")
        .expect("没生成 hysteria 出站");
    // ② hysteria v1 的 obfs 是**裸字符串**，不是 v2 的 {type,password} 对象。
    assert_eq!(hy_ob["obfs"], serde_json::json!("obfs-pw"), "obfs 形状错了");
    assert_eq!(
        hy_ob["auth_str"],
        serde_json::json!("secret"),
        "auth_str 丢了"
    );

    // ③ 真核判决：decode + initialize 全过。
    let dir = std::env::temp_dir().join(format!("polaris-hytor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let p = dir.join("cfg.json");
    let surface = outbound_surface(&value);
    std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
    let (ok, diag) = check(&core, &p);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ok, "随包核拒绝了 hysteria v1 / tor 的出站面：{diag}");
}

/// 拨号前解析（route action `resolve`，默认关）—— 位置约束是这条规则的**全部价值**。
///
/// 故本门断言的是**相对顺序**，不是「存在一条 resolve」：
///  · 排在自定义规则之前 —— 排其后则 smart 模式下自定义规则命中即 `break match`，永远走不到；
///  · 排在探测/更新入站钉死路由与网银直连之后 —— 那几类是终止规则，且目的地绝不能先被解析成 IP。
/// 只断言「存在」的门，对「插错位置」这个唯一会出错的地方完全失明。
#[test]
fn resolve_before_dial_is_off_by_default_and_lands_between_the_two_walls() {
    use polaris_config_engine::user_config::server_config::Protocol;

    let server = ServerConfig {
        id: "s1".into(),
        name: "s1".into(),
        protocol: Protocol::Trojan,
        address: "t.example.com".into(),
        port: 443,
        password: Some("pw".into()),
        ..Default::default()
    };
    let base = UserConfig {
        servers: vec![server],
        selected_server_id: Some("s1".into()),
        proxy_mode: polaris_config_engine::user_config::proxy_mode::ProxyMode::Smart,
        ..Default::default()
    };
    let deps = deps_for(&default_platform());

    let rules_of = |cfg: &UserConfig| -> Vec<Value> {
        let out = generate_sing_box_config(cfg, &BTreeMap::new(), &deps).expect("生成配置");
        serde_json::to_value(&out).expect("序列化")["route"]["rules"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    // ① 默认关：一条都不发。
    let off = rules_of(&base);
    assert!(
        !off.iter().any(|r| r["action"] == "resolve"),
        "resolve_before_dial 未开启却发了 resolve 规则 —— 默认必须是关"
    );

    // ② 开启后存在，且位置夹在两堵墙之间。
    let mut on_cfg = base.clone();
    on_cfg.resolve_before_dial = Some(true);
    let on = rules_of(&on_cfg);
    let idx = |pred: &dyn Fn(&Value) -> bool| on.iter().position(pred);

    let resolve_at = idx(&|r| r["action"] == "resolve").expect("开启后没发 resolve 规则");
    // 上墙：DNS 劫持（`hijack-dns`）—— 本夹具里恒在，且属「目的地绝不能先被解析成 IP」那一类。
    let hijack = on
        .iter()
        .rposition(|r| r["action"] == "hijack-dns")
        .expect("没有 hijack-dns 规则 —— 夹具缩水，本门失去上墙");
    assert!(
        resolve_at > hijack,
        "resolve 排到了 DNS 劫持之前（{resolve_at} ≤ {hijack}）"
    );
    // 探测池 / update-in 那两堵墙**本夹具不产生**（最小 UserConfig 无探测池、无订阅更新入站），
    // 故只在它们真出现时才断言 —— 不假装覆盖，也不因为没覆盖就放行。
    if let Some(last_inbound) = on.iter().rposition(|r| r.get("inbound").is_some()) {
        assert!(
            resolve_at > last_inbound,
            "resolve 排到了入站钉死路由之前（{resolve_at} ≤ {last_inbound}）—— \
             探针/更新流量的目的地会先被解析成 IP，按域名钉出口的那几条就失效了"
        );
    }
    // 下墙：自定义规则腿（rule_set / 外化文件），resolve 必须在它之前。
    if let Some(first_custom) = idx(&|r| r.get("rule_set").is_some()) {
        assert!(
            resolve_at < first_custom,
            "resolve 排到了自定义规则之后（{resolve_at} ≥ {first_custom}）—— \
             smart 模式下自定义规则命中即 break match，本条永远走不到"
        );
    }

    // ③ 真核判决：开启态的整份配置必须被随包核收下。
    if let Some(core) = core_or_skip("resolve 规则门") {
        let cfg = generate_sing_box_config(&on_cfg, &BTreeMap::new(), &deps).expect("生成配置");
        let dir = std::env::temp_dir().join(format!("polaris-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join("cfg.json");
        let surface = outbound_surface(&serde_json::to_value(&cfg).expect("序列化"));
        std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
        let (ok, diag) = check(&core, &p);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "随包核拒绝了带 resolve 规则的配置：{diag}");
    }
}

/// 拨号前解析的**两条排除**（2026-08-11 复审驱动）。
///
/// 判据不是「开关开了就该有 resolve」——那正是复审抓到的形状。两条状态下必须**不注入**：
///  · `exit_fallback`：选中「关外网的组网节点」时 route 侧已全直连，而 `dns-remote` 的 detour
///    仍指着 `proxy-selector`（`generate.rs` 的 `selected_server_tag` 是字面量，不跟随回退）。
///    该状态下远程解析本就打进黑洞；resolve 会把它放大成每条连接 fatalErr 断连。
///  · `direct` 模式：出口恒直连，注入只换解析器并丢掉 direct 出站的 happy-eyeballs。
#[test]
fn resolve_before_dial_is_suppressed_in_the_two_states_where_it_backfires() {
    use polaris_config_engine::user_config::proxy_mode::ProxyMode;
    use polaris_config_engine::user_config::server_config::Protocol;
    use polaris_config_engine::user_config::server_config::WireGuardSettings;

    let deps = deps_for(&default_platform());
    let has_resolve = |cfg: &UserConfig| -> bool {
        let out = generate_sing_box_config(cfg, &BTreeMap::new(), &deps).expect("生成配置");
        serde_json::to_value(&out).expect("序列化")["route"]["rules"]
            .as_array()
            .map(|rs| rs.iter().any(|r| r["action"] == "resolve"))
            .unwrap_or(false)
    };

    let trojan = ServerConfig {
        id: "s1".into(),
        name: "s1".into(),
        protocol: Protocol::Trojan,
        address: "t.example.com".into(),
        port: 443,
        password: Some("pw".into()),
        ..Default::default()
    };

    // 基线：smart + 普通节点 + 开关开 → 必须注入（否则下面两条「不注入」没有信息量）。
    let base = UserConfig {
        servers: vec![trojan.clone()],
        selected_server_id: Some("s1".into()),
        proxy_mode: ProxyMode::Smart,
        resolve_before_dial: Some(true),
        ..Default::default()
    };
    assert!(
        has_resolve(&base),
        "基线就没注入 —— 后两条断言会变成恒真的假绿"
    );

    // ① direct 模式 → 不注入。
    let mut direct_mode = base.clone();
    direct_mode.proxy_mode = ProxyMode::Direct;
    assert!(
        !has_resolve(&direct_mode),
        "direct 模式仍注入了 resolve —— 出口恒直连，只换掉解析器并丢掉 happy-eyeballs"
    );

    // ② 组网出口回退 → 不注入。选中一个**关外网**的 WireGuard 组网节点。
    let mesh = ServerConfig {
        id: "wg1".into(),
        name: "wg1".into(),
        protocol: Protocol::Wireguard,
        address: "wg.example.com".into(),
        port: 51820,
        wireguard_settings: Some(WireGuardSettings {
            private_key: Some("aGVsbG8gd29ybGQgaGVsbG8gd29ybGQgaGVsbG8gd29yaGE=".into()),
            local_address: vec!["10.0.0.2/32".into()],
            peer_public_key: Some("aGVsbG8gd29ybGQgaGVsbG8gd29ybGQgaGVsbG8gd29yaGE=".into()),
            allowed_ips: vec!["10.0.0.0/24".into()],
            // 关外网 = 该节点不承载全隧道 ⇒ 用户出口整体回退 direct。
            allow_internet: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mesh_cfg = UserConfig {
        servers: vec![mesh],
        selected_server_id: Some("wg1".into()),
        proxy_mode: ProxyMode::Smart,
        resolve_before_dial: Some(true),
        ..Default::default()
    };
    assert!(
        !has_resolve(&mesh_cfg),
        "组网出口回退直连时仍注入了 resolve —— dns-remote 的 detour 仍指 proxy-selector，\
         远程解析打进黑洞，每条连接会在 route 阶段 fatalErr 断连"
    );
}

/// DNS 侧的 detour 必须跟随 route 侧**同一条**出口回退。
///
/// 缺陷原型（2026-08-11 实测取证，非推断）：选中「关外网的组网节点」时
///   route.final = "direct"（route 侧已回退）
///   proxy-selector = { default: "wg1", outbounds: ["wg1","direct"] }
///   dns-remote     = { server: "dns.google", detour: "proxy-selector" }
/// 而 wg1 的 allowed_ips 是 10.0.0.0/24 —— dns.google 不在该段 ⇒ cryptokey routing 丢包 ⇒
/// **该状态下每一次远程 DNS 解析必然超时**。这与 resolve 开关无关，是独立存在的活缺陷。
///
/// 门断言的是「两侧回退同步」这条不变式，不是「detour 等于某个字面量」：
/// 正常态必须仍指 selector（否则远程解析会变成直连出网，是另一个方向的错）。
#[test]
fn dns_remote_detour_follows_the_same_exit_fallback_as_route() {
    use polaris_config_engine::user_config::proxy_mode::ProxyMode;
    use polaris_config_engine::user_config::server_config::{Protocol, WireGuardSettings};

    let deps = deps_for(&default_platform());
    let dns_remote_detour = |cfg: &UserConfig| -> String {
        let out = generate_sing_box_config(cfg, &BTreeMap::new(), &deps).expect("生成配置");
        let v = serde_json::to_value(&out).expect("序列化");
        v["dns"]["servers"]
            .as_array()
            .expect("dns.servers")
            .iter()
            .find(|s| s["tag"] == "dns-remote")
            .expect("没有 dns-remote —— 夹具缩水，本门失去射程")["detour"]
            .as_str()
            .unwrap_or("<none>")
            .to_string()
    };
    let route_final = |cfg: &UserConfig| -> String {
        let out = generate_sing_box_config(cfg, &BTreeMap::new(), &deps).expect("生成配置");
        serde_json::to_value(&out).expect("序列化")["route"]["final"]
            .as_str()
            .unwrap_or("<none>")
            .to_string()
    };

    let mesh = |allow_internet: bool| ServerConfig {
        id: "wg1".into(),
        name: "wg1".into(),
        protocol: Protocol::Wireguard,
        address: "wg.example.com".into(),
        port: 51820,
        wireguard_settings: Some(WireGuardSettings {
            private_key: Some("aGVsbG8gd29ybGQgaGVsbG8gd29ybGQgaGVsbG8gd29yaGE=".into()),
            local_address: vec!["10.0.0.2/32".into()],
            peer_public_key: Some("aGVsbG8gd29ybGQgaGVsbG8gd29ybGQgaGVsbG8gd29yaGE=".into()),
            allowed_ips: vec!["10.0.0.0/24".into()],
            allow_internet: Some(allow_internet),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cfg_of = |s: ServerConfig| UserConfig {
        servers: vec![s],
        selected_server_id: Some("wg1".into()),
        proxy_mode: ProxyMode::Smart,
        ..Default::default()
    };

    // ① 回退态：route 与 DNS **同时**回退。
    let fb = cfg_of(mesh(false));
    assert_eq!(
        route_final(&fb),
        "direct",
        "夹具没触发出口回退 —— 本门失去射程"
    );
    assert_eq!(
        dns_remote_detour(&fb),
        "direct",
        "route 侧已回退 direct，dns-remote 的 detour 仍指 selector —— \
         DoH 查询会被送进关外网的组网节点，按 allowed_ips 丢包，每次远程解析必然超时"
    );

    // ② 正常态：必须**仍指 selector**。少了这条，把 detour 写死成 direct 也会绿 ——
    //    那是另一个方向的错（远程解析直连出网）。
    let ok = cfg_of(mesh(true));
    assert_ne!(
        route_final(&ok),
        "direct",
        "夹具二意外也触发了回退，对照失效"
    );
    assert_eq!(
        dns_remote_detour(&ok),
        "proxy-selector",
        "正常态的 dns-remote 不再经代理 —— 远程解析变成直连出网"
    );
}

/// 端点族 VPN 客户端（openconnect / openvpn-client）—— 三个只有真核能判的点。
///
/// ① 必须落在 `endpoints[]`：塞进 `outbounds[]` 内核判 `unknown outbound type`（实测），
///    是 decode 阶段硬失败、整个核起不来；
/// ② openvpn-client 的 `tls` **必填**：缺了判 `initialize endpoint[0]: missing \`tls\` options`；
/// ③ 载荷来自设置结构的序列化 —— 结构字段名一旦与内核键名漂移，本门就红。
///
/// 另钉两条**判据分离**（2026-08-13）：
///  - 二者**必须**进 `lands_in_endpoints` —— 那是数据模型形态，和 ① 同一件事，漏了就会有消费点
///    把它们当 outbound 处理（临时测速核就这么塞过，整核 FATAL）；
///  - 二者**不得**进 `is_mesh_protocol` —— 那是「配置期能否声明可达网段」，它们的段由服务端
///    运行期 push。**注意这是协议级判据**：节点级的组网资格看 `is_mesh_node`（用户在 `meshRoutes`
///    里显式声明了段就具备），本断言不覆盖那一支，也不该覆盖。
#[test]
fn endpoint_family_vpn_clients_land_in_endpoints_and_the_core_accepts_them() {
    use polaris_config_engine::user_config::protocol_settings::{
        OpenconnectSettings, OpenvpnClientSettings, OpenvpnTlsSettings,
    };
    use polaris_config_engine::user_config::server_config::{
        is_mesh_protocol, lands_in_endpoints, Protocol, ServerConfig,
    };

    // 正向锁：二者是 endpoint 腿（数据模型形态）。
    assert!(
        lands_in_endpoints(Protocol::Openconnect) && lands_in_endpoints(Protocol::OpenvpnClient),
        "openconnect / openvpn-client 掉出了 lands_in_endpoints —— \
         消费点会把它们当 outbound 处理，临时测速核塞进 outbounds[] 即整核 FATAL"
    );
    // 反向锁：协议级不具备组网资格（网段配置期不可知）。节点级见 `is_mesh_node`。
    assert!(
        !is_mesh_protocol(Protocol::Openconnect) && !is_mesh_protocol(Protocol::OpenvpnClient),
        "openconnect / openvpn-client 被并进了 is_mesh_protocol —— \
         那条判据是「配置期能否声明可达网段」，它们的段由服务端运行期 push"
    );

    let oc = ServerConfig {
        id: "oc1".into(),
        name: "oc1".into(),
        protocol: Protocol::Openconnect,
        address: "vpn.example.com".into(),
        port: 443,
        openconnect_settings: Some(OpenconnectSettings {
            server: Some("vpn.example.com:443".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            flavor: Some("anyconnect".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ov = ServerConfig {
        id: "ov1".into(),
        name: "ov1".into(),
        protocol: Protocol::OpenvpnClient,
        address: "ovpn.example.com".into(),
        port: 1194,
        openvpn_client_settings: Some(OpenvpnClientSettings {
            server: Some("1.2.3.4".into()),
            server_port: Some(1194),
            username: Some("u".into()),
            password: Some("p".into()),
            tls: Some(OpenvpnTlsSettings {
                certificate: vec!["-----BEGIN CERTIFICATE-----".into()],
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let input = UserConfig {
        servers: vec![oc, ov],
        selected_server_id: Some("oc1".into()),
        ..Default::default()
    };
    let deps = deps_for(&default_platform());
    let cfg = generate_sing_box_config(&input, &BTreeMap::new(), &deps).expect("生成配置");
    let v = serde_json::to_value(&cfg).expect("序列化");

    // ① 落在 endpoints[] 而不是 outbounds[]。
    let eps = v["endpoints"].as_array().cloned().unwrap_or_default();
    let outs = v["outbounds"].as_array().cloned().unwrap_or_default();
    for want in ["openconnect", "openvpn-client"] {
        assert!(
            eps.iter().any(|e| e["type"] == want),
            "{want} 没进 endpoints[]：{eps:?}"
        );
        assert!(
            !outs.iter().any(|o| o["type"] == want),
            "{want} 跑进了 outbounds[] —— 内核判 unknown outbound type，整个核起不来"
        );
    }

    // ③ 载荷键名与内核对齐（结构字段名漂移即红）。
    let oc_ep = eps.iter().find(|e| e["type"] == "openconnect").unwrap();
    assert_eq!(
        oc_ep["flavor"],
        serde_json::json!("anyconnect"),
        "flavor 没下发"
    );
    assert_eq!(oc_ep["server"], serde_json::json!("vpn.example.com:443"));
    let ov_ep = eps.iter().find(|e| e["type"] == "openvpn-client").unwrap();
    assert!(
        ov_ep["tls"]["certificate"].is_array(),
        "② openvpn 的 tls 材料没下发"
    );

    // ② 真核判决。
    if let Some(core) = core_or_skip("端点族 VPN 客户端门") {
        let dir = std::env::temp_dir().join(format!("polaris-epvpn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join("cfg.json");
        std::fs::write(
            &p,
            serde_json::to_vec_pretty(&outbound_surface(&v)).unwrap(),
        )
        .expect("写盘");
        let (ok, diag) = check(&core, &p);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            ok,
            "随包核拒绝了 openconnect / openvpn-client 的端点面：{diag}"
        );
    }
}

/// 透传袋：**未建模的键必须活着穿过「设置结构 → 下发配置」**。
///
/// 缺陷原型：表单是精选子集（内核 hysteria v1 有 20 个非通用键，本仓建模 7 个）。
/// 没有透传袋时，「导入 → 编辑 → 保存」会把其余键**静默丢掉** ——
/// 配置从能连变成连不上，且没有任何提示。这类丢失不会让任何既有测试变红：
/// 生成得出来、内核也收得下，只是少了几个键。
///
/// 同时钉住**优先级**：具名字段是表单的真值，同名时必须压过袋里的旧值，
/// 否则用户在表单里改过的项会被导入时留下的原值盖回去。
#[test]
fn unmodeled_keys_survive_the_passthrough_bag() {
    use polaris_config_engine::user_config::protocol_settings::HysteriaSettings;
    use polaris_config_engine::user_config::server_config::Protocol;

    let mut extra = serde_json::Map::new();
    // 真实的 hysteria v1 调优键，本仓刻意不建模（进袋子而非表单）。
    extra.insert("recv_window".into(), serde_json::json!(8_388_608u32));
    extra.insert("disable_path_mtu_discovery".into(), serde_json::json!(true));
    // 同名冲突：袋里带着导入时的旧值，具名字段是用户改过的新值。
    extra.insert("up_mbps".into(), serde_json::json!(1));

    let server = ServerConfig {
        id: "hy".into(),
        name: "hy".into(),
        protocol: Protocol::Hysteria,
        address: "h.example.com".into(),
        port: 443,
        hysteria_settings: Some(HysteriaSettings {
            auth_str: Some("a".into()),
            up_mbps: Some(100),
            down_mbps: Some(500),
            extra,
            ..Default::default()
        }),
        ..Default::default()
    };
    let input = UserConfig {
        servers: vec![server],
        selected_server_id: Some("hy".into()),
        ..Default::default()
    };
    let deps = deps_for(&default_platform());
    let cfg = generate_sing_box_config(&input, &BTreeMap::new(), &deps).expect("生成配置");
    let v = serde_json::to_value(&cfg).expect("序列化");
    let ob = v["outbounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["type"] == "hysteria")
        .expect("没生成 hysteria 出站");

    assert_eq!(
        ob["recv_window"],
        serde_json::json!(8_388_608u32),
        "未建模键被丢掉了 —— 「导入 → 编辑 → 保存」会让配置从能连变成连不上，且无任何提示"
    );
    assert_eq!(ob["disable_path_mtu_discovery"], serde_json::json!(true));
    assert_eq!(
        ob["up_mbps"],
        serde_json::json!(100),
        "袋里的旧值压过了具名字段 —— 用户在表单里改的项会被导入时的原值盖回去"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 2026-08-12：协议 × 真核覆盖面的**反方向**对差
//
// 上一轮补协议时对的是正方向（「随包核支持而本仓没表单的」）。反方向没对过：
// **本仓声明支持、但其产物从没被真核判过的**。逐张表对完只有一个 —— `tailscale`：
// 它不在「协议 × 传输」交叉门（正确，它不吃 transport），不在金样夹具，也没有专属门。
// 下面第一条补上这条腿，第二条把「谁认领谁」变成加变体就编译不过的棘轮。
// ─────────────────────────────────────────────────────────────────────────────

/// 随包核必须收得下 tailscale endpoint 的**全字段面**。
///
/// 失效形态与 openconnect/openvpn 那条同级：endpoint 的键名写错是 **decode 阶段**硬失败，
/// 症状不是「这个组网节点连不上」而是**整个核起不来**，全部节点跟着下线。
///
/// 三个已实测的判据（都由本门的变异确认过）：
/// ① 键名错（`state_directory` 写成 `state_dir`）→ `endpoints[0].state_dir: json: unknown field`；
/// ② 类型错（`accept_routes` 给字符串）→ `cannot unmarshal`；
/// ③ 全字段齐备（auth_key / control_url / hostname / ephemeral / accept_routes /
///    advertise_routes / exit_node / exit_node_allow_lan_access）→ rc=0。
///
/// `check` 对 tailscale 是**纯解码**：实测 0.1 秒返回，不建 state 目录、不联控制面
/// （故 deps 里那个 `/fake/userData/tailscale` 前缀无害）。
#[test]
fn bundled_core_accepts_tailscale_endpoint() {
    use polaris_config_engine::user_config::server_config::{Protocol, TailscaleSettings};

    let ts = ServerConfig {
        id: "ts1".into(),
        name: "ts1".into(),
        protocol: Protocol::Tailscale,
        tailscale_settings: Some(TailscaleSettings {
            auth_key: Some("tskey-auth-example".into()),
            control_url: Some("https://controlplane.example.com".into()),
            hostname: Some("polaris-node".into()),
            ephemeral: Some(true),
            accept_routes: Some(true),
            advertise_routes: vec!["10.7.0.0/24".into()],
            // exit_node 只在承流时下发 ⇒ allow_internet 必须开，否则这两个键根本不进产物，
            // 本门就变成「没判到它们」的假绿。
            allow_internet: Some(true),
            exit_node: Some("peer-exit".into()),
            exit_node_allow_lan_access: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    let input = UserConfig {
        servers: vec![ts],
        selected_server_id: Some("ts1".into()),
        ..Default::default()
    };
    let deps = deps_for(&default_platform());
    let cfg = generate_sing_box_config(&input, &BTreeMap::new(), &deps).expect("生成配置");
    let v = serde_json::to_value(&cfg).expect("序列化");

    let eps = v["endpoints"].as_array().cloned().unwrap_or_default();
    let ep = eps
        .iter()
        .find(|e| e["type"] == "tailscale")
        .expect("没生成 tailscale endpoint");

    // 先钉产物：这些键必须真的在里面，否则下面的 rc=0 只是「核收下了一个空壳」。
    for k in [
        "state_directory",
        "auth_key",
        "control_url",
        "hostname",
        "ephemeral",
        "accept_routes",
        "advertise_routes",
        "exit_node",
        "exit_node_allow_lan_access",
    ] {
        assert!(
            !ep[k].is_null(),
            "tailscale endpoint 缺 `{k}` —— 本门若只判 rc=0，缺字段照样绿"
        );
    }

    // ── 与 openconnect/openvpn 那条腿的关键差异：tailscale 不带 per-endpoint 解析器 ──
    //
    // 它的 `control_url` 是域名，而 `build_tailscale_endpoint` 写死 `domain_resolver: None`
    // ⇒ 解析全靠 `route.default_domain_resolver`。实测：surface 里不带 route 那一段，
    // 内核就判 `initialize endpoint[0]: missing domain resolver for domain server address`
    // —— **initialize 阶段硬失败，整个核起不来**。
    //
    // 这不是缺陷（生产配置恒有 `route.default_domain_resolver = dns-bootstrap`），但它是一条
    // 没写下来的**跨段耦合**：哪天 route 那处不再下发，症状是「装了 tailscale 节点就起不了核」，
    // 而所有只判出站面的门都还是绿的。故这里把它当判据显式钉住，再连同 route 段一起送真核。
    let resolver = v["route"]["default_domain_resolver"]
        .as_str()
        .expect(
            "route.default_domain_resolver 没了 —— tailscale 的域名 control_url 将无解析器可用，\
             内核在 initialize 阶段直接失败，整个核起不来",
        )
        .to_string();

    if let Some(core) = core_or_skip("tailscale endpoint 门") {
        let mut surface = outbound_surface(&v);
        surface["route"] = json!({ "default_domain_resolver": resolver });
        let dir = std::env::temp_dir().join(format!("polaris-ts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join("cfg.json");
        std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
        let (ok, diag) = check(&core, &p);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "随包核拒绝了 tailscale endpoint：{diag}");
    }
}

/// 每个协议的产物**都必须被某条真核门判过** —— 谁认领，写下来。
///
/// # 为什么要有这条
///
/// 「协议 × 传输」交叉门的协议清单是**手写夹具**（12 条）。夹具驱动的门，覆盖面不会跟着
/// 代码长：2026-08-11 一口气加了四个协议，交叉门的数字一动不动，全绿。真正没人管的是
/// `tailscale` —— 它比那四个早得多，却从来没被真核判过一次。
///
/// 故这里不再写第二份清单，改成**穷尽 `match`**：加了新变体，本文件编译不过，
/// 作者必须当场表态「它归哪条门管」。四种认领各自都要**拿得出证据**，不能只是一句声明：
/// `Sweep` 要求它真在夹具里，`Golden` 要求金样语料里真有该协议的用例，
/// `Named` 要求那个测试函数真在本文件里，`Exempt` 要求写下理由。
///
/// 反方向也锁：夹具里躺着一个没人认领的条目 ⇒ 红（死夹具）。
#[test]
fn every_protocol_is_claimed_by_some_kernel_gate() {
    use polaris_config_engine::user_config::server_config::Protocol;

    enum Claim {
        /// 走「协议 × 传输」交叉门。
        Sweep,
        /// 金样语料里有该协议的用例，随金样门一起被真核判。
        Golden,
        /// 有专属真核门（写出函数名）。
        Named(&'static str),
        /// 不由本文件判 + 理由。
        Exempt(&'static str),
    }

    // 注：这份数组与 `store/tests/protocol_registries_agree.rs` 里那份同构。没有抽公共
    // test-support crate —— 为两个测试文件建一个 crate，代价大于问题本身；两边各自
    // 都有「与枚举源码对差」的完整性断言，漂了任一边都会红。
    const ALL: &[Protocol] = &[
        Protocol::Vless,
        Protocol::Trojan,
        Protocol::Hysteria2,
        Protocol::Shadowsocks,
        Protocol::Anytls,
        Protocol::Tuic,
        Protocol::Vmess,
        Protocol::Naive,
        Protocol::Snell,
        Protocol::Socks,
        Protocol::Http,
        Protocol::Ssh,
        Protocol::Wireguard,
        Protocol::Tailscale,
        Protocol::Hysteria,
        Protocol::Tor,
        Protocol::Openconnect,
        Protocol::OpenvpnClient,
        Protocol::Custom,
    ];

    fn claim(p: Protocol) -> Claim {
        use Protocol::*;
        match p {
            Vless | Trojan | Hysteria2 | Shadowsocks | Anytls | Tuic | Vmess | Naive | Snell
            | Socks | Http | Ssh => Claim::Sweep,
            Wireguard => Claim::Golden,
            Tailscale => Claim::Named("bundled_core_accepts_tailscale_endpoint"),
            Hysteria | Tor => Claim::Named("bundled_core_accepts_hysteria_v1_and_tor"),
            Openconnect | OpenvpnClient => Claim::Named(
                "endpoint_family_vpn_clients_land_in_endpoints_and_the_core_accepts_them",
            ),
            Custom => Claim::Exempt(
                "载荷是用户原样 JSON，本文件不替用户造夹具；同一份 JSON 由 C10「测试内核兼容性」\
                 按钮与 custom 腿共用的 `custom_outbound_type` 谓词判形状",
            ),
        }
    }

    // 变体集合与枚举源码对差（数组漏一个 ⇒ 那个协议本门完全失明）。
    let src = std::fs::read_to_string(
        repo_root().join("crates/config-engine/src/user_config/server_config.rs"),
    )
    .expect("读 server_config.rs");
    let start = src.find("pub enum Protocol {").expect("枚举改名了");
    let body = &src[start..];
    let end = body.find("\n}").expect("枚举没收口");
    let mut from_src: Vec<String> = body[..end]
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("#["))
        .filter_map(|l| l.strip_suffix(','))
        .filter(|l| {
            l.chars().next().is_some_and(char::is_uppercase)
                && l.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(str::to_string)
        .collect();
    let mut from_arr: Vec<String> = ALL.iter().map(|p| format!("{p:?}")).collect();
    from_src.sort();
    from_arr.sort();
    assert_eq!(
        from_arr, from_src,
        "本门的 ALL 数组与 `Protocol` 源码对不上"
    );

    let me = std::fs::read_to_string(file!()).or_else(|_| {
        std::fs::read_to_string(
            repo_root().join("crates/config-engine/tests/kernel_accepts_outbounds.rs"),
        )
    });
    let me = me.expect("读不到本测试文件自身");
    let golden_protocols: Vec<String> = {
        let raw = std::fs::read_to_string(
            repo_root().join("crates/config-engine/fixtures/config-snapshot.json"),
        )
        .expect("读金样夹具");
        let v: Value = serde_json::from_str(&raw).expect("金样夹具解析");
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if let Some(Value::String(p)) = m.get("protocol") {
                        out.push(p.clone());
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        let mut out = vec![];
        walk(&v, &mut out);
        out
    };

    let mut claimed_sweep: Vec<String> = vec![];
    for &p in ALL {
        let wire = match serde_json::to_value(p).expect("序列化") {
            Value::String(s) => s,
            other => panic!("{p:?} 序列化成非字符串 {other:?}"),
        };
        match claim(p) {
            Claim::Sweep => {
                assert!(
                    SWEEP_PROTOCOLS.iter().any(|(n, _)| *n == wire),
                    "{p:?} 声明走交叉门，但 `{wire}` 不在 SWEEP_PROTOCOLS 里 —— \
                     它的产物一次都没被真核判过"
                );
                claimed_sweep.push(wire);
            }
            Claim::Golden => assert!(
                golden_protocols.contains(&wire),
                "{p:?} 声明由金样门覆盖，但金样语料里没有 `{wire}` 的用例 —— \
                 语料被删过，这条认领已经落空"
            ),
            Claim::Named(f) => assert!(
                me.contains(&format!("fn {f}(")),
                "{p:?} 认领的专属门 `{f}` 在本文件里不存在（改名或删了）"
            ),
            Claim::Exempt(why) => assert!(!why.is_empty(), "{p:?} 的豁免没写理由"),
        }
    }

    for (n, _) in SWEEP_PROTOCOLS {
        assert!(
            claimed_sweep.iter().any(|c| c == n),
            "SWEEP_PROTOCOLS 里的 `{n}` 没有任何变体认领 —— 死夹具：\
             它要么是拼错的协议名，要么对应的变体已经被删了"
        );
    }
}

/// 出口选阻断产出的是一种**全新的配置形状**，真核必须收得下。
///
/// 2026-08-13 之前阻断是 `proxy-selector.default = "block"` + 一个 legacy `block` 出站；现在是
/// 「所有『→代理』的规则改写成 `action:"reject"` + 一条**无 matcher** 的兜底 + `final:"direct"`」。
/// 后者此前从未被真核判过 —— 金样里 0 个 case 选中 `__block__`，那条腿在金样上零覆盖。
///
/// 两个只有真核能判的点：① matcher-less 的 reject 规则是否被接受（实测 rc=0，但那是手搓最小配置，
/// 这里判的是**生成侧的完整产物**）；② 删掉 block 出站之后没有任何悬空引用（有的话是 decode 阶段
/// 硬失败，整个核起不来）。
#[test]
fn bundled_core_accepts_block_exit_shape() {
    use polaris_config_engine::user_config::proxy_mode::ProxyMode;
    use polaris_config_engine::user_config::server_config::Protocol;

    let mut input = UserConfig {
        servers: vec![ServerConfig {
            id: "s1".into(),
            name: "n1".into(),
            protocol: Protocol::Trojan,
            address: "a.example.com".into(),
            port: 443,
            password: Some("pw".into()),
            ..Default::default()
        }],
        // 出口选阻断哨兵。
        selected_server_id: Some("__block__".into()),
        ..Default::default()
    };
    // 用 global 而非 smart：smart 会引内置 geo `rule_set`，而本门的 deps 指向测试假路径 ⇒
    // 真核在 initialize 阶段报「.srs 打不开」，那是环境缺件，与本门要判的形状无关。
    // global 档同样走完整的「改写 + 兜底 + final」路径，被测形状不变。
    input.proxy_mode = ProxyMode::Global;

    let deps = deps_for(&default_platform());
    let cfg = generate_sing_box_config(&input, &BTreeMap::new(), &deps).expect("生成配置");
    let v = serde_json::to_value(&cfg).expect("序列化");

    // 先钉产物形态：legacy 出站不得复活。
    let outs = v["outbounds"].as_array().cloned().unwrap_or_default();
    assert!(
        !outs
            .iter()
            .any(|o| o["type"] == "block" || o["tag"] == "block"),
        "legacy block 出站又出现了：{outs:?}"
    );

    if let Some(core) = core_or_skip("阻断出口形状门") {
        let dir = std::env::temp_dir().join(format!("polaris-blockexit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join("cfg.json");
        // 这条腿要连 route 一起送：兜底 reject 与 final 都在 route 里，只送出站面判不到。
        let mut surface = outbound_surface(&v);
        // 剔掉 geo `rule_set` 与引用它的规则：本门 deps 指向测试假路径，真核会在 initialize 阶段
        // 报「.srs 打不开」——那是环境缺件，与「阻断出口的形状」正交。剔除只减不改，
        // 兜底 reject（无 matcher、无 rule_set）与 final 原样保留，被测形状不变。
        let mut route = v["route"].clone();
        route.as_object_mut().unwrap().remove("rule_set");
        let kept: Vec<serde_json::Value> = route["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r.get("rule_set").is_none())
            .cloned()
            .collect();
        route["rules"] = serde_json::Value::Array(kept);
        surface["route"] = route;
        // 兜底那条必须真的在送出去的那份里，否则下面的 rc=0 判的是另一个东西。
        let sent = surface["route"]["rules"].as_array().expect("route.rules");
        assert_eq!(
            sent.last().and_then(|r| r["action"].as_str()),
            Some("reject"),
            "送给真核的那份里没有兜底 reject"
        );
        std::fs::write(&p, serde_json::to_vec_pretty(&surface).unwrap()).expect("写盘");
        let (ok, diag) = check(&core, &p);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "随包核拒绝了阻断出口的配置形状：{diag}");
    }
}
