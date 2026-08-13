//! polaris-unlock-transport —— 解锁检测的**指纹伪装传输层**（`UnlockHttp` 的唯一生产实现）。
//!
//! 见 `~/docs/polaris/design/polaris-unlock-detection-cf.md`（根因与选型定案）。
//!
//! # 解决什么问题（根因 (b)：TLS/H2 指纹）
//!
//! 调研 §3.2 判定**主因**：上游 走 Electron `net.request` = Chromium 的 BoringSSL，ClientHello / HTTP2
//! SETTINGS / 伪头序都是**真 Chrome 形态**，Cloudflare 压根不对它出挑战；Polaris 走 `reqwest + rustls`，
//! ClientHello 是 rustls 默认形态，在 WAF 侧高度可识别 → 1020/403。
//!
//! **同仓已有实证**：`src-tauri/src/runtime/http.rs` 的 `warp_client` 文档记录过同一类问题 ——
//! 共享 rustls client 被 `api.cloudflareclient.com` 按 TLS 指纹判「自动化」。WARP 那条腿靠降级
//! TLS1.2+HTTP/1.1 绕过；解锁检测面对的是通用 CF 边缘，同招不管用，只能上真指纹伪装。
//!
//! 本 crate 用 `wreq` + `wreq_util::Profile::Chrome{N}`（`N` = [`CHROME_MAJOR`]）提供
//! **该版 Chrome 的 TLS(JA3/JA4) + HTTP/2 + 默认头形态**。
//! 能力边界 = **指纹，无 JS 执行** —— 这**恰好就是 上游的能力边界**
//! （调研 §5 已校正：上游 也不执行 JS，它是「没被出挑战」而非「解开了挑战」）。故本 crate 达成的是
//! **与 上游 严格对齐，不多不少**。
//!
//! # 为什么单独一个 crate（而不是塞进 `src-tauri/src/runtime/http.rs`）
//!
//! 任务硬要求：**指纹客户端不得泄漏到其它 HTTP 路径**（订阅拉取 / 内核下载 / 更新 / WARP 注册都不需要
//! 伪装，也不该承担这个依赖的风险）。放进 `src-tauri` 只能靠**约定**守边界 —— 任何模块都能
//! `use wreq`。独立 crate 把它变成**结构性**约束：`wreq` 只是本 crate 的依赖，
//! 其它 crate 想用必须先改 Cargo.toml（review 时看得见）。
//!
//! 代价如实记：进程内会有**两套 TLS 栈**（rustls-ring + BoringSSL）。这是指纹伪装的固有成本，
//! 调研 §4.B「代价」栏已列明并被接受。
//!
//! # 依赖选型（版本与理由）
//!
//! | 项 | 结论 |
//! |---|---|
//! | `rquest` | **已弃**。crates.io `max_version = 0.0.0`（全撤），最后真实版本 5.2.0 停在 2025-07-11；上游仓已归档为 `rquest-deprecated`。**不可用**。 |
//! | `wreq` **6.0.0-rc.29** | 继任者（同作者 0x676e67）。**刻意钉 pre-release**：稳定线 5.3.0 的指纹模板由 `wreq-util 2.x` 供给，Chrome 止于 **137**（≈ 一年前），上游把新模板全放进 6.x/3.x 线 ⇒ 留在稳定版 = 指纹**永远落后一年**，对一个靠「装得像真浏览器」工作的模块，落后本身就是被识别的信号。 |
//! | `wreq-util` **3.0.0-rc.14** | 浏览器指纹模板，其 `wreq` 约束是 `^6.0.0-rc` ⇒ **与上面绑定升**（升 util 必然把 wreq 拖到 RC）。Chrome 模板覆盖 100~**149**，本 crate 按 [`CHROME_MAJOR`] const 派生选用（与 [`UA`](polaris_unlock::endpoints::UA) **精确同版**）。 |
//! | Tauri WebView | **不选**。mac/Linux 是 WebKit 非 Chromium → 配 Chrome UA 反成**更强** bot 信号（比现状更糟）；另有 macOS 14+ / `macos-proxy` 门槛与远程源 IPC 安全面。 |
//! | cronet | **不选**。仓内 `libcronet.*` 只是随 sing-box 内核分发的 so/dylib，**无任何 Rust 绑定**；复用成本 = 从零写 FFI，换来的能力与 `wreq` 同级（指纹，无 JS）。严格劣于 `wreq`。 |
//!
//! **敢钉 RC 的前提 = 爆炸半径已封死**：`wreq`/`wreq-util` 只被本 crate 依赖（由
//! `wreq_is_declared_only_by_this_crate` 门守）；主代理数据面是 sing-box 外部进程 + hyper/tonic gRPC 控制面，
//! 其余 HTTP（订阅拉取 / 更新检查 / 规则资源）走 `src-tauri` 的 reqwest+rustls。**都不经过 `wreq`**
//! ⇒ RC 引擎出问题的后果是「解锁徽章不准」，不是「代理不通」。
//!
//! **构建链代价（三平台 CI 必读）**：`wreq` 6 的 TLS 后端从 `boring2`/`boring-sys2`/`tokio-boring2`
//! 换成 `btls`/`btls-sys`/`tokio-btls`（同为 **vendored BoringSSL**），HTTP/2 从 `hyper2` 换成
//! `http2`+`wreq-proto`+`wreq-rt`。构建前置**没变**：`cmake` + C 工具链 + `libclang`（`bindgen`）。
//! 本机实测：缺 `cmake` 直接失败；缺 clang builtin headers 报 `'stddef.h' file not found`，
//! 需 `BINDGEN_EXTRA_CLANG_ARGS` 指向 gcc include 目录。
//! **交叉编译（Windows/macOS 产物）须重新验**——这是本依赖最大的落地风险面。
//!
//! # 6.x / 3.x 的破坏性变更（本 crate 逐条跟进过的）
//!
//! | 变更 | 处理 |
//! |---|---|
//! | `wreq_util::Emulation` 枚举更名 `Profile`；`Emulation` 变成 profile+platform 的 builder | 派生表返回 [`wreq_util::Profile`]；`Emulation::ChromeNNN` 别名仍编译得过，故源码扫描门同时拦两种写法 |
//! | `Response::chunk()` **删除**，流式读只剩 `bytes_stream()`（`stream` feature） | 开 `stream`；[`read_body_truncating`] 改按值接管 `Response`，截断 + 停滞看门狗逐字保留 |
//! | `wreq::Url` **不再 re-export**（改 `http::Uri` + `IntoUri`） | 直接依赖 `url` crate（wreq 6 内部解 `Location` 用的同一个），零新增 lock 包 |
//! | `RequestBuilder::header()` 从「覆盖同名」变成 **`append`**（文档没跟上，代码是 `headers_mut().append`） | 本实现的每个 header 名在单次请求里只设一次（源是 `BTreeMap`），append 后再由客户端层 `replace_headers` 覆盖 emulation 默认头；并加了**线级不重复**断言把这条钉死 |
//! | `wreq-util` 的 `emulation` feature 不再转发 `wreq/gzip\|brotli\|deflate\|zstd` | 四个压缩 feature 改由本 crate **直接**声明（漏声明 = 解压静默失效 = marker 全落空的静默误判） |
//! | `wreq` 默认 feature 集变化（`charset`/`macos-system-configuration` 出，`tokio-rt` 入） | 无影响：body 一律 `from_utf8_lossy` 不走 charset；代理由 `.proxy()`/`.no_proxy()` 显式指定 |
//! | `Policy::none()` 语义 | **未变**（仍是「一跳都不跟」），逐跳 `redirect_chain` 仍由本实现自己跑 |
//!
//! # 模板漂移风险 / 三处身份的同源约束
//!
//! 指纹模板的价值 100% 取决于它跟不跟得上真 Chrome。Chrome 版本推进后若不同步升级，
//! 指纹会变成「旧版 Chrome」这一**可疑信号**。
//!
//! 但**更糟的失效形态是「只升一处」**：TLS 指纹说 137、UA 说 131 ⇒ 自相矛盾，
//! 指纹服务专抓这种，比单纯陈旧更强的 bot 信号。故浏览器身份收口在
//! [`CHROME_MAJOR`](polaris_unlock::endpoints::CHROME_MAJOR) 一个常量上：
//!
//! - 本 crate 的 emulation **由它 const 派生**（[`chrome_emulation`]）——**没有第二处版本字面量可改**，
//!   且 `wreq-util` 无对应模板时 const 求值 panic ⇒ **编译失败**；
//! - `Profile::Chrome…`（含 `Emulation::Chrome…` 这个 3.x 别名）字面量只准出现在派生表里，
//!   由 `emulation_variant_is_derived_not_hardcoded` 源码扫描门守（绕过派生直接写死 = 转红）；
//! - UA / `sec-ch-ua` 那半边由 unlock crate 的 `sec_ch_ua_major_version_matches_ua` 守。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use polaris_unlock::endpoints::{CHROME_MAJOR, MAX_BODY_BYTES, MAX_REDIRECTS, REQ_TIMEOUT_MS};
use polaris_unlock::http::{HttpMethod, RedirectHop, UnlockHttp, UnlockRequest, UnlockResponse};

/// 单请求超时（= 上游 `unlock-http.ts` 8s，经 unlock crate 常量单点取）。
const UNLOCK_TIMEOUT: Duration = Duration::from_millis(REQ_TIMEOUT_MS);

/// Chrome 主版本号 → `wreq-util` 指纹模板。**这是本 crate 唯一允许写 `Profile::Chrome…` 的地方。**
///
/// 每行形如 `NNN => wreq_util::Profile::ChromeNNN,`，左右两个数字必须相等 ——
/// 由 `emulation_variant_is_derived_not_hardcoded` 源码扫描门逐行校验（写错行号即转红）。
///
/// `_` 分支 `panic!` 在 const 上下文求值 ⇒ 把 [`CHROME_MAJOR`] 升到 `wreq-util` 没有模板的版本
/// 是**编译错误**，绝不会静默退回旧模板（也就不会出现「以为升了、其实没升」）。
///
/// **wreq-util 3.x 的类型改名**：带 `ChromeNNN` 变体的枚举从 `Emulation` 更名为
/// [`wreq_util::Profile`]；`Emulation` 现在是「profile + platform + http2/headers 开关」的
/// builder 结构体。`wreq_util::Emulation::ChromeNNN` 作为**同类型关联常量**仍然编译得过
/// （`define_enum!` 给 `Emulation` 补了一批 `pub const ChromeNNN: Profile`），故上面那扇源码扫描门
/// 两种写法都拦（只放行本表的 `wreq_util::Profile::ChromeNNN` 形态）。
const fn chrome_emulation(major: u32) -> wreq_util::Profile {
    match major {
        131 => wreq_util::Profile::Chrome131,
        132 => wreq_util::Profile::Chrome132,
        133 => wreq_util::Profile::Chrome133,
        134 => wreq_util::Profile::Chrome134,
        135 => wreq_util::Profile::Chrome135,
        136 => wreq_util::Profile::Chrome136,
        137 => wreq_util::Profile::Chrome137,
        138 => wreq_util::Profile::Chrome138,
        139 => wreq_util::Profile::Chrome139,
        140 => wreq_util::Profile::Chrome140,
        141 => wreq_util::Profile::Chrome141,
        142 => wreq_util::Profile::Chrome142,
        143 => wreq_util::Profile::Chrome143,
        144 => wreq_util::Profile::Chrome144,
        145 => wreq_util::Profile::Chrome145,
        146 => wreq_util::Profile::Chrome146,
        147 => wreq_util::Profile::Chrome147,
        148 => wreq_util::Profile::Chrome148,
        149 => wreq_util::Profile::Chrome149,
        _ => panic!(
            "wreq-util 3.0.0-rc.14 无此 Chrome 指纹模板（其 Chrome 模板止于 149）——\
             升 CHROME_MAJOR 前须先确认 wreq-util 已支持该版本并在此表补行"
        ),
    }
}

/// 本 client 使用的指纹模板 —— 由 [`CHROME_MAJOR`] 派生，**不是**独立可改的第二处版本钉子。
const EMULATION: wreq_util::Profile = chrome_emulation(CHROME_MAJOR);

/// 指纹伪装的解锁检测客户端 —— [`UnlockHttp`] 的**唯一生产实现**。
///
/// 出口 pin 语义与原 `HttpRuntime::via_local_proxy` 等价（经本机 mixed 口的 HTTP CONNECT 代理），
/// 故检测请求走用户当前分流出口 —— 与 上游的 socks5 session pin 同效。
pub struct UnlockClient {
    client: wreq::Client,
}

/// 建带 Chrome 指纹（版本 = [`CHROME_MAJOR`]）的 client 骨架（`redirect` 关死：解锁要逐跳 `redirect_chain`）。
fn base_builder() -> wreq::ClientBuilder {
    wreq::Client::builder()
        // 指纹本体：TLS(JA3/JA4) + HTTP/2 SETTINGS/伪头序 + Chrome 默认头集与**发送顺序**。
        // 版本由 `EMULATION` 派生自 `CHROME_MAJOR` —— 此处**不得**写死 `Profile::Chrome…`。
        //
        // wreq-util 3.x 起可另配 `Platform`（`Emulation::builder().profile(..).platform(..)`），
        // 默认 `MacOS`。**刻意不配 `Platform::Windows`**：本实现逐请求覆盖 emulation 的每一条默认头
        // （UA / sec-ch-ua* / accept* / sec-fetch* / priority 全在 `browser_headers` 里），platform
        // 到不了线上；而把它设成 Windows 会让 `browser_headers_reach_the_wire` 那扇门失去鉴别力
        // ——该门正是靠「线上 UA 是我们的 Windows Chrome 而**不是** emulation 默认的 macOS UA」
        // 来证明覆盖是**完全**的。两者同为 Windows 后，覆盖漏一条也测不出来。
        .emulation(EMULATION)
        // 逐跳记录 Location 由本实现自己跑（checker 的判据之一是完整 redirect_chain）。
        .redirect(wreq::redirect::Policy::none())
        // **cookie jar**：补齐「像浏览器」的最后一块行为面（与 TLS/头指纹正交）。
        //
        // 没有它时：一个 cookie 都不发，且**跨重定向跳丢 `Set-Cookie`** —— 而我们恰恰是手动逐跳跟随的，
        // 首跳响应 `Set-Cookie` 后第二跳照样裸奔。真 Chrome 全新 profile 打 `netflix.com/title/X` 的行为是
        // 「首请求无 cookie → 拿到 `Set-Cookie` → 跟随跳转时带上」，我们与之的差别是一条**独立于指纹**的信号。
        //
        // 为什么这里加是安全的（不会引入跨轮/跨出口污染）：`UnlockClient` 在
        // `src-tauri/src/commands/unlock.rs` 里**每轮检测新建**（warm 补测也各建一个），
        // jar 随 client 一起丢弃 ⇒ 不存在「上一轮 / 上一个出口的 cookie 影响这一轮判定」。
        // 轮内 6 个 checker 并发打不同域，cookie 按域隔离，互不串。
        //
        // 该行为由 `set_cookie_is_carried_across_manual_redirect_hops` 回环门守（删掉本行即转红）。
        .cookie_store(true)
}

impl UnlockClient {
    /// 经本机 mixed 端口的检测客户端（**生产路径**：走用户当前分流出口）。
    ///
    /// # Errors
    ///
    /// 代理地址非法或 client 构建失败（BoringSSL 初始化）。
    pub fn via_local_proxy(port: u16) -> Result<Self, String> {
        let proxy = wreq::Proxy::all(format!("http://127.0.0.1:{port}"))
            .map_err(|e| format!("本机代理地址非法（port={port}）: {e}"))?;
        let client = base_builder()
            .proxy(proxy)
            .build()
            .map_err(|e| format!("建解锁检测客户端失败: {e}"))?;
        Ok(Self { client })
    }

    /// 直连检测客户端（**不经代理**）。
    ///
    /// 仅供本 crate 的回环单测使用；生产检测**必须**走 [`Self::via_local_proxy`] ——
    /// 直连会测到宿主 IP 而给出完全错误的结论（上游 对这个陷阱有显式防御：
    /// `UnlockDetectionService.ts:9`「setProxy reject → 本轮放弃，绝不落 default session」）。
    ///
    /// # Errors
    ///
    /// client 构建失败。
    pub fn direct() -> Result<Self, String> {
        base_builder()
            .no_proxy()
            .build()
            .map(|client| Self { client })
            .map_err(|e| format!("建解锁检测客户端失败: {e}"))
    }
}

/// 流式读 body 并**截断**到上限（解锁语义：截断后仍要判，与订阅相反）。返回 `(bytes, truncated)`。
///
/// 每个 chunk 单独计时 = 停滞看门狗（整请求超时管不了「一直有数据但极慢」）。
///
/// **wreq 6 破坏性变更**：`Response::chunk()` 被删，流式读只剩 `bytes_stream()`（`stream` feature）。
/// 故本函数改为**按值接管** `Response`（`bytes_stream(self)` 消费它），调用方须先把 status/headers 取走。
/// 逐 chunk 的截断与停滞看门狗语义**逐字保留** —— 换成 `Response::bytes()` 会同时丢掉这两条
/// （整包缓冲 = 无上限内存 + 慢速流拖满整请求超时）。
async fn read_body_truncating(
    resp: wreq::Response,
    max: usize,
    stall: Duration,
) -> Result<(Vec<u8>, bool), String> {
    let mut stream = Box::pin(resp.bytes_stream());
    let mut buf = Vec::new();
    loop {
        let next = tokio::time::timeout(stall, stream.next())
            .await
            .map_err(|_| format!("读取响应体停滞超过 {}ms", stall.as_millis()))?;
        match next
            .transpose()
            .map_err(|e| format!("读取响应体失败: {e}"))?
        {
            None => return Ok((buf, false)),
            Some(chunk) => {
                if buf.len() + chunk.len() > max {
                    buf.extend_from_slice(&chunk[..max.saturating_sub(buf.len())]);
                    return Ok((buf, true));
                }
                buf.extend_from_slice(&chunk);
            }
        }
    }
}

#[async_trait]
impl UnlockHttp for UnlockClient {
    /// **永不 panic、永不 Err**：失败落 `status=0 + error`（契约：由 checker 兜底为 Timeout）。
    ///
    /// # 头集与 emulation 的关系（诚实边界）
    ///
    /// [`EMULATION`] 已在 client 级设了一套 Chrome 默认头（含**发送顺序**）。本实现把
    /// [`UnlockRequest`] 携带的头（来自 `polaris_unlock::browser`）逐条 `header()` 覆盖上去：
    /// **值**因此被钉死为我们自己的常量（不随 `wreq-util` 补丁版漂移，且 UA/平台自洽由
    /// unlock crate 单测保证），**代价**是这批头会被提到发送序前部，不再是 Chrome 的原生顺序。
    ///
    /// **wreq 6 的 `header()` 语义变了**：5.x 是「覆盖同名」，6.x 实现是 `headers_mut().append`
    /// （其 doc 注释仍写 replaced，**以代码为准**）。本实现仍然安全，靠两条：
    /// ① [`UnlockRequest::headers`] 是 `BTreeMap` ⇒ 单次请求里每个头名只会被 `header()` 设**一次**；
    /// ② 客户端层合并默认头走 `replace_headers`（请求头**按名整体替换**掉 emulation 的默认值，
    ///    见 `wreq-6.0.0-rc.29/src/client/layer/config.rs:125`）。
    /// 二者叠加后净效果与 5.x 的覆盖一致。**但这是隐式的**，故 `browser_headers_reach_the_wire`
    /// 里加了一条「线上同名头不得出现两次」的断言——真退化成 append 时（例如日后有人给同一名字
    /// 大小写各设一次）会立刻转红，而不是安静地发出两个自相矛盾的 UA。
    ///
    /// 权衡：header **顺序**是次级信号，TLS/H2 指纹（本 crate 的主要收益）不受影响；而**值的可控性**
    /// 关系到「UA 与 sec-ch-ua 是否自洽」这条已被单测钉死的硬不变量。故取「值自持、序让步」。
    async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
        let mut current = req.url.clone();
        let mut chain: Vec<RedirectHop> = Vec::new();

        for _ in 0..=MAX_REDIRECTS {
            let mut builder = match req.method {
                HttpMethod::Get => self.client.get(&current),
                HttpMethod::Post => self.client.post(&current),
            };
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            if let Some(body) = &req.body {
                builder = builder.body(body.clone());
            }

            let sent = match tokio::time::timeout(UNLOCK_TIMEOUT, builder.send()).await {
                Err(_) => {
                    return UnlockResponse::err(format!(
                        "请求超时（{}ms）",
                        UNLOCK_TIMEOUT.as_millis()
                    ))
                }
                Ok(Err(e)) => return UnlockResponse::err(format!("请求失败: {e}")),
                Ok(Ok(r)) => r,
            };
            let resp = sent;
            let status = resp.status().as_u16();

            if (300..400).contains(&status) {
                if let Some(loc) = resp
                    .headers()
                    .get(wreq::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                {
                    chain.push(RedirectHop {
                        status,
                        location: loc.clone(),
                    });
                    // wreq 6 不再 re-export `Url`（`wreq::Url` 没了）；`http::Uri` 无相对引用解析。
                    // 直接用 `url` crate —— 与 wreq 5 的 `wreq::Url` 同一实现，也是 wreq 6 内部解
                    // `Location` 用的同一条路径（`src/client/layer/redirect/future.rs:196`），语义不变。
                    // 注意：链里存的仍是**原始 Location 串**（上面 `loc.clone()`），checker 按原样解析。
                    match url::Url::parse(&current).and_then(|b| b.join(&loc)) {
                        Ok(next) => {
                            current = next.to_string();
                            continue;
                        }
                        Err(e) => return UnlockResponse::err(format!("重定向目标非法 {loc}: {e}")),
                    }
                }
                // 30x 无 Location → 当终态（与 net-stack 同口径）。
            }

            // 终态响应头：JS 挑战识别（cf-mitigated / server）的判据来源。
            // HeaderName 本就小写规范化 → 直接照填（契约要求小写键）。
            // 多值头**取首值**（`or_insert`，非后值覆盖）：unlock 只读单值头。
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in resp.headers().iter() {
                if let Ok(s) = v.to_str() {
                    headers
                        .entry(k.as_str().to_string())
                        .or_insert_with(|| s.to_string());
                }
            }

            let (body, truncated) =
                match read_body_truncating(resp, MAX_BODY_BYTES, UNLOCK_TIMEOUT).await {
                    Ok(v) => v,
                    Err(e) => return UnlockResponse::err(e),
                };
            return UnlockResponse {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
                truncated,
                redirect_chain: chain,
                error: None,
                headers,
            };
        }
        UnlockResponse::err(format!("重定向次数超过上限（{MAX_REDIRECTS}）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;

    // ── 回环 HTTP server（stdlib，127.0.0.1；**不触碰宿主网络、不打任何真实 CF 端点**）──────

    fn spawn_server(responses: Vec<Vec<u8>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        thread::spawn(move || {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 16384];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(&resp);
                let _ = sock.flush();
            }
        });
        addr
    }

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

    /// 按 profile 构造带完整浏览器头集的 GET（复刻 checker 侧 `get_req` 的接线）。
    fn browser_get(url: &str, profile: polaris_unlock::RequestProfile) -> UnlockRequest {
        let mut req = UnlockRequest::get(url);
        for (k, v) in polaris_unlock::browser_headers(profile, HttpMethod::Get, url) {
            req = req.header(k, v);
        }
        req
    }

    // ── 解压门（本 crate 存在的**最重要**理由之一）───────────────────────────────
    //
    // 「声明了 Accept-Encoding 却解不开」不会报错，会**静默误判**：checker 拿压缩字节流做 marker
    // 匹配，全部落空 → ChatGPT 误 Ok / Gemini 误 Blocked / Netflix 误 Blocked。
    // 故：`browser::ACCEPT_ENCODING` 声明的**每一种**编码，都必须在这里被真实解压验过。

    /// 明文 canary（下列压缩块解开后都必须等于它）。
    const CANARY: &str = "UNLOCK-DECOMPRESSION-CANARY inSupportedLocation:true";

    /// `gzip` 压缩后的 [`CANARY`]（python3 `gzip.compress`）。
    const GZIP_CANARY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x0b, 0xf5, 0xf3, 0xf1, 0x77,
        0xf6, 0xd6, 0x75, 0x71, 0x75, 0xf6, 0xf7, 0x0d, 0x08, 0x72, 0x0d, 0x0e, 0xf6, 0xf4, 0xf7,
        0xd3, 0x75, 0x76, 0xf4, 0x73, 0x0c, 0x8a, 0x54, 0xc8, 0xcc, 0x0b, 0x2e, 0x2d, 0x28, 0xc8,
        0x2f, 0x2a, 0x49, 0x4d, 0xf1, 0xc9, 0x4f, 0x4e, 0x2c, 0xc9, 0xcc, 0xcf, 0xb3, 0x2a, 0x29,
        0x2a, 0x4d, 0x05, 0x00, 0xaf, 0xe3, 0xb0, 0x86, 0x34, 0x00, 0x00, 0x00,
    ];

    /// `deflate` 压缩后的 [`CANARY`]（zlib 包装 —— 服务器实际发的形态；python3 `zlib.compress`）。
    const DEFLATE_CANARY: &[u8] = &[
        0x78, 0x9c, 0x0b, 0xf5, 0xf3, 0xf1, 0x77, 0xf6, 0xd6, 0x75, 0x71, 0x75, 0xf6, 0xf7, 0x0d,
        0x08, 0x72, 0x0d, 0x0e, 0xf6, 0xf4, 0xf7, 0xd3, 0x75, 0x76, 0xf4, 0x73, 0x0c, 0x8a, 0x54,
        0xc8, 0xcc, 0x0b, 0x2e, 0x2d, 0x28, 0xc8, 0x2f, 0x2a, 0x49, 0x4d, 0xf1, 0xc9, 0x4f, 0x4e,
        0x2c, 0xc9, 0xcc, 0xcf, 0xb3, 0x2a, 0x29, 0x2a, 0x4d, 0x05, 0x00, 0xac, 0x8d, 0x11, 0xb0,
    ];

    /// `br`（brotli）压缩后的 [`CANARY`]（python3 `brotli.compress`）。
    const BR_CANARY: &[u8] = &[
        0x1b, 0x33, 0x00, 0xf8, 0xe5, 0x1b, 0x5b, 0xd7, 0x38, 0xc6, 0xf1, 0x84, 0x45, 0x94, 0x32,
        0x8c, 0xe9, 0x68, 0x43, 0x14, 0xc1, 0xe7, 0x20, 0xbd, 0x06, 0x2b, 0x0c, 0xbd, 0xa4, 0x27,
        0xa3, 0xc4, 0x51, 0x6d, 0x10, 0x0f, 0x0a, 0x27, 0xf7, 0xc2, 0x7d, 0x91, 0x90, 0x12,
    ];

    /// `zstd` 压缩后的 [`CANARY`]（python3 `compression.zstd.compress`）。
    const ZSTD_CANARY: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x34, 0xa1, 0x01, 0x00, 0x55, 0x4e, 0x4c, 0x4f, 0x43, 0x4b,
        0x2d, 0x44, 0x45, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53, 0x49, 0x4f, 0x4e, 0x2d,
        0x43, 0x41, 0x4e, 0x41, 0x52, 0x59, 0x20, 0x69, 0x6e, 0x53, 0x75, 0x70, 0x70, 0x6f, 0x72,
        0x74, 0x65, 0x64, 0x4c, 0x6f, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x3a, 0x74, 0x72, 0x75,
        0x65,
    ];

    /// 编码名 → 该编码压缩过的 canary。**必须覆盖 `ACCEPT_ENCODING` 声明的全集**。
    fn canary_for(token: &str) -> Option<&'static [u8]> {
        match token {
            "gzip" => Some(GZIP_CANARY),
            "deflate" => Some(DEFLATE_CANARY),
            "br" => Some(BR_CANARY),
            "zstd" => Some(ZSTD_CANARY),
            _ => None,
        }
    }

    #[tokio::test]
    async fn declared_accept_encoding_is_decodable_end_to_end() {
        // **跨 crate 不变量**（`browser::ACCEPT_ENCODING` 的文档点名本门）：
        // 声明的每种编码都必须真能解开，否则静默误判。
        //
        // 变异验证：
        //  - 从 Cargo.toml 摘掉 `brotli` feature → br 那轮拿回压缩字节 ≠ CANARY → 转红。
        //  - 给 ACCEPT_ENCODING 加一个没有解码器的编码（如 `sdch`）→ canary_for 返 None → 转红
        //    （**这条是关键**：它保证「新增声明」不会悄悄绕过本门）。
        for token in polaris_unlock::browser::accept_encoding_tokens() {
            let blob = canary_for(token).unwrap_or_else(|| {
                panic!(
                    "ACCEPT_ENCODING 声明了 `{token}`，但本门没有对应的解压用例 —— \
                     要么该编码传输层解不开（会静默误判），要么补上 canary 再声明"
                )
            });
            let addr = spawn_server(vec![http_response(
                "200 OK",
                &[("Content-Encoding", token), ("Content-Type", "text/html")],
                blob,
            )]);
            let c = UnlockClient::direct().expect("建 client");
            let resp = c
                .request(&UnlockRequest::get(format!("http://{addr}/{token}")))
                .await;
            assert_eq!(resp.status, 200, "`{token}` 请求应成功");
            assert_eq!(
                resp.body, CANARY,
                "`{token}` 未被解压 —— body 是压缩字节流，checker 的 marker 匹配会全部落空（静默误判）"
            );
        }
    }

    // ── UnlockHttp 契约门（与原 reqwest 实现同口径，换栈后必须仍成立）──────────────

    #[tokio::test]
    async fn never_errs_on_network_failure_it_reports_status_zero() {
        // 契约：永不 panic / 永不 Err —— 失败落 status=0 + error，由 checker 兜底 Timeout。
        // 若改成 unwrap → 一个检测项挂掉会带崩整轮齐射。
        let c = UnlockClient::direct().expect("建 client");
        let resp = c
            .request(&UnlockRequest::get("http://127.0.0.1:1/dead"))
            .await;
        assert_eq!(resp.status, 0, "传输失败必须 status=0");
        assert!(resp.error.is_some());
        assert!(!resp.reachable(), "status=0 不得判为可达");
    }

    #[tokio::test]
    async fn records_redirect_chain_without_following_automatically() {
        // checker 判据之一是完整 redirect_chain（Claude 靠最终 URL 落在哪个域判定）。
        let addr = spawn_server(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            http_response("200 OK", &[], b"{\"ok\":true}"),
        ]);
        let c = UnlockClient::direct().expect("建 client");
        let resp = c
            .request(&UnlockRequest::get(format!("http://{addr}/a")))
            .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.redirect_chain.len(), 1, "逐跳链必须记录");
        assert_eq!(resp.redirect_chain[0].status, 302);
        assert_eq!(resp.redirect_chain[0].location, "/next");
    }

    #[tokio::test]
    async fn set_cookie_is_carried_across_manual_redirect_hops() {
        // **cookie jar 的唯一自动化证据**（`base_builder()` 的 `.cookie_store(true)`）。
        //
        // 手动逐跳跟随最容易漏的就是这条：首跳 `Set-Cookie` 之后，第二跳若不回带 `Cookie`，
        // 与真 Chrome 的行为差一条独立于 TLS/头指纹的信号（Netflix 判定异常的候选根因之一）。
        //
        // 变异验证（本批真跑过）：删掉 `.cookie_store(true)` → 第二跳线上无 `cookie:` → 转红。
        //
        // 不用 `spawn_server`：它只回放固定字节，而本门要**回显第二跳的请求原文**才看得见 `Cookie`。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            // 第 1 跳：302 + Set-Cookie
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /next\r\nSet-Cookie: unlock_probe=jar-works; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
            // 第 2 跳：回显请求原文
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let n = sock.read(&mut buf).unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(&http_response("200 OK", &[], raw.as_bytes()));
            }
        });

        let c = UnlockClient::direct().expect("建 client");
        let resp = c
            .request(&UnlockRequest::get(format!("http://{addr}/a")))
            .await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.redirect_chain.len(),
            1,
            "仍须逐跳记录（jar 不改这条契约）"
        );
        let wire = resp.body.to_lowercase();
        assert!(
            wire.contains("cookie: unlock_probe=jar-works"),
            "第二跳没带上首跳的 Set-Cookie —— 与真 Chrome 的跳转行为不一致。实收:\n{wire}"
        );
    }

    #[tokio::test]
    async fn captures_cf_mitigated_header_for_challenge_detection() {
        // 挑战识别（`polaris_unlock::challenge::classify`）的**主判据来源**：
        // 终态响应头必须被真实填充、键小写、可大小写不敏感取。漏填 → 挑战永远识别不出来 → 误报回归。
        let addr = spawn_server(vec![http_response(
            "403 Forbidden",
            &[("cf-mitigated", "challenge"), ("Server", "cloudflare")],
            b"<html>Just a moment...</html>",
        )]);
        let c = UnlockClient::direct().expect("建 client");
        let resp = c
            .request(&UnlockRequest::get(format!("http://{addr}/challenge")))
            .await;
        assert_eq!(resp.status, 403);
        assert_eq!(resp.header("cf-mitigated"), Some("challenge"));
        assert_eq!(
            resp.header("CF-Mitigated"),
            Some("challenge"),
            "取头须大小写不敏感"
        );
        assert_eq!(resp.header("server"), Some("cloudflare"));
        assert!(resp.headers.contains_key("cf-mitigated"), "键须小写规范化");
        // 端到端：这个响应必须真被分类器判成挑战（不只是头拿到了）。
        assert!(
            polaris_unlock::classify(&resp).is_some(),
            "捕获的头必须能让 classify 命中——否则挑战识别在生产路径上是断的"
        );
    }

    #[tokio::test]
    async fn truncates_body_rather_than_failing() {
        // 与订阅相反：解锁只看特征串，截断的 body 仍可判 → 截断 + 标记，不报错。
        let body = vec![b'y'; MAX_BODY_BYTES + 100];
        let addr = spawn_server(vec![http_response("200 OK", &[], &body)]);
        let c = UnlockClient::direct().expect("建 client");
        let resp = c
            .request(&UnlockRequest::get(format!("http://{addr}/big")))
            .await;
        assert_eq!(resp.status, 200);
        assert!(resp.truncated, "超限应标记 truncated");
        assert_eq!(resp.body.len(), MAX_BODY_BYTES);
        assert!(resp.error.is_none(), "截断不是错误");
    }

    // ── 边界门：wreq 不得泄漏到其它 crate ──────────────────────────────────────

    #[test]
    fn wreq_is_declared_only_by_this_crate() {
        // 任务硬要求：**指纹客户端不得泄漏到其它 HTTP 路径**（订阅拉取 / 内核下载 / 更新 / WARP
        // 都不需要伪装，也不该承担 BoringSSL 构建链与模板漂移的风险）。
        //
        // 独立 crate 只是让泄漏「需要显式改 Cargo.toml」；本门让它**直接转红**。
        // 变异验证：给 `src-tauri/Cargo.toml` 加一行 `wreq = "5"` → 本门转红。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("定位 workspace 根")
            .to_path_buf();

        let mut manifests = vec![root.join("src-tauri/Cargo.toml")];
        for entry in std::fs::read_dir(root.join("crates")).expect("读 crates/") {
            let dir = entry.expect("目录项").path();
            let m = dir.join("Cargo.toml");
            if m.is_file() {
                manifests.push(m);
            }
        }

        let mut offenders = Vec::new();
        for m in manifests {
            // 本 crate 自己是唯一合法声明处。
            if m.parent().is_some_and(|p| p.ends_with("unlock-transport")) {
                continue;
            }
            let text = std::fs::read_to_string(&m).expect("读 manifest");
            for line in text.lines() {
                let l = line.trim_start();
                if l.starts_with('#') {
                    continue; // 注释里提到 wreq 不算声明
                }
                if l.starts_with("wreq") || l.contains("\"wreq\"") {
                    offenders.push(format!("{}: {l}", m.display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "wreq 只允许由 polaris-unlock-transport 声明（指纹客户端不得泄漏到其它 HTTP 路径）。\
             越界声明:\n{}",
            offenders.join("\n")
        );
    }

    // ── 浏览器身份同源门（transport 这一半）──────────────────────────────────
    //
    // 「TLS 指纹说 149、UA 说 137」比单纯陈旧更糟：**自相矛盾**是更强的 bot 信号。
    // 下面两扇门把 emulation 这一处焊死在 `CHROME_MAJOR` 上；UA/`sec-ch-ua` 那半边在
    // `polaris_unlock::browser` 的 `sec_ch_ua_major_version_matches_ua`。

    #[test]
    fn emulation_matches_the_pinned_chrome_major() {
        // 派生表写错行（如 `149 => Profile::Chrome137`）在编译期是合法的 —— 本门抓它。
        // `Profile` 的 Debug 名即变体名（`Chrome149`），是 wreq-util 暴露版本号的唯一公开途径
        //（模板的 `default_headers` 不可从外部读到）。
        assert_eq!(
            format!("{EMULATION:?}"),
            format!("Chrome{CHROME_MAJOR}"),
            "指纹模板与 CHROME_MAJOR 不同版 —— TLS 指纹与 UA 会自相矛盾"
        );
    }

    /// 解析派生表的一行 `NNN => wreq_util::Profile::ChromeMMM,` → `(NNN, MMM)`。
    fn emulation_mapping_row(line: &str) -> Option<(u32, u32)> {
        let (lhs, rhs) = line.trim().trim_end_matches(',').split_once("=>")?;
        let key = lhs.trim().parse().ok()?;
        let variant = rhs.trim().strip_prefix("wreq_util::Profile::Chrome")?;
        Some((key, variant.parse().ok()?))
    }

    #[test]
    fn emulation_variant_is_derived_not_hardcoded() {
        // **本门是「只升 emulation 不升 UA」这个变异的唯一自动化拦截点**：`chrome_emulation`
        // 的派生让 emulation 没有独立的版本字面量，但绕过派生（直接 `.emulation(Profile::Chrome149)`）
        // 仍然编译得过。本门扫生产代码，只放行派生表里左右同号的行。
        //
        // **两种写法都得拦**：wreq-util 3.x 把枚举更名为 `Profile`，但 `Emulation` 上仍留着
        // 一批 `pub const ChromeNNN: Profile` 别名 —— 只扫 `Profile::Chrome` 会让
        // `wreq_util::Emulation::Chrome149` 这条**编译得过的**旁路静默逃逸（门变成空门）。
        //
        // 变异验证（本批真跑过，见交付说明）：
        //  - `base_builder()` 改成 `.emulation(wreq_util::Profile::Chrome149)` → 转红；
        //  - 同上但写 `wreq_util::Emulation::Chrome149`（3.x 别名）→ 转红；
        //  - 派生表某行改成 `149 => wreq_util::Profile::Chrome137` → 转红（左右不同号）。
        const SRC: &str = include_str!("lib.rs");
        // 只扫生产段（`#[cfg(test)]` 之后是本测试模块，里面的字符串/注释必然含该字面量）。
        let prod = SRC
            .split("#[cfg(test)]")
            .next()
            .expect("源码应含测试模块分界");
        let offenders: Vec<String> = prod
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("Profile::Chrome") || l.contains("Emulation::Chrome"))
            .filter(|(_, l)| !l.trim_start().starts_with("//")) // 文档/注释里提到不算
            .filter(|(_, l)| !matches!(emulation_mapping_row(l), Some((k, v)) if k == v))
            .map(|(i, l)| format!("{}: {}", i + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "`Profile::Chrome…` / `Emulation::Chrome…` 只允许出现在 `chrome_emulation` 的派生表里\
             （左右同号），否则指纹版本会脱离 CHROME_MAJOR 独立漂移。越界行:\n{}",
            offenders.join("\n")
        );
    }

    // ── TLS 指纹门（本 crate 的**主要收益**必须有牙）────────────────────────────
    //
    // 前面所有门在「删掉 `.emulation(…)`」这个变异下**全绿** —— 因为头集是我们自己显式设的、
    // 解压是 feature 决定的，都与指纹无关。也就是说：没有本门，Phase-2 的核心交付物是**无门**的，
    // 有人误删 emulation 会静默退回 rustls 时代的可识别形态（而 CI 全绿）。
    //
    // 本门在**线级**验：让 client 向回环 TCP 发起 HTTPS，抓下**首个 ClientHello 字节**，
    // 断言它带 Chrome 的结构特征。握手随后会失败（对端不是真 TLS server）——不影响，本门只看已抓字节。
    // **不触碰宿主网络、不打任何真实端点。**

    /// GREASE 值（RFC 8701）：两字节相同且低半字节为 `0xa`（`0x0a0a`/`0x1a1a`/…/`0xfafa`）。
    /// Chrome/BoringSSL **必发**；rustls **从不发**。这是区分二者最稳的结构特征。
    fn is_grease(v: u16) -> bool {
        let [hi, lo] = v.to_be_bytes();
        hi == lo && (hi & 0x0f) == 0x0a
    }

    /// 从 ClientHello 记录里解析出 `(cipher_suites, extension_types)`。
    fn parse_client_hello(buf: &[u8]) -> Option<(Vec<u16>, Vec<u16>)> {
        // TLS record: type(1)=0x16 version(2) length(2) | handshake: type(1)=0x01 length(3)
        if buf.len() < 9 || buf[0] != 0x16 || buf[5] != 0x01 {
            return None;
        }
        let mut i = 9; // 跳过 record(5) + handshake header(4)
        i += 2 + 32; // client_version + random
        let sid_len = *buf.get(i)? as usize;
        i += 1 + sid_len;
        let cs_len = u16::from_be_bytes([*buf.get(i)?, *buf.get(i + 1)?]) as usize;
        i += 2;
        let mut ciphers = Vec::new();
        for c in buf.get(i..i + cs_len)?.chunks_exact(2) {
            ciphers.push(u16::from_be_bytes([c[0], c[1]]));
        }
        i += cs_len;
        let comp_len = *buf.get(i)? as usize;
        i += 1 + comp_len;
        let ext_total = u16::from_be_bytes([*buf.get(i)?, *buf.get(i + 1)?]) as usize;
        i += 2;
        let end = i + ext_total;
        let mut exts = Vec::new();
        while i + 4 <= end.min(buf.len()) {
            let ty = u16::from_be_bytes([buf[i], buf[i + 1]]);
            let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
            exts.push(ty);
            i += 4 + len;
        }
        Some((ciphers, exts))
    }

    #[tokio::test]
    async fn client_hello_carries_chrome_grease_fingerprint() {
        // **变异验证**：把 `base_builder()` 里的 `.emulation(EMULATION)` 删掉
        // → ClientHello 不再带 GREASE → 本门转红。这是「指纹伪装真的生效了」的唯一自动化证据。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                sock.set_read_timeout(Some(Duration::from_millis(800))).ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while let Ok(n) = sock.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    // 记录头说了长度，收满即停。
                    if buf.len() >= 5 {
                        let rec = 5 + u16::from_be_bytes([buf[3], buf[4]]) as usize;
                        if buf.len() >= rec {
                            break;
                        }
                    }
                }
                let _ = tx.send(buf);
            }
        });

        let c = UnlockClient::direct().expect("建 client");
        // https:// 打回环 → 会发真 ClientHello，随后握手失败（对端非 TLS server）。
        let _ = c
            .request(&UnlockRequest::get(format!(
                "https://127.0.0.1:{}/x",
                addr.port()
            )))
            .await;

        let hello = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("应捕获到 ClientHello 字节");
        let (ciphers, exts) =
            parse_client_hello(&hello).expect("捕获的字节应是可解析的 TLS ClientHello");

        assert!(
            ciphers.first().is_some_and(|c| is_grease(*c)),
            "Chrome/BoringSSL 的首个 cipher suite 必为 GREASE；实得 {:04x?}（rustls 形态 = 指纹伪装没生效）",
            &ciphers[..ciphers.len().min(4)]
        );
        assert!(
            exts.iter().any(|e| is_grease(*e)),
            "Chrome 必发 GREASE 扩展；实得扩展 {exts:04x?}"
        );
        // ALPN(0x0010)：Chrome 宣告 h2 —— 与 WARP 那条腿刻意钉 HTTP/1.1 的形态正相反。
        assert!(
            exts.contains(&0x0010),
            "应宣告 ALPN 扩展；实得扩展 {exts:04x?}"
        );
    }

    #[tokio::test]
    async fn browser_headers_reach_the_wire() {
        // (c) 的**线级**门：头集不只是构造出来了，得真发出去。
        // 回显 server 把收到的请求原样写回 body，断言关键头在线上出现。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let n = sock.read(&mut buf).unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = sock.write_all(&http_response("200 OK", &[], raw.as_bytes()));
            }
        });

        let url = format!("http://{addr}/probe");
        let req = browser_get(&url, polaris_unlock::RequestProfile::Navigate);
        let c = UnlockClient::direct().expect("建 client");
        let resp = c.request(&req).await;
        let wire = resp.body.to_lowercase();

        for expect in [
            "accept-encoding: gzip, deflate, br, zstd",
            "accept-language: en-us,en;q=0.9",
            "sec-ch-ua-platform: \"windows\"",
            "sec-fetch-mode: navigate",
            "upgrade-insecure-requests: 1",
        ] {
            assert!(
                wire.contains(&expect.to_lowercase()),
                "线上缺 `{expect}`，实收:\n{wire}"
            );
        }
        // UA 必须是我们钉的 Windows Chrome（版本 = CHROME_MAJOR），**不是** emulation 默认的 macOS UA
        //（值自持不随 wreq-util 补丁版漂移——见 impl 文档「头集与 emulation 的关系」）。
        // 版本号从 CHROME_MAJOR 取，**不写字面量**：否则这里就成了第四处要手动同步的版本钉子。
        assert!(
            wire.contains("windows nt 10.0") && wire.contains(&format!("chrome/{CHROME_MAJOR}")),
            "UA 必须是我们钉的 Windows Chrome {CHROME_MAJOR}，实收:\n{wire}"
        );

        // **wreq 6 的 `header()` 是 append 不是 replace**（见 impl 文档）。若哪天覆盖失效，
        // 线上会同时出现 emulation 默认的 macOS UA 与我们的 Windows UA —— 上面那条 `contains`
        // **抓不到**（它只要求我们的 UA 在场）。故逐名清点：任何一个我们设过的头出现两次即转红。
        //
        // 变异验证（本批真跑过）：在 `request()` 的 header 循环后再 `.header("user-agent", "x")`
        // 追加一条 → 本断言转红，而其余所有门全绿。
        let dup: Vec<&str> = [
            "user-agent",
            "accept",
            "accept-encoding",
            "accept-language",
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "priority",
        ]
        .into_iter()
        .filter(|name| {
            wire.lines()
                .filter(|l| l.split_once(':').is_some_and(|(k, _)| k.trim() == *name))
                .count()
                > 1
        })
        .collect();
        assert!(
            dup.is_empty(),
            "线上出现重名头 {dup:?} —— emulation 默认值没被覆盖掉而是被追加，\
             对端会看到两个自相矛盾的值（比不发更强的 bot 信号）。实收:\n{wire}"
        );
    }
}
