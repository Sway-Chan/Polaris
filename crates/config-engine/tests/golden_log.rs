//! B1 金样对拍 harness —— log builder。
//!
//! 读 fixtures/log.json（TS 导出的 180 cases），逐条用 Rust build_log_config 生成，
//! 与 TS output 逐字节 diff（serde_json 规范化后字符串比较）。diff = 0 即 TS↔Rust 等价。
//!
//! fixture 来源：scripts/export-log-fixtures.mts（内联 Polaris singbox-log-builder.ts 逻辑）。
//! 这是 B1 对拍脚手架的首条通路；后续 builder（dns/route/outbounds/inbounds/custom-rules）
//! 各加自己的 fixture 导出器 + 对拍 test 模块。

use polaris_config_engine::builder::{build_log_config, LogBuildDeps, LogConfigInput, Platform};
use polaris_config_engine::user_config::{LogLevel, ProxyModeType};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LogFixture {
    cases: Vec<LogCase>,
}

#[derive(Debug, Deserialize)]
struct LogCase {
    name: String,
    platform: String,
    input: LogCaseInput,
    #[serde(rename = "privacyMode")]
    privacy_mode: bool,
    output: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct LogCaseInput {
    #[serde(rename = "logLevel")]
    log_level: LogLevel,
    #[serde(rename = "disableLogFile")]
    disable_log_file: bool,
    #[serde(rename = "proxyModeType")]
    proxy_mode_type: ProxyModeType,
}

#[test]
fn log_builder_matches_polaris_golden() {
    let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/log.json");
    let raw = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("读 fixture 失败 {fixture_path}: {e}"));
    let fixture: LogFixture = serde_json::from_str(&raw).expect("fixture JSON 解析失败");

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for case in &fixture.cases {
        let input = LogConfigInput {
            log_level: case.input.log_level,
            disable_log_file: case.input.disable_log_file,
            proxy_mode_type: case.input.proxy_mode_type,
        };
        let deps = LogBuildDeps {
            privacy_mode: case.privacy_mode,
            // proto Platform::parse 兼容 "darwin"/"win32"/"linux"；未知 → Other（log builder 视同 Linux）。
            platform: Platform::parse(&case.platform),
            // 与 export-log-fixtures.mts FAKE_LOG_PATH 一致。
            log_file_path: Some("/fake/singbox.log"),
        };

        let rust_output = build_log_config(&input, &deps);
        let rust_json = serde_json::to_value(&rust_output).expect("Rust 输出序列化失败");

        // 逐字节 diff：serde_json::Value 比较（规范化的结构等价，忽略空白/键序）。
        if rust_json != case.output {
            failures.push(format!(
                "[{}] mismatch\n  expected (TS): {}\n  got      (Rust): {}",
                case.name, case.output, rust_json
            ));
        }
        checked += 1;
    }

    assert!(
        !fixture.cases.is_empty(),
        "fixture 为空——导出器未跑或路径错"
    );
    assert!(
        failures.is_empty(),
        "{}/{} cases 对拍失败:\n{}",
        failures.len(),
        checked,
        failures.join("\n")
    );
}
