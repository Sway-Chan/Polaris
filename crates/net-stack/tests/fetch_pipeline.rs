//! 订阅拉取流水线端到端：**真实 HTTP 字节** → 拉取 → 解析 → 节点。
//!
//! 与 `subscription.rs` 内的 mock 单测互补：那层证「逻辑分支对」，这层证「trait 契约真能被
//! 真 HTTP 实现填满」——正文经真 socket 收发、响应头真解析、体积闸真在流式读取时截断。
//!
//! **宿主网络零触碰**：仅 `127.0.0.1:0` 临时端口的回环 listener（无 netns/veth/iptables/TUN/
//! 系统代理）。HTTP server 与 client 均用 **stdlib `std::net`**（不引依赖：简约阶梯 stdlib 优先）。
//!
//! **SSRF guard 与回环的处置**：订阅 URL 用公网域名 `sub.example.com`，注入的 DnsLookup 返回公网
//! IP → guard 放行；测试 client 的**传输层**再把连接落到回环 listener。这不是绕过 guard——guard 的
//! 判定对象是订阅 URL 的 hostname（见 `ssrf.rs` doc），传输落点由 client 实现决定，二者本就分层。
//! 直接用 `http://127.0.0.1:port` 会被 guard 正确拒掉（该行为另有单测覆盖）。

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

use polaris_net_stack::safe_redirect::{FetchInit, HttpClient, MinimalResponse};
use polaris_net_stack::singbox_import::ImportOrigin;
use polaris_net_stack::ssrf::DnsLookup;
use polaris_net_stack::subscription::{
    detect_format, fetch_subscription_full, parse_subscription, SubscriptionFormat,
    MAIN_FETCH_TIMEOUT_MS,
};
use polaris_net_stack::subscription_error::SubscriptionErrorKind;

// ── 本地 HTTP server（stdlib）────────────────────────────────────────────────

/// 起一个回环 HTTP server，按连接序逐个吐 `responses` 里的原始报文。
/// 返回监听地址；线程在服完全部响应后退出（测试结束即回收）。
fn spawn_server(responses: Vec<Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
    let addr = listener.local_addr().expect("取端口");
    thread::spawn(move || {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // 读完请求头即可（测试只发 GET，无 body）。
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(&resp);
            let _ = sock.flush();
            // drop → FIN，client 侧 read 到 EOF（配合 Connection: close）。
        }
    });
    addr
}

/// 构造一份 HTTP/1.1 响应报文。
fn http_response(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status_line}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

// ── 测试用 HttpClient（真 HTTP/1.1 over stdlib TcpStream）──────────────────────

/// 把**所有**请求落到 `addr` 的回环 client（URL 的 host 只用于 Host 头）。
///
/// 仅测试用：证明 [`HttpClient`] 契约（含 `max_body_bytes` 流式截断、响应头回传、
/// 30x 不自动跟随）可被真实 HTTP 实现满足。**不是**产品级客户端（无 TLS / 连接池 / 超时）。
struct LoopbackHttp {
    addr: SocketAddr,
}

impl LoopbackHttp {
    fn fetch_blocking(&self, url: &str, init: &FetchInit) -> Result<MinimalResponse, String> {
        let u = url::Url::parse(url).map_err(|e| format!("URL 非法: {e}"))?;
        let host = u.host_str().unwrap_or("localhost");
        let path = if let Some(q) = u.query() {
            format!("{}?{}", u.path(), q)
        } else {
            u.path().to_string()
        };

        let mut sock =
            TcpStream::connect(self.addr).map_err(|e| format!("tcp connect error: {e}"))?;
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {}\r\nConnection: close\r\n",
            init.user_agent
        );
        for (k, v) in &init.headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        sock.write_all(req.as_bytes())
            .map_err(|e| format!("写请求失败: {e}"))?;

        // 流式读取 + 体积闸：一旦 body 部分超过 max_body_bytes 立即断连接并 Err
        // （**不截断返回** —— 截断会让解析层拿到半截订阅，产出坏节点）。
        let max = init.max_body_bytes;
        let mut raw: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut head_end: Option<usize> = None;
        loop {
            let n = sock
                .read(&mut chunk)
                .map_err(|e| format!("读响应失败: {e}"))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if head_end.is_none() {
                head_end = find_head_end(&raw);
            }
            if let (Some(he), Some(max)) = (head_end, max) {
                if raw.len().saturating_sub(he) > max {
                    drop(sock);
                    return Err(format!("订阅响应体积超过上限 {max}，已拒绝"));
                }
            }
        }
        let head_end = head_end.ok_or_else(|| "响应无完整头部".to_string())?;
        let head = String::from_utf8_lossy(&raw[..head_end.saturating_sub(4)]).into_owned();
        let body = raw[head_end..].to_vec();

        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("状态行不可解析: {status_line}"))?;

        let mut headers = Vec::new();
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let location = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map(|(_, v)| v.clone());

        Ok(MinimalResponse {
            status,
            location,
            headers,
            body,
        })
    }
}

/// 定位 `\r\n\r\n` 之后的首字节下标。
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

impl HttpClient for LoopbackHttp {
    fn fetch(
        &self,
        url: &str,
        init: &FetchInit,
    ) -> impl Future<Output = Result<MinimalResponse, String>> + Send {
        // 同步完成后包成 ready future：阻塞 IO 不进 async 上下文。
        let res = self.fetch_blocking(url, init);
        async move { res }
    }
}

// ── 注入的 DnsLookup ─────────────────────────────────────────────────────────

struct MockLookup {
    map: HashMap<String, Vec<String>>,
}
impl MockLookup {
    /// 一切域名解析到公网 IP（guard 放行）。
    fn public() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}
impl DnsLookup for MockLookup {
    fn lookup_all(&self, host: &str) -> impl Future<Output = Result<Vec<String>, String>> + Send {
        let res = self
            .map
            .get(host)
            .cloned()
            .unwrap_or_else(|| vec!["93.184.216.34".to_string()]);
        async move { Ok(res) }
    }
}

/// 标准 base64 编码（测试内自足，不引依赖）。
fn b64(input: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = input.as_bytes();
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(if c.len() > 1 {
            T[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ── 端到端 ───────────────────────────────────────────────────────────────────

/// **主证据**：本地 HTTP server 供一份 base64 订阅 → 真 socket 拉取 → 解析 → 得到节点。
#[tokio::test]
async fn base64_subscription_over_real_http_yields_nodes() {
    let links = "ss://YWVzLTI1Ni1nY206cGFzc3dvcmQ@1.2.3.4:8388#node-a\n\
                 trojan://pw@5.6.7.8:443?sni=example.com#node-b\n";
    let body = b64(links);
    let addr = spawn_server(vec![http_response(
        "200 OK",
        &[("Content-Type", "text/plain")],
        body.as_bytes(),
    )]);

    let client = LoopbackHttp { addr };
    let text = fetch_subscription_full(
        &client,
        &MockLookup::public(),
        "https://sub.example.com/link?token=abc",
        "Polaris/0.1.0",
        None,
        false,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await
    .expect("拉取应成功");

    // 正文真的回来了（旧实现这里恒是空串）。
    assert_eq!(text.trim(), body.trim());
    assert_eq!(detect_format(text.trim()), SubscriptionFormat::Base64);

    let mut i = 0;
    let mut id_gen = || {
        i += 1;
        format!("id-{i}")
    };
    let parsed = parse_subscription(
        text.trim(),
        "sub-1",
        "2026-07-16T00:00:00Z",
        &mut id_gen,
        ImportOrigin::RemoteSubscription,
    );
    assert_eq!(parsed.servers.len(), 2, "warnings={:?}", parsed.warnings);
    assert_eq!(parsed.servers[0].address, "1.2.3.4");
    assert_eq!(parsed.servers[0].port, 8388);
    assert_eq!(parsed.servers[1].address, "5.6.7.8");
    assert_eq!(parsed.servers[1].port, 443);
}

/// 真 HTTP 30x：跳到公网 → 跟随；正文来自第二跳。
#[tokio::test]
async fn real_http_redirect_followed_to_public_target() {
    let links = "ss://YWVzLTI1Ni1nY206cGFzc3dvcmQ@9.9.9.9:8388#node-r\n";
    let body = b64(links);
    let addr = spawn_server(vec![
        http_response(
            "302 Found",
            &[("Location", "https://cdn.example.com/real")],
            b"",
        ),
        http_response("200 OK", &[], body.as_bytes()),
    ]);

    let client = LoopbackHttp { addr };
    let text = fetch_subscription_full(
        &client,
        &MockLookup::public(),
        "https://sub.example.com/start",
        "Polaris/0.1.0",
        None,
        false,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await
    .expect("公网重定向应被跟随");
    assert_eq!(text.trim(), body.trim());
}

/// **SSRF 变异验证靶子（真 HTTP 版）**：30x 跳到云元数据地址 → 逐跳复检必须拦下。
/// 打断 `safe_redirect_fetch` 的逐跳 guard → 本测试转红。
#[tokio::test]
async fn real_http_redirect_to_metadata_ip_is_blocked() {
    let addr = spawn_server(vec![http_response(
        "302 Found",
        &[("Location", "http://169.254.169.254/latest/meta-data/")],
        b"",
    )]);

    let client = LoopbackHttp { addr };
    let err = fetch_subscription_full(
        &client,
        &MockLookup::public(),
        "https://sub.example.com/start",
        "Polaris/0.1.0",
        None,
        false,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await
    .expect_err("重定向到 169.254.169.254 必须被拒");
    assert_eq!(err.kind, SubscriptionErrorKind::Ssrf);
}

/// 真 HTTP 非 2xx → Http + status。
#[tokio::test]
async fn real_http_404_classified_with_status() {
    let addr = spawn_server(vec![http_response("404 Not Found", &[], b"nope")]);
    let client = LoopbackHttp { addr };
    let err = fetch_subscription_full(
        &client,
        &MockLookup::public(),
        "https://sub.example.com/gone",
        "Polaris/0.1.0",
        None,
        false,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await
    .expect_err("404 必须失败");
    assert_eq!(err.kind, SubscriptionErrorKind::Http);
    assert_eq!(err.http_status, Some(404));
}

/// `FetchInit::max_body_bytes` 的**流式截断契约**：实现侧须在读取中断连接并 Err，
/// 而非读满后再判（否则 10MB 闸拦不住 OOM）。此处用小 cap 直接驱动 client。
#[tokio::test]
async fn client_enforces_streaming_body_cap() {
    let big = vec![b'a'; 64 * 1024];
    let addr = spawn_server(vec![http_response("200 OK", &[], &big)]);
    let client = LoopbackHttp { addr };

    let err = client
        .fetch(
            "https://sub.example.com/big",
            &FetchInit {
                user_agent: "Polaris/0.1.0".to_string(),
                max_body_bytes: Some(1024),
                ..Default::default()
            },
        )
        .await
        .expect_err("超过 cap 必须 Err");
    assert!(err.contains("超过上限"), "err={err}");
}

/// UA 真的进了线上报文（订阅 UA 是机场侧的识别位，丢了会被拦）。
#[tokio::test]
async fn user_agent_reaches_the_wire() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
        let _ = sock.write_all(&http_response("200 OK", &[], b"x"));
        req
    });

    let client = LoopbackHttp { addr };
    let _ = fetch_subscription_full(
        &client,
        &MockLookup::public(),
        "https://sub.example.com/x",
        "Polaris/9.9.9",
        None,
        false,
        MAIN_FETCH_TIMEOUT_MS,
    )
    .await;

    let req = seen.join().unwrap();
    assert!(req.contains("User-Agent: Polaris/9.9.9"), "req={req}");
    assert!(req.contains("Host: sub.example.com"), "req={req}");
}
