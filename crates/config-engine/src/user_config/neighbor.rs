//! LAN 设备识别簇校验（上游 `shared/neighbor.ts` 1:1 移植）。
//!
//! sing-box 1.14 邻居解析：source_mac_address/source_hostname(route+DNS rule)、
//! neighbor_domain(local DNS)、include/exclude_mac_address(TUN)。平台门控 + 值校验。
//! 正则手写（避免 regex crate 依赖），语义对齐 Go net.ParseMAC + HOSTNAME_RE。

#![forbid(unsafe_code)]

/// MAC（EUI-48）合法 —— 对齐 Go net.ParseMAC 三种 6-octet 写法：
/// 冒号 `00:11:22:33:44:55` / 连字符 `00-11-22-33-44-55` / Cisco 点分 `0011.2233.4455`。
/// 大小写不敏感。拒无分隔符/段数位数错/非 hex/混用分隔符。
pub fn is_valid_mac_address(value: Option<&str>) -> bool {
    let v = value.unwrap_or("").trim();
    if v.is_empty() {
        return false;
    }
    is_mac_colon(v) || is_mac_dash(v) || is_mac_cisco(v)
}

/// 全冒号 6 段 hex：`[0-9a-fA-F]{2}(:[0-9a-fA-F]{2}){5}`。
fn is_mac_colon(v: &str) -> bool {
    let parts: Vec<&str> = v.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| is_hex_pair(p))
}

/// 全连字符 6 段 hex：`[0-9a-fA-F]{2}(-[0-9a-fA-F]{2}){5}`。
fn is_mac_dash(v: &str) -> bool {
    let parts: Vec<&str> = v.split('-').collect();
    parts.len() == 6 && parts.iter().all(|p| is_hex_pair(p))
}

/// Cisco 点分 3 组 4 hex：`[0-9a-fA-F]{4}\.[0-9a-fA-F]{4}\.[0-9a-fA-F]{4}`。
fn is_mac_cisco(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| is_hex_quad(p))
}

fn is_hex_pair(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_hex_quad(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 源设备主机名合法（source_hostname）。Polaris HOSTNAME_RE：
/// `[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)*`。
/// 单/多标签 DNS 主机名形状，标签不以连字符首尾、≤63，总长 ≤253。
pub fn is_valid_source_hostname(value: Option<&str>) -> bool {
    let v = value.unwrap_or("").trim();
    if v.is_empty() || v.len() > 253 {
        return false;
    }
    is_hostname_shape(v)
}

/// HOSTNAME_RE 等价手写：点分标签，每标签字母数字开头结尾、中间可连字符、1..=63 字符。
fn is_hostname_shape(v: &str) -> bool {
    let labels: Vec<&str> = v.split('.').collect();
    if labels.is_empty() {
        return false;
    }
    labels.iter().all(|label| is_valid_label(label))
}

fn is_valid_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    // 首尾须字母数字。
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    // 中间允许字母数字+连字符（首尾已验，中间字符集放宽）。
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
}

/// neighbor_domain 单条目归一化：每条须以 `.` 开头（内核 init 要求）。
/// 去全部前导点后重加一个；纯点 → `.`；空 → None。
pub fn normalize_neighbor_domain(value: Option<&str>) -> Option<String> {
    let mut v = value.unwrap_or("").trim().to_string();
    if v.is_empty() {
        return None;
    }
    // 去全部前导点。
    v = v.trim_start_matches('.').to_string();
    if v.is_empty() {
        return Some(".".to_string()); // 纯点 → `.`（匹配任意单标签名）
    }
    Some(format!(".{v}"))
}

/// neighbor_domain 归一化后是否合法。上游 `isValidNeighborDomain`。
pub fn is_valid_neighbor_domain(value: Option<&str>) -> bool {
    let norm = match normalize_neighbor_domain(value) {
        Some(n) => n,
        None => return false,
    };
    if norm == "." {
        return true;
    }
    // 去前导点后按域名后缀形状校验。
    is_hostname_shape(&norm[1..])
}

/// 当前平台是否支持 source_mac_address/source_hostname（route+DNS rule）。
/// 内核仅 Linux/macOS。win32 不支持（neighbor_resolver 无 windows 实现）。
pub fn is_source_device_match_supported(platform: &str) -> bool {
    matches!(platform.to_ascii_lowercase().as_str(), "linux" | "darwin")
}

/// 当前平台是否支持 TUN include/exclude_mac_address。
/// 内核硬限界：仅 Linux（且需 auto_route+auto_redirect+nftables）。
pub fn is_tun_mac_filter_supported(platform: &str) -> bool {
    platform.eq_ignore_ascii_case("linux")
}

/// TUN MAC 过滤模式（include/exclude 互斥）。上游 `TunMacFilterMode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunMacFilterMode {
    Include,
    Exclude,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_colon_valid() {
        assert!(is_valid_mac_address(Some("00:11:22:33:44:55")));
        assert!(is_valid_mac_address(Some("aa:bb:cc:dd:ee:ff")));
        assert!(is_valid_mac_address(Some("AA:BB:CC:DD:EE:FF")));
    }

    #[test]
    fn mac_dash_valid() {
        assert!(is_valid_mac_address(Some("00-11-22-33-44-55")));
    }

    #[test]
    fn mac_cisco_valid() {
        assert!(is_valid_mac_address(Some("0011.2233.4455")));
        assert!(is_valid_mac_address(Some("aabb.ccdd.eeff")));
    }

    #[test]
    fn mac_invalid() {
        assert!(!is_valid_mac_address(Some("001122334455"))); // 无分隔符
        assert!(!is_valid_mac_address(Some("00:11:22:33:44"))); // 段数错
        assert!(!is_valid_mac_address(Some("00:11:22:33:44:55:66"))); // 7 段
        assert!(!is_valid_mac_address(Some("zz:11:22:33:44:55"))); // 非 hex
        assert!(!is_valid_mac_address(Some("00-11:22-33:44:55"))); // 混用分隔符
        assert!(!is_valid_mac_address(None));
        assert!(!is_valid_mac_address(Some("")));
    }

    #[test]
    fn hostname_valid() {
        assert!(is_valid_source_hostname(Some("nas")));
        assert!(is_valid_source_hostname(Some("nas.lan")));
        assert!(is_valid_source_hostname(Some("my-host")));
        assert!(is_valid_source_hostname(Some("a.b.c")));
    }

    #[test]
    fn hostname_invalid() {
        assert!(!is_valid_source_hostname(Some("-host"))); // 连字符开头
        assert!(!is_valid_source_hostname(Some("host-"))); // 连字符结尾
        assert!(!is_valid_source_hostname(Some(""))); // 空
        assert!(!is_valid_source_hostname(Some("a".repeat(254).as_str()))); // 过长
    }

    #[test]
    fn neighbor_domain_normalize() {
        assert_eq!(
            normalize_neighbor_domain(Some("lan")).as_deref(),
            Some(".lan")
        );
        assert_eq!(
            normalize_neighbor_domain(Some(".lan")).as_deref(),
            Some(".lan")
        );
        assert_eq!(
            normalize_neighbor_domain(Some("..lan")).as_deref(),
            Some(".lan")
        ); // 多点收敛
        assert_eq!(normalize_neighbor_domain(Some(".")).as_deref(), Some(".")); // 纯点
        assert_eq!(normalize_neighbor_domain(Some("")).as_deref(), None);
        assert_eq!(normalize_neighbor_domain(None), None);
    }

    #[test]
    fn neighbor_domain_valid() {
        assert!(is_valid_neighbor_domain(Some(".lan")));
        assert!(is_valid_neighbor_domain(Some("."))); // 单点
        assert!(is_valid_neighbor_domain(Some(".home.arpa"))); // 多标签
        assert!(is_valid_neighbor_domain(Some("lan"))); // 归一化加前导点 → .lan 合法
        assert!(!is_valid_neighbor_domain(Some("")));
    }

    #[test]
    fn platform_source_device() {
        assert!(is_source_device_match_supported("linux"));
        assert!(is_source_device_match_supported("darwin"));
        assert!(!is_source_device_match_supported("win32"));
    }

    #[test]
    fn platform_tun_mac() {
        assert!(is_tun_mac_filter_supported("linux"));
        assert!(!is_tun_mac_filter_supported("darwin"));
        assert!(!is_tun_mac_filter_supported("win32"));
    }
}
