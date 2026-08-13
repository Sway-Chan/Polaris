//! daemon 二进制的极简 flag 解析（跨平台共用，无 cfg）。
//!
//! 三平台 daemon `main()` 忠实迁自 上游 Go helper（`helper/helper.go` M16 / `helper-linux/main.go` L15 /
//! `helper-win/main.go` W19），Go 侧用标准库 `flag` 包解析 `--key value` / `--key=value` / 布尔 `--flag`。
//! 本模块提供这三种形态的最小子集解析 —— **纯逻辑**（不碰 OS），故放 crate 顶层共用（组织原则见
//! [`crate`] 文档「默认共用」），三平台 `parse_args` 复用它 + 各自套默认值（flag 名/默认值是真平台差异）。
//!
//! 不追求 `flag` 包全功能（无 `-h`/类型校验/`flag.Usage`）—— daemon 由服务管理器（launchd/systemd/SCM）
//! 以固定 argv 拉起，flag 是安装期锁定的受控输入，非交互 CLI。

use std::collections::HashMap;

/// 解析 daemon flag：`--key value` / `-key value` / `--key=value` / 布尔 `--flag`。
///
/// - `argv` 应含 argv\[0]（程序名，内部跳过），与 [`std::env::args`] 直接对接。
/// - `bool_flags` 列出**无值开关**（如 `console`）：命中即置 `"true"`，不消费下一 token。
/// - 非 flag token（无前导 `-`）忽略（daemon 不接受位置参数）。
/// - 值形态的 flag 若缺值（末尾）取空串（对齐 Go `flag` 对空值的容忍：走各命令的空值分支）。
///
/// 返回 `key`（已剥 `-` 前缀）→ `value` 映射。重复 flag 后者覆盖（对齐 Go `flag` 语义）。
#[must_use]
pub fn parse_flags<I: Iterator<Item = String>>(
    argv: I,
    bool_flags: &[&str],
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut it = argv.skip(1);
    while let Some(tok) = it.next() {
        let key = tok.trim_start_matches('-');
        // 空 key（"--"/"-"）或无前导 `-`（位置参数）→ 忽略。
        if key.is_empty() || key == tok {
            continue;
        }
        if let Some((k, v)) = key.split_once('=') {
            map.insert(k.to_owned(), v.to_owned());
        } else if bool_flags.contains(&key) {
            map.insert(key.to_owned(), "true".to_owned());
        } else {
            // 值形态：消费下一 token（缺则空串）。
            let v = it.next().unwrap_or_default();
            map.insert(key.to_owned(), v);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> std::vec::IntoIter<String> {
        // 模拟真实 argv：首元素为程序名（被跳过）。
        let mut v = vec!["polaris-helper".to_owned()];
        v.extend(args.iter().map(|s| (*s).to_owned()));
        v.into_iter()
    }

    #[test]
    fn parses_value_flag_space_form() {
        let m = parse_flags(argv(&["--singbox", "/opt/sb"]), &[]);
        assert_eq!(m.get("singbox").map(String::as_str), Some("/opt/sb"));
    }

    #[test]
    fn parses_value_flag_eq_form() {
        let m = parse_flags(argv(&["--confdir=/etc/polaris"]), &[]);
        assert_eq!(m.get("confdir").map(String::as_str), Some("/etc/polaris"));
    }

    #[test]
    fn single_dash_accepted() {
        // Go flag 包 `-flag` 与 `--flag` 等价。
        let m = parse_flags(argv(&["-socket", "/run/x.sock"]), &[]);
        assert_eq!(m.get("socket").map(String::as_str), Some("/run/x.sock"));
    }

    #[test]
    fn bool_flag_does_not_consume_next() {
        let m = parse_flags(argv(&["--console", "--coredir", "/c"]), &["console"]);
        assert_eq!(m.get("console").map(String::as_str), Some("true"));
        assert_eq!(m.get("coredir").map(String::as_str), Some("/c"));
    }

    #[test]
    fn missing_flag_absent_from_map() {
        let m = parse_flags(argv(&["--singbox", "/opt/sb"]), &[]);
        assert!(!m.contains_key("support"));
    }

    #[test]
    fn positional_tokens_ignored() {
        let m = parse_flags(argv(&["positional", "--singbox", "/opt/sb"]), &[]);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("singbox").map(String::as_str), Some("/opt/sb"));
    }

    #[test]
    fn value_with_spaces_preserved() {
        // launchd argv 里 `--support "/Library/Application Support/Polaris"` 是单 token。
        let m = parse_flags(
            argv(&["--support=/Library/Application Support/Polaris"]),
            &[],
        );
        assert_eq!(
            m.get("support").map(String::as_str),
            Some("/Library/Application Support/Polaris")
        );
    }

    #[test]
    fn trailing_value_flag_gets_empty() {
        let m = parse_flags(argv(&["--coredir"]), &[]);
        assert_eq!(m.get("coredir").map(String::as_str), Some(""));
    }

    #[test]
    fn empty_argv_yields_empty_map() {
        let m = parse_flags(argv(&[]), &["console"]);
        assert!(m.is_empty());
    }
}
