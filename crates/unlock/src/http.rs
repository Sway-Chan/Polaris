//! 解锁检测传输层抽象 —— HTTP trait（取代 Polaris 的 Electron `net.request` 薄封装）。
//!
//! ## electron 依赖隔离
//! 上游 `unlock-http.ts` 把 Electron `Session`（分流出口 pin）+ `net.request`（手动重定向/body 截断/超时）
//! 绞在一起，本 crate 是纯逻辑移植：**只保留契约**（发请求 -> 拿响应 + 重定向链），把 Electron 管道全砍掉。
//! - `Session`（pin socks5 端口）→ 由 trait 实现方（B7 网络应用层）持有；本 crate 不感知代理/会话。
//! - `redirect:'manual'` 逐跳记 `Location` → 契约要求 trait 实现方返回完整 `redirect_chain`（实现方可基于
//!   reqwest 的 redirect Policy::none + 手动 follow，或 hyper 底层）。
//! - body 上限截断 / 8s 单请求超时 / 5 跳封顶 → 由实现方在真实传输中执行；本 crate 只消费结果。
//!
//! 这样 checkers.ts 的判定逻辑（只读 `status`/`body`/`redirect_chain`/`error`）可零摩擦移植，且天然可 mock 无网络测试。

use std::collections::BTreeMap;

use async_trait::async_trait;

/// HTTP 方法（仅解锁检测用到的两种）。对齐 上游 `UnlockRequest.method`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }
}

/// 单个请求。对应 上游 `UnlockRequest`。
#[derive(Debug, Clone)]
pub struct UnlockRequest {
    pub url: String,
    pub method: HttpMethod,
    /// 请求头（已小写化由实现方处理，如 reqwest 自动）。键唯一性由 BTreeMap 保证。
    pub headers: BTreeMap<String, String>,
    /// POST body（原样写出）。
    pub body: Option<String>,
}

impl UnlockRequest {
    /// GET 请求工厂（带默认 headers）。
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: BTreeMap::new(),
            body: None,
        }
    }

    /// POST 请求工厂。
    pub fn post(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Post,
            headers: BTreeMap::new(),
            body: None,
        }
    }

    /// 链式插 header（覆盖同键）。
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// 链式设 POST body。
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }
}

/// 重定向链的一跳。对应 上游 `RedirectHop`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectHop {
    pub status: u16,
    pub location: String,
}

/// 一次请求的响应。对应 上游 `UnlockResponse`。
///
/// 关键不变量（对齐 Polaris）：
/// - `status == 0` ⇒ 传输失败（`error` 非空）。checker 据 `status > 0 && error.is_empty()` 判「可达」。
/// - `body` 可被上限截断（trait 实现方负责）；`truncated` 标记。
/// - `redirect_chain` 为逐跳记录（manual redirect）。
/// - `headers` 为**终态响应**的头（见字段 doc）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnlockResponse {
    /// 最终响应状态码；网络错误/超时/构造失败 = 0。
    pub status: u16,
    /// 响应体（可能被实现方截断）。
    pub body: String,
    /// body 是否被上限截断。
    pub truncated: bool,
    /// 逐跳重定向链。
    pub redirect_chain: Vec<RedirectHop>,
    /// 非空 = 传输失败（超时 / 网络错 / 超跳）。
    pub error: Option<String>,
    /// **终态响应头**（非 30x 中间响应）。JS 挑战识别（Cloudflare challenge / Turnstile）的判据来源：
    /// 主判据是 `cf-mitigated: challenge`（CF 官方契约，所有挑战类型都设），另读 `server` / `x-country-code`。
    ///
    /// **键统一小写**（HTTP 头大小写不敏感，但 `BTreeMap` 区分大小写；trait 实现方须写入小写键——
    /// reqwest 的 `HeaderName` 本就小写规范化，照它填即可）。按名取用 [`UnlockResponse::header`]
    /// （大小写不敏感线性查），勿直接 `headers.get`（会被大小写坑）。
    ///
    /// 多值头（同名多次）取首值即可：unlock 只读 `cf-mitigated`/`server`/`x-country-code`/`location`
    /// 这几个单值头，不为多值头过度设计。
    pub headers: BTreeMap<String, String>,
}

impl UnlockResponse {
    /// 构造一个成功的响应（无重定向、无头）。
    pub fn ok(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: None,
            headers: BTreeMap::new(),
        }
    }

    /// 构造一个传输失败响应（status=0 + error）。
    pub fn err(error: impl Into<String>) -> Self {
        Self {
            status: 0,
            body: String::new(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: Some(error.into()),
            headers: BTreeMap::new(),
        }
    }

    /// 按名取响应头（**大小写不敏感**，取首个命中）。对齐 net-stack `MinimalResponse::header`。
    ///
    /// CF 挑战识别调 `resp.header("cf-mitigated")`——即使实现方漏写小写也不被坑。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 是否可达（status>0 且无传输错）。对齐 上游 `reachable()`。
    pub fn reachable(&self) -> bool {
        self.status > 0 && self.error.is_none()
    }
}

/// HTTP 传输契约。
///
/// 取代 上游 `UnlockFetch = (req) => Promise<UnlockResponse>`。实现方负责：
/// - 真实网络（B7 网络应用层基于 reqwest/hyper，带超时/body 截断/manual redirect）；
/// - 测试 mock（按 URL 返回预制响应，零网络）。
///
/// 实现方**永不 panic**（对应 上游 `fetchUnlock` 的「永不 reject」）：失败落 `status=0 + error`，
/// 由 checker 兜底为 `Timeout`。
#[async_trait]
pub trait UnlockHttp: Send + Sync {
    /// 发一个请求，返回响应（失败落 status=0 + error，不 panic、不 Result::Err）。
    async fn request(&self, req: &UnlockRequest) -> UnlockResponse;
}
