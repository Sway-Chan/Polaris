//! 订阅 URL 安全校验 + 拉取 + 解析调度（Polaris SubscriptionService 的纯逻辑切片 1:1 移植）。
//!
//! 纯逻辑：真实 HTTP 请求由注入的 [`crate::safe_redirect::HttpClient`] trait 承载（测试 mock /
//! 本地 HTTP server，不触碰宿主网络）。职责：
//! - [`fetch_subscription_full`]：起始 URL 协议校验 + SSRF guard + safe-redirect-fetch（逐跳复检）
//!   + HTTP 状态校验 + 体积闸，**返回正文文本**。
//! - [`parse_subscription`]：判定格式（Clash / sing-box JSON / base64 / url-list）后分发解析；
//!   Clash 走 [`crate::clash_parser`]，其余格式由调用方（运行时层）按需扩展。
//! - 错误分类见 [`crate::subscription_error`]（审计 §C4）。
//!
//! proxy-providers 的并发编排属运行时层（依赖真实 HTTP 并发 + 超时），不在此纯逻辑移植；
//! 本模块仅暴露单 provider 拉取 + 解析的纯函数 [`fetch_and_parse_provider`]。
//!
//! **已移植**：条件 GET（`If-None-Match` / `If-Modified-Since` → 304 短路，见 [`Conditional`] 与
//! [`fetch_subscription_with_meta`]）与 `subscription-userinfo`（流量/到期元数据，见
//! [`parse_user_info`] / [`SubscriptionUserInfo`]）。304 **不再**归
//! [`SubscriptionErrorKind::Http`]，而是短路成 `not_modified=true` —— 且带 fail-safe：
//! 本次未发条件头却收 304 一律不认（见 [`fetch_core`] 步骤 3.5）。

#![forbid(unsafe_code)]

use url::Url;

use polaris_config_engine::user_config::server_config::ServerConfig;

use crate::clash_parser::{self, ClashParseResult};
use crate::safe_redirect::{
    safe_redirect_fetch, HttpClient, SafeFetchRejectReason, SafeRedirectFetchOptions,
};
use crate::singbox_import::ImportOrigin;
use crate::ssrf::DnsLookup;
use crate::subscription_error::{
    classify_subscription_error, SubscriptionErrorKind, SubscriptionErrorSignal,
};

/// 订阅响应体上限（10 MB）。上游 `SubscriptionService.MAX_BODY_BYTES` 同口径，
/// 与 `local_import_parse` 的体积闸一致（同一份正文，两条入口不该有两个阈值）。
///
/// 双闸：content-length 预检（早拒）+ 读取侧字节累计（content-length 可缺失/撒谎）。
/// 兼作 YAML 锚点炸弹的输入面收窄。
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// 主订阅拉取超时（30s）。上游 `MAIN_FETCH_TIMEOUT_MS`。
/// 防 slow-loris 挂死拉取流水线（scheduler `isRunning` 永真 → 后续更新全卡）。
pub const MAIN_FETCH_TIMEOUT_MS: u64 = 30_000;

/// proxy-provider 拉取超时（15s，比主订阅紧）。Polaris provider 编排口径。
pub const PROVIDER_FETCH_TIMEOUT_MS: u64 = 15_000;

/// 默认订阅 UA：中性 `Polaris/<version>`（不带 clash.meta/mihomo 标识）。
///
/// **勿用于 GitHub API / 资源下载**：带版本号会泄漏客户端指纹，那条链路应使用应用自标识 UA。
pub fn default_subscription_user_agent(version: &str) -> String {
    format!("Polaris/{version}")
}

/// 订阅流量/到期元数据（`Subscription-UserInfo` 响应头解析）。上游 `SubscriptionConfig['userInfo']`。
///
/// 字节数与到期时间戳均以 `u64` 承载（上游 用 `number`；流量总量可超 `u32`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionUserInfo {
    /// 已上传字节。
    pub upload: Option<u64>,
    /// 已下载字节。
    pub download: Option<u64>,
    /// 总流量字节。
    pub total: Option<u64>,
    /// 到期时间（Unix 秒）。
    pub expire: Option<u64>,
}

impl SubscriptionUserInfo {
    /// 至少解出一个字段才算「有」（对齐 上游 `Object.keys(result).length > 0`）。
    fn is_present(&self) -> bool {
        self.upload.is_some()
            || self.download.is_some()
            || self.total.is_some()
            || self.expire.is_some()
    }

    /// 序列化为前端 `userInfo` 形态（缺省字段不落键，对齐 TS `skip_serializing_if`）。
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        if let Some(v) = self.upload {
            m.insert("upload".into(), v.into());
        }
        if let Some(v) = self.download {
            m.insert("download".into(), v.into());
        }
        if let Some(v) = self.total {
            m.insert("total".into(), v.into());
        }
        if let Some(v) = self.expire {
            m.insert("expire".into(), v.into());
        }
        serde_json::Value::Object(m)
    }
}

/// 解析 `Subscription-UserInfo` 头（`upload=..; download=..; total=..; expire=..`）。
///
/// 上游 `SubscriptionService.parseUserInfo` 1:1：分号分段、`key=value`、`parseInt` 容错
/// （非数字段跳过），全缺 → `None`。`parseInt` 语义 = 取前导十进制数字（`"123abc"` → 123），
/// 用 `u64` 承载（负数/溢出 → 跳过该字段，不整体失败）。
#[must_use]
pub fn parse_user_info(header: Option<&str>) -> Option<SubscriptionUserInfo> {
    let header = header?;
    let mut result = SubscriptionUserInfo::default();
    for part in header.split(';') {
        let part = part.trim();
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let Some(num) = parse_int_prefix(value.trim()) else {
            continue;
        };
        match key.trim() {
            "upload" => result.upload = Some(num),
            "download" => result.download = Some(num),
            "total" => result.total = Some(num),
            "expire" => result.expire = Some(num),
            _ => {}
        }
    }
    result.is_present().then_some(result)
}

/// JS `parseInt(s, 10)` 的窄化：取前导十进制数字（首个非数字截断），无前导数字 → `None`。
/// 机场偶尔在 total 后带单位/注释（`"107374182400 bytes"`）；忠实 `parseInt` 只取数字段。
fn parse_int_prefix(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

/// 条件 GET 验证器（上次 200 响应的 `ETag` / `Last-Modified`）。上游 `{ etag, lastModified }`。
///
/// 缓存验证器非凭据（逐跳携带无泄漏面）；缺省 = 首次/无验证器 → 全量 GET（零回归）。
#[derive(Debug, Clone, Default)]
pub struct Conditional {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Conditional {
    fn has_any(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

/// 订阅拉取产出（正文 + 元数据）。上游 `fetchSubscriptionText` 返回体。
///
/// `not_modified=true`（304 命中，仅当本次确实发了条件头）→ `text` 空、调用方短路 parse/reconcile。
#[derive(Debug, Clone, Default)]
pub struct FetchedSubscription {
    pub text: String,
    pub user_info: Option<SubscriptionUserInfo>,
    /// 本次 200 响应的验证器（回写 sub，下次条件 GET 用）。
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// 304 Not Modified（条件 GET 命中）。
    pub not_modified: bool,
}

/// 脱敏 URL 供错误文案/日志使用：**去掉 query 与 userinfo**。
///
/// 订阅 token 就在 query 里（`?token=xxx`），原样进错误文案 = 凭据进日志/上报。
/// 上游 `SubscriptionService.redactUrl` 同职责；此处额外清 userinfo（`user:pass@`），
/// 且不走 `origin`——`origin` 对非特殊 scheme（如 `ftp:`）会序列化成 `null`，
/// 而「协议不支持」的错误文案恰恰需要显示原始 scheme 才有诊断价值。
pub fn redact_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            let had_query = u.query().is_some();
            u.set_query(None);
            u.set_fragment(None);
            let _ = u.set_username("");
            let _ = u.set_password(None);
            if had_query {
                format!("{u}?<redacted>")
            } else {
                u.to_string()
            }
        }
        // 非法 URL 无法结构化处理：截到 `?` 前兜底去 query。
        Err(_) => match url.find('?') {
            Some(q) => format!("{}?<redacted>", &url[..q]),
            None => url.to_string(),
        },
    }
}

/// 订阅拉取失败。`kind` 在**抛出点**即确定（不回头 re-parse 自己的字符串），
/// `http_status` 仅 [`SubscriptionErrorKind::Http`] 时有值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionFetchError {
    pub kind: SubscriptionErrorKind,
    pub message: String,
    pub http_status: Option<u16>,
}

impl SubscriptionFetchError {
    fn new(kind: SubscriptionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
        }
    }
}

impl std::fmt::Display for SubscriptionFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SubscriptionFetchError {}

/// 订阅内容格式探测结果。上游 `ImportFormat`（订阅侧子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionFormat {
    /// Clash / mihomo YAML（含 proxies 或 proxy-providers）。
    Clash,
    /// sing-box JSON outbound 数组（outbound 用扁平 `type`）。
    SingboxJson,
    /// Xray / v2ray JSON（outbound 用 `protocol`+`settings`+`streamSettings`）。
    XrayJson,
    /// base64 编码的分享链接列表。
    Base64,
    /// 纯文本分享链接列表（vless://... 等，每行一条）。
    UrlList,
    /// 无法识别。
    Unknown,
}

/// 拉取订阅正文（协议校验 → SSRF guard 逐跳复检 → HTTP 状态校验 → 体积闸 → 正文）。
///
/// **这是拉取流水线拿到正文的唯一入口**，产出直接喂 [`parse_subscription`]。
///
/// 安全与健壮性逐层（顺序即优先级）：
/// 1. **协议闸**：起始 URL 须 http(s) —— `file://`/`ftp://` 直接拒（错误文案带脱敏 URL）。
/// 2. **SSRF guard**：首跳 + **每一跳 Location** 都过 [`crate::ssrf::assert_host_allowed`]
///    （由 [`safe_redirect_fetch`] 内部执行）。首跳单独再 guard 一次是多余的——
///    旧实现那次重复调用已随本次重写移除。`exempt_fake_ip` **仅实际经代理时**传 true。
/// 3. **重定向**：`redirect: manual` 自管链，上限 5 跳（[`safe_redirect_fetch`] 默认）。
/// 4. **HTTP 状态**：非 2xx → [`SubscriptionErrorKind::Http`] 并带 status。
/// 5. **体积闸**：content-length 预检 + 正文字节复检，双闸 [`MAX_BODY_BYTES`]。
///    （实现侧还须在**流式读取**时截断，见 [`FetchInit::max_body_bytes`](crate::safe_redirect::FetchInit::max_body_bytes)——
///    到了这层 body 已在内存里，此闸只是纵深防御，防不住恶意实现。）
/// 6. **超时**：`timeout_ms` 透传实现侧（[`MAIN_FETCH_TIMEOUT_MS`] / [`PROVIDER_FETCH_TIMEOUT_MS`]）。
///
/// 上游 `SubscriptionService.fetchSubscriptionText` 的对位实现。
pub async fn fetch_subscription_full<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
    user_agent: &str,
    headers: Option<Vec<(String, String)>>,
    exempt_fake_ip: bool,
    timeout_ms: u64,
) -> Result<String, SubscriptionFetchError> {
    // 文本-only 入口（proxy-provider 子拉取等复用同一安全管线，不消费元数据/条件 GET）。
    // conditional=None → 不发条件头 → 304 走非 2xx Http 分支（fail-safe：无验证器不认 304）。
    fetch_core(
        client,
        lookup,
        url,
        user_agent,
        headers,
        None,
        exempt_fake_ip,
        timeout_ms,
    )
    .await
    .map(|f| f.text)
}

/// 拉取订阅正文 **+ 元数据**（`Subscription-UserInfo` 流量/到期 + `ETag`/`Last-Modified` 验证器
/// + 304 条件 GET 短路）。上游 `SubscriptionService.fetchSubscriptionText` 的完整对位。
///
/// 与 [`fetch_subscription_full`] 同一安全管线（协议闸 / SSRF 逐跳 / 状态 / 体积闸），仅额外：
/// - `conditional` 非空 → 发 `If-None-Match` / `If-Modified-Since`；304（**仅当确实发了条件头**）→
///   `not_modified=true`、`text` 空，调用方短路 parse/reconcile（零节点扰动、省流省渲染）。
/// - 200 → 解析 `Subscription-UserInfo`（流量/到期）+ 回传 `etag`/`last-modified` 供回写 sub。
///
/// # Errors
///
/// 协议不支持 / SSRF 拒绝 / 非 2xx（含 304 但**未**发条件头）/ 网络错误 / 体积超限。
pub async fn fetch_subscription_with_meta<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
    user_agent: &str,
    conditional: Option<&Conditional>,
    exempt_fake_ip: bool,
    timeout_ms: u64,
) -> Result<FetchedSubscription, SubscriptionFetchError> {
    fetch_core(
        client,
        lookup,
        url,
        user_agent,
        None,
        conditional,
        exempt_fake_ip,
        timeout_ms,
    )
    .await
}

/// 拉取管线核心（协议闸 → SSRF 逐跳 → 条件 GET/304 → 状态 → 体积闸 → 元数据）。
///
/// `extra_headers`（provider 子拉取的透传头）与 `conditional`（条件 GET）合并后交
/// [`safe_redirect_fetch`]。`sent_conditional` 仅在实际追加了条件头时为真——用于 304 fail-safe：
/// 未发条件头却收 304（某些 CDN 违规）绝不认作 not_modified（会得空 body→0 节点→误删存量）。
#[allow(clippy::too_many_arguments)]
async fn fetch_core<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
    user_agent: &str,
    extra_headers: Option<Vec<(String, String)>>,
    conditional: Option<&Conditional>,
    exempt_fake_ip: bool,
    timeout_ms: u64,
) -> Result<FetchedSubscription, SubscriptionFetchError> {
    // 1) 协议闸：仅 http(s)。非法 URL 同归 scheme（用户可见原因一致：地址不对）。
    let parsed = Url::parse(url).map_err(|_| {
        SubscriptionFetchError::new(
            SubscriptionErrorKind::Scheme,
            format!("订阅地址非法: {}", redact_url(url)),
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(SubscriptionFetchError::new(
            SubscriptionErrorKind::Scheme,
            format!(
                "订阅地址协议不支持（仅允许 http/https）: {}",
                redact_url(url)
            ),
        ));
    }

    // 条件 GET 头拼装（缓存验证器非凭据，逐跳携带无泄漏面）。与调用方透传头合并。
    let mut headers = extra_headers.unwrap_or_default();
    let sent_conditional = conditional.is_some_and(Conditional::has_any);
    if let Some(c) = conditional {
        if let Some(etag) = &c.etag {
            headers.push(("If-None-Match".to_string(), etag.clone()));
        }
        if let Some(lm) = &c.last_modified {
            headers.push(("If-Modified-Since".to_string(), lm.clone()));
        }
    }
    let headers = if headers.is_empty() {
        None
    } else {
        Some(headers)
    };

    // 2~3) SSRF guard（首跳 + 逐跳）+ 手动重定向链。
    let response = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url,
        user_agent: user_agent.to_string(),
        headers,
        exempt_fake_ip,
        max_redirects: None,
        timeout_ms: Some(timeout_ms),
        max_body_bytes: Some(MAX_BODY_BYTES),
        lookup,
    })
    .await
    .map_err(|e| match e.reason {
        // 安全拒绝：原文案冒泡（含 hostname / 解析结果，诊断需要）。
        SafeFetchRejectReason::Ssrf | SafeFetchRejectReason::TooManyRedirects => {
            SubscriptionFetchError::new(SubscriptionErrorKind::Ssrf, e.message)
        }
        SafeFetchRejectReason::RedirectProtocol => {
            SubscriptionFetchError::new(SubscriptionErrorKind::Scheme, e.message)
        }
        // 网络错误：message 不透明（实现侧的 io 错误串）→ 交 §C4 分类器判 dns/timeout/refused。
        SafeFetchRejectReason::Network => {
            let cls = classify_subscription_error(&SubscriptionErrorSignal {
                message: Some(e.message.clone()),
                ..Default::default()
            });
            SubscriptionFetchError {
                kind: cls.kind,
                message: e.message,
                http_status: None,
            }
        }
    })?;

    // 3.5) 条件 GET 命中（304）——**仅当本次确实发了条件头**才认（fail-safe，见函数 doc）。
    //      短路：不读 body、不 parse/reconcile（零节点扰动）；仍回传验证器供刷新。
    if response.status == 304 && sent_conditional {
        return Ok(FetchedSubscription {
            not_modified: true,
            etag: response.header("etag").map(str::to_string),
            last_modified: response.header("last-modified").map(str::to_string),
            ..Default::default()
        });
    }

    // 4) HTTP 状态校验。
    if !(200..300).contains(&response.status) {
        return Err(SubscriptionFetchError {
            kind: SubscriptionErrorKind::Http,
            message: format!("订阅服务器返回 HTTP {}", response.status),
            http_status: Some(response.status),
        });
    }

    // 5) 体积闸：content-length 预检（实现侧若已流式截断则到不了这里；此为纵深防御）。
    if let Some(cl) = response.header("content-length") {
        if let Ok(n) = cl.trim().parse::<usize>() {
            if n > MAX_BODY_BYTES {
                return Err(SubscriptionFetchError::new(
                    SubscriptionErrorKind::TooLarge,
                    format!("订阅响应体积 {n} 字节超过上限 {MAX_BODY_BYTES}，已拒绝"),
                ));
            }
        }
    }
    if response.body.len() > MAX_BODY_BYTES {
        return Err(SubscriptionFetchError::new(
            SubscriptionErrorKind::TooLarge,
            format!(
                "订阅响应体积 {} 字节超过上限 {MAX_BODY_BYTES}，已拒绝",
                response.body.len()
            ),
        ));
    }

    // 6) 元数据：Subscription-UserInfo（流量/到期）+ 验证器（下次条件 GET）。
    let user_info = parse_user_info(response.header("subscription-userinfo"));
    let etag = response.header("etag").map(str::to_string);
    let last_modified = response.header("last-modified").map(str::to_string);

    // 正文：lossy 解码（对齐 上游 `TextDecoder` 语义）。订阅正文是 base64/YAML/JSON（ASCII 面），
    // 为个别坏字节整单失败会把「能用的订阅」判死；坏字节最终由解析层按格式拒。
    Ok(FetchedSubscription {
        text: String::from_utf8_lossy(&response.body).into_owned(),
        user_info,
        etag,
        last_modified,
        not_modified: false,
    })
}

/// 探测订阅内容格式。Polaris 订阅内容格式判定（Clash YAML/JSON / sing-box JSON / xray JSON /
/// base64 / url-list）。**格式判定的单一真值**——`parse_subscription` 与 [`extract_proxy_providers`]
/// 都以它为准，不得各自另判一套（此前 `extract_proxy_providers` 单独判 `is_clash_probe` 就漏了 JSON 编码）。
pub fn detect_format(trimmed: &str) -> SubscriptionFormat {
    let t = trimmed.trim_start();
    // Clash：proxies: 或 proxy-providers: 行。
    if clash_parser::is_clash_probe(t) {
        return SubscriptionFormat::Clash;
    }
    // JSON：sing-box（outbound 用扁平 `type`）/ xray（outbound 用 `protocol`+`settings`，无 `type`）。
    // 二者共用 `outbounds` 键，靠 [`crate::xray_import::looks_like_xray`] 区分（对齐 上游 parseLocalContent
    // 的 `looksXray` 判定）；`endpoints`（wireguard/tailscale）唯 sing-box。
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(outbounds) = v.get("outbounds").and_then(serde_json::Value::as_array) {
                return if crate::xray_import::looks_like_xray(outbounds) {
                    SubscriptionFormat::XrayJson
                } else {
                    SubscriptionFormat::SingboxJson
                };
            }
            if v.get("endpoints").is_some() {
                return SubscriptionFormat::SingboxJson;
            }
            // **JSON 编码的 Clash**（`{"proxies":[…]}` / `{"proxy-providers":{…}}`）。少数机场按
            // `Content-Type: application/json` 下发同一份 Clash 配置。判定放在 outbounds/endpoints
            // **之后**，与 上游 `parseLocalContent` 的分支顺序一致（sing-box 优先）。
            // 此前无此分支 → 落 `Unknown` → 用户侧只看到「暂不支持的订阅格式」。
            if clash_parser::is_json_clash(&v) {
                return SubscriptionFormat::Clash;
            }
        }
        // JSON 解析失败但形似 → 保守判 sing-box（其解析分支返回 warning，不误吞进 url-list/base64）。
        if t.contains("\"outbounds\"") || t.contains("\"endpoints\"") {
            return SubscriptionFormat::SingboxJson;
        }
    }
    // url-list：以协议 scheme 开头（vless:// vmess:// ss:// trojan:// ...）。
    let first_line = t.lines().next().unwrap_or("");
    if first_line.contains("://") {
        return SubscriptionFormat::UrlList;
    }
    // base64：尝试解码；含分享链 scheme 即 url-list。
    if let Ok(decoded) = base64_decode(trimmed) {
        if decoded.contains("://") {
            return SubscriptionFormat::Base64;
        }
    }
    SubscriptionFormat::Unknown
}

/// 解析订阅正文（按探测格式分发）。已建：Clash（YAML **与 JSON** 两种编码）/ base64 / url-list /
/// Xray JSON / sing-box JSON。
///
/// - **Clash**：走 [`crate::clash_parser`]（既有实现，不重写）。JSON 编码（`{"proxies":[…]}`）由
///   [`crate::clash_parser::try_load_clash_doc`] 转成 `serde_yaml::Value` 后复用同一条解析路径。
/// - **Base64**：解码后按 url-list 处理（多数机场订阅的实际形态）。
/// - **UrlList**：逐行 [`crate::share_link::parse_share_url`]。
/// - **XrayJson**：outbounds[] 走 [`crate::xray_import::parse_xray_outbounds`]（vmess/vless/trojan/ss）。
/// - **SingboxJson**：`outbounds[]` 走 [`crate::singbox_import::parse_singbox_outbounds`]，
///   **`endpoints[]` 走 [`crate::singbox_import::parse_singbox_endpoints`]**，两者结果合并
///   （两个数组的 type 域不相交，见后者文档）。endpoints-only 的配置（机场下发 WireGuard 组网）
///   此前恒 0 节点，现按 endpoint 建模映射入库。
///
/// `id_gen` 注入 UUID 生成（对齐 Polaris randomUUID）。
/// `origin` 决定未建模 type 是否透传 custom，见 [`ImportOrigin`]。
pub fn parse_subscription(
    trimmed: &str,
    subscription_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
    origin: ImportOrigin,
) -> ClashParseResult {
    let format = detect_format(trimmed);
    match format {
        SubscriptionFormat::Clash => {
            let doc = match clash_parser::try_load_clash_doc(trimmed) {
                Ok(d) => d,
                Err(e) => {
                    return ClashParseResult {
                        warnings: vec![e],
                        ..Default::default()
                    };
                }
            };
            let proxies = doc
                .get(serde_yaml::Value::String("proxies".to_string()))
                .cloned()
                .unwrap_or(serde_yaml::Value::Null);
            clash_parser::parse_clash_proxies(&proxies, subscription_id, now, id_gen)
        }
        SubscriptionFormat::UrlList => {
            crate::share_link::parse_url_list(trimmed, subscription_id, now, id_gen)
        }
        SubscriptionFormat::Base64 => match base64_decode(trimmed) {
            Ok(decoded) => {
                crate::share_link::parse_url_list(&decoded, subscription_id, now, id_gen)
            }
            // detect_format 已试解成功才判 Base64，此分支理论不可达；仍不 panic（订阅是外部输入）。
            Err(()) => ClashParseResult {
                warnings: vec!["订阅 base64 解码失败".to_string()],
                ..Default::default()
            },
        },
        SubscriptionFormat::XrayJson => match serde_json::from_str::<serde_json::Value>(trimmed) {
            // detect_format 已确认是含 outbounds 数组的合法 JSON；此处取 outbounds 交 xray 解析器。
            Ok(v) => crate::xray_import::parse_xray_outbounds(
                v.get("outbounds").unwrap_or(&serde_json::Value::Null),
                subscription_id,
                now,
                id_gen,
            ),
            Err(e) => ClashParseResult {
                warnings: vec![format!("Xray JSON 解析失败: {e}")],
                ..Default::default()
            },
        },
        SubscriptionFormat::SingboxJson => match serde_json::from_str::<serde_json::Value>(trimmed)
        {
            // detect_format 已确认形似 sing-box JSON；`outbounds[]` 与 `endpoints[]` 各交对应解析器
            // 后合并 —— 两个数组由内核定义为不相交的 type 域（`wireguard` 作 outbound 已于 1.13 移除、
            // `tailscale` 作 outbound 即 unknown），故不存在同一节点被两条腿各数一次。
            Ok(v) => {
                let null = serde_json::Value::Null;
                let mut r = crate::singbox_import::parse_singbox_outbounds(
                    v.get("outbounds").unwrap_or(&null),
                    subscription_id,
                    now,
                    id_gen,
                    origin,
                );
                let ep = crate::singbox_import::parse_singbox_endpoints(
                    v.get("endpoints").unwrap_or(&null),
                    subscription_id,
                    now,
                    id_gen,
                    origin,
                );
                r.servers.extend(ep.servers);
                r.skipped += ep.skipped;
                r.failed += ep.failed;
                r.warnings.extend(ep.warnings);
                r
            }
            Err(e) => ClashParseResult {
                warnings: vec![format!("sing-box JSON 解析失败: {e}")],
                ..Default::default()
            },
        },
        SubscriptionFormat::Unknown => ClashParseResult {
            warnings: vec![format!("暂不支持的订阅格式: {format:?}")],
            ..Default::default()
        },
    }
}

/// provider 子拉取失败 —— **带永久性分类**。
///
/// # 为什么 `Result<_, String>` 不够（这是修掉的真实缺陷）
///
/// `permanent` 决定该 provider 进不进 `failed_providers`，而 `failed_providers` 非空
/// 会让 reconcile 对**无 `providerName` 的节点**（主正文内联 `proxies` / 迁移前存量）一律保留
/// （见命令层 `leftover_survives_partial` 规则 2）。于是「provider URL **永久**坏掉」
/// （404 / 域名注销 / SSRF 拒绝）会把整条订阅钉死在 partial：
///  - 主正文里**真下架**的内联节点永不删除；
///  - 每轮更新都判「内容变了」→ 每轮 save + 广播 `config:changed` → 每轮整核评估 + 前端全量重渲染。
///
/// 分类判据（由运行时层填，那里才有 HTTP 状态/错误种类）：
///  - `permanent = true`：重试不会变好 —— 4xx（404/403/410…）、SSRF guard 拒绝、URL 非法/协议不支持。
///    仅 warn，**不**置 `any_failed` → 该 provider 名下节点按真下架正常删除（它确实拿不回来了）。
///  - `permanent = false`：瞬时 —— 超时、连不上、5xx、正文解析失败（WAF 错误页可能下轮就好）。
///    置 `any_failed` + 进 `failed_providers` → 该 provider 名下存量**保留**，防穿仓。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFetchError {
    pub message: String,
    /// `true` = 重试不转好（不触发 merge-only 保护）；`false` = 瞬时（触发 merge-only 保护）。
    pub permanent: bool,
}

impl ProviderFetchError {
    /// 瞬时失败（默认方向：**宁滞留不误删**）。
    #[must_use]
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: false,
        }
    }

    /// 永久失败（重试不转好 → 不保护存量）。
    #[must_use]
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: true,
        }
    }
}

impl std::fmt::Display for ProviderFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 注入的正文拉取闭包类型（返回 boxed future，便于运行时层包装 safe-redirect-fetch + read body）。
pub type FetchTextFn = Box<
    dyn Fn(
            &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, ProviderFetchError>> + Send>,
        > + Send
        + Sync,
>;

/// 拉取并解析单个 proxy-provider（http type）。
///
/// Polaris resolveProxyProviders 单 provider 切片：fetch（SSRF guard）→ parse（allowProviders:false）
/// → filter/exclude-filter → override。失败返回 Err（供调用方判 partial / merge-only）。
///
/// 参数与 Polaris ProviderDeps + provider 配置项 1:1 对齐（刻意 8 参数，不强制收敛）。
/// `fetch_text` 注入正文拉取（含安全校验，由运行时层实现 safe-redirect-fetch + read body）。
#[allow(clippy::too_many_arguments)]
pub async fn fetch_and_parse_provider(
    url: &str,
    filter: Option<&str>,
    exclude_filter: Option<&str>,
    override_val: Option<&serde_yaml::Value>,
    subscription_id: &str,
    now: &str,
    fetch_text: &(impl Fn(
        &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, ProviderFetchError>> + Send>,
    > + Send
          + Sync),
    id_gen: &mut impl FnMut() -> String,
) -> Result<ClashParseResult, ProviderFetchError> {
    let text = fetch_text(url).await?;
    let trimmed = text.trim();
    let mut parsed = clash_parser::parse_clash_proxies(
        &clash_parser::try_load_clash_doc(trimmed)
            .map_err(ProviderFetchError::transient)?
            .get(serde_yaml::Value::String("proxies".to_string()))
            .cloned()
            .unwrap_or(serde_yaml::Value::Null),
        subscription_id,
        now,
        id_gen,
    );

    if filter.is_some() || exclude_filter.is_some() {
        let mut warns = Vec::new();
        let filtered = clash_parser::apply_provider_filters(
            std::mem::take(&mut parsed.servers),
            filter,
            exclude_filter,
            &mut |m| warns.push(m),
            url,
        );
        parsed.servers = filtered;
        parsed.warnings.extend(warns);
    }

    if let Some(ov) = override_val {
        clash_parser::apply_override(&mut parsed.servers, ov);
    }

    Ok(parsed)
}

/// 节点稳定指纹（对账/去重键）：`protocol|address|port|cred|network`（**排除 name/detour**）。
/// 上游 `SubscriptionService.serverFingerprint`。
///
/// 排除显示名：订阅方常改名/调顺序，用 name 做键会把同一物理节点误判「删旧增新」→ id 抖动、
/// selectedServerId 丢失、本地编辑被清。cred（uuid / password / 嵌套 ss·ssh password / username /
/// wg peerPublicKey）区分同 host:port 并列节点；network 维度区分同 host:port:cred 但传输不同
/// （tcp/ws/grpc）的节点，缺此维度会被误并静默吞节点。
///
/// **与命令层 `node_fingerprint(&Value)` 是同一公式的两侧**（typed / json），由跨类型等价单测锁定同步。
#[must_use]
pub fn server_fingerprint(s: &ServerConfig) -> String {
    let protocol = serde_json::to_value(s.protocol)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let cred = non_empty(s.uuid.clone())
        .or_else(|| non_empty(s.password.clone()))
        .or_else(|| {
            non_empty(
                s.shadowsocks_settings
                    .as_ref()
                    .map(|ss| ss.password.clone()),
            )
        })
        .or_else(|| non_empty(s.username.clone()))
        .or_else(|| non_empty(s.ssh_settings.as_ref().and_then(|ssh| ssh.password.clone())))
        .or_else(|| {
            non_empty(
                s.wireguard_settings
                    .as_ref()
                    .and_then(|w| w.peer_public_key.clone()),
            )
        })
        .unwrap_or_default();
    let network = s.network.as_deref().unwrap_or("tcp").to_ascii_lowercase();
    format!("{protocol}|{}|{}|{cred}|{network}", s.address, s.port)
}

/// `Option<String>` 里的空串归 `None`（对齐 上游 `x || ...` 的 falsy 空串语义）。
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// 同指纹去重（首见保留）。上游 `dedupeByFingerprint`：内联在前、provider 按声明序在后 →
/// 同节点多源留内联那份。
#[must_use]
pub fn dedupe_by_fingerprint(servers: Vec<ServerConfig>) -> Vec<ServerConfig> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(servers.len());
    for s in servers {
        if seen.insert(server_fingerprint(&s)) {
            out.push(s);
        }
    }
    out
}

/// 从 Clash 订阅正文提取 `proxy-providers` 映射（非 Clash / 无 providers → `None`）。
///
/// 供命令层判定「是否需 provider 编排」并取出 provider 配置（供 [`resolve_proxy_providers`]）。
/// 判定走 [`detect_format`]（**单一真值**）→ YAML 与 JSON 两种编码同覆盖，与 [`parse_subscription`]
/// 的 Clash 分支严格同口径。此前这里单独判 `is_clash_probe`（纯 YAML 行首探测），JSON 编码的
/// `{"proxy-providers":{…}}` 会被漏掉 → provider 一个都不拉、节点全丢。
#[must_use]
pub fn extract_proxy_providers(text: &str) -> Option<serde_yaml::Value> {
    let trimmed = text.trim();
    if detect_format(trimmed) != SubscriptionFormat::Clash {
        return None;
    }
    let doc = clash_parser::try_load_clash_doc(trimmed).ok()?;
    let providers = doc.get(serde_yaml::Value::String("proxy-providers".to_string()))?;
    providers.as_mapping().is_some().then(|| providers.clone())
}

/// proxy-providers 编排产出。上游 `ResolveProvidersResult`。
#[derive(Debug, Default)]
pub struct ProviderResolveResult {
    /// 各 provider 解析出的节点（已标 `provider_name`，供调用方按 provider 精确 merge-only）。
    pub servers: Vec<ServerConfig>,
    pub warnings: Vec<String>,
    /// 任一 provider **transient** 失败（拉取/解析异常）→ 调用方 reconcile 改 merge-only 防穿仓。
    pub any_failed: bool,
    /// transient 失败的 provider 名（供 provider 级精确 merge-only）。
    pub failed_providers: Vec<String>,
}

/// 多源 proxy-providers 编排（上游 `resolveProxyProviders` 1:1，运行时层）。
///
/// 逐 provider（**声明序，顺序执行**——`&mut id_gen` 不可跨并发共享；provider≤8 且各带超时，
/// 顺序对正确性无损，仅牺牲并发 UX，见模块注记）：验证 `type:http` + `url` → [`fetch_and_parse_provider`]
/// （复用同一 SSRF-guarded 拉取 + Clash 解析 + filter/override）→ 按 [`ProviderFetchError::permanent`]
/// 分类。成功节点标 `provider_name` 供精确 merge-only。
///
/// # 「进不进 `failed_providers`」的唯一判据：**这一轮拿不到它的节点，是不是意味着它真下架了**
///
/// 进名单 = 该 provider 名下的存量节点本轮**不删**（宁滞留不误删）。三类必须进：
///
/// | 形态 | 为什么不能当「真下架」 |
/// |---|---|
/// | transient 拉取/解析失败 | 超时/5xx/WAF 错误页，下轮可能就好 |
/// | **被 `max_providers` 截断**（第 9+ 个） | 我们**压根没拉**它 —— 拿不到 ≠ 下架。此前不进名单 → 它名下节点**每轮都被真删**（且下轮又被截断，永远删不完/删了白删） |
/// | **0 节点** | 机场返 200 空正文 / `filter` 因上游改名临时滤尽 —— 与主正文「0 节点 → merge-only」（命令层 `perform_subscription_update` 第 4 步）**同口径**，不能一边保守一边激进 |
///
/// 不进名单的只有 permanent：配置面非法（`type` 不支持 / 缺 `url` / 配置非对象）与
/// permanent 拉取失败（4xx / SSRF 拒绝）—— 这些重试不转好，硬保留只会让下架节点无限滞留。
///
/// **残留（如实登记）**：一个**永久**变空的 provider（机场真的清空了它）会让存量节点一直留着。
/// 无 per-provider 持久状态就实现不了「宽限 N 轮」，而两害相权：误删是**不可逆**的（用户丢节点 id +
/// 选中项 + 本地编辑），滞留是**用户可见且可手动删**的。方向与主正文一致。
///
/// `fetch_text` 注入正文拉取（含安全校验，由运行时层实现 safe-redirect-fetch + read body）。
pub async fn resolve_proxy_providers<F>(
    providers: &serde_yaml::Value,
    subscription_id: &str,
    now: &str,
    max_providers: usize,
    fetch_text: &F,
    id_gen: &mut impl FnMut() -> String,
) -> ProviderResolveResult
where
    F: Fn(
            &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, ProviderFetchError>> + Send>,
        > + Send
        + Sync,
{
    let mut out = ProviderResolveResult::default();
    let Some(map) = providers.as_mapping() else {
        return out;
    };

    let total = map.len();
    if total > max_providers {
        // 被截断的 provider **一个都没拉过** → 它们名下的存量节点本轮必须保住（见函数文档表格）。
        let truncated: Vec<String> = map
            .iter()
            .skip(max_providers)
            .map(|(name_v, _)| provider_name_of(name_v))
            .collect();
        out.warnings.push(format!(
            "proxy-providers 数量 {total} 超上限 {max_providers}，已截断（未拉取: {}；\
             其名下存量节点本轮保留，不作下架处理）",
            truncated.join(", ")
        ));
        out.any_failed = true;
        out.failed_providers.extend(truncated);
    }

    let mut succeeded = 0usize;
    let mut attempted = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (name_v, prov) in map.iter().take(max_providers) {
        attempted += 1;
        let name = provider_name_of(name_v);

        // permanent（配置面非法）：仅 warn，不置 any_failed（重试不转好，否则 reconcile 永久
        // merge-only、下架节点无限滞留）。分类总表见函数文档。
        if prov.as_mapping().is_none() {
            failures.push(format!("{name}(配置非对象)"));
            continue;
        }
        let ty = prov
            .get("type")
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_ascii_lowercase);
        match ty.as_deref() {
            Some("file") => {
                failures.push(format!("{name}(type:file 不支持，安全面忽略)"));
                continue;
            }
            Some("http") => {}
            other => {
                failures.push(format!(
                    "{name}(不支持的 type: {})",
                    other.unwrap_or("(缺省)")
                ));
                continue;
            }
        }
        let Some(url) = prov
            .get("url")
            .and_then(serde_yaml::Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            failures.push(format!("{name}(缺 url)"));
            continue;
        };
        let filter = prov.get("filter").and_then(serde_yaml::Value::as_str);
        let exclude = prov
            .get("exclude-filter")
            .and_then(serde_yaml::Value::as_str);
        let override_val = prov.get("override");

        match fetch_and_parse_provider(
            url,
            filter,
            exclude,
            override_val,
            subscription_id,
            now,
            fetch_text,
            id_gen,
        )
        .await
        {
            Ok(mut parsed) => {
                if parsed.servers.is_empty() {
                    // HTTP 成功但解析/过滤后 0 节点 —— **不判 permanent**（此前如此，是本条 review 的缺陷）。
                    // 机场返 200 空正文、或 `filter` 因上游改名临时滤尽，都会走到这里；判 permanent
                    // 意味着该 provider 名下**全部存量节点当场删光**，而主正文遇到同样的「0 节点」
                    // 是走 merge-only 不删的（命令层 `perform_subscription_update` 第 4 步）——
                    // 同一现象两套方向，保守的那套才对（误删不可逆，滞留可手删）。
                    out.any_failed = true;
                    out.failed_providers.push(name.clone());
                    failures.push(format!("{name}(0 节点，存量保留不作下架)"));
                    continue;
                }
                succeeded += 1;
                for s in &mut parsed.servers {
                    s.provider_name = Some(name.clone());
                }
                for w in parsed.warnings {
                    out.warnings.push(format!("[{name}] {w}"));
                }
                out.servers.append(&mut parsed.servers);
            }
            // permanent（4xx / SSRF 拒绝 / URL 非法）→ 仅 warn，**不**保护存量：它确实拿不回来了，
            // 硬保留会把整条订阅永久钉在 partial（连主正文内联的真下架节点都删不掉，且每轮 save+广播）。
            Err(e) if e.permanent => {
                failures.push(format!("{name}({} · 永久失败)", e.message));
            }
            Err(e) => {
                out.any_failed = true;
                out.failed_providers.push(name.clone());
                failures.push(format!("{name}({})", e.message));
            }
        }
    }

    if !failures.is_empty() {
        out.warnings.push(format!(
            "proxy-providers {succeeded}/{attempted} 成功，失败: {}",
            failures.join(", ")
        ));
    }
    // 相邻去重：截断腿（`skip`）与失败腿（`take`）不相交，唯一的重复来源是**非字符串键**
    // 全被 [`provider_name_of`] 归一成 `(unnamed)` —— 那些恰好是相邻 push 的，`dedup` 够用。
    // （`leftover_survives_partial` 只做 `any` 匹配，重复不影响判定，只是让告警文案出现两遍同名。）
    out.failed_providers.dedup();
    out
}

/// provider 名（非字符串键 → `(unnamed)`）。截断腿与失败腿共用同一取名口径，不容许两处漂移。
fn provider_name_of(name_v: &serde_yaml::Value) -> String {
    name_v
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| "(unnamed)".to_string())
}

/// 轻量 base64 解码（容忍换行/空白，URL-safe 与标准均支持）。失败返回 Err。
///
/// `pub(crate)`：[`crate::share_link`] 的 vmess base64-JSON / ss base64-userinfo /
/// shadow-tls base64-JSON 三处复用同一份解码器（Node `Buffer.from(x,'base64')` 同样兼容
/// 标准与 URL-safe 两套字母表）——不另造第二份。
pub(crate) fn base64_decode(input: &str) -> Result<String, ()> {
    let clean: String = input
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            _ => c,
        })
        .collect();
    // 补齐 padding。
    let mut s = clean;
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    base64_decode_inner(&s)
}

/// 最小 base64 解码（避免引入新依赖；订阅 base64 体量小，纯实现可接受）。
fn base64_decode_inner(s: &str) -> Result<String, ()> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s
        .as_bytes()
        .iter()
        .filter(|&&b| b != b'=')
        .copied()
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in &bytes {
        let v = u32::from(val(b).ok_or(())?);
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use crate::safe_redirect::{FetchInit, MinimalResponse};
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Mutex;

    /// mock DnsLookup：默认解析到公网 IP；可注入特定 hostname → 内网 IP（触发 SSRF guard）。
    struct MockLookup {
        private: HashMap<String, Vec<String>>,
    }
    impl MockLookup {
        fn public() -> Self {
            Self {
                private: HashMap::new(),
            }
        }
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
                .unwrap_or_else(|| vec!["93.184.216.34".to_string()]);
            async move { Ok(res) }
        }
    }

    /// mock HttpClient：按 url 返回预设响应；记录每次请求的 FetchInit（供断言 UA / 超时 / 体积闸透传）。
    struct MockFetch {
        responses: Mutex<HashMap<String, MinimalResponse>>,
        /// 网络错误注入：url → 错误串。
        errors: Mutex<HashMap<String, String>>,
        seen: Mutex<Vec<(String, FetchInit)>>,
    }
    impl MockFetch {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                errors: Mutex::new(HashMap::new()),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn set(&self, url: &str, resp: MinimalResponse) -> &Self {
            self.responses.lock().unwrap().insert(url.to_string(), resp);
            self
        }
        fn set_err(&self, url: &str, msg: &str) -> &Self {
            self.errors
                .lock()
                .unwrap()
                .insert(url.to_string(), msg.to_string());
            self
        }
        fn last_init(&self) -> FetchInit {
            self.seen.lock().unwrap().last().unwrap().1.clone()
        }
    }
    impl HttpClient for MockFetch {
        fn fetch(
            &self,
            url: &str,
            init: &FetchInit,
        ) -> impl Future<Output = Result<MinimalResponse, String>> + Send {
            self.seen
                .lock()
                .unwrap()
                .push((url.to_string(), init.clone()));
            let err = self.errors.lock().unwrap().get(url).cloned();
            let resp = self.responses.lock().unwrap().remove(url);
            async move {
                if let Some(e) = err {
                    return Err(e);
                }
                Ok(resp.unwrap_or(MinimalResponse {
                    status: 404,
                    ..Default::default()
                }))
            }
        }
    }

    fn ok_body(body: &str) -> MinimalResponse {
        MinimalResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    async fn fetch(
        client: &MockFetch,
        lookup: &MockLookup,
        url: &str,
    ) -> Result<String, SubscriptionFetchError> {
        fetch_subscription_full(
            client,
            lookup,
            url,
            "Polaris/0.1.0",
            None,
            false,
            MAIN_FETCH_TIMEOUT_MS,
        )
        .await
    }

    /// 核心回归：**正文真的被返回**（旧实现恒返回空串 / `Ok(())` —— 拉取流水线拿不到正文）。
    #[tokio::test]
    async fn returns_body_text() {
        let c = MockFetch::new();
        c.set("https://sub.example.com/x", ok_body("hello-subscription"));
        let r = fetch(&c, &MockLookup::public(), "https://sub.example.com/x").await;
        assert_eq!(r.unwrap(), "hello-subscription");
    }

    /// **SSRF 变异验证的靶子**：订阅 URL 解析到内网 → 必须拒。
    /// 打断 fetch_subscription_full 里的 safe_redirect_fetch guard → 本测试转红。
    #[tokio::test]
    async fn ssrf_guard_rejects_private_host() {
        let c = MockFetch::new();
        c.set("https://intranet.example.com/x", ok_body("leaked"));
        let lk = MockLookup {
            private: HashMap::from([(
                "intranet.example.com".to_string(),
                vec!["192.168.1.10".to_string()],
            )]),
        };
        let e = fetch(&c, &lk, "https://intranet.example.com/x")
            .await
            .expect_err("解析到内网的订阅地址必须被拒");
        assert_eq!(e.kind, SubscriptionErrorKind::Ssrf);
        // 必须真的没发出请求（不是拿到正文后才拒）。
        assert!(
            c.seen.lock().unwrap().is_empty(),
            "SSRF guard 须在发请求前拦截"
        );
    }

    /// SSRF：字面内网 IP 直接拒（不依赖 DNS）。
    #[tokio::test]
    async fn ssrf_guard_rejects_literal_private_ip() {
        let c = MockFetch::new();
        c.set("http://127.0.0.1:8080/x", ok_body("leaked"));
        let e = fetch(&c, &MockLookup::public(), "http://127.0.0.1:8080/x")
            .await
            .expect_err("字面回环地址必须被拒");
        assert_eq!(e.kind, SubscriptionErrorKind::Ssrf);
    }

    /// SSRF：30x 跳内网必须逐跳复检拦下（首跳公网、次跳内网）。
    #[tokio::test]
    async fn ssrf_guard_rejects_redirect_to_private() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 302,
                location: Some("http://169.254.169.254/latest/meta-data".to_string()),
                ..Default::default()
            },
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("重定向到云元数据地址必须被拒");
        assert_eq!(e.kind, SubscriptionErrorKind::Ssrf);
    }

    /// 协议闸：非 http(s) 拒，且错误文案不含 query（token 脱敏）。
    #[tokio::test]
    async fn rejects_non_http_scheme_and_redacts_token() {
        let c = MockFetch::new();
        let e = fetch(
            &c,
            &MockLookup::public(),
            "file:///etc/passwd?token=secret123",
        )
        .await
        .expect_err("file:// 必须被拒");
        assert_eq!(e.kind, SubscriptionErrorKind::Scheme);
        assert!(
            !e.message.contains("secret123"),
            "错误文案泄漏了 token: {}",
            e.message
        );
    }

    /// HTTP 状态：非 2xx → Http + status（供 i18n `{{status}}` 插值）。
    #[tokio::test]
    async fn non_2xx_classified_as_http_with_status() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 403,
                ..Default::default()
            },
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("403 必须失败");
        assert_eq!(e.kind, SubscriptionErrorKind::Http);
        assert_eq!(e.http_status, Some(403));
    }

    /// 体积闸：content-length 预检（早拒，不看 body）。
    #[tokio::test]
    async fn content_length_precheck_rejects_oversize() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 200,
                headers: vec![(
                    "Content-Length".to_string(),
                    (MAX_BODY_BYTES + 1).to_string(),
                )],
                body: b"small".to_vec(),
                ..Default::default()
            },
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("content-length 超限必须拒");
        assert_eq!(e.kind, SubscriptionErrorKind::TooLarge);
    }

    /// 体积闸：content-length 撒谎/缺失时，正文字节复检兜底。
    #[tokio::test]
    async fn body_size_recheck_rejects_oversize_when_content_length_lies() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 200,
                headers: vec![("content-length".to_string(), "5".to_string())],
                body: vec![b'a'; MAX_BODY_BYTES + 1],
                ..Default::default()
            },
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("正文超限必须拒（content-length 不可信）");
        assert_eq!(e.kind, SubscriptionErrorKind::TooLarge);
    }

    /// UA / 超时 / 体积闸须透传到实现侧（否则实现侧无从流式截断、无从设超时）。
    #[tokio::test]
    async fn passes_ua_timeout_and_cap_to_client() {
        let c = MockFetch::new();
        c.set("https://sub.example.com/x", ok_body("ok"));
        fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .unwrap();
        let init = c.last_init();
        assert_eq!(init.user_agent, "Polaris/0.1.0");
        assert_eq!(init.timeout_ms, Some(MAIN_FETCH_TIMEOUT_MS));
        assert_eq!(init.max_body_bytes, Some(MAX_BODY_BYTES));
    }

    /// 网络错误**不得**被误报成 SSRF（safe_redirect 曾把 client 错误一律标 reason=Ssrf）。
    #[tokio::test]
    async fn network_error_is_not_misclassified_as_ssrf() {
        let c = MockFetch::new();
        c.set_err(
            "https://sub.example.com/x",
            "tcp connect error: Connection refused (os error 111)",
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("连接被拒必须失败");
        assert_eq!(e.kind, SubscriptionErrorKind::Refused);
    }

    #[tokio::test]
    async fn network_timeout_classified() {
        let c = MockFetch::new();
        c.set_err(
            "https://sub.example.com/x",
            "request timed out after 30000ms",
        );
        let e = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .expect_err("超时必须失败");
        assert_eq!(e.kind, SubscriptionErrorKind::Timeout);
    }

    /// 拉取 → 解析 全链（mock client）：base64 订阅正文 → 真节点。
    #[tokio::test]
    async fn fetch_then_parse_yields_nodes() {
        let links = "ss://YWVzLTI1Ni1nY206cGFzc3dvcmQ@1.2.3.4:8388#node-a\n";
        let b64 = super::tests::b64(links);
        let c = MockFetch::new();
        c.set("https://sub.example.com/x", ok_body(&b64));
        let text = fetch(&c, &MockLookup::public(), "https://sub.example.com/x")
            .await
            .unwrap();
        assert_eq!(detect_format(text.trim()), SubscriptionFormat::Base64);
        let mut n = 0;
        let mut id_gen = || {
            n += 1;
            format!("id-{n}")
        };
        let parsed = parse_subscription(
            text.trim(),
            "sub-1",
            "2026-07-16T00:00:00Z",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(parsed.servers.len(), 1);
        assert_eq!(parsed.servers[0].address, "1.2.3.4");
        assert_eq!(parsed.servers[0].port, 8388);
    }

    async fn fetch_meta(
        client: &MockFetch,
        lookup: &MockLookup,
        url: &str,
        conditional: Option<&Conditional>,
    ) -> Result<FetchedSubscription, SubscriptionFetchError> {
        fetch_subscription_with_meta(
            client,
            lookup,
            url,
            "Polaris/0.1.0",
            conditional,
            false,
            MAIN_FETCH_TIMEOUT_MS,
        )
        .await
    }

    /// userInfo 解析 + 验证器回传（打断 parse_user_info / header 读取任一 → 断言转红）。
    #[tokio::test]
    async fn meta_parses_userinfo_and_validators() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 200,
                headers: vec![
                    (
                        "Subscription-UserInfo".to_string(),
                        "upload=100; download=200; total=1000; expire=1700000000".to_string(),
                    ),
                    ("ETag".to_string(), "\"abc123\"".to_string()),
                    (
                        "Last-Modified".to_string(),
                        "Wed, 21 Oct 2025 07:28:00 GMT".to_string(),
                    ),
                ],
                body: b"vless://11111111-1111-1111-1111-111111111111@a.com:443?type=tcp#n".to_vec(),
                ..Default::default()
            },
        );
        let f = fetch_meta(&c, &MockLookup::public(), "https://sub.example.com/x", None)
            .await
            .expect("200 应成功");
        let ui = f.user_info.expect("应解出 userInfo");
        assert_eq!(ui.upload, Some(100));
        assert_eq!(ui.download, Some(200));
        assert_eq!(ui.total, Some(1000));
        assert_eq!(ui.expire, Some(1_700_000_000));
        assert_eq!(f.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(
            f.last_modified.as_deref(),
            Some("Wed, 21 Oct 2025 07:28:00 GMT")
        );
        assert!(!f.not_modified);
    }

    /// 304 + 确实发了条件头 → not_modified 短路（不读 body），回传验证器。
    #[tokio::test]
    async fn meta_304_with_conditional_shortcircuits() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 304,
                headers: vec![("ETag".to_string(), "\"same\"".to_string())],
                ..Default::default()
            },
        );
        let cond = Conditional {
            etag: Some("\"same\"".to_string()),
            last_modified: None,
        };
        let f = fetch_meta(
            &c,
            &MockLookup::public(),
            "https://sub.example.com/x",
            Some(&cond),
        )
        .await
        .expect("304 条件命中不是错误");
        assert!(f.not_modified, "发了条件头且 304 → not_modified");
        assert!(f.text.is_empty(), "304 不读 body");
        assert_eq!(f.etag.as_deref(), Some("\"same\""));
        // 条件头必须真的发出（If-None-Match）。
        let init = c.last_init();
        assert!(
            init.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("if-none-match") && v == "\"same\""),
            "须发 If-None-Match，实际头: {:?}",
            init.headers
        );
    }

    /// 304 但**未**发条件头（fail-safe）→ 归 Http 错误（绝不当 not_modified，防空 body 误删存量）。
    #[tokio::test]
    async fn meta_304_without_conditional_is_http_error() {
        let c = MockFetch::new();
        c.set(
            "https://sub.example.com/x",
            MinimalResponse {
                status: 304,
                ..Default::default()
            },
        );
        let e = fetch_meta(&c, &MockLookup::public(), "https://sub.example.com/x", None)
            .await
            .expect_err("未发条件头的 304 必须当失败");
        assert_eq!(e.kind, SubscriptionErrorKind::Http);
        assert_eq!(e.http_status, Some(304));
    }

    #[test]
    fn parse_user_info_variants() {
        // 全字段。
        let ui = parse_user_info(Some("upload=1; download=2; total=3; expire=4")).unwrap();
        assert_eq!(
            (ui.upload, ui.download, ui.total, ui.expire),
            (Some(1), Some(2), Some(3), Some(4))
        );
        // 部分字段 + 非数字段跳过。
        let ui = parse_user_info(Some("total=500; expire=bad; junk=x")).unwrap();
        assert_eq!(ui.total, Some(500));
        assert_eq!(ui.expire, None);
        // parseInt 前缀语义（带单位）。
        let ui = parse_user_info(Some("total=107374182400 bytes")).unwrap();
        assert_eq!(ui.total, Some(107_374_182_400));
        // 全空 / None → None。
        assert!(parse_user_info(Some("garbage")).is_none());
        assert!(parse_user_info(None).is_none());
        // 大流量超 u32（4TB）不溢出。
        let ui = parse_user_info(Some("total=4398046511104")).unwrap();
        assert_eq!(ui.total, Some(4_398_046_511_104));
    }

    #[test]
    fn redact_url_strips_query_and_userinfo() {
        assert_eq!(
            redact_url("https://sub.example.com/link?token=secret123"),
            "https://sub.example.com/link?<redacted>"
        );
        assert_eq!(
            redact_url("https://sub.example.com/link"),
            "https://sub.example.com/link"
        );
        // userinfo 也是凭据。
        assert!(!redact_url("https://user:pass@sub.example.com/l?t=1").contains("pass"));
        // 非法 URL 兜底截断。
        assert_eq!(redact_url("not a url?token=x"), "not a url?<redacted>");
    }

    #[test]
    fn default_ua_is_neutral() {
        assert_eq!(default_subscription_user_agent("1.2.3"), "Polaris/1.2.3");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_config_engine::user_config::server_config::Protocol;

    /// 供 fetch_tests 复用的 base64 编码。
    pub(super) fn b64(s: &str) -> String {
        base64_encode(s)
    }

    #[test]
    fn detect_clash_format() {
        let yaml = "proxies:\n  - name: x\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n";
        assert_eq!(detect_format(yaml), SubscriptionFormat::Clash);
        assert_eq!(
            detect_format("proxy-providers:\n  p:\n    url: x"),
            SubscriptionFormat::Clash
        );
    }

    // ── JSON 编码的 Clash 订阅（此前整类不可用：落 Unknown → 「暂不支持的订阅格式」）──────
    //
    // 变异验证：把 `detect_format` 的 `is_json_clash` 分支删掉 → 三条 detect 断言 + 解析断言全红；
    // 把 `try_load_clash_doc` 的 JSON 分支删掉 → 解析断言红（serde_yaml 也能吃多数 JSON，但
    // `json_clash_with_json_only_escape_parses` 这条专挑 YAML 1.1 不认的转义，必红）。

    /// 一份最小 JSON 编码 Clash 订阅（两个 ss 节点）。
    const JSON_CLASH: &str = r#"{"proxies":[
        {"name":"J-1","type":"ss","server":"1.2.3.4","port":8388,"cipher":"aes-256-gcm","password":"pw1"},
        {"name":"J-2","type":"ss","server":"5.6.7.8","port":8389,"cipher":"aes-256-gcm","password":"pw2"}
    ]}"#;

    #[test]
    fn detect_json_encoded_clash() {
        assert_eq!(detect_format(JSON_CLASH), SubscriptionFormat::Clash);
        // 只有 proxy-providers 的 JSON 形态同样算 Clash。
        assert_eq!(
            detect_format(r#"{"proxy-providers":{"p":{"type":"http","url":"https://e.com/p"}}}"#),
            SubscriptionFormat::Clash
        );
        // 结构不符不得误判（`proxies` 不是数组 / `proxy-providers` 不是对象）。
        assert_eq!(
            detect_format(r#"{"proxies":"not-an-array"}"#),
            SubscriptionFormat::Unknown
        );
        // sing-box JSON 仍走 sing-box 分支（分支顺序：outbounds 优先，对齐 上游）。
        assert_eq!(
            detect_format(r#"{"outbounds":[{"type":"vless","server":"a.com"}]}"#),
            SubscriptionFormat::SingboxJson
        );
    }

    #[test]
    fn json_encoded_clash_parses_into_nodes() {
        let mut gen = {
            let mut n = 0u32;
            move || {
                n += 1;
                format!("id-{n}")
            }
        };
        let r = parse_subscription(
            JSON_CLASH,
            "sub-json",
            "2026-01-01T00:00:00Z",
            &mut gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(
            r.servers.len(),
            2,
            "JSON 编码的 Clash 应解析出 2 个节点，实得 {} + warnings {:?}",
            r.servers.len(),
            r.warnings
        );
        assert_eq!(r.servers[0].name, "J-1");
        assert_eq!(r.servers[1].address, "5.6.7.8");
        assert_eq!(r.servers[0].subscription_id.as_deref(), Some("sub-json"));
    }

    #[test]
    fn json_clash_with_json_only_escape_parses() {
        // `\/` 是合法 JSON 转义、**YAML 1.1（libyaml）不认** —— 这条钉死「必须走真 JSON 解析器」，
        // 而不是靠「YAML 是 JSON 超集」把正文直接喂给 serde_yaml。
        let text = r#"{"proxies":[{"name":"a\/b","type":"ss","server":"1.2.3.4","port":8388,"cipher":"aes-256-gcm","password":"pw"}]}"#;
        assert_eq!(detect_format(text), SubscriptionFormat::Clash);
        let mut gen = || "id-1".to_string();
        let r = parse_subscription(
            text,
            "s",
            "2026-01-01T00:00:00Z",
            &mut gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(r.servers.len(), 1, "warnings: {:?}", r.warnings);
        assert_eq!(r.servers[0].name, "a/b");
    }

    #[test]
    fn extract_proxy_providers_covers_json_encoding() {
        // 此前 `extract_proxy_providers` 单独判 `is_clash_probe`（YAML 行首探测）→ JSON 编码的
        // provider 一个都拉不到、节点全丢。改走 detect_format（单一真值）后两种编码同覆盖。
        let json = r#"{"proxy-providers":{"P1":{"type":"http","url":"https://e.com/p1"}}}"#;
        let got = extract_proxy_providers(json).expect("JSON 编码的 proxy-providers 必须被提取到");
        let map = got.as_mapping().expect("providers 应是 mapping");
        assert_eq!(map.len(), 1);
        assert_eq!(
            got.get("P1")
                .and_then(|p| p.get("url"))
                .and_then(serde_yaml::Value::as_str),
            Some("https://e.com/p1")
        );
        // YAML 编码不回归。
        assert!(extract_proxy_providers(
            "proxy-providers:\n  P1:\n    type: http\n    url: https://e.com/p1\n"
        )
        .is_some());
        // 非 Clash 正文仍返 None（不得把任意 JSON 当 Clash 拆）。
        assert!(extract_proxy_providers(r#"{"outbounds":[]}"#).is_none());
        assert!(extract_proxy_providers("vless://x@a.com:443#n").is_none());
    }

    #[test]
    fn detect_singbox_json_format() {
        let json = r#"{"outbounds":[{"type":"direct"}]}"#;
        assert_eq!(detect_format(json), SubscriptionFormat::SingboxJson);
        let json2 = r#"{"endpoints":[]}"#;
        assert_eq!(detect_format(json2), SubscriptionFormat::SingboxJson);
    }

    #[test]
    fn detect_xray_json_format() {
        // outbound 有 protocol、无 type → xray（与 sing-box 区分）。
        let json = r#"{"outbounds":[{"protocol":"vless","settings":{}}]}"#;
        assert_eq!(detect_format(json), SubscriptionFormat::XrayJson);
        // 有 type 的同键 JSON 仍判 sing-box（不误判 xray）。
        let singbox = r#"{"outbounds":[{"type":"vless","server":"a.com"}]}"#;
        assert_eq!(detect_format(singbox), SubscriptionFormat::SingboxJson);
    }

    #[test]
    fn parse_subscription_xray_yields_nodes() {
        let json = r#"{"outbounds":[
            {"protocol":"freedom"},
            {"protocol":"vless","tag":"n1","settings":{"vnext":[{"address":"a.com","port":443,"users":[{"id":"u1"}]}]},
             "streamSettings":{"network":"ws","security":"tls","wsSettings":{"path":"/p"}}}
        ]}"#;
        let mut n = 0;
        let mut id_gen = || {
            n += 1;
            format!("id-{n}")
        };
        let parsed = parse_subscription(
            json,
            "sub-x",
            "2026-07-18T00:00:00Z",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(parsed.servers.len(), 1, "vless 入库、freedom 忽略");
        assert_eq!(parsed.servers[0].address, "a.com");
        assert_eq!(parsed.servers[0].network.as_deref(), Some("ws"));
        assert_eq!(
            parsed.servers[0].subscription_id.as_deref(),
            Some("sub-x"),
            "订阅路径挂 sub id"
        );
    }

    /// endpoints-only 的 sing-box 原生订阅（机场下发 WireGuard 组网）—— 此前恒 0 节点 + warning，
    /// 现按 endpoint 建模映射入库。语料 `sing-box check` rc=0（随包核 1.14.0-beta.7）。
    #[test]
    fn parse_subscription_singbox_endpoints_only_yields_wireguard() {
        let json = r#"{"endpoints":[{
            "type":"wireguard","tag":"WG-HK","mtu":1408,
            "address":["172.16.0.2/32"],
            "private_key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEk=",
            "peers":[{"address":"wg.example.com","port":2408,
                      "public_key":"bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=",
                      "allowed_ips":["0.0.0.0/0","::/0"],
                      "persistent_keepalive_interval":25}]
        }]}"#;
        assert_eq!(detect_format(json), SubscriptionFormat::SingboxJson);
        let mut n = 0;
        let mut id_gen = || {
            n += 1;
            format!("id-{n}")
        };
        let r = parse_subscription(
            json,
            "sub-wg",
            "2026-07-18T00:00:00Z",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(r.servers.len(), 1, "endpoints[] 不再被整份丢弃");
        assert_eq!((r.skipped, r.failed), (0, 0));
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Wireguard);
        assert_eq!(s.name, "WG-HK");
        assert_eq!((s.address.as_str(), s.port), ("wg.example.com", 2408));
        assert_eq!(s.subscription_id.as_deref(), Some("sub-wg"));
        let wg = s.wireguard_settings.as_ref().unwrap();
        assert_eq!(wg.allow_internet, Some(true));
        assert!(wg.allowed_ips.is_empty(), "catch-all 全抽进 allowInternet");
    }

    /// `outbounds[]` + `endpoints[]` 同时在场：两条腿的结果合并，不互相吞。
    #[test]
    fn parse_subscription_singbox_merges_outbounds_and_endpoints() {
        let json = r#"{
          "outbounds":[
            {"type":"direct","tag":"direct"},
            {"type":"trojan","tag":"T","server":"t.example.com","server_port":443,"password":"p"}
          ],
          "endpoints":[
            {"type":"wireguard","tag":"WG","address":["10.0.0.2/32"],"private_key":"pk",
             "peers":[{"address":"1.2.3.4","port":51820,"public_key":"pub","allowed_ips":["10.0.0.0/24"]}]},
            {"type":"tailscale","tag":"TS","auth_key":"tskey-auth-SECRET"}
          ]
        }"#;
        let mut n = 0;
        let mut id_gen = || {
            n += 1;
            format!("id-{n}")
        };
        let r = parse_subscription(
            json,
            "sub-mix",
            "2026-07-18T00:00:00Z",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        let protos: Vec<Protocol> = r.servers.iter().map(|s| s.protocol).collect();
        assert_eq!(
            protos,
            vec![Protocol::Trojan, Protocol::Wireguard],
            "outbounds 的节点在前、endpoints 的在后；direct 忽略、tailscale 跳过"
        );
        assert_eq!(r.skipped, 1, "tailscale endpoint");
        assert_eq!(r.failed, 0);
        assert!(r.warnings.iter().any(|w| w.contains("tailscale endpoint")));
    }

    #[test]
    fn detect_url_list_format() {
        assert_eq!(
            detect_format("vless://uuid@host:443\nss://..."),
            SubscriptionFormat::UrlList
        );
    }

    #[test]
    fn detect_base64_format() {
        // "vless://..." 的 base64
        let b64 = base64_encode("vless://abc@host:443#name");
        assert_eq!(detect_format(&b64), SubscriptionFormat::Base64);
    }

    #[test]
    fn detect_unknown_format() {
        assert_eq!(
            detect_format("just some random text"),
            SubscriptionFormat::Unknown
        );
        assert_eq!(detect_format(""), SubscriptionFormat::Unknown);
    }

    #[test]
    fn parse_clash_subscription_full() {
        let yaml = "proxies:\n  - name: x\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-256-gcm\n    password: pw\n";
        let mut counter = 0u32;
        let mut id_gen = || {
            counter += 1;
            format!("id-{counter}")
        };
        let r = parse_subscription(
            yaml,
            "sub-1",
            "2024-01-01",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(r.servers.len(), 1);
        assert_eq!(r.servers[0].name, "x");
    }

    #[test]
    fn parse_unknown_format_warns() {
        let mut id_gen = || "id".to_string();
        let r = parse_subscription(
            "random",
            "sub-1",
            "now",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert!(r.servers.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("暂不支持")));
    }

    #[test]
    fn parse_invalid_clash_yaml_warns() {
        let mut id_gen = || "id".to_string();
        let r = parse_subscription(
            "proxies: [bad",
            "sub-1",
            "now",
            &mut id_gen,
            ImportOrigin::RemoteSubscription,
        );
        assert!(r.servers.is_empty());
        assert!(r.warnings[0].contains("Clash YAML 解析失败"));
    }

    #[test]
    fn base64_roundtrip() {
        let orig = "vless://abc@example.com:443#节点";
        let encoded = base64_encode(orig);
        assert_eq!(base64_decode(&encoded).unwrap(), orig);
    }

    #[test]
    fn base64_url_safe() {
        // 含 +/ → 转成 -_ 的 URL-safe 形式也应可解码
        let orig = "ss://aes-256-gcm:pass@host:8388";
        let std = base64_encode(orig);
        let urlsafe: String = std
            .chars()
            .map(|c| match c {
                '+' => '-',
                '/' => '_',
                _ => c,
            })
            .collect();
        assert_eq!(base64_decode(&urlsafe).unwrap(), orig);
    }

    /// 测试用 base64 编码（标准字母表）。
    fn base64_encode(input: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    fn id_gen() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("pid-{n}")
        }
    }

    /// 造一个 Clash 订阅正文（ss 节点，name 列表）。
    fn clash_body(nodes: &[(&str, &str)]) -> String {
        let proxies = nodes
            .iter()
            .map(|(name, host)| {
                format!(
                    "  - {{name: {name}, type: ss, server: {host}, port: 8388, cipher: aes-256-gcm, password: pw}}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("proxies:\n{proxies}")
    }

    /// mock fetch_text：url → `Ok(body)` / `Err(ProviderFetchError)`。
    /// 未登记的 URL 一律 **transient**（= 「没桩」不该被当成「远端确认没了」）。
    fn mock_fetch(
        map: HashMap<String, Result<String, ProviderFetchError>>,
    ) -> impl Fn(&str) -> Pin<Box<dyn Future<Output = Result<String, ProviderFetchError>> + Send>>
           + Send
           + Sync {
        let responses = Arc::new(map);
        move |url: &str| {
            let r = responses
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(ProviderFetchError::transient("no mock for url")));
            Box::pin(async move { r })
                as Pin<Box<dyn Future<Output = Result<String, ProviderFetchError>> + Send>>
        }
    }

    fn providers_yaml(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).expect("providers yaml")
    }

    #[tokio::test]
    async fn two_http_providers_merge_and_tag_provider_name() {
        let fetch = mock_fetch(HashMap::from([
            (
                "https://p1.com/sub".to_string(),
                Ok(clash_body(&[("A", "a.com")])),
            ),
            (
                "https://p2.com/sub".to_string(),
                Ok(clash_body(&[("B", "b.com")])),
            ),
        ]));
        let providers = providers_yaml(
            "p1:\n  type: http\n  url: https://p1.com/sub\np2:\n  type: http\n  url: https://p2.com/sub\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert_eq!(r.servers.len(), 2, "两 provider 各 1 节点");
        assert!(!r.any_failed);
        // 打断 provider_name 标记 → 此断言转红。
        let names: Vec<&str> = r
            .servers
            .iter()
            .filter_map(|s| s.provider_name.as_deref())
            .collect();
        assert!(
            names.contains(&"p1") && names.contains(&"p2"),
            "节点须标 provider_name: {names:?}"
        );
    }

    #[tokio::test]
    async fn transient_fetch_failure_sets_any_failed_and_names() {
        let fetch = mock_fetch(HashMap::from([
            (
                "https://ok.com/sub".to_string(),
                Ok(clash_body(&[("A", "a.com")])),
            ),
            (
                "https://bad.com/sub".to_string(),
                Err(ProviderFetchError::transient("timeout")),
            ),
        ]));
        let providers = providers_yaml(
            "ok:\n  type: http\n  url: https://ok.com/sub\nbad:\n  type: http\n  url: https://bad.com/sub\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert!(
            r.any_failed,
            "拉取失败 → any_failed（触发 merge-only 防穿仓）"
        );
        assert_eq!(r.failed_providers, vec!["bad".to_string()]);
        assert_eq!(r.servers.len(), 1, "成功 provider 节点保留");
    }

    /// **permanent 拉取失败**（4xx / SSRF 拒绝）→ 仅 warn，**不**保护存量。
    ///
    /// 变异锁：把 `resolve_proxy_providers` 的 `Err(e) if e.permanent` 臂删掉（退回「一律 transient」）
    /// → 本用例转红。守的是「provider URL 永久坏掉 → 整条订阅永久 partial」这一终态：
    /// `failed_providers` 非空会让**无 `providerName`** 的主正文内联节点也一律保留（命令层
    /// `leftover_survives_partial` 规则 2）→ 内联真下架节点永不删除，且每轮 partial 都 save+broadcast。
    #[tokio::test]
    async fn permanent_fetch_failure_does_not_protect_leftovers() {
        let fetch = mock_fetch(HashMap::from([
            (
                "https://ok.com/sub".to_string(),
                Ok(clash_body(&[("A", "a.com")])),
            ),
            (
                "https://gone.com/sub".to_string(),
                Err(ProviderFetchError::permanent("HTTP 404")),
            ),
        ]));
        let providers = providers_yaml(
            "ok:\n  type: http\n  url: https://ok.com/sub\ngone:\n  type: http\n  url: https://gone.com/sub\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert!(
            !r.any_failed,
            "永久失败不得触发 merge-only（否则订阅永久钉在 partial）"
        );
        assert!(r.failed_providers.is_empty());
        assert_eq!(r.servers.len(), 1, "成功 provider 节点仍保留");
        assert!(
            r.warnings.iter().any(|w| w.contains("永久失败")),
            "永久失败须在 warning 里可见: {:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn permanent_config_issue_warns_but_not_any_failed() {
        // type:file（安全面拒）+ 不支持 type + 缺 url —— 配置面非法 = permanent（不置 any_failed）。
        let providers = providers_yaml(
            "f:\n  type: file\n  path: /x\nq:\n  type: quic\n  url: https://q.com\nnourl:\n  type: http\n",
        );
        let mut g = id_gen();
        let fetch = mock_fetch(HashMap::new());
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert!(
            !r.any_failed,
            "配置面问题是 permanent，不置 any_failed（否则永久 merge-only）"
        );
        assert!(r.failed_providers.is_empty());
        assert!(r.servers.is_empty());
        // 汇总 warning 存在。
        assert!(
            r.warnings.iter().any(|w| w.contains("成功")),
            "应有汇总 warning: {:?}",
            r.warnings
        );
    }

    /// **0 节点 → 保护存量**（与主正文「0 节点 → merge-only」同口径）。
    ///
    /// 变异锁：把 0 节点分支改回「仅 warn、不进 `failed_providers`」→ 本用例转红。
    /// 触发形态：机场 200 + 空正文，或 `filter` 因上游改名临时滤尽 —— 判 permanent 会把该 provider
    /// 名下**全部存量节点当场删光**，而同一现象在主正文那边是不删的。
    #[tokio::test]
    async fn zero_node_provider_is_protected_like_the_main_body() {
        let fetch = mock_fetch(HashMap::from([
            (
                "https://ok.com/sub".to_string(),
                Ok(clash_body(&[("A", "a.com")])),
            ),
            (
                "https://empty.com/sub".to_string(),
                Ok("proxies: []".to_string()),
            ),
        ]));
        let providers = providers_yaml(
            "ok:\n  type: http\n  url: https://ok.com/sub\nempty:\n  type: http\n  url: https://empty.com/sub\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert!(r.any_failed, "0 节点须触发 merge-only 保护");
        assert_eq!(
            r.failed_providers,
            vec!["empty".to_string()],
            "只保护 0 节点那一个 provider，成功 provider 的真下架照常删"
        );
        assert_eq!(r.servers.len(), 1);

        // filter 滤尽（上游把节点名前缀改了）→ 同样保护。
        let fetch = mock_fetch(HashMap::from([(
            "https://f.com/sub".to_string(),
            Ok(clash_body(&[("A", "a.com")])),
        )]));
        let providers = providers_yaml(
            "flt:\n  type: http\n  url: https://f.com/sub\n  filter: \"NOTHING-MATCHES\"\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 8, &fetch, &mut g).await;
        assert!(r.any_failed, "filter 滤尽 → 保护存量（不当真下架）");
        assert_eq!(r.failed_providers, vec!["flt".to_string()]);
    }

    /// **被 `max_providers` 截断的 provider 必须进 `failed_providers`。**
    ///
    /// 变异锁：删掉截断分支里的 `out.any_failed = true` / `failed_providers.extend(truncated)`
    /// → 本用例转红。守的是最恶性的一条：第 9+ 个 provider **压根没被拉取**，此前既不进名单也不置
    /// `any_failed` → 它名下的存量节点在全量 reconcile 里被当成「远端已下架」**每轮真删**
    /// （而下一轮它仍被截断，于是删了也拿不回来）。
    #[tokio::test]
    async fn truncates_at_max_providers_and_protects_the_untried_ones() {
        let fetch = mock_fetch(HashMap::from([
            (
                "https://p1.com/sub".to_string(),
                Ok(clash_body(&[("A", "a.com")])),
            ),
            (
                "https://p2.com/sub".to_string(),
                Ok(clash_body(&[("B", "b.com")])),
            ),
        ]));
        let providers = providers_yaml(
            "p1:\n  type: http\n  url: https://p1.com/sub\np2:\n  type: http\n  url: https://p2.com/sub\n",
        );
        let mut g = id_gen();
        let r = resolve_proxy_providers(&providers, "sub1", "now", 1, &fetch, &mut g).await;
        assert_eq!(r.servers.len(), 1, "max=1 只拉第一个");
        assert!(
            r.warnings.iter().any(|w| w.contains("超上限")),
            "应有截断 warning"
        );
        assert!(
            r.any_failed,
            "有 provider 没被拉过 → 必须触发 merge-only 保护"
        );
        assert_eq!(
            r.failed_providers,
            vec!["p2".to_string()],
            "被截断的 provider 名必须进保护名单（拿不到 ≠ 下架）"
        );
        assert!(
            r.warnings.iter().any(|w| w.contains("p2")),
            "截断 warning 须点名是谁没拉: {:?}",
            r.warnings
        );
    }

    fn ss(name: &str, host: &str, port: u16, pw: &str) -> ServerConfig {
        ServerConfig {
            id: format!("id-{name}"),
            name: name.to_string(),
            protocol: Protocol::Shadowsocks,
            address: host.to_string(),
            port,
            password: Some(pw.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn fingerprint_excludes_name_includes_cred_and_network() {
        // 同 host:port:cred，仅 name 不同 → 同指纹（改名不误判增删）。
        let a = ss("HK-1", "cdn.com", 443, "pw");
        let b = ss("HK-2", "cdn.com", 443, "pw");
        assert_eq!(
            server_fingerprint(&a),
            server_fingerprint(&b),
            "改名不改指纹"
        );
        // 不同 cred → 不同指纹。
        let c = ss("HK-1", "cdn.com", 443, "other");
        assert_ne!(
            server_fingerprint(&a),
            server_fingerprint(&c),
            "cred 变 → 指纹变"
        );
        // 不同 network → 不同指纹。
        let mut d = ss("HK-1", "cdn.com", 443, "pw");
        d.network = Some("ws".to_string());
        assert_ne!(
            server_fingerprint(&a),
            server_fingerprint(&d),
            "network 变 → 指纹变"
        );
        // 指纹形态：protocol|address|port|cred|network（无 name）。
        assert_eq!(server_fingerprint(&a), "shadowsocks|cdn.com|443|pw|tcp");
    }

    #[test]
    fn dedupe_keeps_first_of_same_fingerprint() {
        let inline = ss("inline", "x.com", 443, "pw");
        let provider = ss("provider-dup", "x.com", 443, "pw"); // 同指纹（仅 name 异）
        let other = ss("other", "y.com", 443, "pw");
        let out = dedupe_by_fingerprint(vec![inline, provider, other]);
        assert_eq!(out.len(), 2, "同指纹去重");
        assert_eq!(out[0].name, "inline", "首见（内联）保留");
    }

    #[test]
    fn extract_proxy_providers_detects_and_ignores() {
        let with = "proxy-providers:\n  p1:\n    type: http\n    url: https://x.com\n";
        assert!(extract_proxy_providers(with).is_some());
        // 纯 inline clash（无 providers）→ None。
        assert!(extract_proxy_providers("proxies:\n  - {name: a, type: ss, server: a.com, port: 1, cipher: aes-256-gcm, password: p}").is_none());
        // 非 clash → None。
        assert!(extract_proxy_providers("vless://u@h:443#n").is_none());
    }
}
