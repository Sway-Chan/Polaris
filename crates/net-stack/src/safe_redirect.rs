//! 带逐跳 SSRF 复检的安全重定向 fetch（上游 `main/safe-redirect-fetch.ts` 1:1 移植）。
//!
//! 默认 `redirect: follow` 会让首 URL 过 guard 后被 30x 跳到内网而不复检，构成 SSRF 绕过；
//! 逐跳复检是安全关键。本 helper 收口「redirect: manual 自管链 + 每跳 Location 过
//! [`assert_host_allowed`] + 重定向上限」。真实 HTTP 由注入的 [`HttpClient`] trait 承载
//! （测试 mock，不触碰宿主网络）。

#![forbid(unsafe_code)]

use std::future::Future;

use url::Url;

use crate::ssrf::{assert_host_allowed, DnsLookup};

/// 安全 fetch 拒绝原因：调用方据此映射 HTTP 状态码或区分错误类别。
/// 上游 `SafeFetchRejectReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeFetchRejectReason {
    /// 重定向目标协议不支持（仅允许 http/https），或 Location 非法。
    RedirectProtocol,
    /// SSRF guard 命中（首跳或某跳解析到内网）。
    Ssrf,
    /// 重定向次数超过上限。
    TooManyRedirects,
    /// 注入的 [`HttpClient`] 冒泡的网络错误（连接被拒 / 超时 / DNS 失败…）。
    ///
    /// **与 [`Ssrf`](SafeFetchRejectReason::Ssrf) 必须分开**：二者都会让 fetch 失败，但语义相反——
    /// 前者是「拒绝访问」（安全拦截，用户改 URL 也没用），后者是「连不上」（可重试）。混为一谈会让
    /// 每个 ECONNREFUSED 都被报成「订阅地址指向内网」，把真实网络故障的排查引到错误方向。
    Network,
}

/// redirect 链中的安全拒绝（区别于 HttpClient 网络错误）。上游 `SafeFetchError`。
#[derive(Debug, Clone)]
pub struct SafeFetchError {
    pub reason: SafeFetchRejectReason,
    pub message: String,
}

impl std::fmt::Display for SafeFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SafeFetchError {}

/// helper 容忍的最小 Response 形状（上游 `MinimalResponse` 的同构子集）。
///
/// **body 由实现侧读满后交回**（这是拉取流水线拿到正文的唯一通道）：实现侧须按
/// [`FetchInit::max_body_bytes`] **流式**累计，超限即中断连接并返回 `Err`——**不得截断后返回**
/// （截断会把超大订阅变成「解析出半截节点」的坏数据，比明确失败更糟）。
///
/// 30x 中间响应的 body 由本 helper 丢弃（不消费、体积小）；仅终态 response 的 body 有意义。
#[derive(Debug, Default)]
pub struct MinimalResponse {
    pub status: u16,
    /// location header（30x 时取）。大小写不敏感由实现保证。
    pub location: Option<String>,
    /// 全部响应头。供调用方取 content-length / etag / last-modified / subscription-userinfo。
    /// key 大小写不敏感——按名取用 [`MinimalResponse::header`]，勿直接比对。
    pub headers: Vec<(String, String)>,
    /// 已读取的响应体原始字节（是否为 UTF-8 由调用方判定）。
    pub body: Vec<u8>,
}

impl MinimalResponse {
    /// 按名取响应头（大小写不敏感，取首个命中）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 绑定好会话的 fetch（上游 `fetchImpl`）。helper 固定下发 redirect:'manual' 语义：
/// 实现方返回的 Response 不得自动跟随重定向（须原样返回 30x + Location）。
pub trait HttpClient: Send + Sync {
    /// 发起一次请求（manual redirect）。失败返回 Err（网络错误原样冒泡）。
    fn fetch(
        &self,
        url: &str,
        init: &FetchInit,
    ) -> impl Future<Output = Result<MinimalResponse, String>> + Send;
}

/// 单次请求参数（上游 `init`）。
#[derive(Debug, Clone, Default)]
pub struct FetchInit {
    pub user_agent: String,
    /// 附加请求头（条件 GET 验证器等非凭据，逐跳携带）。
    pub headers: Vec<(String, String)>,
    /// 单次请求超时（连接 + 读取），毫秒。`None` → 实现侧默认。
    /// 防 slow-loris 把拉取流水线挂死（上游 `MAIN_FETCH_TIMEOUT_MS`）。
    pub timeout_ms: Option<u64>,
    /// 响应体上限（字节）。实现侧须**流式**累计、超限即中断并 `Err`（防 OOM）。`None` → 不限。
    pub max_body_bytes: Option<usize>,
}

/// [`safe_redirect_fetch`] 配置。上游 `SafeRedirectFetchOptions`。
pub struct SafeRedirectFetchOptions<'a, H: HttpClient, L: DnsLookup> {
    /// 绑定好会话的 fetch 实现。
    pub fetch_impl: &'a H,
    /// 起始 URL，调用方已保证为 http(s)。
    pub url: &'a str,
    pub user_agent: String,
    /// 附加请求头（与 User-Agent 合并）。
    pub headers: Option<Vec<(String, String)>>,
    /// 仅实际经代理时豁免 Polaris FakeIP；直连/端口回退传 false。
    pub exempt_fake_ip: bool,
    /// 重定向上限（含首跳之外的跳数），默认 5。
    pub max_redirects: Option<usize>,
    /// 单次请求超时，透传 [`FetchInit::timeout_ms`]（逐跳各自计时）。
    pub timeout_ms: Option<u64>,
    /// 响应体上限，透传 [`FetchInit::max_body_bytes`]。
    pub max_body_bytes: Option<usize>,
    /// 注入 DNS 解析。
    pub lookup: &'a L,
}

const DEFAULT_MAX_REDIRECTS: usize = 5;

/// SSRF guard：[`assert_host_allowed`] 报错统一归类为 [`SafeFetchError`] reason=`Ssrf`，
/// 保留原始消息（含 hostname/解析结果）。上游 `guard`。
async fn guard<L: DnsLookup>(
    url: &Url,
    lookup: &L,
    exempt_fake_ip: bool,
) -> Result<(), SafeFetchError> {
    assert_host_allowed(url.host_str().unwrap_or(""), lookup, exempt_fake_ip)
        .await
        .map_err(|msg| SafeFetchError {
            reason: SafeFetchRejectReason::Ssrf,
            message: msg,
        })
}

/// 首跳 + 每一跳 Location 都过 SSRF guard（含 DNS 逐 IP 判定），通过才续跳，最多 maxRedirects 跳。
///
/// 返回终态（非 30x）response；中途 30x 的 response 由本 helper 忽略（其 body 未消费、
/// 由实现侧释放）。上游 `safeRedirectFetch`。
pub async fn safe_redirect_fetch<H: HttpClient, L: DnsLookup>(
    opts: SafeRedirectFetchOptions<'_, H, L>,
) -> Result<MinimalResponse, SafeFetchError> {
    let max_redirects = opts.max_redirects.unwrap_or(DEFAULT_MAX_REDIRECTS);

    let mut current_url = match Url::parse(opts.url) {
        Ok(u) => u,
        Err(_) => {
            return Err(SafeFetchError {
                reason: SafeFetchRejectReason::RedirectProtocol,
                message: "起始 URL 非法，已拒绝".to_string(),
            });
        }
    };

    // 首跳 guard（起始 URL 协议由调用方保证为 http(s)）。
    guard(&current_url, opts.lookup, opts.exempt_fake_ip).await?;

    let init = FetchInit {
        user_agent: opts.user_agent.clone(),
        headers: opts.headers.clone().unwrap_or_default(),
        timeout_ms: opts.timeout_ms,
        max_body_bytes: opts.max_body_bytes,
    };

    for hop in 0..=max_redirects {
        // 实现侧冒泡的网络错误 → reason=Network（**不是** Ssrf：见该 variant 的 doc）。
        let response = opts
            .fetch_impl
            .fetch(current_url.as_str(), &init)
            .await
            .map_err(|msg| SafeFetchError {
                reason: SafeFetchRejectReason::Network,
                message: msg,
            })?;

        let is_redirect = (300..400).contains(&response.status);
        let loc = if is_redirect {
            response.location.as_deref()
        } else {
            None
        };
        let Some(loc) = loc else {
            // 非 30x（或无 Location）→ 终态响应
            return Ok(response);
        };

        // 确认是 30x：续跳或抛错都已离开本跳响应。
        if hop >= max_redirects {
            return Err(SafeFetchError {
                reason: SafeFetchRejectReason::TooManyRedirects,
                message: format!("重定向次数超过上限（{max_redirects}），已拒绝"),
            });
        }
        // 相对 Location 以当前 URL 为基准解析。
        let next_obj = match current_url.join(loc) {
            Ok(u) => u,
            Err(_) => {
                return Err(SafeFetchError {
                    reason: SafeFetchRejectReason::RedirectProtocol,
                    message: "重定向目标非法，已拒绝".to_string(),
                });
            }
        };
        if next_obj.scheme() != "http" && next_obj.scheme() != "https" {
            return Err(SafeFetchError {
                reason: SafeFetchRejectReason::RedirectProtocol,
                message: "重定向目标协议不支持（仅允许 http/https），已拒绝".to_string(),
            });
        }
        // 重定向目标过同一 guard（含 DNS 解析）。
        guard(&next_obj, opts.lookup, opts.exempt_fake_ip).await?;
        current_url = next_obj;
    }

    // hop 循环 0..=max_redirects 必执行一次 fetch；理论上不会到达此处（最后一跳若无 Location
    // 已 return Ok；若有 Location 则 hop>=max_redirects 已 return Err）。
    Err(SafeFetchError {
        reason: SafeFetchRejectReason::TooManyRedirects,
        message: format!("重定向次数超过上限（{max_redirects}），已拒绝"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssrf::DnsLookup;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 测试用 mock DnsLookup：默认公网，可注入特定 hostname 的内网解析。
    struct MockLookup {
        private: HashMap<String, Vec<String>>,
    }

    impl DnsLookup for MockLookup {
        fn lookup_all(
            &self,
            host: &str,
        ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
            let res = self
                .private
                .get(host)
                .cloned()
                .unwrap_or_else(|| vec!["8.8.8.8".to_string()]);
            async move { Ok(res) }
        }
    }

    /// 测试用 mock HttpClient：按脚本返回预设 (status, location) 序列。
    #[allow(clippy::type_complexity)]
    struct MockFetch {
        /// url → 重定向链脚本（按请求序逐个弹出）。Mutex 保 Sync（HttpClient: Send + Sync）。
        scripts: Mutex<HashMap<String, Vec<(u16, Option<String>)>>>,
    }

    impl MockFetch {
        fn new() -> Self {
            Self {
                scripts: Mutex::new(HashMap::new()),
            }
        }
        fn set(&self, url: &str, chain: Vec<(u16, Option<String>)>) {
            self.scripts.lock().unwrap().insert(url.to_string(), chain);
        }
    }

    impl HttpClient for MockFetch {
        fn fetch(
            &self,
            url: &str,
            _init: &FetchInit,
        ) -> impl Future<Output = Result<MinimalResponse, String>> + Send {
            let mut scripts = self.scripts.lock().unwrap();
            let chain = scripts.get_mut(url);
            let resp = if let Some(c) = chain {
                if c.is_empty() {
                    MinimalResponse {
                        status: 500,
                        ..Default::default()
                    }
                } else {
                    let (status, location) = c.remove(0);
                    MinimalResponse {
                        status,
                        location,
                        ..Default::default()
                    }
                }
            } else {
                MinimalResponse {
                    status: 404,
                    ..Default::default()
                }
            };
            async move { Ok(resp) }
        }
    }

    fn opts<'a, H: HttpClient, L: DnsLookup>(
        fetch_impl: &'a H,
        lookup: &'a L,
        url: &'a str,
    ) -> SafeRedirectFetchOptions<'a, H, L> {
        SafeRedirectFetchOptions {
            fetch_impl,
            url,
            user_agent: "Polaris/0.1".to_string(),
            headers: None,
            exempt_fake_ip: false,
            max_redirects: None,
            timeout_ms: None,
            max_body_bytes: None,
            lookup,
        }
    }

    #[tokio::test]
    async fn no_redirect_returns_terminal() {
        let fetch = MockFetch::new();
        fetch.set("https://example.com/sub", vec![(200, None)]);
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://example.com/sub")).await;
        assert!(r.is_ok());
        assert_eq!(r.unwrap().status, 200);
    }

    #[tokio::test]
    async fn follows_safe_redirect_chain() {
        let fetch = MockFetch::new();
        fetch.set(
            "https://example.com/a",
            vec![(302, Some("https://cdn.example.com/b".to_string()))],
        );
        fetch.set("https://cdn.example.com/b", vec![(200, None)]);
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://example.com/a")).await;
        assert!(r.is_ok());
        assert_eq!(r.unwrap().status, 200);
    }

    #[tokio::test]
    async fn relative_location_resolved() {
        let fetch = MockFetch::new();
        fetch.set(
            "https://example.com/start",
            vec![(302, Some("/next".to_string()))],
        );
        fetch.set("https://example.com/next", vec![(200, None)]);
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://example.com/start")).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn redirect_to_private_ssrf_blocked() {
        let fetch = MockFetch::new();
        fetch.set(
            "https://evil.example.com/redir",
            vec![(
                302,
                Some("https://meta.evil.example.com/secret".to_string()),
            )],
        );
        let lk = MockLookup {
            private: HashMap::from([(
                "meta.evil.example.com".to_string(),
                vec!["169.254.169.254".to_string()],
            )]),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://evil.example.com/redir")).await;
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().reason, SafeFetchRejectReason::Ssrf);
    }

    #[tokio::test]
    async fn redirect_to_non_http_protocol_blocked() {
        let fetch = MockFetch::new();
        fetch.set(
            "https://example.com/f",
            vec![(302, Some("file:///etc/passwd".to_string()))],
        );
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://example.com/f")).await;
        assert!(r.is_err());
        assert_eq!(
            r.unwrap_err().reason,
            SafeFetchRejectReason::RedirectProtocol
        );
    }

    #[tokio::test]
    async fn too_many_redirects_blocked() {
        let fetch = MockFetch::new();
        // 循环重定向：a→b→a→b...（无终态）。max_redirects=3 ⇒ hop 0..3 续跳、hop=3 时触发 too-many。
        // MockFetch 每请求弹一个脚本条目，故每 URL 提供足够长度的链（4 跳：a 用 2 次、b 用 2 次）。
        let a_target = "https://loop.example.com/b".to_string();
        let b_target = "https://loop.example.com/a".to_string();
        fetch.set(
            "https://loop.example.com/a",
            vec![(302, Some(a_target.clone())), (302, Some(a_target))],
        );
        fetch.set(
            "https://loop.example.com/b",
            vec![(302, Some(b_target.clone())), (302, Some(b_target))],
        );
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let mut o = opts(&fetch, &lk, "https://loop.example.com/a");
        o.max_redirects = Some(3);
        let r = safe_redirect_fetch(o).await;
        assert!(r.is_err());
        assert_eq!(
            r.unwrap_err().reason,
            SafeFetchRejectReason::TooManyRedirects
        );
    }

    #[tokio::test]
    async fn first_hop_ssrf_blocked() {
        // 首跳 URL 本身解析到内网 → guard 首跳即拒
        let fetch = MockFetch::new();
        let lk = MockLookup {
            private: HashMap::from([(
                "internal.example.com".to_string(),
                vec!["10.0.0.5".to_string()],
            )]),
        };
        let r = safe_redirect_fetch(opts(&fetch, &lk, "https://internal.example.com/x")).await;
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().reason, SafeFetchRejectReason::Ssrf);
    }

    #[tokio::test]
    async fn max_redirects_zero_allows_one_fetch() {
        // max_redirects=0：允许首跳 fetch，但若首跳即 30x → too-many（hop>=max_redirects=0）
        let fetch = MockFetch::new();
        fetch.set("https://example.com/ok", vec![(200, None)]);
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let mut o = opts(&fetch, &lk, "https://example.com/ok");
        o.max_redirects = Some(0);
        let r = safe_redirect_fetch(o).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn max_redirects_zero_blocks_redirect() {
        let fetch = MockFetch::new();
        fetch.set(
            "https://example.com/r",
            vec![(302, Some("https://cdn.example.com/x".to_string()))],
        );
        let lk = MockLookup {
            private: HashMap::new(),
        };
        let mut o = opts(&fetch, &lk, "https://example.com/r");
        o.max_redirects = Some(0);
        let r = safe_redirect_fetch(o).await;
        assert!(r.is_err());
        assert_eq!(
            r.unwrap_err().reason,
            SafeFetchRejectReason::TooManyRedirects
        );
    }
}
