//! 帧编解码 + 安全约束（移植自 Polaris Go helper 的 line framing 与白名单校验）。
//!
//! ## 帧结构（逐平台对照）
//!
//! Polaris helper 用 **line-based 文本协议**（Go `bufio.Reader.ReadString('\n')`，每行以 `\n` 结尾）。
//! 三平台的帧差异**仅在首行**：
//! - mac/win：行1 = `<token>`（鉴权），行2 = `<command>`，行3.. = 参数行。
//! - linux：行1 = `<command>`（无 token 行，鉴权经 SO_PEERCRED 在 socket 层完成）。
//!
//! [`Framer`] 按 [`Platform`] 决定是否在头部加 token 行，让上层 [`Request`](crate::Request) 与鉴权机制解耦。
//!
//! ## 安全约束（逐行移植 Go 源的白名单函数）
//!
//! - [`is_valid_cidr`]：移植自 Go `net.ParseCIDR`（防 route-add/del 参数注入）。
//! - [`is_mac_iface_allowed`]：移植自 `helper.go:255-272`（polaris-ts/polaris-wg/utunN）。
//! - [`is_win_iface_allowed`]：移植自 `helper-win/helper.go:50-60`（polaris-* 前缀）。
//!
//! 这些校验在 helper 侧与（可选的）client 侧**都跑** —— 双侧一致校验是纵深防御（client 早失败省一轮 RTT，
//! helper 侧是权威边界，绝不依赖 client）。

use std::net::{IpAddr, Ipv4Addr};

use crate::{Platform, Request};

/// 单帧最大字节数（防御恶意客户端发超长行耗尽内存；Go `bufio.Reader` 默认无上限，本协议加保守护栏）。
///
/// 选 64KB：start 的 cfg 路径 + install-core 的 hash 远小于此；sing-box config 不经此协议传（走文件路径）。
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// 单连接读超时（秒）—— 移植自 Go `conn.SetReadDeadline(time.Now().Add(5 * time.Second))`
///（`helper.go:401` / `helper-win/helper.go:165` / `helper-linux/helper.go:335`）。
///
/// 防止无 token 进程连上后不发数据耗尽 fd/goroutine，或持 token 客户端发一半卡死、在 mu.Lock() 之后阻塞读
/// → 永久持锁拖垮整个 helper。
pub const READ_TIMEOUT_SECS: u64 = 5;

/// 把一个 [`Request`] 编码为完整帧的行序列（每行不含 `\n`，由调用方 IO 层统一加 `\n`）。
///
/// 返回的 Vec 即将写入 socket/pipe 的全部行：
/// - mac/win：`[token, command, arg1, arg2, ...]`
/// - linux：`[command, arg1, arg2, ...]`（无 token 行）
///
/// `token` 在 linux 下被忽略（传 `""` 即可）。
#[must_use]
pub fn encode_frame(platform: Platform, token: &str, req: &Request) -> Vec<String> {
    let mut lines = Vec::new();
    // mac/win：行1 = token（Go handle() 首个 readLine；linux 经 SO_PEERCRED 鉴权，无此行）
    if platform != Platform::Linux {
        lines.push(token.to_owned());
    }
    // 命令行
    lines.push(req.command_name().to_owned());
    // 参数行（命令特定）
    req.write_args(&mut lines);
    lines
}

/// 把帧行序列拼接为单字节串（每行尾加 `\n`），用于直接 write 到 socket/pipe。
///
/// 调用方负责控制写超时（本函数纯内存拼装，不阻塞）。
#[must_use]
pub fn frame_to_bytes(lines: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for l in lines {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    }
    out
}

/// 便利：一步编码为字节串（`encode_frame` + `frame_to_bytes`）。
#[must_use]
pub fn encode(platform: Platform, token: &str, req: &Request) -> Vec<u8> {
    frame_to_bytes(&encode_frame(platform, token, req))
}

// ===== 安全约束（逐行移植 Go 源白名单函数）=====

/// 校验 IPv4/IPv6 CIDR（移植自 Go `net.ParseCIDR`，`helper.go:470` / `helper-win/helper.go:231`）。
///
/// 非法 CIDR 在 Go 源里被**静默跳过**（`continue`）—— 本函数返回 false 等价语义，helper/client 侧据此过滤。
/// 接受 `a.b.c.d/N`（N ∈ 0..=32）与 `::/N`（N ∈ 0..=128）两族。
///
/// CIDR 的 `/` 前缀由本函数剥离，**地址本体交 stdlib [`IpAddr`] 的 `FromStr`**：`IpAddr::from_str`
/// 不吃 CIDR 形式，故前缀须先剥 —— 但剥完之后地址解析是 stdlib 已完全覆盖的功能，无需手写。
/// stdlib 另内置前导零拒绝（CVE-2021-29922 那类八进制歧义加固）与单 `::` 约束，严于原手写版。
#[must_use]
pub fn is_valid_cidr(s: &str) -> bool {
    let Some((addr, prefix)) = s.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => prefix <= 32,
        Ok(IpAddr::V6(_)) => prefix <= 128,
        Err(_) => false,
    }
}

/// 校验 IPv4 地址（移植自 Go `net.ParseIP(gw).To4() != nil`，`helper.go:486`）。
///
/// stdlib [`Ipv4Addr`] 的 `FromStr` 只认点分十进制 `a.b.c.d`（拒前导零/八进制），语义即 Go 的
/// `ParseIP(...).To4() != nil`（IPv6 串在此返回 false）。
#[must_use]
pub fn is_valid_ipv4(s: &str) -> bool {
    s.parse::<Ipv4Addr>().is_ok()
}

/// macOS 接口白名单（移植自 `helper.go:255-272` 的 `ifaceAllowed`）。
///
/// 仅允许 `polaris-ts` / `polaris-wg` / `utunN`（N 为 1-3 位数字）。杜绝任意接口名注入 route 命令。
#[must_use]
pub fn is_mac_iface_allowed(s: &str) -> bool {
    if s == "polaris-ts" || s == "polaris-wg" {
        return true;
    }
    let Some(rest) = s.strip_prefix("utun") else {
        return false;
    };
    // rest 须为 1-3 位纯数字（Go: `rest == "" || len(rest) > 3` → false）
    !rest.is_empty() && rest.len() <= 3 && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Windows 接口白名单（移植自 `helper-win/helper.go:50-60` 的 `ifaceAllowed`）。
///
/// 仅允许 `polaris-` 前缀 + 小写字母/数字/连字符，长度 ≤ 24。
#[must_use]
pub fn is_win_iface_allowed(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("polaris-") else {
        return false;
    };
    if s.len() > 24 {
        return false;
    }
    rest.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 校验 install-core 的 want_hash（移植自 `helper.go:137` 的 `len(wantHash) != 64`）。
///
/// 须为 64 字符的 hex sha256。Go 源用 `strings.EqualFold(hex.EncodeToString(sum[:]), wantHash)` 比对，
/// 即大小写不敏感 —— 本函数同样接受大小写混用。
#[must_use]
pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::StartParams;

    #[test]
    fn mac_frame_has_token_first() {
        // mac: 行1=token, 行2=command, 行3..=args（对照 helper.go:403-404 的两个 readLine）
        let req = Request::Ping;
        let lines = encode_frame(Platform::Mac, "tok123", &req);
        assert_eq!(lines, vec!["tok123", "ping"]);
    }

    #[test]
    fn win_frame_has_token_first() {
        // win: 同 mac（命名管道 + token 行，helper-win/helper.go:167-168）
        let lines = encode_frame(Platform::Win, "tok", &Request::Status);
        assert_eq!(lines, vec!["tok", "status"]);
    }

    #[test]
    fn linux_frame_no_token_line() {
        // linux: 行1=command（无 token 行，SO_PEERCRED 鉴权，helper-linux/helper.go:343）
        let lines = encode_frame(Platform::Linux, "ignored", &Request::Ping);
        assert_eq!(lines, vec!["ping"]);
    }

    #[test]
    fn mac_start_frame_full() {
        // 完整 mac start 帧（对照 helper.go:508-513 的 6 行 readLine 序列）
        let req = Request::Start(StartParams {
            cfg: "/tmp/c.json".into(),
            log: "/tmp/l.log".into(),
            fwd: true,
            parent_pid: Some(1000),
        });
        let bytes = encode(Platform::Mac, "TOK", &req);
        let s = String::from_utf8(bytes).unwrap();
        assert_eq!(s, "TOK\nstart\n/tmp/c.json\n/tmp/l.log\n1\n1000\n");
    }

    #[test]
    fn frame_to_bytes_adds_newlines() {
        let bytes = frame_to_bytes(&["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        assert_eq!(bytes, b"a\nb\nc\n");
    }

    // ===== 接口白名单（逐字对照 Go 源）=====

    #[test]
    fn mac_iface_whitelist_matches_go_source() {
        // helper.go:255-272 的 ifaceAllowed
        assert!(is_mac_iface_allowed("polaris-ts"));
        assert!(is_mac_iface_allowed("polaris-wg"));
        assert!(is_mac_iface_allowed("utun3"));
        assert!(is_mac_iface_allowed("utun123"));
        // 拒绝
        assert!(!is_mac_iface_allowed("en0"));
        assert!(!is_mac_iface_allowed("utun"));
        assert!(!is_mac_iface_allowed("utun1234")); // >3 位
        assert!(!is_mac_iface_allowed("utunX"));
        assert!(!is_mac_iface_allowed("polaris-other"));
        assert!(!is_mac_iface_allowed(""));
    }

    #[test]
    fn win_iface_whitelist_matches_go_source() {
        // helper-win/helper.go:50-60 的 ifaceAllowed：polaris- 前缀 + rest 每字符须 [a-z0-9-]
        assert!(is_win_iface_allowed("polaris-ts"));
        assert!(is_win_iface_allowed("polaris-wg"));
        assert!(is_win_iface_allowed("polaris-tun0"));
        assert!(is_win_iface_allowed("polaris-abc-123"));
        // Go 源对 "polaris-"（rest 为空）返回 true —— rest 为空时 `for _, c := range ""` 不执行循环体，
        // 函数落到 `return true`。本实现用 `all()` over 空迭代器（恒 true）保持一致：
        assert!(is_win_iface_allowed("polaris-"));
        // 拒绝
        assert!(!is_win_iface_allowed("polaris-ABC")); // 大写
        assert!(!is_win_iface_allowed("polaris_abc")); // 下划线
        assert!(!is_win_iface_allowed("en0"));
        // 超长拒绝（Go: len(s) > 24）
        assert!(!is_win_iface_allowed(&format!(
            "polaris-{}",
            "a".repeat(30)
        )));
    }

    // ===== CIDR / IPv4 校验 =====

    #[test]
    fn cidr_validation_matches_go_parsecidr() {
        // helper.go:470 的 net.ParseCIDR 校验
        assert!(is_valid_cidr("10.0.0.0/8"));
        assert!(is_valid_cidr("172.16.0.0/12"));
        assert!(is_valid_cidr("0.0.0.0/0"));
        assert!(is_valid_cidr("::/0"));
        assert!(is_valid_cidr("2001:db8::/32"));
        // 拒绝
        assert!(!is_valid_cidr("10.0.0.0/33")); // IPv4 prefix > 32
        assert!(!is_valid_cidr("10.0.0.0")); // 无 /
        assert!(!is_valid_cidr("10.0.0.0/abc"));
        assert!(!is_valid_cidr("not-a-cidr"));
        assert!(!is_valid_cidr("256.1.1.1/8")); // octet > 255
    }

    #[test]
    fn ipv4_validation_matches_go_parseip_tov4() {
        // helper.go:486 的 net.ParseIP(gw).To4()
        assert!(is_valid_ipv4("192.168.1.1"));
        assert!(is_valid_ipv4("10.0.0.1"));
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(!is_valid_ipv4("256.1.1.1"));
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("::1")); // IPv6 非 IPv4
        assert!(!is_valid_ipv4("not-an-ip"));
    }

    /// 前导零必须拒绝（八进制歧义 = CVE-2021-29922 类）。
    ///
    /// 锁死一次**行为修正**：旧的手写解析注释自称「拒绝前导零（如 "01"）…避免八进制歧义」，实则只拦了
    /// `len > 3`，`"010".parse::<u16>()` 收成 10 → `010.0.0.1` 被当 `10.0.0.1` 放行，与注释相反。
    /// 改用 stdlib `Ipv4Addr`/`IpAddr` 的 `FromStr` 后真正拒绝，与 Go 1.17+ `net.ParseIP`（本模块的移植 oracle）
    /// 及原注释意图一致。此测试防回归到「注释说拒、代码放行」的旧形态。
    #[test]
    fn rejects_leading_zero_octets() {
        assert!(!is_valid_ipv4("010.0.0.1"));
        assert!(!is_valid_ipv4("192.168.01.1"));
        assert!(!is_valid_cidr("010.0.0.0/8"));
        // 对照组：无前导零的等价地址正常放行
        assert!(is_valid_ipv4("10.0.0.1"));
        assert!(is_valid_cidr("10.0.0.0/8"));
    }

    #[test]
    fn sha256_hex_validation() {
        // helper.go:137 的 len(wantHash) != 64 校验
        assert!(is_valid_sha256_hex("a".repeat(64).as_str()));
        assert!(is_valid_sha256_hex("ABCDEF0123456789".repeat(4).as_str())); // 大小写混用
        assert!(!is_valid_sha256_hex("abc")); // 太短
        assert!(!is_valid_sha256_hex("z".repeat(64).as_str())); // 非 hex
        assert!(!is_valid_sha256_hex(&"a".repeat(63))); // 63 字符
    }

    #[test]
    fn ipv6_parsing_variants() {
        // 确保 CIDR 校验对 IPv6 各形态正确（route-add 的 IPv6 族，helper.go:474）
        assert!(is_valid_cidr("::1/128"));
        assert!(is_valid_cidr("2001:db8::1/64"));
        assert!(is_valid_cidr("fe80::/10"));
        assert!(!is_valid_cidr("2001:db8::/130")); // prefix > 128
                                                   // 完整 IPv6（无 ::）
        assert!(is_valid_cidr("2001:0db8:0000:0000:0000:0000:0000:0001/64"));
    }
}
