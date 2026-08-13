//! **单节点测速的 CONNECT 隧道探针**（对齐 上游 `SpeedTestService.measureViaTunnel` /
//! `measureWarmRtt`，`src/main/services/SpeedTestService.ts:990-1057` / `:1279-1330`）。
//!
//! 本模块只做「一条隧道 + 两次 GET」的传输与报文编解码；**计时结构**（一个总超时包住全程）在
//! [`crate::commands::speedtest::measure_warm_ttfb`]，**测谁 / 谁缺席**的裁定在 `commands/speedtest.rs`。
//!
//! # 根因：为什么必须是 CONNECT（真机实测 + sing-box 源码双证）
//!
//! 2026-07-30 真机实测：Polaris 与 上游 对同一订阅测出的延迟**相差巨大**，上游 明显更接近真实 TTFB。
//! 分析文档 `~/docs/polaris/design/polaris-speedtest-parity-2026-07-30.md` 的 **U3**（当时判为
//! 「需真机实测，code review 判不准」）由此被证实：改前的 Polaris 经 reqwest 向本机 http 入站发
//! **absolute-form** 请求，两次 GET 是否复用同一条**上游**连接取决于 sing-box 的入站实现。
//!
//! 现在这条不再是猜测 —— 内核侧 HTTP 入站的握手实现是 `github.com/sagernet/sing` 的
//! `protocol/http/handshake.go`（sing-box 的 `protocol/http` 入站直接复用它），本机 Go module cache
//! 里 `sing@v0.8.11-0.20260514110501-905ad103a4df` 的这段源码逐字说明了两件事：
//!
//! **① absolute-form 每一次请求都重拨一次出站（= 每次都含完整节点握手）**：
//!  - `:167` 每处理一个 absolute-form 请求就**新建一个 `http.Client`**（连同它自己的 `http.Transport`），
//!    `:183` `defer httpClient.CloseIdleConnections()` 在请求结束时把上游连接**显式拆掉**
//!    ⇒ 上游连接**结构性不可能**跨请求复用；
//!  - `:150` `keepAlive` 还额外要求客户端发了 `Proxy-Connection: keep-alive`（一个 hop-by-hop 遗留头，
//!    reqwest 不发）⇒ `:200` `response.Close = true`、`:210` `conn.Close()`，连**客户端←→代理**这一段
//!    也被内核在每次响应后关掉。
//!
//! 即：改前 Polaris 的 measured 那次 GET **必然**含一次完整的节点握手（proxy 协议握手 + 出口到目标的
//! TCP/TLS），数值系统性偏高。这与真机读数一致，也正是 上游 注释里「**避免赌核内部行为**」要躲的赌局
//! —— 只是赌的结果是输。
//!
//! **② CONNECT 200 之后握手才开始，故第一次 GET 必须丢弃**：
//!  - `:89` 收到 CONNECT 时内核**先**把 `HTTP/1.1 200 Connection established` 写回客户端，
//!  - `:104` **然后**才 `handler.NewConnectionEx(...)` 把连接交给路由/出站去拨号。
//!
//! 所以「收到 200」**不蕴含**「节点握手已完成」；握手落在**第一次 GET** 的往返里。这就是本模块发两次
//! GET 的判据（详见 [`WarmTunnel::get`] 的文档），不是保险，是必需 —— 只发一次会把握手原样收回 measured，
//! 退化成改前那个病。
//!
//! # 为什么不能「用 reqwest 剥离建连阶段」（这是本次选型里最容易走错的一步）
//!
//! reqwest/hyper 侧能拿到的「建连耗时」是**客户端 → 本机代理**的回环 TCP —— 几十微秒，压根不是问题所在。
//! 真正要剥掉的节点握手发生在 **sing-box 进程内部**，夹在「我们把请求字节写出去」与「我们收到响应首字节」
//! 之间，**从客户端侧不可分辨**：没有任何 HTTP 客户端 API 能把它从 TTFB 里减出来（它就在 TTFB 里，
//! 且没有任何信号标出它的边界）。
//!
//! CONNECT 之所以是**唯一**能从客户端侧确定性剥离握手的手段：它把「建连」与「发请求」拆成两个**各自可观测**
//! 的阶段 —— 隧道建立（CONNECT + 首次 GET 承担握手）与请求往返（第二次 GET）被 `200` 这条线 + 一次丢弃的
//! GET 切干净，剩下的 measured 就是纯请求往返。
//!
//! # 不触碰宿主网络
//!
//! 本模块的传输面全部只连 `127.0.0.1:<本机代理口>`；单测经**回环 mock 代理**（`tests` 子模块）覆盖
//! CONNECT 报文形态 / origin-form / 计第二次 / 非 2xx / 超时关 socket / TLS 腿接线，**不出网**。
//! 真延迟数值走真核真出站 = 真机门。

use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// 响应头累积上限：超过即判失败。
///
/// 没有这条上限时，一个「一直吐字节但永远不出 `\r\n\r\n`」的端点会让缓冲无界增长直到总超时才被掐断
/// （8s × K 槽的内存放大）。64 KiB 远大于任何正常响应头。
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// 单次 socket 读的块大小（响应头场景足够大，不必按页调优）。
const READ_CHUNK: usize = 8 * 1024;

// ══════════════════════════════════════════════════════════════════════════════
//  ① 纯逻辑：测速目标解析 + HTTP 报文编解码（无 I/O，全部可离线单测）
// ══════════════════════════════════════════════════════════════════════════════

/// 测速目标（URL → 隧道所需的四个要素）。对齐 上游 `shared/speed-test.ts SpeedTestTarget`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedTestTarget {
    /// 是否 https（真 ⇒ 隧道建成后还要在隧道上做一次 TLS 握手）。
    https: bool,
    /// 主机名（不含端口；IPv6 字面量保留方括号形态，直接可拼进 Host 头与 CONNECT authority）。
    host: String,
    /// 端口（未显式给出时按 scheme 取 80 / 443）。
    port: u16,
    /// 请求路径（含前导 `/` 与 query）。
    path: String,
    /// GET 的 `Host` 头值：标准端口省略端口，非标准端口带 `host:port`。
    host_header: String,
}

impl SpeedTestTarget {
    /// 解析测速 URL。非法 / 非 http(s) / 无 host / 端口越界 → `None`（调用方回落默认端点）。
    ///
    /// 复用 `reqwest::Url`（= `url` crate，reqwest 的公开再导出）—— 本仓已有依赖，不新引 URL 解析器。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let u = reqwest::Url::parse(raw.trim()).ok()?;
        let https = match u.scheme() {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        let host = u.host_str()?;
        if host.is_empty() {
            return None;
        }
        let default_port = if https { 443u16 } else { 80u16 };
        let port = u.port().unwrap_or(default_port);
        let path = match u.query() {
            Some(q) => format!("{}?{}", u.path(), q),
            None => u.path().to_string(),
        };
        let host_header = if port == default_port {
            host.to_string()
        } else {
            format!("{host}:{port}")
        };
        Some(Self {
            https,
            host: host.to_string(),
            port,
            path,
            host_header,
        })
    }

    /// CONNECT 的 authority-form 目标（**标准端口也显式带 port**）。
    ///
    /// 两条理由，缺一不可：① RFC 9110 §9.3.6 的 authority-form **要求**带端口，省了是协议违例；
    /// ② 上游 注释点名的那条 —— 省略端口后，「非标端口」与「标准端口」的拼接形态不再自洽，
    /// 一个 `host:8443` 目标会被误拼成 `host`（连到 443）。
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// CONNECT 请求报文（建隧道那一步）。
    fn connect_request(&self) -> String {
        let a = self.authority();
        format!("CONNECT {a} HTTP/1.1\r\nHost: {a}\r\n\r\n")
    }

    /// 隧道上的 GET 报文 —— **origin-form**（`GET /path HTTP/1.1`），不是 absolute-form。
    ///
    /// 隧道建成后我们面对的就是 origin 本身（内核已把字节透明转发到目标），absolute-form 在这里是
    /// **错的**：目标服务器会按 RFC 把它当作请求代理，通常回 400。改前那条路径发 absolute-form 是因为
    /// 它打的是**代理**（本机 http 入站），不是 origin —— 两者的请求行形态本就不同，这也是「退回
    /// absolute-form」这个变异会被 `tunnel_speaks_connect_then_origin_form_gets` 抓住的原因。
    ///
    /// 用 GET 而非 HEAD：默认端点 `generate_204` 为 GET 设计、204 规范无 body、连接可立即复用；
    /// HEAD 在部分端点上 405 或行为不一。
    fn get_request(&self) -> String {
        format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
            self.path, self.host_header
        )
    }

    /// TLS SNI 用的主机名（IPv6 字面量要脱掉方括号，`ServerName` 不收 `[::1]` 形态）。
    fn sni_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&self.host)
    }
}

/// 子串定位（`memmem` 级别的需求不值得引依赖）。
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 在缓冲里定位「**自 `HTTP/` 状态行锚点起**、响应头已收齐（`\r\n\r\n`）」的头部切片。
///
/// # 为什么必须从 `HTTP/` 锚定，而不是见 `\r\n\r\n` 就算
///
/// 第二次 GET 发出前我们只清空了**自己的**缓冲，第一次响应的 body 残余仍可能滞留在 socket 里、
/// 先于第二次响应到达。不锚定就会把残余里的空行当成「第二次响应头收齐」，上报值塌成 ≈0ms ——
/// 那比虚高更危险：一个坏节点会因为假的超低延迟被排到最前面。上游 对同一个坑有逐字相同的处置
/// （`SpeedTestService.ts:1310-1315`）。
///
/// 残余本身若含字面量 `HTTP/` 仍会误判 —— 这是 上游 同款的**已知有界残留**：默认端点是 204（规范无
/// body），自配端点才可能有 body，且要恰好在残余里出现 `HTTP/` 才命中。要根治需按 `Content-Length` /
/// chunked 精确排空第一次响应体，那是在测速路径上塞半个 HTTP 客户端，与收益不成比例。
fn find_response_head(buf: &[u8]) -> Option<&[u8]> {
    let anchor = find_subslice(buf, b"HTTP/")?;
    let rest = &buf[anchor..];
    let end = find_subslice(rest, b"\r\n\r\n")?;
    Some(&rest[..end + 4])
}

/// 从响应头（**自状态行起**）解析 3 位状态码。解析不出 → `None`。
///
/// 对齐 上游 `parseHttpStatusCode`（`shared/speed-test.ts`）的 `^HTTP\/\d(?:\.\d)?\s+(\d{3})\b`。
/// **解析不出必须判失败**（调用方 → `None` → 上层记 -1）：畸形响应被当成功记 TTFB，等于软重引入
/// 上游 issue #154 修好之前「错误页当成功」的那个问题。
fn parse_http_status_code(head: &[u8]) -> Option<u16> {
    if head.len() < 5 || !head[..5].eq_ignore_ascii_case(b"HTTP/") {
        return None;
    }
    let mut i = 5;
    if !head.get(i)?.is_ascii_digit() {
        return None;
    }
    i += 1;
    if head.get(i) == Some(&b'.') {
        i += 1;
        if !head.get(i)?.is_ascii_digit() {
            return None;
        }
        i += 1;
    }
    // 版本与状态码之间至少一个 SP/HTAB（不放行 CR/LF：那是跨行拼出来的假状态行）。
    if !matches!(head.get(i), Some(b' ' | b'\t')) {
        return None;
    }
    while matches!(head.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    let digits = head.get(i..i + 3)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // `\b`：第 4 位不得再是数字（`HTTP/1.1 2000` 不是状态码）。
    if head.get(i + 3).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let code = u16::from(digits[0] - b'0') * 100
        + u16::from(digits[1] - b'0') * 10
        + u16::from(digits[2] - b'0');
    (100..=599).contains(&code).then_some(code)
}

/// 测速目标响应是否「可接受为成功」：**仅 2xx**（含 `generate_204` 的 204 / 自配端点的 200）。
///
/// 非 2xx（3xx 重定向 / 4xx 如 CF-Workers 节点对 cp.cloudflare 的 403 / 5xx）判失败 —— 堵住
/// 「错误页被当成功记 TTFB」（上游 issue #154 ③）。
fn is_acceptable_status(code: u16) -> bool {
    (200..=299).contains(&code)
}

// ══════════════════════════════════════════════════════════════════════════════
//  ② 传输：CONNECT 建隧道 → （https）TLS 握手 → 同一条 socket 上两次 GET
// ══════════════════════════════════════════════════════════════════════════════

/// 「在**已建立**的隧道上发一次 GET 并读到响应头收齐」的能力 —— [`crate::commands::speedtest`] 的
/// 计时核对本模块的**唯一**注入面。
///
/// 抽成 trait 而不是把 [`SpeedTestTunnel`] 直接塞进计时核：计时核的不变量（一个总超时包住
/// CONNECT+TLS+GET1+GET2）必须能用 `#[tokio::test(start_paused = true)]` 的**假时钟**验证，而假时钟
/// 与真 socket 不能共存（真 I/O 挂起时 tokio 会自动推进时钟，测试变成不确定）。故计时核只见 trait。
pub trait WarmTunnel {
    /// 在隧道上发一次 GET，读到「**响应头收齐**」为止（不等 body —— 那才是 TTFB 的定义，
    /// 与 mihomo `client.Do` 收齐响应头即返回同口径）。
    ///
    /// 返回 `Some(是否 2xx)`；`None` = 传输错 / 对端过早关闭 / 响应头畸形 / 超 [`MAX_HEAD_BYTES`]。
    ///
    /// # 调用两次的判据（见模块文档②，有 sing 源码为证）
    ///
    /// 内核在 CONNECT 时**先回 200、后拨号**（`sing/protocol/http/handshake.go:89` → `:104`），故
    /// 「200 到手」不蕴含「节点握手完成」，握手落在第一次 GET 的往返里。第一次丢弃、第二次计时是
    /// **必需**，不是保险。
    ///
    /// 第二次的增量成本：隧道已建、TLS 已握手，只是在一条就绪 socket 上多一个请求往返（≈1 RTT），
    /// 且与第一次共用同一个 8s 总预算，不放大不可达节点的封顶耗时。
    fn get(&mut self) -> impl Future<Output = Option<bool>> + Send;
}

/// 隧道上的 HTTP/1.1 请求-响应循环（明文 socket 与隧道内 TLS 流共用同一份实现）。
pub struct TunnelConn<S> {
    conn: S,
    /// 预先编好的 GET 报文（两次 GET 逐字节相同）。
    request: Vec<u8>,
    /// 响应头累积缓冲（每次 GET 前整段清空，见 [`WarmTunnel::get`] 实现内注释）。
    buf: Vec<u8>,
}

impl<S> TunnelConn<S> {
    fn new(conn: S, target: &SpeedTestTarget) -> Self {
        Self {
            conn,
            request: target.get_request().into_bytes(),
            buf: Vec::new(),
        }
    }
}

impl<S> WarmTunnel for TunnelConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn get(&mut self) -> Option<bool> {
        // **整段清空**（不是只切掉上一次的响应头）：上一次响应的 body 残余绝不含本次响应数据
        // —— 本次请求下一行才发出去。只切响应头会让自配「非 204 带 body」端点的 body（含空行）
        // 污染本次判定、把值塌成 ≈0ms。对齐 上游 `SpeedTestService.ts:1302-1307`。
        self.buf.clear();
        self.conn.write_all(&self.request).await.ok()?;
        self.conn.flush().await.ok()?;
        let code = read_response_status(&mut self.conn, &mut self.buf).await?;
        Some(is_acceptable_status(code))
    }
}

/// 已建立的测速隧道：明文，或隧道之上的 TLS。
///
/// `Tls` 变体装箱：`TlsStream` 内含 rustls 的读写缓冲，裸放进 enum 会让整个 enum（含明文变体）
/// 一起变肥（clippy `large_enum_variant`）。
pub enum SpeedTestTunnel {
    Plain(TunnelConn<TcpStream>),
    Tls(Box<TunnelConn<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl WarmTunnel for SpeedTestTunnel {
    async fn get(&mut self) -> Option<bool> {
        match self {
            Self::Plain(c) => c.get().await,
            Self::Tls(c) => c.get().await,
        }
    }
}

/// 读到「响应头收齐」并返回状态码。`None` = 传输错 / 对端过早关闭 / 畸形 / 超上限。
async fn read_response_status<S: AsyncRead + Unpin>(
    conn: &mut S,
    buf: &mut Vec<u8>,
) -> Option<u16> {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        if let Some(head) = find_response_head(buf) {
            // 畸形状态行 → None（**不得**当成功：见 [`parse_http_status_code`] 文档）。
            return parse_http_status_code(head);
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return None;
        }
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // 对端在响应头收齐前关闭（上游的 `early-close`）
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 建 CONNECT 隧道：TCP 连本机代理口 → 发 CONNECT → 读响应（**非 2xx 即失败**）→ https 目标再在
/// 隧道上做一次 TLS 握手。
///
/// `proxy_port` 两条生产路径共用：主核池是 `probe-in-k` 的端口，临时核是它为该节点建的入站口
/// （回退腿则是 `mixed-in`）—— 三者都是本机 **http** 入站，说的是同一套 CONNECT。
///
/// # socket 生命周期（防 fd 泄漏）
///
/// 本函数**不 spawn 任何任务**：所有 I/O 都发生在返回的 future 自身里。调用方
/// （[`crate::commands::speedtest::measure_warm_ttfb`]）用 `tokio::time::timeout` 包住**冷建链那一段**
/// （CONNECT + TLS + 第一次 GET），超时即 drop 这个 future ⇒ `TcpStream`/`TlsStream` 随之析构、
/// fd 立即归还。若把 I/O 挪进
/// `tokio::spawn`，超时只会丢掉 join handle 而**任务仍在跑**，socket 挂在运行时里直到自己结束 ——
/// 这正是 上游 注释点名的那个坑（「持有所有已建立句柄，finish 时统一 destroy；大订阅并发 32 时累积」），
/// Rust 侧靠「不 spawn + drop 兜底」达成同一效果。门：`timeout_closes_the_socket_not_leaks_it`。
pub async fn open_tunnel(proxy_port: u16, target: &SpeedTestTarget) -> Option<SpeedTestTunnel> {
    let mut sock = TcpStream::connect(("127.0.0.1", proxy_port)).await.ok()?;
    // 关 Nagle：小请求的 TTFB 不该被 delayed-ACK / 合包拖慢（上游 `socket.setNoDelay(true)`）。
    let _ = sock.set_nodelay(true);

    sock.write_all(target.connect_request().as_bytes())
        .await
        .ok()?;
    sock.flush().await.ok()?;

    let mut buf = Vec::new();
    let code = read_response_status(&mut sock, &mut buf).await?;
    if !is_acceptable_status(code) {
        // 代理拒绝建隧道（典型 502 = 出站拨不通）→ 该节点本轮不可测，上层记 -1。
        return None;
    }
    // 200 之后缓冲里的任何残余都是代理抢发的非请求数据（sing 的 CONNECT 腿写完 200 就交棒，
    // 不会再写）→ 与 上游 一致地丢弃（node 的 'connect' 事件同样把 `head` 参数弃之不用）。

    if target.https {
        let connector = tokio_rustls::TlsConnector::from(tls_client_config()?);
        let server_name = rustls::pki_types::ServerName::try_from(target.sni_host())
            .ok()?
            .to_owned();
        let tls = connector.connect(server_name, sock).await.ok()?;
        return Some(SpeedTestTunnel::Tls(Box::new(TunnelConn::new(tls, target))));
    }
    Some(SpeedTestTunnel::Plain(TunnelConn::new(sock, target)))
}

// ══════════════════════════════════════════════════════════════════════════════
//  ③ 隧道内 TLS（仅 https 测速目标；**不校验证书**，理由见下）
// ══════════════════════════════════════════════════════════════════════════════

/// 隧道内 TLS 的 client 配置（进程内建一次）。
///
/// 用**显式** provider（`builder_with_provider`）而不是 `ClientConfig::builder()`：后者读进程全局
/// 默认 provider，未安装时**直接 panic**。测速是热路径且可能先于任何 reqwest client 跑起来
/// （`runtime/http.rs::install_ring_provider` 只在建 `HttpRuntime` 时才装），依赖全局状态等于把一个
/// 顺序耦合埋进 panic 路径。显式传 provider 后本模块与全局状态无关。
fn tls_client_config() -> Option<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .ok()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(MeasureOnlyVerifier(provider)))
                .with_no_client_auth();
            Some(Arc::new(cfg))
        })
        .clone()
}

/// **不校验服务端证书**的 verifier —— 只允许测速隧道用。
///
/// # 为什么可以不校验（以及为什么这不是安全降级）
///
/// 测速隧道从不读、也从不发任何内容数据：它发一个固定的 `GET <测速路径>`，只看响应**头**的状态码与
/// 到达时刻，body 一个字节都不读。它量的是**可达性与往返时延**，不是内容可信度 —— 中间人能做的最坏
/// 情形是让延迟数字变好看，而延迟数字本来就由被测节点全权决定（节点自己就是那条链路）。
/// 上游 在同一位置作同一决定（`tls.connect({ rejectUnauthorized: false })`，
/// `SpeedTestService.ts:1046`），注释写明「测速仅量可达性+TTFB，不校验证书（与 HTTP 路径等价）」。
///
/// 附带收益（诚实登记为收益而非动机）：不校验就不需要根证书源，省掉 `rustls-native-certs` /
/// `webpki-roots` 依赖与它们的平台 I/O。
///
/// # 边界必须有牙
///
/// 这个 verifier **只能**留在测速隧道里。泄漏到订阅拉取 / 内核下载 / 更新 / 解锁检测任何一条链上都是
/// 真实的安全降级（那些链路要读并**信任**内容）。边界由门
/// `dangerous_tls_verifier_stays_in_this_module` 守：全仓扫 `with_custom_certificate_verifier`，
/// 出现在本文件之外即转红。
#[derive(Debug)]
struct MeasureOnlyVerifier(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for MeasureOnlyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    /// 签名**照常验**（只放行「证书链不可信」，不放行「握手被伪造」）—— 少验一样是白费力气，
    /// 但多留一层就少一层被误当通用 client 复制走的诱因。
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
pub(crate) mod mock_proxy {
    //! 回环 mock 代理 —— 本模块与 `commands::speedtest` 的测试**共用**。
    //!
    //! 放在 `mod tests` 外面并 `pub(crate)`，是为了让 `commands::speedtest` 能用同一个假代理去驱动
    //! **生产入口** `measure_via_local_proxy`：只在本模块内测 `open_tunnel` 会留下「函数本身说 CONNECT、
    //! 但生产调用点已被换回 reqwest」这个假绿缺口（测方法体 ≠ 测接线）。
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ══════════════════════════════════════════════════════════════════════
    //  回环 mock 代理：**只绑 127.0.0.1，不触碰宿主网络**
    //
    //  形态 = 一个说 HTTP CONNECT 的假代理。它把收到的请求行逐条记下来（供断言 CONNECT /
    //  origin-form），按脚本回响应。真核 / 真节点 / 真目标一个都不涉及。
    // ══════════════════════════════════════════════════════════════════════

    /// mock 代理对**一次 GET** 的应答脚本。
    #[derive(Clone)]
    pub(crate) struct GetReply {
        /// 应答前先等多久（用来验「计的是第二次 GET」：把延迟放在第一次上）。
        pub(crate) delay: Duration,
        /// 原样写回的响应字节（含状态行与 `\r\n\r\n`）。
        pub(crate) raw: &'static str,
    }

    impl GetReply {
        pub(crate) fn ok() -> Self {
            Self {
                delay: Duration::ZERO,
                raw: "HTTP/1.1 204 No Content\r\n\r\n",
            }
        }
        pub(crate) fn delayed(ms: u64) -> Self {
            Self {
                delay: Duration::from_millis(ms),
                ..Self::ok()
            }
        }
        pub(crate) fn raw(raw: &'static str) -> Self {
            Self {
                delay: Duration::ZERO,
                raw,
            }
        }
    }

    /// mock 代理脚本。
    pub(crate) struct Script {
        /// CONNECT 的应答（`None` = 收到 CONNECT 后什么都不回，用来验总超时）。
        pub(crate) connect_reply: Option<&'static str>,
        /// 建隧道后逐次 GET 的应答；用尽后不再应答（挂住，交给总超时）。
        pub(crate) gets: Vec<GetReply>,
    }

    /// mock 代理的观测面（测试断言的取材）。
    #[derive(Default)]
    pub(crate) struct Observed {
        /// 按序记下的请求行（第 0 条是 CONNECT，之后是隧道内的 GET）。
        pub(crate) request_lines: Vec<String>,
        /// 隧道内客户端发来的**首个数据块**（https 腿用来抓 ClientHello 字节）。
        pub(crate) first_tunnel_bytes: Vec<u8>,
        /// 服务端在脚本跑完后读到 EOF（= 客户端把 socket 关了）。
        pub(crate) saw_client_close: bool,
    }

    /// 起一个回环 mock 代理，返回 `(端口, 观测面)`。
    pub(crate) async fn spawn_mock_proxy(script: Script) -> (u16, Arc<Mutex<Observed>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 回环端口");
        let port = listener.local_addr().expect("取端口").port();
        let observed = Arc::new(Mutex::new(Observed::default()));
        let sink = observed.clone();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];

            // ── CONNECT ──
            let line = loop {
                let Ok(n) = sock.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = find_subslice(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    buf.drain(..end + 4);
                    break head.lines().next().unwrap_or_default().to_string();
                }
            };
            sink.lock().unwrap().request_lines.push(line);
            let Some(reply) = script.connect_reply else {
                // 不回 CONNECT 响应：客户端应挂到总超时，随后**关掉 socket**（本测的观测点）。
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                sink.lock().unwrap().saw_client_close = n == 0;
                return;
            };
            if sock.write_all(reply.as_bytes()).await.is_err() {
                return;
            }

            // ── 隧道内逐次 GET ──
            for reply in script.gets {
                let line = loop {
                    if let Some(end) = find_subslice(&buf, b"\r\n\r\n") {
                        let head = buf[..end].to_vec();
                        buf.drain(..end + 4);
                        break head;
                    }
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        sink.lock().unwrap().saw_client_close = true;
                        return;
                    }
                    {
                        let mut o = sink.lock().unwrap();
                        if o.first_tunnel_bytes.is_empty() {
                            o.first_tunnel_bytes = chunk[..n].to_vec();
                        }
                    }
                    buf.extend_from_slice(&chunk[..n]);
                };
                let text = String::from_utf8_lossy(&line).to_string();
                sink.lock()
                    .unwrap()
                    .request_lines
                    .push(text.lines().next().unwrap_or_default().to_string());
                if !reply.delay.is_zero() {
                    tokio::time::sleep(reply.delay).await;
                }
                if sock.write_all(reply.raw.as_bytes()).await.is_err() {
                    return;
                }
            }

            // 脚本跑完（或 `gets` 本就为空）：再读一次。两个用途 —— ① 观测客户端有没有把 socket 关掉；
            // ② 兜住「隧道内首个数据块压根不是 GET」的形态（https 腿发的是 TLS ClientHello，不会走上面
            // 那个按 `\r\n\r\n` 定界的 GET 循环）。
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            let mut o = sink.lock().unwrap();
            if n > 0 && o.first_tunnel_bytes.is_empty() {
                o.first_tunnel_bytes = chunk[..n].to_vec();
            }
            o.saw_client_close = n == 0;
        });
        (port, observed)
    }

    pub(crate) const OK_204: &str = "HTTP/1.1 200 Connection established\r\n\r\n";
}

#[cfg(test)]
mod tests {
    use super::mock_proxy::*;
    use super::*;
    use std::time::Duration;

    // ══════════════════════════════════════════════════════════════════════
    //  纯逻辑：目标解析 + 报文编解码
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn parses_default_endpoint_and_derives_port_path_host_header() {
        let t = SpeedTestTarget::parse("http://www.gstatic.com/generate_204")
            .expect("默认端点必可解析");
        assert!(!t.https);
        assert_eq!(t.host, "www.gstatic.com");
        assert_eq!(t.port, 80);
        assert_eq!(t.path, "/generate_204");
        assert_eq!(
            t.host_header, "www.gstatic.com",
            "标准端口的 Host 头不带端口"
        );
    }

    #[test]
    fn https_default_port_and_non_standard_port_shape() {
        let t = SpeedTestTarget::parse("https://example.com/x?a=1").expect("https 可解析");
        assert!(t.https);
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/x?a=1", "query 必须保留（自配端点常带鉴权参数）");
        assert_eq!(t.host_header, "example.com");

        let t = SpeedTestTarget::parse("https://example.com:8443/p").expect("非标端口可解析");
        assert_eq!(t.port, 8443);
        assert_eq!(
            t.host_header, "example.com:8443",
            "非标端口的 Host 头必须带端口"
        );
    }

    #[test]
    fn rejects_non_http_schemes_and_hostless_urls() {
        for bad in [
            "socks5://1.2.3.4:1080",
            "ftp://example.com/x",
            "http://",
            "not a url",
            "",
        ] {
            assert!(
                SpeedTestTarget::parse(bad).is_none(),
                "`{bad}` 不该被当成合法测速目标"
            );
        }
    }

    /// 🔴 CONNECT 的 authority-form **标准端口也必须显式带 port**。
    ///
    /// 牙：把 [`SpeedTestTarget::authority`] 改成「标准端口省略端口」→ 本测转红。
    #[test]
    fn connect_request_always_carries_an_explicit_port() {
        let t = SpeedTestTarget::parse("http://www.gstatic.com/generate_204").unwrap();
        assert_eq!(
            t.connect_request(),
            "CONNECT www.gstatic.com:80 HTTP/1.1\r\nHost: www.gstatic.com:80\r\n\r\n"
        );
        let t = SpeedTestTarget::parse("https://example.com/x").unwrap();
        assert!(t
            .connect_request()
            .starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
    }

    /// 🔴 隧道上的 GET 必须是 **origin-form**（absolute-form 是改前那条错路的形态）。
    #[test]
    fn tunnel_get_is_origin_form_not_absolute_form() {
        let t = SpeedTestTarget::parse("http://www.gstatic.com/generate_204").unwrap();
        let req = t.get_request();
        assert!(
            req.starts_with("GET /generate_204 HTTP/1.1\r\n"),
            "隧道内应发 origin-form，实得: {req:?}"
        );
        assert!(
            !req.contains("GET http://"),
            "绝不能退回 absolute-form（那是打代理的形态，打 origin 会被判 400）"
        );
        assert!(req.contains("\r\nHost: www.gstatic.com\r\n"));
    }

    #[test]
    fn parses_status_codes_and_rejects_malformed_status_lines() {
        assert_eq!(
            parse_http_status_code(b"HTTP/1.1 204 No Content\r\n\r\n"),
            Some(204)
        );
        assert_eq!(
            parse_http_status_code(b"HTTP/1.0 200 OK\r\n\r\n"),
            Some(200)
        );
        assert_eq!(
            parse_http_status_code(b"HTTP/2 403 Forbidden\r\n\r\n"),
            Some(403)
        );
        // 畸形一律 None —— 绝不当成功（否则错误页会被记成一个漂亮的 TTFB）。
        for bad in [
            &b"NOTHTTP/1.1 200 OK\r\n\r\n"[..],
            &b"HTTP/1.1 2000 X\r\n\r\n"[..],
            &b"HTTP/1.1  \r\n\r\n"[..],
            &b"HTTP/1.1\r\n200\r\n\r\n"[..],
            &b"HTTP/x.y 200 OK\r\n\r\n"[..],
        ] {
            assert_eq!(
                parse_http_status_code(bad),
                None,
                "畸形状态行必须判失败: {bad:?}"
            );
        }
    }

    #[test]
    fn only_2xx_is_acceptable() {
        assert!(
            is_acceptable_status(200) && is_acceptable_status(204) && is_acceptable_status(299)
        );
        for code in [199u16, 300, 301, 403, 502] {
            assert!(!is_acceptable_status(code), "{code} 不该被当成功");
        }
    }

    /// 🔴 第二次响应必须从 `HTTP/` 锚点起判「收齐」—— 不锚定会把第一次响应的 body 残余
    /// 当成第二次的响应头，值塌成 ≈0ms。
    ///
    /// 牙：把 [`find_response_head`] 里的锚点去掉、改成直接找 `\r\n\r\n` → 本测转红。
    #[test]
    fn response_head_scan_anchors_on_the_status_line() {
        // 缓冲里先是上一次响应的 body 残余（含空行！），之后才是本次的响应头。
        let buf = b"leftover body\r\n\r\nHTTP/1.1 204 No Content\r\nX: 1\r\n\r\n";
        let head = find_response_head(buf).expect("锚定后应能找到本次响应头");
        assert!(
            head.starts_with(b"HTTP/1.1 204"),
            "实得: {:?}",
            String::from_utf8_lossy(head)
        );
        assert_eq!(parse_http_status_code(head), Some(204));

        // 头未收齐时必须继续等（返回 None），不能拿半截头去判状态码。
        assert!(find_response_head(b"HTTP/1.1 204 No Content\r\nX: 1\r\n").is_none());
        assert!(find_response_head(b"garbage without a status line\r\n\r\n").is_none());
    }

    fn http_target() -> SpeedTestTarget {
        SpeedTestTarget::parse("http://www.gstatic.com/generate_204").unwrap()
    }

    /// 走完整条腿（建隧道 + 两次 GET，第二次计时）—— 与生产同一条代码路径，只是预算可注入。
    ///
    /// 生产是**两段**预算（冷建链 / 复用请求，见 `commands::speedtest::measure_warm_ttfb`）；本模块的门
    /// 测的是**线级报文形态与 socket 生命周期**，不测分段边界（那条在 `commands::speedtest` 的假时钟门里
    /// ——真 socket 与假时钟不能共存）。故这里两段注入同一个值，语义等价于原来的单一 `total`。
    async fn measure(port: u16, target: &SpeedTestTarget, budget: Duration) -> Option<u32> {
        crate::commands::speedtest::measure_warm_ttfb(budget, budget, open_tunnel(port, target))
            .await
    }

    /// 🔴 **结构事实门**：本腿说的是 CONNECT + origin-form GET，不是 absolute-form。
    ///
    /// 牙：把 [`open_tunnel`] 换回「经 reqwest 本机代理发 absolute-form」→ mock 代理收到的首个请求行
    /// 不再是 `CONNECT ...`（而是 `GET http://... HTTP/1.1`）→ 本测转红。把 [`SpeedTestTarget::get_request`]
    /// 改回 absolute-form → 第二/三条断言转红。
    #[tokio::test]
    async fn tunnel_speaks_connect_then_origin_form_gets() {
        let (port, observed) = spawn_mock_proxy(Script {
            connect_reply: Some(OK_204),
            gets: vec![GetReply::ok(), GetReply::ok()],
        })
        .await;
        let out = measure(port, &http_target(), Duration::from_secs(5)).await;
        assert!(out.is_some(), "mock 代理按脚本回 204，应出值");

        let lines = observed.lock().unwrap().request_lines.clone();
        assert_eq!(
            lines.first().map(String::as_str),
            Some("CONNECT www.gstatic.com:80 HTTP/1.1"),
            "首个请求行必须是带显式端口的 CONNECT，实得 {lines:?}"
        );
        assert_eq!(lines.len(), 3, "CONNECT + 两次 GET，实得 {lines:?}");
        for line in &lines[1..] {
            assert_eq!(
                line, "GET /generate_204 HTTP/1.1",
                "隧道内必须是 origin-form，实得 {line:?}"
            );
        }
    }

    /// 🔴 **计的是第二次 GET**：把 500ms 延迟放在**第一次**应答上，测得值必须远小于它。
    ///
    /// 牙：把计时核改成「量第一次 GET」→ 测得值 ≈500ms → 本测转红。
    #[tokio::test]
    async fn measures_the_second_get_not_the_first() {
        let (port, _) = spawn_mock_proxy(Script {
            connect_reply: Some(OK_204),
            gets: vec![GetReply::delayed(500), GetReply::ok()],
        })
        .await;
        let out = measure(port, &http_target(), Duration::from_secs(5))
            .await
            .expect("两次应答都正常，应出值");
        assert!(
            out < 250,
            "第一次（暖身）承担了 500ms 延迟，measured 只应量第二次的往返；实得 {out}ms"
        );
    }

    /// 🔴 **非 2xx 一律判失败**（隧道建立与目标响应两处各一条）。
    ///
    /// 牙：把 [`is_acceptable_status`] 放宽成「有状态码就算成功」→ 两条断言全红。
    #[tokio::test]
    async fn non_2xx_is_never_counted_as_success() {
        // ① CONNECT 被拒（502 = 出站拨不通）→ 隧道建不成 → None。
        let (port, _) = spawn_mock_proxy(Script {
            connect_reply: Some("HTTP/1.1 502 Bad Gateway\r\n\r\n"),
            gets: vec![],
        })
        .await;
        assert_eq!(
            measure(port, &http_target(), Duration::from_secs(5)).await,
            None,
            "CONNECT 非 2xx 必须判失败"
        );

        // ② 目标对 measured 那次回 403（CF-Workers 节点的典型形态）→ None。
        let (port, _) = spawn_mock_proxy(Script {
            connect_reply: Some(OK_204),
            gets: vec![
                GetReply::ok(),
                GetReply::raw("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n"),
            ],
        })
        .await;
        assert_eq!(
            measure(port, &http_target(), Duration::from_secs(5)).await,
            None,
            "measured 那次非 2xx 必须判失败（错误页绝不当成功记 TTFB）"
        );
    }

    /// 对端在响应头收齐前关闭（上游的 `early-close`）→ `None`，绝不伪造数值。
    #[tokio::test]
    async fn early_close_yields_none() {
        let (port, _) = spawn_mock_proxy(Script {
            connect_reply: Some(OK_204),
            gets: vec![GetReply::raw("HTTP/1.1 204 No")], // 半截响应头后 mock 退出 → 客户端读到 EOF
        })
        .await;
        assert_eq!(
            measure(port, &http_target(), Duration::from_secs(5)).await,
            None
        );
    }

    /// 🔴 **超时路径必须真把 socket 关掉**（而不是把它挂在一个还在跑的 tokio 任务里）。
    ///
    /// mock 代理收到 CONNECT 后**什么都不回**；客户端应在注入的短总超时后放弃，且服务端随即读到
    /// EOF —— 那就是 fd 已归还的线级证据。
    ///
    /// 牙：把 [`open_tunnel`] 的 I/O 挪进 `tokio::spawn`（只 await join handle）→ 超时只丢 handle、
    /// 任务与 socket 继续存活 → 服务端读不到 EOF → 本测转红。
    #[tokio::test]
    async fn timeout_closes_the_socket_not_leaks_it() {
        let (port, observed) = spawn_mock_proxy(Script {
            connect_reply: None,
            gets: vec![],
        })
        .await;
        let out = measure(port, &http_target(), Duration::from_millis(300)).await;
        assert_eq!(out, None, "隧道建不成必须返回 None（上层记 -1）");

        // 给 mock 一点时间观察到 EOF（客户端 drop 是同步的，这里只是让出调度）。
        for _ in 0..50 {
            if observed.lock().unwrap().saw_client_close {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            observed.lock().unwrap().saw_client_close,
            "总超时后客户端必须把隧道 socket 关掉（否则大订阅并发时 fd 会累积）"
        );
    }

    /// 🔴 **https 腿真的在隧道上做 TLS 握手**（不是悄悄退回明文）。
    ///
    /// 不起真 TLS server：让 mock 代理接下 CONNECT 后把客户端发来的**首个数据块**抓下来，断言它是
    /// 一条 TLS ClientHello 记录、且 SNI 里带目标主机名。握手随后当然会失败（对端不是 TLS server）
    /// —— 不影响，本门只看已抓到的字节。**不触碰宿主网络。**
    ///
    /// 牙：把 [`open_tunnel`] 里的 `target.https` 分支删掉（一律走明文）→ 首个数据块变成
    /// `GET /x HTTP/1.1` → 本测转红。
    #[tokio::test]
    async fn https_target_performs_tls_handshake_inside_the_tunnel() {
        let (port, observed) = spawn_mock_proxy(Script {
            connect_reply: Some(OK_204),
            gets: vec![],
        })
        .await;
        let target = SpeedTestTarget::parse("https://speed.example.com/probe").unwrap();
        let out = measure(port, &target, Duration::from_millis(800)).await;
        assert_eq!(out, None, "对端不是真 TLS server，握手必失败 → None");

        let first = observed.lock().unwrap().first_tunnel_bytes.clone();
        assert!(
            first.len() > 5 && first[0] == 0x16 && first[1] == 0x03,
            "隧道内首个数据块应是 TLS handshake 记录（0x16 0x03..），实得 {:02x?}",
            &first[..first.len().min(16)]
        );
        assert!(
            find_subslice(&first, b"speed.example.com").is_some(),
            "ClientHello 应带 SNI = 目标主机名（否则真 https 端点会拿错证书/被拒）"
        );
    }

    /// 🔵 **边界门**：不校验证书的 verifier 只许留在本模块。
    ///
    /// 泄漏到订阅拉取 / 内核下载 / 更新 / 解锁检测任何一条链上都是真实的安全降级（那些链路要读并
    /// **信任**内容，而测速只看状态码与时刻）。独立函数只是让泄漏「需要显式复制一段代码」，本门让它
    /// **直接转红**。
    ///
    /// 牙：在 `src-tauri/src/` 任何其它文件里写 `with_custom_certificate_verifier(` → 本门转红。
    #[test]
    fn dangerous_tls_verifier_stays_in_this_module() {
        const NEEDLE: &str = "with_custom_certificate_verifier";
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("读 src 目录") {
                let path = entry.expect("目录项").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("speedtest_tunnel.rs")
                    && std::fs::read_to_string(&path)
                        .is_ok_and(|src| src.contains(&format!("{NEEDLE}(")))
                {
                    offenders.push(path.display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "不校验证书的 TLS verifier 只允许出现在测速隧道模块（其它链路要信任内容）。越界:\n{}",
            offenders.join("\n")
        );
    }
}
