//! 解锁检测端点 / 判定 marker / titleId / apiKey —— 全集中此单文件。
//!
//! 1:1 移植自 上游 `unlock-endpoints.ts`。为何单文件：社区检测端点/marker 半年级漂移（Netflix titleId、Disney
//! 公开 apiKey、Gemini 可用性 marker、OpenAI 支持国家清单）。漂移时**只改此处**，不动 checker 逻辑与引擎。
//! 判定法沿用社区标准（RegionRestrictionCheck 系）。

use regex::Regex;
use std::sync::LazyLock;

/// **我们伪装的 Chrome 主版本号 —— 浏览器身份的唯一真值源（SoT）。**
///
/// 浏览器身份被编码在三处，它们必须**同版**，否则得到的不是「旧指纹」而是**自相矛盾的指纹**
/// （TLS 说 A、UA 说 B）——那比单纯陈旧更糟，指纹服务专抓这种。三处全部由本常量收口：
///
/// | 处 | 绑定方式 | 漏改的后果 |
/// |---|---|---|
/// | [`UA`]（本文件） | 单测 `sec_ch_ua_major_version_matches_ua` 断言 `chrome_major_from_ua(UA) == CHROME_MAJOR` | 测试红 |
/// | [`SEC_CH_UA`](crate::browser::SEC_CH_UA) | 同上一门断言两个品牌的 `v="…"` = 本常量 | 测试红 |
/// | `polaris-unlock-transport` 的 `wreq_util::Profile::Chrome{N}` | **const 派生**（`chrome_emulation(CHROME_MAJOR)`），无第二处字面量 | wreq-util 无该模板 ⇒ **编译失败** |
///
/// 升级流程：改本常量 → 同步 [`UA`] 与 `SEC_CH_UA` 的版本号（含 `sec-ch-ua` 的 GREASE 品牌串，
/// 每版不同，见 `SEC_CH_UA` 文档）→ 跑 `cargo test --workspace`。任一处漏改必红。
///
/// **上限由 `wreq-util` 的模板集决定**（3.0.0-rc.14 的 Chrome 模板止于 149）。升过头 = 编译期报错，
/// 绝不会静默退回旧模板。
pub const CHROME_MAJOR: u32 = 149;

/// 统一 UA（社区常用桌面 Chrome UA；对端按 UA 分流时保持一致）。上游 `UA`。
///
/// 主版本号必须 = [`CHROME_MAJOR`]（单测钉死）。逐字对齐 `wreq-util` Chrome149 模板的 **Windows 行**
/// （`wreq-util-3.0.0-rc.14/src/emulate/profile/chrome.rs:1788`）——emulation 与 UA 同源才不自相矛盾。
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// 出口 trace（缓存 key 用）：apex 域 cdn-cgi/trace，与 上游 `IpInfoService.EP_CF_TRACE` 同源。
/// 上游 `EGRESS_TRACE_URL`。**仅 proxy 腿用**——绝不进直连链（旁路由透明分流会把它劫走代理误标直连出口）。
pub const EGRESS_TRACE_URL: &str = "https://cloudflare.com/cdn-cgi/trace";

/// 本地直连出口探测**专用**端点（direct 腿）：**仅国内** ipip，绝不 fallback 到国外端点
/// （cloudflare/ip-api/ipify）。旁路由/软路由透明分流会把国外目标劫持走代理出口 → 直连出口被误标为
/// 境外节点 IP；国内接口走真实大陆出口，是这类环境下唯一测得对本地直连出口的办法。与 [`EGRESS_TRACE_URL`]
/// （仅 proxy 腿）互斥。上游 `IpInfoService.EP_IPIP`（host=myip.ipip.net path=/json，http:80 absolute-form）。
pub const DIRECT_IPINFO_URL: &str = "http://myip.ipip.net/json";

/// 单请求超时 / body 上限 / 最大重定向跳数（传输层硬约束）。上游 `REQ_TIMEOUT_MS` / `MAX_BODY_BYTES` / `MAX_REDIRECTS`。
pub const REQ_TIMEOUT_MS: u64 = 8_000;
pub const MAX_BODY_BYTES: usize = 1_500_000;
pub const MAX_REDIRECTS: usize = 5;

/// 单 checker 总预算（收敛 Disney 主链+备法 4 连请求的尾延迟）。上游 `CHECKER_BUDGET_MS`。
pub const CHECKER_BUDGET_MS: u64 = 15_000;

// ----------------------------------------------------------------------------
// ChatGPT
// ----------------------------------------------------------------------------

/// ChatGPT：cookie_requirements(r1) + ios 首页(r2) + trace(loc)。上游 `CHATGPT`。
pub struct ChatgptEndpoints;

impl ChatgptEndpoints {
    pub const TRACE_URL: &'static str = "https://chat.openai.com/cdn-cgi/trace";
    pub const COOKIE_URL: &'static str = "https://api.openai.com/compliance/cookie_requirements";
    pub const IOS_URL: &'static str = "https://ios.chat.openai.com/";
    /// r1（cookie_requirements）body 命中即「不支持地区」（对齐 check.sh：r1 只查 unsupported_country）。
    pub const COOKIE_BLOCK_MARKER: &'static str = "unsupported_country";
    /// r2（ios 首页）body 命中即「VPN/代理被识别」（对齐 check.sh：r2 只查 VPN）。
    pub const IOS_BLOCK_MARKER: &'static str = "VPN";
}

// ----------------------------------------------------------------------------
// Claude
// ----------------------------------------------------------------------------

/// Claude：claude.ai/（manual redirect）+ trace(loc)。上游 `CLAUDE`。
pub struct ClaudeEndpoints;

impl ClaudeEndpoints {
    pub const HOME_URL: &'static str = "https://claude.ai/";
    pub const TRACE_URL: &'static str = "https://claude.ai/cdn-cgi/trace";
    /// 重定向 Location 命中即封禁（地区不可用）。
    pub const BLOCK_MARKER: &'static str = "app-unavailable-in-region";
}

// ----------------------------------------------------------------------------
// Gemini
// ----------------------------------------------------------------------------

/// Gemini：gemini.google.com/（跟随重定向）。marker 脆弱、低置信，真机校准。上游 `GEMINI`。
pub struct GeminiEndpoints;

impl GeminiEndpoints {
    pub const HOME_URL: &'static str = "https://gemini.google.com/";
    /// 可用性 marker（社区经验值，脆弱）。命中 -> ok，缺失 -> blocked（对齐 check.sh WebTest_Gemini）。
    pub const AVAILABLE_MARKER: &'static str = "45631641,null,true";

    /// region（仅展示，可缺）：body 里 `,2,1,200,"XXX"` 提 3 字母地区码。上游 `GEMINI.regionRe`。
    pub fn region_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#",2,1,200,"([A-Z]{3})"#).unwrap());
        &RE
    }
}

// ----------------------------------------------------------------------------
// Grok（xAI）—— 弱检测（设计 §3）
// ----------------------------------------------------------------------------

/// Grok（xAI）弱检测端点。**诚实边界**：仅站点可达 / 风控拦截 / 超时 + 出口国家码；
/// **测不了**登录后模型可用性（EU 的 Grok 4.5 限制是**模型级**，站点仍 200；datacenter IP 的
/// Authentication error 在认证会话内，裸 HTTP 不可见）。
///
/// ## 来源（本机实测 2026-07-16，JP 出口 `loc=JP`/`colo=NRT`，单出口单次，普适性待多出口标定）
/// - `GET https://grok.com/` → 200 + 419KB Next.js 应用页；`server: cloudflare`；响应头 `x-country-code: JP`；未触发挑战。
/// - `GET https://grok.com/cdn-cgi/trace` → 200，`loc=JP`（region 来源，与 chatgpt/claude 同构）。
/// - `GET https://api.x.ai/v1/models`（无凭证）→ 干净 401 JSON（L2 佐证探针；**不接入判定**，待标定后评估）。
///
/// ## G3 Blocked geo marker：**查无**
/// EU 出口预期站点仍 200（限制是模型级非站点级），社区脚本对 grok 的地区判定同样只有
/// 「trace loc + 硬编码制裁名单」这类启发（设计 §8.5）。故 **Blocked 判定 marker 全部待真机标定
/// （EU/TR/CN/US 出口）**——上线初期为空规则，**禁编造端点/marker**。
///
/// ## 待标定后才上的强规则（设计 §9.1 G3′，本批**刻意不实现**）
/// 匿名 `POST https://grok.com/rest/models`（body `{}`）实测 200 返模型清单，是唯一匿名可读、
/// 且随出口地区变化的结构化地区信号。但哨兵 modelId **必须**由 US/EU 双出口真机差集定出
/// （清单还受 A/B 实验与上下架影响，不纯是 geo），设计 §11 明确「未标定前 G3′ 不上线」——
/// 上一条会误报的强规则比维持弱检测更糟。标定项见 `~/docs/polaris/design/polaris-unlock-challenge-and-grok.md` §10 T7。
///
/// ## runtime 语义待真机标定
/// reqwest/wreq 的 TLS 指纹与 curl/Chrome 均不同 → CF 挑战真实触发率**本机无法真触发**（本机直连、无受限出口）。
/// 分类器逻辑用 mock 各态验，触发率登记为待真机标定（见设计 §4/§6）。
pub struct GrokEndpoints;

impl GrokEndpoints {
    /// 首页（CF 后，实测 200 + Next.js 应用页）——判定主体（G1 可达 / G2 挑战 / G4 特征）。
    pub const HOME_URL: &'static str = "https://grok.com/";
    /// 出口 trace（`loc=` 取国家码，与 chatgpt/claude checker 同构）。实测可用。
    pub const TRACE_URL: &'static str = "https://grok.com/cdn-cgi/trace";
    /// 应用页特征 marker（G4：实测出现于正常 200 页的 Next.js chunk 引用）。
    /// **单出口实测**，跨出口稳定性待真机标定；最终以标定数据定稿（设计 §3.3/§4）。
    pub const APP_MARKER: &'static str = "cdn.grok.com/_next";
    /// region 辅源响应头（grok 边缘按请求 IP 自算的国家码；实测稳定出现，但**非公开契约**）。
    /// 主源仍是 [`Self::TRACE_URL`] 的 `loc=`（与其它 checker 同构）。
    pub const COUNTRY_HEADER: &'static str = "x-country-code";
    // G3 Blocked geo marker：查无静态 geo-block marker → **空规则**，禁编造。真机标定（§4/§10）后回填。
}

// ----------------------------------------------------------------------------
// Netflix
// ----------------------------------------------------------------------------

/// Netflix：两非自制 titleId（对齐 check.sh MediaUnlockTest_Netflix），manual redirect。上游 `NETFLIX`。
pub struct NetflixEndpoints;

impl NetflixEndpoints {
    /// 非自制 title #1（LEGO Ninjago 81280792）。上游 `NETFLIX.nonOriginalUrl`。
    pub const NON_ORIGINAL_URL: &'static str = "https://www.netflix.com/title/81280792";
    /// 非自制 title #2（Breaking Bad 70143836）。
    pub const NON_ORIGINAL_URL_2: &'static str = "https://www.netflix.com/title/70143836";
    /// body 命中即该 title 在本地区不可看（Netflix "Oh no!" 错误页）。
    pub const OH_NO_MARKER: &'static str = "Oh no!";
    /// Netflix **未在本国提供业务**（大陆 CN 等）标记：HTTP 403 + body "Not Available"。
    /// 区别于 per-title 限制的 "Oh no!"（国家有 Netflix 但该片不可看=partial）。
    pub const NOT_AVAILABLE_MARKER: &'static str = "Not Available";

    /// region（仅展示）：countryName 前最近的 "id" 值（本地化 payload）。上游 `NETFLIX.regionRe`。
    /// `[^{}]` 限同一 JSON 对象内、长度封顶 —— 防跨对象误取与大 body 回溯失控。
    pub fn region_re() -> &'static Regex {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#""id":"([^"]{1,32})"[^{}]{0,200}?"countryName""#).unwrap()
        });
        &RE
    }
}

// ----------------------------------------------------------------------------
// Disney+
// ----------------------------------------------------------------------------

/// Disney+：bamgrid 三段串联 devices -> token -> graphql，末段再打 disneyplus.com 取最终 URL 判 preview/未上线。
/// 上游 `DISNEY`。源 lmc999/RegionRestrictionCheck `disney_check`。
pub struct DisneyEndpoints;

impl DisneyEndpoints {
    pub const DEVICES_URL: &'static str = "https://disney.api.edge.bamgrid.com/devices";
    pub const TOKEN_URL: &'static str = "https://disney.api.edge.bamgrid.com/token";
    pub const GRAPHQL_URL: &'static str =
        "https://disney.api.edge.bamgrid.com/graph/v1/device/graphql";
    /// disneyplus.com 首页（跟随重定向后取最终 URL 判 preview）。
    pub const PREVIEW_URL: &'static str = "https://www.disneyplus.com/";

    /// 公开静态 Bearer（disney&browser&1.0.0）。devices/token 用 `Bearer <bearer>`，graphql 用**裸 token**（无前缀）。
    /// 源 lmc999/RegionRestrictionCheck；漂移时同 cookie 模板一并刷新。
    pub const BEARER: &'static str =
        "ZGlzbmV5JmJyb3dzZXImMS4wLjA.Cu56AgSfBTDag5NiRA81oLHkDZfu5L3CKadnefEAY84";

    /// devices 探针 body（固定）。上游 `devicesBody`（JSON.stringify 形态）。
    pub const DEVICES_BODY: &'static str = r#"{"deviceFamily":"browser","applicationRuntime":"chrome","deviceProfile":"windows","attributes":{}}"#;

    /// token grant body 模板（token-exchange，form-urlencoded）。源 lmc999 cookies 第 1 行，原样进常量。
    /// 占位 `DISNEYASSERTION` 由 checker 替换为 devices 拿到的 assertion。上游 `tokenBodyTemplate`。
    pub const TOKEN_BODY_TEMPLATE: &'static str = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&latitude=0&longitude=0&platform=browser&subject_token=DISNEYASSERTION&subject_token_type=urn%3Abamtech%3Aparams%3Aoauth%3Atoken-type%3Adevice";

    /// token body 里被替换的 assertion 占位符。上游 `assertionPlaceholder`。
    pub const ASSERTION_PLACEHOLDER: &'static str = "DISNEYASSERTION";

    /// graphql refreshToken mutation body 模板。源 lmc999 cookies 第 8 行，原样进常量（字面 `\n` 转义原封保留）。
    /// 占位 `ILOVEDISNEY` 由 checker 替换为 token 拿到的 refresh_token。上游 `graphqlBodyTemplate`。
    pub const GRAPHQL_BODY_TEMPLATE: &'static str = "{\"query\":\"mutation refreshToken($input: RefreshTokenInput!) {\\n            refreshToken(refreshToken: $input) {\\n                activeSession {\\n                    sessionId\\n                }\\n            }\\n        }\",\"variables\":{\"input\":{\"refreshToken\":\"ILOVEDISNEY\"}}}";

    /// graphql body 里被替换的 refresh_token 占位符。上游 `refreshTokenPlaceholder`。
    pub const REFRESH_TOKEN_PLACEHOLDER: &'static str = "ILOVEDISNEY";

    /// token 响应 body 命中即 Disney 地区拒绝 -> blocked。上游 `forbiddenMarker`。
    pub const FORBIDDEN_MARKER: &'static str = "forbidden-location";

    /// CloudFront/Akamai `403 ERROR` bot 页 —— devices/token 响应命中即 blocked。上游 `errorMarker`。
    pub const ERROR_MARKER: &'static str = "403 ERROR";

    /// disneyplus.com 最终 URL（含跳转链）命中即「未上线 / preview」-> blocked。只扫 URL，body 不参与。
    /// 上游 `previewMarkers`。
    pub const PREVIEW_MARKERS: &'static [&'static str] = &["preview", "unavailable"];

    /// `"assertion":"..."` 提取 devices 返回的 assertion。Polaris checker 内联正则。
    pub fn assertion_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""assertion"\s*:\s*"([^"]+)""#).unwrap());
        &RE
    }

    /// `"refresh_token":"..."` 提取 token 返回的 refresh_token。Polaris checker 内联正则。
    pub fn refresh_token_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""refresh_token"\s*:\s*"([^"]+)""#).unwrap());
        &RE
    }

    /// graphql `"countryCode":"XX"` 提 countryCode（region）。Polaris checker 内联正则。
    pub fn country_code_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""countryCode"\s*:\s*"([A-Za-z]{2})""#).unwrap());
        &RE
    }

    /// graphql `"inSupportedLocation":true|false` 提支持位。Polaris checker 内联正则。
    pub fn in_supported_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""inSupportedLocation"\s*:\s*(true|false)"#).unwrap());
        &RE
    }
}

// ----------------------------------------------------------------------------
// TikTok
// ----------------------------------------------------------------------------

/// TikTok：首页跟随跳转取最终 URL 判可用性 + passport `store_region` 取地区码。
///
/// ## 来源
/// 1-stream/RegionRestrictionCheck `check.sh` 的 `MediaUnlockTest_Tiktok`
/// （<https://github.com/1-stream/RegionRestrictionCheck>，2026-07-16 抓取的 main 分支版本，函数位于 check.sh:3457-3482）。
/// **无 上游 oracle**（上游 只覆盖 6 服务，无 tiktok）——判定法直接对齐该脚本，非移植。
///
/// 该脚本原文（判定骨架）：
/// ```text
/// result  = curl -fsSL -w %{url_effective} "https://www.tiktok.com/"          # 最终落地 URL
/// result1 = curl -fsSL -X POST "https://www.tiktok.com/passport/web/store_region/"
/// region  = jq ".data.store_region"                                            # 形如 "us"
/// curl 失败                                   -> Failed (Network Connection)
/// result 命中 */about | */status* | *landing* -> region==cn ? "Provided by Douyin"(黄) : "No"(红)
/// 否则                                        -> "Yes (Region: XX)"(绿)
/// ```
///
/// ## 为何只此一个来源
/// lmc999/RegionRestrictionCheck 与 nkeonkeo/MediaUnlockTest（本 crate 其余服务的判定法来源）
/// **均未覆盖 TikTok**（2026-07-16 核对：lmc999 check.sh 无 `Tiktok` 函数；MediaUnlockTest 无 tiktok.go）。
/// 故 1-stream fork 为唯一社区来源，漂移风险高于其他服务。
///
/// ## 未验证
/// 端点真实性/marker 时效**未经真机验证**（本机无海外代理出口）。需在真实代理环境回归标定，
/// 采集项见 `~/docs/polaris/design/polaris-unlock-calibration.md` §1。
pub struct TiktokEndpoints;

impl TiktokEndpoints {
    /// 首页：跟随重定向后取最终 URL。可用地区停在 `www.tiktok.com/`（feed）；
    /// 不可用地区被打到 landing/about/status 落地页。
    pub const HOME_URL: &'static str = "https://www.tiktok.com/";

    /// region 端点：**POST（无 body）**，返回 `{"data":{"store_region":"us"},...}`。
    /// 仅供 region 展示，失败不影响 Ok/Blocked 判定（对齐 check.sh 只以首页结果判定）。
    pub const STORE_REGION_URL: &'static str = "https://www.tiktok.com/passport/web/store_region/";

    /// 最终 URL **以此结尾** = 本地区不提供 TikTok（对齐 check.sh `[[ "$result" == *"/about" ]]`）。
    pub const BLOCK_SUFFIX: &'static str = "/about";

    /// 最终 URL **含此子串** = 本地区不提供 TikTok
    /// （对齐 check.sh `[[ "$result" == *"/status"* ]]` / `[[ "$result" == *"landing"* ]]`）。
    pub const BLOCK_MARKERS: &'static [&'static str] = &["/status", "landing"];

    /// 落地页 + `store_region` 命中此值 = 大陆出口被导向抖音
    /// （check.sh 黄色 `Provided by Douyin`，介于 Yes/No 之间 -> 映射 `Partial`）。
    pub const DOUYIN_REGION: &'static str = "CN";

    /// `"store_region":"xx"` 提地区码（对齐 check.sh `jq ".data.store_region"`）。
    /// 用正则而非 serde_json：沿用本文件既有抽取法（Disney/Spotify 同样对 JSON body 走正则），
    /// 且对截断 body（[`MAX_BODY_BYTES`]）更宽容。
    pub fn store_region_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""store_region"\s*:\s*"([^"]+)""#).unwrap());
        &RE
    }
}

// ----------------------------------------------------------------------------
// Spotify
// ----------------------------------------------------------------------------

/// Spotify：signup 端点 GET ?validate=1 判地区可用性。上游 `SPOTIFY`。
///
/// §18：原用 POST + 固定注册 body 已被 Spotify anti-abuse 全局指纹拉黑 -> 即使原生家宽也返 status:320
/// 「检测到代理」-> 全误判 blocked。改 GET ?validate=1（无 body、无需 email），响应仍带 country/is_country_launched。
pub struct SpotifyEndpoints;

impl SpotifyEndpoints {
    pub const SIGNUP_URL: &'static str =
        "https://spclient.wg.spotify.com/signup/public/v1/account?validate=1";

    /// `"status":NNN` 提状态码。Polaris checker 内联正则。
    pub fn status_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""status"\s*:\s*(\d+)"#).unwrap());
        &RE
    }

    /// `"country":"XX"` 提国家码。Polaris checker 内联正则。
    pub fn country_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""country"\s*:\s*"([^"]+)""#).unwrap());
        &RE
    }

    /// `"is_country_launched":true|false` 提开服位。Polaris checker 内联正则。
    pub fn launched_re() -> &'static Regex {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""is_country_launched"\s*:\s*(true|false)"#).unwrap());
        &RE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netflix_region_re_extracts_id() {
        let body = r#"{"id":"HK","foo":"bar","countryName":"Hong Kong"}"#;
        let m = NetflixEndpoints::region_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("HK"));
    }

    #[test]
    fn gemini_region_re_extracts_three_letter() {
        let body = "blah,2,1,200,\"USA\",more";
        let m = GeminiEndpoints::region_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("USA"));
    }

    #[test]
    fn disney_assertion_re_extracts_token() {
        let body = r#"{"assertion":"abc123"}"#;
        let m = DisneyEndpoints::assertion_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("abc123"));
    }

    #[test]
    fn disney_country_code_uppercased_match() {
        let body = r#"{"countryCode":"jp"}"#;
        let m = DisneyEndpoints::country_code_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("jp"));
    }

    #[test]
    fn tiktok_store_region_re_extracts_from_nested_json() {
        // 真实形态：{"data":{"store_region":"us"},"message":"success"}
        let body = r#"{"data":{"store_region":"us"},"message":"success"}"#;
        let m = TiktokEndpoints::store_region_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("us"));
    }

    #[test]
    fn tiktok_store_region_re_none_when_absent() {
        let body = r#"{"message":"error","data":{}}"#;
        assert!(TiktokEndpoints::store_region_re().captures(body).is_none());
    }

    #[test]
    fn spotify_status_re_extracts_number() {
        let body = r#"{"status":1,"country":"US"}"#;
        let m = SpotifyEndpoints::status_re().captures(body);
        assert_eq!(m.and_then(|c| c.get(1)).map(|m| m.as_str()), Some("1"));
    }

    #[test]
    fn token_body_template_contains_placeholder() {
        assert!(
            DisneyEndpoints::TOKEN_BODY_TEMPLATE.contains(DisneyEndpoints::ASSERTION_PLACEHOLDER)
        );
    }

    #[test]
    fn graphql_body_template_contains_placeholder() {
        assert!(DisneyEndpoints::GRAPHQL_BODY_TEMPLATE
            .contains(DisneyEndpoints::REFRESH_TOKEN_PLACEHOLDER));
    }

    #[test]
    fn devices_body_is_valid_json() {
        // 不变量：DEVICES_BODY 必须是合法 JSON（Polaris 经 JSON.stringify 产出）
        let _: serde_json::Value =
            serde_json::from_str(DisneyEndpoints::DEVICES_BODY).expect("DEVICES_BODY must be JSON");
    }
}
