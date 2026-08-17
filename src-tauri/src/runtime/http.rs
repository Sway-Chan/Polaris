//! 传输层单点 —— **整个 workspace 唯一的真实 HTTP/TLS 客户端**。
//!
//! 各 domain crate 只声明窄 trait（端口），真实传输在此注入（见 `runtime.rs` 模块文档的注入架构）。
//! 本模块用**一个** [`reqwest::Client`] 适配四个既有窄 trait：
//!
//! | trait | 所有者 crate | 本模块适配点 |
//! |---|---|---|
//! | [`HttpClient`] | net-stack（`safe_redirect.rs`） | [`HttpRuntime`]（manual redirect + 流式限长） |
//! | [`UpdateDownloader`] | updater（`traits.rs`） | [`CoreDownloader`]（**唯一**下载适配器） |
//! | ~~`UnlockHttp`~~ | unlock（`http.rs`） | **已迁出** → `polaris-unlock-transport`（wreq 指纹伪装，见适配③处注释） |
//! | [`WarpHttp`] | mesh（`warp_http.rs`） | [`HttpRuntime`]（json/status 双语义） |
//! | [`DohPost`](polaris_dns_race::DohPost) | dns-race（`query.rs`） | [`HttpRuntime`]（C11 节点域名竞速的 DoH 上游） |
//!
//! 另补 [`SystemDnsLookup`]：net-stack `DnsLookup` 的**首个生产实现**（此前全仓仅测试 mock，
//! 意味着 SSRF guard 在生产路径上根本没有解析器可用）。
//!
//! # 编排不在这里（上游 双份编排的病根）
//!
//! 上游的 `core-downloader.ts` 与 `UpdateService.ts` 各写了约 170 行同构下载编排，两文件注释
//! 互指重复。Rust 侧编排**已经各归其位**：updater 守 staged 周期、net-stack 守 SSRF + 逐跳重定向。
//! 故本模块**只有传输**，且把 `UpdateDownloader` doc 划给「实现侧」的那几件事
//! （停滞看门狗 / 镜像回退 / 403 限流分类 / 16MiB 闸 / 15s 超时 / Content-Length 完整性）
//! **全部收在 [`CoreDownloader`] 这一个适配器里** —— 订阅路径的 [`HttpClient`] 适配器不得复制它们。
//!
//! # rustls provider：一个「编译过 ≠ 能跑」的陷阱（实证）
//!
//! `reqwest` 的 `rustls` feature 会拉 `aws-lc-rs`（需 cmake/NASM，破坏交叉编译干净），故本仓用
//! `rustls-no-provider` + 手动选 `ring`。代价是 **reqwest 不再自带 provider**：
//! `reqwest-0.13.4/src/async_impl/client.rs:2482 default_rustls_crypto_provider()` 在
//! 未安装 provider 时**直接 `panic!`** —— 而且是在 `Client::build()` 运行期，不是编译期。
//!
//! 即：漏掉 [`install_ring_provider`] 的代码**编译完全通过、类型完全正确、第一次建 client 就炸**。
//! 这正是 §K7.1「组合面无门」的形态。故 [`HttpRuntime::new`] 无条件先装 provider，
//! 并由 `http_runtime_builds_a_real_client_without_panicking` 钉死（该测试若删掉 provider 安装即 panic 转红）。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use polaris_mesh::warp_http::{WarpHttp, WarpHttpMethod, WarpHttpRequest, WarpHttpResponse};
use polaris_net_stack::safe_redirect::{FetchInit, HttpClient, MinimalResponse};
use polaris_net_stack::ssrf::DnsLookup;
use polaris_updater::traits::{DownloadError, UpdateDownloader};

// ── 传输层常量（单点定义，各消费族不得自造第二份）───────────────────────────────

/// 应用自标识 UA（= 上游 `APP_USER_AGENT`）。
///
/// **与订阅 UA 是两回事**：订阅走 `net_stack::subscription::default_subscription_user_agent`
/// （中性 `Polaris/<ver>`，由调用方按订阅偏好覆盖）；本 UA 用于 GitHub API / 资源下载。
/// **勿与 mesh 的 `WARP_USER_AGENT` 共用** —— 那个是伪装 okhttp 的特例。
pub fn app_user_agent() -> String {
    format!("Polaris/{}", env!("CARGO_PKG_VERSION"))
}

/// 响应头超时（连接 + 首字节）。= Polaris 下载链路 15s。
/// **不是**整请求超时：大文件下载正常会超过 15s，整请求超时会把正常下载判死。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// 停滞看门狗：body 流式读取时**两个 chunk 之间**的最大间隔（= 上游 `createIdleTimeout` 30s）。
/// 对付 slow-loris / 半死连接：整请求超时管不了「一直有数据但极慢」，idle 才管得了「彻底没数据」。
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// **内存型**下载的体积硬闸（16 MiB）。核二进制 ~10MiB 量级；超此即拒，防 OOM。
///
/// # 它是「内存闸」，不是「文件闸」
///
/// 两条内核腿（手动换核 / 自动换核）走的是 `download` → `Vec<u8>` → 解归档，字节全程在堆上，
/// 故上限就该按**内存**定。App 安装包腿改成流式落盘后内存占用与包体积解耦，再拿 16 MiB 卡它
/// 只会把所有正常安装包拒之门外 —— 那条腿的闸值改由调用方按「清单声明大小 + 裕度」注入
/// （见 [`CoreDownloader::with_max_bytes`]）。
///
/// `pub(crate)`：三个 `updater_downloader` 调用点里的两条内核腿要**逐字传这个值**
/// （语义与形参化之前完全一致）；各写一份字面量必然漂移。
pub(crate) const MAX_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;

/// 下载路径的重定向上限（GitHub release 资产必然 302 到 objects.githubusercontent.com）。
const MAX_DOWNLOAD_REDIRECTS: usize = 5;

/// WARP 请求超时（= 上游 `WarpService` 15s）。
const WARP_TIMEOUT: Duration = Duration::from_secs(15);

/// WARP 响应体截断上限（register 错误信息 ~200B / unregister 的 body-code-1020 检测需前 512B）。
const WARP_MAX_BODY_BYTES: usize = 8 * 1024;

// ── rustls crypto provider ────────────────────────────────────────────────────

/// 安装 `ring` 作为 rustls 默认 CryptoProvider（进程级，幂等）。
///
/// 见模块文档「rustls provider 陷阱」：不装 → `Client::build()` 运行期 panic。
fn install_ring_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            // 已由他处安装（例如测试进程内先建过 client）：沿用既有，不是错误。
            log::debug!("rustls CryptoProvider 已安装，沿用既有");
        }
    });
}

// ── WARP 注册专用 client（TLS 指纹规避）─────────────────────────────────────────

/// 建 WARP 注册专用 client：**TLS1.2-only + HTTP/1.1-only**，对齐 上游 `WarpService` 的 node-`https`
/// （`minVersion=maxVersion=TLSv1.2`，默认走 HTTP/1.1、不提供 h2）指纹规避。
///
/// # 为什么单独一个 client（不复用共享 `client`）
///
/// 共享 client 走 rustls-default：ClientHello 提供 **TLS1.3（supported_versions 含 0x0304）+ h2 ALPN**。
/// `api.cloudflareclient.com` 的 Cloudflare WAF 按 TLS 指纹判「非浏览器/自动化」→ 返 **1020/403**
/// （见 mesh crate `warp_http.rs` 文档）。用 rustls 现有能力复刻 上游的**同一形态**：
/// - [`tls_version_max`](reqwest::ClientBuilder::tls_version_max)`(TLS_1_2)` → reqwest 把 `rustls::ALL_VERSIONS`
///   retain 到 ≤TLS1.2 → ClientHello 不再宣告 TLS1.3；
/// - [`http1_only`](reqwest::ClientBuilder::http1_only) → ALPN 只报 `http/1.1`（不报 `h2`）。
///
/// # 忠实边界：收窄形态，**非**字节级 okhttp JA3
///
/// rustls 只带 AEAD cipher（无 CBC）、扩展集固定，**无法**字节级仿 okhttp 的 JA3。但 oracle 实证 上游 **本身也没仿**：
/// 它用 node-OpenSSL 的 TLS1.2 ClientHello（≠ okhttp 真 JA3，okhttp=Android Conscrypt/BoringSSL）就过了 CF，
/// 说明 CF 判的是**粗形态**（TLS1.2/HTTP1.1 vs TLS1.3/h2），非精确 JA3 白名单。故收窄形态在忠实迁移口径上足矣。
/// **残余风险**：rustls 的 TLS1.2 指纹仍是第三种（既非 node-OpenSSL 亦非 okhttp）——若 CF 恰把它列黑，则仅钉版本不够。
/// 这只能真打 `api.cloudflareclient.com` 验证（见 WarpHttp impl 文档的**真机门**）。失败形态是明确的 403+body-1020，
/// 已被 `classify_deregister_result` 分类，不会伪装成成功。
///
/// no_proxy + redirect-none 同共享 client：WARP 注册须直连 CF（自举友好，且 mesh 契约「无重定向」）。
/// **不设默认 UA**：okhttp UA 由 mesh 层随请求头逐条下发（`warp_http.rs` 的 `build_reg_headers`）。
fn build_warp_client() -> Result<reqwest::Client, String> {
    install_ring_provider();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .http1_only()
        .tls_version_min(reqwest::tls::Version::TLS_1_2)
        .tls_version_max(reqwest::tls::Version::TLS_1_2)
        .build()
        .map_err(|e| format!("建 WARP 客户端失败: {e}"))
}

// ── DnsLookup 生产实现 ─────────────────────────────────────────────────────────

/// 系统解析器（`tokio::net::lookup_host`）—— net-stack [`DnsLookup`] 的**生产实现**。
///
/// 此前全仓只有测试 mock：意味着 `assert_host_allowed` 的 SSRF guard 在生产路径上
/// **没有解析器可注入**，H1（DNS rebinding）防线是「逻辑在、接线不在」。本类型接上它。
///
/// 端口传 0：只要 A/AAAA 记录，不实际连接。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDnsLookup;

impl DnsLookup for SystemDnsLookup {
    fn lookup_all(&self, host: &str) -> impl Future<Output = Result<Vec<String>, String>> + Send {
        let host = host.to_string();
        async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| format!("DNS 解析失败 {host}: {e}"))?;
            // 逐 IP 交给 guard 判定（**全部**返回，不只首个：rebinding 常把恶意 IP 藏在第二条）。
            let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            if ips.is_empty() {
                return Err(format!("DNS 解析 {host} 无结果"));
            }
            Ok(ips)
        }
    }
}

// ── 传输层单点 ────────────────────────────────────────────────────────────────

/// 真实 HTTP 客户端（进程内单实例，经 `AppRuntime` 注入各 command）。
///
/// # 为什么 redirect 关死
///
/// 四个消费族**全部**要 manual redirect 语义：
/// - net-stack `HttpClient`：doc 明写「不得自动跟随（须原样返回 30x + Location）」——
///   自动跟随会让首 URL 过 guard 后被跳到内网而不复检 = SSRF 绕过；
/// - unlock：要逐跳 `redirect_chain`（判定依据之一）；
/// - WARP：无重定向；
/// - 下载：要自管链以便镜像回退。
///
/// 裁决文档设想的「同一 client per-request 关/开 redirect」在 reqwest 上**不成立**
/// （`redirect::Policy` 只在 `ClientBuilder` 上，`RequestBuilder` 无 per-request 覆盖）。
/// 故取「client 全局关 redirect + 需要跟随者自己跑循环」——**唯一**需要跟随的是
/// [`CoreDownloader`]，而它本就要为镜像回退跑自己的循环。仍是一个 client。
///
/// # 为什么禁用代理
///
/// 本 App **自己就是设置系统代理的那一方**。若 reqwest 继承系统/环境代理，订阅拉取会绕回
/// 我们自己的核 —— 起核前拉订阅直接死锁（鸡生蛋）。故 `no_proxy()` + 编译期关掉
/// `system-proxy` feature 双保险。确需经代理的路径（订阅 `viaProxy`）走
/// [`HttpRuntime::via_local_proxy`] 显式建第二个 client，**显式优于隐式**。
pub struct HttpRuntime {
    client: reqwest::Client,
    /// WARP 注册专用 client（**TLS1.2-only + HTTP/1.1-only**）。
    ///
    /// 共享 `client` 走 rustls-default（ClientHello 提供 TLS1.3 + h2 ALPN），会被 `api.cloudflareclient.com`
    /// 的 Cloudflare WAF 按 TLS 指纹判「自动化」→ **1020/403**。此 client 用 rustls 现有能力收窄 ClientHello
    /// 形态、对齐 上游 `WarpService` 的 node-`https`（TLS1.2 pin + HTTP/1.1）规避。见 [`build_warp_client`]。
    ///
    /// **仅** [`warp_send`] 用 —— 订阅拉取 / 内核下载 / 解锁 / 更新等其它消费族继续走共享 `client`（它们不面对 CF WAF，
    /// 且钉 TLS1.2 会牺牲这些路径本可用的 TLS1.3/h2）。隔离由 `warp_send(self.warp_client()?, ..)` 单点保证。
    ///
    /// # 为什么**惰性**（`OnceLock` 而非构造期就建）
    ///
    /// [`Self::via_local_proxy`] 是**测速热路径**（每个被测节点一次），而它此前每次都白建一个 rustls client：
    /// reqwest 0.13.4 在 `build()` 里调 `rustls_platform_verifier::Verifier::new`，
    /// **Linux 走 `rustls_native_certs::load_native_certs()`，每个 client 读一次系统信任库**
    /// （`rustls-platform-verifier-0.7.0/src/verification/others.rs:88-100`，十几到几十毫秒）；
    /// macOS（`apple.rs:59-66`）/ Windows（`windows.rs:557-564`）只存字段不加载证书，便宜。
    /// 即在 Mac/Win 上这不是主要耗时，但**在任何平台上都是纯浪费** —— WARP 注册与测速毫无关系。
    ///
    /// **代价（如实登记）**：建这个 client 的错误（rustls 配置非法）此前在 [`Self::new`] 处冒泡、
    /// App 起不来就报错退出；改惰性后推迟到**首次 WARP 请求**才冒泡。那条路径本就返回 `Result<_, String>`
    /// 且有明确失败形态，故不会被吞；反过来说，WARP 客户端建不出来也不再拖垮整个 App 启动。
    warp_client: std::sync::OnceLock<reqwest::Client>,
}

impl HttpRuntime {
    /// 建传输层单点。
    ///
    /// # Errors
    ///
    /// TLS 后端初始化失败（`rustls` 配置非法）。**不 panic**：起不来 App 也要能报错退出。
    pub fn new() -> Result<Self, String> {
        install_ring_provider();
        let client = reqwest::Client::builder()
            // 见类型文档：四个消费族全要 manual redirect。
            .redirect(reqwest::redirect::Policy::none())
            // 见类型文档：绝不继承我们自己设的系统代理。
            .no_proxy()
            .user_agent(app_user_agent())
            .build()
            .map_err(|e| format!("建 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            client,
            warp_client: std::sync::OnceLock::new(),
        })
    }

    /// 建传输层单点，并把指定 host 的 DNS 解析**钉死**到给定 socket 地址。
    ///
    /// 生产用途：DNS 固定（防解析漂移 / 内部服务寻址）。
    /// 生产组合面门用途：让**真实** reqwest client 连到回环测试服务器，而 SSRF guard 仍对
    /// **真实公网 hostname** 走真实 `SystemDnsLookup` 判定 —— 二者分层（guard 判 hostname、
    /// 传输落点由 client 决定），故这不是「绕过 guard」。
    ///
    /// # Errors
    ///
    /// TLS 后端初始化失败。
    pub fn with_resolve_overrides(
        overrides: &[(&str, std::net::SocketAddr)],
    ) -> Result<Self, String> {
        install_ring_provider();
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(app_user_agent());
        for (host, addr) in overrides {
            builder = builder.resolve(host, *addr);
        }
        let client = builder
            .build()
            .map_err(|e| format!("建 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            client,
            warp_client: std::sync::OnceLock::new(),
        })
    }

    // ── 经本机 sing-box 入站的两个构造器：**scheme 必须与入站类型对上** ──────────────
    //
    // `config-engine/builder/inbounds.rs` 建的本机入站有三种，**不是同一种协议**：
    //   | 入站 | type | 消费者 |
    //   |---|---|---|
    //   | `mixed-in`            | `mixed`（HTTP+SOCKS 同口，按首字节分流） | 测速回退腿 / ipinfo 出口探测 |
    //   | `probe-in-k` / `probe-{direct,proxy}-in` | **`http`（纯 HTTP）** | 测速探测池（`speedtest.rs`） |
    //   | `update-in`           | **`socks`（纯 SOCKS）**                  | 订阅 viaProxy / icon 远端代理 |
    //
    // 故**不能**用一个 scheme 通吃：socks5 打不通 `probe-in-k`（纯 http），http 打不通 `update-in`
    // （纯 socks）。两个构造器按消费端入站类型分别取用 —— 选错的后果都是真机才可见的静默失效
    // （测速全超时 / 订阅更新恒失败），故这里把对应关系写死在文档里。

    /// 经本机 **HTTP** 入站的 client（`mixed-in` / `probe-in-k` / `probe-*-in`）。
    ///
    /// 消费者：`speedtest.rs`（探测池 `probe-in-k` 与 mixed 回退腿）、`misc.rs`（ipinfo 出口探测，mixed 口）。
    /// **不得**用于 `update-in`（那是纯 socks 入站，见 [`Self::via_local_socks_proxy`]）。
    ///
    /// 每次调用新建（低频路径；缓存一个按端口 key 的池收益不抵复杂度）。
    ///
    /// # Errors
    ///
    /// 代理 URL 非法或 client 构建失败。
    pub fn via_local_proxy(port: u16) -> Result<Self, String> {
        Self::with_local_proxy_url(&format!("http://127.0.0.1:{port}"), port)
    }

    /// 经本机 **SOCKS5** 入站的 client（`update-in`）。
    ///
    /// 消费者：订阅 `viaProxy` 拉取（`commands/subscription.rs`）、图标远端代理（`icon_cache.rs`）。
    /// 二者都 pin 到 `update-in` —— `config-engine/builder/inbounds.rs` 里它是
    /// **`type:"socks"`** 入站，纯 socks、压根不说 HTTP。
    ///
    /// 此前这两条链都错用了 [`Self::via_local_proxy`]（`http://`）：reqwest 向 socks 服务器发明文
    /// `CONNECT`/绝对 URI，首字节不是 `0x05` → sing-box 直接断连 → **「经代理更新订阅」整条链恒失败**
    /// （单测发现不了：建 client 本来就成功，失败在握手）。行为对齐 上游
    /// `UpdateNetwork.getProxiedSession`（Chromium `proxyRules: socks5://127.0.0.1:<update-in>`
    /// = SOCKS5 + **代理端**解析）—— 字面 scheme 不同（见下方「域名解析归属」），行为同。
    ///
    /// 需 reqwest `socks` feature（见 `Cargo.toml`：`socks = []`，零新依赖）；未开时本函数在
    /// `Proxy::all` 处 Err「unknown proxy scheme」——**不是** panic，两个调用方各自有直连回退。
    ///
    /// # `socks5h://` 而非 `socks5://` —— 域名解析归属（这条改错等于把功能反着做）
    ///
    /// reqwest 0.13.4 `connect.rs:540-541` 按 scheme 分派解析位置：`socks5` → `DnsResolve::Local`
    /// （**本机**解析出 IP 再把 IP 塞进 socks 请求 ATYP=0x01），`socks5h` → `DnsResolve::Proxy`
    /// （原样发域名，ATYP=0x03，由**代理端**解析）。上游 那边 Electron 用的
    /// `proxyRules: socks5://127.0.0.1:<update-in>` 走的是 **Chromium** 的 scheme 语义 ——
    /// Chromium 的 `socks5://` 恒发域名（remote DNS，`socks4://` 才是本地解析）。故要与 上游
    /// **行为**对齐，reqwest 侧必须写 `socks5h://`；照抄它的字面 scheme 反而把语义写反。
    ///
    /// 为什么这条是功能性的、不是洁癖：用户勾「经代理更新订阅」的典型动机就是订阅域名在本地
    /// **解析不了或被污染**。system-proxy（非 TUN）模式下本机 DNS 不经核，本地解析走的就是被污染的
    /// resolver —— 本地解析会让「经代理更新」连向毒 IP 或直接失败，且把订阅域名泄漏给本地 resolver。
    ///
    /// ## guard 与传输各自的解析归属（改这里前先读完）
    ///
    /// | 谁 | 解析在哪 | 拿解析结果干什么 |
    /// |---|---|---|
    /// | SSRF guard（`safe_redirect_fetch` + `SystemDnsLookup`） | **本机** | 只做**判定**：解析得内网/回环 → 拒；否则放行 |
    /// | 传输（本函数的 `socks5h`） | **代理端**（sing-box） | 真正**连接**目标 |
    ///
    /// 两者**刻意不共用同一次解析**：guard 必须先在本地拿到一个 IP 才有东西可判，而连接必须交给
    /// 代理端才能绕开被污染的本地视图。**代价（如实登记）**：判定对象与连接目标可以分家 ——
    /// 本地解析得公网 IP（guard 放行）而代理端解析得内网 IP 的情形，guard 拦不住。该残留与 上游
    /// 同构（`SubscriptionService` 明写 guard 键于本地解析、`exemptFakeIp` 只在真走 proxied 时开），
    /// 兜底靠 update-in 出站本身经核 route，而非靠 guard。**不要**为了「自洽」把这里改回 `socks5://`：
    /// 那会用一次已知不可信的本地解析去决定真实连接目标，把上面那条动机整个抵消。
    ///
    /// FakeIP 场景两种 scheme 都成立：`socks5h` 直接把域名交给核（核自己 FakeIP 映射），无需本机先拿
    /// `198.18.x.x`。
    ///
    /// # Errors
    ///
    /// 代理 URL 非法（或 `socks` feature 未启用）或 client 构建失败。
    pub fn via_local_socks_proxy(port: u16) -> Result<Self, String> {
        Self::with_local_proxy_url(&format!("socks5h://127.0.0.1:{port}"), port)
    }

    /// 两个 scheme 变体的共同实现（除代理 URL 外配置**逐字相同**，不容许两处漂移）。
    fn with_local_proxy_url(proxy_url: &str, port: u16) -> Result<Self, String> {
        install_ring_provider();
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|e| format!("本机代理地址非法（port={port}）: {e}"))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(proxy)
            .user_agent(app_user_agent())
            .build()
            .map_err(|e| format!("建经代理 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            client,
            warp_client: std::sync::OnceLock::new(),
        })
    }

    /// 底层 client（供本模块内适配器共用）。
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// WARP 专用 client（**首次调用时才建**，见 [`Self::warp_client`] 字段文档）。
    ///
    /// 竞态无害：两个线程同时未命中时都会各建一个，`OnceLock` 只留先到的那个，另一个立即析构。
    /// 幂等构造 + 极低频（WARP 注册），不值得为它上 `Mutex`。
    ///
    /// # Errors
    ///
    /// rustls 配置非法 / TLS 后端初始化失败。
    fn warp_client(&self) -> Result<&reqwest::Client, String> {
        if let Some(c) = self.warp_client.get() {
            return Ok(c);
        }
        let built = build_warp_client()?;
        Ok(self.warp_client.get_or_init(|| built))
    }
}

// ── C19：更新链路「经代理」决策（上游 `shared/update-proxy.ts` 1:1 移植）────────────────
//
// **msvp 是生成无关**：不进 config-engine 生成侧，只在**运行期 HTTP 抓取处**决定「走 update-in socks 口
// vs 直连」。消费者 = UpdateNetwork（App/内核更新检查+下载）/ icon-protocol（图标远端代理）/ RuleResource
// （规则资源下载）。**订阅走独立 `subscriptionProxyPolicy`，不经此**（见 `commands/subscription.rs`）。

/// 更新链路「经代理」生效求值（单一真值）。上游 `resolveMainSessionViaProxy`。
///
/// = 代理运行中 `AND` `mainSessionViaProxy` 未显式关闭（默认开）。代理未运行 → 直连（自举友好：
/// 启动期代理未起时更新检查/资源拉取走直连，不卡死）。
#[must_use]
pub fn resolve_main_session_via_proxy(
    proxy_running: bool,
    main_session_via_proxy: Option<bool>,
) -> bool {
    proxy_running && main_session_via_proxy != Some(false)
}

/// 更新链路会话目标求值（viaProxy + 有效 update-in 端口），单一真值。上游 `resolveUpdateProxyTarget`。
///
/// 把易漂移的「端口闸」收口到一处：`resolve_main_session_via_proxy` 决定经代理；端口不可用（`0`，未运行/
/// 未分配）→ 强制直连（不 pin 无效口）。返回 `(via_proxy, port)` 自洽（`via_proxy=true ⟹ port>0`）。
/// UpdateNetwork（选走 update-in 口 vs 直连）与 icon-protocol（喂 `{viaProxy,port}` 给 SSRF guard）共用，
/// 避免端口闸规则在两处分叉。
#[must_use]
pub fn resolve_update_proxy_target(
    proxy_running: bool,
    main_session_via_proxy: Option<bool>,
    update_in_port: u16,
) -> (bool, u16) {
    let mut via_proxy = resolve_main_session_via_proxy(proxy_running, main_session_via_proxy);
    // port==0 → 强制直连（对齐 TS `if (viaProxy && port <= 0) viaProxy = false`）。
    if via_proxy && update_in_port == 0 {
        via_proxy = false;
    }
    (via_proxy, update_in_port)
}

/// 收集响应头为 `(name, value)` 列（非 UTF-8 头值跳过 —— 契约里 header 是 String）。
fn collect_headers(resp: &reqwest::Response) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect()
}

/// body 流式读取的失败形态。
///
/// 分三态而非一个 String，是因为**三个调用方的映射目标不同**：`HttpClient` 全归 String 冒泡给
/// `safe_redirect_fetch` 归类；[`CoreDownloader`] 要把 `Io{received}` 判成
/// [`DownloadError::Incomplete`]、把 `Stalled` 判成 [`DownloadError::Stalled`]。
/// 若压成一个 String，下载侧就只能靠**串匹配**回头猜自己刚抛的错 —— §R1 明令禁止。
#[derive(Debug)]
enum BodyReadError {
    /// 两个 chunk 之间超过看门狗间隔。
    Stalled,
    /// 超过上限（**已中断连接**，不返回截断内容）。
    TooLarge(usize),
    /// 传输/解码失败。`received` = 断掉前已收字节数（下载侧据此判 Incomplete）。
    Io { received: usize, message: String },
    /// **落盘**失败（仅流式腿可能出现：磁盘满 / 权限 / 卷被拔）。
    ///
    /// 与 [`Self::Io`] 分开是因为映射目标不同：`Io` 在已知 Content-Length 时会被还原成
    /// [`DownloadError::Incomplete`]（「网络把包送少了」），而写盘失败**不是**下载不完整 ——
    /// 把磁盘满报成「下载不完整」会让用户一遍遍重下一个永远装不满的盘。
    Sink {
        received: usize,
        source: std::io::Error,
    },
}

impl std::fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stalled => write!(f, "读取响应体停滞"),
            Self::TooLarge(limit) => write!(f, "响应体超过上限 {limit} 字节，已中断"),
            Self::Io { message, .. } => write!(f, "读取响应体失败: {message}"),
            Self::Sink { source, .. } => write!(f, "写入下载文件失败: {source}"),
        }
    }
}

/// 下载进度回调：`(已收字节, Content-Length)`。`None` = 服务端没给长度 ⇒ **算不出百分比**
/// （调用方须保持 indeterminate，不许拿已收字节瞎凑一个分母）。
///
/// `Send + Sync`：回调在 [`CoreDownloader::download_inner`] spawn 出的 tokio task 里跑，
/// 而调用方在 blocking 线程等结果 —— 跨线程边界。
pub type DownloadProgressFn = dyn Fn(u64, Option<u64>) + Send + Sync;

/// 流式读 body，**超限即中断并 Err**（不截断）。
///
/// 截断后返回会把「超大订阅」变成「解析出半截节点」的坏数据 —— 比明确失败更糟
/// （net-stack `MinimalResponse` doc 的硬契约）。
async fn read_body_capped(
    resp: &mut reqwest::Response,
    max: Option<usize>,
    stall: Duration,
) -> Result<Vec<u8>, BodyReadError> {
    read_body_capped_with_progress(resp, max, stall, None, None).await
}

/// [`read_body_capped`] + 逐 chunk 进度回调。
///
/// 独立成函数（而非给 `read_body_capped` 加参数）是为让订阅 / WARP 两条不需要进度的调用点
/// 保持零改动、零成本：它们仍走上面那个三参版本。
///
/// `expected` 由调用方从 `Content-Length` 注入（本函数不重读 header）——回调要的分母与
/// 下载侧做完整性比对用的是**同一个值**，各读一次必然漂移。
async fn read_body_capped_with_progress(
    resp: &mut reqwest::Response,
    max: Option<usize>,
    stall: Duration,
    expected: Option<u64>,
    on_progress: Option<&DownloadProgressFn>,
) -> Result<Vec<u8>, BodyReadError> {
    let mut buf = Vec::new();
    loop {
        // 每个 chunk 单独计时 = 停滞看门狗（整请求超时管不了「极慢但有数据」）。
        let next = tokio::time::timeout(stall, resp.chunk())
            .await
            .map_err(|_| BodyReadError::Stalled)?;
        match next.map_err(|e| BodyReadError::Io {
            received: buf.len(),
            message: e.to_string(),
        })? {
            None => return Ok(buf),
            Some(chunk) => {
                if let Some(limit) = max {
                    if buf.len() + chunk.len() > limit {
                        // 中断连接（drop resp 即断）+ Err —— **不返回截断的 buf**。
                        return Err(BodyReadError::TooLarge(limit));
                    }
                }
                buf.extend_from_slice(&chunk);
                if let Some(cb) = on_progress {
                    cb(buf.len() as u64, expected);
                }
            }
        }
    }
}

/// [`read_body_capped_with_progress`] 的**落盘版姊妹函数**：字节直接进 `sink`，不在内存里攒。
///
/// # 为什么是姊妹函数而不是就地改造
///
/// 上面那个是 App 腿与内核腿**唯一共用**的字节累积点。把它改成泛型/多一个 sink 参数，等于让
/// 两条内核腿（手动换核 / 自动换核）跟着换一条代码路径 —— 而它们的语义一个字都不该变。
/// 故上面那份**原样保留**给内核腿与订阅腿，本函数只服务流式落盘的 App 腿。
///
/// 三项语义与上面**逐字对齐**（漂了就是两条腿的失败面分叉）：
///  - 停滞看门狗：`tokio::time::timeout(stall, resp.chunk())`，**每个 chunk 单独计时**；
///  - 进度回调时机：每个 chunk **落定之后**以累计 `received` + 同一个 `expected` 触发；
///  - 超限判定：`已收 + 本 chunk > limit` 即 [`BodyReadError::TooLarge`] 并**中断连接**
///    （drop `resp`），绝不把已写出的部分当成功 —— 落盘版由调用方负责删残件。
///
/// 唯一新增的失败形态是 [`BodyReadError::Sink`]（写盘失败），它**不能**折叠进
/// [`BodyReadError::Io`]：后者在已知 Content-Length 时会被还原成
/// [`DownloadError::Incomplete`]，而磁盘满不是「下载不完整」。
///
/// 返回累计写出的字节数（内存版返回 `Vec` 的位置）。
async fn read_body_to_sink_with_progress(
    resp: &mut reqwest::Response,
    max: Option<usize>,
    stall: Duration,
    expected: Option<u64>,
    on_progress: Option<&DownloadProgressFn>,
    // `+ Send`：本函数的 future 要被 spawn 到 tokio runtime（见 `try_candidates`），
    // 跨 await 持有的引用必须 Send。
    sink: &mut (dyn std::io::Write + Send),
) -> Result<u64, BodyReadError> {
    let mut received: usize = 0;
    loop {
        // 每个 chunk 单独计时 = 停滞看门狗（整请求超时管不了「极慢但有数据」）。
        let next = tokio::time::timeout(stall, resp.chunk())
            .await
            .map_err(|_| BodyReadError::Stalled)?;
        match next.map_err(|e| BodyReadError::Io {
            received,
            message: e.to_string(),
        })? {
            None => return Ok(received as u64),
            Some(chunk) => {
                if let Some(limit) = max {
                    if received + chunk.len() > limit {
                        // 中断连接（drop resp 即断）+ Err —— **不把已写出的部分当成功**。
                        return Err(BodyReadError::TooLarge(limit));
                    }
                }
                sink.write_all(&chunk)
                    .map_err(|source| BodyReadError::Sink { received, source })?;
                received += chunk.len();
                if let Some(cb) = on_progress {
                    cb(received as u64, expected);
                }
            }
        }
    }
}

/// 流式读 body 并**截断**到上限（解锁检测语义：截断后仍要判定，与订阅相反）。
/// 返回 `(body, truncated)`。
async fn read_body_truncating(
    resp: &mut reqwest::Response,
    max: usize,
    stall: Duration,
) -> Result<(Vec<u8>, bool), String> {
    let mut buf = Vec::new();
    loop {
        let next = tokio::time::timeout(stall, resp.chunk())
            .await
            .map_err(|_| format!("读取响应体停滞超过 {}ms", stall.as_millis()))?;
        match next.map_err(|e| format!("读取响应体失败: {e}"))? {
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

// ── 适配 ①：net-stack HttpClient（订阅拉取）──────────────────────────────────

impl HttpClient for HttpRuntime {
    /// 单跳 GET（manual redirect：30x 原样返回，**不跟随**）。
    ///
    /// 逐跳 SSRF 复检与链路编排归 `safe_redirect_fetch`（net-stack），本适配器**只做一跳传输**。
    fn fetch(
        &self,
        url: &str,
        init: &FetchInit,
    ) -> impl Future<Output = Result<MinimalResponse, String>> + Send {
        let mut req = self.client.get(url);
        if !init.user_agent.is_empty() {
            req = req.header(reqwest::header::USER_AGENT, init.user_agent.clone());
        }
        for (k, v) in &init.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let response_timeout = init
            .timeout_ms
            .map_or(RESPONSE_TIMEOUT, Duration::from_millis);
        let max_body = init.max_body_bytes;
        async move {
            let mut resp = tokio::time::timeout(response_timeout, req.send())
                .await
                .map_err(|_| format!("请求超时（{}ms）", response_timeout.as_millis()))?
                .map_err(|e| format!("请求失败: {e}"))?;

            let status = resp.status().as_u16();
            let headers = collect_headers(&resp);
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            // 30x 的 body 由 safe_redirect_fetch 丢弃（体积小）→ 不读，省一次传输。
            let body = if (300..400).contains(&status) {
                Vec::new()
            } else {
                read_body_capped(&mut resp, max_body, STALL_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?
            };

            Ok(MinimalResponse {
                status,
                location,
                headers,
                body,
            })
        }
    }
}

// ── 适配 ②：updater UpdateDownloader（**唯一**下载适配器）────────────────────

/// GitHub 域名表（镜像回退的判定面）。
///
/// **注意**：审计 §C9 裁决 gh-proxy 的 URL 重写（5 域名表 + `applyGhProxy`）应归 net-stack
/// 纯函数模块，消费方为本适配器。那个模块**尚未落地**（全仓 grep 零命中）。`ghProxyPrefix` 现经通用
/// config 保存路径落盘（`update({ghProxyPrefix})` → config_save）。故此处**只做最小可用的镜像前缀拼接**，且刻意不建第二份 5 域名表 ——
/// §A3 血证：`RuleResourceManager` 曾有 2 份 `GITHUB_HOSTS` 副本漂移，令三级兜底自相矛盾。
/// gh-proxy 模块落地后，本函数应改为调它，见任务报告边界声明。
const GITHUB_ASSET_HOSTS: [&str; 2] = ["github.com", "objects.githubusercontent.com"];

/// 该 URL 是否是可经 gh 镜像加速的 GitHub 资产地址。
fn is_github_asset(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| GITHUB_ASSET_HOSTS.contains(&h.as_str()))
}

/// 把 HTTP 状态映射为下载错误；2xx → `None`。**纯函数**（可单测，无 IO）。
///
/// 403 限流分类：GitHub 在限流时返 403 且 `x-ratelimit-remaining: 0`。它与「403 无权限」
/// 表象相同但处置相反（前者等一会就好，后者永远不好），故消息里带出来 ——
/// 变体仍用 [`DownloadError::HttpStatus`]（枚举无专用限流变体，**不新造**：
/// trait 是 updater 的，接线批不改编排）。
fn classify_download_status(status: u16, headers: &[(String, String)]) -> Option<DownloadError> {
    if (200..300).contains(&status) {
        return None;
    }
    if status == 403 {
        let rate_limited = headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-ratelimit-remaining") && v.trim() == "0");
        if rate_limited {
            return Some(DownloadError::Other(
                "GitHub API 限流（x-ratelimit-remaining=0）：稍后重试或配置 gh 加速前缀".into(),
            ));
        }
    }
    Some(DownloadError::HttpStatus(status))
}

/// body 读取失败 → 下载错误的**唯一**映射（两个消费端共用，避免失败分类在两条腿上分叉）。
///
/// 逐条与形参化之前的 `download_once` 内联 match 一致：
///  - `Stalled` → [`DownloadError::Stalled`]（看门狗间隔，非请求超时）；
///  - `TooLarge` → [`DownloadError::Other`]（带上限数字）；
///  - `Io` + 已知 Content-Length → [`DownloadError::Incomplete`]。hyper 会先于我们发现
///    「连接早于 Content-Length 关闭」并报成解码错，故靠 received + expected 还原成结构化
///    Incomplete，而非泛化 Other；长度未知时才回落 Other。
///  - `Sink` → [`DownloadError::Io`]（**写盘失败原样透传**，不冒充「下载不完整」）。
///
/// **纯函数**（无 IO）⇒ 可单测。
fn map_body_error(e: BodyReadError, expected: Option<u64>) -> DownloadError {
    match e {
        BodyReadError::Stalled => DownloadError::Stalled(STALL_TIMEOUT.as_millis() as u64),
        BodyReadError::TooLarge(limit) => {
            DownloadError::Other(format!("下载体积超过上限 {limit} 字节，已中断"))
        }
        BodyReadError::Io { received, message } => match expected {
            Some(n) => DownloadError::Incomplete {
                received: received as u64,
                expected: n,
            },
            None => DownloadError::Other(message),
        },
        // `received` 在此**真被消费**（不是死字段）：磁盘满的诊断价值一半在「写到第几字节才满」，
        // 而 `DownloadError::Io` 只装得下一个 `io::Error` ⇒ 把它并进消息里，
        // `kind` 原样保留（上层若按 kind 分流不受影响）。
        BodyReadError::Sink { received, source } => DownloadError::Io(std::io::Error::new(
            source.kind(),
            format!("写入下载文件失败（已写出 {received} 字节）: {source}"),
        )),
    }
}

/// 完整性：实收 vs 一个**声明值**（= 上游 `parseExpectedBytes`）。**纯函数**。
///
/// 覆盖「连接干净关闭但字节偏少/偏多」的情形（[`map_body_error`] 的 `Io` 分支覆盖「连接异常断」）。
///
/// **三个消费端共用同一条判据**（各写一份 ⇒ 某条腿会少一道完整性门）：
///  1. 内存腿传 `bytes.len()` vs `Content-Length`；
///  2. 落盘腿传实际写出的字节数 vs `Content-Length`；
///  3. App 更新腿（`commands/updater.rs` 的 `check_declared_size`）传实收字节 vs
///     **发布清单声明的 `fileSize`**。第 3 条与前两条的信任根不同：`Content-Length` 是
///     「撒谎方自己给的数」，对撒谎方零约束；`fileSize` 来自 GitHub release 清单，
///     镜像/中间人改不动它。故它是无摘要腿（旧 release）唯一有牙的等值判据。
///
/// `pub(crate)`：第 3 个消费端在 `commands/` 层，但判据不该因此复制一份。
pub(crate) fn check_content_length(
    received: u64,
    expected: Option<u64>,
) -> Result<(), DownloadError> {
    if let Some(n) = expected {
        if received != n {
            return Err(DownloadError::Incomplete {
                received,
                expected: n,
            });
        }
    }
    Ok(())
}

/// 流式落盘下载的结果：写出的字节数 + **边写边算**出来的 sha256。
///
/// 摘要随下载一遍算完（不是落盘后再读一遍文件），故「校验」这一步零额外 IO、零额外内存。
#[derive(Debug, Clone)]
pub struct StreamedDownload {
    /// 实际写出的字节数。
    pub bytes: u64,
    /// 全部字节的 sha256（小写 hex）。
    pub sha256_hex: String,
}

/// 建写句柄的工厂：**每个候选 URL 试一次就要一个新句柄**。
///
/// 镜像回退会换一个候选重下，那时已写出的部分必须被丢弃、从头写起 —— 传一个句柄进去做不到
/// （句柄已被写脏），传工厂才能让每次尝试都拿到截断过的干净句柄。
pub type DownloadSinkFactory =
    dyn Fn() -> std::io::Result<Box<dyn std::io::Write + Send>> + Send + Sync;

/// 边写边算 sha256 的写入包装：一遍数据流同时喂给文件与 hasher。
///
/// 落盘后再读回来算摘要 = 把刚省下的那次全量 IO 又花掉；先攒内存再算 = 把刚省下的内存又占回来。
struct HashingSink {
    inner: Box<dyn std::io::Write + Send>,
    hasher: polaris_updater::verify::Sha256Stream,
}

impl HashingSink {
    fn new(inner: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            inner,
            hasher: polaris_updater::verify::Sha256Stream::new(),
        }
    }

    /// flush 底层句柄并交出 `(喂进 hasher 的累计字节数, 摘要)`。
    ///
    /// # `flush` 覆盖什么、**不**覆盖什么（别把它当持久化）
    ///
    /// **必须 flush**：`BufWriter` 之类的包装在 drop 里吞掉写错误，不 flush 就可能出现
    /// 「摘要算对了、盘上少一截」——而后续 rename 会把这个残包提升成 dest。
    ///
    /// 但生产注入的是 `StdFs::open_write` 给的**裸 `std::fs::File`**，对它
    /// `Write::flush` 是 **no-op**（`File` 无用户态缓冲）—— 即这一句在生产路径上什么都没做，
    /// 它守的是「将来有人在中间塞一层带缓冲的包装」。**且本类型全程没有 `sync_all`**：
    /// 字节只到 page cache，随后的 rename 先于数据持久化落地，断电后 dest 可能是半截文件，
    /// 而 `update_install` 只做 `is_file()`、不复核摘要。这条**不是本批引入的回归**
    /// （旧的 `atomic_replace` 同样无 fsync），故此处只如实标注、不擅自加 fsync。
    ///
    /// 返回累计字节数是为让调用方与**网络侧**独立维护的 `received` 互校：两个数分别回答
    /// 「网络收了多少」与「sink 真吃下多少」，对不上就说明摘要算在了一份与盘上不同的内容上。
    fn finish(mut self) -> std::io::Result<(u64, String)> {
        self.inner.flush()?;
        let hashed = self.hasher.len();
        Ok((hashed, self.hasher.finish()))
    }
}

impl std::io::Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 先落盘再喂 hasher：写失败时 hasher 不能已经吃进这段，否则重试路径的摘要会脏。
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// **唯一**的下载适配器（= `UpdateDownloader` 的生产实现，取代 `UnavailableDownloader`）。
///
/// 收口 `UpdateDownloader` doc 划给「实现侧」的全部细节：重定向跟随 / UA /
/// Content-Length 完整性 / 停滞看门狗 / 镜像回退 / 16MiB 闸 / 15s 响应超时。
/// **订阅路径不得复制这些**（那正是 上游 双份编排的成因）。
///
/// # sync trait 的 async 桥
///
/// `UpdateDownloader::download` 是**同步**签名（staged 周期整条是同步纯逻辑），而 reqwest 是
/// async。桥法：把 future `spawn` 到 tokio runtime，同步线程 `recv` 等结果。
///
/// **调用方必须在 blocking 线程上调**（`spawn_blocking` / 非 async command）：
/// 在 async 上下文里直接调会阻塞 executor 线程。Tauri 的同步 command 跑在**主线程**上 ——
/// 15s 下载会冻 UI，故消费该适配器的 command 一律 `async fn` + `spawn_blocking`。
///
/// # 为什么 `derive(Clone)` 而不是手抄一个克隆器
///
/// [`Self::try_candidates`] 要把一份自身移进 spawn 出去的 task（`&self` 借用不能跨 spawn）。
/// 原先那份手写的 `for_task` 逐字段抄一遍，四个字段全是 `Clone` —— 手抄版的唯一「能力」
/// 是**漏抄新字段而编译得过**：`max_bytes` 形参化那次就差点漏掉，漏了的话 App 腿会静默地
/// 拿 16 MiB 内存闸去下几十 MiB 的安装包。派生版漏字段直接编译不过。
#[derive(Clone)]
pub struct CoreDownloader {
    http: Arc<HttpRuntime>,
    handle: tokio::runtime::Handle,
    /// gh 加速前缀（如 `https://ghproxy.net/`）；空 = 不用镜像。
    gh_proxy_prefix: String,
    /// 单次下载体积硬闸。默认 [`MAX_DOWNLOAD_BYTES`]（内存腿口径），由
    /// [`Self::with_max_bytes`] 按腿覆盖。
    max_bytes: usize,
}

impl CoreDownloader {
    /// 新建。`handle` 须是活着的 tokio runtime handle（command 层 `Handle::current()`）。
    ///
    /// 体积闸默认取 [`MAX_DOWNLOAD_BYTES`]（= 形参化之前的行为）；流式落盘腿须显式
    /// [`Self::with_max_bytes`] 覆盖。
    #[must_use]
    pub fn new(http: Arc<HttpRuntime>, handle: tokio::runtime::Handle) -> Self {
        Self {
            http,
            handle,
            gh_proxy_prefix: String::new(),
            max_bytes: MAX_DOWNLOAD_BYTES,
        }
    }

    /// 设 gh 加速前缀（镜像回退用）。
    #[must_use]
    pub fn with_gh_proxy(mut self, prefix: impl Into<String>) -> Self {
        self.gh_proxy_prefix = prefix.into().trim().to_string();
        self
    }

    /// 覆盖单次下载的体积硬闸。
    ///
    /// **闸值属于「这一腿下多大的东西」，不属于传输层**：内核腿把整包收进内存（`Vec<u8>`）
    /// ⇒ 闸就是内存闸，恒取 [`MAX_DOWNLOAD_BYTES`]；App 安装包腿流式落盘 ⇒ 内存不随包体积长，
    /// 闸改由「清单声明大小 + 裕度」注入。写死一个常量给两条腿共用，必然是「要么卡死大安装包、
    /// 要么给换核腿开一个 OOM 口子」二选一。
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// 候选 URL 列表：原址优先，GitHub 资产且配了前缀时追加镜像作为**回退**。
    fn candidates(&self, url: &str) -> Vec<String> {
        let mut out = vec![url.to_string()];
        if !self.gh_proxy_prefix.is_empty() && is_github_asset(url) {
            let prefix = self.gh_proxy_prefix.trim_end_matches('/');
            out.push(format!("{prefix}/{url}"));
        }
        out
    }

    /// 请求 + 跟随重定向 + 状态分类 + Content-Length 预检；返回**就绪待读**的响应与期望字节数。
    ///
    /// # 为什么抽出来
    ///
    /// 两个消费端（[`Self::download_once`] 整包入内存 / [`Self::download_once_to_sink`] 流式落盘）
    /// 在「读 body」之前要做的事**一模一样**：重定向跟随（GitHub 资产必然 302）、非 http/https 拒绝、
    /// 403 限流分类、超上限早拒。复制第二份必然漂移 —— 上游 `core-downloader.ts` 与
    /// `UpdateService.ts` 两份同构编排就是前车之鉴（见模块文档）。
    async fn open_download_response(
        &self,
        url: &str,
    ) -> Result<(reqwest::Response, Option<u64>), DownloadError> {
        let mut current = url.to_string();
        for _ in 0..=MAX_DOWNLOAD_REDIRECTS {
            let resp =
                tokio::time::timeout(RESPONSE_TIMEOUT, self.http.client().get(&current).send())
                    .await
                    .map_err(|_| DownloadError::Stalled(RESPONSE_TIMEOUT.as_millis() as u64))?
                    .map_err(|e| DownloadError::Other(format!("请求失败: {e}")))?;

            let status = resp.status().as_u16();

            // 30x：自己跟随（client 全局关了 redirect，见 HttpRuntime doc）。
            if (300..400).contains(&status) {
                let Some(loc) = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                else {
                    return Err(DownloadError::Other(format!(
                        "HTTP {status} 缺 Location 头"
                    )));
                };
                let base = reqwest::Url::parse(&current)
                    .map_err(|e| DownloadError::Other(format!("URL 非法 {current}: {e}")))?;
                let next = base
                    .join(&loc)
                    .map_err(|e| DownloadError::Other(format!("重定向目标非法 {loc}: {e}")))?;
                if next.scheme() != "http" && next.scheme() != "https" {
                    return Err(DownloadError::Other(format!(
                        "重定向目标协议不支持（仅 http/https）: {}",
                        next.scheme()
                    )));
                }
                current = next.to_string();
                continue;
            }

            let headers = collect_headers(&resp);
            if let Some(e) = classify_download_status(status, &headers) {
                return Err(e);
            }

            // Content-Length 预检（体积闸的早拒腿：不用先下完才发现超）。
            let expected = resp.content_length();
            if let Some(n) = expected {
                if n > self.max_bytes as u64 {
                    let limit = self.max_bytes;
                    return Err(DownloadError::Other(format!(
                        "下载体积 {n} 字节超过上限 {limit}，已拒绝"
                    )));
                }
            }
            return Ok((resp, expected));
        }
        Err(DownloadError::Other(format!(
            "重定向次数超过上限（{MAX_DOWNLOAD_REDIRECTS}）"
        )))
    }

    /// 真实下载一个 URL 到内存（含重定向跟随 + 完整性 + 看门狗 + 可选逐 chunk 进度）。
    async fn download_once(
        &self,
        url: &str,
        on_progress: Option<&DownloadProgressFn>,
    ) -> Result<Vec<u8>, DownloadError> {
        let (mut resp, expected) = self.open_download_response(url).await?;
        // 流式读 + 停滞看门狗 + 硬闸（content-length 可缺失/撒谎 → 读取侧必须再拦一次）。
        let bytes = read_body_capped_with_progress(
            &mut resp,
            Some(self.max_bytes),
            STALL_TIMEOUT,
            expected,
            on_progress,
        )
        .await
        .map_err(|e| map_body_error(e, expected))?;
        check_content_length(bytes.len() as u64, expected)?;
        Ok(bytes)
    }

    /// 真实下载一个 URL **直接写进 `sink`**（字节不在内存里攒）。
    ///
    /// 与 [`Self::download_once`] 共用 [`Self::open_download_response`]（重定向 / 状态分类 /
    /// 预检）、[`map_body_error`]（失败分类）与 [`check_content_length`]（完整性）——
    /// **没有第二份平行编排**。差别只有一处：body 走 [`read_body_to_sink_with_progress`]。
    ///
    /// 返回实际写出的字节数。**失败时 sink 里可能已有部分内容**（本函数不知道 sink 是什么，
    /// 清理残件是调用方的责任）。
    async fn download_once_to_sink(
        &self,
        url: &str,
        sink: &mut (dyn std::io::Write + Send),
        on_progress: Option<&DownloadProgressFn>,
    ) -> Result<u64, DownloadError> {
        let (mut resp, expected) = self.open_download_response(url).await?;
        let received = read_body_to_sink_with_progress(
            &mut resp,
            Some(self.max_bytes),
            STALL_TIMEOUT,
            expected,
            on_progress,
            sink,
        )
        .await
        .map_err(|e| map_body_error(e, expected))?;
        check_content_length(received, expected)?;
        Ok(received)
    }

    /// 同步下载 **+ 逐 chunk 进度回调**（见类型文档「sync trait 的 async 桥」：须在 blocking 线程调用）。
    ///
    /// [`UpdateDownloader::download`] 的签名是 trait 定的（无进度参数），故细粒度进度只能走这条
    /// 固有方法。二者共用 [`Self::download_inner`] —— **不复制第二份镜像回退/重定向编排**。
    ///
    /// **当前无生产调用点**（如实登记，2026-08-16 全仓反查：定义 + 一条单测，无第三处）——
    /// App 安装包腿改流式落盘后已换走 [`Self::download_to_sink_with_progress`]，两条内核腿走
    /// 无进度的 trait 方法。保留为「内存腿 + 进度」的**成对 API**，理由有二：
    ///  1. 它与 [`Self::download_to_sink_with_progress`] 是同一条 `read_body_*_with_progress`
    ///     语义的两个形态，流式腿那条门（`streaming_download_reports_progress_like_the_memory_leg`）
    ///     声称「与内存腿同时机同分母」，删掉这一半就没有参照物了；
    ///  2. 删它要连带把 `download_inner` / `read_body_capped_with_progress` 的进度参数一并摘掉，
    ///     那是动内核腿的代码路径 —— 收益（少一个未调用的 pub 方法）与半径不成比例。
    ///
    /// 回调在每个 chunk 到达时触发，可能高频（几百次）；**限频归调用方**（发 IPC 前按整数百分比
    /// 去重），此处不替调用方决定节流策略。镜像回退时回调会从新候选的 0 重新开始 —— 这是真值
    /// （确实在重下），调用方若不想让进度条倒退需自行取 max。
    pub fn download_with_progress(
        &self,
        url: &str,
        on_progress: Arc<DownloadProgressFn>,
    ) -> Result<Vec<u8>, DownloadError> {
        self.download_inner(url, Some(on_progress))
    }

    /// 下载编排**单点**：候选列表（原址→镜像）逐个试，首个成功即返；全败返最后一个错。
    ///
    /// 泛型化是为让内存腿与流式落盘腿共用**同一条**候选编排 —— 「镜像何时回退、失败如何记账、
    /// runtime 关掉怎么算」各写一份必然漂移。`attempt` 收到的是一份可移进 task 的
    /// `self.clone()`（`&self` 借用不能跨 spawn）与该次候选 URL。
    fn try_candidates<T, F, Fut>(&self, url: &str, attempt: F) -> Result<T, DownloadError>
    where
        T: Send + 'static,
        F: Fn(Self, String) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = Result<T, DownloadError>> + Send + 'static,
    {
        let mut last: Option<DownloadError> = None;
        for cand in self.candidates(url) {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let dl = self.clone();
            let attempt = attempt.clone();
            let url_owned = cand.clone();
            self.handle.spawn(async move {
                let _ = tx.send(attempt(dl, url_owned).await);
            });
            match rx.recv() {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) => {
                    log::warn!("下载失败（{cand}）: {e}");
                    last = Some(e);
                }
                Err(_) => {
                    last = Some(DownloadError::Other(
                        "下载任务被取消（runtime 已关闭？）".into(),
                    ));
                }
            }
        }
        Err(last.unwrap_or_else(|| DownloadError::Other("无可用下载地址".into())))
    }

    /// 整包入内存的下载（[`UpdateDownloader::download`] 与 [`Self::download_with_progress`] 共用）。
    fn download_inner(
        &self,
        url: &str,
        on_progress: Option<Arc<DownloadProgressFn>>,
    ) -> Result<Vec<u8>, DownloadError> {
        self.try_candidates(url, move |dl, cand| {
            let cb = on_progress.clone();
            async move { dl.download_once(&cand, cb.as_deref()).await }
        })
    }

    /// **流式落盘**下载 + 逐 chunk 进度 + 增量 sha256（见类型文档「sync trait 的 async 桥」：
    /// 须在 blocking 线程调用）。
    ///
    /// 与 [`Self::download_with_progress`] 的差别只有「字节去哪」：那条把整包攒进 `Vec<u8>`
    /// （内存峰值 = 包体积），本条把每个 chunk 直接写进 `new_sink()` 给出的句柄，
    /// 内存占用与包体积**解耦**。摘要随写一遍算完（[`HashingSink`]），故校验不需要再读一遍。
    ///
    /// `new_sink` 是**工厂**不是句柄：镜像回退换候选重下时要拿一个截断过的干净句柄，
    /// 否则第二次的字节会接在第一次的残料后面（长度对不上、摘要也对不上）。
    ///
    /// **失败时句柄里可能已有部分内容** —— 本方法不知道 sink 背后是什么，**清理残件是调用方的责任**。
    ///
    /// # Errors
    ///
    /// 除 [`Self::download_with_progress`] 的全部失败形态外，多一条建句柄/写盘失败
    /// （[`DownloadError::Io`]，**不冒充** `Incomplete`）。
    pub fn download_to_sink_with_progress(
        &self,
        url: &str,
        new_sink: Arc<DownloadSinkFactory>,
        on_progress: Arc<DownloadProgressFn>,
    ) -> Result<StreamedDownload, DownloadError> {
        self.try_candidates(url, move |dl, cand| {
            let new_sink = new_sink.clone();
            let cb = on_progress.clone();
            async move {
                // 每个候选各拿一个新句柄（截断已写出的残料）。
                let mut sink = HashingSink::new(new_sink().map_err(DownloadError::Io)?);
                let bytes = dl
                    .download_once_to_sink(&cand, &mut sink, Some(cb.as_ref()))
                    .await?;
                let (hashed, sha256_hex) = sink.finish().map_err(DownloadError::Io)?;
                // 网络侧的 `bytes` 与 hasher 侧的 `hashed` 是**两个独立维护的计数**：前者由
                // `read_body_to_sink_with_progress` 按 chunk 累加，后者由 `HashingSink::write`
                // 按**实际写出的 n** 累加。二者相等才证明「摘要算的就是盘上那份」——不等意味着
                // 某个 `write` 短写了却被当成全写（`write_all` 之外的路径），此时摘要会对着一份
                // 与文件不同的内容成立，而它正是落位前的唯一校验。零成本换一条硬证据。
                if hashed != bytes {
                    return Err(DownloadError::Other(format!(
                        "下载内部不一致：sink 收下 {hashed} 字节、网络侧记 {bytes} 字节"
                    )));
                }
                Ok(StreamedDownload { bytes, sha256_hex })
            }
        })
    }
}

impl UpdateDownloader for CoreDownloader {
    /// 同步下载（见类型文档「sync trait 的 async 桥」：**须在 blocking 线程调用**）。
    /// 需要进度的调用点走固有方法 [`CoreDownloader::download_with_progress`]。
    fn download(&self, url: &str) -> Result<Vec<u8>, DownloadError> {
        self.download_inner(url, None)
    }
}

// ── 适配 ③（已迁出）：unlock UnlockHttp ───────────────────────────────────────
//
// **解锁检测的传输层已迁到独立 crate `polaris-unlock-transport`**（`wreq` + Chrome 131 指纹伪装）。
//
// 迁出理由（见该 crate 模块文档）：CF 按 **TLS/JA3 指纹**判自动化 → rustls 形态吃 1020/403，
// 这正是本文件 `warp_client` 文档记录过的同一类问题。解锁面对通用 CF 边缘，只能上真指纹伪装。
//
// **本文件（reqwest + rustls）刻意不再实现 `UnlockHttp`**：指纹客户端与 BoringSSL 构建链的
// 爆炸半径被限制在那一个 crate 内，订阅拉取 / 内核下载 / WARP 注册**继续走这里**，不受影响。
// 若此处再加回一个 `impl UnlockHttp for HttpRuntime`，就等于给解锁检测开了一条绕过指纹伪装的后门。

// ── 适配 ④：mesh WarpHttp ─────────────────────────────────────────────────────

/// 发一个 WARP 请求，返回 (status, 截断后的 body)。
async fn warp_send(
    client: &reqwest::Client,
    req: &WarpHttpRequest,
) -> Result<WarpHttpResponse, String> {
    let mut builder = match req.method {
        WarpHttpMethod::Post => client.post(&req.url),
        WarpHttpMethod::Put => client.put(&req.url),
        WarpHttpMethod::Delete => client.delete(&req.url),
    };
    for (k, v) in &req.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if let Some(body) = &req.body {
        builder = builder.body(body.clone());
    }
    let mut resp = tokio::time::timeout(WARP_TIMEOUT, builder.send())
        .await
        .map_err(|_| format!("WARP 请求超时（{}ms）", WARP_TIMEOUT.as_millis()))?
        .map_err(|e| format!("WARP 请求失败: {e}"))?;
    let status = resp.status().as_u16();
    // 响应流错误（对端 RST / 提前关闭）显式映射 Err —— 契约明写：否则上游 Promise 永挂。
    let (body, _truncated) =
        read_body_truncating(&mut resp, WARP_MAX_BODY_BYTES, WARP_TIMEOUT).await?;
    Ok(WarpHttpResponse {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// # TLS 指纹规避：走**专用** [`build_warp_client`]（TLS1.2-only + HTTP/1.1）
///
/// `warp_http.rs` 模块文档明写：**Cloudflare WAF 校验 TLS 指纹**，rustls-default 的 ClientHello（TLS1.3 + h2）
/// ≠ Node/okhttp → **1020/403**。故本 impl 的两条路径**都用 `self.warp_client`**（钉 TLS1.2 + `http1_only`），
/// **不**用共享 `self.client` —— 对齐 上游 `WarpService` 的 node-`https`(TLS1.2 pin) 形态。选型理由见
/// [`build_warp_client`]：oracle 实证 上游 靠的是**粗形态**（TLS1.2/HTTP1.1）而非精确 okhttp JA3，
/// 故用 rustls 现有能力收窄形态即可，**零新依赖**（不引 boring/native-tls 的 C 构建重量）。
///
/// # ⚠️ 真机门 —— **未经真实 CF 验证**
///
/// rustls 的 TLS1.2 ClientHello 仍是**第三种**指纹（既非 node-OpenSSL 亦非 okhttp）。若 CF 判的不止是粗形态、
/// 而是把 rustls 的具体 TLS1.2 指纹也列黑，则仅钉版本不足以过 1020。这**只能**向 `api.cloudflareclient.com`
/// 发真实注册请求（创建真实设备账户，有副作用）验证——本批遵「禁本机碰宿主网络」未做，如实登记为未验。
/// 若真机 1020，处置是**升级到精确 JA3 指纹栈**（如 `boring`/`tokio-boring`，代价见任务报告的跨平台构建成本），
/// **不是**改本适配器的重试逻辑。失败形态是明确的 403+body-1020，已由 `classify_deregister_result` 分类，不会伪装成成功。
#[async_trait]
impl WarpHttp for HttpRuntime {
    /// register/applyLicense：2xx → Ok(body)；非 2xx / 网络错 → Err（带 status + 截断 body）。
    async fn json_request(&self, req: &WarpHttpRequest) -> Result<String, String> {
        let resp = warp_send(self.warp_client()?, req).await?;
        if (200..300).contains(&resp.status) {
            Ok(resp.body)
        } else {
            // 契约：非 2xx 即 Err，带 status + 截断 body（body 里可能有 CF 的 error 1020）。
            Err(format!("WARP API {}: {}", resp.status, resp.body))
        }
    }

    /// unregister：**保留 4xx 状态**返回（供 `classify_deregister_result` 做 done/drop/retry 分类）；
    /// 仅网络层错误（无 HTTP 状态）才 Err。
    async fn status_request(&self, req: &WarpHttpRequest) -> Result<WarpHttpResponse, String> {
        warp_send(self.warp_client()?, req).await
    }
}

// ── 适配 ⑤：dns-race DohPost（C11 节点域名竞速的 DoH 上游）────────────────────

/// DoH 单请求超时（含连接 + 首字节 + 读完 body）。
///
/// **必须显著小于 `polaris_dns_race::PER_UPSTREAM_TIMEOUT`(1500ms)**：单上游超时是竞速层的最后一道
/// 兜底，若传输层自己不超时就全靠它，日志里只会看到「上游超时」而分不清是慢还是挂。
/// 取 1200ms —— 国内 DoH（223.5.5.5 / 1.12.12.12）正常 RTT 在几十毫秒量级，1.2s 已是极宽裕。
const DOH_TIMEOUT: Duration = Duration::from_millis(1200);

/// DoH 响应体上限。DNS over HTTPS 单条响应远小于此（UDP 侧宣告的 EDNS0 payload 一般 1232/4096）；
/// 设闸是防「上游被劫持成一个大文件」把内存吃穿。
const DOH_MAX_BODY_BYTES: usize = 8 * 1024;

/// 节点域名竞速 sidecar 的 DoH 上游传输。
///
/// 走**共享** `self.client`（rustls-default + `no_proxy()`）：
/// - `no_proxy()` 是硬要求 —— sidecar 起在**起核路径上**，若这里的请求继承我们自己设的系统代理，
///   就成了「解析节点域名要先连上代理，而连代理要先解析节点域名」的自举死锁。
/// - 目标 URL 的 host 恒为**字面 IP**（`parse_custom_upstream` 拒绝域名上游、内置上游是 IP DoH），
///   故不需要也不会触发第二层 DNS 解析；证书按 IP SAN 校验。
///
/// **真机门**：对 `223.5.5.5` / `1.12.12.12` 的真实 DoH POST（IP-SAN 证书链是否被 rustls+webpki
/// 接受、境内 RTT 是否落在预算内）本机不可验 —— 本仓禁止测试触碰宿主网络。此处只保证形态正确
/// （方法/头/超时/体积闸/错误映射），端到端见任务报告的真机验证项。
#[async_trait]
impl polaris_dns_race::DohPost for HttpRuntime {
    async fn post_dns_message(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
        let req = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/dns-message")
            .header(reqwest::header::ACCEPT, "application/dns-message")
            .body(body);
        let mut resp = tokio::time::timeout(DOH_TIMEOUT, req.send())
            .await
            .map_err(|_| format!("DoH 超时（{}ms）: {url}", DOH_TIMEOUT.as_millis()))?
            .map_err(|e| format!("DoH 请求失败 {url}: {e}"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // 非 2xx 一律 Err = 竞速层的 FAIL（**不是** EMPTY）：上游故障绝不能被当成「域名无记录」。
            return Err(format!("DoH {status}: {url}"));
        }
        read_body_capped(&mut resp, Some(DOH_MAX_BODY_BYTES), DOH_TIMEOUT)
            .await
            .map_err(|e| format!("DoH 读响应失败 {url}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;

    // ── 本地 HTTP server（stdlib，回环端口；不触碰宿主网络）──────────────────
    //
    // 与 net-stack `tests/fetch_pipeline.rs` 的 server 同形态但**刻意不共用**：那个在 net-stack 的
    // tests/ 下（跨 crate 不可 import），且此处要的是「真 reqwest 打真 socket」。共用需提第三个
    // dev-dep crate —— 二十行 stdlib 不值得（简约阶梯）。

    fn spawn_server(responses: Vec<Vec<u8>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        thread::spawn(move || {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 8192];
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

    // ── provider 陷阱的门（见模块文档）────────────────────────────────────────

    #[tokio::test]
    async fn http_runtime_builds_a_real_client_without_panicking() {
        // **这扇门守的是「编译过 ≠ 能跑」**：reqwest `rustls-no-provider` 下若没先装 ring provider，
        // `Client::builder().build()` 在 client.rs:2482 直接 panic!。
        // 删掉 HttpRuntime::new 里的 install_ring_provider() → 本测试 panic 转红。
        let rt = HttpRuntime::new().expect("建 client 不得失败");
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "ring provider 必须已安装——否则 reqwest 建 client 时 panic"
        );
        // 真发一个请求以证明 client 可用（打回环，不碰宿主网络/公网）。
        let addr = spawn_server(vec![http_response("200 OK", &[], b"ok")]);
        let init = FetchInit {
            user_agent: app_user_agent(),
            ..Default::default()
        };
        let r = rt
            .fetch(&format!("http://{addr}/probe"), &init)
            .await
            .expect("回环请求应成功");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"ok");
    }

    // ── HttpClient 契约门 ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_does_not_follow_redirects_and_returns_location() {
        // 契约硬要求：**不得自动跟随**（自动跟随 = SSRF 逐跳复检被绕过）。
        // 若 HttpRuntime::new 把 Policy::none() 改成默认（follow）→ 本测试转红。
        let addr = spawn_server(vec![http_response(
            "302 Found",
            &[("Location", "https://evil.internal/secret")],
            b"",
        )]);
        let rt = HttpRuntime::new().unwrap();
        let init = FetchInit::default();
        let r = rt.fetch(&format!("http://{addr}/r"), &init).await.unwrap();
        assert_eq!(r.status, 302, "30x 必须原样返回，不得跟随");
        assert_eq!(r.location.as_deref(), Some("https://evil.internal/secret"));
    }

    #[tokio::test]
    async fn fetch_headers_are_case_insensitively_retrievable() {
        let addr = spawn_server(vec![http_response(
            "200 OK",
            &[("Content-Type", "text/yaml"), ("ETag", "\"abc\"")],
            b"proxies: []",
        )]);
        let rt = HttpRuntime::new().unwrap();
        let r = rt
            .fetch(&format!("http://{addr}/s"), &FetchInit::default())
            .await
            .unwrap();
        assert_eq!(r.header("content-type"), Some("text/yaml"));
        assert_eq!(r.header("etag"), Some("\"abc\""));
    }

    #[tokio::test]
    async fn fetch_body_over_limit_errors_and_never_truncates() {
        // MinimalResponse doc 的硬契约：超限**中断并 Err**，不得截断后返回
        // （截断 = 把超大订阅变成「解析出半截节点」的坏数据，比明确失败更糟）。
        let body = vec![b'x'; 4096];
        let addr = spawn_server(vec![http_response("200 OK", &[], &body)]);
        let rt = HttpRuntime::new().unwrap();
        let init = FetchInit {
            max_body_bytes: Some(1024),
            ..Default::default()
        };
        let r = rt.fetch(&format!("http://{addr}/big"), &init).await;
        assert!(
            r.is_err(),
            "超限必须 Err，实得 Ok（= 截断后返回，契约违反）"
        );
    }

    #[tokio::test]
    async fn fetch_network_error_surfaces_as_err_not_panic() {
        // 关键分层：连不上 → Err（reason=Network），由 safe_redirect_fetch 归类；不得 panic。
        let rt = HttpRuntime::new().unwrap();
        // 端口 1 上不会有 listener（无副作用、不触碰宿主网络配置）。
        let r = rt
            .fetch("http://127.0.0.1:1/nope", &FetchInit::default())
            .await;
        assert!(r.is_err());
    }

    // ── 经本机入站的两个构造器：scheme × 入站类型 配对门（「经代理更新订阅」断链的根因）────
    //
    // 此前订阅/图标两条链都用 `via_local_proxy`（`http://127.0.0.1:{port}`）打 `update-in`，而它是
    // sing-box **`type:"socks"`** 入站（`config-engine/builder/inbounds.rs:86-105`）—— 明文 HTTP
    // 打 socks 服务器首字节就对不上，整条「经代理更新订阅」恒失败。
    //
    // 该缺陷**靠「构建成功」根本发现不了**（建 client 本来就成功，失败在真握手），故这里起真回环
    // 服务器实测协议：socks5 变体必须真说 socks5；http 变体必须**仍**说 HTTP（`probe-in-k` 是纯
    // `http` 入站，被误改成 socks5 会让测速在真机上全线超时——正是本批刻意不把两者合并的原因）。

    /// 极小 SOCKS5 服务器（RFC1928 no-auth + CONNECT），握手完成后**直接**在同一连接上
    /// 回 `body`（客户端此时认为隧道已建立，会在其上发明文 HTTP，正好被我们当请求读掉）。
    ///
    /// 仅支持一次连接、no-auth、CONNECT —— 够钉住「reqwest 到底说不说 socks5」这一件事。
    ///
    /// 第二个返回值把观察到的 **CONNECT 目标**（`(ATYP, 地址串)`）回传给用例：`0x03` = 域名原样
    /// 转交代理端解析（`socks5h`），`0x01` = 客户端已在本机解析成 IP（`socks5`）。这是
    /// 「解析归属」唯一能被自动断言的地面证据 —— 见
    /// [`HttpRuntime::via_local_socks_proxy`] 的「域名解析归属」小节。
    fn spawn_socks5_server(
        body: &'static str,
    ) -> (SocketAddr, std::sync::mpsc::Receiver<(u8, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        let (target_tx, target_rx) = std::sync::mpsc::channel::<(u8, String)>();
        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // 1) 问候：VER=5, NMETHODS=n, METHODS[n]。
            let mut head = [0u8; 2];
            if sock.read_exact(&mut head).is_err() || head[0] != 0x05 {
                return; // 首字节不是 0x05 ⇒ 客户端说的不是 socks5（正是回归形态）
            }
            let mut methods = vec![0u8; head[1] as usize];
            if sock.read_exact(&mut methods).is_err() {
                return;
            }
            // 2) 选 no-auth。
            if sock.write_all(&[0x05, 0x00]).is_err() {
                return;
            }
            // 3) 请求：VER=5, CMD=1(CONNECT), RSV=0, ATYP, ADDR, PORT。
            let mut req = [0u8; 4];
            if sock.read_exact(&mut req).is_err() || req[0] != 0x05 || req[1] != 0x01 {
                return;
            }
            let addr_len = match req[3] {
                0x01 => 4,
                0x04 => 16,
                0x03 => {
                    let mut n = [0u8; 1];
                    if sock.read_exact(&mut n).is_err() {
                        return;
                    }
                    n[0] as usize
                }
                _ => return,
            };
            let mut rest = vec![0u8; addr_len + 2]; // ADDR + PORT
            if sock.read_exact(&mut rest).is_err() {
                return;
            }
            // 回传 CONNECT 目标（域名 → 原样字符串；IPv4 → 点分十进制），供解析归属断言。
            let observed = match req[3] {
                0x03 => String::from_utf8_lossy(&rest[..addr_len]).into_owned(),
                0x01 => format!("{}.{}.{}.{}", rest[0], rest[1], rest[2], rest[3]),
                _ => String::new(),
            };
            let _ = target_tx.send((req[3], observed));
            // 4) 回成功（BND.ADDR=0.0.0.0:0，客户端不校验）。
            if sock
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .is_err()
            {
                return;
            }
            // 5) 隧道内的明文 HTTP 请求 → 直接回响应。
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        });
        (addr, target_rx)
    }

    #[tokio::test]
    async fn via_local_socks_proxy_really_speaks_socks5_to_a_socks_inbound() {
        // 变异验证：把 `via_local_socks_proxy` 的 scheme 改回 `http://` → reqwest 发的首字节是
        // 'C'(CONNECT) 而非 0x05 → 上面的 server 直接 return → 请求失败 → 本测试转红。
        // 摘掉 Cargo.toml 的 reqwest `socks` feature → `Proxy::all` 直接 Err → 同样转红。
        let (addr, _targets) = spawn_socks5_server("OK-VIA-SOCKS");
        let rt = HttpRuntime::via_local_socks_proxy(addr.port())
            .expect("建经本机 socks 代理 client 不得失败（socks feature 未启用时会在此 Err）");
        // 目标用回环地址：本地解析无需真 DNS，且请求真正落到上面的 socks server 上。
        let r = rt
            .fetch("http://127.0.0.1:9/sub", &FetchInit::default())
            .await
            .expect("经 socks5 代理的请求应成功（http:// 代理打 socks 入站必失败）");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"OK-VIA-SOCKS");
    }

    /// **变异锁（解析归属）**：把 `via_local_socks_proxy` 的 scheme 从 `socks5h://` 改回
    /// `socks5://` → reqwest 走 `DnsResolve::Local`、先在本机把域名解析成 IP 再发 ATYP=0x01
    /// → 本用例的 `atyp == 0x03` 断言当场转红（且 `sub.invalid` 本地解析必失败，请求直接 Err）。
    ///
    /// 这条钉的是功能语义而非协议语义：用户开「经代理更新订阅」正是因为**本地**解析不可信
    /// （被污染/被封锁/泄漏域名），域名必须原样交给 sing-box 由代理端解析。
    #[tokio::test]
    async fn via_local_socks_proxy_hands_the_hostname_to_the_proxy_not_the_local_resolver() {
        let (addr, targets) = spawn_socks5_server("OK-REMOTE-DNS");
        let rt =
            HttpRuntime::via_local_socks_proxy(addr.port()).expect("建经本机 socks 代理 client");
        // `.invalid` 是 RFC 2606 保留 TLD：本机**永远解析不出来**（不碰宿主 DNS、不出网）。
        // 本地解析变体会在这里直接失败；代理端解析变体则把域名原样发给上面的 mock server。
        let r = rt
            .fetch("http://sub.invalid/list", &FetchInit::default())
            .await
            .expect("socks5h 不做本地解析 → 请求应到达 mock socks server");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"OK-REMOTE-DNS");

        let (atyp, target) = targets
            .recv_timeout(Duration::from_secs(5))
            .expect("mock socks server 应观测到 CONNECT 目标");
        assert_eq!(
            atyp, 0x03,
            "必须 ATYP=DOMAIN（域名交代理端解析）；实得 {atyp:#04x} 说明客户端在本机解析了"
        );
        assert_eq!(target, "sub.invalid", "域名须原样转交，不得被本机改写成 IP");
    }

    #[tokio::test]
    async fn via_local_proxy_still_speaks_plain_http_to_an_http_inbound() {
        // 反向门：`probe-in-k` / `probe-*-in` 是 sing-box **纯 `http`** 入站（`inbounds.rs`
        // `http_loopback`），测速探测池全靠它。若有人图省事把 `via_local_proxy` 也改成 socks5
        // 「统一一下」，真机上测速会全线超时而单测毫无察觉 —— 本门就是拦这个。
        //
        // 断言方式：HTTP 代理的请求行是**绝对 URI**（`GET http://host/path HTTP/1.1`），
        // socks5 则是先发 0x05 二进制握手。直接看首字节/请求行即可区分。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = sock.flush();
            }
        });

        let rt = HttpRuntime::via_local_proxy(addr.port()).expect("建经本机 http 代理 client");
        let r = rt
            .fetch("http://example.invalid/probe", &FetchInit::default())
            .await
            .expect("经 http 代理的请求应成功");
        assert_eq!(r.status, 200);

        let first_request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("代理服务器应收到请求");
        assert!(
            first_request.starts_with("GET http://example.invalid/probe "),
            "http 入站要求绝对 URI 的明文 HTTP 请求行（socks5 会先发 0x05 二进制握手），实得: {:?}",
            first_request.lines().next()
        );
    }

    // ── 下载适配器：纯函数门 ─────────────────────────────────────────────────

    #[test]
    fn classify_403_ratelimit_is_distinguishable_from_403_forbidden() {
        // 二者表象同、处置相反（限流=等一会就好；无权限=永远不好）→ 消息必须能区分。
        let limited =
            classify_download_status(403, &[("x-ratelimit-remaining".into(), "0".into())])
                .expect("403 必须是错误");
        assert!(
            format!("{limited}").contains("限流"),
            "限流 403 的消息应点明限流，实得: {limited}"
        );

        let forbidden = classify_download_status(403, &[]).expect("403 必须是错误");
        assert!(matches!(forbidden, DownloadError::HttpStatus(403)));
    }

    #[test]
    fn classify_2xx_is_not_an_error() {
        assert!(classify_download_status(200, &[]).is_none());
        assert!(classify_download_status(204, &[]).is_none());
        assert!(matches!(
            classify_download_status(500, &[]),
            Some(DownloadError::HttpStatus(500))
        ));
    }

    #[test]
    fn gh_mirror_is_fallback_not_primary_and_only_for_github() {
        let http = Arc::new(HttpRuntime::new().unwrap());
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let dl = CoreDownloader::new(http, handle).with_gh_proxy("https://ghproxy.net/");

        let gh = dl.candidates("https://github.com/a/b/releases/download/v1/core.zip");
        assert_eq!(gh.len(), 2, "GitHub 资产应有原址 + 镜像两个候选");
        assert!(gh[0].starts_with("https://github.com/"), "原址必须**优先**");
        assert!(gh[1].starts_with("https://ghproxy.net/https://github.com/"));

        // 非 GitHub 地址不套镜像（gh 镜像只代理 GitHub）。
        let other = dl.candidates("https://example.com/core.zip");
        assert_eq!(other, vec!["https://example.com/core.zip".to_string()]);
    }

    #[test]
    fn no_gh_proxy_configured_means_no_mirror_candidate() {
        let http = Arc::new(HttpRuntime::new().unwrap());
        let handle = tokio::runtime::Runtime::new().unwrap().handle().clone();
        let dl = CoreDownloader::new(http, handle);
        assert_eq!(
            dl.candidates("https://github.com/a/b/core.zip").len(),
            1,
            "未配前缀时不得凭空造镜像地址"
        );
    }

    // ── 下载适配器：真 socket 门 ─────────────────────────────────────────────

    #[test]
    fn download_follows_redirect_and_returns_bytes() {
        // 下载路径**必须**自己跟随 30x（GitHub 资产必然 302 到 objects.githubusercontent.com），
        // 因为 client 全局关了 redirect。若把 `open_download_response` 的 30x 分支删掉 → 本测试转红
        // （U1 把重定向跟随从 `download_once` 抽进了两条腿共用的 `open_download_response`，
        //  故锚点是后者 —— 与下方 `streaming_download_follows_redirects_and_hashes_while_writing`
        //  的措辞对齐）。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let addr = spawn_server(vec![
            // 首跳 302 → 同 server 的 /asset
            b"HTTP/1.1 302 Found\r\nLocation: /asset\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            http_response("200 OK", &[], b"CORE-BYTES"),
        ]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        // 在 blocking 线程上调（契约：download 是同步桥）。
        let bytes = std::thread::spawn(move || dl.download(&format!("http://{addr}/start")))
            .join()
            .unwrap()
            .expect("下载应成功");
        assert_eq!(bytes, b"CORE-BYTES");
    }

    /// 🟡 **进度回调必须真被逐 chunk 调用，且带上 Content-Length 分母**。
    ///
    /// 此前 `CoreDownloader::download` 只返 `Vec<u8>`，`update_download` 因此只能发
    /// `downloading(0%)` / `downloaded(100%)` 两点，进度条整段 indeterminate。
    ///
    /// **变异锁**：把 `read_body_capped_with_progress` 里的 `cb(...)` 删掉、或把 `expected`
    /// 换成 `None` 传下去 ⇒ 本条转红。
    #[test]
    fn download_with_progress_reports_received_and_total() {
        use std::sync::Mutex as StdMutex;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = vec![b'z'; 3000];
        let total = body.len() as u64;
        let addr = spawn_server(vec![http_response("200 OK", &[], &body)]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());

        /// 一次进度回调的观测记录：`(已收字节, Content-Length)`。
        type ProgressLog = Arc<StdMutex<Vec<(u64, Option<u64>)>>>;
        let seen: ProgressLog = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let cb: Arc<DownloadProgressFn> = Arc::new(move |received, expected| {
            sink.lock().unwrap().push((received, expected));
        });

        let bytes = std::thread::spawn(move || {
            dl.download_with_progress(&format!("http://{addr}/asset"), cb)
        })
        .join()
        .unwrap()
        .expect("下载应成功");
        assert_eq!(bytes.len(), body.len());

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "进度回调一次都没触发 = 进度腿是死的");
        assert_eq!(
            seen.last().copied(),
            Some((total, Some(total))),
            "末次回调须是 (总字节, Content-Length)——分母缺失则百分比算不出来"
        );
        // 单调不减（received 是累计值，不是增量）。
        assert!(
            seen.windows(2).all(|w| w[0].0 <= w[1].0),
            "received 必须是累计值：{seen:?}"
        );
    }

    #[test]
    fn download_without_progress_still_works_trait_path_unchanged() {
        // 门：加进度腿不得改变既有 trait 路径的行为（staged 周期走的就是它）。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let addr = spawn_server(vec![http_response("200 OK", &[], b"PLAIN")]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        let bytes = std::thread::spawn(move || dl.download(&format!("http://{addr}/x")))
            .join()
            .unwrap()
            .expect("无进度回调路径应照常成功");
        assert_eq!(bytes, b"PLAIN");
    }

    #[test]
    fn download_incomplete_body_is_reported_not_silently_accepted() {
        // Content-Length 撒谎（说 100，实给 5）→ 必须报 Incomplete，**不得**把半截字节当成功返回
        // （半截核二进制过了 SHA256 校验才被发现 = 白下一次；更糟的是若没校验就落位）。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
                );
                let _ = sock.flush();
            }
        });
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        let err = std::thread::spawn(move || dl.download(&format!("http://{addr}/x")))
            .join()
            .unwrap()
            .expect_err("Content-Length 不符必须报错");
        assert!(
            matches!(
                err,
                DownloadError::Incomplete {
                    received: 5,
                    expected: 100
                }
            ),
            "应报 Incomplete{{received:5,expected:100}}，实得: {err:?}"
        );
    }

    // ── 流式落盘腿（U1）───────────────────────────────────────────────────────
    //
    // 全部走本文件既有的**回环** mock server（`spawn_server`，见其上方注释：stdlib TcpListener
    // 绑 127.0.0.1，不触碰宿主网络），与内存腿的门同形态。

    /// 收进内存的假 sink（测试用）：验「写进去的字节 == 服务端发的字节」。
    #[derive(Clone, Default)]
    struct MemSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for MemSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 写到第 `fail_after` 字节就报错的 sink（测「写盘失败**不得**冒充下载不完整」）。
    struct FailingSink {
        written: usize,
        fail_after: usize,
    }

    impl std::io::Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written + buf.len() > self.fail_after {
                return Err(std::io::Error::other("mock: disk full"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 🟡 **流式腿必须与内存腿走同一条编排：重定向照跟、字节一个不少、摘要边写边算。**
    ///
    /// **变异探针**：把 `download_once_to_sink` 里的 `open_download_response` 换成一份
    /// 自己复制的请求逻辑（漏掉 30x 分支）⇒ 本条转红。
    #[test]
    fn streaming_download_follows_redirects_and_hashes_while_writing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'q'; 5000];
        let addr = spawn_server(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /asset\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            http_response("200 OK", &[], &payload),
        ]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());

        let sink = MemSink::default();
        let sink_for_factory = sink.clone();
        let factory: Arc<DownloadSinkFactory> =
            Arc::new(move || Ok(Box::new(sink_for_factory.clone())));
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});

        let out = std::thread::spawn(move || {
            dl.download_to_sink_with_progress(&format!("http://{addr}/start"), factory, noop)
        })
        .join()
        .unwrap()
        .expect("流式下载应成功");

        assert_eq!(out.bytes, payload.len() as u64);
        assert_eq!(
            *sink.0.lock().unwrap(),
            payload,
            "写进 sink 的字节必须与服务端发的逐字相同"
        );
        assert_eq!(
            out.sha256_hex,
            polaris_updater::verify::sha256_hex(&payload),
            "边写边算的摘要必须等于整包摘要"
        );
    }

    /// 🟡 **进度回调在流式腿上必须与内存腿同时机同分母。**
    ///
    /// 前端契约零改动的前提就是这条：回调仍在每个 chunk 到达时以 `(累计已收, Content-Length)` 触发。
    ///
    /// **变异探针**：把 sink 版循环里的 `cb(...)` 删掉、或把 `expected` 换成 `None` ⇒ 转红。
    #[test]
    fn streaming_download_reports_progress_like_the_memory_leg() {
        use std::sync::Mutex as StdMutex;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'w'; 4000];
        let total = payload.len() as u64;
        let addr = spawn_server(vec![http_response("200 OK", &[], &payload)]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());

        /// 一次进度回调的观测记录：`(已收字节, Content-Length)`（同内存腿那条门）。
        type ProgressLog = Arc<StdMutex<Vec<(u64, Option<u64>)>>>;
        let seen: ProgressLog = Arc::new(StdMutex::new(Vec::new()));
        let log = seen.clone();
        let cb: Arc<DownloadProgressFn> = Arc::new(move |received, expected| {
            log.lock().unwrap().push((received, expected));
        });
        let factory: Arc<DownloadSinkFactory> = Arc::new(|| Ok(Box::new(std::io::sink())));

        std::thread::spawn(move || {
            dl.download_to_sink_with_progress(&format!("http://{addr}/asset"), factory, cb)
        })
        .join()
        .unwrap()
        .expect("流式下载应成功");

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "进度回调一次都没触发 = 进度腿是死的");
        assert_eq!(
            seen.last().copied(),
            Some((total, Some(total))),
            "末次回调须是 (总字节, Content-Length)——分母缺失则百分比算不出来"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 <= w[1].0),
            "received 必须是累计值：{seen:?}"
        );
    }

    /// 🟡 **Content-Length 撒谎在流式腿上照样报 Incomplete（不得因为字节已落盘就当成功）。**
    #[test]
    fn streaming_download_reports_incomplete_body_too() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
                );
                let _ = sock.flush();
            }
        });
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        let factory: Arc<DownloadSinkFactory> = Arc::new(|| Ok(Box::new(std::io::sink())));
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});

        let err = std::thread::spawn(move || {
            dl.download_to_sink_with_progress(&format!("http://{addr}/x"), factory, noop)
        })
        .join()
        .unwrap()
        .expect_err("Content-Length 不符必须报错");
        assert!(
            matches!(
                err,
                DownloadError::Incomplete {
                    received: 5,
                    expected: 100
                }
            ),
            "应报 Incomplete{{received:5,expected:100}}，实得: {err:?}"
        );
    }

    /// 🟡 **写盘失败必须原样报 IO，绝不冒充「下载不完整」。**
    ///
    /// 磁盘满被报成 Incomplete ⇒ 上层判「网络把包送少了」→ 引导用户一遍遍重下一个永远装不满的盘。
    ///
    /// **变异探针**：把 [`BodyReadError::Sink`] 折叠进 [`BodyReadError::Io`] ⇒ 本条转红
    /// （会变成 `Incomplete`，因为服务端给了 Content-Length）。
    #[test]
    fn sink_write_failure_is_reported_as_io_not_incomplete_download() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'p'; 8000];
        let addr = spawn_server(vec![http_response("200 OK", &[], &payload)]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        // 只允许写 10 字节 → 必然在中途失败（服务端要发 8000）。
        let factory: Arc<DownloadSinkFactory> = Arc::new(|| {
            Ok(Box::new(FailingSink {
                written: 0,
                fail_after: 10,
            }))
        });
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});

        let err = std::thread::spawn(move || {
            dl.download_to_sink_with_progress(&format!("http://{addr}/asset"), factory, noop)
        })
        .join()
        .unwrap()
        .expect_err("写盘失败必须报错");
        assert!(
            matches!(err, DownloadError::Io(_)),
            "写盘失败必须报 Io（磁盘满不是「下载不完整」），实得: {err:?}"
        );
        assert!(format!("{err}").contains("disk full"), "实得: {err}");
        // `BodyReadError::Sink.received` 必须**真被消费**（此前它构造后即被 `..` 丢弃 = 死字段）：
        // 磁盘满的诊断价值一半在「写到第几字节才满」。
        assert!(
            format!("{err}").contains("已写出"),
            "写盘失败的消息须带上已写出的字节数，实得: {err}"
        );
    }

    /// 独占的临时目录（本 crate 无 tempfile dev-dep；用完即删，同 `commands/updater.rs` 的形态）。
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "polaris-http-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 🟡 **镜像回退必须让下一个候选从「空文件」重写，绝不把字节接在上一次的残料后面。**
    ///
    /// [`DownloadSinkFactory`] 传**工厂**而非句柄，唯一理由就是这条。而此前唯一验字节的用例
    /// （`streaming_download_follows_redirects_and_hashes_while_writing`）用的是共享
    /// `Arc<Mutex<Vec<u8>>>` 的 `MemSink`：工厂每次返回的「新句柄」共享同一个 buffer 且**不截断**，
    /// 于是「换候选时句柄到底有没有被截断」在结构上根本检不出 —— 实现正确但零覆盖。
    ///
    /// 本条用**真文件**（生产就是 `StdFs::open_write` → `File::create`）复现真实回退形态：
    /// 首候选写出 5 字节后报 Incomplete，次候选返完整包。
    ///
    /// **变异探针**：把 `StdFs::open_write` 的 `File::create` 换成
    /// `OpenOptions::new().append(true).create(true)` ⇒ 盘上变成 `short` + payload，
    /// 三条断言（字节数 / 逐字内容 / sha256）同时转红。
    #[test]
    fn mirror_fallback_rewrites_the_sink_from_scratch() {
        use polaris_updater::traits::UpdateFs;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'm'; 3000];
        let addr = spawn_server(vec![
            // 首候选：声明 100 字节只发 5 → 写出 5 字节后判 Incomplete（镜像回退的真实触发形态）。
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".to_vec(),
            // 次候选：完整包。
            http_response("200 OK", &[], &payload),
        ]);

        // 候选表要产出「原址 + 镜像」两条，故 host 必须是 GitHub 资产域名；用 resolve 覆盖把它们
        // 都钉到回环 mock server 上（**端口必须写在 URL 里** —— reqwest 的 resolve 覆盖只改 IP，
        // 端口取自 URL）。不出网。
        let http = Arc::new(
            HttpRuntime::with_resolve_overrides(&[
                ("github.com", addr),
                ("ghmirror.invalid", addr),
            ])
            .unwrap(),
        );
        let url = format!("http://github.com:{}/a/b/core.zip", addr.port());
        let dl = CoreDownloader::new(http, rt.handle().clone())
            .with_gh_proxy(format!("http://ghmirror.invalid:{}/", addr.port()));
        assert_eq!(dl.candidates(&url).len(), 2, "本用例的前提是真有两个候选");

        let dir = scratch("mirror-fallback");
        let tmp = dir.join("update.pkg.polaris-new");
        let created = Arc::new(AtomicUsize::new(0));
        let factory: Arc<DownloadSinkFactory> = {
            let (tmp, created) = (tmp.clone(), created.clone());
            Arc::new(move || {
                created.fetch_add(1, Ordering::SeqCst);
                polaris_updater::traits::StdFs.open_write(&tmp)
            })
        };
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});

        let out =
            std::thread::spawn(move || dl.download_to_sink_with_progress(&url, factory, noop))
                .join()
                .unwrap()
                .expect("首候选失败后应由镜像候选把包下完");

        assert_eq!(
            created.load(Ordering::SeqCst),
            2,
            "换候选必须**重新**建句柄（传工厂的全部意义）"
        );
        assert_eq!(
            out.bytes,
            payload.len() as u64,
            "字节数带上了首候选的残料 = 句柄没被截断"
        );
        assert_eq!(
            std::fs::read(&tmp).unwrap(),
            payload,
            "盘上必须**只有**次候选的完整包，不得是 `short` + payload"
        );
        assert_eq!(
            out.sha256_hex,
            polaris_updater::verify::sha256_hex(&payload),
            "摘要必须算在干净的那一份上（否则落位校验会对着一份拼接内容成立）"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 🟡 **流式腿的读侧超限：与内存腿同一条判定，且已写出的部分绝不算成功。**
    ///
    /// 刻意**不给 Content-Length** ⇒ `open_download_response` 的预检不参与，只剩
    /// [`read_body_to_sink_with_progress`] 的读侧闸 —— 那正是内存腿有门、流式腿此前没门的一格。
    ///
    /// **变异探针**：删掉 sink 版循环里的 `if received + chunk.len() > limit` 判定 ⇒ 转红。
    #[test]
    fn streaming_leg_rejects_an_oversized_body_on_the_read_side_too() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'o'; 4096];
        // 无 Content-Length（靠 close 定界）⇒ 预检无从早拒。
        let mut raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        raw.extend_from_slice(&payload);
        let addr = spawn_server(vec![raw]);

        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone()).with_max_bytes(1024);
        let sink = MemSink::default();
        let sink_for_factory = sink.clone();
        let factory: Arc<DownloadSinkFactory> =
            Arc::new(move || Ok(Box::new(sink_for_factory.clone())));
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});

        let err = std::thread::spawn(move || {
            dl.download_to_sink_with_progress(&format!("http://{addr}/big"), factory, noop)
        })
        .join()
        .unwrap()
        .expect_err("超本腿闸值必须报错，不得把已落盘的部分当成功");
        assert!(format!("{err}").contains("超过上限 1024"), "实得: {err}");

        let written = sink.0.lock().unwrap().len();
        assert!(
            written <= 1024 && written < payload.len(),
            "超限那一 chunk 不得写出去（实得已写 {written} 字节）"
        );
    }

    /// 🟡 **流式腿的停滞看门狗：与内存腿同一条判定，且已写出的部分绝不算成功。**
    ///
    /// 走**直调**而非端到端：`STALL_TIMEOUT` 是 30s 常量，端到端跑要等半分钟；
    /// 而 [`read_body_to_sink_with_progress`] 的 `stall` 本就是形参，直调即可注入 150ms。
    ///
    /// **变异探针**：把 sink 版循环里的 `tokio::time::timeout(stall, resp.chunk())` 换成裸
    /// `resp.chunk().await` ⇒ 本条挂死（超时转红）；把 `Stalled` 折叠进 `Io` ⇒ 首条断言转红。
    #[tokio::test]
    async fn streaming_leg_reports_a_stall_without_accepting_the_partial_write() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = Arc::new(AtomicBool::new(true));
        let hold_srv = hold.clone();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                // 发头 + 一小段 body 后**不再发也不关连接** —— 关了就变成 Io/Incomplete，测不到 Stalled。
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npart",
                );
                let _ = sock.flush();
                while hold_srv.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });

        let rt = HttpRuntime::new().unwrap();
        let mut resp = rt
            .client()
            .get(format!("http://{addr}/slow"))
            .send()
            .await
            .expect("首字节应到达");
        let sink = MemSink::default();
        let mut sink_handle = sink.clone();
        let err = read_body_to_sink_with_progress(
            &mut resp,
            Some(1024),
            Duration::from_millis(150),
            Some(100),
            None,
            &mut sink_handle,
        )
        .await
        .expect_err("两个 chunk 之间超过看门狗间隔必须报停滞");

        assert!(
            matches!(err, BodyReadError::Stalled),
            "停滞必须报 Stalled（折叠进 Io 会被还原成「下载不完整」，引导用户白重下），实得: {err:?}"
        );
        assert_eq!(
            sink.0.lock().unwrap().len(),
            4,
            "停滞前已写出的字节留在 sink 里（清残件是调用方的责任），但结论必须是失败"
        );
        // 与内存腿共用同一条映射（`DownloadError::Stalled`，不是 Incomplete）。
        assert!(matches!(
            map_body_error(err, Some(100)),
            DownloadError::Stalled(_)
        ));
        hold.store(false, Ordering::Relaxed);
    }

    /// 🟡 **三处体积闸都是「严格大于才拒」——恰好等于上限的包必须放行。**
    ///
    /// 三处判定（Content-Length 预检 / 内存腿读侧 / 流式腿读侧）今日全用 `>`，但**此前无任何测试
    /// 钉住边界**：现有的门一律拿 `limit + 1` 去撞，把任一处改成 `>=` 全套仍绿。
    ///
    /// 本条现在**是 App 腿的地基**（2026-08-17 订正：原写「App 腿的闸值是『声明值 + 裕度』」，
    /// 那个裕度已删）。`commands/updater.rs::app_update_size_limit` 现在让闸值**恰好等于**
    /// 清单声明的 `fileSize`，删裕度不卡正常包的**全部理由**就是这三处的严格大于语义 ——
    /// 任一处改成 `>=`，一个大小正好等于声明值的正常安装包就会被拒，且失败长得像
    /// 「服务端给多了」。两边互引，口径必须一致。
    ///
    /// **变异探针**：把三处任一的 `>` 改成 `>=` ⇒ 对应那一段转红。
    #[test]
    fn size_limit_boundary_admits_a_body_of_exactly_the_limit() {
        const LIMIT: usize = 2048;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'e'; LIMIT];

        // ① Content-Length 预检：`n > max_bytes` —— CL 恰好等于闸值必须放行。
        let addr = spawn_server(vec![http_response("200 OK", &[], &payload)]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone()).with_max_bytes(LIMIT);
        let bytes = std::thread::spawn(move || dl.download(&format!("http://{addr}/exact")))
            .join()
            .unwrap()
            .expect("Content-Length 恰好等于闸值不得被预检拒掉");
        assert_eq!(bytes.len(), LIMIT);

        // ② 内存腿读侧：`buf.len() + chunk.len() > limit`。无 Content-Length ⇒ 预检不参与。
        let mut raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        raw.extend_from_slice(&payload);
        let addr2 = spawn_server(vec![raw.clone()]);
        let http2 = Arc::new(HttpRuntime::new().unwrap());
        let dl2 = CoreDownloader::new(http2, rt.handle().clone()).with_max_bytes(LIMIT);
        let bytes2 = std::thread::spawn(move || dl2.download(&format!("http://{addr2}/exact")))
            .join()
            .unwrap()
            .expect("读侧累计恰好等于闸值不得被拒（内存腿）");
        assert_eq!(bytes2.len(), LIMIT);

        // ③ 流式腿读侧：`received + chunk.len() > limit`，同上。
        let addr3 = spawn_server(vec![raw]);
        let http3 = Arc::new(HttpRuntime::new().unwrap());
        let dl3 = CoreDownloader::new(http3, rt.handle().clone()).with_max_bytes(LIMIT);
        let sink = MemSink::default();
        let sink_for_factory = sink.clone();
        let factory: Arc<DownloadSinkFactory> =
            Arc::new(move || Ok(Box::new(sink_for_factory.clone())));
        let noop: Arc<DownloadProgressFn> = Arc::new(|_, _| {});
        let out = std::thread::spawn(move || {
            dl3.download_to_sink_with_progress(&format!("http://{addr3}/exact"), factory, noop)
        })
        .join()
        .unwrap()
        .expect("读侧累计恰好等于闸值不得被拒（流式腿）");
        assert_eq!(out.bytes, LIMIT as u64);
        assert_eq!(sink.0.lock().unwrap().len(), LIMIT);
    }

    /// 🟡 **体积闸是**形参**：内核腿的 16MiB 与 App 腿的清单闸互不干扰。**
    ///
    /// **变异探针**：把 `open_download_response` 里的 `self.max_bytes` 改回常量
    /// `MAX_DOWNLOAD_BYTES` ⇒ 第一条断言转红（一个 1KiB 的闸拦不住 2KiB 的响应）。
    #[test]
    fn per_leg_size_limit_is_honoured_not_the_global_constant() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![b'x'; 2048];

        // ① 闸收紧到 1 KiB → Content-Length 预检就该早拒（远小于全局 16MiB 常量）。
        let addr = spawn_server(vec![http_response("200 OK", &[], &payload)]);
        let http = Arc::new(HttpRuntime::new().unwrap());
        let tight = CoreDownloader::new(http, rt.handle().clone()).with_max_bytes(1024);
        let err = std::thread::spawn(move || tight.download(&format!("http://{addr}/asset")))
            .join()
            .unwrap()
            .expect_err("超本腿闸值必须早拒");
        assert!(format!("{err}").contains("超过上限"), "实得: {err}");

        // ② 闸放宽到远超全局常量 → 同一份响应照常通过（证明常量已不再是硬编码的天花板）。
        let addr2 = spawn_server(vec![http_response("200 OK", &[], &payload)]);
        let http2 = Arc::new(HttpRuntime::new().unwrap());
        let loose =
            CoreDownloader::new(http2, rt.handle().clone()).with_max_bytes(MAX_DOWNLOAD_BYTES * 8);
        let bytes = std::thread::spawn(move || loose.download(&format!("http://{addr2}/asset")))
            .join()
            .unwrap()
            .expect("闸放宽后应成功");
        assert_eq!(bytes.len(), payload.len());
    }

    /// 失败分类映射是纯函数，两个消费端共用同一条 —— 逐条钉住。
    #[test]
    fn body_error_maps_to_the_same_download_error_for_both_legs() {
        assert!(matches!(
            map_body_error(BodyReadError::Stalled, Some(10)),
            DownloadError::Stalled(_)
        ));
        assert!(
            format!("{}", map_body_error(BodyReadError::TooLarge(99), None)).contains("超过上限")
        );
        // Io + 已知 Content-Length → Incomplete（结构化，不是泛化 Other）。
        assert!(matches!(
            map_body_error(
                BodyReadError::Io {
                    received: 5,
                    message: "boom".into()
                },
                Some(100)
            ),
            DownloadError::Incomplete {
                received: 5,
                expected: 100
            }
        ));
        // Io + 长度未知 → Other（算不出 Incomplete 就不许编一个）。
        assert!(matches!(
            map_body_error(
                BodyReadError::Io {
                    received: 5,
                    message: "boom".into()
                },
                None
            ),
            DownloadError::Other(_)
        ));
        // 写盘失败即便有 Content-Length 也**不得**变成 Incomplete。
        assert!(matches!(
            map_body_error(
                BodyReadError::Sink {
                    received: 5,
                    source: std::io::Error::other("disk full")
                },
                Some(100)
            ),
            DownloadError::Io(_)
        ));
    }

    #[test]
    fn content_length_check_is_shared_by_both_legs() {
        assert!(check_content_length(100, Some(100)).is_ok());
        assert!(check_content_length(100, None).is_ok(), "长度未知即不判");
        assert!(matches!(
            check_content_length(5, Some(100)),
            Err(DownloadError::Incomplete {
                received: 5,
                expected: 100
            })
        ));
        assert!(
            check_content_length(101, Some(100)).is_err(),
            "多给也是不完整（服务端撒谎/被注入）"
        );
    }

    #[test]
    fn download_rejects_oversized_content_length_before_reading_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let big = MAX_DOWNLOAD_BYTES + 1;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {big}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                let _ = sock.flush();
            }
        });
        let http = Arc::new(HttpRuntime::new().unwrap());
        let dl = CoreDownloader::new(http, rt.handle().clone());
        let err = std::thread::spawn(move || dl.download(&format!("http://{addr}/big")))
            .join()
            .unwrap()
            .expect_err("超 16MiB 必须早拒");
        assert!(format!("{err}").contains("超过上限"), "实得: {err}");
    }

    // ── 换核链路：下载半程的生产组合面门 ─────────────────────────────────────
    //
    // §K7.1 纪律 + updater 批的等待项：真 CoreDownloader（真 reqwest）**真被注入**
    // CoreStagedUpdater → 真下载 → 真 SHA256 校验 → 真落位。
    // **未覆盖**（如实登记）：真机换核落位到受保护核目录需 helper 特权写 + ProxyRuntime 停起协同，
    // 本机不验（破坏宿主）。本门只证「下载半程」在生产路径上真能跑通，不冒充全链闭环。

    #[test]
    fn core_swap_download_half_real_downloader_injected_into_staged_updater() {
        use polaris_updater::manifest::VersionManifestEntry;
        use polaris_updater::staged::{
            ApplyOutcome, CoreStagedUpdater, MemoryStateStore, StagedConfig,
        };
        use polaris_updater::traits::StdFs;
        use polaris_updater::verify::sha256_hex_lower;

        let rt = tokio::runtime::Runtime::new().unwrap();
        // 真核字节（内容任意，关键是 SHA256 真校验）。
        let core_bytes = b"POLARIS-FAKE-CORE-BINARY-BYTES".to_vec();
        let sha = sha256_hex_lower(&core_bytes);

        // 真回环 server 服核字节。
        let body = core_bytes.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                resp.extend_from_slice(&body);
                let _ = sock.write_all(&resp);
                let _ = sock.flush();
            }
        });

        let dest =
            std::env::temp_dir().join(format!("polaris-coreswap-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);

        let entry = VersionManifestEntry {
            version: "9.9.9".into(), // 远新于 current → 过版本闸
            url: format!("http://{addr}/sing-box.zip"),
            sha256: Some(sha),
            prerelease: false,
            notes: String::new(),
        };

        // ── 真 CoreDownloader 注入 CoreStagedUpdater ──
        let http = Arc::new(HttpRuntime::new().unwrap());
        let downloader = CoreDownloader::new(http, rt.handle().clone());
        let fs = StdFs;
        let store = MemoryStateStore::default();
        let mut cfg = StagedConfig::new(&dest);
        cfg.restrict_band = false; // 手动路径允许跨带（本门测的是下载+校验+落位，非带闸）

        // apply 是同步（内部 download 会 spawn 到 rt.handle 并阻塞等）——须在 blocking 线程调，
        // 不能占用 rt 的 worker。
        let dest2 = dest.clone();
        let outcome = std::thread::spawn(move || {
            let updater = CoreStagedUpdater::new(&downloader, &fs, &store, cfg);
            updater.apply(&entry, "1.0.0", "sing-box")
        })
        .join()
        .unwrap()
        .expect("下载半程应成功（真 socket 收字节 + SHA256 校验过 + 落位）");

        assert_eq!(
            outcome,
            ApplyOutcome::Applied,
            "真下载器注入后，下载→校验→落位应 Applied"
        );
        // 真落位物：dest/sing-box 存在且字节 = 我们服的真核字节。
        let landed = std::fs::read(dest2.join("sing-box")).expect("落位的核应可读");
        assert_eq!(landed, core_bytes, "落位字节须与下载字节逐字节相同");
        let _ = std::fs::remove_dir_all(&dest2);
    }

    #[test]
    fn core_swap_download_half_sha256_mismatch_is_rejected_not_landed() {
        // 变异/安全门：manifest 的 sha256 与真下载字节不符（中间人篡改形态）→ 必须拒绝落位。
        // 证明「下载成功」不等于「盲信落位」——SHA256 校验真的在生产下载路径后面把关。
        use polaris_updater::manifest::VersionManifestEntry;
        use polaris_updater::staged::{CoreStagedUpdater, MemoryStateStore, StagedConfig};
        use polaris_updater::traits::StdFs;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = b"REAL-BYTES".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                resp.extend_from_slice(&body);
                let _ = sock.write_all(&resp);
                let _ = sock.flush();
            }
        });

        let dest =
            std::env::temp_dir().join(format!("polaris-coreswap-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let entry = VersionManifestEntry {
            version: "9.9.9".into(),
            url: format!("http://{addr}/sing-box.zip"),
            // 故意给错 hash（64 个 a）。
            sha256: Some("a".repeat(64)),
            prerelease: false,
            notes: String::new(),
        };
        let http = Arc::new(HttpRuntime::new().unwrap());
        let downloader = CoreDownloader::new(http, rt.handle().clone());
        let fs = StdFs;
        let store = MemoryStateStore::default();
        let mut cfg = StagedConfig::new(&dest);
        cfg.restrict_band = false;

        let dest2 = dest.clone();
        let res = std::thread::spawn(move || {
            let updater = CoreStagedUpdater::new(&downloader, &fs, &store, cfg);
            updater.apply(&entry, "1.0.0", "sing-box")
        })
        .join()
        .unwrap();

        assert!(res.is_err(), "SHA256 不符必须报错，不得落位");
        assert!(
            !dest2.join("sing-box").exists(),
            "校验失败时核**不得**落到 dest（否则坏核入库）"
        );
        let _ = std::fs::remove_dir_all(&dest2);
    }

    // ── UnlockHttp 契约门（已迁出）───────────────────────────────────────────
    //
    // 原先这里有 4 扇门（never-err / redirect_chain / cf-mitigated 捕获 / body 截断）。
    // `impl UnlockHttp for HttpRuntime` 迁到 `polaris-unlock-transport` 后，那些门**同步迁过去了**
    // 并且更强 —— 它们现在打的是**真正的生产传输**（wreq + Chrome 指纹），而不是一个已经不再
    // 服务解锁的 reqwest client。见 `crates/unlock-transport/src/lib.rs` 的 tests：
    //   never_errs_on_network_failure_it_reports_status_zero
    //   records_redirect_chain_without_following_automatically
    //   captures_cf_mitigated_header_for_challenge_detection
    //   truncates_body_rather_than_failing
    //   declared_accept_encoding_is_decodable_end_to_end（新增：解压门）
    //   browser_headers_reach_the_wire（新增：头集线级门）

    // ── WarpHttp 契约门 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn warp_status_request_preserves_4xx_for_classification() {
        // 契约关键：unregister **不抛错**，保留 4xx 让 classify_deregister_result 分类
        // （403 + body code 1020 → Retry，而非 Drop）。若这里跟 json_request 一样把非 2xx 变 Err，
        // 分类器就永远收不到 status → 1020 被误判成 Drop → 设备泄漏在 CF 侧。
        let addr = spawn_server(vec![http_response("403 Forbidden", &[], b"error 1020")]);
        let rt = HttpRuntime::new().unwrap();
        let resp = rt
            .status_request(&WarpHttpRequest {
                method: WarpHttpMethod::Delete,
                url: format!("http://{addr}/reg/dev"),
                headers: Default::default(),
                body: None,
            })
            .await
            .expect("status_request 对 4xx 必须 Ok（保留状态），不得 Err");
        assert_eq!(resp.status, 403);
        assert_eq!(resp.body, "error 1020");
    }

    #[tokio::test]
    async fn warp_json_request_maps_non_2xx_to_err_with_status_and_body() {
        let addr = spawn_server(vec![http_response("403 Forbidden", &[], b"error 1020")]);
        let rt = HttpRuntime::new().unwrap();
        let err = rt
            .json_request(&WarpHttpRequest {
                method: WarpHttpMethod::Post,
                url: format!("http://{addr}/reg"),
                headers: Default::default(),
                body: Some("{}".into()),
            })
            .await
            .expect_err("register 非 2xx 必须 Err");
        assert!(err.contains("403"), "错误须带 status，实得: {err}");
        assert!(
            err.contains("1020"),
            "错误须带 body（CF 的 1020 在里面），实得: {err}"
        );
    }

    /// 🔵 **`warp_client` 惰性**：建 `HttpRuntime` 不得顺带建 WARP client。
    ///
    /// [`HttpRuntime::via_local_proxy`] 是**测速热路径**（每个被测节点一次），而 WARP client 与测速毫无
    /// 关系：Linux 上每建一个就读一次系统信任库（reqwest 0.13.4 → `rustls_platform_verifier`
    /// `others.rs:88-100`，十几到几十毫秒），Mac/Win 便宜但同样是白建。
    ///
    /// **变异锁**：把三个构造器里的 `OnceLock::new()` 换回 `build_warp_client()?` → 第一条断言转红。
    #[test]
    fn warp_client_is_not_built_until_a_warp_request_needs_it() {
        let rt = HttpRuntime::via_local_proxy(1080).expect("建经代理 client 不得失败");
        assert!(
            rt.warp_client.get().is_none(),
            "测速热路径的构造器不得顺带建 WARP client"
        );
        // 首次取用才建，且此后复用同一个（惰性 ≠ 每次重建）。
        let first: *const reqwest::Client = rt.warp_client().expect("按需构造必须成功");
        assert!(rt.warp_client.get().is_some(), "取用后必须已缓存");
        let second: *const reqwest::Client = rt.warp_client().expect("第二次取用必须成功");
        assert!(std::ptr::eq(first, second), "惰性构造必须只建一次");
    }

    // ── WARP TLS 指纹隔离门（FX-warp-tls / 审查表 row32）───────────────────────
    //
    // warp_client 必须钉 TLS1.2 + HTTP/1.1（对齐 上游 node-`https` 规避 CF 1020），且**独立于**共享 client。
    // 下列门在**线级**断言（真 reqwest 打真回环 socket、解析 ClientHello 字节）——reqwest 不暴露 client 的 TLS
    // 配置（读不到），且唯有线级才防得住「配置设了但 reqwest 未真正下发 pin」这类组合面漏（§K7.1）。

    /// 起一个回环 TCP listener，捕获客户端发来的**首个**请求字节（TLS ClientHello 或明文 HTTP），
    /// 经 channel 回传后随即关连接（握手/请求随之快速失败——本门只看已捕获字节，不需完成握手）。
    fn spawn_capture() -> (SocketAddr, std::sync::mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
        let addr = listener.local_addr().expect("取端口");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match sock.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            // TLS 握手记录（byte0=0x16，[3..5]=record len）完整即停。
                            if buf.len() >= 5 && buf[0] == 0x16 {
                                let rec = 5 + u16::from_be_bytes([buf[3], buf[4]]) as usize;
                                if buf.len() >= rec {
                                    break;
                                }
                            }
                            // 明文 HTTP：请求头结束（CRLFCRLF）即停。
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                            if buf.len() >= 16 * 1024 {
                                break;
                            }
                        }
                        Err(_) => break, // 读超时/连接错——回传已有字节。
                    }
                }
                let _ = tx.send(buf);
            }
        });
        (addr, rx)
    }

    /// 解析 ClientHello 扩展表 → (是否宣告 TLS1.3, ALPN 协议名列表)。**只**解析定位 1020 判据的两项：
    /// supported_versions(0x002b) 是否含 0x0304、ALPN(0x0010) 的协议名。字节级、无第三方 TLS 解析依赖。
    fn parse_client_hello(buf: &[u8]) -> (bool, Vec<Vec<u8>>) {
        // record(5) + handshake 头(4 = type1 + len3) + client_version(2) + random(32)
        let mut p = 5 + 4 + 2 + 32;
        if p + 1 > buf.len() {
            return (false, Vec::new());
        }
        let sid = buf[p] as usize; // session_id
        p += 1 + sid;
        if p + 2 > buf.len() {
            return (false, Vec::new());
        }
        let cs = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize; // cipher_suites
        p += 2 + cs;
        if p + 1 > buf.len() {
            return (false, Vec::new());
        }
        let comp = buf[p] as usize; // compression_methods
        p += 1 + comp;
        if p + 2 > buf.len() {
            return (false, Vec::new());
        }
        let ext_total = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
        p += 2;
        let end = (p + ext_total).min(buf.len());
        let mut offers_tls13 = false;
        let mut alpn: Vec<Vec<u8>> = Vec::new();
        while p + 4 <= end {
            let ty = u16::from_be_bytes([buf[p], buf[p + 1]]);
            let ln = u16::from_be_bytes([buf[p + 2], buf[p + 3]]) as usize;
            p += 4;
            if p + ln > end {
                break;
            }
            let data = &buf[p..p + ln];
            if ty == 0x002b && !data.is_empty() {
                // supported_versions: list_len(1) 后接 u16 版本表；0x0304 = TLS1.3。
                let list_len = data[0] as usize;
                let list = &data[1..(1 + list_len).min(data.len())];
                if list.chunks(2).any(|c| c == [0x03, 0x04]) {
                    offers_tls13 = true;
                }
            } else if ty == 0x0010 && data.len() >= 2 {
                // ALPN: list_len(2) 后接 [name_len(1) name..]。
                let mut q = 2;
                while q < data.len() {
                    let nl = data[q] as usize;
                    q += 1;
                    if q + nl > data.len() {
                        break;
                    }
                    alpn.push(data[q..q + nl].to_vec());
                    q += nl;
                }
            }
            p += ln;
        }
        (offers_tls13, alpn)
    }

    #[tokio::test]
    async fn warp_client_pins_tls12_and_http1_on_the_wire() {
        // 线级门：warp_send（经 WarpHttp impl）打出的 ClientHello 必须【不宣告 TLS1.3】且【ALPN 只 http/1.1】。
        //   变异①（载荷）：删 build_warp_client 的 .tls_version_max(TLS_1_2) → 出现 0x0304 → offers_tls13 转真 → 红。
        //   变异②：删 .http1_only() → 本构建无 http2 feature，ALPN 仍只 http/1.1，本门**不转红**（如实标注：
        //           http1_only 是前瞻护栏，其牙在开启 http2 feature 后才咬——见对照门注释）。
        //   变异③（隔离）：warp_send 改回 &self.client → shared 宣告 TLS1.3 → offers_tls13 转真 → 红。
        let (addr, rx) = spawn_capture();
        let rt = HttpRuntime::new().unwrap();
        let _ = rt
            .json_request(&WarpHttpRequest {
                method: WarpHttpMethod::Post,
                url: format!("https://127.0.0.1:{}/reg", addr.port()),
                headers: Default::default(),
                body: Some("{}".into()),
            })
            .await; // 握手必失败（listener 读完即关）——只取已捕获的 ClientHello。
        let hello = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("应捕获到 ClientHello");
        assert_eq!(hello.first(), Some(&0x16u8), "首字节应为 TLS 握手记录");
        let (offers_tls13, alpn) = parse_client_hello(&hello);
        assert!(
            !offers_tls13,
            "WARP client 不得宣告 TLS1.3（supported_versions 不得含 0x0304）"
        );
        assert!(
            !alpn.is_empty(),
            "应解析出 ALPN，实得空（解析失败或未发 ALPN）"
        );
        assert!(
            alpn.iter().all(|p| p != b"h2"),
            "WARP client ALPN 不得含 h2，实得 {alpn:?}"
        );
        assert!(
            alpn.iter().any(|p| p == b"http/1.1"),
            "WARP client ALPN 应含 http/1.1，实得 {alpn:?}"
        );
    }

    #[tokio::test]
    async fn shared_client_offers_tls13_on_the_wire_unlike_warp() {
        // 对照门：共享 client **确实**宣告 TLS1.3 —— 这正是 warp_client 相对它收窄掉的**载荷维度**（TLS1.2 pin）。
        // 二义：(1) 证 warp 与 shared 是真差异、非两 client 恰好同形；(2) 证 parse_client_hello 非「恒返 false」的
        // 坏解析器（坏解析器会让 assert!(offers_tls13) 转红）。
        //
        // **h2 不作对照**：本仓 reqwest 关掉了 `http2` feature（`default-features=false`），故**两个** client 的
        // ALPN 都只报 http/1.1 —— h2 规避是**构建级**属性、非 warp 专属。warp_client 仍显式 `.http1_only()` 作
        // 前瞻护栏（若日后有人开 http2 feature，shared 会向 CF 类端点报 h2，而 warp 借此仍锁 http/1.1）。
        let (addr, rx) = spawn_capture();
        let rt = HttpRuntime::new().unwrap();
        let _ = rt
            .client()
            .get(format!("https://127.0.0.1:{}/", addr.port()))
            .send()
            .await;
        let hello = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("应捕获到 ClientHello");
        let (offers_tls13, alpn) = parse_client_hello(&hello);
        assert!(
            offers_tls13,
            "共享 client 应宣告 TLS1.3（正是 warp_client 收窄掉的载荷维度）"
        );
        assert!(
            alpn.iter().any(|p| p == b"http/1.1"),
            "本构建（无 http2 feature）两 client ALPN 均应含 http/1.1，实得 {alpn:?}"
        );
    }

    #[tokio::test]
    async fn warp_send_forwards_okhttp_headers_to_wire() {
        // 契约门：warp_send 逐条下发请求头（okhttp UA + CF-Client-Version 由 mesh 层给，见 warp_http.rs）。
        //   变异：warp_send 删 `for (k,v) in &req.headers` 循环 → UA 不达线 → 红。
        //   兼防「误给 warp_client 设默认 UA=Polaris/x 覆盖」的回归（本门断言线上是 okhttp 而非 Polaris）。
        let (addr, rx) = spawn_capture();
        let rt = HttpRuntime::new().unwrap();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("User-Agent".to_string(), "okhttp/3.12.1".to_string());
        headers.insert("CF-Client-Version".to_string(), "a-7.21-0721".to_string());
        let _ = rt
            .json_request(&WarpHttpRequest {
                method: WarpHttpMethod::Post,
                url: format!("http://127.0.0.1:{}/reg", addr.port()), // 明文：直接读 HTTP 请求头
                headers,
                body: Some("{}".into()),
            })
            .await;
        let req = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("应捕获到 HTTP 请求");
        let text = String::from_utf8_lossy(&req);
        assert!(
            text.contains("okhttp/3.12.1"),
            "请求须带 okhttp UA，实得:\n{text}"
        );
        assert!(
            text.contains("a-7.21-0721"),
            "请求须带 CF-Client-Version，实得:\n{text}"
        );
        assert!(
            !text.contains("Polaris/"),
            "WARP 请求不得漏出应用默认 UA（Polaris/…）"
        );
    }

    // ── 真实 HTTPS 冒烟（默认 #[ignore]：依赖公网，不进默认 gate）─────────────
    //
    // 证「ring-backed rustls 握手对真实公网 HTTPS 端点真能打通」——这是「编译过 + 回环 HTTP 跑通」
    // **之外**唯一能证 TLS 栈真可用的路径（回环用的是明文 HTTP，不碰 TLS 握手）。
    // 纯出站 GET，不接管网络（brief 明确允许）。跑法：`cargo test -p polaris -- --ignored real_https`。
    #[tokio::test]
    #[ignore = "依赖公网 HTTPS 连通性；手动 --ignored 跑"]
    async fn real_https_get_handshakes_and_returns_body() {
        let rt = HttpRuntime::new().expect("建 client");
        let init = FetchInit {
            user_agent: app_user_agent(),
            max_body_bytes: Some(64 * 1024),
            timeout_ms: Some(10_000),
            ..Default::default()
        };
        // 稳定的小 HTTPS 端点（返回请求者 IP 的纯文本；小、无重定向）。
        let r = rt
            .fetch("https://api.ipify.org/", &init)
            .await
            .expect("真实 HTTPS 握手 + 请求应成功（ring/rustls 打通）");
        assert_eq!(r.status, 200, "实得 status={}", r.status);
        assert!(!r.body.is_empty(), "应收到非空 body（TLS 握手 + 传输成功）");
    }

    // ── DnsLookup 生产实现门 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn system_dns_resolves_localhost_to_loopback() {
        // 只解析 localhost（不出网、不碰宿主 DNS 配置）。证明 SSRF guard 终于有解析器可注入。
        let ips = SystemDnsLookup
            .lookup_all("localhost")
            .await
            .expect("解析 localhost");
        assert!(!ips.is_empty());
        assert!(
            ips.iter().any(|i| i == "127.0.0.1" || i == "::1"),
            "localhost 应解析到回环，实得: {ips:?}"
        );
    }

    // ── C19：更新链路经代理决策真值表 ────────────────────────────────────────

    #[test]
    fn resolve_main_session_via_proxy_truth_table() {
        // 核未运行 → 恒直连（自举友好），无视 msvp。
        assert!(!resolve_main_session_via_proxy(false, None));
        assert!(!resolve_main_session_via_proxy(false, Some(true)));
        assert!(!resolve_main_session_via_proxy(false, Some(false)));
        // 核运行中 → 默认经代理（缺省/开）；仅显式 false 才直连。
        assert!(resolve_main_session_via_proxy(true, None), "缺省=经代理");
        assert!(resolve_main_session_via_proxy(true, Some(true)));
        assert!(
            !resolve_main_session_via_proxy(true, Some(false)),
            "显式关闭=直连"
        );
    }

    #[test]
    fn resolve_update_proxy_target_port_gate() {
        // 经代理但端口不可用（0）→ 强制直连（端口闸），且 port 透传。
        assert_eq!(resolve_update_proxy_target(true, None, 0), (false, 0));
        // 经代理 + 有效端口 → viaProxy=true，port>0（自洽）。
        assert_eq!(
            resolve_update_proxy_target(true, None, 45678),
            (true, 45678)
        );
        // 核未运行 → 直连，即便端口非 0（核未运行时 update-in 口本就不存在）。
        assert_eq!(
            resolve_update_proxy_target(false, None, 45678),
            (false, 45678)
        );
        // 显式关闭 msvp → 直连，即便端口有效。
        assert_eq!(
            resolve_update_proxy_target(true, Some(false), 45678),
            (false, 45678)
        );
        // 自洽不变式：via_proxy=true ⟹ port>0。
        for running in [true, false] {
            for msvp in [None, Some(true), Some(false)] {
                for port in [0u16, 1, 45678] {
                    let (via, p) = resolve_update_proxy_target(running, msvp, port);
                    if via {
                        assert!(p > 0, "via_proxy=true 时 port 必 >0（running={running} msvp={msvp:?} port={port}）");
                    }
                }
            }
        }
    }

    /// # 为什么排除 Windows（2026-08-05，Windows CI 腿首次跑通后实测）
    ///
    /// 空主机名在 unix 的 `getaddrinfo` 上本地即返 `EAI_NONAME`，而 **Windows 把它解析成本机地址**
    /// ⇒ `lookup_all("")` 返回非空 `Ok` ⇒ 本断言在 Windows 上必红。
    ///
    /// **这不是安全缺口**：被守的那条腿在实现里是
    /// `if ips.is_empty() { return Err(...) }`（见 `SystemDnsLookup::lookup_all`）——
    /// `Ok(vec![])` 根本构造不出来，「空循环放行」在实现层已被堵死。本用例守的是另一条腿
    /// （`lookup_host(...).map_err(...)?` 的失败传播），那行代码**与平台无关**，
    /// Linux/macOS 覆盖到它就等于覆盖了 Windows 上的同一行。
    ///
    /// 没有换成「跨平台都注定失败」的输入，是因为找不到一个既满足这条又满足下面那三个约束
    /// （不出网 / 不看解析器脸色 / 本地即失败）的输入 —— 猜一个再让 Windows 腿红一次，
    /// 比诚实门控更糟。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn system_dns_failure_is_err_not_empty_ok() {
        // 关键失败安全：解析失败必须 Err。若返回 Ok(vec![])，guard 的「逐 IP 判定」会**空循环通过**
        // → 解析失败静默变成「SSRF 检查通过」。
        //
        // **失败输入必须在 resolver 之前就注定失败，不能靠上游 DNS 诚实回 NXDOMAIN**：原先用
        // `this-host-does-not-exist.invalid`，在会劫持/fake-ip 的解析器后面（家用路由跑
        // OpenClash 就是）会解析「成功」→ 本门转红（2026-07-28 在 5.238 实测）。空主机名让
        // `getaddrinfo` 本地即返 EAI_NONAME，不出网、不看解析器脸色，走的仍是同一条 `map_err` 腿。
        let r = SystemDnsLookup.lookup_all("").await;
        assert!(
            r.is_err(),
            "解析失败必须 Err（Ok(vec![]) 会让 guard 空循环放行）"
        );
    }
}
