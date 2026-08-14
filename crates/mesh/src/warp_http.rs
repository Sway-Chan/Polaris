//! WARP 传输层抽象 —— HTTP trait + WarpService 编排。
//!
//! 1:1 移植自 上游 `src/main/services/WarpService.ts`，剥离 node `https` + `crypto`。
//!
//! ## 维度7 R5：TLS 指纹回归点
//! Polaris 刻意用 node `https`（可钉 TLS1.2）而非 Electron `net.fetch`（会协商 TLS1.3/h2）——Cloudflare WAF
//! 校验 TLS 指纹，rustls 的 ClientHello ≠ Node/okhttp → 1020/403。本 crate 把 WARP 注册/许可/注销的网络
//! 发送全部抽象到 [`WarpHttp`] trait：实现方（应用层）负责用匹配的 TLS 指纹栈发请求（如 native-tls/openssl
//! 仿 okhttp，或经 helper 子进程复用 Node 指纹）。本 crate 只消费 `WarpHttpResponse`（status + body），
//! 不感知 TLS/HTTP 版本，测试 mock 注入零网络。
//!
//! ## 密钥对生成
//! Polaris 用 node `crypto.generateKeyPairSync('x25519')` → 取 DER 末 32 字节裸 key。同样抽象到
//! [`WarpKeypair`] trait，实现方用对应密码学库（ring/ed25519-dalek 等），本 crate 不绑定密码学后端。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::warp::{
    self, build_register_body, build_unregister_request, classify_deregister_result,
    parse_register_response, DeregisterResult, WarpWireGuardDraft, WARP_API_BASE, WARP_API_VERSION,
    WARP_CLIENT_VERSION, WARP_USER_AGENT,
};

/// WARP HTTP 响应：status + 截断后的 body。对齐 上游 `requestStatus` 的返回（注销需据 status/body 分类）。
/// register/applyLicense 的非 2xx 在 trait 实现方就映射为 `Err`（与 上游 `request` 一致：非 2xx reject）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarpHttpResponse {
    pub status: u16,
    pub body: String,
}

/// WARP HTTP 请求入参。实现方据 method/url/headers/body + TLS1.2-only/HTTP1.1 指纹发送。
#[derive(Debug, Clone)]
pub struct WarpHttpRequest {
    pub method: WarpHttpMethod,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
}

/// WARP 请求方法（仅用到 POST/PUT/DELETE）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpHttpMethod {
    Post,
    Put,
    Delete,
}

impl WarpHttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            WarpHttpMethod::Post => "POST",
            WarpHttpMethod::Put => "PUT",
            WarpHttpMethod::Delete => "DELETE",
        }
    }
}

/// WARP HTTP 传输契约（维度7 R5 隔离点）。
///
/// 实现方负责：
/// - 真实网络（应用层用匹配 okhttp/Node TLS 指纹的栈，钉 TLS1.2、HTTP/1.1，否则 CF 1020/403）；
/// - 15s 超时、响应体上限（register 截断 ~200B 用于错误信息、unregister 截断前 512B 供 body-code-1020 检测）；
/// - 响应流错误（对端 RST/提前关闭）显式映射为 `Err`（Polaris 注释：否则 Promise 永挂）；
/// - register/applyLicense：非 2xx → `Err`（带 status + 截断 body）；2xx → `Ok(body)`（body 为 JSON 文本）；
/// - unregister：**不抛错**，返回 `Ok(WarpHttpResponse{status,body})` 让 [`WarpService`] 据状态分类
///   （上游 `unregister` 用 `requestStatus` 而非 `request`，正是为保留 4xx 状态做 done/drop/retry 分类）；
///   仅网络层错误（无 HTTP 状态）才 `Err`。
///
/// 测试 mock：按 url 返回预制响应，零网络。
#[async_trait]
pub trait WarpHttp: Send + Sync {
    /// register/applyLicense：成功返 JSON body 文本；非 2xx 或网络错返 Err。
    async fn json_request(&self, req: &WarpHttpRequest) -> Result<String, String>;

    /// unregister：返 status+body（即使 4xx）；仅网络层错误返 Err。
    async fn status_request(&self, req: &WarpHttpRequest) -> Result<WarpHttpResponse, String>;
}

/// Curve25519 keypair 生成契约。返回 (private_key_b64, public_key_b64)。
/// Polaris 用 `crypto.generateKeyPairSync('x25519')` 取 DER 末 32 字节裸 key。
pub trait WarpKeypair: Send + Sync {
    fn generate_keypair(&self) -> (String, String);
}

/// 默认标准请求头（register/applyLicense 共用，applyLicense 额外加 Authorization）。
/// 对齐 上游 `register`/`applyLicense` 的 headers（User-Agent + CF-Client-Version + Content-Type + Accept）。
fn default_headers() -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert("User-Agent".to_string(), WARP_USER_AGENT.to_string());
    h.insert(
        "CF-Client-Version".to_string(),
        WARP_CLIENT_VERSION.to_string(),
    );
    h.insert("Content-Type".to_string(), "application/json".to_string());
    h.insert("Accept".to_string(), "application/json".to_string());
    h
}

/// WARP 服务（纯逻辑编排，依赖注入 [`WarpHttp`] + [`WarpKeypair`]）。
///
/// 1:1 移植 上游 `WarpService` 的 register/applyLicense/unregister 编排逻辑；
/// 网络（node https）→ `WarpHttp`，密钥（node crypto）→ `WarpKeypair`，日志 → `WarpLog`。
pub struct WarpService<H, K, L> {
    http: H,
    keypair: K,
    log: L,
    /// 可覆盖的 API 版本段（默认 [`WARP_API_VERSION`]）。CF 滚动版本时调用方可注入新串重试。
    api_version: String,
}

/// WARP 日志契约（取代 上游 `LogManager`）。实现方负责落盘/转发；本 crate 不感知。
pub trait WarpLog: Send + Sync {
    /// level: "info" | "warn" | "error"；message 已格式化好；source 恒 "WarpService"。
    fn log(&self, level: &str, message: &str);
}

/// 空实现日志（No-op），供不需要日志的调用方/测试用。
pub struct NoopWarpLog;
impl WarpLog for NoopWarpLog {
    fn log(&self, _level: &str, _message: &str) {}
}

/// register 可选入参。上游 `register(opts?: { licenseKey?: string })`。
#[derive(Debug, Clone, Default)]
pub struct RegisterOptions {
    pub license_key: Option<String>,
}

/// applyLicense 返回。上游 `applyLicense` 返回 `{ warpPlus, license }`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyLicenseResult {
    pub warp_plus: bool,
    pub license: String,
}

impl<H, K, L> WarpService<H, K, L>
where
    H: WarpHttp,
    K: WarpKeypair,
    L: WarpLog,
{
    /// 新建。用默认 API 版本段。
    pub fn new(http: H, keypair: K, log: L) -> Self {
        Self {
            http,
            keypair,
            log,
            api_version: WARP_API_VERSION.to_string(),
        }
    }

    /// 用指定 API 版本段新建（CF 版本滚动时调用方可注入新串）。
    pub fn with_api_version(http: H, keypair: K, log: L, api_version: impl Into<String>) -> Self {
        Self {
            http,
            keypair,
            log,
            api_version: api_version.into(),
        }
    }

    /// 注册一台匿名 WARP 设备并产出 WG 草稿。licenseKey 非空则注册后应用 WARP+ license（失败不阻断，降级免费）。
    /// token 仅本流程内存使用，随 warpDevice 落节点。上游 `WarpService.register`。
    pub async fn register(&self, opts: RegisterOptions) -> Result<WarpWireGuardDraft, String> {
        let (private_key, public_key) = self.keypair.generate_keypair();
        let base = format!("{}/{}", WARP_API_BASE, self.api_version);
        let headers = default_headers();
        let tos = rfc3339_utc_now();
        let body = serde_json::to_string(&build_register_body(&public_key, &tos))
            .map_err(|e| format!("WARP body 序列化失败: {}", e))?;
        self.log.log(
            "info",
            &format!("WARP 注册中（version={}）…", self.api_version),
        );
        let req = WarpHttpRequest {
            method: WarpHttpMethod::Post,
            url: format!("{}/reg", base),
            headers: headers.clone(),
            body: Some(body),
        };
        let reg_text = self.http.json_request(&req).await?;
        let reg_json: Value =
            serde_json::from_str(&reg_text).map_err(|_| "WARP API 返回非 JSON".to_string())?;
        let mut result = parse_register_response(&reg_json)?;
        let token = reg_json
            .get("token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let license_key = opts
            .license_key
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        if let Some(key) = license_key {
            if !token.is_empty() && !result.device_id.is_empty() {
                match self.apply_license(&result.device_id, &token, key).await {
                    Ok(acct) => {
                        result.warp_plus = acct.warp_plus;
                        if !acct.license.is_empty() {
                            result.license = acct.license;
                        }
                    }
                    Err(e) => {
                        self.log.log(
                            "warn",
                            &format!("WARP+ license 应用失败（降级免费版）: {}", e),
                        );
                    }
                }
            }
        }

        self.log.log(
            "info",
            &format!(
                "WARP 注册成功：{}:{}（warp_plus={}）",
                result.address, result.port, result.warp_plus
            ),
        );

        Ok(WarpWireGuardDraft {
            address: result.address,
            port: result.port,
            private_key,
            peer_public_key: result.peer_public_key,
            local_address: result.local_address,
            reserved: result.reserved,
            meta: warp::WarpDraftMeta {
                device_id: result.device_id.clone(),
                account_id: result.account_id,
                license: result.license,
                warp_plus: result.warp_plus,
            },
            warp_device: warp::WarpDeviceCreds {
                device_id: result.device_id,
                token,
            },
        })
    }

    /// 对**已注册设备**应用 WARP+ license（PUT /reg/{deviceId}/account）。
    /// 升级免重新注册：license 是对设备所属账户打补丁、不新建设备，能原地升级免费→WARP+。
    /// 需设备 token（Bearer 鉴权）。无 token 的旧节点无法升级（须删了重建）。上游 `applyLicense`。
    pub async fn apply_license(
        &self,
        device_id: &str,
        token: &str,
        license: &str,
    ) -> Result<ApplyLicenseResult, String> {
        if device_id.is_empty() || token.is_empty() {
            return Err("缺少设备凭据（deviceId/token）".to_string());
        }
        let key = license.trim();
        if key.is_empty() {
            return Err("license 为空".to_string());
        }
        let base = format!("{}/{}", WARP_API_BASE, self.api_version);
        let mut headers = default_headers();
        headers.insert("Authorization".to_string(), format!("Bearer {}", token));
        let body = serde_json::to_string(&serde_json::json!({ "license": key }))
            .map_err(|e| format!("WARP body 序列化失败: {}", e))?;
        let req = WarpHttpRequest {
            method: WarpHttpMethod::Put,
            url: format!("{}/reg/{}/account", base, device_id),
            headers,
            body: Some(body),
        };
        let acct_text = self.http.json_request(&req).await?;
        let acct: Value =
            serde_json::from_str(&acct_text).map_err(|_| "WARP API 返回非 JSON".to_string())?;
        let warp_plus = acct
            .get("warp_plus")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let id_prefix: String = device_id.chars().take(8).collect();
        self.log.log(
            "info",
            &format!(
                "WARP+ license 已应用（{}…，warp_plus={}）",
                id_prefix, warp_plus
            ),
        );
        let lic = acct
            .get("license")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| key.to_string());
        Ok(ApplyLicenseResult {
            warp_plus,
            license: lic,
        })
    }

    /// 注销一台已注册的 WARP 设备（删除节点时调）。上游 `unregister`。
    /// 裸 `DELETE /{version}/reg/{deviceId}` + Bearer token + 同 register 的 TLS1.2/okhttp 指纹。
    /// **捕获 HTTP 状态码**（不像 register 非 2xx 即抛丢失状态）→ 经 [`classify_deregister_result`]
    /// 返 Done/Drop/Retry；网络异常 → classify(None)=Retry。不抛异常（队列重试语义靠返回值）。
    /// 日志只打 deviceId 前缀，绝不打 token。
    pub async fn unregister(&self, device_id: &str, token: &str) -> DeregisterResult {
        if device_id.is_empty() || token.is_empty() {
            return DeregisterResult::Drop; // 无凭据无从注销，视作放弃出队。
        }
        let (url, headers) = build_unregister_request(&self.api_version, device_id, token);
        let id_prefix: String = device_id.chars().take(8).collect();
        let req = WarpHttpRequest {
            method: WarpHttpMethod::Delete,
            url,
            headers,
            body: None,
        };
        match self.http.status_request(&req).await {
            Ok(resp) => {
                // 关键：把 body 一并传给分类——CF WAF 1020 常以 403 携带 body code 1020 返回，
                // 须优先于 401/403 的 Drop 走 Retry（Polaris 同款逻辑）。
                let body_opt = if resp.body.is_empty() {
                    None
                } else {
                    Some(resp.body.as_str())
                };
                let result = classify_deregister_result(Some(resp.status), body_opt);
                self.log.log(
                    "info",
                    &format!(
                        "WARP 设备注销 {}… → HTTP {}（{:?}）",
                        id_prefix, resp.status, result
                    ),
                );
                result
            }
            Err(e) => {
                // 网络失败 / 超时：无 HTTP 状态 → Retry。
                let result = classify_deregister_result(None, Some(e.as_str()));
                self.log.log(
                    "info",
                    &format!(
                        "WARP 设备注销 {}… 网络异常（{:?}）: {}",
                        id_prefix, result, e
                    ),
                );
                result
            }
        }
    }
}

/// 构造 RFC3339 UTC 时间戳（ToS 接受时间）。
/// Polaris 用 `new Date().toISOString()`；本 crate 不接入系统时钟（纯逻辑），故暴露给调用方注入。
/// 此处提供一默认实现基于 SystemTime；测试可通过 [`WarpKeypair`] 之外的时钟抽象覆盖（见 register 调用方）。
fn rfc3339_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // 秒级精度足够 ToS（Polaris 的 toISOString 是毫秒，此处不追求 bit-精确；真实实现方可用 chrono）。
    let secs = dur.as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // 从 1970-01-01 起算 civil date（Howard Hinnant 算法）。
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        year, month, d, h, m, s
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 记录型 keypair mock：返回固定 (priv, pub)。
    struct FixedKeypair;
    impl WarpKeypair for FixedKeypair {
        fn generate_keypair(&self) -> (String, String) {
            ("PRIVB64".to_string(), "PUBB64".to_string())
        }
    }

    /// HTTP mock：按 method+url 片段匹配，返回预设结果。
    #[derive(Default, Clone)]
    struct MockHttp {
        responses: Arc<Mutex<Vec<MockResp>>>,
    }

    struct MockResp {
        match_url_contains: String,
        match_method: WarpHttpMethod,
        outcome: MockOutcome,
    }

    enum MockOutcome {
        JsonOk(String),
        JsonErr(String),
        Status(WarpHttpResponse),
        NetErr(String),
    }

    impl MockHttp {
        fn push_json_ok(&self, match_url_contains: &str, method: WarpHttpMethod, body: &str) {
            self.responses.lock().unwrap().push(MockResp {
                match_url_contains: match_url_contains.to_string(),
                match_method: method,
                outcome: MockOutcome::JsonOk(body.to_string()),
            });
        }
        fn push_status(
            &self,
            match_url_contains: &str,
            method: WarpHttpMethod,
            resp: WarpHttpResponse,
        ) {
            self.responses.lock().unwrap().push(MockResp {
                match_url_contains: match_url_contains.to_string(),
                match_method: method,
                outcome: MockOutcome::Status(resp),
            });
        }
    }

    #[async_trait]
    impl WarpHttp for MockHttp {
        async fn json_request(&self, req: &WarpHttpRequest) -> Result<String, String> {
            let mut guard = self.responses.lock().unwrap();
            for (i, r) in guard.iter().enumerate() {
                if r.match_method == req.method && req.url.contains(&r.match_url_contains) {
                    let outcome = std::mem::replace(
                        &mut guard[i].outcome,
                        MockOutcome::JsonErr("consumed".into()),
                    );
                    return match outcome {
                        MockOutcome::JsonOk(body) => Ok(body),
                        MockOutcome::JsonErr(e) => Err(e),
                        _ => Err("json_request matched a status outcome".to_string()),
                    };
                }
            }
            Err(format!("no mock for {} {}", req.method.as_str(), req.url))
        }

        async fn status_request(&self, req: &WarpHttpRequest) -> Result<WarpHttpResponse, String> {
            let mut guard = self.responses.lock().unwrap();
            for (i, r) in guard.iter().enumerate() {
                if r.match_method == req.method && req.url.contains(&r.match_url_contains) {
                    let outcome = std::mem::replace(
                        &mut guard[i].outcome,
                        MockOutcome::JsonErr("consumed".into()),
                    );
                    return match outcome {
                        MockOutcome::Status(resp) => Ok(resp),
                        MockOutcome::NetErr(e) => Err(e),
                        _ => Err("status_request matched a json outcome".to_string()),
                    };
                }
            }
            Err(format!("no mock for {} {}", req.method.as_str(), req.url))
        }
    }

    fn ok_register_body() -> String {
        serde_json::json!({
            "id": "devid",
            "token": "secret-token",
            "account": { "id": "acctid", "license": "lic", "warp_plus": false },
            "config": {
                "interface": { "addresses": { "v4": "172.16.0.2" } },
                "peers": [{ "public_key": "PEERPUB", "endpoint": { "host": "engage.cloudflareclient.com:2408" } }],
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn register_no_license_produces_draft() {
        let http = MockHttp::default();
        http.push_json_ok("/reg", WarpHttpMethod::Post, &ok_register_body());
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        let draft = svc.register(RegisterOptions::default()).await.unwrap();
        assert_eq!(draft.address, "engage.cloudflareclient.com");
        assert_eq!(draft.port, 2408);
        assert_eq!(draft.private_key, "PRIVB64");
        assert_eq!(draft.peer_public_key, "PEERPUB");
        assert_eq!(draft.local_address, vec!["172.16.0.2/32"]);
        assert_eq!(draft.warp_device.device_id, "devid");
        assert_eq!(draft.warp_device.token, "secret-token");
        assert!(!draft.meta.warp_plus);
    }

    #[tokio::test]
    async fn register_with_license_applies_then_updates_warpplus() {
        let http = MockHttp::default();
        http.push_json_ok("/reg", WarpHttpMethod::Post, &ok_register_body());
        http.push_json_ok(
            "/account",
            WarpHttpMethod::Put,
            serde_json::json!({ "warp_plus": true, "license": "newlic" })
                .to_string()
                .as_str(),
        );
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        let draft = svc
            .register(RegisterOptions {
                license_key: Some("mykey".to_string()),
            })
            .await
            .unwrap();
        assert!(draft.meta.warp_plus);
        assert_eq!(draft.meta.license, "newlic");
    }

    #[tokio::test]
    async fn register_license_failure_degrades_to_free() {
        let http = MockHttp::default();
        http.push_json_ok("/reg", WarpHttpMethod::Post, &ok_register_body());
        // applyLicense 失败：json_request 返 Err。
        http.responses.lock().unwrap().push(MockResp {
            match_url_contains: "/account".to_string(),
            match_method: WarpHttpMethod::Put,
            outcome: MockOutcome::JsonErr("WARP API 403: error 1020".to_string()),
        });
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        // register 不应因 license 失败而失败（降级免费）。
        let draft = svc
            .register(RegisterOptions {
                license_key: Some("mykey".to_string()),
            })
            .await
            .unwrap();
        assert!(!draft.meta.warp_plus);
    }

    #[tokio::test]
    async fn register_non2xx_errors() {
        let http = MockHttp::default();
        http.responses.lock().unwrap().push(MockResp {
            match_url_contains: "/reg".to_string(),
            match_method: WarpHttpMethod::Post,
            outcome: MockOutcome::JsonErr("WARP API 403: error 1020".to_string()),
        });
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        let err = svc.register(RegisterOptions::default()).await.unwrap_err();
        assert!(err.contains("403"));
    }

    #[tokio::test]
    async fn apply_license_requires_credentials() {
        let http = MockHttp::default();
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert!(svc.apply_license("", "tok", "lic").await.is_err());
        assert!(svc.apply_license("dev", "", "lic").await.is_err());
        assert!(svc.apply_license("dev", "tok", "  ").await.is_err());
    }

    #[tokio::test]
    async fn unregister_204_is_done() {
        let http = MockHttp::default();
        http.push_status(
            "/reg/dev-123",
            WarpHttpMethod::Delete,
            WarpHttpResponse {
                status: 204,
                body: String::new(),
            },
        );
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(
            svc.unregister("dev-123", "tok").await,
            DeregisterResult::Done
        );
    }

    #[tokio::test]
    async fn unregister_404_is_done() {
        let http = MockHttp::default();
        http.push_status(
            "/reg/dev-123",
            WarpHttpMethod::Delete,
            WarpHttpResponse {
                status: 404,
                body: String::new(),
            },
        );
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(
            svc.unregister("dev-123", "tok").await,
            DeregisterResult::Done
        );
    }

    #[tokio::test]
    async fn unregister_403_with_1020_is_retry() {
        let http = MockHttp::default();
        http.push_status(
            "/reg/dev-123",
            WarpHttpMethod::Delete,
            WarpHttpResponse {
                status: 403,
                body: "error 1020".to_string(),
            },
        );
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(
            svc.unregister("dev-123", "tok").await,
            DeregisterResult::Retry
        );
    }

    #[tokio::test]
    async fn unregister_401_is_drop() {
        let http = MockHttp::default();
        http.push_status(
            "/reg/dev-123",
            WarpHttpMethod::Delete,
            WarpHttpResponse {
                status: 401,
                body: String::new(),
            },
        );
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(
            svc.unregister("dev-123", "tok").await,
            DeregisterResult::Drop
        );
    }

    #[tokio::test]
    async fn unregister_network_error_is_retry() {
        let http = MockHttp::default();
        http.responses.lock().unwrap().push(MockResp {
            match_url_contains: "/reg/dev-123".to_string(),
            match_method: WarpHttpMethod::Delete,
            outcome: MockOutcome::NetErr("ETIMEDOUT".to_string()),
        });
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(
            svc.unregister("dev-123", "tok").await,
            DeregisterResult::Retry
        );
    }

    #[tokio::test]
    async fn unregister_no_credentials_is_drop() {
        let http = MockHttp::default();
        let svc = WarpService::new(http, FixedKeypair, NoopWarpLog);
        assert_eq!(svc.unregister("", "tok").await, DeregisterResult::Drop);
        assert_eq!(svc.unregister("dev", "").await, DeregisterResult::Drop);
    }
}
