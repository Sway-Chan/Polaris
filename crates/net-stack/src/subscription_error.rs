//! 订阅错误分类（审计 §C4）。移植 上游 `shared/subscription-preview.ts` 的
//! `classifySubscriptionError` + `SUBSCRIPTION_ERROR_I18N_KEY`。
//!
//! **为什么在 Rust 侧**：上游的分类跑在 main（Node）——catch 后把错误摊平成信号再判。Polaris 的
//! main = Rust，故分类归此层；渲染侧只消费 `errorKind` + i18n key 取文案（`sub.preview.*` 十类
//! title/detail 已在 `ui/src/i18n/locales/*.json` 齐备）。TS 侧同名函数因此是**主进程逻辑的残留**
//! （零调用方），已随本批删除，避免双真值漂移。
//!
//! **分层**：本模块只做「信号 → 分类」的纯映射。拉取层（[`crate::subscription`]）对自己主动
//! 抛出的错误（scheme/ssrf/toolarge/http）**在源头直接定 kind**，不回头 re-parse 自己的字符串
//! （上游 用 `/HTTP Error: (\d+)/` 正则从自己抛的文案里反提 status —— 文案一改就静默失配）。
//! 本模块的文案关键字分支服务于**不透明信号**：注入的 HttpClient 冒泡上来的网络错误（io 错误串）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 订阅错误分类。与 TS `SubscriptionErrorKind` 联合类型逐项对齐（serde 小写 = TS 字面量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionErrorKind {
    /// 域名解析失败。
    Dns,
    /// 连接/读取超时。
    Timeout,
    /// 连接被拒绝（端口不可达）。
    Refused,
    /// 服务器返回 4xx/5xx。
    Http,
    /// 命中 SSRF guard（内网/本机地址），含重定向超限。
    Ssrf,
    /// 非 http(s) 协议。
    Scheme,
    /// 响应体积超上限。
    TooLarge,
    /// 拉到内容但非有效订阅格式。
    Parse,
    /// 解析成功但 0 节点。
    Empty,
    /// 未归类。
    Unknown,
}

/// [`classify_subscription_error`] 的输入：调用方把错误的可判定信号摊平传入。
/// 对齐 TS `SubscriptionErrorSignal`。
#[derive(Debug, Clone, Default)]
pub struct SubscriptionErrorSignal {
    /// 错误文案（拉取/解析层抛出的 message）。
    pub message: Option<String>,
    /// 平台网络错误码（如 `ECONNREFUSED`）。运行时 HttpClient 实现侧由
    /// `std::io::ErrorKind` / 平台 errno 映射填入；无则 None。
    pub code: Option<String>,
    /// HTTP 状态码（非 2xx 时由调用方显式传入，**优先级最高**）。
    pub http_status: Option<u16>,
}

/// 分类结果。`http_status` 仅 [`SubscriptionErrorKind::Http`] 时有值（供 i18n `{{status}}` 插值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionErrorClass {
    pub kind: SubscriptionErrorKind,
    pub http_status: Option<u16>,
}

/// 子串命中任一（大小写由调用方先归一）。
///
/// 用 `contains` 而非 `regex`：TS 侧这些 pattern 全是纯字面量交替（无字符类/量词/锚点），
/// regex 引擎在此零收益（简约阶梯：原生 > 依赖）。
fn any_of(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// 错误信号 → 分类。**判定顺序即优先级**（1:1 移植 TS `classifySubscriptionError`）：
///
/// 1. `http_status >= 400` → [`SubscriptionErrorKind::Http`]（确定性最强，显式传入）；
/// 2. **本项目主动抛出的确定性文案**（体积/协议/空/SSRF）—— 先于网络码，因为这些文案由我们自己
///    产出、形态稳定；
/// 3. 平台网络错误码（`ENOTFOUND` / `ETIMEDOUT` / `ECONNREFUSED` …）；
/// 4. 文案关键字兜底（错误码常被实现侧藏进 message）；
/// 5. 解析类文案 → [`SubscriptionErrorKind::Parse`]。
///
/// 未覆盖形态 → [`SubscriptionErrorKind::Unknown`]（UI 给通用文案 + 原始 message 供上报），**不误判**。
pub fn classify_subscription_error(sig: &SubscriptionErrorSignal) -> SubscriptionErrorClass {
    // 1) HTTP 状态显式传入。
    if let Some(status) = sig.http_status {
        if status >= 400 {
            return SubscriptionErrorClass {
                kind: SubscriptionErrorKind::Http,
                http_status: Some(status),
            };
        }
    }

    let code = sig.code.as_deref().unwrap_or("").to_ascii_uppercase();
    let msg = sig.message.as_deref().unwrap_or("");
    // 仅 ASCII 小写：中文不受影响，与 TS `toLowerCase()` 在本用例等价。
    let m = msg.to_lowercase();

    let cls = |kind| SubscriptionErrorClass {
        kind,
        http_status: None,
    };

    // 2) 本项目确定性文案（体积 / 协议 / 空集 / SSRF）。
    //
    // **有意分歧（上游 bug）**：TS 侧只认 `体积超过上限`，但 content-length 预检抛的是
    // `订阅响应体积 {n} 字节超过上限 {max}，已拒绝` —— 中间插了字节数，**子串不连续**，
    // 上游 自己的分类器认不出自己抛的文案 → 落 unknown（UI 显示「无法验证订阅」而非「订阅过大」）。
    // 此处补 `字节超过上限` 覆盖该形态。
    // 刻意**不**收敛成 `超过上限`：那会把 `重定向次数超过上限` 抢在 ssrf 分支前误判成 toolarge。
    if any_of(
        msg,
        &["体积超过上限", "字节超过上限", "too large", "导入内容过大"],
    ) {
        return cls(SubscriptionErrorKind::TooLarge);
    }
    if any_of(msg, &["协议不支持", "仅允许 http"]) {
        return cls(SubscriptionErrorKind::Scheme);
    }
    if any_of(msg, &["0 个可用节点", "得到 0 个", "为空"]) {
        return cls(SubscriptionErrorKind::Empty);
    }
    if any_of(
        &m,
        &[
            "内网",
            "本机",
            "私有地址",
            "ssrf",
            "loopback",
            "private",
            "重定向次数超过",
        ],
    ) {
        return cls(SubscriptionErrorKind::Ssrf);
    }

    // 3) 平台网络错误码。
    match code.as_str() {
        "ENOTFOUND" | "EAI_AGAIN" => return cls(SubscriptionErrorKind::Dns),
        "ETIMEDOUT" | "UND_ERR_CONNECT_TIMEOUT" | "ABORT_ERR" => {
            return cls(SubscriptionErrorKind::Timeout)
        }
        "ECONNREFUSED" | "ECONNRESET" | "EHOSTUNREACH" | "ENETUNREACH" => {
            return cls(SubscriptionErrorKind::Refused)
        }
        _ => {}
    }

    // 4) 文案关键字兜底。
    if any_of(&m, &["err_name_not_resolved", "getaddrinfo", "dns"]) {
        return cls(SubscriptionErrorKind::Dns);
    }
    if any_of(
        &m,
        &[
            "err_timed_out",
            "timeout",
            "timed out",
            "aborted",
            "the operation was aborted",
        ],
    ) {
        return cls(SubscriptionErrorKind::Timeout);
    }
    if any_of(
        &m,
        &[
            "err_connection_refused",
            "connection refused",
            "econnrefused",
        ],
    ) {
        return cls(SubscriptionErrorKind::Refused);
    }
    if any_of(
        &m,
        &[
            "err_connection_reset",
            "err_connection_closed",
            "err_address_unreachable",
            "unreachable",
        ],
    ) {
        return cls(SubscriptionErrorKind::Refused);
    }

    // 5) 解析类文案。
    if any_of(
        &m,
        &[
            "解析失败",
            "无法识别",
            "结构异常",
            "格式错误",
            "不是有效",
            "parse",
            "yaml",
            "json",
        ],
    ) || any_of(msg, &["解析", "识别", "格式"])
    {
        return cls(SubscriptionErrorKind::Parse);
    }

    cls(SubscriptionErrorKind::Unknown)
}

/// errorKind → i18n key 对（渲染侧 `t()` 取展示文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionErrorI18nKey {
    pub title: &'static str,
    pub detail: &'static str,
}

/// 分类 → i18n key。1:1 对齐 TS `SUBSCRIPTION_ERROR_I18N_KEY`（渲染侧同名常量保留，供 UI 直接取）。
///
/// 十类 title/detail 已在 `ui/src/i18n/locales/*.json` 的 `sub.preview.*` 齐备；
/// `httpDetail` 带 `{{status}}` 插值 → 取 [`SubscriptionErrorClass::http_status`]。
pub fn subscription_error_i18n_key(kind: SubscriptionErrorKind) -> SubscriptionErrorI18nKey {
    use SubscriptionErrorKind as K;
    let (title, detail) = match kind {
        K::Dns => ("sub.preview.dnsTitle", "sub.preview.dnsDetail"),
        K::Timeout => ("sub.preview.timeoutTitle", "sub.preview.timeoutDetail"),
        K::Refused => ("sub.preview.refusedTitle", "sub.preview.refusedDetail"),
        K::Http => ("sub.preview.httpTitle", "sub.preview.httpDetail"),
        K::Ssrf => ("sub.preview.ssrfTitle", "sub.preview.ssrfDetail"),
        K::Scheme => ("sub.preview.schemeTitle", "sub.preview.schemeDetail"),
        K::TooLarge => ("sub.preview.toolargeTitle", "sub.preview.toolargeDetail"),
        K::Parse => ("sub.preview.parseTitle", "sub.preview.parseDetail"),
        K::Empty => ("sub.preview.emptyTitle", "sub.preview.emptyDetail"),
        K::Unknown => ("sub.preview.unknownTitle", "sub.preview.unknownDetail"),
    };
    SubscriptionErrorI18nKey { title, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(message: &str) -> SubscriptionErrorSignal {
        SubscriptionErrorSignal {
            message: Some(message.to_string()),
            ..Default::default()
        }
    }

    fn kind_of(message: &str) -> SubscriptionErrorKind {
        classify_subscription_error(&sig(message)).kind
    }

    #[test]
    fn http_status_wins_over_everything() {
        // 状态码优先级最高：即便 message 像 DNS 错误也归 http。
        let c = classify_subscription_error(&SubscriptionErrorSignal {
            message: Some("getaddrinfo ENOTFOUND".to_string()),
            code: Some("ENOTFOUND".to_string()),
            http_status: Some(403),
        });
        assert_eq!(c.kind, SubscriptionErrorKind::Http);
        assert_eq!(c.http_status, Some(403));
    }

    #[test]
    fn http_status_below_400_is_not_http() {
        // 3xx/2xx 不是 http 错误 —— 不得吞掉真实原因。
        let c = classify_subscription_error(&SubscriptionErrorSignal {
            message: Some("connection refused".to_string()),
            code: Some("ECONNREFUSED".to_string()),
            http_status: Some(302),
        });
        assert_eq!(c.kind, SubscriptionErrorKind::Refused);
        assert_eq!(c.http_status, None);
    }

    #[test]
    fn deterministic_messages_classify() {
        assert_eq!(
            kind_of("订阅响应体积 99 字节超过上限 10485760，已拒绝"),
            SubscriptionErrorKind::TooLarge
        );
        assert_eq!(kind_of("导入内容过大"), SubscriptionErrorKind::TooLarge);
        assert_eq!(
            kind_of("订阅地址协议不支持（仅允许 http/https）: ftp://x/y"),
            SubscriptionErrorKind::Scheme
        );
        assert_eq!(
            kind_of("解析得到 0 个可用节点：未识别到任何可用节点"),
            SubscriptionErrorKind::Empty
        );
    }

    /// 两种体积文案都要认（`体积超过上限` 与 `体积 N 字节超过上限`）。
    /// 上游 只认前者 → 自己 content-length 预检抛的后者落 unknown（已在实现处记为有意分歧）。
    #[test]
    fn both_oversize_message_shapes_classify() {
        assert_eq!(
            kind_of("订阅响应体积超过上限 10485760，已拒绝"),
            SubscriptionErrorKind::TooLarge
        );
        assert_eq!(
            kind_of("订阅响应体积 20971520 字节超过上限 10485760，已拒绝"),
            SubscriptionErrorKind::TooLarge
        );
    }

    /// 排序陷阱回归：`重定向次数超过上限` 也含「超过上限」，但它是 SSRF 族，
    /// **不得**被 toolarge 分支抢走（needle 若收敛成「超过上限」即转红）。
    #[test]
    fn redirect_limit_is_ssrf_not_toolarge() {
        assert_eq!(
            kind_of("重定向次数超过上限（5），已拒绝"),
            SubscriptionErrorKind::Ssrf
        );
    }

    #[test]
    fn ssrf_messages_classify() {
        // assert_host_allowed 的真实文案（ssrf.rs:248）。
        assert_eq!(
            kind_of("订阅地址指向本机/内网/link-local，已拒绝: localhost"),
            SubscriptionErrorKind::Ssrf
        );
        // safe_redirect 的重定向超限文案。
        assert_eq!(
            kind_of("重定向次数超过上限（5），已拒绝"),
            SubscriptionErrorKind::Ssrf
        );
    }

    #[test]
    fn network_codes_classify() {
        let by_code = |c: &str| {
            classify_subscription_error(&SubscriptionErrorSignal {
                code: Some(c.to_string()),
                ..Default::default()
            })
            .kind
        };
        assert_eq!(by_code("ENOTFOUND"), SubscriptionErrorKind::Dns);
        assert_eq!(by_code("EAI_AGAIN"), SubscriptionErrorKind::Dns);
        assert_eq!(by_code("ETIMEDOUT"), SubscriptionErrorKind::Timeout);
        assert_eq!(by_code("ABORT_ERR"), SubscriptionErrorKind::Timeout);
        assert_eq!(by_code("ECONNREFUSED"), SubscriptionErrorKind::Refused);
        assert_eq!(by_code("EHOSTUNREACH"), SubscriptionErrorKind::Refused);
        // 小写 code 也认（实现侧可能传 errno 名的小写形态）。
        assert_eq!(by_code("econnrefused"), SubscriptionErrorKind::Refused);
    }

    #[test]
    fn message_keyword_fallback_classifies() {
        assert_eq!(
            kind_of("net::ERR_NAME_NOT_RESOLVED"),
            SubscriptionErrorKind::Dns
        );
        assert_eq!(
            kind_of("operation timed out"),
            SubscriptionErrorKind::Timeout
        );
        assert_eq!(
            kind_of("tcp connect error: Connection refused (os error 111)"),
            SubscriptionErrorKind::Refused
        );
        assert_eq!(
            kind_of("network is unreachable"),
            SubscriptionErrorKind::Refused
        );
    }

    #[test]
    fn parse_messages_classify() {
        assert_eq!(
            kind_of("订阅 YAML 解析失败: mapping values are not allowed"),
            SubscriptionErrorKind::Parse
        );
        assert_eq!(
            kind_of("暂不支持的订阅格式: Unknown"),
            SubscriptionErrorKind::Parse
        );
    }

    #[test]
    fn unknown_when_nothing_matches() {
        assert_eq!(
            kind_of("something went sideways"),
            SubscriptionErrorKind::Unknown
        );
        assert_eq!(
            classify_subscription_error(&SubscriptionErrorSignal::default()).kind,
            SubscriptionErrorKind::Unknown
        );
    }

    #[test]
    fn i18n_keys_cover_all_kinds() {
        // 十类逐一有 key，且 title/detail 不重复（防复制粘贴串行）。
        use SubscriptionErrorKind as K;
        let all = [
            K::Dns,
            K::Timeout,
            K::Refused,
            K::Http,
            K::Ssrf,
            K::Scheme,
            K::TooLarge,
            K::Parse,
            K::Empty,
            K::Unknown,
        ];
        let mut seen = std::collections::HashSet::new();
        for k in all {
            let key = subscription_error_i18n_key(k);
            assert!(key.title.starts_with("sub.preview."));
            assert!(key.detail.starts_with("sub.preview."));
            assert!(seen.insert(key.title), "title 重复: {}", key.title);
            assert!(seen.insert(key.detail), "detail 重复: {}", key.detail);
        }
    }

    #[test]
    fn serde_matches_ts_literals() {
        // 序列化字面量须与 TS SubscriptionErrorKind 联合类型逐字一致（IPC 契约）。
        let json = |k: SubscriptionErrorKind| serde_json::to_string(&k).unwrap();
        assert_eq!(json(SubscriptionErrorKind::Dns), "\"dns\"");
        assert_eq!(json(SubscriptionErrorKind::TooLarge), "\"toolarge\"");
        assert_eq!(json(SubscriptionErrorKind::Ssrf), "\"ssrf\"");
        assert_eq!(json(SubscriptionErrorKind::Unknown), "\"unknown\"");
    }
}
