//! 解锁 checker —— 纯逻辑，HTTP trait 注入（天然可单测、无网络）。
//!
//! 6 个服务（chatgpt/claude/gemini/netflix/disney/spotify）1:1 移植自 上游 `checkers.ts`；
//! `grok`/`tiktok` 无 上游 oracle —— tiktok 对齐 1-stream/RegionRestrictionCheck（见 [`TiktokEndpoints`]），
//! grok 是本仓设计的**诚实弱检测**（见 [`check_grok`] 与 [`GrokEndpoints`]）。
//! 判定法逐服务对齐 RegionRestrictionCheck 系主脚本 check.sh（状态机：Ok/Partial/Blocked/Timeout）：
//! - 端点 / marker / titleId / apiKey / 正则全在 [`endpoints`](crate::endpoints)，本文件只有判定逻辑（漂移不改这里）。
//! - 网络失败 / 无响应 / 无法判定 -> `Timeout`（统一兜底，不误 Block）。
//!
//! 关键差异（Rust 化）：
//! - Polaris 用 `Promise.all` 并发齐射；Rust 用 [`futures::future::join`]。每 checker 内部并发请求**仍是并发**
//!   （语义对齐，错误隔离：单请求失败只影响本 checker 的 Timeout 兜底，不影响其他请求）。
//! - 异步 checker 永不 panic：所有路径显式返回 `UnlockResult`。

use crate::browser::{browser_headers, RequestProfile};
use crate::challenge::classify;
use crate::endpoints::{
    ChatgptEndpoints, ClaudeEndpoints, DisneyEndpoints, GeminiEndpoints, GrokEndpoints,
    NetflixEndpoints, SpotifyEndpoints, TiktokEndpoints,
};
use crate::http::{UnlockHttp, UnlockRequest, UnlockResponse};
use crate::trace::parse_trace;
use crate::types::{ServiceId, UnlockResult, UnlockStatus};

/// 大小写不敏感子串匹配（对齐 check.sh `grep -i` 的 marker：ChatGPT VPN/unsupported_country、Disney 403 ERROR/forbidden-location）。
/// 上游 `hasCI`。
fn has_ci(body: &str, marker: &str) -> bool {
    body.to_lowercase().contains(&marker.to_lowercase())
}

/// 给请求套上与 [`UA`](crate::endpoints::UA) 自洽的完整浏览器头集（见 [`crate::browser`] 模块文档）。
///
/// **先套浏览器头、后由调用方追加业务头**（`Authorization` / `Content-Type`）——
/// [`UnlockRequest::header`] 后设者覆盖，故业务头永远赢。
fn with_browser_headers(mut req: UnlockRequest, profile: RequestProfile) -> UnlockRequest {
    let url = req.url.clone();
    for (k, v) in browser_headers(profile, req.method, &url) {
        req = req.header(k, v);
    }
    req
}

/// 带完整浏览器头集的 GET 请求。
fn get_req(url: &str, profile: RequestProfile) -> UnlockRequest {
    with_browser_headers(UnlockRequest::get(url), profile)
}

/// 带完整浏览器头集的 POST 请求（Api profile —— 检测里的 POST 全是 bamgrid JSON/form API）。
fn post_req(url: &str) -> UnlockRequest {
    with_browser_headers(UnlockRequest::post(url), RequestProfile::Api)
}

/// 任一响应命中 CF 挑战 / 1020 防火墙拒绝。
///
/// 命中即该 checker 的判据**全部失效**（拿到的是 CF 的墙，不是服务的真实响应）→ 归
/// [`UnlockStatus::Restricted`]，而非拿墙的 body 去跑 marker 匹配得出误 Ok/误 Blocked。
/// 判据本体见 [`crate::challenge::classify`]。
fn any_challenged(responses: &[&UnlockResponse]) -> bool {
    responses.iter().any(|r| classify(r).is_some())
}

/// 打服务自有 trace 取地区码（失败返 None，不影响判定，仅作展示 region）。上游 `traceRegion`。
async fn trace_region<H: UnlockHttp + ?Sized>(http: &H, url: &str) -> Option<String> {
    let r = http.request(&get_req(url, RequestProfile::Api)).await;
    if r.status != 200 || r.body.is_empty() {
        return None;
    }
    parse_trace(&r.body).and_then(|info| info.country_code)
}

// ===========================================================================
// ChatGPT
// ===========================================================================

/// ChatGPT（对齐 check.sh WebTest_OpenAI）：r1=cookie_requirements 查 unsupported_country、r2=ios 首页查 VPN。
///
/// 任一网络失败 -> Timeout；两净 -> Ok；两脏 -> Blocked；一净一脏 -> Partial（r1 净 r2 脏 = web-only / r1 脏 r2 净 = mobile-only）。
/// region 取 trace(loc)（仅展示，可缺）。上游 `checkChatgpt`。
pub async fn check_chatgpt<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    let (trace, cookie, ios) = futures::future::join3(
        http.request(&get_req(ChatgptEndpoints::TRACE_URL, RequestProfile::Api)),
        http.request(&get_req(ChatgptEndpoints::COOKIE_URL, RequestProfile::Api)),
        // ios.chat.openai.com/ 是顶层 HTML 首页（判 body 里的 VPN marker）→ 导航形态。
        http.request(&get_req(
            ChatgptEndpoints::IOS_URL,
            RequestProfile::Navigate,
        )),
    )
    .await;

    // 任一端点不可达 -> timeout
    if !cookie.reachable() || !ios.reachable() {
        return UnlockResult::timeout();
    }
    let region = if trace.status == 200 {
        parse_trace(&trace.body).and_then(|info| info.country_code)
    } else {
        None
    };

    // 挑战识别优先（设计 §2.5 正确性修复）：挑战页 body 无 VPN/unsupported_country marker → 两净 → 会**误 Ok**。
    // 任一判定响应命中 CF 挑战/1020 → Restricted（风控拦截，非地区问题），堵掉误报路径。
    if any_challenged(&[&cookie, &ios]) {
        return UnlockResult::with_region(UnlockStatus::Restricted, region);
    }

    let cookie_dirty = has_ci(&cookie.body, ChatgptEndpoints::COOKIE_BLOCK_MARKER);
    let ios_dirty = has_ci(&ios.body, ChatgptEndpoints::IOS_BLOCK_MARKER);
    match (cookie_dirty, ios_dirty) {
        (false, false) => UnlockResult::with_region(UnlockStatus::Ok, region), // 两净
        (true, true) => UnlockResult::with_region(UnlockStatus::Blocked, region), // 两脏
        // 一净一脏（web-only / mobile-only）
        _ => UnlockResult::with_region(UnlockStatus::Partial, region),
    }
}

// ===========================================================================
// Claude
// ===========================================================================

/// Claude（判定基线 check.sh WebTest_Claude，按真机 net.request 实况放宽到「域」）：跟随 claude.ai/ 跳转取最终 URL。
///
/// 实测（真 Chromium net.request，非 curl）：登出用户 claude.ai/ 会 302 到 claude.ai/login（本区可用的正常行为）；
/// 地区封禁则跳去 www.anthropic.com/app-unavailable-in-region。故：
/// - 无响应 -> Timeout；含 `app-unavailable-in-region` -> Blocked；命中 CF 挑战 -> Restricted；
///   最终落在 **claude.ai 任意路径**（/、/login、/new）-> Ok（本区可用）；被引到其它未知域 -> Timeout（不误 Block）。
///
/// **与 上游的刻意分歧**：上游 原注释把「403 challenge 停 claude.ai/」也算 Ok（因为它顶着 Chromium 指纹
/// 压根不会被出挑战，那条分支实际打不到）。Polaris 若照抄，被挑战时 host 仍 = `claude.ai` → **误报 Ok**
/// （用户看到绿灯，实际检测什么都没测到）。故挑战识别在 host 判定**前**短路 → `Restricted`。
/// 顺序上 `app-unavailable-in-region` 仍在最前：那是源站给出的**明确地区裁决**，CF 挑战页不会产生该 URL。
///
/// region 取 trace（仅展示，可缺）。上游 `checkClaude`。
pub async fn check_claude<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    let (res, region) = futures::future::join(
        http.request(&get_req(
            ClaudeEndpoints::HOME_URL,
            RequestProfile::Navigate,
        )),
        trace_region(http, ClaudeEndpoints::TRACE_URL),
    )
    .await;

    if !res.reachable() {
        return UnlockResult::with_region(UnlockStatus::Timeout, region); // 无响应 ≠ 封禁
    }
    let chain = &res.redirect_chain;
    let last = chain
        .last()
        .map(|h| h.location.as_str())
        .unwrap_or(ClaudeEndpoints::HOME_URL);
    // 去 query / fragment
    let final_url = strip_query_fragment(last);

    if final_url.contains(ClaudeEndpoints::BLOCK_MARKER) {
        return UnlockResult::with_region(UnlockStatus::Blocked, region);
    }
    let host = hostname_of(&final_url);
    match host.as_deref() {
        // 停在 claude.ai 任意路径 → Ok，**命中挑战也算**。
        //
        // 挑战不是地区裁决：Anthropic 对封禁地区给的是**明确的 302 到
        // `www.anthropic.com/app-unavailable-in-region`**（上面一条已拦），而 CF 挑战停在本域说明源站
        // 认这个区、只是在做风控，浏览器过一下挑战就能用。故「挑战 = 本区可用」是真机语义
        // （`polaris-unlock-challenge-and-grok.md` §claude 的标定结论）。
        //
        // 此前这里把挑战短路在 host 判定**之前**判 `Restricted`，与上述标定相反：用户在能用的地区
        // 只要撞上一次风控挑战就看到红灯。挑战识别保留在下面的**未知域**分支——那里它才有信息量。
        Some("claude.ai") => UnlockResult::with_region(UnlockStatus::Ok, region),
        // 被引到其它未知域：命中挑战 → `Restricted`（风控把导航截停在中间域，不是地区问题）；
        // 否则 `Timeout`（说不出所以然，但绝不误判 Blocked）。
        _ if any_challenged(&[&res]) => UnlockResult::with_region(UnlockStatus::Restricted, region),
        _ => UnlockResult::with_region(UnlockStatus::Timeout, region),
    }
}

/// 去掉 URL 的 query（`?...`）和 fragment（`#...`）。上游 `last.split('?')[0].split('#')[0]`。
fn strip_query_fragment(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    no_query.split('#').next().unwrap_or(no_query).to_string()
}

/// 取 URL 的 hostname。上游 `new URL(finalUrl).hostname`。非法 URL 返回 None。
fn hostname_of(url: &str) -> Option<String> {
    // 简化 host 解析：剥离 scheme，取 authority 首段（@ 前 userinfo 剥除），去 port。
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host_no_port = host.split(':').next().unwrap_or(host);
    if host_no_port.is_empty() {
        return None;
    }
    // 合法性：至少含一个 '.' 或为已知裸 host（如 localhost）；这里对齐 checker 用途（只比 == "claude.ai"）
    Some(host_no_port.to_lowercase())
}

// ===========================================================================
// Gemini
// ===========================================================================

/// Gemini（对齐 check.sh WebTest_Gemini）：GET gemini.google.com/ 跟随跳转。网络失败 -> Timeout；
/// 可用性 marker 命中 -> Ok，缺失 -> Blocked。region：`,2,1,200,"XXX"` 提 3 字母码（仅展示，可缺）。上游 `checkGemini`。
pub async fn check_gemini<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    let res = http
        .request(&get_req(
            GeminiEndpoints::HOME_URL,
            RequestProfile::Navigate,
        ))
        .await;
    if !res.reachable() {
        return UnlockResult::timeout();
    }
    let region = GeminiEndpoints::region_re()
        .captures(&res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    // 挑战识别在 marker 判定**前**短路（设计 §2.5 正确性修复）：挑战页缺 AVAILABLE_MARKER → 会**误 Blocked**。
    // 命中 CF 挑战/1020 → Restricted（风控拦截，非地区不可用）。
    if any_challenged(&[&res]) {
        return UnlockResult::with_region(UnlockStatus::Restricted, region);
    }
    if res.body.contains(GeminiEndpoints::AVAILABLE_MARKER) {
        UnlockResult::with_region(UnlockStatus::Ok, region)
    } else {
        UnlockResult::with_region(UnlockStatus::Blocked, region)
    }
}

// ===========================================================================
// Grok（xAI）—— 弱检测（设计 §3）
// ===========================================================================

/// Grok（xAI）弱 checker（设计 §3 诚实边界 + §5 落地顺序第 4 项）。
///
/// 能做 = 站点可达(Ok) / 风控拦截(Restricted) / 超时(Timeout) + 出口国家码。
/// 不可做 = 登录后模型可用性：EU 的 Grok 4.5 限制是模型级、站点仍 200；datacenter IP 的认证错误在认证会话内，
/// 裸 HTTP 均不可见。**故 `Ok` 的语义是「grok.com 前端可达、未被风控拦截」，不是「登录后 Grok 可用」**
/// （前端徽章 hover 附了同义弱化说明，i18n `home.unlockGrokWeakHint`）。
///
/// 判定表（设计 §3.3，按序短路）：
/// - **G1** P1 不可达（`status==0`/error）→ [`UnlockStatus::Timeout`]（无响应 ≠ 封禁）。
/// - **G2** P1 [`classify`] 命中挑战/1020 → [`UnlockStatus::Restricted`]（风控拦截，**非地区问题**：
///   用户行动是换 IP 质量更好的同国出口，不是换国家 —— 与 `Blocked` 正交，色档也不同，见前端 `.ub.restricted`）。
/// - **G3** 显式地区 block marker → `Blocked`。**当前查无此 marker → 空规则**（禁编造，待真机标定 EU/TR/CN/US）。
/// - **G4** 200 + body 含应用页特征 marker（[`GrokEndpoints::APP_MARKER`]，单出口实测）→ `Ok`（**弱语义**，见上）。
/// - **G5** 其余（200 但特征缺失 / 非 CF 的 4xx/5xx）→ `Timeout`（保守兜底，绝不误 Ok/Blocked）。
///
/// region：P2 `loc=`（主，[`trace_region`]）；[`GrokEndpoints::COUNTRY_HEADER`] 头（辅，单出口实测存在，非公开契约）。
///
/// **本批刻意不做**：设计 §9.1 的 G3′（匿名 `POST /rest/models` 模型清单差异法）——它是唯一匿名可判的
/// 地区信号，但哨兵 modelId 必须由 US/EU 双出口真机差集定出（清单还受 A/B 实验影响），设计 §11 明确
/// 「未标定前 G3′ 不上线」。上一条永不命中或会误报的强规则，比诚实的弱检测更糟。
pub async fn check_grok<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    let (res, trace_loc) = futures::future::join(
        // grok.com/ 是顶层 HTML 应用页 → 导航形态。
        http.request(&get_req(GrokEndpoints::HOME_URL, RequestProfile::Navigate)),
        trace_region(http, GrokEndpoints::TRACE_URL),
    )
    .await;

    // region：trace loc 主源；缺失回退 x-country-code 头（辅源，取 2 字母国家码大写化）。
    let region = trace_loc.or_else(|| {
        res.header(GrokEndpoints::COUNTRY_HEADER)
            .map(str::trim)
            .filter(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()))
            .map(|c| c.to_uppercase())
    });

    // G1：不可达 → Timeout（无响应 ≠ 封禁）。
    if !res.reachable() {
        return UnlockResult::with_region(UnlockStatus::Timeout, region);
    }
    // G2：CF 挑战 / 1020 → Restricted（风控拦截，非地区问题）。**必须在 G4 之前**：
    // 挑战页无 APP_MARKER → 会落 G5 的 Timeout，把「被风控挡住」说成「网络超时」→ 行动指引是错的。
    if any_challenged(&[&res]) {
        return UnlockResult::with_region(UnlockStatus::Restricted, region);
    }
    // G3：显式地区 block marker —— **查无 → 空规则**，禁编造（待真机标定，见 GrokEndpoints）。
    // G4：200 + 应用页特征 → Ok（弱语义：仅站点可达，非登录后模型可用）。
    if res.status == 200 && res.body.contains(GrokEndpoints::APP_MARKER) {
        return UnlockResult::with_region(UnlockStatus::Ok, region);
    }
    // G5：保守兜底 Timeout。
    UnlockResult::with_region(UnlockStatus::Timeout, region)
}

// ===========================================================================
// Netflix
// ===========================================================================

/// 已知非地区 2 字母段（如 `en` 语言码 —— help.netflix.com 会裸跳 `/en`，那是语言不是国家，误判会污染 region）。
/// 上游 `NETFLIX_NON_REGION_SEGMENTS`。
const NETFLIX_NON_REGION_SEGMENTS: &[&str] = &["en"];

/// 从 Netflix 本地化路径首段取 region（无前缀 = US）。兼容 `/{region}-{lang}/`（如 `/hk-en/`）与 bare `/{region}/`（如 `/jp/`）。
/// 上游 `netflixRegion`。
fn netflix_region_from_chain(res: &crate::http::UnlockResponse) -> Option<String> {
    for hop in &res.redirect_chain {
        let after_scheme = hop.location.split("://").nth(1).unwrap_or(&hop.location);
        let path = after_scheme
            .find('/')
            .map(|i| &after_scheme[i..])
            .unwrap_or("");
        // **`continue` 而非 `?`**：`?` 会从**整个函数**提前返回 None，即「第一跳不像地区段就放弃后面所有跳」。
        // 对齐 上游 `netflixRegion`（`checkers.ts:102-110`）—— 那里是 try/catch + 循环续跑，逐跳找到为止。
        // 真实链常是 `netflix.com/` → `netflix.com/hk-en/title/...`：首跳无路径段，`?` 直接吞掉后面那跳的 HK。
        let Some(first_seg) = path.split('/').find(|s| !s.is_empty()) else {
            continue;
        };
        // 匹配 `xx` 或 `xx-yyy`
        let Some(m) = parse_region_seg(first_seg) else {
            continue;
        };
        if !NETFLIX_NON_REGION_SEGMENTS.contains(&m.as_str()) {
            return Some(m.to_uppercase());
        }
    }
    None
}

/// 解析 `xx` 或 `xx-yyyy` 形式的地区段，返回 2 字母小写国家码。对齐 上游 `/^([a-z]{2})(-[a-z]{2,4})?$/`。
fn parse_region_seg(seg: &str) -> Option<String> {
    // 分出首段 2 字母 + 可选 `-xxxx`
    let bytes = seg.as_bytes();
    if bytes.len() < 2 || !bytes[..2].iter().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let head = std::str::from_utf8(&bytes[..2]).ok()?.to_string();
    let rest = &seg[2..];
    if rest.is_empty() {
        return Some(head);
    }
    // 剩余必须是 `-` + 2~4 小写字母
    let tail = rest.strip_prefix('-')?;
    if !(2..=4).contains(&tail.len()) {
        return None;
    }
    if !tail.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    Some(head)
}

/// Netflix（对齐 check.sh MediaUnlockTest_Netflix）：两非自制 title（81280792 + 70143836），UA + accept-language。
///
/// 任一网络失败 -> Timeout；两个 body 都含 `Oh no!`（非自制均不可看） -> Partial（仅自制剧）；任一不含（可看） -> Ok。
/// region（仅展示）：body countryName 前的 "id" 值，缺则重定向本地化前缀兜底（无 = US）。
/// **国家级不可用**（大陆 CN 等）：两非自制皆命中 403/Not Available -> Blocked（堵「可达 + 无 Oh no! 即判 ok」谬误）。
/// 上游 `checkNetflix`。
pub async fn check_netflix<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    // 原 `netflix_headers()`（UA + accept-language）已被浏览器头集覆盖：`Accept-Language: en-US,en;q=0.9`
    // 现是全局默认（见 `browser::ACCEPT_LANGUAGE`），故此处不再需要特调头。
    let (r1, r2) = futures::future::join(
        http.request(&get_req(
            NetflixEndpoints::NON_ORIGINAL_URL,
            RequestProfile::Navigate,
        )),
        http.request(&get_req(
            NetflixEndpoints::NON_ORIGINAL_URL_2,
            RequestProfile::Navigate,
        )),
    )
    .await;

    if !r1.reachable() || !r2.reachable() {
        return UnlockResult::timeout(); // 任一不可达
    }
    // 挑战识别在 not_available 判定**前**短路：CF 挑战页是 **403**，会直接落进下面的
    // `r.status == 403` 分支 → **误 Blocked**（把风控拦截报成「Netflix 未进本国」）。
    if any_challenged(&[&r1, &r2]) {
        return UnlockResult::new(UnlockStatus::Restricted);
    }
    // Netflix 未进本国（CN 等）：403 + "Not Available"。两非自制皆命中 = 国家级不可用。
    let not_available = |r: &crate::http::UnlockResponse| {
        r.status == 403 || has_ci(&r.body, NetflixEndpoints::NOT_AVAILABLE_MARKER)
    };
    if not_available(&r1) && not_available(&r2) {
        return UnlockResult::new(UnlockStatus::Blocked);
    }
    // region：regionRe 优先 -> 重定向链 -> 200 兜底 US
    let region = NetflixEndpoints::region_re()
        .captures(&r1.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .or_else(|| netflix_region_from_chain(&r1))
        .or_else(|| (r1.status == 200).then_some("US".to_string()));
    let d1 = r1.body.contains(NetflixEndpoints::OH_NO_MARKER);
    let d2 = r2.body.contains(NetflixEndpoints::OH_NO_MARKER);
    if d1 && d2 {
        return UnlockResult::with_region(UnlockStatus::Partial, region); // 两非自制皆不可看 -> 仅自制剧
    }
    UnlockResult::with_region(UnlockStatus::Ok, region) // 任一可看 -> 完整解锁
}

// ===========================================================================
// Disney+
// ===========================================================================

/// Disney+（对齐 check.sh MediaUnlockTest_DisneyPlus）：bamgrid devices -> token -> graphql，
/// 末段打 disneyplus.com 判 preview。
///
/// devices 网络失败 -> Timeout；devices/token body `403 ERROR` -> Blocked；token `forbidden-location` -> Blocked；
/// 无 assertion / refresh_token -> Timeout；graphql 取 countryCode + inSupportedLocation。映射（对齐 check.sh 顺序）：
/// region 空 -> Blocked；JP -> Ok；preview/unavailable -> Blocked；inSupportedLocation:false -> Partial；
/// :true -> Ok；其余 -> Timeout。上游 `checkDisney`。
pub async fn check_disney<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    // ① devices -> assertion
    let dev_req = post_req(DisneyEndpoints::DEVICES_URL)
        .header(
            "Authorization",
            format!("Bearer {}", DisneyEndpoints::BEARER),
        )
        .header("Content-Type", "application/json; charset=UTF-8")
        .body(DisneyEndpoints::DEVICES_BODY);
    let dev_res = http.request(&dev_req).await;
    if !dev_res.reachable() {
        return UnlockResult::timeout();
    }
    // 挑战识别在 ERROR_MARKER 判定**前**短路：CF 挑战页无 `403 ERROR` 串但状态是 403，
    // 后续取不到 assertion → 会落 Timeout（不算误 Block，但把「被风控挡住」说成「网络超时」，
    // 用户拿到的行动指引是错的）。归 Restricted 才诚实。
    if any_challenged(&[&dev_res]) {
        return UnlockResult::new(UnlockStatus::Restricted);
    }
    if has_ci(&dev_res.body, DisneyEndpoints::ERROR_MARKER) {
        return UnlockResult::new(UnlockStatus::Blocked); // 403 ERROR = IP 被 Disney 封
    }
    let assertion = match DisneyEndpoints::assertion_re()
        .captures(&dev_res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    {
        Some(a) => a,
        None => return UnlockResult::timeout(),
    };

    // ② token（grant body 模板注入 assertion）-> refresh_token
    let token_body = DisneyEndpoints::TOKEN_BODY_TEMPLATE.replacen(
        DisneyEndpoints::ASSERTION_PLACEHOLDER,
        &assertion,
        1,
    );
    let tok_req = post_req(DisneyEndpoints::TOKEN_URL)
        .header(
            "Authorization",
            format!("Bearer {}", DisneyEndpoints::BEARER),
        )
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(token_body);
    let tok_res = http.request(&tok_req).await;
    // 同 ①：挑战识别先于 forbidden-location / 403 ERROR 判定。
    if any_challenged(&[&tok_res]) {
        return UnlockResult::new(UnlockStatus::Restricted);
    }
    if has_ci(&tok_res.body, DisneyEndpoints::FORBIDDEN_MARKER)
        || has_ci(&tok_res.body, DisneyEndpoints::ERROR_MARKER)
    {
        return UnlockResult::new(UnlockStatus::Blocked);
    }
    let refresh_token = match DisneyEndpoints::refresh_token_re()
        .captures(&tok_res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    {
        Some(rt) => rt,
        // 无 refresh_token（含 token 不可达的情况，上 forbidden/error 已兜底）
        None => return UnlockResult::timeout(),
    };

    // ③ graphql（refreshToken mutation 注入 refreshToken；authorization = 裸 token 无 Bearer 前缀）
    let graphql_body = DisneyEndpoints::GRAPHQL_BODY_TEMPLATE.replacen(
        DisneyEndpoints::REFRESH_TOKEN_PLACEHOLDER,
        &refresh_token,
        1,
    );
    let g_req = post_req(DisneyEndpoints::GRAPHQL_URL)
        // 裸 token 无 Bearer 前缀（对齐 上游 `Authorization: DISNEY.bearer`）
        .header("Authorization", DisneyEndpoints::BEARER)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(graphql_body);
    let g_res = http.request(&g_req).await;
    // graphql 传输失败必落 timeout（防 body='' -> region=None -> 误判 blocked）
    if !g_res.reachable() {
        return UnlockResult::timeout();
    }
    // 挑战识别在 region 提取**前**短路：挑战页无 countryCode → region=None → 下方 ④ 直接
    // **误 Blocked**（与「传输失败」那条防线堵的是同一个谬误，只是触发源不同）。
    if any_challenged(&[&g_res]) {
        return UnlockResult::new(UnlockStatus::Restricted);
    }
    let region = DisneyEndpoints::country_code_re()
        .captures(&g_res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_uppercase()));
    let in_supported = DisneyEndpoints::in_supported_re()
        .captures(&g_res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));

    // ④ 映射（对齐 check.sh 顺序）。region 空 -> Blocked；JP 特判 -> Ok。
    let region = match region {
        None => return UnlockResult::new(UnlockStatus::Blocked),
        Some(r) => r,
    };
    if region == "JP" {
        return UnlockResult::with_region(UnlockStatus::Ok, Some(region));
    }

    // ⑤ disneyplus.com 最终 URL preview/unavailable -> Blocked（只扫 URL，body 不参与）。
    let pv = http
        .request(&get_req(
            DisneyEndpoints::PREVIEW_URL,
            RequestProfile::Navigate,
        ))
        .await;
    // ⑤ 的两道前置守卫（本仓补，上游 `checkers.ts:200-204` 也缺）：
    // 本腿靠**重定向链是否含 preview/unavailable**判 Blocked。传输失败 / CF 挑战时链是空的或是挑战页的链，
    // 「没命中 marker」于是被静默读成「通过了 ⑤」→ 落到 ⑥ 按 inSupportedLocation 出 Ok/Partial。
    // 那是拿「没测成」冒充「测过且没问题」。与本文件既有的两道防线（graphql 腿的 `reachable()` / `any_challenged`）
    // 同一条原则：判不了就如实说判不了，绝不把缺证据当成阴性证据。
    if !pv.reachable() {
        return UnlockResult::with_region(UnlockStatus::Timeout, Some(region));
    }
    if any_challenged(&[&pv]) {
        return UnlockResult::with_region(UnlockStatus::Restricted, Some(region));
    }
    let mut final_url = DisneyEndpoints::PREVIEW_URL.to_lowercase();
    for hop in &pv.redirect_chain {
        final_url.push(' ');
        final_url.push_str(&hop.location.to_lowercase());
    }
    if DisneyEndpoints::PREVIEW_MARKERS
        .iter()
        .any(|m| final_url.contains(m))
    {
        return UnlockResult::with_region(UnlockStatus::Blocked, Some(region));
    }

    // ⑥ inSupportedLocation 显式信号；缺失 -> 无法判定 -> Timeout（绝不误 Block）。
    match in_supported.as_deref() {
        Some("false") => UnlockResult::with_region(UnlockStatus::Partial, Some(region)), // Disney 明确「即将上线」
        Some("true") => UnlockResult::with_region(UnlockStatus::Ok, Some(region)),
        _ => UnlockResult::with_region(UnlockStatus::Timeout, Some(region)),
    }
}

// ===========================================================================
// TikTok
// ===========================================================================

/// TikTok（对齐 1-stream/RegionRestrictionCheck `check.sh` `MediaUnlockTest_Tiktok`，判据来源见
/// [`TiktokEndpoints`]）：首页跟随跳转取最终 URL 判可用性，passport `store_region` 取地区码。
///
/// - 首页不可达 -> `Timeout`（无响应 ≠ 封禁）。
/// - 首页命中 CF 挑战 / 1020 -> `Restricted`（见下「本仓补的一道守卫」）。
/// - 最终 URL 命中 `/about` 结尾 / 含 `/status` / 含 `landing`（= 被打到「本地区不提供 TikTok」落地页）：
///   - `store_region` == `CN` -> `Partial`（大陆出口被导向抖音；check.sh 黄色 `Provided by Douyin`）；
///   - 否则 -> `Blocked`。
/// - 未命中（停在正常 feed）-> `Ok`。
///
/// region 取 `store_region`（大写，仅展示、可缺）。**store_region 请求失败不致 Timeout**：
/// 对齐 check.sh 仅以首页结果判定，region 缺失只是少个展示值，不改 Ok/Blocked。
///
/// **本仓补的一道守卫（check.sh 没有，设计 §2.5 的通用原则）**：本 checker 的「无落地页 marker」是
/// **兜底判 `Ok`** 的路径 —— 挑战页不重定向，最终 URL 仍是首页 → 直接**误 Ok**（谎报绿灯，且什么都没测到）。
/// 故挑战识别在分类**前**短路。与 chatgpt/gemini 的接入同因。
pub async fn check_tiktok<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    let (home, region_res) = futures::future::join(
        // 顶层 HTML 首页（判最终落地 URL）→ 导航形态；store_region 是同源 POST API → Api 形态。
        http.request(&get_req(
            TiktokEndpoints::HOME_URL,
            RequestProfile::Navigate,
        )),
        http.request(&post_req(TiktokEndpoints::STORE_REGION_URL)),
    )
    .await;

    if !home.reachable() {
        return UnlockResult::timeout(); // 首页不可达 ≠ 封禁
    }
    // region 仅展示：store_region 不可达 / 无字段 -> None，不参与 Ok/Blocked 判定。
    // 对齐 check.sh `region=$(jq ".data.store_region")`，正则从 body 提 store_region 值并大写化。
    let region = TiktokEndpoints::store_region_re()
        .captures(&region_res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_uppercase()));

    // 挑战识别在分类**前**短路（见函数文档）：挑战页不重定向 → 最终 URL == 首页 → 会误 Ok。
    if any_challenged(&[&home]) {
        return UnlockResult::with_region(UnlockStatus::Restricted, region);
    }

    // 最终 URL（无重定向则为请求 URL 本身，对齐 curl `%{url_effective}`）→ 纯分类器判定。
    let last = home
        .redirect_chain
        .last()
        .map(|h| h.location.as_str())
        .unwrap_or(TiktokEndpoints::HOME_URL);
    classify_tiktok(last, region)
}

/// TikTok 纯分类器（**无 HTTP**，天然可单测）：给定首页跟随重定向后的**最终 URL** +
/// `store_region` 地区码（已大写化，仅展示），映射到 [`UnlockStatus`]。
///
/// 对齐 1-stream/RegionRestrictionCheck `check.sh` `MediaUnlockTest_Tiktok`（check.sh:3457-3482）的
/// `%{url_effective}` 判定表：
/// - 最终 URL 命中 `*/about`（结尾）/ `*/status*` / `*landing*`（落地页 = 本区不提供 TikTok）：
///   - `region == "CN"` → [`UnlockStatus::Partial`]（大陆出口被导向抖音；check.sh 黄色 `Provided by Douyin`）；
///   - 否则 → [`UnlockStatus::Blocked`]（check.sh 红色 `No`）。
/// - 未命中（停在正常 feed）→ [`UnlockStatus::Ok`]（check.sh 绿色 `Yes`）。
///
/// **与 check.sh 的一处刻意差异（硬化，非移植 bug）**：check.sh 对含 query 的 raw `url_effective` 直接
/// 匹配 `*"/about"`（bash 后缀匹配），故 `/about?lang=en` 会漏判为 Yes；此处先 [`strip_query_fragment`] +
/// lowercase 归一化再匹配，`/about?lang=en` 正确判 Blocked。
pub fn classify_tiktok(final_url: &str, region: Option<String>) -> UnlockResult {
    let url = strip_query_fragment(final_url).to_lowercase();
    let landed_on_block = url.ends_with(TiktokEndpoints::BLOCK_SUFFIX)
        || TiktokEndpoints::BLOCK_MARKERS
            .iter()
            .any(|m| url.contains(m));
    if !landed_on_block {
        return UnlockResult::with_region(UnlockStatus::Ok, region); // 停在正常 feed
    }
    // 落地页 = 本区不提供 TikTok；CN 出口特判为被导向抖音（部分可用）。
    if region.as_deref() == Some(TiktokEndpoints::DOUYIN_REGION) {
        return UnlockResult::with_region(UnlockStatus::Partial, region);
    }
    UnlockResult::with_region(UnlockStatus::Blocked, region)
}

// ===========================================================================
// Spotify
// ===========================================================================

/// Spotify（§18）：GET ?validate=1 signup 端点，解析 status/country/is_country_launched。
///
/// 网络失败 / 无 status -> Timeout；`status` 320/120 -> Blocked（真·代理/datacenter flag，GET 面实测不误触）；
/// country / is_country_launched 缺 -> Timeout；is_country_launched:false -> Blocked（地区未开服）；
/// is_country_launched:true + country -> Ok。上游 `checkSpotify`。
pub async fn check_spotify<H: UnlockHttp + ?Sized>(http: &H) -> UnlockResult {
    // 原 `Accept-Language: en` 由浏览器头集的 `en-US,en;q=0.9` 取代（同为英文，且与 UA 自洽）。
    // method 默认即 GET（对齐 上游 `method: 'GET'`）
    let req = get_req(SpotifyEndpoints::SIGNUP_URL, RequestProfile::Api);
    let res = http.request(&req).await;
    if !res.reachable() {
        return UnlockResult::timeout();
    }
    // 挑战识别先于 status 解析：挑战页无 `"status":NNN` → 落 Timeout，把「被风控挡住」
    // 说成「网络超时」→ 用户拿到错误的行动指引（该换出口，而不是重试）。
    if any_challenged(&[&res]) {
        return UnlockResult::new(UnlockStatus::Restricted);
    }
    let status_code = SpotifyEndpoints::status_re()
        .captures(&res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let status_code = match status_code {
        Some(s) => s,
        None => return UnlockResult::timeout(),
    };
    if status_code == "320" || status_code == "120" {
        return UnlockResult::new(UnlockStatus::Blocked); // 代理/datacenter IP 被 flag
    }
    let region = SpotifyEndpoints::country_re()
        .captures(&res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let launched = SpotifyEndpoints::launched_re()
        .captures(&res.body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let (region, launched) = match (region, launched) {
        (Some(r), Some(l)) => (r, l),
        _ => return UnlockResult::timeout(),
    };
    if launched == "false" {
        return UnlockResult::with_region(UnlockStatus::Blocked, Some(region));
    }
    UnlockResult::with_region(UnlockStatus::Ok, Some(region)) // is_country_launched:true + country -> 已开服
}

// ===========================================================================
// 调度表
// ===========================================================================

/// 按 ServiceId 调度到对应 checker。上游 `CHECKERS: Record<ServiceId, Checker>`。
///
/// 返回 `UnlockResult`（永不 panic；checker 内部异常由 Rust 类型系统保证不发生）。
pub async fn run_checker<H: UnlockHttp + ?Sized>(id: ServiceId, http: &H) -> UnlockResult {
    match id {
        ServiceId::Chatgpt => check_chatgpt(http).await,
        ServiceId::Claude => check_claude(http).await,
        ServiceId::Gemini => check_gemini(http).await,
        ServiceId::Grok => check_grok(http).await,
        ServiceId::Netflix => check_netflix(http).await,
        ServiceId::Disney => check_disney(http).await,
        ServiceId::Tiktok => check_tiktok(http).await,
        ServiceId::Spotify => check_spotify(http).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpMethod, UnlockResponse};

    /// 测试 mock：按 URL 子串匹配返回预制响应（**首个匹配生效** -> 特化脚本须排前）；未匹配 -> 传输失败。
    struct MockHttp(Vec<(&'static str, UnlockResponse)>);

    #[async_trait::async_trait]
    impl UnlockHttp for MockHttp {
        async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
            for (pat, resp) in &self.0 {
                if req.url.contains(pat) {
                    return resp.clone();
                }
            }
            UnlockResponse::err("no-script")
        }
    }

    #[test]
    fn parse_region_seg_plain() {
        assert_eq!(parse_region_seg("hk"), Some("hk".to_string()));
        assert_eq!(parse_region_seg("jp"), Some("jp".to_string()));
    }

    #[test]
    fn parse_region_seg_with_lang() {
        assert_eq!(parse_region_seg("hk-en"), Some("hk".to_string()));
        assert_eq!(parse_region_seg("jp-ja"), Some("jp".to_string()));
    }

    #[test]
    fn parse_region_seg_rejects_uppercase_and_bad() {
        // Polaris 正则 `/^([a-z]{2})(-[a-z]{2,4})?$/`：头部 2 小写字母，tail 2-4 小写字母
        assert_eq!(parse_region_seg("HK"), None); // 大写不算（路径段已 lowercase 化）
        assert_eq!(parse_region_seg("abc"), None); // 3 字母头部
        assert_eq!(parse_region_seg("us-abcde"), None); // tail 5 字母（超 4）
        assert_eq!(parse_region_seg("us-a"), None); // tail 1 字母（不足 2）
        assert_eq!(parse_region_seg("us-ABC"), None); // tail 大写
                                                      // us-abc：tail 3 字母合法 -> 接受（对齐 Polaris 正则）
        assert_eq!(parse_region_seg("us-abc"), Some("us".to_string()));
        assert_eq!(parse_region_seg("en"), Some("en".to_string())); // 合法但被外层 NON_REGION 过滤
    }

    #[test]
    fn hostname_strips_port_userinfo() {
        assert_eq!(
            hostname_of("https://claude.ai/login"),
            Some("claude.ai".to_string())
        );
        assert_eq!(
            hostname_of("https://claude.ai:443/"),
            Some("claude.ai".to_string())
        );
        assert_eq!(
            hostname_of("https://user:pass@www.anthropic.com/x"),
            Some("www.anthropic.com".to_string())
        );
    }

    #[test]
    fn strip_query_fragment_works() {
        assert_eq!(
            strip_query_fragment("https://x.com/a?b=1"),
            "https://x.com/a"
        );
        assert_eq!(
            strip_query_fragment("https://x.com/a#frag"),
            "https://x.com/a"
        );
        assert_eq!(
            strip_query_fragment("https://x.com/a?b=1#c"),
            "https://x.com/a"
        );
    }

    #[test]
    fn netflix_region_from_chain_picks_locale() {
        use crate::http::{RedirectHop, UnlockResponse};
        let res = UnlockResponse {
            status: 200,
            body: String::new(),
            truncated: false,
            error: None,
            redirect_chain: vec![RedirectHop {
                status: 302,
                location: "https://www.netflix.com/hk-en/title/81280792".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(netflix_region_from_chain(&res), Some("HK".to_string()));
    }

    // ── 挑战识别接入既有 checker（设计 §2.5 正确性修复）────────────────────────────
    // 与 challenge.rs 的分类器单测互补：这里验「命中挑战 → checker 返 Restricted 而非误 Ok/Blocked」。

    /// CF 挑战响应（403 + `cf-mitigated: challenge`，reachable）。
    fn challenge_resp() -> UnlockResponse {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("cf-mitigated".to_string(), "challenge".to_string());
        headers.insert("server".to_string(), "cloudflare".to_string());
        UnlockResponse {
            status: 403,
            body: "<html><head><title>Just a moment...</title></head></html>".to_string(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: None,
            headers,
        }
    }

    #[tokio::test]
    async fn chatgpt_challenge_yields_restricted_not_ok() {
        // 现状 bug：挑战页无 VPN/unsupported_country marker → 两净 → 误 Ok。接入后 → Restricted。
        let http = MockHttp(vec![
            (
                "chat.openai.com/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=US\n"),
            ),
            ("api.openai.com", challenge_resp()), // cookie 端点被挑战拦
            (
                "ios.chat.openai.com",
                UnlockResponse::ok(200, "<html>welcome</html>"),
            ),
        ]);
        let r = check_chatgpt(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
        assert_eq!(r.region.as_deref(), Some("US")); // region 仍从 trace 取
    }

    #[tokio::test]
    async fn gemini_challenge_yields_restricted_not_blocked() {
        // 现状 bug：挑战页缺 AVAILABLE_MARKER → 误 Blocked。接入后（marker 判定前短路）→ Restricted。
        let http = MockHttp(vec![("gemini.google.com", challenge_resp())]);
        let r = check_gemini(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    /// CF **1020 防火墙拒绝**响应（403 + `server: cloudflare` + body 1020 marker，**无** cf-mitigated）。
    /// 走 `challenge.rs` 的辅判据门 —— 与 `challenge_resp()` 是两条不同的命中路径，须各自覆盖。
    fn firewall_1020_resp() -> UnlockResponse {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("server".to_string(), "cloudflare".to_string());
        UnlockResponse {
            status: 403,
            body: r#"<span class="cf-error-code">1020</span> Ray ID: abc"#.to_string(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: None,
            headers,
        }
    }

    // ── 新增覆盖：claude / netflix / spotify / disney 的挑战误报路径 ──────────────
    // 每条都对应一个**具体的**误报形态（注释写明「不接挑战识别会误报成什么」）。
    // 变异验证：删掉对应 checker 里的 `any_challenged` 短路 → 该测试转红（断言的正是那个误报值）。

    #[tokio::test]
    async fn claude_challenge_on_own_domain_is_ok() {
        // 真机语义（2026-07-28 用户实测背书）：**命中挑战 = 服务在本区可访问**，浏览器过一下挑战就能用。
        // Anthropic 对封禁地区给的是明确的 302 → `www.anthropic.com/app-unavailable-in-region`（另一条门守）。
        //
        // 守的缺陷：此前 `any_challenged` 短路在 host 判定之前 → 能用的地区只要撞上一次风控挑战就报
        // `Restricted`（红灯）。变异：把 `any_challenged` 挪回 host 判定之前 → 本条转红。
        let http = MockHttp(vec![
            (
                "claude.ai/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=JP\n"),
            ),
            ("claude.ai", challenge_resp()),
        ]);
        let r = check_claude(&http).await;
        assert_eq!(
            r.status,
            UnlockStatus::Ok,
            "挑战停在 claude.ai 本域 = 本区可用，判 Restricted 是把风控当成地区封禁"
        );
        assert_eq!(
            r.region.as_deref(),
            Some("JP"),
            "region 仍从 trace 取，不受挑战影响"
        );
    }

    #[tokio::test]
    async fn claude_challenge_on_unknown_domain_is_restricted_not_timeout() {
        // 挑战识别在**未知域**分支才有信息量：导航被风控截停在中间域，说得出「是风控」就别报
        // 说不清的 `Timeout`。变异：删未知域分支的 `any_challenged` → 本条转红（回落 Timeout）。
        let mut resp = challenge_resp();
        resp.redirect_chain = vec![RedirectHopForTest::hop(
            302,
            "https://challenges.cloudflare.com/hold",
        )];
        let http = MockHttp(vec![("claude.ai", resp)]);
        let r = check_claude(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    #[tokio::test]
    async fn claude_geo_block_still_wins_over_challenge() {
        // 反向门：`app-unavailable-in-region` 是源站的**明确地区裁决**，必须仍判 Blocked，
        // 不得被挑战识别抢走。若把 any_challenged 挪到 BLOCK_MARKER 之前 → 本测试转红。
        let mut resp = challenge_resp();
        resp.redirect_chain = vec![RedirectHopForTest::hop(
            302,
            "https://www.anthropic.com/app-unavailable-in-region",
        )];
        let http = MockHttp(vec![("claude.ai", resp)]);
        let r = check_claude(&http).await;
        assert_eq!(r.status, UnlockStatus::Blocked);
    }

    #[tokio::test]
    async fn netflix_challenge_yields_restricted_not_blocked() {
        // 误报形态：挑战页是 403 → 命中 `not_available`（`r.status == 403`）→ 误 **Blocked**
        //（把风控拦截报成「Netflix 未进本国」，用户以为换国家，实际该换 IP 质量）。
        let http = MockHttp(vec![("www.netflix.com/title", challenge_resp())]);
        let r = check_netflix(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    #[tokio::test]
    async fn netflix_firewall_1020_yields_restricted_not_blocked() {
        // 同上，但走 1020 辅判据门（无 cf-mitigated 头）——证明两条命中路径都接住了。
        let http = MockHttp(vec![("www.netflix.com/title", firewall_1020_resp())]);
        let r = check_netflix(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    #[tokio::test]
    async fn netflix_genuine_403_not_available_still_blocked() {
        // 反向门：真·国家级不可用（403 + "Not Available"，**非** CF）必须仍判 Blocked。
        // 若挑战识别放宽到「凡 403 即 Restricted」→ 本测试转红（真 Blocked 被吞成 Restricted）。
        let mut r403 = UnlockResponse::ok(403, "Not Available in your country");
        r403.headers
            .insert("server".to_string(), "nginx".to_string());
        let http = MockHttp(vec![("www.netflix.com/title", r403)]);
        let r = check_netflix(&http).await;
        assert_eq!(r.status, UnlockStatus::Blocked);
    }

    #[tokio::test]
    async fn spotify_challenge_yields_restricted_not_timeout() {
        // 误报形态：挑战页无 `"status":NNN` → 误 **Timeout**（说「网络超时」，
        // 用户去重试，实际该换出口）。
        let http = MockHttp(vec![("spclient.wg.spotify.com", challenge_resp())]);
        let r = check_spotify(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    #[tokio::test]
    async fn disney_devices_challenge_yields_restricted_not_timeout() {
        // 误报形态：挑战页无 assertion → 误 **Timeout**。
        let http = MockHttp(vec![("bamgrid.com/devices", challenge_resp())]);
        let r = check_disney(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    #[tokio::test]
    async fn disney_graphql_challenge_yields_restricted_not_blocked() {
        // 误报形态：graphql 挑战页无 countryCode → region=None → 误 **Blocked**。
        // MockHttp 首个匹配生效 → 特化的 /graph 必须排在泛化 bamgrid 之前。
        let http = MockHttp(vec![
            ("bamgrid.com/graph", challenge_resp()),
            (
                "bamgrid.com/devices",
                UnlockResponse::ok(200, r#"{"assertion":"A1"}"#),
            ),
            (
                "bamgrid.com/token",
                UnlockResponse::ok(200, r#"{"refresh_token":"R1"}"#),
            ),
        ]);
        let r = check_disney(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
    }

    /// **Disney ⑤ preview 腿的传输守卫**：preview 请求打不通时，「重定向链里没有 preview/unavailable」
    /// 不构成「不 Blocked」的证据 —— 那是缺证据，不是阴性证据。必须落 Timeout，不得靠 ⑥ 的
    /// `inSupportedLocation:true` 顺势出 Ok。
    ///
    /// **变异锁**：删掉 `if !pv.reachable()` 那道守卫 → final_url 只剩 PREVIEW_URL（不含 marker）→
    /// 落到 ⑥ 按 `inSupportedLocation:true` 出 **Ok** → 本测转红。
    #[tokio::test]
    async fn disney_preview_unreachable_yields_timeout_not_ok() {
        let http = MockHttp(vec![
            (
                "bamgrid.com/graph",
                UnlockResponse::ok(200, r#"{"countryCode":"HK","inSupportedLocation":true}"#),
            ),
            (
                "bamgrid.com/devices",
                UnlockResponse::ok(200, r#"{"assertion":"A1"}"#),
            ),
            (
                "bamgrid.com/token",
                UnlockResponse::ok(200, r#"{"refresh_token":"R1"}"#),
            ),
            // preview 腿传输失败（status=0 + error）→ reachable()==false。
            (
                "www.disneyplus.com",
                UnlockResponse::err("connection reset"),
            ),
        ]);
        let r = check_disney(&http).await;
        assert_eq!(
            r.status,
            UnlockStatus::Timeout,
            "preview 腿没打通 → 判不了 ⑤ → 如实 Timeout（绝不拿缺证据冒充阴性证据）"
        );
    }

    /// **Disney ⑤ preview 腿的挑战守卫**：CF 挑战页同理 —— 拿到的是挑战页的链，不是真实落地链。
    ///
    /// **变异锁**：删掉 `any_challenged(&[&pv])` 那道守卫 → 挑战页链不含 marker → 落 ⑥ 出 **Ok** → 转红。
    #[tokio::test]
    async fn disney_preview_challenge_yields_restricted_not_ok() {
        let http = MockHttp(vec![
            (
                "bamgrid.com/graph",
                UnlockResponse::ok(200, r#"{"countryCode":"HK","inSupportedLocation":true}"#),
            ),
            (
                "bamgrid.com/devices",
                UnlockResponse::ok(200, r#"{"assertion":"A1"}"#),
            ),
            (
                "bamgrid.com/token",
                UnlockResponse::ok(200, r#"{"refresh_token":"R1"}"#),
            ),
            ("www.disneyplus.com", challenge_resp()),
        ]);
        let r = check_disney(&http).await;
        assert_eq!(
            r.status,
            UnlockStatus::Restricted,
            "preview 腿被风控拦截 → Restricted（换出口），不是 Ok"
        );
    }

    // ── Grok 弱 checker（设计 §3.3 判定表 G1/G2/G4/G5 + G3 空规则 + region 双源）─────────

    #[tokio::test]
    async fn grok_ok_on_app_page_with_trace_region() {
        // G4（解锁）：200 + 应用页特征 marker + trace loc → Ok（**弱语义**：仅站点可达）。region 取 trace loc。
        let http = MockHttp(vec![
            (
                "grok.com/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=JP\n"),
            ),
            (
                "grok.com",
                UnlockResponse::ok(
                    200,
                    "<html><script src=cdn.grok.com/_next/x.js></script></html>",
                ),
            ),
        ]);
        let r = check_grok(&http).await;
        assert_eq!(r.status, UnlockStatus::Ok);
        assert_eq!(r.region.as_deref(), Some("JP"));
    }

    #[tokio::test]
    async fn grok_restricted_on_challenge() {
        // G2（受限）：首页命中 CF 挑战 → Restricted（风控拦截，非地区问题）。
        let http = MockHttp(vec![
            (
                "grok.com/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=US\n"),
            ),
            ("grok.com", challenge_resp()),
        ]);
        let r = check_grok(&http).await;
        assert_eq!(r.status, UnlockStatus::Restricted);
        assert_eq!(r.region.as_deref(), Some("US"));
    }

    /// **变异锁（G2 必须在 G4 之前短路）**：构造一个**同时**命中 CF 挑战与 `APP_MARKER` 的响应
    /// （真实挑战页可以内嵌站点资源引用，`grok.com/` 的 CF 挑战尤其可能带 `cdn.grok.com` 串）。
    ///
    /// 删掉 `any_challenged` 短路、或把它挪到 G4 之后 → 本例走 G4 判 **Ok** → 绿灯谎报「站点可达」，
    /// 而实际什么都没测到 → 本测转红。
    #[tokio::test]
    async fn grok_challenge_wins_over_app_marker_not_ok() {
        let mut resp = challenge_resp();
        resp.body = format!(
            "<html>Just a moment...<script src={}></script></html>",
            GrokEndpoints::APP_MARKER
        );
        let http = MockHttp(vec![
            (
                "grok.com/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=US\n"),
            ),
            ("grok.com", resp),
        ]);
        let r = check_grok(&http).await;
        assert_eq!(
            r.status,
            UnlockStatus::Restricted,
            "挑战页即便含应用页特征串也必须归 Restricted —— 判据来自 CF 的墙，不是 grok 的真实响应"
        );
    }

    #[tokio::test]
    async fn grok_timeout_when_home_unreachable() {
        // G1（不可达）：首页传输失败 → Timeout（无响应 ≠ 封禁），无 region。
        let http = MockHttp(vec![]);
        let r = check_grok(&http).await;
        assert_eq!(r.status, UnlockStatus::Timeout);
        assert_eq!(r.region, None);
    }

    #[tokio::test]
    async fn grok_timeout_on_200_without_app_marker() {
        // G5（无法判定）：200 但无应用页特征（非 CF、非挑战）→ 保守兜底 Timeout（不误 Ok/Blocked）。
        let http = MockHttp(vec![
            (
                "grok.com/cdn-cgi/trace",
                UnlockResponse::ok(200, "ip=1.1.1.1\nloc=US\n"),
            ),
            (
                "grok.com",
                UnlockResponse::ok(200, "<html>unexpected portal</html>"),
            ),
        ]);
        let r = check_grok(&http).await;
        assert_eq!(r.status, UnlockStatus::Timeout);
        assert_eq!(r.region.as_deref(), Some("US"));
    }

    #[tokio::test]
    async fn grok_region_falls_back_to_x_country_code_header() {
        // region 辅源：trace 无有效 loc（同脚本回落，body 无 ip=）→ 用 x-country-code 头。
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(GrokEndpoints::COUNTRY_HEADER.to_string(), "DE".to_string());
        let home = UnlockResponse {
            status: 200,
            body: "<html>cdn.grok.com/_next</html>".to_string(),
            truncated: false,
            redirect_chain: Vec::new(),
            error: None,
            headers,
        };
        // 只脚本 "grok.com" → trace 请求也命中它，body 无 ip= → trace_region None → 回落 header。
        let http = MockHttp(vec![("grok.com", home)]);
        let r = check_grok(&http).await;
        assert_eq!(r.status, UnlockStatus::Ok);
        assert_eq!(r.region.as_deref(), Some("DE"));
    }

    /// **G3 空规则的诚实门（钉死「未解锁」分支目前不存在）**：grok 查无静态 geo-block marker，
    /// 设计 §3.3/§11 明确 G3 上线初期为空规则、G3′（`POST /rest/models` 模型清单差异法）**须真机标定后**才上。
    ///
    /// 本测把这条边界钉成断言：任何「站点非 200 / 非挑战」的形态都必须落 `Timeout`，**绝不**冒出 `Blocked`。
    /// 谁哪天凭猜测加一条 Blocked 规则（如把 403 直接判地区封禁），本测转红 —— 逼他先做标定。
    #[tokio::test]
    async fn grok_never_reports_blocked_before_g3_is_calibrated() {
        for (status, body) in [
            (403u16, "Forbidden"),
            (451, "Unavailable For Legal Reasons"),
            (503, "Service Unavailable"),
            (200, "This service is not available in your region"),
        ] {
            let http = MockHttp(vec![
                (
                    "grok.com/cdn-cgi/trace",
                    UnlockResponse::ok(200, "ip=1.1.1.1\nloc=DE\n"),
                ),
                ("grok.com", UnlockResponse::ok(status, body)),
            ]);
            let r = check_grok(&http).await;
            assert_eq!(
                r.status,
                UnlockStatus::Timeout,
                "status={status} body={body:?}：G3 未标定前不得判 Blocked（宁可无信息，不可误报）"
            );
        }
    }

    // ── TikTok（对齐 1-stream check.sh MediaUnlockTest_Tiktok；判据来源见 TiktokEndpoints）───

    /// 302 → location 的响应（模拟 curl `%{url_effective}` 的最终落点）。
    fn redirect_to(location: &str) -> UnlockResponse {
        UnlockResponse {
            status: 200,
            body: String::new(),
            truncated: false,
            error: None,
            redirect_chain: vec![crate::http::RedirectHop {
                status: 302,
                location: location.to_string(),
            }],
            ..Default::default()
        }
    }

    /// store_region 正常返回 `region`（小写，如 "us"）的脚本项。
    /// **须排在首页脚本之前**：mock 首匹配生效，而 `"www.tiktok.com/"` 是 passport URL 的子串。
    fn store_region(region: &str) -> (&'static str, UnlockResponse) {
        (
            "tiktok.com/passport/web/store_region/",
            UnlockResponse::ok(
                200,
                format!(r#"{{"data":{{"store_region":"{region}"}},"message":"success"}}"#),
            ),
        )
    }

    #[tokio::test]
    async fn tiktok_ok_when_home_stays_on_feed() {
        // 解锁：无跳转 = 停在正常 feed → check.sh 绿色 Yes → Ok。
        let http = MockHttp(vec![
            store_region("us"),
            (
                "www.tiktok.com/",
                UnlockResponse::ok(200, "<html>feed</html>"),
            ),
        ]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Ok);
        assert_eq!(r.region.as_deref(), Some("US")); // store_region 大写化
    }

    #[tokio::test]
    async fn tiktok_blocked_on_about_landing() {
        // 未解锁：check.sh `[[ "$result" == *"/about" ]]` → No。带 query 顺带验 strip_query_fragment 硬化。
        let http = MockHttp(vec![
            store_region("jp"),
            (
                "www.tiktok.com/",
                redirect_to("https://www.tiktok.com/about?lang=en"),
            ),
        ]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Blocked);
        assert_eq!(r.region.as_deref(), Some("JP"));
    }

    #[tokio::test]
    async fn tiktok_partial_when_cn_redirected_to_douyin() {
        // 部分解锁：check.sh 黄色 "Provided by Douyin" —— 落地页 + store_region==cn → Partial（非 Blocked）。
        let http = MockHttp(vec![
            store_region("cn"),
            (
                "www.tiktok.com/",
                redirect_to("https://www.tiktok.com/about"),
            ),
        ]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Partial);
        assert_eq!(r.region.as_deref(), Some("CN"));
    }

    #[tokio::test]
    async fn tiktok_timeout_when_home_unreachable() {
        // 不可达：无脚本匹配 → status=0 + error → Timeout（无响应 ≠ 封禁）。
        let http = MockHttp(vec![]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Timeout);
        assert_eq!(r.region, None);
    }

    /// **变异锁（挑战识别必须在 URL 分类之前）**：CF 挑战页**不重定向** → `redirect_chain` 空 →
    /// 最终 URL 退化成首页 URL → 不含任何落地页 marker → 走「停在 feed」分支判 **Ok**。
    ///
    /// 即：TikTok 这条 checker 的兜底方向是 `Ok`，缺了挑战守卫就是**谎报绿灯**。
    /// 删掉 `any_challenged(&[&home])` → 本测得到 Ok → 转红。
    #[tokio::test]
    async fn tiktok_challenge_yields_restricted_not_ok() {
        let http = MockHttp(vec![
            store_region("us"),
            ("www.tiktok.com/", challenge_resp()),
        ]);
        let r = check_tiktok(&http).await;
        assert_eq!(
            r.status,
            UnlockStatus::Restricted,
            "挑战页不重定向 → 最终 URL 仍是首页 → 缺守卫就误 Ok（绿灯但什么都没测到）"
        );
        assert_eq!(
            r.region.as_deref(),
            Some("US"),
            "region 仍从 store_region 取"
        );
    }

    #[tokio::test]
    async fn tiktok_store_region_failure_does_not_gate_verdict() {
        // 对齐 check.sh：只以首页结果判定；store_region 挂了只是没 region 展示，不改 Ok/Blocked。
        let http = MockHttp(vec![(
            "www.tiktok.com/",
            UnlockResponse::ok(200, "<html>feed</html>"),
        )]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Ok);
        assert_eq!(r.region, None);
    }

    #[tokio::test]
    async fn tiktok_blocked_without_region_when_store_region_missing() {
        // 落地页 + 无 region → region != CN → Blocked（不误判为 Douyin/Partial）。
        let http = MockHttp(vec![(
            "www.tiktok.com/",
            redirect_to("https://www.tiktok.com/about"),
        )]);
        let r = check_tiktok(&http).await;
        assert_eq!(r.status, UnlockStatus::Blocked);
        assert_eq!(r.region, None);
    }

    /// TikTok 纯分类器 fixture 表：直接喂 check.sh `%{url_effective}` 文档化响应形态 → 断言 UnlockStatus。
    /// 来源 = 1-stream/RegionRestrictionCheck `check.sh:3457-3482`（`MediaUnlockTest_Tiktok`）。
    /// **无 HTTP、无 mock**：纯 parse/判定层验证（HTTP 编排由上面的 MockHttp 集测覆盖）。
    /// 端点/响应真实性未经真机验证（本机无海外出口）——URL 形态来自 check.sh 判定分支，见标定 runbook。
    ///
    /// **变异锁**：`/about?lang=en` 一行钉死「先 strip_query_fragment 再匹配」这条硬化 ——
    /// 改回 check.sh 的 raw 后缀匹配（bash `*"/about"`）→ 该行判 Ok → 转红。
    #[test]
    fn tiktok_classifier_matches_checksh_decision_table() {
        // (首页跟随跳转后的最终 url_effective, store_region 大写, 期望 status)
        let cases: &[(&str, Option<&str>, UnlockStatus)] = &[
            // 停在 feed（未被打到落地页）→ check.sh 绿色 Yes → Ok
            ("https://www.tiktok.com/", Some("US"), UnlockStatus::Ok),
            (
                "https://www.tiktok.com/foryou",
                Some("JP"),
                UnlockStatus::Ok,
            ),
            // `*"/about"` 结尾落地页 → check.sh 红色 No → Blocked
            (
                "https://www.tiktok.com/about",
                Some("JP"),
                UnlockStatus::Blocked,
            ),
            // 硬化：`/about?lang=xx`（带 query）check.sh 会漏判 Yes，此处归一化后仍判 Blocked
            (
                "https://www.tiktok.com/about?lang=en",
                Some("DE"),
                UnlockStatus::Blocked,
            ),
            // `*"/status"*` 落地页 → No → Blocked
            (
                "https://www.tiktok.com/status/unavailable",
                Some("DE"),
                UnlockStatus::Blocked,
            ),
            // `*"landing"*` 落地页 → No → Blocked
            (
                "https://www.tiktok.com/login/landing",
                Some("FR"),
                UnlockStatus::Blocked,
            ),
            // 落地页 + region==CN → check.sh 黄色 Provided by Douyin → Partial
            (
                "https://www.tiktok.com/about",
                Some("CN"),
                UnlockStatus::Partial,
            ),
            // 落地页 + region 缺失 → 非 CN → Blocked（不误判 Douyin）
            ("https://www.tiktok.com/about", None, UnlockStatus::Blocked),
        ];
        for (url, region, want) in cases {
            let got = classify_tiktok(url, region.map(|s| s.to_string()));
            assert_eq!(got.status, *want, "status url={url} region={region:?}");
            // region 仅透传（不改判定）：验证 store_region 值原样带出供 UI 展示
            assert_eq!(got.region.as_deref(), *region, "region 透传 url={url}");
        }
    }

    /// **Netflix region 逐跳续找**：真实链常是 `netflix.com/` →（无路径段）→ `netflix.com/hk-en/title/...`。
    ///
    /// **变异锁**：把两处 `else { continue }` 改回 `?` → 首跳无路径段即从整个函数返回 None → region 丢成
    /// `None`（进而被上层的 `status==200 → "US"` 兜底误标成 US）→ 本测转红。
    #[test]
    fn netflix_region_skips_non_region_hops_instead_of_aborting() {
        use crate::http::{RedirectHop, UnlockResponse};
        let res = UnlockResponse {
            status: 200,
            body: String::new(),
            truncated: false,
            error: None,
            redirect_chain: vec![
                // 跳 1：无路径段（`?` 会在此处放弃整条链）。
                RedirectHop {
                    status: 302,
                    location: "https://www.netflix.com/".to_string(),
                },
                // 跳 2：路径段不是地区形态（`?` 同样会在此放弃）。
                RedirectHop {
                    status: 302,
                    location: "https://www.netflix.com/browse/genre".to_string(),
                },
                // 跳 3：真正的地区段 —— 必须找得到。
                RedirectHop {
                    status: 302,
                    location: "https://www.netflix.com/hk-en/title/81280792".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(netflix_region_from_chain(&res), Some("HK".to_string()));
    }

    /// 反向门：`en` 这类已知非地区段仍须跳过（不能因为改成 continue 就把语言码当地区）。
    #[test]
    fn netflix_region_still_skips_known_non_region_segment() {
        use crate::http::{RedirectHop, UnlockResponse};
        let res = UnlockResponse {
            status: 200,
            body: String::new(),
            truncated: false,
            error: None,
            redirect_chain: vec![
                RedirectHop {
                    status: 302,
                    location: "https://help.netflix.com/en".to_string(),
                },
                RedirectHop {
                    status: 302,
                    location: "https://www.netflix.com/jp/title/70143836".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(netflix_region_from_chain(&res), Some("JP".to_string()));
    }

    // ── 浏览器头集确实接到了每个请求上（(c) 的接线门）────────────────────────────

    /// 记录所有发出请求的 mock（验头集接线，不验判定）。
    struct RecordingHttp(std::sync::Mutex<Vec<UnlockRequest>>);

    #[async_trait::async_trait]
    impl UnlockHttp for RecordingHttp {
        async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
            self.0.lock().unwrap().push(req.clone());
            UnlockResponse::err("recording-only")
        }
    }

    /// 跑**全部**已实现 checker（上线集 + 停飞集），收集它们实际发出的请求。
    ///
    /// 刻意遍历 [`ServiceId::ALL`] ⧺ [`ServiceId::PENDING_CALIBRATION`] 而非硬编码清单：新增服务自动
    /// 落进头集接线门，不会因为「加了 checker 忘了加进这个列表」而漏守（grok/tiktok 接入时就是靠这条
    /// 自动覆盖）。**停飞集也要跑**：grok 的 checker 仍在仓里，标定后随时上线，其间不能失去头集守护
    /// （否则「停飞期间悄悄退化 → 上线那天才发现」）。
    async fn record_all_requests() -> Vec<UnlockRequest> {
        let http = RecordingHttp(std::sync::Mutex::new(Vec::new()));
        for id in ServiceId::ALL.iter().chain(ServiceId::PENDING_CALIBRATION) {
            let _ = run_checker(*id, &http).await;
        }
        let out = http.0.lock().unwrap().clone();
        out
    }

    #[tokio::test]
    async fn every_outgoing_request_carries_the_full_browser_header_set() {
        // 这扇门守 (c) 的**接线**（`browser.rs` 的单测只守头集内容本身）：
        // 任何一个 checker 漏套 `get_req`/`post_req` → 该请求缺头 → 转红。
        // 变异验证：把任一 checker 改回 `UnlockRequest::get(url).header("User-Agent", UA)` → 转红。
        let reqs = record_all_requests();
        let reqs = reqs.await;
        assert!(!reqs.is_empty(), "应至少发出一个请求");
        for req in &reqs {
            for name in [
                "user-agent",
                "accept",
                "accept-encoding",
                "accept-language",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
            ] {
                let found = req
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case(name));
                assert!(found, "{} 缺 {name} 头（漏套浏览器头集）", req.url);
            }
        }
    }

    #[tokio::test]
    async fn accept_encoding_is_sent_on_every_request() {
        // (c) 的核心症状单列一门：**自称 Chrome 却不发 Accept-Encoding**。
        // 注意这只证明「发了」；「发的编码传输层解得开」由 src-tauri 的回环解压门守。
        let reqs = record_all_requests().await;
        for req in &reqs {
            let ae = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
                .map(|(_, v)| v.as_str());
            assert_eq!(
                ae,
                Some(crate::browser::ACCEPT_ENCODING),
                "{} 的 Accept-Encoding 必须 = 头集 SoT",
                req.url
            );
        }
    }

    #[tokio::test]
    async fn business_headers_win_over_browser_header_set() {
        // Disney 的 Authorization/Content-Type 必须盖过头集（后设者覆盖）——
        // 若顺序反了，bamgrid 的鉴权会被 `Accept: */*` 之类顶掉而全线 Timeout。
        let reqs = record_all_requests().await;
        let dev = reqs
            .iter()
            .find(|r| r.url.contains("/devices"))
            .expect("Disney devices 请求必须发出");
        assert_eq!(
            dev.headers.get("Content-Type").map(String::as_str),
            Some("application/json; charset=UTF-8")
        );
        assert!(dev
            .headers
            .get("Authorization")
            .is_some_and(|v| v.starts_with("Bearer ")));
        assert_eq!(dev.method, HttpMethod::Post);
        // POST 的 Api profile 必须带 Origin（Chrome 语义）。
        assert_eq!(
            dev.headers.get("Origin").map(String::as_str),
            Some("https://disney.api.edge.bamgrid.com")
        );
    }

    /// 测试用 RedirectHop 构造糖（避免在断言里重复写字段）。
    struct RedirectHopForTest;
    impl RedirectHopForTest {
        fn hop(status: u16, location: &str) -> crate::http::RedirectHop {
            crate::http::RedirectHop {
                status,
                location: location.to_string(),
            }
        }
    }
}
