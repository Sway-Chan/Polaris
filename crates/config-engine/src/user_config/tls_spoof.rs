//! TLS spoof 门控（上游 `shared/tls-spoof.ts` 1:1 移植）。
//!
//! sing-box 1.14 抗审查能力（tls.spoof/tls_spoof + spoof_method）：真握手前发伪造 ClientHello
//! 诱使 SNI 过滤中间盒放行。硬限界：需提权 / ARM64 不支持 / 拒 IP-literal SNI。
//!
//! outbound builder + route action rule 共用此门控（单一真值，杜绝两处漂移）。

#![forbid(unsafe_code)]

use crate::user_config::is_ip_literal;

/// 合法 spoof 方法枚举（sing-box 1.14 实证：仅这三个，其它 → `tls_spoof: unknown method`）。
pub const TLS_SPOOF_METHODS: &[&str] = &["wrong-ack", "wrong-md5", "wrong-timestamp"];

/// 方法值是否合法枚举（容错旁路写/旧配置）。
pub fn is_valid_tls_spoof_method(m: Option<&str>) -> bool {
    match m {
        Some(m) => TLS_SPOOF_METHODS.contains(&m),
        None => false,
    }
}

/// 当前 arch 是否支持 TLS spoof。**唯一判据非 ARM64**（内核仅 amd64 实现）。
/// 接受 process.arch 取值（'x64'/'arm64'/'arm'/'ia32'/...）。arm/arm64/aarch64 不支持。
pub fn is_tls_spoof_supported_arch(arch: Option<&str>) -> bool {
    let arch = match arch {
        Some(a) => a.to_ascii_lowercase(),
        None => return false, // 拿不到 arch 保守置不可用（避免误下发致 FATAL）
    };
    !matches!(arch.as_str(), "arm64" | "arm" | "aarch64")
}

/// 协议是否适用 TLS spoof（仅标准 sing-box TCP-TLS 栈有意义）。
/// 排除 hysteria2/tuic（TLS 在 QUIC 内，无 TCP ClientHello）/ naive（TLS 由 Cronet 自管）。
pub fn is_tls_spoof_supported_protocol(protocol: Option<&str>) -> bool {
    let p = protocol.unwrap_or("").to_ascii_lowercase();
    !matches!(p.as_str(), "hysteria2" | "tuic" | "naive")
}

/// TLS spoof 是否应下发（构建期统一门控，上游 `validateTlsSpoof`）。
///
/// outbound（protocol + serverSni）与 route action rule（均 None）共用。
/// 任一不满足即 false（不 emit，否则内核 FATAL/无效）。
///
/// `is_ip_literal_fn` 注入保持模块零跨依赖（与 TS 设计一致）。
#[allow(clippy::too_many_arguments)]
pub fn validate_tls_spoof(
    spoof_sni: Option<&str>,
    method: Option<&str>,
    arch: Option<&str>,
    is_ip_literal_fn: fn(&str) -> bool,
    protocol: Option<&str>,
    server_sni: Option<&str>,
) -> bool {
    let sni = spoof_sni.map(|s| s.trim()).unwrap_or("");
    if !is_valid_tls_spoof_method(method) {
        return false;
    }
    if !is_tls_spoof_supported_arch(arch) {
        return false;
    }
    if sni.is_empty() {
        return false;
    }
    if is_ip_literal_fn(sni) {
        return false;
    }
    if let Some(proto) = protocol {
        if !is_tls_spoof_supported_protocol(Some(proto)) {
            return false;
        }
    }
    if let Some(real_sni) = server_sni {
        if sni == real_sni {
            return false;
        }
        // 真 server_name 为 IP 字面量（address 是 IP 且未填 serverName 的回退）→ 真握手无 SNI，内核 init FATAL。
        if is_ip_literal_fn(real_sni) {
            return false;
        }
    }
    true
}

/// 便捷封装：用 crate 内置 is_ip_literal 作判定函数（多数调用方用此）。
pub fn validate_tls_spoof_default(
    spoof_sni: Option<&str>,
    method: Option<&str>,
    arch: Option<&str>,
    protocol: Option<&str>,
    server_sni: Option<&str>,
) -> bool {
    validate_tls_spoof(spoof_sni, method, arch, is_ip_literal, protocol, server_sni)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_enum_strict() {
        assert!(is_valid_tls_spoof_method(Some("wrong-ack")));
        assert!(is_valid_tls_spoof_method(Some("wrong-md5")));
        assert!(is_valid_tls_spoof_method(Some("wrong-timestamp")));
        assert!(!is_valid_tls_spoof_method(Some("wrong-foo")));
        assert!(!is_valid_tls_spoof_method(None));
    }

    #[test]
    fn arch_gate() {
        assert!(is_tls_spoof_supported_arch(Some("x64")));
        assert!(is_tls_spoof_supported_arch(Some("ia32")));
        assert!(!is_tls_spoof_supported_arch(Some("arm64")));
        assert!(!is_tls_spoof_supported_arch(Some("arm")));
        assert!(!is_tls_spoof_supported_arch(Some("aarch64")));
        assert!(!is_tls_spoof_supported_arch(None));
    }

    #[test]
    fn protocol_gate() {
        assert!(is_tls_spoof_supported_protocol(Some("vless")));
        assert!(is_tls_spoof_supported_protocol(Some("trojan")));
        assert!(!is_tls_spoof_supported_protocol(Some("hysteria2")));
        assert!(!is_tls_spoof_supported_protocol(Some("tuic")));
        assert!(!is_tls_spoof_supported_protocol(Some("naive")));
    }

    #[test]
    fn validate_happy_path() {
        assert!(validate_tls_spoof_default(
            Some("example.com"),
            Some("wrong-ack"),
            Some("x64"),
            None,
            None,
        ));
    }

    #[test]
    fn validate_rejects_ip_literal_sni() {
        assert!(!validate_tls_spoof_default(
            Some("1.2.3.4"),
            Some("wrong-ack"),
            Some("x64"),
            None,
            None,
        ));
    }

    #[test]
    fn validate_rejects_arm64() {
        assert!(!validate_tls_spoof_default(
            Some("example.com"),
            Some("wrong-ack"),
            Some("arm64"),
            None,
            None,
        ));
    }

    #[test]
    fn validate_rejects_empty_sni() {
        assert!(!validate_tls_spoof_default(
            Some(""),
            Some("wrong-ack"),
            Some("x64"),
            None,
            None,
        ));
        assert!(!validate_tls_spoof_default(
            None,
            Some("wrong-ack"),
            Some("x64"),
            None,
            None,
        ));
    }

    #[test]
    fn validate_outbound_server_sni_diff() {
        // 诱饵 ≠ 真 server_name。
        assert!(validate_tls_spoof_default(
            Some("decoy.com"),
            Some("wrong-md5"),
            Some("x64"),
            None,
            Some("real.com"),
        ));
        // 诱饵 == 真 server_name → 内核 FATAL `spoof must differ from server_name`。
        assert!(!validate_tls_spoof_default(
            Some("same.com"),
            Some("wrong-md5"),
            Some("x64"),
            None,
            Some("same.com"),
        ));
        // 真 server_name 为 IP 字面量 → FATAL。
        assert!(!validate_tls_spoof_default(
            Some("decoy.com"),
            Some("wrong-md5"),
            Some("x64"),
            None,
            Some("1.2.3.4"),
        ));
    }
}
