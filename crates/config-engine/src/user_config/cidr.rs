//! IPv4/IPv6 CIDR 算术（上游 `shared/ip.ts` parse/overlap/contains/subtract 1:1 移植）。
//!
//! v4 用 u32，v6 用 u128（Rust 原生替代 JS BigInt）。差集二分递归（CIDR 天然层级）。
//! 供 tun-route-exclude（Windows bypassLAN carve）+ route-builder（mesh 重叠检测）共用。

#![forbid(unsafe_code)]

use crate::user_config::ip::is_ipv4;

/// IPv4 CIDR → (网络地址, 前缀)。非法 → None。无 /n 视 /32。上游 `parseIpv4Cidr`。
fn parse_ipv4_cidr(cidr: &str) -> Option<(u32, u32)> {
    let t = cidr.trim();
    let (ip_part, prefix_part) = match t.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    if !is_ipv4(ip_part) {
        return None;
    }
    let prefix = match prefix_part {
        None => 32,
        Some(p) => p.parse::<u32>().ok().filter(|&n| n <= 32)?,
    };
    let o: Vec<u32> = ip_part
        .split('.')
        .map(|s| s.parse::<u32>().unwrap_or(999))
        .collect();
    if o.iter().any(|&x| x > 255) {
        return None;
    }
    let ip_int = (o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3];
    let mask = if prefix == 0 {
        0
    } else {
        0xFFFFFFFFu32 << (32 - prefix)
    };
    Some((ip_int & mask, prefix))
}

/// 两 IPv4 CIDR 是否相交（按较短前缀比对）。上游 `ipv4CidrsOverlap`。
pub fn ipv4_cidrs_overlap(a: &str, b: &str) -> bool {
    let (Some(pa), Some(pb)) = (parse_ipv4_cidr(a), parse_ipv4_cidr(b)) else {
        return false;
    };
    let min_prefix = pa.1.min(pb.1);
    if min_prefix == 0 {
        return true;
    }
    let mask = 0xFFFFFFFFu32 << (32 - min_prefix);
    (pa.0 & mask) == (pb.0 & mask)
}

/// v4 contains：outer 前缀 ≤ inner 且网络地址匹配。
fn v4_contains(outer: (u32, u32), inner: (u32, u32)) -> bool {
    if outer.1 > inner.1 {
        return false;
    }
    let mask = if outer.1 == 0 {
        0
    } else {
        0xFFFFFFFFu32 << (32 - outer.1)
    };
    (inner.0 & mask) == outer.0
}

/// v4 格式化。
fn fmt_v4(net: u32, prefix: u32) -> String {
    format!(
        "{}.{}.{}.{}/{}",
        (net >> 24) & 0xFF,
        (net >> 16) & 0xFF,
        (net >> 8) & 0xFF,
        net & 0xFF,
        prefix
    )
}

/// v4 差集二分：base ∖ carve。上游 `excludeV4`。
fn exclude_v4(base: (u32, u32), carve: (u32, u32)) -> Vec<(u32, u32)> {
    if !v4_contains(base, carve) {
        return if v4_contains(carve, base) {
            vec![]
        } else {
            vec![base]
        };
    }
    if base.1 == carve.1 {
        return vec![]; // 等价
    }
    let child_pfx = base.1 + 1;
    let block_size = 1u32 << (32 - child_pfx);
    let left = (base.0, child_pfx);
    let right = (base.0.wrapping_add(block_size), child_pfx);
    let mut out = exclude_v4(left, carve);
    out.extend(exclude_v4(right, carve));
    out
}

/// V6_FULL = 2^128 - 1。
const V6_FULL: u128 = !0u128;

/// IPv6 字面量（含 :: 压缩）→ u128。非法 → None。上游 `ipv6ToBigInt`。
fn ipv6_to_u128(addr: &str) -> Option<u128> {
    let a = addr.trim();
    if a.is_empty() || !a.contains(':') || !a.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
        return None;
    }
    let halves: Vec<&str> = a.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let head: Vec<&str> = if halves[0].is_empty() {
        vec![]
    } else {
        halves[0].split(':').collect()
    };
    let tail: Vec<&str> = if halves.len() == 2 && !halves[1].is_empty() {
        halves[1].split(':').collect()
    } else {
        vec![]
    };
    if halves.len() == 1 && head.len() != 8 {
        return None;
    }
    let fill = 8usize.saturating_sub(head.len() + tail.len());
    if halves.len() == 2 && fill < 1 {
        return None;
    }
    let mut groups: Vec<&str> = head;
    if halves.len() == 2 {
        groups.extend(std::iter::repeat_n("0", fill));
    }
    groups.extend(tail);
    if groups.len() != 8 {
        return None;
    }
    let mut v: u128 = 0;
    for g in groups {
        if g.is_empty() || g.len() > 4 {
            return None;
        }
        let n = u16::from_str_radix(g, 16).ok()?;
        v = (v << 16) | n as u128;
    }
    Some(v)
}

/// IPv6 CIDR → (网络地址, 前缀)。上游 `parseIpv6Cidr`。
fn parse_ipv6_cidr(cidr: &str) -> Option<(u128, u32)> {
    let t = cidr.trim();
    let (ip_part, prefix_part) = match t.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let prefix = match prefix_part {
        None => 128,
        Some(p) => p.parse::<u32>().ok().filter(|&n| n <= 128)?,
    };
    let v = ipv6_to_u128(ip_part)?;
    let mask = if prefix == 0 {
        0
    } else {
        V6_FULL ^ ((1u128 << (128 - prefix)) - 1)
    };
    Some((v & mask, prefix))
}

/// 两 IPv6 CIDR 是否相交。上游 `ipv6CidrsOverlap`。
pub fn ipv6_cidrs_overlap(a: &str, b: &str) -> bool {
    let (Some(pa), Some(pb)) = (parse_ipv6_cidr(a), parse_ipv6_cidr(b)) else {
        return false;
    };
    let min_prefix = pa.1.min(pb.1);
    if min_prefix == 0 {
        return true;
    }
    let mask = V6_FULL ^ ((1u128 << (128 - min_prefix)) - 1);
    (pa.0 & mask) == (pb.0 & mask)
}

/// 两 CIDR 是否相交（族自动分派，跨族 false）。上游 `cidrsOverlap`。
pub fn cidrs_overlap(a: &str, b: &str) -> bool {
    ipv4_cidrs_overlap(a, b) || ipv6_cidrs_overlap(a, b)
}

/// target 是否与候选集任一相交。上游 `cidrOverlapsAny`。
pub fn cidr_overlaps_any(target: &str, candidates: &[String]) -> bool {
    candidates.iter().any(|c| cidrs_overlap(target, c))
}

/// outer 是否包含 inner（方向性）。上游 `cidrContains`。
pub fn cidr_contains(outer: &str, inner: &str) -> bool {
    if let (Some(o4), Some(i4)) = (parse_ipv4_cidr(outer), parse_ipv4_cidr(inner)) {
        return v4_contains(o4, i4);
    }
    if let (Some(o6), Some(i6)) = (parse_ipv6_cidr(outer), parse_ipv6_cidr(inner)) {
        return v6_contains(o6, i6);
    }
    false
}

fn v6_contains(outer: (u128, u32), inner: (u128, u32)) -> bool {
    if outer.1 > inner.1 {
        return false;
    }
    let mask = if outer.1 == 0 {
        0
    } else {
        V6_FULL ^ ((1u128 << (128 - outer.1)) - 1)
    };
    (inner.0 & mask) == outer.0
}

fn fmt_v6(net: u128, prefix: u32) -> String {
    let groups: Vec<String> = (0..8)
        .rev()
        .map(|i| format!("{:x}", (net >> (i * 16)) & 0xFFFF))
        .collect();
    // 压缩最长连续 0 段（≥2 才压，RFC5952）。
    let mut best_start = None;
    let mut best_len = 0;
    let mut cur_start = None;
    let mut cur_len = 0;
    for (i, g) in groups.iter().enumerate() {
        if g == "0" {
            if cur_start.is_none() {
                cur_start = Some(i);
            }
            cur_len += 1;
            if cur_len > best_len {
                best_len = cur_len;
                best_start = cur_start;
            }
        } else {
            cur_start = None;
            cur_len = 0;
        }
    }
    let addr = if best_len < 2 {
        groups.join(":")
    } else {
        let bs = best_start.unwrap();
        format!(
            "{}::{}",
            groups[..bs].join(":"),
            groups[bs + best_len..].join(":")
        )
    };
    format!("{addr}/{prefix}")
}

fn exclude_v6(base: (u128, u32), carve: (u128, u32)) -> Vec<(u128, u32)> {
    if !v6_contains(base, carve) {
        return if v6_contains(carve, base) {
            vec![]
        } else {
            vec![base]
        };
    }
    if base.1 == carve.1 {
        return vec![];
    }
    let child_pfx = base.1 + 1;
    let block_size = 1u128 << (128 - child_pfx);
    let left = (base.0, child_pfx);
    let right = (base.0.wrapping_add(block_size), child_pfx);
    let mut out = exclude_v6(left, carve);
    out.extend(exclude_v6(right, carve));
    out
}

/// CIDR 差集（族感知）：(∪base) ∖ (∪carve)。上游 `subtractCidrs`。
pub fn subtract_cidrs(base: &[String], carve: &[String]) -> Vec<String> {
    let carve_v4: Vec<(u32, u32)> = carve.iter().filter_map(|c| parse_ipv4_cidr(c)).collect();
    let carve_v6: Vec<(u128, u32)> = carve.iter().filter_map(|c| parse_ipv6_cidr(c)).collect();

    let mut out = Vec::new();
    for b in base {
        if let Some(b4) = parse_ipv4_cidr(b) {
            let mut pieces = vec![b4];
            for cv in &carve_v4 {
                let mut next = Vec::new();
                for p in pieces {
                    next.extend(exclude_v4(p, *cv));
                }
                pieces = next;
                if pieces.is_empty() {
                    break;
                }
            }
            for (net, pfx) in pieces {
                out.push(fmt_v4(net, pfx));
            }
            continue;
        }
        if let Some(b6) = parse_ipv6_cidr(b) {
            let mut pieces = vec![b6];
            for cv in &carve_v6 {
                let mut next = Vec::new();
                for p in pieces {
                    next.extend(exclude_v6(p, *cv));
                }
                pieces = next;
                if pieces.is_empty() {
                    break;
                }
            }
            for (net, pfx) in pieces {
                out.push(fmt_v6(net, pfx));
            }
            continue;
        }
        out.push(b.clone()); // 无法解析原样保留
    }
    out
}

/// 按"与 ranges 任一相交"分两组。上游 `partitionCidrsByOverlap`。
pub fn partition_cidrs_by_overlap(
    cidrs: &[String],
    ranges: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut overlapping = Vec::new();
    let mut disjoint = Vec::new();
    for c in cidrs {
        if !ranges.is_empty() && cidr_overlaps_any(c, ranges) {
            overlapping.push(c.clone());
        } else {
            disjoint.push(c.clone());
        }
    }
    (overlapping, disjoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_overlap() {
        assert!(ipv4_cidrs_overlap("192.168.0.0/16", "192.168.1.0/24"));
        assert!(ipv4_cidrs_overlap("10.0.0.0/8", "10.1.2.0/24"));
        assert!(!ipv4_cidrs_overlap("10.0.0.0/8", "192.168.0.0/16"));
        assert!(ipv4_cidrs_overlap("0.0.0.0/0", "1.2.3.0/24")); // 全覆盖
    }

    #[test]
    fn v4_contains() {
        assert!(cidr_contains("10.0.0.0/8", "10.1.2.0/24"));
        assert!(!cidr_contains("10.1.2.0/24", "10.0.0.0/8"));
        assert!(!cidr_contains("10.0.0.0/8", "192.168.0.0/16"));
    }

    #[test]
    fn v4_subtract_basic() {
        // 10.0.0.0/8 ∖ 10.1.0.0/16 → 10.0.0.0/16 + 10.2.0.0/15...10.128.0.0/9
        let result = subtract_cidrs(&["10.0.0.0/8".to_string()], &["10.1.0.0/16".to_string()]);
        assert!(result.len() > 1);
        // 验证不含 10.1.0.0/16 且并集覆盖原 /8 减该段。
        assert!(!result.iter().any(|c| c == "10.1.0.0/16"));
    }

    #[test]
    fn v4_subtract_equal() {
        let result = subtract_cidrs(&["10.0.0.0/8".to_string()], &["10.0.0.0/8".to_string()]);
        assert!(result.is_empty());
    }

    #[test]
    fn v4_subtract_no_overlap() {
        let result = subtract_cidrs(&["10.0.0.0/8".to_string()], &["192.168.0.0/16".to_string()]);
        assert_eq!(result, vec!["10.0.0.0/8".to_string()]);
    }

    #[test]
    fn v6_overlap() {
        assert!(ipv6_cidrs_overlap("fc00::/7", "fd00::/8"));
        assert!(ipv6_cidrs_overlap("2001:db8::/32", "2001:db8:1::/48"));
        assert!(!ipv6_cidrs_overlap("fc00::/7", "2001::/16"));
    }

    #[test]
    fn v6_subtract() {
        let result = subtract_cidrs(&["fc00::/7".to_string()], &["fd00::/8".to_string()]);
        assert!(!result.is_empty());
    }

    #[test]
    fn cross_family_disjoint() {
        assert!(!cidrs_overlap("10.0.0.0/8", "fc00::/7"));
    }

    #[test]
    fn partition_by_overlap() {
        let cidrs = vec![
            "10.0.0.0/8".to_string(),
            "192.168.0.0/16".to_string(),
            "1.2.3.0/24".to_string(),
        ];
        let ranges = vec!["192.168.0.0/16".to_string()];
        let (overlapping, disjoint) = partition_cidrs_by_overlap(&cidrs, &ranges);
        assert_eq!(overlapping, vec!["192.168.0.0/16".to_string()]);
        assert_eq!(disjoint.len(), 2);
    }
}
