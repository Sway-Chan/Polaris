//! GFW DNS 投毒 decoy IP 集 —— 上游 `shared/gfw-decoy-ips.ts` 1:1 移植。
//!
//! GFW 对被墙域名注入的伪造应答指向**有限已知 IP 段**（Facebook/Twitter/Dropbox + 历史单点）。
//! 竞速层命中这些 IP 的应答判 POISONED、弃之不抢跑（first-clean-wins），令干净上游胜出。
//! 纯字节匹配、零 I/O、可逐字节单测。
//!
//! 来源：GFWatch（USENIX Security '21 "How Great is the Great Firewall?"）+ 社区维护清单
//! + 上游侧本机实测样本（31.13.95.169 / 185.45.7.x / 45.114.11.25 / 157.240.17.35 /
//!   202.160.128.16 / 2a03:2880::/29）。
//!
//! 定位：**防御网非权威** —— 漏一个 decoy 只是不过滤（FakeIP 影子规则才是治本），命中即弃是 fail-safe。
//! 池会演化，随 geo 资源同节奏维护（POISONED 日志计数提供漂移信号）。
//!
//! **为什么内置表写成字面字节而不是解析字符串 CIDR**：TS 版在模块初始化时 `map(parseV4)`，Rust 侧同等做法
//! 要么引 `once_cell`/`LazyLock` 运行期解析（多一层无收益的启动逻辑），要么手写 const fn 解析器。
//! 直接写网络字节是**零运行期成本 + 编译期即定型**，且每条保留原 CIDR 文本注释，可读性不损失。
//!
//! ## 运行期可覆盖（[`DecoySet`]）
//!
//! 内置表是**编译期定型**的，改一条要重新发版；而上面「池会演化」那句要求它能跟 geo 资源同节奏更新。
//! 故本模块同时给出可在运行期构造的 [`DecoySet`]：[`DecoySet::builtin`] 即内置表，
//! [`DecoySet::parse`] 从文本清单（一行一个 CIDR）构造。竞速层消费的是**注入进来的** `&DecoySet`
//! （见 [`crate::race::race_forward`]），不再直接读常量 —— 保住本模块「纯函数、零 I/O」的定位，
//! 读文件/挑路径一律留给调用方。
//!
//! **覆盖语义是「替换」不是「并集」**：这张表两个方向都会错 —— 既会漏新 decoy，也会**误杀**
//! （`31.13.0.0/16` 就是 Facebook 的真实段，节点真托管在那儿会被错判 POISONED）。并集只能修前者、
//! 永远修不了后者，那就等于把误杀写死。替换让两个方向都可修。
//!
//! **但「解析结果为空」回落内置**（[`ParsedDecoySet::fell_back`]）：空清单远比「用户想关掉过滤」
//! 更可能是下载被截断/文件被清空。要真关就填一条匹配不到的段，别用空文件表达 —— 空文件在这里
//! 是**故障**而非意图。

#![forbid(unsafe_code)]

/// 一条 decoy 前缀：`(网络字节, 前缀位数)`。
type DecoyCidr<const N: usize> = ([u8; N], u8);

/// Facebook（含 face:b00c v6）/ Twitter / Dropbox 段 + 历史单点 + 实测样本。
const V4_DECOY_CIDRS: &[DecoyCidr<4>] = &[
    ([31, 13, 0, 0], 16),     // 31.13.0.0/16
    ([66, 220, 0, 0], 16),    // 66.220.0.0/16
    ([69, 63, 0, 0], 16),     // 69.63.0.0/16
    ([69, 171, 0, 0], 16),    // 69.171.0.0/16
    ([157, 240, 0, 0], 16),   // 157.240.0.0/16
    ([173, 252, 0, 0], 16),   // 173.252.0.0/16
    ([179, 60, 192, 0], 22),  // 179.60.192.0/22
    ([185, 60, 216, 0], 22),  // 185.60.216.0/22
    ([104, 244, 40, 0], 21),  // 104.244.40.0/21  Twitter
    ([199, 59, 148, 0], 22),  // 199.59.148.0/22  Twitter
    ([202, 160, 128, 0], 22), // 202.160.128.0/22 实测 202.160.128.16
    ([162, 125, 0, 0], 16),   // 162.125.0.0/16   Dropbox
    ([185, 45, 7, 0], 24),    // 185.45.7.0/24    实测 chat.openai 185.45.7.x
    ([45, 114, 11, 0], 24),   // 45.114.11.0/24   实测 gemini 45.114.11.25
    ([8, 7, 198, 45], 32),    // 历史单点
    ([46, 82, 174, 68], 32),  // 历史单点
    ([59, 24, 3, 173], 32),   // 历史单点
    ([93, 46, 8, 89], 32),    // 历史单点
    ([203, 98, 7, 65], 32),   // 历史单点
];

/// Facebook（face:b00c）v6 段：`2a03:2880::/29`。
const V6_DECOY_CIDRS: &[DecoyCidr<16>] = &[(
    [0x2a, 0x03, 0x28, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    29,
)];

/// `ip`（网络字节）是否落在 `cidr` 内。上游 `inCidr`。
fn in_cidr<const N: usize>(ip: &[u8], cidr: &DecoyCidr<N>) -> bool {
    if ip.len() != N {
        return false;
    }
    let mut bits = cidr.1;
    for (b, want) in ip.iter().zip(cidr.0.iter()) {
        if bits == 0 {
            break;
        }
        let take = bits.min(8);
        let mask: u8 = if take == 8 {
            0xff
        } else {
            (0xffu16 << (8 - take)) as u8
        };
        if b & mask != want & mask {
            return false;
        }
        bits -= take;
    }
    true
}

/// `ip` 是否命中 `cidrs` 里任一段。抽出来是为了让内置表与 [`DecoySet`] 走同一份匹配逻辑
/// —— 两处各写一遍 `iter().any()` 就是第二份真值源，改了一处忘另一处不会转红。
fn any_hit<const N: usize>(ip: &[u8], cidrs: &[DecoyCidr<N>]) -> bool {
    cidrs.iter().any(|c| in_cidr(ip, c))
}

/// `ip` 原始网络字节（4=IPv4 / 16=IPv6）是否命中任一**内置** GFW decoy 段。
/// 非 4/16 字节 → `false`（不误杀）。上游 `isDecoyIp`。
///
/// 运行期可覆盖的版本见 [`DecoySet::contains`]；竞速层用的是后者。
#[must_use]
pub fn is_decoy_ip(ip: &[u8]) -> bool {
    match ip.len() {
        4 => any_hit(ip, V4_DECOY_CIDRS),
        16 => any_hit(ip, V6_DECOY_CIDRS),
        _ => false,
    }
}

/// 运行期 decoy 段集合。默认 [`DecoySet::builtin`]，可由文本清单替换（见模块文档「运行期可覆盖」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoySet {
    v4: Vec<DecoyCidr<4>>,
    v6: Vec<DecoyCidr<16>>,
}

/// [`DecoySet::parse`] 的结果。**坏行不致命**：能认的收下、认不出的逐条报出去，由调用方决定怎么说。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDecoySet {
    /// 解析所得（`fell_back` 为 true 时这里已是内置表）。
    pub set: DecoySet,
    /// 被跳过的行：`(行号从 1 起, 原文去空白)`。调用方按需截断上报，本层不擅自丢。
    pub bad_lines: Vec<(usize, String)>,
    /// true = 解析出零条有效段 ⇒ `set` 已回落内置表（空清单当故障，见模块文档）。
    pub fell_back: bool,
}

impl Default for DecoySet {
    fn default() -> Self {
        Self::builtin()
    }
}

impl DecoySet {
    /// 内置表（编译期定型的那份）。
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            v4: V4_DECOY_CIDRS.to_vec(),
            v6: V6_DECOY_CIDRS.to_vec(),
        }
    }

    /// 从文本清单解析。一行一条，`#` / `//` 起的行与空行跳过；行内 `#` 之后视为注释。
    ///
    /// 接受 `31.13.0.0/16`、`2a03:2880::/29`，也接受**裸 IP**（补 /32 或 /128 —— 内置表本就有
    /// 历史单点，让清单能照抄）。前缀位数越界（v4 >32 / v6 >128）算坏行。
    ///
    /// 主机位非零**不算错**（`in_cidr` 两侧同时掩码，`8.7.198.45/24` 与 `8.7.198.0/24` 等价），
    /// 拒它只会让手写清单无谓地失败。
    #[must_use]
    pub fn parse(text: &str) -> ParsedDecoySet {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        let mut bad_lines = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            // 行内 `#` 之后是注释。IPv6 用 `:` 不用 `#`，按 `#` 切不会伤到地址。
            let line = raw.split('#').next().unwrap_or_default().trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            match parse_cidr_line(line) {
                Some(Cidr::V4(c)) => v4.push(c),
                Some(Cidr::V6(c)) => v6.push(c),
                None => bad_lines.push((idx + 1, line.to_string())),
            }
        }
        let fell_back = v4.is_empty() && v6.is_empty();
        let set = if fell_back {
            Self::builtin()
        } else {
            Self { v4, v6 }
        };
        ParsedDecoySet {
            set,
            bad_lines,
            fell_back,
        }
    }

    /// 段条数 `(v4, v6)`。供调用方把「用了几条」如实打进日志 —— 不打就没人知道覆盖生效没生效。
    #[must_use]
    pub fn len(&self) -> (usize, usize) {
        (self.v4.len(), self.v6.len())
    }

    /// `ip` 原始网络字节（4 / 16）是否命中本集合任一段。非 4/16 字节 → `false`（不误杀）。
    #[must_use]
    pub fn contains(&self, ip: &[u8]) -> bool {
        match ip.len() {
            4 => any_hit(ip, &self.v4),
            16 => any_hit(ip, &self.v6),
            _ => false,
        }
    }
}

/// `parse_cidr_line` 的两种产物（v4 / v6 前缀长度不同，无法共用一个类型）。
enum Cidr {
    V4(DecoyCidr<4>),
    V6(DecoyCidr<16>),
}

/// 单行 → 一条前缀。认不出返回 `None`（调用方记坏行）。用 std 的 `IpAddr` 解析，不引第三方。
fn parse_cidr_line(line: &str) -> Option<Cidr> {
    let (addr_part, bits_part) = match line.split_once('/') {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (line, None), // 裸 IP → 满前缀
    };
    let addr: std::net::IpAddr = addr_part.parse().ok()?;
    match addr {
        std::net::IpAddr::V4(v4) => {
            let bits = match bits_part {
                Some(b) => b.parse::<u8>().ok()?,
                None => 32,
            };
            (bits <= 32).then(|| Cidr::V4((v4.octets(), bits)))
        }
        std::net::IpAddr::V6(v6) => {
            let bits = match bits_part {
                Some(b) => b.parse::<u8>().ok()?,
                None => 128,
            };
            (bits <= 128).then(|| Cidr::V6((v6.octets(), bits)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_decoy_samples_are_matched() {
        // 上游侧本机实测到的真实投毒样本，逐条锁死。
        assert!(is_decoy_ip(&[31, 13, 95, 169]));
        assert!(is_decoy_ip(&[157, 240, 17, 35]));
        assert!(is_decoy_ip(&[185, 45, 7, 12]));
        assert!(is_decoy_ip(&[45, 114, 11, 25]));
        assert!(is_decoy_ip(&[202, 160, 128, 16]));
        assert!(is_decoy_ip(&[8, 7, 198, 45]), "历史单点 /32");
    }

    #[test]
    fn clean_ips_are_not_matched() {
        assert!(!is_decoy_ip(&[1, 1, 1, 1]));
        assert!(!is_decoy_ip(&[223, 5, 5, 5]));
        assert!(
            !is_decoy_ip(&[104, 21, 0, 1]),
            "104.21 不在 104.244.40.0/21"
        );
        assert!(!is_decoy_ip(&[8, 7, 198, 46]), "/32 邻居不命中");
    }

    #[test]
    fn prefix_boundaries_are_exact() {
        // 179.60.192.0/22 覆盖 .192-.195，不覆盖 .196。
        assert!(is_decoy_ip(&[179, 60, 195, 255]));
        assert!(!is_decoy_ip(&[179, 60, 196, 0]));
        // 185.45.7.0/24 不外溢到 185.45.8.x。
        assert!(is_decoy_ip(&[185, 45, 7, 255]));
        assert!(!is_decoy_ip(&[185, 45, 8, 0]));
    }

    #[test]
    fn v6_prefix_29_matches_facebook_range() {
        let mut ip = [0u8; 16];
        ip[0] = 0x2a;
        ip[1] = 0x03;
        ip[2] = 0x28;
        ip[3] = 0x80;
        ip[15] = 1;
        assert!(is_decoy_ip(&ip));
        // /29 的边界：第 4 字节高 5 位须一致（0x28 → 0b00101000，前 5 位 00101）。
        let mut out = ip;
        out[2] = 0x30; // 0b00110000 → 前 5 位不同
        assert!(!is_decoy_ip(&out));
    }

    #[test]
    fn non_ip_lengths_never_match() {
        assert!(!is_decoy_ip(&[]));
        assert!(!is_decoy_ip(&[31, 13, 95]));
        assert!(!is_decoy_ip(&[0u8; 8]));
    }

    // ── DecoySet（运行期可覆盖）─────────────────────────────────────────

    /// 内置集合与自由函数必须逐样本一致 —— 否则 `any_hit` 抽取就没抽干净，两条路会漂。
    #[test]
    fn builtin_set_agrees_with_free_function() {
        let s = DecoySet::builtin();
        for ip in [
            [31u8, 13, 95, 169],
            [157, 240, 17, 35],
            [8, 7, 198, 45],
            [1, 1, 1, 1],
            [179, 60, 196, 0],
        ] {
            assert_eq!(s.contains(&ip), is_decoy_ip(&ip), "{ip:?}");
        }
        assert_eq!(s.len(), (19, 1), "内置表条数变了就要同步本断言与文档");
    }

    #[test]
    fn parse_accepts_cidr_bare_ip_comments_and_blank_lines() {
        let p = DecoySet::parse(
            "\n# 注释行\n// 另一种注释\n1.2.0.0/16\n  9.9.9.9  # 行内注释，裸 IP 补 /32\n2a03:2880::/29\n",
        );
        assert!(!p.fell_back);
        assert!(p.bad_lines.is_empty(), "{:?}", p.bad_lines);
        assert_eq!(p.set.len(), (2, 1));
        assert!(p.set.contains(&[1, 2, 3, 4]));
        assert!(p.set.contains(&[9, 9, 9, 9]));
        assert!(!p.set.contains(&[9, 9, 9, 10]), "裸 IP 必须是 /32 不是整段");
        let mut v6 = [0u8; 16];
        v6[0] = 0x2a;
        v6[1] = 0x03;
        v6[2] = 0x28;
        v6[3] = 0x80;
        assert!(p.set.contains(&v6));
    }

    /// **替换不是并集**：覆盖后内置段必须失效，否则误杀永远修不掉（模块文档的核心不变式）。
    #[test]
    fn parsed_set_replaces_builtin_rather_than_unions() {
        let p = DecoySet::parse("1.2.0.0/16");
        assert!(!p.fell_back);
        assert!(
            !p.set.contains(&[31, 13, 95, 169]),
            "内置 31.13/16 必须被替换掉，并集会把误杀写死"
        );
    }

    /// 空清单 / 全坏行 → 回落内置（空文件当故障，不当「关掉过滤」）。
    #[test]
    fn empty_or_all_bad_falls_back_to_builtin() {
        for text in ["", "   \n\n# 只有注释\n", "not-an-ip\n1.2.3.4/99\n"] {
            let p = DecoySet::parse(text);
            assert!(p.fell_back, "text={text:?}");
            assert_eq!(p.set, DecoySet::builtin(), "text={text:?}");
            assert!(p.set.contains(&[31, 13, 95, 169]));
        }
    }

    #[test]
    fn bad_lines_are_reported_with_line_numbers_and_good_ones_kept() {
        let p = DecoySet::parse("1.2.0.0/16\nnope\n3.4.0.0/16\n1.2.3.4/33\n");
        assert!(!p.fell_back);
        assert_eq!(p.set.len(), (2, 0));
        assert_eq!(
            p.bad_lines,
            vec![(2, "nope".to_string()), (4, "1.2.3.4/33".to_string())],
            "行号从 1 起，且坏行不能吞掉后面的好行"
        );
    }

    /// 主机位非零不算错（`in_cidr` 两侧同时掩码）—— 拒它只会让手写清单无谓失败。
    #[test]
    fn host_bits_set_is_accepted_and_masked() {
        let p = DecoySet::parse("8.7.198.45/24");
        assert!(p.bad_lines.is_empty());
        assert!(p.set.contains(&[8, 7, 198, 1]));
    }
}
