//! 真实上游查询实现 —— 上游 `node-dns-race-server.ts` 的 `queryOneUpstream` / `udpQuery` /
//! `systemQuery` / `makeDefaultUpstreamQuery` 移植。
//!
//! **DoH 不在本 crate 里发**：整个 workspace 只有 `src-tauri/src/runtime/http.rs` 持有真实
//! HTTP/TLS 客户端（见该模块文档「各 domain crate 只声明窄 trait，真实传输在此注入」）。故这里只声明
//! [`DohPost`] 端口，由 src-tauri 的 `HttpRuntime` 适配。UDP / system 两种上游无 TLS 栈、直接用
//! tokio 与 OS resolver，留在本 crate。

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::UdpSocket;

use crate::race::UpstreamQuery;
use crate::upstream::{ResolveUpstream, UpstreamKind};
use crate::wire::{build_answer_response, decode_dns_question, AnswerRecord, TYPE_A, TYPE_AAAA};

/// 单上游超时（上游 `PER_UPSTREAM_TIMEOUT_MS`）。
///
/// **必须小于竞速总预算**：单上游没有自己的超时，一个挂死的上游会占着 `FuturesUnordered` 的一格
/// 直到总预算耗尽，Tier2 兜底也就永远轮不上。
pub const PER_UPSTREAM_TIMEOUT: Duration = Duration::from_millis(1500);

/// DoH 传输端口（窄 trait）：POST `application/dns-message`，返回响应体原始字节。
///
/// 实现方须负责：超时之外的连接管理、**不经本机代理**（sidecar 起在起核路径上，走代理即自举死锁）、
/// 非 2xx → `Err`。
#[async_trait]
pub trait DohPost: Send + Sync {
    async fn post_dns_message(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// IPv6 字面量在 URL authority / `SocketAddr` 文本里**必须**带方括号。
///
/// 少了方括号，地址内部的冒号与 `host:port` 的分隔冒号无法区分，两条腿都**恒静默 FAIL**：
/// - DoH：`https://2606:4700:4700::1111:443/dns-query` 被 URL 解析判非法 → 该上游每次都 Err；
/// - UDP：`::1:53` 的 `parse::<SocketAddr>()` 直接 Err → 同上。
///
/// 而 config-engine 的 `parse_dns_server_spec` 明确接受 IPv6（`doh_v6` / `bare_v6` 两条单测钉着），
/// [`is_valid_custom_upstream_spec`](crate::upstream::is_valid_custom_upstream_spec) 也会告诉 UI
/// 「这条 spec 合法」——「UI 判合法 + 运行期恒失败」是最坏形态（用户池里只勾一个 v6 上游时 tier1
/// 非空、不触发默认回退 ⟹ 节点解析全 SERVFAIL 且日志只有一句上游 Err）。故此处补齐方括号，
/// 让 v6 上游**真的可用**（`udp_query` 早就写了 `peer.is_ipv6()` 的绑定族分支，本就是按可用设计的）。
fn bracket_ipv6(ip: &str) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

/// `ip:port` → [`std::net::SocketAddr`]，IPv6 自动补方括号。见 [`bracket_ipv6`]。
fn upstream_socket_addr(ip: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    let text = format!("{}:{port}", bracket_ipv6(ip));
    text.parse()
        .map_err(|e| format!("上游地址非法 {text}: {e}"))
}

/// `system` 腿的**自递归防护**（纵深防御第二道；第一道是 `plan_upstreams` 的 INV-1 摘除，
/// 见 [`system_query`] 文档「两道防线」）：同一 qname 的 system 查询在飞期间拒绝二次进入。
///
/// 为什么需要（TUN 下的放大链，见 [`system_query`] 文档「TUN 自递归」）：sidecar 的 system 腿走
/// OS resolver → 明文 :53 → 被 route 的 `hijack-dns` 抓走 → 内核按域名规则又指回 `dns-node-race`
/// → 回到本 sidecar → 又走 system 腿……每层各带一份 Tier1 齐射，Tier1 全 FAIL（离线 / DoH 被封）
/// 时逐级放大。本守卫把递归**钉死在一层**：第二层的 system 腿立刻 Err，只剩它自己的 Tier1，
/// 拿到结论就回，链条终止。
///
/// 代价（可接受、方向 fail-safe）：两个**并发**的同名查询里，后到的那个拿不到 system 腿。
/// system 本就是 Tier2 兜底（不抢跑、Tier1 有结论时根本不问它），退化面是「后到者少一个兜底腿」，
/// 而不是「解析出错」。
#[derive(Debug, Default)]
struct SystemInFlight(Mutex<HashSet<String>>);

/// 在飞占位。`drop` 即释放（含 `system_query` 的任何 early-return / 取消腿 —— 竞速抢跑会直接 drop
/// 掉未完成的 future，用 RAII 才不会漏释放把 qname 永久钉死）。
struct SystemLegGuard<'a> {
    owner: &'a SystemInFlight,
    qname: String,
}

impl Drop for SystemLegGuard<'_> {
    fn drop(&mut self) {
        let mut set = self.owner.0.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.qname);
    }
}

impl SystemInFlight {
    /// 占位成功 → `Some(guard)`；同名已在飞 → `None`（调用方须立即 Err，别递归下去）。
    fn enter(&self, qname: &str) -> Option<SystemLegGuard<'_>> {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(qname.to_string()) {
            return None;
        }
        Some(SystemLegGuard {
            owner: self,
            qname: qname.to_string(),
        })
    }
}

/// 生产用的 [`UpstreamQuery`]：按上游 kind 分派 DoH / UDP / system，并统一套单上游超时。
pub struct DefaultUpstreamQuery {
    doh: Arc<dyn DohPost>,
    per_upstream: Duration,
    /// system 腿自递归防护（见 [`SystemInFlight`]）。按实例持有 —— 一个 sidecar 一份。
    system_in_flight: SystemInFlight,
}

impl DefaultUpstreamQuery {
    #[must_use]
    pub fn new(doh: Arc<dyn DohPost>) -> Self {
        Self {
            doh,
            per_upstream: PER_UPSTREAM_TIMEOUT,
            system_in_flight: SystemInFlight::default(),
        }
    }

    /// 自定义单上游超时（单测用；生产走 [`PER_UPSTREAM_TIMEOUT`]）。
    #[must_use]
    pub fn with_timeout(doh: Arc<dyn DohPost>, per_upstream: Duration) -> Self {
        Self {
            doh,
            per_upstream,
            system_in_flight: SystemInFlight::default(),
        }
    }

    /// 单上游一次查询（无超时包装）。上游 `queryOneUpstream`。
    async fn query_one(&self, up: &ResolveUpstream, query: &[u8]) -> Result<Vec<u8>, String> {
        match up.kind {
            // DoT（`dot=true`）二期未实现 —— `parse_custom_upstream` 已在入口拒掉 `tls://`，
            // 这里保留一条明确的 Err 腿，防将来放开 UI 却忘了实现传输（静默全 FAIL）。
            UpstreamKind::Doh if up.dot => Err(format!("上游 {} 是 DoT，二期未实现", up.id)),
            UpstreamKind::Doh => {
                // IPv6 字面量必须带方括号，否则 URL 非法 → 该上游恒 FAIL（见 `bracket_ipv6`）。
                let url = format!(
                    "https://{}:{}{}",
                    bracket_ipv6(up.ip.as_deref().unwrap_or_default()),
                    up.port.unwrap_or(443),
                    up.path.as_deref().unwrap_or("/dns-query")
                );
                self.doh.post_dns_message(&url, query.to_vec()).await
            }
            UpstreamKind::Udp => {
                let ip = up.ip.as_deref().ok_or("udp 上游缺 IP")?;
                udp_query(ip, up.port.unwrap_or(53), query).await
            }
            UpstreamKind::System => system_query(&self.system_in_flight, query).await,
        }
    }
}

#[async_trait]
impl UpstreamQuery for DefaultUpstreamQuery {
    async fn query(&self, upstream: &ResolveUpstream, query: &[u8]) -> Result<Vec<u8>, String> {
        tokio::time::timeout(self.per_upstream, self.query_one(upstream, query))
            .await
            .map_err(|_| format!("上游 {} 超时", upstream.id))?
    }
}

/// 明文 UDP 查询：发 query 到 `ip:port`，收首个**合法**响应。上游 `udpQuery`。
///
/// 硬化（与 上游 同）：只接受来自该上游 `ip:port` 的响应，且响应 id 必须 == query id ——
/// 防 off-path UDP 注入伪造响应污染解析。不匹配 → 忽略、继续等真响应（超时由上层兜）。
async fn udp_query(ip: &str, port: u16, query: &[u8]) -> Result<Vec<u8>, String> {
    let peer = upstream_socket_addr(ip, port)?;
    // 绑定族要与目标一致，否则 v6 上游发不出去。
    let bind = if peer.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let sock = UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("UDP 上游本地绑定失败: {e}"))?;
    sock.send_to(query, peer)
        .await
        .map_err(|e| format!("UDP 上游发送失败 {peer}: {e}"))?;
    let mut buf = vec![0u8; MAX_DNS_UDP_BYTES];
    loop {
        let (n, from) = sock
            .recv_from(&mut buf)
            .await
            .map_err(|e| format!("UDP 上游接收失败 {peer}: {e}"))?;
        if from != peer {
            continue; // 非该上游发来的包
        }
        if n >= 2 && query.len() >= 2 && (buf[0] != query[0] || buf[1] != query[1]) {
            continue; // id 不匹配 = 注入/串包
        }
        return Ok(buf[..n].to_vec());
    }
}

/// UDP DNS 收包缓冲上限。512 是无 EDNS0 的经典上限，但内核会带 EDNS0（常见 payload 1232/4096），
/// 收小了会**静默截断**成畸形包（分类为 FAIL）。取 4096 覆盖常见宣告值。
pub const MAX_DNS_UDP_BYTES: usize = 4096;

/// system 上游：用 OS 解析器按 qtype 解析 → 构造 wire 响应。上游 `systemQuery`。
///
/// 仅 A（IPv4）；AAAA / 其它 qtype → `Err`（让 Tier1 兜，二期）。
///
/// **与 上游的一处刻意分歧**：上游 用 node `dns.resolve4` 的 `ENOTFOUND`/`ENODATA` 错误码把
/// 「域名无 A」映射成 NOERROR AN=0（EMPTY）。Rust `lookup_host` 返回的是 `io::Error`，getaddrinfo
/// 的 `EAI_*` 在 std 里既无稳定 `ErrorKind` 也无 `raw_os_error`，只能匹配错误**文案** —— 那是跨平台
/// / 跨 libc / 跨 locale 都会漂的东西。故这里取**保守腿**：解析成功但无 A → EMPTY（如实空）；
/// 解析报错 → `Err`（FAIL）。代价是「system 是唯一应答者且域名确实不存在」时回 SERVFAIL 而非
/// NXDOMAIN —— 两者对拨号都是失败，且 FAIL 会让 SERVFAIL 而非「空」被下结论，方向是 fail-safe 的。
///
/// # TUN 自递归（本腿的固有形态，非本函数可根治）
///
/// TUN 接管期这条腿是**自指**的：
/// ```text
/// 内核查 node.example.com → dns-node-race(127.0.0.1:P) → 本 sidecar
///   → system 腿 → OS resolver → 明文 UDP :53 发往 LAN DNS
///   → route.rs 的 `hijack-dns`（劫持所有其余 :53；LAN 解析器只有在 `lan_resolver_for_dns` 被注入时
///     才先被直连放行，而 src-tauri 当前恒传 None）
///   → 内核 DNS 引擎按域名规则命中「节点域名 → dns-node-race」 → 又回到本 sidecar → system 腿 → …
/// ```
/// Tier1 全 FAIL（离线 / DoH 被封）时每一层都会再放一轮齐射，逐级放大。
///
/// 这不是新发现的形态：非竞速路径早就为它立了不变量 —— `helpers.rs::get_node_resolver_tag` 的
/// **INV-1**「TUN + rule ctx + single=system → 强制走 dns-node(IP-DoH) 防递归」。
///
/// # 两道防线（缺一不可，别因为有了第一道就删第二道）
///
/// 1. **INV-1 本体（首要防线，已恢复）**：[`plan_upstreams`](crate::upstream::plan_upstreams) 收
///    `proxy_mode_type`，**TUN 接管期把 `system` 从竞速池里摘掉** —— 池里没有 system，这条腿在 TUN
///    下根本不会被调到，链条从源头断开。判据见该函数文档「INV-1」。
/// 2. **在飞守卫（纵深防御，本模块的 [`SystemInFlight`]）**：把同 qname 的 system 腿钉死在一层。
///
/// 第一道防线到位后，第二道在**当前代码路径下应当永不触发** —— 这不是删它的理由，是它该有的样子。
/// 它守的是第一道那条**假设链**：`plan_upstreams` 是起 sidecar 的唯一决策点，而 sidecar 的上游集在
/// 起核那一刻定死、此后不随配置热更新；今天「切代理模式必重起核」（`proxyModeType` 在
/// `config_generation_norm` 的投影里 ⇒ 热切换判定直接退回重启）才使「上游集与运行核的模式恒一致」
/// 成立。哪天有人给 `dnsConfig` 加一条热更新腿（把新上游集推进在跑的 sidecar），这条假设即失效，
/// 而失效的表现是**无声的**逐级放大 —— 那正是本守卫要兜的形态。
/// 另外它顺带去重了并发的同 qname system 查询（见 [`SystemInFlight`] 文档的「代价」段）。
///
/// **真机复现步骤**（本机不可验：需真 TUN + 真内核，且必须断外网，见 crate 文档「测试边界」）：
/// 1. 设置 → 节点域名解析：开竞速，上游池**勾上 `system`**（默认池不含它，必须手动勾）；
/// 2. 代理模式切 TUN，连接，确认 `config.json` 的 `dns-node-race` 已生成且 route 有 `hijack-dns`；
/// 3. 断开外网上行（拔网线 / 防火墙 drop 到 223.5.5.5:443 与 1.12.12.12:443），使 Tier1 全 FAIL；
/// 4. 触发一次节点域名解析（切节点 / 测速）；
/// 5. 观察：在 LAN DNS 侧或 sing-box 日志里数同一 qname 的重复查询次数。
///    - 两道防线都没有时：同一 qname 在一次总预算内出现 **>2 次**（逐级放大）；
///    - 只有在飞守卫时：**≤2 次**（第二层的 system 腿立刻 Err「自递归防护」，链条终止）；
///    - 恢复 INV-1 后（当前）：**0 次** —— TUN 下 system 压根不在池里，那条明文 :53 查询不再发出。
///      要验的是「LAN DNS 侧看不到本机对节点域名的明文查询」，而不只是数次数。
async fn system_query(in_flight: &SystemInFlight, query: &[u8]) -> Result<Vec<u8>, String> {
    let q = decode_dns_question(query).ok_or("system: 畸形 query")?;
    if q.qtype != TYPE_A {
        return Err(if q.qtype == TYPE_AAAA {
            "system: AAAA 二期".into() // 让 Tier1 兜
        } else {
            format!("system: 不支持的 qtype {}", q.qtype)
        });
    }
    // 自递归防护（见本函数文档「TUN 自递归」）：同名在飞 → 立刻 Err，**绝不**再问一次 OS resolver。
    // 守卫必须在 `lookup_host` 之前拿到、且活过整个 await（RAII，取消腿也会释放）。
    let _guard = in_flight
        .enter(&q.qname)
        .ok_or_else(|| format!("system: {} 同名查询在飞（自递归防护）", q.qname))?;
    // 端口 0：只要地址记录，不实际连接（对齐 runtime/http.rs `SystemDnsLookup`）。
    let addrs = tokio::net::lookup_host((q.qname.as_str(), 0))
        .await
        .map_err(|e| format!("system 解析失败 {}: {e}", q.qname))?;
    let answers: Vec<AnswerRecord> = addrs
        .filter_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(AnswerRecord {
                rtype: TYPE_A,
                rdata: v4.octets().to_vec(),
            }),
            std::net::IpAddr::V6(_) => None, // 只回 A（与 qtype 一致）
        })
        .collect();
    Ok(build_answer_response(query, &answers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::parse_custom_upstream;
    use crate::wire::{classify_dns_response, encode_dns_query, DnsResponseClass};
    use polaris_config_engine::user_config::dns_config::CustomDnsUpstream;
    use std::sync::Mutex;

    /// 记录被请求 URL 的 mock DoH（零网络）。
    struct RecordingDoh {
        urls: Mutex<Vec<String>>,
        reply: Result<Vec<u8>, String>,
    }

    #[async_trait]
    impl DohPost for RecordingDoh {
        async fn post_dns_message(&self, url: &str, _body: Vec<u8>) -> Result<Vec<u8>, String> {
            self.urls.lock().unwrap().push(url.to_string());
            self.reply.clone()
        }
    }

    fn doh(reply: Result<Vec<u8>, String>) -> Arc<RecordingDoh> {
        Arc::new(RecordingDoh {
            urls: Mutex::new(Vec::new()),
            reply,
        })
    }

    #[tokio::test]
    async fn doh_url_is_assembled_from_ip_port_path() {
        let rec = doh(Ok(vec![0u8; 12]));
        let q = DefaultUpstreamQuery::new(rec.clone());
        let up = parse_custom_upstream(&CustomDnsUpstream {
            id: "c".into(),
            spec: "https://9.9.9.9:8443/q".into(),
        })
        .unwrap();
        q.query(&up, &[0u8; 12]).await.unwrap();
        assert_eq!(
            rec.urls.lock().unwrap().as_slice(),
            ["https://9.9.9.9:8443/q"]
        );
    }

    #[tokio::test]
    async fn per_upstream_timeout_converts_hang_to_err() {
        struct HangDoh;
        #[async_trait]
        impl DohPost for HangDoh {
            async fn post_dns_message(&self, _u: &str, _b: Vec<u8>) -> Result<Vec<u8>, String> {
                std::future::pending().await
            }
        }
        let q = DefaultUpstreamQuery::with_timeout(Arc::new(HangDoh), Duration::from_millis(20));
        let up = crate::upstream::builtin_upstream("ali").unwrap();
        let r = q.query(&up, &[0u8; 12]).await;
        assert!(r.is_err(), "挂死上游必须被单上游超时转成 FAIL");
    }

    /// mock UDP 上游 —— **只绑 127.0.0.1**，随测试结束即关（严禁碰宿主网络）。
    /// `answer_ip` = 要回的 A 记录；`forge_id` = 是否故意用错 message id（验注入硬化）。
    async fn spawn_mock_udp_upstream(
        answer_ip: [u8; 4],
        forge_id: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DNS_UDP_BYTES];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let mut resp = build_answer_response(
                    &buf[..n],
                    &[AnswerRecord {
                        rtype: TYPE_A,
                        rdata: answer_ip.to_vec(),
                    }],
                );
                if forge_id {
                    resp[0] ^= 0xff; // 伪造 id：客户端必须忽略
                }
                let _ = sock.send_to(&resp, from).await;
                if forge_id {
                    continue; // 一直发坏包，逼客户端超时
                }
                break;
            }
        });
        (port, handle)
    }

    #[tokio::test]
    async fn udp_upstream_roundtrip_over_loopback() {
        let (port, srv) = spawn_mock_udp_upstream([7, 7, 7, 7], false).await;
        let q = encode_dns_query("node.example.com", TYPE_A, 0x1357);
        let resp = udp_query("127.0.0.1", port, &q)
            .await
            .expect("回环 mock 上游应答");
        assert_eq!(classify_dns_response(&resp, TYPE_A), DnsResponseClass::Hit);
        assert_eq!(
            crate::wire::extract_answer_ip_bytes(&resp),
            vec![vec![7, 7, 7, 7]]
        );
        srv.abort();
    }

    #[tokio::test]
    async fn udp_upstream_ignores_responses_with_wrong_message_id() {
        let (port, srv) = spawn_mock_udp_upstream([6, 6, 6, 6], true).await;
        let q = encode_dns_query("node.example.com", TYPE_A, 0x2468);
        // id 不匹配的包被忽略 → 没有合法应答 → 由超时兜（此处直接用 timeout 断言不会误收）。
        let r = tokio::time::timeout(Duration::from_millis(120), udp_query("127.0.0.1", port, &q))
            .await;
        assert!(r.is_err(), "伪造 id 的应答绝不能被当成答案");
        srv.abort();
    }

    #[tokio::test]
    async fn system_upstream_rejects_aaaa() {
        let q = encode_dns_query("node.example.com", TYPE_AAAA, 1);
        assert!(
            system_query(&SystemInFlight::default(), &q).await.is_err(),
            "AAAA 二期 → FAIL 让 Tier1 兜"
        );
    }

    #[tokio::test]
    async fn system_upstream_rejects_malformed_query_without_resolving() {
        assert!(system_query(&SystemInFlight::default(), &[0u8; 4])
            .await
            .is_err());
    }

    /// IPv6 自定义 DoH 上游的 URL 必须带方括号。
    /// 变异验证：把 `query_one` 里的 `bracket_ipv6(...)` 换回裸 `up.ip` → URL 变成
    /// `https://2606:4700:4700::1111:443/dns-query`（无括号）→ 本断言转红。
    #[tokio::test]
    async fn doh_url_brackets_ipv6_literal() {
        let rec = doh(Ok(vec![0u8; 12]));
        let q = DefaultUpstreamQuery::new(rec.clone());
        let up = parse_custom_upstream(&CustomDnsUpstream {
            id: "v6".into(),
            spec: "https://[2606:4700:4700::1111]/dns-query".into(),
        })
        .expect("config-engine 的 parse_dns_server_spec 明确接受 v6 字面量");
        q.query(&up, &[0u8; 12]).await.unwrap();
        assert_eq!(
            rec.urls.lock().unwrap().as_slice(),
            ["https://[2606:4700:4700::1111]:443/dns-query"],
            "无方括号的 URL 会被 reqwest 判非法 → 该上游恒静默 FAIL"
        );
    }

    /// v6 上游地址文本 → SocketAddr（纯解析，不开 socket、不碰宿主网络）。
    /// 变异验证：去掉 `bracket_ipv6` → `::1:53` / `2606:...::1111:53` 均 parse 失败 → 转红。
    #[test]
    fn upstream_socket_addr_parses_ipv6_literals() {
        let a = upstream_socket_addr("::1", 53).expect("v6 回环字面量须可解析");
        assert!(a.is_ipv6() && a.port() == 53);
        let b = upstream_socket_addr("2606:4700:4700::1111", 5353).expect("v6 全局字面量");
        assert!(b.is_ipv6() && b.port() == 5353);
        // 已带方括号的输入不重复加括号。
        assert!(upstream_socket_addr("[::1]", 53).is_ok());
        // v4 不受影响。
        let v4 = upstream_socket_addr("8.8.8.8", 53).expect("v4");
        assert!(v4.is_ipv4());
        // 真非法仍是 Err（守住「补括号 ≠ 放宽校验」）。
        assert!(upstream_socket_addr("not-an-ip", 53).is_err());
    }

    /// 【机制】system 腿在飞守卫：同名互斥、异名不互斥、释放后可重入。
    #[test]
    fn system_in_flight_guard_is_per_qname_and_releases_on_drop() {
        let f = SystemInFlight::default();
        let g1 = f.enter("node.example.com").expect("首次占位");
        assert!(
            f.enter("node.example.com").is_none(),
            "同名在飞必须拒绝 —— 这正是 TUN 下自递归被钉死在一层的机制"
        );
        assert!(f.enter("other.example.com").is_some(), "异名互不影响");
        drop(g1);
        assert!(f.enter("node.example.com").is_some(), "释放后可重入");
    }

    /// 【接线】`system_query` 真的会先过守卫、且被拒时**不做任何解析**。
    ///
    /// qname 取 `localhost`：/etc/hosts（nsswitch `files` 先于 `dns`）本地即答，
    /// 即便守卫被改坏也不会把查询发到宿主网络 —— 只会让下面的断言转红。
    /// 变异验证：删掉 `system_query` 里的 `in_flight.enter(...)?` → 返回 Ok → 转红。
    #[tokio::test]
    async fn system_query_refuses_reentrant_same_qname() {
        let f = SystemInFlight::default();
        let held = f
            .enter("localhost")
            .expect("模拟上一层 system 腿正在等 OS resolver");
        let wire = encode_dns_query("localhost", TYPE_A, 0x4242);
        let err = system_query(&f, &wire)
            .await
            .expect_err("同名重入必须立刻 FAIL，不得再问 OS resolver（否则 TUN 下逐级放大）");
        assert!(err.contains("自递归防护"), "实际: {err}");
        drop(held);
        // 守卫释放后同一 qname 可以正常走完（localhost 走 /etc/hosts，零网络）。
        assert!(
            system_query(&f, &wire).await.is_ok(),
            "守卫释放后不得留下永久占位"
        );
    }
}
