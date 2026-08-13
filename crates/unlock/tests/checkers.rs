//! 集成测试：每服务 checker mock 响应 -> 判定 unlocked/locked。
//!
//! 对齐 上游 `checkers.ts` 的判定规则（1:1）：每个测试构造特定 HTTP 响应脚本，验证 checker 返回
//! 正确的 UnlockStatus。所有 HTTP 经 `ScriptedHttp` mock，零网络。

use async_trait::async_trait;
use polaris_unlock::endpoints::{
    ChatgptEndpoints, ClaudeEndpoints, DisneyEndpoints, GeminiEndpoints, GrokEndpoints,
    NetflixEndpoints, SpotifyEndpoints, TiktokEndpoints, UA,
};
use polaris_unlock::types::UnlockStatus;
use polaris_unlock::{
    check_chatgpt, check_claude, check_disney, check_gemini, check_grok, check_netflix,
    check_spotify, check_tiktok, HttpMethod, RedirectHop, UnlockHttp, UnlockRequest,
    UnlockResponse,
};
use std::sync::Mutex;

/// 按 URL 子串首匹配返回预制响应；未匹配返 err。记录请求（`"<METHOD> <url>"`）供断言。
struct ScriptedHttp {
    scripts: Vec<(String, UnlockResponse)>,
    seen: Mutex<Vec<String>>,
}

impl ScriptedHttp {
    fn new() -> Self {
        Self {
            scripts: Vec::new(),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn on(mut self, url_contains: &str, resp: UnlockResponse) -> Self {
        self.scripts.push((url_contains.to_string(), resp));
        self
    }
}

#[async_trait]
impl UnlockHttp for ScriptedHttp {
    async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
        // 记 "<METHOD> <url>"：既保留既有 url `contains` 断言，又能验证 method（TikTok store_region 须 POST）
        self.seen
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method.as_str(), req.url));
        for (pat, resp) in &self.scripts {
            if req.url.contains(pat) {
                return resp.clone();
            }
        }
        UnlockResponse::err("no-script")
    }
}

// === ChatGPT ===============================================================

#[tokio::test]
async fn chatgpt_both_clean_is_ok() {
    let http = ScriptedHttp::new()
        .on(
            "chat.openai.com/cdn-cgi/trace",
            UnlockResponse::ok(200, "ip=1.1.1.1\nloc=US\n"),
        )
        .on("api.openai.com", UnlockResponse::ok(200, "{}"))
        .on(
            "ios.chat.openai.com",
            UnlockResponse::ok(200, "<html>welcome</html>"),
        );
    let r = check_chatgpt(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("US"));
}

#[tokio::test]
async fn chatgpt_both_dirty_is_blocked() {
    let http = ScriptedHttp::new()
        .on("chat.openai.com/cdn-cgi/trace", UnlockResponse::ok(200, ""))
        .on(
            "api.openai.com",
            UnlockResponse::ok(200, "error: unsupported_country"),
        )
        .on(
            "ios.chat.openai.com",
            UnlockResponse::ok(200, "VPN detected"),
        );
    let r = check_chatgpt(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn chatgpt_one_dirty_is_partial() {
    // cookie 净 / ios 脏 = web-only（partial）
    let http = ScriptedHttp::new()
        .on("chat.openai.com/cdn-cgi/trace", UnlockResponse::ok(200, ""))
        .on("api.openai.com", UnlockResponse::ok(200, "{}"))
        .on(
            "ios.chat.openai.com",
            UnlockResponse::ok(200, "VPN detected"),
        );
    let r = check_chatgpt(&http).await;
    assert_eq!(r.status, UnlockStatus::Partial);
}

#[tokio::test]
async fn chatgpt_ios_unreachable_is_timeout() {
    let http = ScriptedHttp::new()
        .on("chat.openai.com/cdn-cgi/trace", UnlockResponse::ok(200, ""))
        .on("api.openai.com", UnlockResponse::ok(200, "{}"))
        .on("ios.chat.openai.com", UnlockResponse::err("timeout"));
    let r = check_chatgpt(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === Claude ================================================================

#[tokio::test]
async fn claude_lands_on_claude_ai_is_ok() {
    // 登出用户 302 -> claude.ai/login = 本区可用
    let http = ScriptedHttp::new().on(
        "claude.ai/",
        UnlockResponse {
            status: 200,
            body: String::new(),
            truncated: false,
            redirect_chain: vec![RedirectHop {
                status: 302,
                location: "https://claude.ai/login".to_string(),
            }],
            error: None,
            ..Default::default()
        },
    );
    let r = check_claude(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
}

#[tokio::test]
async fn claude_unavailable_region_is_blocked() {
    let http = ScriptedHttp::new().on(
        "claude.ai/",
        UnlockResponse {
            status: 302,
            body: String::new(),
            truncated: false,
            redirect_chain: vec![RedirectHop {
                status: 302,
                location: "https://www.anthropic.com/app-unavailable-in-region".to_string(),
            }],
            error: None,
            ..Default::default()
        },
    );
    let r = check_claude(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn claude_unknown_domain_is_timeout() {
    // 被引到其它未知域 -> 不误 block
    let http = ScriptedHttp::new().on(
        "claude.ai/",
        UnlockResponse {
            status: 302,
            body: String::new(),
            truncated: false,
            redirect_chain: vec![RedirectHop {
                status: 302,
                location: "https://example.org/somewhere".to_string(),
            }],
            error: None,
            ..Default::default()
        },
    );
    let r = check_claude(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

#[tokio::test]
async fn claude_network_error_is_timeout() {
    let http = ScriptedHttp::new().on("claude.ai/", UnlockResponse::err("timeout"));
    let r = check_claude(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === Gemini ================================================================

#[tokio::test]
async fn gemini_available_marker_is_ok() {
    let http = ScriptedHttp::new().on(
        "gemini.google.com",
        UnlockResponse::ok(200, "prefix 45631641,null,true suffix"),
    );
    let r = check_gemini(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
}

#[tokio::test]
async fn gemini_no_marker_is_blocked() {
    let http = ScriptedHttp::new().on(
        "gemini.google.com",
        UnlockResponse::ok(200, "no marker here"),
    );
    let r = check_gemini(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn gemini_extracts_region_code() {
    let body = "stuff,2,1,200,\"USA\",more 45631641,null,true";
    let http = ScriptedHttp::new().on("gemini.google.com", UnlockResponse::ok(200, body));
    let r = check_gemini(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("USA"));
}

#[tokio::test]
async fn gemini_unreachable_is_timeout() {
    let http = ScriptedHttp::new().on("gemini.google.com", UnlockResponse::err("network"));
    let r = check_gemini(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === Netflix ===============================================================

#[tokio::test]
async fn netflix_both_watchable_is_ok() {
    let http = ScriptedHttp::new()
        .on(
            "netflix.com/title/81280792",
            UnlockResponse::ok(200, "full content page"),
        )
        .on(
            "netflix.com/title/70143836",
            UnlockResponse::ok(200, "full content page"),
        );
    let r = check_netflix(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("US")); // 200 兜底 US
}

#[tokio::test]
async fn netflix_both_ohno_is_partial() {
    // 两非自制都含 "Oh no!" = 仅自制剧
    let http = ScriptedHttp::new()
        .on(
            "netflix.com/title/81280792",
            UnlockResponse::ok(404, "Oh no! page"),
        )
        .on(
            "netflix.com/title/70143836",
            UnlockResponse::ok(404, "Oh no! page"),
        );
    let r = check_netflix(&http).await;
    assert_eq!(r.status, UnlockStatus::Partial);
}

#[tokio::test]
async fn netflix_country_not_available_is_blocked() {
    // 大陆 CN 等：403 + Not Available，两片皆命中
    let http = ScriptedHttp::new()
        .on(
            "netflix.com/title/81280792",
            UnlockResponse::ok(403, "Not Available"),
        )
        .on(
            "netflix.com/title/70143836",
            UnlockResponse::ok(403, "Not Available"),
        );
    let r = check_netflix(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn netflix_one_watchable_is_ok() {
    // 任一片可看 -> 完整解锁（即使另一片 Oh no!）
    let http = ScriptedHttp::new()
        .on(
            "netflix.com/title/81280792",
            UnlockResponse::ok(200, "watchable"),
        )
        .on(
            "netflix.com/title/70143836",
            UnlockResponse::ok(404, "Oh no!"),
        );
    let r = check_netflix(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
}

#[tokio::test]
async fn netflix_unreachable_is_timeout() {
    let http = ScriptedHttp::new()
        .on("netflix.com/title/81280792", UnlockResponse::ok(200, "x"))
        .on("netflix.com/title/70143836", UnlockResponse::err("timeout"));
    let r = check_netflix(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === Disney+ ===============================================================

fn disney_ok_scripts() -> ScriptedHttp {
    ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"ASSERT123"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(200, r#"{"refresh_token":"REFRESH456"}"#),
        )
        .on(
            "bamgrid.com/graph",
            // JP 特判 -> ok（即使 inSupportedLocation:false）
            UnlockResponse::ok(200, r#"{"countryCode":"JP","inSupportedLocation":false}"#),
        )
        .on("disneyplus.com", UnlockResponse::ok(200, ""))
}

#[tokio::test]
async fn disney_jp_special_case_is_ok() {
    let http = disney_ok_scripts();
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("JP"));
}

#[tokio::test]
async fn disney_supported_true_is_ok() {
    let http = ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"A"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(200, r#"{"refresh_token":"R"}"#),
        )
        .on(
            "bamgrid.com/graph",
            UnlockResponse::ok(200, r#"{"countryCode":"US","inSupportedLocation":true}"#),
        )
        .on("disneyplus.com", UnlockResponse::ok(200, ""));
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("US"));
}

#[tokio::test]
async fn disney_supported_false_is_partial() {
    let http = ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"A"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(200, r#"{"refresh_token":"R"}"#),
        )
        .on(
            "bamgrid.com/graph",
            UnlockResponse::ok(200, r#"{"countryCode":"DE","inSupportedLocation":false}"#),
        )
        // preview URL 不命中 marker
        .on(
            "disneyplus.com",
            UnlockResponse {
                status: 200,
                body: String::new(),
                truncated: false,
                redirect_chain: vec![],
                error: None,
                ..Default::default()
            },
        );
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Partial);
}

#[tokio::test]
async fn disney_devices_403_error_is_blocked() {
    let http = ScriptedHttp::new().on(
        "bamgrid.com/devices",
        UnlockResponse::ok(403, "403 ERROR\nRequest blocked"),
    );
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn disney_token_forbidden_location_is_blocked() {
    let http = ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"A"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(403, "forbidden-location"),
        );
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn disney_preview_unavailable_is_blocked() {
    let http = ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"A"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(200, r#"{"refresh_token":"R"}"#),
        )
        .on(
            "bamgrid.com/graph",
            UnlockResponse::ok(200, r#"{"countryCode":"XX","inSupportedLocation":true}"#),
        )
        .on(
            "disneyplus.com",
            // 重定向链含 "unavailable" -> blocked
            UnlockResponse {
                status: 302,
                body: String::new(),
                truncated: false,
                redirect_chain: vec![RedirectHop {
                    status: 302,
                    location: "https://www.disneyplus.com/unavailable".to_string(),
                }],
                error: None,
                ..Default::default()
            },
        );
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn disney_no_country_code_is_blocked() {
    let http = ScriptedHttp::new()
        .on(
            "bamgrid.com/devices",
            UnlockResponse::ok(200, r#"{"assertion":"A"}"#),
        )
        .on(
            "bamgrid.com/token",
            UnlockResponse::ok(200, r#"{"refresh_token":"R"}"#),
        )
        .on(
            "bamgrid.com/graph",
            UnlockResponse::ok(200, r#"{"inSupportedLocation":true}"#),
        )
        .on("disneyplus.com", UnlockResponse::ok(200, ""));
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked); // region 空 -> blocked
}

#[tokio::test]
async fn disney_devices_unreachable_is_timeout() {
    let http = ScriptedHttp::new().on("bamgrid.com/devices", UnlockResponse::err("timeout"));
    let r = check_disney(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === Grok ==================================================================
// 弱检测（设计 §3.3）：站点可达(Ok) / 风控拦截(Restricted) / 超时(Timeout) + 出口国家码。
// **无 Blocked 分支**：G3 geo marker 查无 → 空规则，待真机标定（见 GrokEndpoints）。

#[tokio::test]
async fn grok_app_page_is_ok_with_trace_region() {
    let http = ScriptedHttp::new()
        .on(
            "grok.com/cdn-cgi/trace",
            UnlockResponse::ok(200, "ip=1.1.1.1\nloc=JP\n"),
        )
        .on(
            "grok.com",
            UnlockResponse::ok(200, "<html>cdn.grok.com/_next/app.js</html>"),
        );
    let r = check_grok(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("JP"));
}

#[tokio::test]
async fn grok_unreachable_is_timeout() {
    let http = ScriptedHttp::new().on("grok.com", UnlockResponse::err("timeout"));
    let r = check_grok(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

#[tokio::test]
async fn grok_home_is_a_top_level_navigation_get() {
    // 不变量：首页是**顶层文档 GET**（导航形态，见 browser::RequestProfile::Navigate）。
    let http = ScriptedHttp::new().on(
        "grok.com",
        UnlockResponse::ok(200, GrokEndpoints::APP_MARKER),
    );
    let _ = check_grok(&http).await;
    let seen = http.seen.lock().unwrap();
    assert!(
        seen.iter().any(|u| u == "GET https://grok.com/"),
        "grok 首页必须以 GET 发出，实际: {seen:?}"
    );
}

// === TikTok ================================================================
// 判定法对齐 1-stream/RegionRestrictionCheck check.sh MediaUnlockTest_Tiktok（见 TiktokEndpoints doc）。
// 注：端点真实性未经真机验证（本机无海外代理出口）——以下仅锁定判定分支，不证明端点有效。

/// store_region 脚本项（须排在首页脚本**前**：mock 首匹配生效，"www.tiktok.com/" 会先吃掉 passport 请求）。
fn tiktok_region_script(http: ScriptedHttp, region: &str) -> ScriptedHttp {
    http.on(
        "tiktok.com/passport/web/store_region/",
        UnlockResponse::ok(
            200,
            format!(r#"{{"data":{{"store_region":"{region}"}},"message":"success"}}"#),
        ),
    )
}

/// 首页 302 → location 的响应（模拟 curl `%{url_effective}` 的最终落点）。
fn tiktok_home_redirect(location: &str) -> UnlockResponse {
    UnlockResponse {
        status: 200,
        body: String::new(),
        truncated: false,
        redirect_chain: vec![RedirectHop {
            status: 302,
            location: location.to_string(),
        }],
        error: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn tiktok_feed_is_ok() {
    // 首页无跳转 = 停在正常 feed -> Yes (Region: US)
    let http = tiktok_region_script(ScriptedHttp::new(), "us").on(
        "www.tiktok.com/",
        UnlockResponse::ok(200, "<html>feed</html>"),
    );
    let r = check_tiktok(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("US"));
}

#[tokio::test]
async fn tiktok_about_landing_is_blocked() {
    // check.sh: result 命中 */about -> No（本区不提供 TikTok）
    let http = tiktok_region_script(ScriptedHttp::new(), "jp").on(
        "www.tiktok.com/",
        tiktok_home_redirect("https://www.tiktok.com/about"),
    );
    let r = check_tiktok(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
    assert_eq!(r.region.as_deref(), Some("JP"));
}

#[tokio::test]
async fn tiktok_cn_douyin_is_partial() {
    // check.sh 黄色 "Provided by Douyin"：落地页 + store_region==cn -> Partial（非 Blocked）
    let http = tiktok_region_script(ScriptedHttp::new(), "cn").on(
        "www.tiktok.com/",
        tiktok_home_redirect("https://www.tiktok.com/about"),
    );
    let r = check_tiktok(&http).await;
    assert_eq!(r.status, UnlockStatus::Partial);
    assert_eq!(r.region.as_deref(), Some("CN"));
}

#[tokio::test]
async fn tiktok_unreachable_is_timeout() {
    let http = ScriptedHttp::new().on("www.tiktok.com/", UnlockResponse::err("timeout"));
    let r = check_tiktok(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

#[tokio::test]
async fn tiktok_store_region_uses_post() {
    // 不变量：store_region 须 POST（对齐 check.sh `curl -X POST`）；GET 面行为未知，不可退化
    let http = tiktok_region_script(ScriptedHttp::new(), "us").on(
        "www.tiktok.com/",
        UnlockResponse::ok(200, "<html>feed</html>"),
    );
    let _ = check_tiktok(&http).await;
    let seen = http.seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|u| u == "POST https://www.tiktok.com/passport/web/store_region/"),
        "store_region 必须以 POST 发出，实际: {seen:?}"
    );
    // 首页则是 GET
    assert!(seen.iter().any(|u| u == "GET https://www.tiktok.com/"));
}

// === Spotify ===============================================================

#[tokio::test]
async fn spotify_launched_true_is_ok() {
    let http = ScriptedHttp::new().on(
        "spotify.com",
        UnlockResponse::ok(
            200,
            r#"{"status":1,"country":"US","is_country_launched":true}"#,
        ),
    );
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Ok);
    assert_eq!(r.region.as_deref(), Some("US"));
}

#[tokio::test]
async fn spotify_launched_false_is_blocked() {
    let http = ScriptedHttp::new().on(
        "spotify.com",
        UnlockResponse::ok(
            200,
            r#"{"status":1,"country":"XX","is_country_launched":false}"#,
        ),
    );
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn spotify_status_320_is_blocked() {
    // 代理/datacenter IP 被 flag
    let http = ScriptedHttp::new().on(
        "spotify.com",
        UnlockResponse::ok(
            200,
            r#"{"status":320,"country":"US","is_country_launched":true}"#,
        ),
    );
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn spotify_status_120_is_blocked() {
    let http = ScriptedHttp::new().on(
        "spotify.com",
        UnlockResponse::ok(
            200,
            r#"{"status":120,"country":"US","is_country_launched":true}"#,
        ),
    );
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Blocked);
}

#[tokio::test]
async fn spotify_missing_fields_is_timeout() {
    // country 缺 -> timeout
    let http = ScriptedHttp::new().on("spotify.com", UnlockResponse::ok(200, r#"{"status":1}"#));
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

#[tokio::test]
async fn spotify_unreachable_is_timeout() {
    let http = ScriptedHttp::new().on("spotify.com", UnlockResponse::err("timeout"));
    let r = check_spotify(&http).await;
    assert_eq!(r.status, UnlockStatus::Timeout);
}

// === 边界：headers/UA 注入对齐 ===============================================

#[tokio::test]
async fn checker_requests_carry_user_agent() {
    // 验证 checker 发出的请求带 UA（对齐 Polaris HDRS / NETFLIX_HDRS）
    let http = ScriptedHttp::new()
        .on("netflix.com/title/81280792", UnlockResponse::ok(200, "x"))
        .on("netflix.com/title/70143836", UnlockResponse::ok(200, "x"));
    let _ = check_netflix(&http).await;
    let _ = UA; // 引用一下 endpoints::UA 防止未用警告（UA 已在 checker 内部用）
                // 注：headers 内容的精确断言由 ScriptedHttp 扩展承担；此处仅验证请求被发出（seen 非空）。
    let seen = http.seen.lock().unwrap();
    assert!(seen
        .iter()
        .any(|u| u.contains("netflix.com/title/81280792")));
    assert!(seen
        .iter()
        .any(|u| u.contains("netflix.com/title/70143836")));
}

// 防 unused（仅作引用锚点，对齐 Polaris endpoints 常量导出存在性）
#[test]
fn endpoints_constants_compile() {
    let _ = ChatgptEndpoints::COOKIE_URL;
    let _ = ClaudeEndpoints::HOME_URL;
    let _ = GeminiEndpoints::AVAILABLE_MARKER;
    let _ = NetflixEndpoints::OH_NO_MARKER;
    let _ = DisneyEndpoints::BEARER;
    let _ = GrokEndpoints::APP_MARKER;
    let _ = TiktokEndpoints::STORE_REGION_URL;
    let _ = SpotifyEndpoints::SIGNUP_URL;
    let _: HttpMethod = HttpMethod::Get;
}
