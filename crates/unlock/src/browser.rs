//! 浏览器请求头集 —— 与 [`UA`](crate::endpoints::UA) **自洽**的 Chrome 请求头构造（纯函数，零 IO）。
//!
//! ## 为什么需要（根因 (c)）
//!
//! 迁移调研 `~/docs/polaris/design/polaris-unlock-detection-cf.md` §3.3 定案：Polaris 此前每个检测请求
//! **只发 `User-Agent`** 一个头；且 `src-tauri` 的 reqwest 是 `default-features = false` 未开压缩 feature
//! → **连 `Accept-Encoding` 都不发**。
//!
//! 「自称 Chrome，却不发 `Accept-Encoding`/`Accept`/`Accept-Language`」是一条**独立于 TLS 指纹**的、
//! 极强的 bot 信号 —— UA 与行为自相矛盾。上游侧不写这些头不是因为不需要，而是 Chromium 的 `net` 栈
//! **自动补齐**了它们（调研 §1.3）。本模块把那份「隐式自动补齐」在 Rust 侧变成显式常量。
//!
//! ## 自洽性是硬约束（本模块的全部价值）
//!
//! 补头的收益**完全来自自洽**，补出矛盾比不补更糟。故三条不变量由单测钉死（见本文件 `tests`）：
//!
//! 1. **`sec-ch-ua` 的 Chrome 主版本号必须 = UA 里的**（= [`CHROME_MAJOR`](crate::endpoints::CHROME_MAJOR)）。
//!    漂一个版本 = 新造一条矛盾信号。
//! 2. **`sec-ch-ua-platform` 必须 = UA 里的平台**（UA 写 `Windows NT 10.0` → 必须 `"Windows"`）。
//! 3. **`Accept-Encoding` 声明的每种编码，传输层必须真能解压**。这条**跨 crate**，由
//!    `src-tauri/src/runtime/http.rs` 的回环解压门守（本 crate 看不见 reqwest 的 feature 集）。
//!    违反后果不是「被识别」而是**静默误判**：拿回压缩字节流当 body 做 marker 匹配 → 所有 checker 判错。
//!
//! ## profile：为什么分两种而不是一套通吃
//!
//! Chrome 对「地址栏导航」与「页内 fetch」发的头**本就不同**。若给 JSON API 发
//! `Accept: text/html` + `Upgrade-Insecure-Requests: 1`，那又是一条新造的矛盾。故按用途分：
//!
//! - [`RequestProfile::Navigate`] —— 顶层文档 GET（= 地址栏敲 URL 回车）。Chrome 此形态**不带**
//!   `Origin`/`Referer`，`Sec-Fetch-Site: none` + `Sec-Fetch-User: ?1`，**完全自洽**。
//! - [`RequestProfile::Api`] —— 同源 fetch/XHR。`Accept: */*` + `Sec-Fetch-Mode: cors`。
//!   Chrome 对同源 **GET** fetch 不发 `Origin`，对同源 **POST** 发 —— 故 `Origin` 由
//!   [`browser_headers`] 按 method 决定（见 [`origin_of`]）。
//!
//! ## 诚实边界：本模块修不了什么
//!
//! - **header 顺序**：Chrome 有固定发送序，而 [`UnlockRequest`](crate::http::UnlockRequest) 用
//!   `BTreeMap` → 字典序发出。这是 HTTP 层的残余指纹信号，phase-1 无解。
//! - **TLS/JA3 + HTTP/2 SETTINGS 指纹**：本模块一点没动（调研 §3.2 判定的**主因**）。
//!
//! 二者都由 phase-2 的 `wreq` 指纹客户端（`src-tauri/src/runtime/unlock_http.rs`）解决。
//! 本模块在 phase-2 后仍是 `UnlockRequest` 的头集 SoT（`wreq` 的 emulation 负责传输层形态，
//! 业务头仍由此出），故**不是**临时脚手架。

use crate::endpoints::UA;
use crate::http::HttpMethod;

// ── 与 UA 自洽的常量集（改任一条都必须同步改 UA，单测会拦）──────────────────────

/// `Accept-Encoding`：= Chrome 真机值（131~**149** 各版一致 —— 这批模板共用同一个
/// `header_initializer_with_zstd_priority` → `header_chrome_accept!(zstd, …)`，该 arm 的字面量在
/// `wreq-util-3.0.0-rc.14/src/emulate/macros.rs:124`），且**每种编码传输层都真能解压**。
///
/// 由 `polaris-unlock-transport` 的 `wreq` 四个压缩 feature（`gzip`/`deflate`/`brotli`/`zstd`）落实，
/// 并由该 crate 的**回环解压门**逐编码钉死（见 `crates/unlock-transport/src/lib.rs` 的
/// `declared_accept_encoding_is_decodable_end_to_end`）。
///
/// **「声明了却解不开」是本模块最危险的失效形态**：body 拿回的是压缩字节流，marker 匹配全落空
/// → 所有 checker **静默误判**（不是报错，是安静地给出错误结论）。故这条不变量必须有牙。
pub const ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// `Accept-Language`（原 Netflix 特调头，现升为全局默认 —— 对齐 上游 `NETFLIX_HDRS` 的值）。
///
/// 检测判据（Netflix `Oh no!` / Disney `forbidden-location` / ChatGPT `unsupported_country`）全按英文页面标定，
/// 故必须钉 `en-US`：跟随出口地区语言会让 marker 匹配不到 → 误判。
pub const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// `sec-ch-ua`（UA Client Hints 品牌列表）。两个真品牌的版本号必须 =
/// [`CHROME_MAJOR`](crate::endpoints::CHROME_MAJOR)（单测钉死）。
///
/// **第三项是 GREASE 品牌，逐版不同、不可派生**：Chrome 每个大版本换一次这个占位品牌名，
/// `wreq-util` 的模板逐版抄了真机值（3.x 起在 `src/emulate/profile/chrome.rs`）——
/// v131 = `"Not_A Brand";v="24"`、v136 = `"Not:A-Brand";v="24"`、v137 = `"Not/A)Brand";v="24"`、
/// **v149 = `"Not)A;Brand";v="24"`**。
/// 升 `CHROME_MAJOR` 时必须照该文件对应 `mod_generator!(vNNN, …)` 行的 **Windows** 条目手动同步这一段
/// （无自动门：GREASE 品牌按规范本就是任意串，测不出「对错」，只能对模板）。
///
/// 本行的 149 值逐字抄自 `wreq-util-3.0.0-rc.14/src/emulate/profile/chrome.rs:1787`
/// （`mod_generator!(v149, …)` 的 `Windows` 条目；同版 UA 在下一行 :1788）。
/// 品牌**顺序**也照模板：`Google Chrome` → `Chromium` → GREASE（v137 同序；注意 v138/v148 等版本
/// 模板里的顺序与品牌名都不同，不可跨版照抄）。
pub const SEC_CH_UA: &str =
    "\"Google Chrome\";v=\"149\", \"Chromium\";v=\"149\", \"Not)A;Brand\";v=\"24\"";

/// `sec-ch-ua-mobile`：桌面 UA → `?0`。
pub const SEC_CH_UA_MOBILE: &str = "?0";

/// `sec-ch-ua-platform`：必须与 [`UA`] 的 `Windows NT 10.0` 一致。
pub const SEC_CH_UA_PLATFORM: &str = "\"Windows\"";

/// 顶层文档导航的 `Accept`（Chrome 真机值，逐字；131~**149** 未变 —— 同上，v137 与 v149 的模板都走
/// `header_chrome_accept!(zstd, …)`，该字面量在 `wreq-util-3.0.0-rc.14/src/emulate/macros.rs:121`）。
pub const ACCEPT_NAVIGATE: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";

/// fetch/XHR 的 `Accept`。
pub const ACCEPT_API: &str = "*/*";

/// `priority`（RFC 9218 结构化优先级，Chrome 自 117 起在**所有**请求上发）。
/// 顶层文档 = 最高优先级 `u=0`；子资源 / fetch = `u=1`。`i` = incremental。
pub const PRIORITY_NAVIGATE: &str = "u=0, i";
pub const PRIORITY_API: &str = "u=1, i";

// ── profile ──────────────────────────────────────────────────────────────────

/// 请求用途 —— 决定 `Accept` / `Sec-Fetch-*` / `Upgrade-Insecure-Requests` 这组**上下文相关**头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestProfile {
    /// 顶层文档 GET（地址栏导航）：claude.ai/、gemini.google.com/、Netflix title 页、disneyplus.com/、ios.chat.openai.com/。
    Navigate,
    /// 同源 fetch/XHR：cdn-cgi/trace、api.openai.com/compliance、spotify signup、bamgrid devices/token/graphql。
    Api,
}

/// 取 URL 的 origin（`scheme://host[:port]`）——`Origin` 头的值。非法 URL → `None`。
///
/// 纯字符串切分（不引 `url` crate）：只需支持 `scheme://authority/...` 这一种形态，
/// 且 authority 里的 userinfo 在检测端点上不存在（简约阶梯：一行切分够用即不加依赖）。
#[must_use]
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

/// 构造与 [`UA`] 自洽的完整浏览器请求头集。
///
/// 返回 `(name, value)` 列（含 `User-Agent` 本身）——调用方逐条塞进
/// [`UnlockRequest`](crate::http::UnlockRequest)。**不含**业务头（`Authorization` / `Content-Type`），
/// 那些由 checker 自己追加（后设者覆盖，见 `UnlockRequest::header`）。
///
/// `method` 只影响 `Origin`：Chrome 对同源 GET fetch 不发、对同源 POST 发。
#[must_use]
pub fn browser_headers(
    profile: RequestProfile,
    method: HttpMethod,
    url: &str,
) -> Vec<(&'static str, String)> {
    // 全形态共有（Chrome 任何请求都发）。
    let mut out: Vec<(&'static str, String)> = vec![
        ("User-Agent", UA.to_string()),
        ("Accept-Encoding", ACCEPT_ENCODING.to_string()),
        ("Accept-Language", ACCEPT_LANGUAGE.to_string()),
        ("sec-ch-ua", SEC_CH_UA.to_string()),
        ("sec-ch-ua-mobile", SEC_CH_UA_MOBILE.to_string()),
        ("sec-ch-ua-platform", SEC_CH_UA_PLATFORM.to_string()),
    ];

    match profile {
        RequestProfile::Navigate => {
            // 地址栏导航形态：无 Origin/Referer、Sec-Fetch-Site: none、Sec-Fetch-User: ?1。
            out.push(("Accept", ACCEPT_NAVIGATE.to_string()));
            out.push(("Upgrade-Insecure-Requests", "1".to_string()));
            out.push(("Sec-Fetch-Dest", "document".to_string()));
            out.push(("Sec-Fetch-Mode", "navigate".to_string()));
            out.push(("Sec-Fetch-Site", "none".to_string()));
            out.push(("Sec-Fetch-User", "?1".to_string()));
            out.push(("priority", PRIORITY_NAVIGATE.to_string()));
        }
        RequestProfile::Api => {
            out.push(("Accept", ACCEPT_API.to_string()));
            out.push(("Sec-Fetch-Dest", "empty".to_string()));
            out.push(("Sec-Fetch-Mode", "cors".to_string()));
            out.push(("Sec-Fetch-Site", "same-origin".to_string()));
            // Chrome：同源 GET fetch **不发** Origin，同源 POST **发**。发错方向即新造矛盾。
            if method == HttpMethod::Post {
                if let Some(origin) = origin_of(url) {
                    out.push(("Origin", origin));
                }
            }
            out.push(("priority", PRIORITY_API.to_string()));
        }
    }
    out
}

// ── 自洽性校验用的纯解析器（单测消费；也便于漂移时定位）─────────────────────────

/// 从 UA 提 Chrome 主版本号（`Chrome/137.0.0.0` → `137`）。
#[must_use]
pub fn chrome_major_from_ua(ua: &str) -> Option<u32> {
    let after = ua.split("Chrome/").nth(1)?;
    let major = after.split('.').next()?;
    major.parse().ok()
}

/// 从 UA 提 UA-CH 平台 token（`Windows NT` → `"Windows"`；`Macintosh` → `"macOS"`；`X11`/`Linux` → `"Linux"`）。
///
/// 返回值**含引号**，可与 [`SEC_CH_UA_PLATFORM`] 直接比较（UA-CH 是 structured-header string）。
#[must_use]
pub fn platform_token_from_ua(ua: &str) -> Option<&'static str> {
    if ua.contains("Windows NT") {
        Some("\"Windows\"")
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        Some("\"macOS\"")
    } else if ua.contains("X11") || ua.contains("Linux") {
        Some("\"Linux\"")
    } else {
        None
    }
}

/// 拆 [`ACCEPT_ENCODING`] 为编码 token 列（去 `q=` 权重、去空白、小写）。
///
/// 消费方 = `src-tauri` 的**跨 crate 解压门**：逐 token 验传输层真能解压。
#[must_use]
pub fn accept_encoding_tokens() -> Vec<&'static str> {
    ACCEPT_ENCODING
        .split(',')
        .map(|t| t.split(';').next().unwrap_or(t).trim())
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::CHROME_MAJOR;

    fn get(headers: &[(&'static str, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    // ── 不变量 1/2：header 集必须与 UA 自洽（补出矛盾 = 比不补更糟）───────────────

    #[test]
    fn sec_ch_ua_major_version_matches_ua() {
        // **浏览器身份同源门（unlock crate 这一半）**：UA 与 sec-ch-ua 都必须钉在
        // [`CHROME_MAJOR`] 上。另一半（`Emulation::Chrome{N}`）由 `polaris-unlock-transport`
        // 的 const 派生 + 源码扫描门守。
        //
        // 变异验证：
        //  - 只升 UA（`Chrome/137`→`Chrome/138`）不动 CHROME_MAJOR → 第一条断言红；
        //  - 只升 CHROME_MAJOR 不动 UA → 同一条红；
        //  - 只升 UA + CHROME_MAJOR 忘了 SEC_CH_UA → 后两条红。
        let major = chrome_major_from_ua(UA).expect("UA 必须含 Chrome/<major>");
        assert_eq!(
            major, CHROME_MAJOR,
            "UA 的 Chrome 主版本必须 = CHROME_MAJOR（endpoints.rs 的唯一真值源）"
        );
        assert!(
            SEC_CH_UA.contains(&format!("\"Google Chrome\";v=\"{major}\"")),
            "sec-ch-ua 的 Google Chrome 版本必须 = UA 的 {major}，实得: {SEC_CH_UA}"
        );
        assert!(
            SEC_CH_UA.contains(&format!("\"Chromium\";v=\"{major}\"")),
            "sec-ch-ua 的 Chromium 版本必须 = UA 的 {major}，实得: {SEC_CH_UA}"
        );
    }

    #[test]
    fn sec_ch_ua_platform_matches_ua_platform() {
        // 打断点：UA 写 Windows 而 SEC_CH_UA_PLATFORM 填 "macOS" → 转红。
        let from_ua = platform_token_from_ua(UA).expect("UA 必须能判出平台");
        assert_eq!(
            SEC_CH_UA_PLATFORM, from_ua,
            "sec-ch-ua-platform 必须与 UA 的平台一致"
        );
    }

    #[test]
    fn platform_parser_covers_three_desktop_families() {
        assert_eq!(
            platform_token_from_ua("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"),
            Some("\"Windows\"")
        );
        assert_eq!(
            platform_token_from_ua("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"),
            Some("\"macOS\"")
        );
        assert_eq!(
            platform_token_from_ua("Mozilla/5.0 (X11; Linux x86_64)"),
            Some("\"Linux\"")
        );
        assert_eq!(platform_token_from_ua("curl/8.0"), None);
    }

    #[test]
    fn chrome_major_parser_rejects_non_chrome_ua() {
        // 刻意用一个**与 CHROME_MAJOR 无关**的版本号：这是解析器的夹具，不是第二处版本钉子
        // （若照抄真值，日后升级时会被误当成「还有一处要改」而引发无谓改动）。
        assert_eq!(
            chrome_major_from_ua("Mozilla/5.0 ... Chrome/99.0.0.0 Safari"),
            Some(99)
        );
        assert_eq!(chrome_major_from_ua("curl/8.0"), None);
    }

    // ── 不变量 3 的本地半腿：声明的编码集非空且是已知可解码名 ──────────────────
    // （真正的「传输层解得开」由 src-tauri 回环解压门守 —— 跨 crate，本 crate 看不见 reqwest feature）

    #[test]
    fn accept_encoding_tokens_are_parsed_and_nonempty() {
        let toks = accept_encoding_tokens();
        assert_eq!(toks, vec!["gzip", "deflate", "br", "zstd"]);
        assert!(
            !toks.is_empty(),
            "Accept-Encoding 不得为空——自称 Chrome 却不发压缩协商即 (c) 的原始 bug"
        );
    }

    #[test]
    fn accept_encoding_matches_chrome_exactly() {
        // Chrome 真机值（经 wreq-util `header_chrome_accept!(zstd, …)` 模板核对；131~149 一致）。
        // 多声明一种解不开的编码 → 静默误判；少声明 → 与 UA 不完全一致的弱信号。
        // 打断点：改成 "gzip" → 转红；加 "sdch"（无解码器）→ 转红（unlock-transport 的解压门也会红）。
        assert_eq!(ACCEPT_ENCODING, "gzip, deflate, br, zstd");
    }

    // ── 基础头集：全形态共有的六条必须在 ──────────────────────────────────────

    #[test]
    fn every_profile_sends_the_always_on_chrome_headers() {
        for profile in [RequestProfile::Navigate, RequestProfile::Api] {
            let h = browser_headers(profile, HttpMethod::Get, "https://example.com/");
            for name in [
                "User-Agent",
                "Accept",
                "Accept-Encoding",
                "Accept-Language",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
            ] {
                assert!(
                    get(&h, name).is_some(),
                    "{profile:?} 缺 {name} —— 自称 Chrome 却不发即 (c) 的 bot 信号"
                );
            }
            assert_eq!(get(&h, "User-Agent").as_deref(), Some(UA));
        }
    }

    // ── Navigate profile：地址栏导航形态自洽 ─────────────────────────────────

    #[test]
    fn navigate_profile_is_a_self_consistent_top_level_navigation() {
        let h = browser_headers(
            RequestProfile::Navigate,
            HttpMethod::Get,
            "https://claude.ai/",
        );
        assert_eq!(get(&h, "Accept").as_deref(), Some(ACCEPT_NAVIGATE));
        assert_eq!(get(&h, "Sec-Fetch-Dest").as_deref(), Some("document"));
        assert_eq!(get(&h, "Sec-Fetch-Mode").as_deref(), Some("navigate"));
        assert_eq!(get(&h, "Sec-Fetch-Site").as_deref(), Some("none"));
        assert_eq!(get(&h, "Sec-Fetch-User").as_deref(), Some("?1"));
        assert_eq!(get(&h, "Upgrade-Insecure-Requests").as_deref(), Some("1"));
        assert_eq!(get(&h, "priority").as_deref(), Some(PRIORITY_NAVIGATE));
        // 地址栏导航**不带** Origin/Referer —— 带了才是矛盾。
        assert!(get(&h, "Origin").is_none(), "顶层导航不得带 Origin");
        assert!(get(&h, "Referer").is_none(), "顶层导航不得带 Referer");
    }

    // ── Api profile：同源 fetch 形态 + Origin 的 method 依赖 ──────────────────

    #[test]
    fn api_profile_get_has_no_origin_but_post_does() {
        // Chrome 语义：同源 GET fetch 不发 Origin，同源 POST 发。发反 = 新造矛盾信号。
        // 打断点：把 `if method == Post` 去掉（GET 也发 Origin）→ 前半转红；
        //         把整个 Origin 分支删掉 → 后半转红。
        let g = browser_headers(
            RequestProfile::Api,
            HttpMethod::Get,
            "https://api.openai.com/compliance/cookie_requirements",
        );
        assert!(get(&g, "Origin").is_none(), "同源 GET fetch 不得带 Origin");

        let p = browser_headers(
            RequestProfile::Api,
            HttpMethod::Post,
            "https://disney.api.edge.bamgrid.com/devices",
        );
        assert_eq!(
            get(&p, "Origin").as_deref(),
            Some("https://disney.api.edge.bamgrid.com"),
            "同源 POST 必须带 Origin"
        );
    }

    #[test]
    fn api_profile_does_not_claim_to_be_a_document_navigation() {
        // 给 JSON API 发 text/html + Upgrade-Insecure-Requests 就是新造的矛盾 —— 必须不发。
        let h = browser_headers(
            RequestProfile::Api,
            HttpMethod::Get,
            "https://spclient.wg.spotify.com/signup/public/v1/account?validate=1",
        );
        assert_eq!(get(&h, "Accept").as_deref(), Some(ACCEPT_API));
        assert!(get(&h, "Upgrade-Insecure-Requests").is_none());
        assert!(get(&h, "Sec-Fetch-User").is_none());
        assert_eq!(get(&h, "Sec-Fetch-Dest").as_deref(), Some("empty"));
        assert_eq!(get(&h, "Sec-Fetch-Mode").as_deref(), Some("cors"));
        // 子资源/fetch 的优先级 ≠ 顶层文档的 u=0（发反也是矛盾）。
        assert_eq!(get(&h, "priority").as_deref(), Some(PRIORITY_API));
    }

    // ── origin_of 纯函数 ─────────────────────────────────────────────────────

    #[test]
    fn origin_of_extracts_scheme_and_authority() {
        assert_eq!(
            origin_of("https://disney.api.edge.bamgrid.com/devices"),
            Some("https://disney.api.edge.bamgrid.com".to_string())
        );
        assert_eq!(
            origin_of("https://Example.COM:8443/a/b?c=1"),
            Some("https://example.com:8443".to_string())
        );
        assert_eq!(origin_of("not-a-url"), None);
        assert_eq!(origin_of("https://"), None);
    }
}
