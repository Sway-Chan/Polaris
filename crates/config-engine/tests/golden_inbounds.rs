//! B1/H2 金样对拍 harness —— buildInbounds。
//!
//! 读 fixtures/inbounds.json（TS 导出 13 cases），逐条调 Rust build_inbounds，与 output diff。

use polaris_config_engine::builder::inbounds::{build_inbounds, InboundsDeps};
use polaris_config_engine::user_config::UserConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<InboundCase>,
}

#[derive(Debug, Deserialize)]
struct InboundCase {
    name: String,
    input: InboundInput,
    output: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct InboundInput {
    config: serde_json::Value,
    platform: String,
    ports: Ports,
}

#[derive(Debug, Deserialize)]
struct Ports {
    #[serde(rename = "probeDirect")]
    probe_direct: Option<u16>,
    #[serde(rename = "probeProxy")]
    probe_proxy: Option<u16>,
    #[serde(rename = "updateIn")]
    update_in: Option<u16>,
    #[serde(rename = "probePool", default)]
    probe_pool: Vec<u16>,
}

#[test]
fn inbounds_matches_polaris_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/inbounds.json");
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 fixture 失败: {e}"));
    let fixture: Fixture = serde_json::from_str(&raw).expect("fixture 解析失败");

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for case in &fixture.cases {
        let config: UserConfig = match serde_json::from_value(case.input.config.clone()) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("[{}] config 反序列化失败: {e}", case.name));
                continue;
            }
        };
        let deps = InboundsDeps {
            probe_direct_port: case.input.ports.probe_direct,
            probe_proxy_port: case.input.ports.probe_proxy,
            update_in_port: case.input.ports.update_in,
            probe_pool_ports: case.input.ports.probe_pool.clone(),
            platform: case.input.platform.clone(),
            own_lan_cidrs: vec![],
            log: |_, _| {},
        };

        let rust_inbounds = build_inbounds(&config, None, &deps);
        let rust_json: Vec<serde_json::Value> = rust_inbounds
            .iter()
            .map(|ib| serde_json::to_value(ib).unwrap())
            .collect();

        // 品牌统一：fixture 与 Rust 侧均使用 polaris-tun0（§D.2 品牌改名已落到两端），
        // 不再需要对 interface_name 做前缀替换，直接克隆 expected 即可。
        let ts_normalized: Vec<serde_json::Value> = case.output.to_vec();

        if rust_json != ts_normalized {
            failures.push(format!(
                "[{}] diff\n  TS ({} inbounds): {}\n  Rust ({}): {}",
                case.name,
                case.output.len(),
                serde_json::to_string(&case.output).unwrap_or_default(),
                rust_json.len(),
                serde_json::to_string(&rust_json).unwrap_or_default()
            ));
        }
        checked += 1;
    }

    assert!(!fixture.cases.is_empty(), "fixture 为空");
    assert!(
        failures.is_empty(),
        "{}/{} cases 对拍失败:\n{}",
        failures.len(),
        checked,
        failures.join("\n\n")
    );
}
