//! Tailscale `control_url` 前置校验 —— 内核 panic 的**唯一判据源**。
//!
//! # 为什么需要这道校验（上游机制，2026-07-31 实测 + 读源码确认）
//!
//! sing-box `protocol/tailscale/endpoint.go::NewEndpoint` 里有一处**无条件类型断言**：
//!
//! ```text
//! // v1.14.0-beta.3 endpoint.go:174-195
//! var remoteIsDomain bool
//! if options.ControlURL != "" {
//!     controlURL, err := url.Parse(options.ControlURL)
//!     if err != nil { return nil, E.Cause(err, "parse control URL") }
//!     remoteIsDomain = M.ParseSocksaddr(controlURL.Hostname()).IsDomain()
//! } else {
//!     remoteIsDomain = true
//! }
//! outboundDialer, err := dialer.NewWithOptions(dialer.Options{
//!     ..., RemoteIsDomain: remoteIsDomain, ResolverOnDetour: true, NewDialer: true,
//! })
//! dialerQueryOptions := outboundDialer.(dialer.ResolveDialer).QueryOptions()   // ← :195 断言
//! ```
//!
//! 而 `common/dialer/dialer.go:65` 决定「拨号器要不要被包成 `ResolveDialer`」的那道门是：
//!
//! ```text
//! if options.RemoteIsDomain && ( !hasDetour || options.ResolverOnDetour || <domain_resolver 非空> ) {
//!     ... dialer = NewResolveDialer(...)   // 只有进了这里，断言才成立
//! }
//! ```
//!
//! `RemoteIsDomain` 是**合取式的第一项**：它为 false 时整个条件短路，右边三项（含
//! `domain_resolver` 是否配了）一个都不会被求值。于是：
//!
//! - `control_url` 的 host 是**域名** → `remoteIsDomain = true` → 包成 `resolveDialer` → 断言成立；
//! - `control_url` 的 host 是 **IP 字面量或为空** → `remoteIsDomain = false` → 拨号器停在
//!   `*dialer.DefaultDialer`（有 detour 时是 `*dialer.DetourDialer`）→ **:195 断言直接 panic**：
//!   `interface conversion: *dialer.DefaultDialer is not dialer.ResolveDialer: missing method QueryOptions`
//!
//! 这条合取顺序也解释了为什么**补 `domain_resolver` 治不好**（已实测证否）：它是被短路掉的那一项。
//!
//! # 判据 = 「host 不是域名」，比「host 是 IP」更宽
//!
//! `M.Socksaddr::IsDomain()` 在 host 为**空串**时同样返回 false。而 Go 的 `url.Parse` 对
//! **不带 scheme** 的输入（`hs.example.com`、`not-a-url`）解析成功但 `Host` 为空 ⇒ 同样 panic。
//! 也就是说少打一个 `https://` 与填 IP 是**同一个 panic**，且前者是远更常见的手滑。
//!
//! 本模块因此把三类都拦下：IP 字面量 / 缺 scheme / host 缺失或畸形。
//!
//! # 与上游判据的两处**刻意**偏严（下面的单测逐条钉住）
//!
//! 1. **前导零点分四段**（`192.168.001.010`）：Go `netip.ParseAddr` 拒前导零 ⇒ 上游当域名、不 panic。
//!    但它也绝不可能解析成功（DNS 查 "192.168.001.010" 必 NXDOMAIN）——用户意图显然是 IP。
//!    判成 [`ControlUrlReject::IpLiteral`] 让他拿到「要填域名」这句可行动的话，好过让核悄悄连不上。
//! 2. **裸 IPv6 / 方括号里塞 IPv4**（`http://fd7a::1`、`http://[192.168.1.10]:8080`）：上游一个当域名放行、
//!    一个 `parse control URL` 报错（FATAL 而非 panic）。两者都不是能用的地址，本模块一律拒。
//!
//! 偏严只会多拦「本来也不工作」的写法，不会拦住任何**能工作**的域名写法（阴性对照见单测
//! `domain_forms_never_rejected`：`localhost` 明确归**合法**侧——上游 `IsDomain()` 判它是域名，
//! 实测 `sing-box check` 通过）。
//!
//! # 射程自曝
//!
//! 判据来自 `sing-box check`（构造期）——`NewEndpoint` 在 check 与 run 里是同一条代码路径，故 panic 与否
//! 两边一致；但**「不 panic」不等于「连得上」**：控制面可达性、证书、headscale 版本兼容一概不在本模块射程内。

#![forbid(unsafe_code)]

use crate::user_config::ip::{is_ip_literal, strip_brackets};

/// `control_url` 被拒的成因。取值经 [`reject_token`] 转成**稳定机器 token** 下发前端换 i18n 文案，
/// 故枚举项的语义不得复用（要新增成因就加新项，别把旧项的含义改掉）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlUrlReject {
    /// host 是 IP 字面量（v4 / v6 / 带 zone / 方括号形式）→ 内核 `endpoint.go:195` panic。
    IpLiteral,
    /// 缺 `scheme://` → Go `url.Parse` 得到空 Host → 同一处 panic。
    MissingScheme,
    /// 有 scheme 但 host 为空（`http://`、`http://:8080`）→ 同一处 panic。
    NoHost,
    /// host 畸形（裸 IPv6、方括号不配平、内嵌空白等）→ 内核 `parse control URL` FATAL 或 panic。
    Malformed,
}

/// [`ControlUrlReject`] → 稳定机器 token。
///
/// 前端按 token 查 i18n 文案（`ui/src/domain/invalid-node-reason.ts`），**不渲染 token 本身**；
/// `ui/src/contracts/invalid-node-reason-coverage.test.ts` 双向对账本函数与前端映射表。
pub fn reject_token(reject: ControlUrlReject) -> &'static str {
    match reject {
        ControlUrlReject::IpLiteral => "control-url-ip",
        ControlUrlReject::MissingScheme => "control-url-scheme",
        ControlUrlReject::NoHost | ControlUrlReject::Malformed => "control-url-invalid",
    }
}

/// host 是否 IP 字面量（内核 `M.ParseSocksaddr(...).IsDomain() == false` 的 IP 那一半）。
///
/// 取**并集**而非只用一种解析：
/// - `is_ip_literal` 是 上游 正则语义（容前导零），比 Go 宽 → 覆盖上面「偏严 #1」；
/// - `IpAddr::from_str` 是严格语义，与 Go `netip.ParseAddr` 同口径 → 兜住正则写不下的 v6 边角。
///
/// **zone id 必须先截断**：`fe80::1%eth0` 在 Go 那边 `netip` 认 zone、判为 IP（实测 panic），
/// Rust `IpAddr` 不认 zone 会解析失败 —— 不截断就会把它漏判成域名，那正是 fail-open。
fn is_ip_host(host: &str) -> bool {
    let h = strip_brackets(host);
    let h = h.split('%').next().unwrap_or(h);
    is_ip_literal(h) || h.parse::<std::net::IpAddr>().is_ok()
}

/// 端口后缀（`:8080`）判定。空端口（`host:`）不算合法后缀。
fn is_port_suffix(s: &str) -> bool {
    match s.strip_prefix(':') {
        Some(p) => !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// host 是否**可能**是域名。
///
/// 刻意只列**否定**字符（`: / ? # @ [ ] \` 与控制字符），不做 LDH 白名单：headscale 用 IDN 域名
/// 完全合法，白名单会把它误伤成非法。走到本函数时 `/ ? # @` 已在上游切走，剩下的主要是裸 IPv6 的冒号
/// 与不配平的方括号。
fn is_hostname_like(host: &str) -> bool {
    !host.is_empty()
        && !host
            .chars()
            .any(|c| matches!(c, ':' | '/' | '?' | '#' | '@' | '[' | ']' | '\\') || c.is_control())
}

/// Tailscale `control_url` 的前置校验：`None` = 可下发，`Some(_)` = **必须拦在下发之前**。
///
/// 空串 / 全空白 → `None`（用户没填 → 内核走 `remoteIsDomain = true` 的 else 分支，安全）。
pub fn tailscale_control_url_reject(raw: &str) -> Option<ControlUrlReject> {
    let s = raw.trim();
    if s.is_empty() {
        // 未填 = 用官方 controlplane，内核 else 分支恒 remoteIsDomain=true → 不可能 panic。
        return None;
    }
    // 内嵌空白 → Go `url.Parse` 直接报错（实测 `parse control URL`），FATAL 掉整个核。
    if s.chars().any(char::is_whitespace) {
        return Some(ControlUrlReject::Malformed);
    }

    // scheme：必须有 `://`，且 scheme 本身合法（ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )）。
    // 不限定 http/https —— 那是额外的产品意见，本模块只复刻内核的 panic 判据。
    let Some(pos) = s.find("://") else {
        return Some(ControlUrlReject::MissingScheme);
    };
    let scheme = &s[..pos];
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return Some(ControlUrlReject::MissingScheme);
    }

    // authority = scheme 之后、首个 `/ ? #` 之前；再剥 userinfo（内核 `url.Hostname()` 同样只取 host）。
    let rest = &s[pos + 3..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };

    // 方括号形式：内核只接受 IPv6 —— 是 IP 就 panic，不是 IP（如 `[192.168.1.10]`，Go 认 v4 不该带括号）
    // 就 `parse control URL` FATAL。两条都得拦，前者给「别填 IP」的话更有用。
    if let Some(stripped) = hostport.strip_prefix('[') {
        let Some(end) = stripped.find(']') else {
            return Some(ControlUrlReject::Malformed);
        };
        let inner = &stripped[..end];
        let after = &stripped[end + 1..];
        if !after.is_empty() && !is_port_suffix(after) {
            return Some(ControlUrlReject::Malformed);
        }
        return Some(if is_ip_host(inner) {
            ControlUrlReject::IpLiteral
        } else {
            ControlUrlReject::Malformed
        });
    }

    // 非方括号：末段全数字才当端口剥掉；否则残留的冒号意味着裸 IPv6 之类的畸形。
    let host = match hostport.rfind(':') {
        Some(i) if is_port_suffix(&hostport[i..]) => &hostport[..i],
        Some(_) => return Some(ControlUrlReject::Malformed),
        None => hostport,
    };

    if host.is_empty() {
        return Some(ControlUrlReject::NoHost);
    }
    if is_ip_host(host) {
        return Some(ControlUrlReject::IpLiteral);
    }
    if !is_hostname_like(host) {
        return Some(ControlUrlReject::Malformed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::ControlUrlReject::*;
    use super::*;

    /// 阳性：内核实测 panic 的每一种形态都必须被判非法。
    ///
    /// 表里每一行都在 2026-07-31 用本机 `resources/linux/sing-box`（1.14.0-beta.3）跑过
    /// `sing-box check`，结论逐条对齐（panic → 这里必 `Some`）。
    ///
    /// **这条实测断言只覆盖标了「实测」的行**。核**接受**但我们仍拦的形态一律归
    /// [`intentionally_stricter_than_upstream`]，不许混进来 —— 混进来会让「panic ⟺ 这里 Some」
    /// 这个双向对齐变成单向，日后有人「对齐上游」时就分不清哪些能放、哪些放了会炸。
    /// （`//hs.example.com` 曾被误归本表，2026-07-31 实测证否后已移走。）
    ///
    /// **变异实测**：把 `tailscale_control_url_reject` 里的 `if is_ip_host(host)` 那条删掉
    /// ⇒ 本测 10 条 IP 形态全红。
    #[test]
    fn ip_literal_forms_all_rejected() {
        for url in [
            "http://192.168.1.10:8080",         // 实测 PANIC
            "http://192.168.1.10",              // 无端口，实测 PANIC
            "https://192.168.1.10:8080",        // scheme 无关，实测 PANIC
            "http://192.168.1.10:8080/key",     // 带 path，实测 PANIC
            "http://user:pw@192.168.1.10:8080", // 带 userinfo，实测 PANIC
            "http://127.0.0.1:39824",           // 陈先生原始复现样本
            "https://127.0.0.1:39824",          // 同上，https
            "http://0.0.0.0:8080",              // 实测 PANIC
            "https://203.0.113.9",              // 公网 IP，实测 PANIC
            "ws://192.168.1.10:8080",           // 非 http scheme 照样 PANIC
        ] {
            assert_eq!(
                tailscale_control_url_reject(url),
                Some(IpLiteral),
                "IPv4 形态未被判为 IP 字面量: {url}"
            );
        }
    }

    /// 阳性：IPv6 各形态（方括号 / 带端口 / zone / v4-mapped / 大写）实测同样 panic。
    ///
    /// **变异实测**：删掉 `is_ip_host` 里的 `h.split('%').next()` zone 截断
    /// ⇒ `[fe80::1%25eth0]` 一条转红（Rust `IpAddr` 不认 zone，会漏判成域名 = fail-open）。
    #[test]
    fn ipv6_forms_all_rejected() {
        for url in [
            "http://[fd7a:115c:a1e0::1]:8080",    // 实测 PANIC
            "http://[::1]",                       // 无端口，实测 PANIC
            "http://[::1]:39824",                 // 实测 PANIC
            "http://[::ffff:192.168.1.10]:8080",  // v4-mapped，实测 PANIC
            "http://[2001:db8:0:0:0:0:0:1]:8080", // 全展开，实测 PANIC
            "http://[FD7A:115C::1]:8080",         // 大写 hex，实测 PANIC
            "http://[fe80::1%25eth0]:8080",       // 带 zone id，实测 PANIC
        ] {
            assert_eq!(
                tailscale_control_url_reject(url),
                Some(IpLiteral),
                "IPv6 形态未被判为 IP 字面量: {url}"
            );
        }
    }

    /// 阳性：缺 scheme —— 与填 IP 是**同一处** panic，且是更常见的手滑。
    ///
    /// **变异实测**：把 `let Some(pos) = s.find("://") else { ... }` 改成 `.unwrap_or(0)` 之类的放行写法
    /// ⇒ 本测全红。
    #[test]
    fn missing_scheme_rejected() {
        for url in [
            "hs.example.com",    // 实测 PANIC（url.Parse 成功但 Host 为空）
            "not-a-url",         // 实测 PANIC
            "192.168.1.10:8080", // 无 scheme 的 IP 写法
            "mailto:a@b.com",    // 有冒号但无 `://`
        ] {
            assert_eq!(
                tailscale_control_url_reject(url),
                Some(MissingScheme),
                "缺 scheme 未被拦: {url}"
            );
        }
    }

    /// 阳性：有 scheme 但 host 缺失 —— 实测同样 panic。
    #[test]
    fn empty_host_rejected() {
        assert_eq!(tailscale_control_url_reject("http://"), Some(NoHost));
        assert_eq!(tailscale_control_url_reject("http://:8080"), Some(NoHost));
        assert_eq!(tailscale_control_url_reject("http:///path"), Some(NoHost));
    }

    /// 阳性：畸形 host。内核这几条是 `parse control URL` FATAL（不是 panic），但 FATAL 会拖垮**整个核**
    /// （所有节点一起断），比丢掉一个节点更糟 → 一样拦。
    #[test]
    fn malformed_host_rejected() {
        // 内嵌空白：实测 `parse control URL` FATAL。
        assert_eq!(
            tailscale_control_url_reject("http://192.168.1.10 :8080"),
            Some(Malformed)
        );
        // 方括号不配平。
        assert_eq!(tailscale_control_url_reject("http://[::1"), Some(Malformed));
        // 裸 IPv6（无方括号）：上游当域名放行，但它永远解析不出去。
        assert_eq!(
            tailscale_control_url_reject("http://fd7a::1"),
            Some(Malformed)
        );
        // 方括号里塞 IPv4：内核 `parse control URL` FATAL；本模块给更有用的「别填 IP」。
        assert_eq!(
            tailscale_control_url_reject("http://[192.168.1.10]:8080"),
            Some(IpLiteral)
        );
    }

    /// **阴性对照（最重要的一条）**：合法域名写法一个都不许被误伤。
    ///
    /// 这里每一条也都实测 `sing-box check` **通过**。少了这条对照，把判据写成「一律拒」也能让上面
    /// 五条阳性全绿 —— 那样的门只是把 panic 换成了「谁都连不上」。
    ///
    /// `localhost` 归**合法**侧：上游 `M.ParseSocksaddr("localhost").IsDomain()` 为 true
    /// ⇒ 走 resolveDialer 分支 ⇒ 实测 check 通过、不 panic。自建 headscale 用 `http://localhost:8080`
    /// 是常见写法，拦它属于纯误伤。
    ///
    /// **变异实测**：把 `is_ip_host` 改成恒 `true` ⇒ 本测全红（而五条阳性仍全绿）。
    #[test]
    fn domain_forms_never_rejected() {
        for url in [
            "https://hs.example.com",             // 实测 PASS
            "http://example.invalid",             // 实测 PASS
            "https://headscale.local:8080",       // 实测 PASS
            "http://localhost:8080",              // 实测 PASS —— 不是 IP，不许拦
            "http://localhost",                   // 实测 PASS
            "https://controlplane.tailscale.com", // 官方默认值
            "https://hs.example.com.:8080",       // 尾点 FQDN，实测 PASS
            "http://1.2.3.4.5:8080",              // 五段 → 不是 IPv4，实测 PASS
            "http://12345",                       // 纯数字但非 IP，实测 PASS
            "HTTPS://HS.EXAMPLE.COM",             // 大写 scheme，实测 PASS
            "https://hs.example.com/key/path",    // 带 path
            "https://用户.example.com",           // IDN：白名单式 host 校验会误伤，故只列否定字符
        ] {
            assert_eq!(
                tailscale_control_url_reject(url),
                None,
                "合法域名写法被误伤: {url}"
            );
        }
    }

    /// 未填 = 合法（内核走 `remoteIsDomain = true` 的 else 分支）。
    #[test]
    fn empty_is_allowed() {
        assert_eq!(tailscale_control_url_reject(""), None);
        assert_eq!(tailscale_control_url_reject("   "), None);
        assert_eq!(tailscale_control_url_reject("\t\n"), None);
        // 两侧空白会被发射面 trim 掉，等价于已 trim 的值。
        assert_eq!(
            tailscale_control_url_reject("  https://hs.example.com  "),
            None
        );
    }

    /// 与上游判据的**刻意偏严**：钉住方向，防有人「对齐上游」时把它改回 fail-open。
    #[test]
    fn intentionally_stricter_than_upstream() {
        // 前导零点分四段：Go netip 拒前导零 ⇒ 上游当域名放行（实测 PASS，不 panic）。
        // 我们判 IP：它 DNS 也解析不出去，给「要填域名」比让核静默连不上有用。
        assert_eq!(
            tailscale_control_url_reject("http://192.168.001.010:8080"),
            Some(IpLiteral),
            "前导零 IPv4 应按 IP 拦（刻意偏严，见模块头注 #1）"
        );
        // 协议相对 URL：**核实测 `sing-box check` 通过，不 panic**（Go `url.Parse("//host")` 给出的
        // Host 是 `host` 而非空——此前本行被误归在 `missing_scheme_rejected` 里、注释写「Go 侧同样
        // Host 为空」，是推理错误，2026-07-31 实测证否后移到这里）。
        //
        // 我们仍然拦，理由与前导零那条同类：① 用户写 `//host` 几乎必然是想写 `https://host` 的手滑；
        // ② check 通过只说明 schema 与初始化那一步不炸，tsnet 后续拿它拼请求是否可用**未验**。
        // 给「请填完整 URL」比让核带着一个可疑控制面静默跑下去有用。
        assert_eq!(
            tailscale_control_url_reject("//hs.example.com"),
            Some(MissingScheme),
            "协议相对 URL 应按缺 scheme 拦（刻意偏严：核接受，我们不接受）"
        );
    }

    /// token 映射稳定（前端 i18n 映射表按它取键）。
    #[test]
    fn tokens_are_stable() {
        assert_eq!(reject_token(IpLiteral), "control-url-ip");
        assert_eq!(reject_token(MissingScheme), "control-url-scheme");
        assert_eq!(reject_token(NoHost), "control-url-invalid");
        assert_eq!(reject_token(Malformed), "control-url-invalid");
    }
}
