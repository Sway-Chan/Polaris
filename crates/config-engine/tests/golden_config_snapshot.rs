//! B1 整体金样对拍 harness —— generateSingBoxConfig 全链路。
//!
//! 读 fixtures/config-snapshot.json（TS 导出的 40+ 场景），逐条用 Rust generate_sing_box_config
//! 生成完整 SingBoxConfig，与 TS output 逐字段 diff。diff = 0 即 TS↔Rust 端到端等价。
//!
//! fixture 来源：scripts/export-config-snapshot-fixtures.mts（复用 Polaris config-snapshot.test.ts 的 snap() 场景）。
//! 这是 B1 的最终 gate——覆盖协议×模式×地区分流×TUN/systemProxy×FakeIP×DNS进阶×mesh/endpoint×抗封×transport 矩阵。
//!
//! 纪律：serde_json::Value 比较（规范化结构等价，忽略键序/空白）。路径分隔符归一化（\ → /）。
//!
//! # 夹具的来历与「定向更新」的例外（读之前必看）
//!
//! `fixtures/config-snapshot.json` 是 **上游 4.2.6 期从 TS builder 导出的冻结快照**，不是本仓产物。
//! 常态纪律是**只重生、不手改**：夹具一旦被人按 Rust 输出「顺手对齐」，这道门就退化成自证。
//!
//! **本次是有据可查的例外**：为对齐上游 `a942c60`（#335 —— dial 侧 `domain_resolver` 由纯 tag
//! 改为 `{server, strategy:"prefer_ipv4"}`，根因见 `singbox::DomainResolver`）对夹具做了**程序化定向
//! 变换**。变换规则与验收如下，复核时按此逐条对：
//!
//! - 射程：仅 `enableIPv6 !== true` 的场景，仅 `outbounds[]`（排除 `direct`/`block`/`selector`）与
//!   `endpoints[]` 两个容器的 `domain_resolver`；`dns.servers[].domain_resolver`（那是 DNS server
//!   自身的 bootstrap，不是 dial 侧）、`route.default_domain_resolver`、顶层 `dns.strategy` **一处未动**。
//! - 计数：恰好 46 处（45 proxy outbound + 1 wireguard endpoint），与上游侧的金样 delta 同数。
//! - 验收：变换**前**先跑全量 diff 分类，确认 Rust↔夹具的差异**只有** `domain_resolver` 这一类
//!   （46/46，无任何其它字段），才动夹具；`enableIPv6=true` 的那一条场景 diff 为 0，证明该分支字节零变化。
//!
//! **第二次例外（2026-08-10，FakeIP 两条修复）**：这次不是「对齐上游」而是**刻意与上游分叉** ——
//! 夹具冻结的是 上游的行为，而这两条正是 上游的缺陷（issue #347 取证），所以金样必须跟着改，
//! 否则这道门会把缺陷钉成基线。同样走程序化定向变换 + 全量语义对差：
//!
//! - **类 A（33 处）**：给形状**恰好**为 `{"query_type":["A","AAAA"],"server":"fakeip"}` 的 DNS 规则
//!   加 `"rewrite_ttl": 5`。射程判据是精确整体相等，不是「含 fakeip」——变换前统计过全夹具
//!   `server=="fakeip"` 的规则形状分布：**33 条，全部同形，一 case 一条**，无第二种形状。
//!   另 4 个 case（FakeIP 关）零命中，它们的 diff 在变换前后都是 0。理由见 `builder::dns` 的
//!   `FAKEIP_REWRITE_TTL`。
//! - **类 B（1 处）**：`DNS bypassFakeIP 自定义规则` 这一 case 里，由 bypass 规则产生的那条 DNS 规则
//!   `server` 由 `dns-bootstrap` 改 `dns-remote`。该 case 的两条 `customRules` 都是
//!   `action: "proxy"`（脚本里断言过），走代理的域名就该用境外解析器；旧值是从 上游 逐字继承的
//!   缺陷。理由见 `builder::dns` 里 bypass 分桶那段。
//! - **验收**：变换后对 backup 做**全量递归语义对差**，差异总数 **34 = 33 + 1**，只有上述两类，
//!   无任何第三类；随后全量金样 37/37 diff=0。变换只增不删、不改键序，行级 diff 33 个单行 hunk + 1。
//!
//! **第三次例外（2026-08-13，移除全部内置域名 reject 表）**：同第二次，是**刻意与上游分叉**
//! （上游的同一块删除当时还停在未提交的工作区里，issue #352）。用户裁定：本应用不内置
//! 任何域名 reject 表；开关驱动的 reject（blockQuic 的 UDP443、webrtcLeakProtection 的 STUN）
//! 与用户自定义规则的阻断**一条不动**。
//!
//! - **变换**：从每个 case 的 `expected.config.route.rules` 里删掉三种形状的规则 ——
//!   ① `action=reject` + `domain_keyword∋dns.google` + `port=[443,853]`；
//!   ② 同上 keyword + `network=["udp"]`（DoH-over-QUIC）；
//!   ③ `action=reject` + `domain_suffix∋gvt2.com`（Chrome/Edge beacon 14 域）。
//! - **计数**：共 110 条 = ①37 + ②37 + ③36。①② 命中全部 37 个 case（**印证它们无任何开关**）；
//!   ③ 少的那一个正是 direct 模式的 case，命中原代码 `proxy_mode != "direct"` 那道门。
//!   每 case 删除数分布 = 36 个删 3 条 + 1 个删 2 条，与上述完全自洽。
//! - **反向验收**：变换后夹具里**剩余**的 reject 只有两种形状 —— `network=["udp"], port=[443]`
//!   与 `protocol="stun"`，共 5 条，全部由开关产生。这同时证明那三张表就是内置域名 reject 的**全集**，
//!   没有第四张躲在别处。
//! - **验收**：变换后全量金样 **37/37 diff=0**（本门自身即差异器：若还有第二类差异，它必然仍红）。
//! - 生成侧的「不许重建」由 `builder::route::tests::no_builtin_domain_reject_table` 钉住。
//!
//! **第四次例外（2026-08-13，删除 legacy `block` 出站）**：出口选阻断改由**规则级** `action:"reject"`
//! 表达（所有「→代理」的规则整体改写 + 一条无 matcher 兜底），`{type:"block",tag:"block"}` 这个
//! 出站随之没有引用者，删除。触发它的是一条运行期事实：legacy block 每拦一条连接打一行 ERROR，
//! `log.level=warn` 过滤不掉，而本仓核日志单文件 + 满则轮转一次 ⇒ 停在阻断态会把排障历史挤出去。
//!
//! - **变换**：从每个 case 的 `expected.config.outbounds` 删掉形状**恰好**为
//!   `{"type":"block","tag":"block"}`（无其它键）的那一个。
//! - **计数**：37 条，**每 case 恰好 1 条，覆盖全部 37 个 case** —— 与「该出站此前是无条件发射」
//!   完全自洽。形状不符而被保留的 block 出站：**0**（即不存在第二种变体）。
//! - **反向验收**：变换后全夹具里 `"block"` 字面量出现 **0 次**。这同时证明**没有任何 selector
//!   成员表或 default 引用过它** —— 金样里 0 个 case 选中 `__block__`，与生成侧「成员按需加入」一致。
//! - **验收**：变换后全量金样 **37/37 diff=0**。
//! - ⚠️ **金样对新行为零覆盖**：没有任何 case 的 `selectedServerId` 是 `__block__`，故 exit=block
//!   这条路径的回归检测**不在金样上**，在 `builder::route::tests::block_exit_rejects_all_proxy_bound_traffic`
//!   （行为级，实现无关）与 `kernel_accepts_outbounds::bundled_core_accepts_block_exit_shape`（真核）。
//!
//! # 重生方式（不要在本仓手搓）
//!
//! 导出器在 **上游仓**：`scripts/export-config-snapshot-fixtures.test.ts`。重生要求 上游 主工作树
//! 干净——**当前它卡在 beta.7 半成品分支上**，所以本次走的是上面那条「程序化定向变换 + 全量 diff 自查」
//! 的路径。等 上游 主树可用时，应以真正重生的导出覆盖，届时本次变换应当被逐字复现（若不能，说明有漂移）。

#![allow(clippy::field_reassign_with_default)]

use polaris_config_engine::builder::{
    generate_sing_box_config, generate_sing_box_config_with_report, GenerateConfigDeps,
};
use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::proxy_mode::ProxyMode;
use serde::Deserialize;
use std::collections::BTreeMap;

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
    expected: ExpectedOutput,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // config 是对拍唯一被比较字段；id_to_tag_map/rule_target_map 仅为夹具完整性反序列化
struct ExpectedOutput {
    /// sing-box config JSON（完整 SingBoxConfig）。
    config: serde_json::Value,
    /// 生成后回写的 id→tag 映射。
    #[serde(default)]
    id_to_tag_map: BTreeMap<String, String>,
    /// 生成后回写的 ruleKey→targetServerId 映射。
    #[serde(default)]
    rule_target_map: BTreeMap<String, String>,
}

fn default_platform() -> String {
    "linux".to_string()
}

/// 上游 `existsSync` 的对拍 mock：快照导出时内置 geo `.srs` 文件已落盘（→ true），
/// custom-rule 的 `.json`（ext route）与 `.dns.json`（DNS bypass）外化文件未落盘（→ false）。
/// 后者落盘前 Polaris 回落 inline 生成（route-builder.ts:212 onDegraded）；故 mock 须返 false 才与金样一致。
fn snapshot_is_valid_srs(path: &str) -> bool {
    path.ends_with(".srs")
}

/// Polaris exporter 的 per-scenario `setup(svc)` 注入复刻（export-config-snapshot-fixtures.test.ts）。
/// 夹具不含 deps 元数据，按场景名注入 probe/lanResolver/updateIn——值与 exporter 的 setup() 调用逐字一致。
/// 返回 (probeDirectPort, probeProxyPort, updateInPort, lanResolverForDns)。
fn scenario_deps(name: &str) -> (Option<u16>, Option<u16>, Option<u16>, Option<String>) {
    match name {
        // 31. probe 端口注入：svc.probeDirectPort=21001; svc.probeProxyPort=21002
        "probe 端口注入" => (Some(21001), Some(21002), None, None),
        // 29. DNS lanResolverForDns 注入：svc.lanResolverForDns='192.168.1.1'
        "DNS lanResolverForDns 注入" => (None, None, None, Some("192.168.1.1".to_string())),
        // 32/33. update-in 端口注入：svc.updateInPort=21003
        "update-in 端口注入（smart）" | "update-in 端口注入（direct）" => {
            (None, None, Some(21003), None)
        }
        _ => (None, None, None, None),
    }
}

/// 路径分隔符归一化（\ → /），对齐 Polaris normalizeSep（跨平台快照一致）。
fn normalize_sep(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            *s = s.replace('\\', "/");
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalize_sep(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                normalize_sep(v);
            }
        }
        _ => {}
    }
}

/// 品牌归一化（历史脉络）：金样与 Rust 侧均统一使用 `polaris-` 前缀（polaris-dns-v2 cache_id /
/// polaris-tun0 接口名）。金样品牌统一后两端已一致，此函数现为 no-op，仅保留遍历骨架以备将来再次
/// 出现品牌差异时复用。
fn normalize_brand(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalize_brand(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                normalize_brand(v);
            }
        }
        _ => {}
    }
}

#[test]
fn generate_config_matches_polaris_snapshot() {
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/config-snapshot.json");
    // **不静默缩量**：`return` 型跳过让「夹具没了」与「全部通过」在 CI 上长得一模一样——这道门是
    // config-gen 的最终 gate，它没跑就必须转红。夹具由 `scripts/export-*-fixtures.mts` 导出且已入库。
    let raw = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!("fixtures/config-snapshot.json 读取失败（TS 导出器未跑？）: {e}")
    });
    let fixture: SnapshotFixture = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => panic!("fixture JSON 解析失败: {e}"),
    };

    assert!(
        !fixture.cases.is_empty(),
        "fixture 为空——导出器未跑或路径错"
    );

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for case in &fixture.cases {
        // Polaris exporter 的 setup(svc) per-scenario 注入（probe/lanResolver/updateIn 端口）——
        // 夹具不携带 deps 元数据，按场景名复刻 export-config-snapshot-fixtures.test.ts 的 setup() 调用。
        let (probe_direct_port, probe_proxy_port, update_in_port, lan_resolver_for_dns) =
            scenario_deps(&case.name);
        // 构造 deps：对齐 config-snapshot.test.ts snap() 的默认注入值。
        let deps = GenerateConfigDeps {
            platform: case.platform.clone(),
            arch: "x86_64".into(),
            race_server_port: 0, // race off（snap 默认）
            probe_direct_port,
            probe_proxy_port,
            update_in_port,
            probe_pool_ports: vec![],
            lan_resolver_for_dns,
            race_upstream_ips: vec![],
            race_upstream_ports: vec![],
            has_cronet: true, // naive 绕过 isNodeUsable（snap mock）
            cronet_copy_failed: false,
            has_management_api: false, // 1.13.0 无 services
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
            is_valid_srs_fn: snapshot_is_valid_srs, // 解封 geo .srs；custom-rule .json 落盘前不存在（对齐 Polaris existsSync）
            own_lan_cidrs: vec![],
            log: |_, _| {},
            on_degraded: || {},
        };

        let resolved_ips = BTreeMap::new();
        let result = generate_sing_box_config(&case.input, &resolved_ips, &deps);

        let rust_config = match result {
            Ok(cfg) => serde_json::to_value(&cfg).expect("Rust 输出序列化失败"),
            Err(e) => {
                failures.push(format!("[{}] Rust 生成失败: {e}", case.name));
                checked += 1;
                continue;
            }
        };

        // 路径归一化 + 品牌归一化（两端都做，防 host OS 差异 + polaris→polaris 品牌差异）。
        let mut expected_config = case.expected.config.clone();
        normalize_sep(&mut expected_config);
        normalize_brand(&mut expected_config); // TS polaris- → polaris-
        let mut rust_normalized = rust_config.clone();
        normalize_sep(&mut rust_normalized);

        if rust_normalized != expected_config {
            // 找第一个差异点（辅助调试）。
            let diff = find_first_diff(&expected_config, &rust_normalized);
            failures.push(format!(
                "[{}] config mismatch{}\n  expected: {}\n  got:      {}",
                case.name, diff, expected_config, rust_normalized
            ));
        }
        checked += 1;
    }

    // **射程对账**：本门每条 case 都必须被检查过（生成失败也计入 `checked` 并记 failure），
    // 少一条就说明有腿在静默缩量。
    assert_eq!(
        checked,
        fixture.cases.len(),
        "射程对账失败：checked={checked} ≠ fixture cases={}",
        fixture.cases.len()
    );
    assert!(
        failures.is_empty(),
        "{}/{} cases 对拍失败:\n{}",
        failures.len(),
        checked,
        failures.join("\n---\n")
    );
    eprintln!("整体对拍：{checked} cases 全部 diff=0");
}

/// **「资源缺失世界」对偶用例** —— 上面那个金样门的结构性盲区补丁。
///
/// 金样把 [`snapshot_is_valid_srs`] stub 成 `path.ends_with(".srs")` = **恒真**，断言的是「文件都在」
/// 的宇宙；而真机（2026-07-20）是「文件一个都不在」的宇宙：金样里 208 处 `rule_set` 引用，真机 0 处。
/// **那个门结构性抓不到「资源缺失」这整条故障面**（旧结论「config-gen 正确 — 金样证明」正是被它误导）。
///
/// 本用例把同一批 fixture 场景换到 `is_valid_srs_fn = |_| false` 重跑，断言的不是逐字节相等
/// （那个世界没有金样），而是**降级后的安全不变式**：
///
/// > **任何非 `proxyMode=direct` 的场景，`route.final` 都不得是 `direct`。**
///
/// 因为「资源缺失 → 规则被剪光」+「final=direct 兜底」叠加 = 全量明文直连（fail-open）。
/// 用户没选直连却全程明文出网，是本项目最严重的安全失效模式。
///
/// 教训（写给后来者）：把外部资源存在性 stub 成恒真的测试门，等于把「资源缺失」移出射程。
/// **此类 stub 必须配一个「资源缺失世界」的对偶用例**，否则那条腿没有门。
#[test]
fn resource_missing_world_never_falls_back_to_plaintext_direct() {
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/config-snapshot.json");
    // **不静默缩量**：fixture 缺失 = 这道门今天没跑，不是「通过」。同批 handoff 记的「空过滤器 rc=0
    // 冒充通过」就是这个形态——`return` 型门等于没门。
    let raw = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!("fixtures/config-snapshot.json 读取失败（TS 导出器未跑？）: {e}")
    });
    let fixture: SnapshotFixture =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture JSON 解析失败: {e}"));
    assert!(
        !fixture.cases.is_empty(),
        "fixture 为空——导出器未跑或路径错"
    );

    let mut failures = Vec::new();
    let mut checked = 0usize;
    let mut pruned_cases = 0usize;
    // 生成失败（无可用节点等）的场景：**记名单**而非静默 continue，末尾与 cases.len() 对账。
    let mut skipped: Vec<String> = Vec::new();
    // D4/D7 组网出口回退场景（本不变式的已知边界，见下方注释）。
    let mut mesh_fallback_cases: Vec<String> = Vec::new();

    for case in &fixture.cases {
        let (probe_direct_port, probe_proxy_port, update_in_port, lan_resolver_for_dns) =
            scenario_deps(&case.name);
        let deps = GenerateConfigDeps {
            platform: case.platform.clone(),
            arch: "x86_64".into(),
            race_server_port: 0,
            probe_direct_port,
            probe_proxy_port,
            update_in_port,
            probe_pool_ports: vec![],
            lan_resolver_for_dns,
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
            // ★ 与金样唯一的差异：**这个宇宙里一个 .srs 都不在**（= 真机首装的默认状态）。
            is_valid_srs_fn: |_| false,
            own_lan_cidrs: vec![],
            log: |_, _| {},
            on_degraded: || {},
        };

        let resolved_ips = BTreeMap::new();
        let Ok(outcome) = generate_sing_box_config_with_report(&case.input, &resolved_ips, &deps)
        else {
            // 生成失败（无可用节点等）不属本用例射程，由金样门覆盖——但必须留名对账。
            skipped.push(case.name.clone());
            continue;
        };
        checked += 1;
        if !outcome.pruned_rule_set_tags.is_empty() {
            pruned_cases += 1;
        }

        let user_selected_direct = case.input.proxy_mode == ProxyMode::Direct;
        let final_outbound = outcome
            .config
            .route
            .as_ref()
            .and_then(|r| r.final_outbound.as_deref());

        // **不变式的第二条边界（D4/D7 组网出口回退）**：选中的组网节点关了外网访问时
        // `user_exit_tag` 本身就是 `direct`，T2 fail-safe **无处可退**（写 direct 到 direct 是 no-op），
        // 于是 `final=direct` 在非 direct 模式下是**合法的设计语义**，不是 fail-open。
        //
        // 当前 fixture 里没有这种场景（下面的 assert 会盯着这件事）。将来谁补了 mesh-exit-fallback
        // fixture，这道门会在那一条上「误报转红」——**那不是噪音，是本不变式的已知边界**，
        // 正确做法是像这里一样把它排除在射程外，而不是把整条不变式改掉/删掉。
        let mesh_exit_fallback =
            polaris_config_engine::builder::route::mesh_selected_exit_falls_back_to_direct(
                &case.input,
            );
        if mesh_exit_fallback {
            mesh_fallback_cases.push(case.name.clone());
        }

        if user_selected_direct {
            // 用户显式选全直连 → direct 是意图不是降级，必须保持。
            if final_outbound != Some("direct") {
                failures.push(format!(
                    "[{}] proxyMode=direct 却把 final 改成了 {final_outbound:?}——用户意图被 fail-safe 误伤",
                    case.name
                ));
            }
        } else if mesh_exit_fallback {
            // 已在射程外（见上）——只登记，不断言。
        } else if final_outbound == Some("direct") {
            failures.push(format!(
                "[{}] 资源缺失世界里 final 落到 direct：规则已被剪光 + 兜底直连 = 全量明文直连（fail-open）。\
                 剪枝清单={:?}",
                case.name, outcome.pruned_rule_set_tags
            ));
        }
    }

    // **射程对账**：checked + skipped 必须等于 fixture 全量。缺这条，任何让 generate 提前失败的
    // 改动都会让本门静默缩量（极限情形 checked=0 仍绿），与「空过滤器 rc=0 冒充通过」同型。
    assert_eq!(
        checked + skipped.len(),
        fixture.cases.len(),
        "射程对账失败：checked={checked} + skipped={} ≠ fixture cases={}（跳过名单：{skipped:?}）",
        skipped.len(),
        fixture.cases.len()
    );
    // 这个世界必须真的发生过剪枝——否则本用例在空跑（stub 没生效 / 场景不引用 geo），
    // 那样它就成了假绿的又一个来源。
    assert!(
        pruned_cases > 0,
        "{checked} 个场景里一次剪枝都没发生 → 本对偶用例没有真正进入「资源缺失世界」，等于空门"
    );
    assert!(
        failures.is_empty(),
        "{}/{} cases 在资源缺失世界里退化成明文直连:\n{}",
        failures.len(),
        checked,
        failures.join("\n---\n")
    );
    eprintln!(
        "资源缺失世界：{checked} cases 全部守住「非 direct 模式绝不明文直连」\
         （{pruned_cases} 例发生剪枝；{} 例生成失败跳过；{} 例 mesh 出口回退在射程外）",
        skipped.len(),
        mesh_fallback_cases.len()
    );
}

// ── `res:builtin:<tag>` 自定义规则的端到端门 ──────────────────────────────────────────
//
// **为什么是手写场景而不是 fixture**：`fixtures/config-snapshot.json` 40+ 个场景里**一条 `res:` 引用
// 都没有**（`grep '"res:' fixtures/` = 0 命中）⇒ 整条「用户在自定义规则里引用规则资源」的路径
// **不在金样射程内**。此前 `builtin_rule_set_file_name` 恒返 `None`，用户规则被整条静默剪掉，
// 金样门一片绿——正是「stub 把一整条故障面移出射程」的又一实例（同文件上方对偶用例的同型教训）。

/// 端到端 golden：一条引用 `res:builtin:<tag>` 的自定义规则，必须在生成的 sing-box config 里
/// **既有 route 规则引用、又有配套 rule_set 定义**（缺任一边：前者=规则失效，后者=核启动 FATAL/被剪）。
///
/// **变异锁**：把 `custom_rules.rs` 内置分支改回恒 `None`（旧占位实现）→ 规则被剪、rule_set 定义消失
/// ⇒ 下面三条断言全红。把路径由 catalog fileName 改回 `<tag>.srs` 拼接 → `category-ai` 那条红。
#[test]
fn builtin_res_ref_survives_into_generated_config() {
    let mut config = smart_config_with_rule_set_ref("res:builtin:geosite-netflix");
    // 第二条规则引用 fileName ≠ `<tag>.srs` 的条目（`geosite-category-ai` → `…-!cn.srs`）。
    config
        .custom_rules
        .push(rule_set_rule("r-ai", "res:builtin:geosite-category-ai"));

    let deps = GenerateConfigDeps {
        // 金样同款「.srs 都在」的世界（真机播种后的常态）。
        is_valid_srs_fn: snapshot_is_valid_srs,
        ..scenario_deps_base()
    };
    let outcome = generate_sing_box_config_with_report(&config, &BTreeMap::new(), &deps)
        .expect("生成应成功（单 vless 节点 + smart）");
    let route = outcome.config.route.as_ref().expect("smart 模式应有 route");

    // 1) route 规则里确有引用——只有自定义规则腿能产出它（内置 geo 注入腿只加定义、不加规则）。
    let referencing = |tag: &str| {
        route.rules.iter().any(|r| match r.rule_set.as_ref() {
            Some(polaris_config_engine::singbox::OneOrMany::Many(v)) => v.iter().any(|t| t == tag),
            Some(polaris_config_engine::singbox::OneOrMany::One(s)) => s == tag,
            None => false,
        })
    };
    assert!(
        referencing("geosite-netflix"),
        "res:builtin 规则被静默剪掉了（route.rules 无 geosite-netflix 引用）：{:?}",
        route.rules
    );
    assert!(
        referencing("geosite-category-ai"),
        "fileName≠<tag>.srs 的内置条目同样不得被剪"
    );

    // 2) 引用必须有配套定义，且路径取自 catalog fileName（**不是**由 tag 拼出来的）。
    let def = |tag: &str| {
        route
            .rule_set
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|rs| rs.tag == tag)
            .unwrap_or_else(|| panic!("缺 rule_set 定义: {tag}"))
    };
    let nf = def("geosite-netflix");
    assert_eq!(nf.type_field, "local");
    assert_eq!(nf.format, "binary");
    assert_eq!(
        nf.path.as_deref(),
        Some("/fake/userData/rules/geosite-netflix.srs")
    );
    assert_eq!(
        def("geosite-category-ai").path.as_deref(),
        Some("/fake/userData/rules/geosite-category-ai-!cn.srs"),
        "路径须来自 catalog 的 fileName（MetaCubeX 无裸 category-ai .srs），不得由 tag 拼接"
    );

    // 3) 没有被末端悬空剪枝二次剪掉。
    assert!(
        !outcome
            .pruned_rule_set_tags
            .iter()
            .any(|t| t == "geosite-netflix" || t == "geosite-category-ai"),
        "剪枝清单不该含内置引用：{:?}",
        outcome.pruned_rule_set_tags
    );
}

/// **Task B 的端到端断言**：`.srs` 缺失 → 规则被剪，且**必须**经生产接线（route → custom_rules）
/// 打出 warn。此前 `CustomRulesDeps::log_warn` 是 `let _ = msg;`，生产期规则被剪零线索。
///
/// **变异锁**：把 `log_warn` 改回 no-op、或删掉 `route.rs` 里 `log: deps.log` 这行 → 本条转红。
#[test]
fn builtin_res_ref_missing_srs_emits_warn_through_production_wiring() {
    thread_local! {
        static SINK: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    fn capture(_lvl: polaris_config_engine::user_config::LogLevel, msg: &str) {
        SINK.with(|s| s.borrow_mut().push(msg.to_string()));
    }
    SINK.with(|s| s.borrow_mut().clear());

    let config = smart_config_with_rule_set_ref("res:builtin:geosite-netflix");
    let deps = GenerateConfigDeps {
        is_valid_srs_fn: |_| false, // 首装态：一个 .srs 都不在
        log: capture,
        ..scenario_deps_base()
    };
    let outcome =
        generate_sing_box_config_with_report(&config, &BTreeMap::new(), &deps).expect("生成应成功");
    let route = outcome.config.route.as_ref().expect("smart 模式应有 route");
    assert!(
        !route.rules.iter().any(|r| r.rule_set.is_some()),
        "文件缺失须 fail-closed：不得留下引用不存在 rule_set 的规则"
    );

    let warns = SINK.with(|s| s.borrow().clone());
    assert!(
        warns
            .iter()
            .any(|m| m.contains("内置资源文件缺失/损坏") && m.contains("geosite-netflix.srs")),
        "规则被剪必须留线索（生产曾静音）：{warns:?}"
    );
}

/// smart + systemProxy + 单 vless 节点 + 一条 `ruleSet` 类型自定义规则。
fn smart_config_with_rule_set_ref(value: &str) -> UserConfig {
    use polaris_config_engine::user_config::proxy_mode::ProxyModeType;
    use polaris_config_engine::user_config::server_config::{Protocol, SecurityMode, ServerConfig};
    UserConfig {
        servers: vec![ServerConfig {
            id: "s1".into(),
            name: "HK".into(),
            protocol: Protocol::Vless,
            address: "hk.example.com".into(),
            port: 443,
            uuid: Some("u".into()),
            security: Some(SecurityMode::Tls),
            ..Default::default()
        }],
        selected_server_id: Some("s1".into()),
        proxy_mode: ProxyMode::Smart,
        proxy_mode_type: ProxyModeType::SystemProxy,
        custom_rules: vec![rule_set_rule("r-builtin", value)],
        ..Default::default()
    }
}

fn rule_set_rule(id: &str, value: &str) -> polaris_config_engine::user_config::rule::Rule {
    use polaris_config_engine::user_config::rule::{Rule, RuleAction, RuleType};
    Rule {
        id: id.into(),
        type_field: RuleType::RuleSet,
        values: vec![value.into()],
        conditions: None,
        combine_mode: None,
        action: RuleAction::Proxy,
        enabled: true,
        bypass_fakeip: None,
        target_server_id: None,
        remarks: None,
        tls_spoof: None,
        tls_spoof_method: None,
    }
}

/// 上面两条手写场景共用的 deps 基线（与本文件金样门同值；调用方按需覆盖 `is_valid_srs_fn` / `log`）。
fn scenario_deps_base() -> GenerateConfigDeps {
    GenerateConfigDeps {
        platform: "linux".into(),
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
        is_valid_srs_fn: |_| false,
        own_lan_cidrs: vec![],
        log: |_, _| {},
        on_degraded: || {},
    }
}

/// 找两个 JSON Value 的第一个差异路径（辅助调试）。
fn find_first_diff(expected: &serde_json::Value, actual: &serde_json::Value) -> String {
    fn diff_at(
        expected: &serde_json::Value,
        actual: &serde_json::Value,
        path: &str,
    ) -> Option<String> {
        match (expected, actual) {
            (serde_json::Value::Object(e), serde_json::Value::Object(a)) => {
                // 检查 expected 有而 actual 无/不同的键。
                for (k, ev) in e {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match a.get(k) {
                        Some(av) => {
                            if let Some(d) = diff_at(ev, av, &p) {
                                return Some(d);
                            }
                        }
                        None => return Some(format!("：缺键 {p}")),
                    }
                }
                // 检查 actual 有而 expected 无的多余键。
                for k in a.keys() {
                    if !e.contains_key(k) {
                        let p = if path.is_empty() {
                            k.clone()
                        } else {
                            format!("{path}.{k}")
                        };
                        return Some(format!("：多余键 {p}"));
                    }
                }
                None
            }
            (serde_json::Value::Array(e), serde_json::Value::Array(a)) => {
                if e.len() != a.len() {
                    return Some(format!(
                        "：数组长度 {path} expected={} actual={}",
                        e.len(),
                        a.len()
                    ));
                }
                for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                    let p = format!("{path}[{i}]");
                    if let Some(d) = diff_at(ev, av, &p) {
                        return Some(d);
                    }
                }
                None
            }
            (e, a) if e != a => Some(format!("：值 {path} expected={e} actual={a}")),
            _ => None,
        }
    }
    diff_at(expected, actual, "").unwrap_or_default()
}

/// **引用闭合门**：任何下发的 `domain_resolver` 引用的 tag 都必须在 `dns.servers[].tag` 里存在。
///
/// # 为什么金样门抓不到这条（上游的一次实证逃逸）
///
/// 上游 记录过：把 `dns-node-race` 这个 DNS server tag 改名、而 dial 侧引用不动，**全量单测 +
/// 快照全绿放行**——因为快照比的是「Rust 输出 == 冻结的 TS 输出」，两端同源同错时差异为零；
/// 只有真跑内核才红（`domain_resolver` 指向不存在的 server ⇒ 解析期 FATAL）。
/// 逐字节对拍门天然对**内部一致性**无感，这道门就是补那个洞。
///
/// # 射程：泛化遍历，不逐结构枚举
///
/// 对生成结果的 JSON **递归**收集所有 `domain_resolver` / `default_domain_resolver`，两种形态统一取 tag
/// （字符串 → 自身；`{server, strategy}` → `server`）。故它同时覆盖 dial 侧（outbounds/endpoints）、
/// `dns.servers[]` 的 bootstrap、`route.default_domain_resolver`，以及**将来任何新长出来的载体**——
/// 逐结构枚举的写法会在新载体出现时静默漏检，而漏检恰恰是本门要防的那件事。
#[test]
fn every_domain_resolver_reference_resolves_to_a_dns_server_tag() {
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/config-snapshot.json");
    // 不静默缩量：夹具读不到 = 这道门今天没跑，必须转红（同本文件其它门）。
    let raw = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("fixtures/config-snapshot.json 读取失败: {e}"));
    let fixture: SnapshotFixture =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture JSON 解析失败: {e}"));

    /// 递归收集 (JSON 路径, 被引用的 tag)。`{strategy}` 缺 `server` 记成 `None`——
    /// 内核对那种形态是解析期硬拒（`empty domain_resolver.server`），必须一并转红。
    fn collect(v: &serde_json::Value, path: &str, out: &mut Vec<(String, Option<String>)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let p = format!("{path}/{k}");
                    if k == "domain_resolver" || k == "default_domain_resolver" {
                        let tag = match child {
                            serde_json::Value::String(s) => Some(s.clone()),
                            serde_json::Value::Object(o) => {
                                o.get("server").and_then(|s| s.as_str()).map(String::from)
                            }
                            _ => None,
                        };
                        out.push((p.clone(), tag));
                    }
                    collect(child, &p, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for (i, child) in arr.iter().enumerate() {
                    collect(child, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    let mut failures: Vec<String> = Vec::new();
    let mut checked_cases = 0usize;
    let mut checked_refs = 0usize;

    for case in &fixture.cases {
        let (probe_direct_port, probe_proxy_port, update_in_port, lan_resolver_for_dns) =
            scenario_deps(&case.name);
        let deps = GenerateConfigDeps {
            platform: case.platform.clone(),
            arch: "x86_64".into(),
            race_server_port: 0,
            probe_direct_port,
            probe_proxy_port,
            update_in_port,
            probe_pool_ports: vec![],
            lan_resolver_for_dns,
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
            is_valid_srs_fn: snapshot_is_valid_srs,
            own_lan_cidrs: vec![],
            log: |_, _| {},
            on_degraded: || {},
        };

        let resolved_ips = BTreeMap::new();
        let Ok(cfg) = generate_sing_box_config(&case.input, &resolved_ips, &deps) else {
            // 生成失败由金样门归因，本门不重复报，但也不计入 checked（末尾对账会暴露缩量）。
            continue;
        };
        checked_cases += 1;

        let known: std::collections::BTreeSet<String> = cfg
            .dns
            .as_ref()
            .map(|d| d.servers.iter().map(|s| s.tag.clone()).collect())
            .unwrap_or_default();

        let value = serde_json::to_value(&cfg).expect("config 序列化失败");
        let mut refs = Vec::new();
        collect(&value, "", &mut refs);
        checked_refs += refs.len();

        for (path, tag) in refs {
            match tag {
                None => failures.push(format!("[{}] {path} 取不出 server tag", case.name)),
                Some(t) if !known.contains(&t) => failures.push(format!(
                    "[{}] {path} 引用了不存在的 DNS server tag `{t}`（现有：{:?}）",
                    case.name, known
                )),
                _ => {}
            }
        }
    }

    // **反空过**：refs=0 会让本门在「生成器不再下发 domain_resolver」时假绿。
    // 每条场景至少有 direct 的一处 + route.default 一处，故下界取 case 数的 2 倍。
    assert!(
        checked_refs >= checked_cases * 2,
        "射程异常：{checked_cases} 条场景只收集到 {checked_refs} 处 domain_resolver 引用——\
         生成器停发了？本门在这种情况下会假绿，故此处转红"
    );
    assert!(
        failures.is_empty(),
        "{} 处 domain_resolver 引用悬空（共检查 {checked_refs} 处 / {checked_cases} 条场景）:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("引用闭合：{checked_refs} 处 domain_resolver 引用全部命中 dns.servers[].tag（{checked_cases} 条场景）");
}
