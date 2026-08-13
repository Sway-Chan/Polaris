//! **显式 HTTP client 门**：锁住「生成产物里不存在任何依赖隐式默认 HTTP client 的消费点」。
//!
//! # 这扇门守的是什么
//!
//! sing-box 1.14.0 把「隐式默认 HTTP client（经默认出站）」标为弃用、**计划 1.16.0 移除**
//! （上游 `experimental/deprecated/constants.go` 的 `OptionImplicitDefaultHTTPClient`：
//! DeprecatedVersion "1.14.0" / ScheduledVersion "1.16.0"）。移除后
//! `httpclient.Manager.DefaultTransport()` 拿不到回落工厂即返回 nil，消费点直接报错——
//! 对 dashboard 是 `create dashboard http client` → API service 起不来。
//!
//! 核里**只有两个**消费点会去要那个默认 transport（alpha.45 源码全仓 `DefaultTransport()`
//! 调用面：`service/api/dashboard.go:106` 与 `route/rule/rule_set_remote.go:282`）。本门就按
//! 这两条逐一断言显式声明到位。
//!
//! # 为什么门里带正向对照
//!
//! 本仓当前**不生成任何 `type:"remote"` rule-set**（fail-closed 改造后全量 `type:"local"`），
//! 所以「远端 rule-set 必须带 http_client」这条在真实语料上恒真——**恒真的断言等于没有断言**。
//! 故 [`predicate_has_teeth`] 用合成配置反向证明：把违规形态喂给同一个谓词，必须报出违规。
//! 语料侧转绿 + 谓词侧有牙，合起来才是有信息量的门。

use polaris_config_engine::builder::{generate_sing_box_config, GenerateConfigDeps};
use polaris_config_engine::user_config::app_config::UserConfig;
use serde::Deserialize;
use serde_json::{json, Value};
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
}

fn default_platform() -> String {
    "linux".to_string()
}

/// 与 `golden_config_snapshot.rs` 同口径：解封 geo `.srs`，custom-rule 外化文件视为未落盘。
fn is_valid_srs(path: &str) -> bool {
    path.ends_with(".srs")
}

/// 谓词：返回该 config 上所有「依赖隐式默认 HTTP client」的违规点。空 vec = 合规。
///
/// 直接吃 `serde_json::Value` 而非强类型：门要守的是**落到核嘴里的 JSON 形态**，
/// 走类型会把「字段存在但序列化时被 skip 掉」这类失效方式漏掉。
fn violations(cfg: &Value) -> Vec<String> {
    let mut out = Vec::new();

    // 配置里真实存在的出站 tag 全集（detour 必须命中其一，否则核 "outbound not found" FATAL）。
    let mut known_tags: Vec<&str> = Vec::new();
    for key in ["outbounds", "endpoints"] {
        if let Some(arr) = cfg.get(key).and_then(Value::as_array) {
            known_tags.extend(arr.iter().filter_map(|o| o.get("tag")?.as_str()));
        }
    }

    // ── 消费点 1：services[].dashboard（core `service/api/dashboard.go:resolveTransport`）──
    if let Some(services) = cfg.get("services").and_then(Value::as_array) {
        for (i, svc) in services.iter().enumerate() {
            let Some(dash) = svc.get("dashboard") else {
                continue;
            };
            if dash.is_null() {
                continue;
            }
            let Some(hc) = dash.get("http_client") else {
                out.push(format!(
                    "services[{i}].dashboard 缺 http_client（会落隐式默认 HTTP client）"
                ));
                continue;
            };
            match hc.get("detour").and_then(Value::as_str) {
                None | Some("") => out.push(format!(
                    "services[{i}].dashboard.http_client.detour 为空——核判 IsEmpty() 后仍回落隐式默认"
                )),
                Some(d) if !known_tags.contains(&d) => out.push(format!(
                    "services[{i}].dashboard.http_client.detour={d:?} 不在出站/endpoint tag 集合里"
                )),
                Some(_) => {}
            }
        }
    }

    // ── 消费点 2：route.rule_set[] 里的 type:"remote"（core `rule_set_remote.go:resolveTransport`）──
    if let Some(rule_sets) = cfg
        .get("route")
        .and_then(|r| r.get("rule_set"))
        .and_then(Value::as_array)
    {
        for (i, rs) in rule_sets.iter().enumerate() {
            if rs.get("type").and_then(Value::as_str) != Some("remote") {
                continue;
            }
            let tag = rs.get("tag").and_then(Value::as_str).unwrap_or("?");
            if rs.get("http_client").is_none() {
                out.push(format!(
                    "route.rule_set[{i}]({tag}) 是 remote 但缺 http_client（会落隐式默认 HTTP client）"
                ));
            }
            // `download_detour` 是 1.14.0 起的 legacy 键，同样 1.16.0 移除；且与 http_client
            // 并存时核在运行期直接报 "http_client is conflict with deprecated download_detour
            // field"（`sing-box check` 放行，只有起核才炸）——故在此拦。
            if rs.get("download_detour").is_some() {
                out.push(format!(
                    "route.rule_set[{i}]({tag}) 用了 legacy download_detour（1.16.0 移除；与 http_client 并存会运行期 FATAL）"
                ));
            }
        }
    }

    out
}

/// 主门：37 例金样输入 × {带 serve_dir, 不带 serve_dir} 全量生成后逐条查不变式。
///
/// 用金样**输入**而非输出：输出侧 `has_management_api=false` 恒无 `services`，覆盖不到 dashboard。
/// 这里把管理 API 与 dashboard 一律打开，让每个模式/地区/TUN/协议组合都真的走一遍注入分支。
#[test]
fn no_implicit_default_http_client_in_generated_config() {
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/config-snapshot.json");
    // 与金样门同纪律：夹具缺失必须转红，不能 `return` 静默缩量成「全绿」。
    let raw = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("fixtures/config-snapshot.json 读取失败: {e}"));
    let fixture: SnapshotFixture =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture JSON 解析失败: {e}"));
    assert!(
        !fixture.cases.is_empty(),
        "fixture 为空——导出器未跑或路径错"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut dashboards_seen = 0usize;

    for case in &fixture.cases {
        for serve_dir in [Some("/fake/dashboard".to_string()), None] {
            let mut input = case.input.clone();
            input.singbox_dashboard = Some(true);

            let deps = GenerateConfigDeps {
                platform: case.platform.clone(),
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
                has_management_api: true, // 金样默认 false；本门必须打开才碰得到 services
                privacy_mode: false,
                log_level: polaris_config_engine::user_config::LogLevel::Info,
                disable_log_file: false,
                dashboard_serve_dir: serve_dir.clone(),
                tailscale_api_port: 51066,
                cache_path: "/fake/userData/cache.db".into(),
                log_file_path: Some("/fake/userData/singbox.log".into()),
                runtime_rules_dir: "/fake/userData/rules".into(),
                rule_resources_path: "/fake/userData/rule-resource".into(),
                custom_rules_dir: "/fake/userData/custom-rules".into(),
                tailscale_state_dir_prefix: "/fake/userData/tailscale".into(),
                is_valid_srs_fn: is_valid_srs,
                own_lan_cidrs: vec![],
                log: |_, _| {},
                on_degraded: || {},
            };

            let Ok(config) = generate_sing_box_config(&input, &BTreeMap::new(), &deps) else {
                // 生成失败（如场景本身构造了非法节点）不是本门的判据，跳过但不计入覆盖。
                continue;
            };
            let value = serde_json::to_value(&config).expect("SingBoxConfig 序列化失败");
            checked += 1;
            if value
                .get("services")
                .and_then(Value::as_array)
                .is_some_and(|s| s.iter().any(|x| x.get("dashboard").is_some()))
            {
                dashboards_seen += 1;
            }
            for v in violations(&value) {
                failures.push(format!("[{}|serve_dir={:?}] {v}", case.name, serve_dir));
            }
        }
    }

    // 覆盖下界：防「生成全 Err → 一条没查 → 空绿」。
    assert!(
        checked >= fixture.cases.len(),
        "有效生成数 {checked} 低于场景数 {}——门在空转",
        fixture.cases.len()
    );
    assert!(
        dashboards_seen >= checked,
        "只有 {dashboards_seen}/{checked} 例真的注入了 dashboard——本门没碰到目标分支"
    );
    assert!(
        failures.is_empty(),
        "存在依赖隐式默认 HTTP client 的消费点（sing-box 1.16.0 将移除该回落）：\n{}",
        failures.join("\n")
    );
}

/// 正向对照：证明 [`violations`] 谓词能抓到违规，而不是恒返回空。
///
/// 真实语料里 remote rule-set 一条都没有，那条断言在主门里恒真；没有这个对照，
/// 谓词整段被删成 `Vec::new()` 主门也照样绿。
#[test]
fn predicate_has_teeth() {
    let base_outbounds = json!([{ "tag": "proxy-selector" }, { "tag": "direct" }]);

    // 1. dashboard 缺 http_client → 必须报违规。
    let bad_dash = json!({
        "outbounds": base_outbounds,
        "services": [{ "type": "api", "dashboard": { "enabled": true, "path": "/x" } }],
    });
    assert_eq!(
        violations(&bad_dash).len(),
        1,
        "dashboard 缺 http_client 未被抓到"
    );

    // 2. dashboard 的 detour 为空串 → 核会判 IsEmpty() 重新落回隐式默认，必须报违规。
    let empty_detour = json!({
        "outbounds": base_outbounds,
        "services": [{ "type": "api", "dashboard": { "enabled": true, "http_client": { "detour": "" } } }],
    });
    assert_eq!(violations(&empty_detour).len(), 1, "空 detour 未被抓到");

    // 3. detour 指向不存在的出站 → 核 "outbound not found" FATAL，必须报违规。
    let dangling = json!({
        "outbounds": base_outbounds,
        "services": [{ "type": "api", "dashboard": { "enabled": true, "http_client": { "detour": "nope" } } }],
    });
    assert_eq!(violations(&dangling).len(), 1, "悬空 detour 未被抓到");

    // 4. remote rule-set 缺 http_client → 必须报违规（本仓当前不生成，锁的是将来）。
    let bad_remote = json!({
        "outbounds": base_outbounds,
        "route": { "rule_set": [
            { "tag": "r1", "type": "remote", "format": "binary", "url": "https://x/y.srs" },
            { "tag": "l1", "type": "local", "format": "binary", "path": "/x/y.srs" },
        ]},
    });
    assert_eq!(
        violations(&bad_remote).len(),
        1,
        "remote rule-set 缺 http_client 未被抓到（local 不应误报）"
    );

    // 5. legacy download_detour → 必须报违规。
    let legacy = json!({
        "outbounds": base_outbounds,
        "route": { "rule_set": [
            { "tag": "r1", "type": "remote", "format": "binary", "url": "https://x/y.srs",
              "http_client": { "detour": "direct" }, "download_detour": "direct" },
        ]},
    });
    assert_eq!(
        violations(&legacy).len(),
        1,
        "legacy download_detour 未被抓到"
    );

    // 6. 合规形态 → 零违规（防谓词恒报错这种「反向恒真」）。
    let good = json!({
        "outbounds": base_outbounds,
        "services": [{ "type": "api", "dashboard": { "enabled": true, "http_client": { "detour": "proxy-selector" } } }],
        "route": { "rule_set": [
            { "tag": "r1", "type": "remote", "format": "binary", "url": "https://x/y.srs",
              "http_client": { "detour": "direct" } },
            { "tag": "l1", "type": "local", "format": "binary", "path": "/x/y.srs" },
        ]},
    });
    assert!(
        violations(&good).is_empty(),
        "合规配置被误报：{:?}",
        violations(&good)
    );
}
