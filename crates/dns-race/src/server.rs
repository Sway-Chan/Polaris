//! 本地节点域名解析竞速 DNS server（sidecar）—— 上游 `NodeDnsRaceServer` 移植。
//!
//! 形态：进程内 UDP server 绑 `127.0.0.1:<动态口>`，作为 sing-box 的 `dns-node-race` 上游。
//! 内核 Do53 query → [`race_forward`] 多上游竞速 → 命中上游的**完整响应**透传回内核（回填 id）。
//!
//! # I/O 薄壳
//! 本文件只做三件事：绑口、收发、把字节丢给 [`race_forward`]。所有判定（谁赢 / 弃谁 / 回什么）
//! 都在 `race` + `wire` + `decoy` 三个可单测模块里 —— 这里没有任何一行会改变解析结论。
//!
//! # fail-open 两层
//! 1. **存活层**：`race_forward` 内部全 FAIL → SERVFAIL（绝不挂着不回，内核拿得到确定答案）；
//! 2. **进程层**：收包循环遇 socket 故障 → watchdog **按原端口**重建。
//!    重绑原端口是关键：内核 config 里已烧进这个端口且不因 socket 重建而重新生成，换新口
//!    会让内核查一个死口 → 节点域名解析**静默**失效。
//!
//!    重建能成功的**前提是先释放旧 socket**：tokio 的 `UdpSocket::bind` 不设 `SO_REUSEADDR`，
//!    旧 fd 还开着时同口重绑必 `EADDRINUSE`（本机回环实测 errno 98）。故当前监听 socket 放在一个
//!    **共享槽**（[`SockSlot`]）里而不是各处散着 `Arc` 克隆：重建腿把槽清空 + drop 掉收发循环自己
//!    那份引用之后才 bind；在飞的 [`handle_one`] 只持槽、回包时才现取当前 socket，因此
//!    「在飞查询寿命（可达总预算 2s）> 重试窗（5×200ms）」不再能把端口钉死。
//!    对齐 上游 `node-dns-race-server.ts` 的 `onSocketDown`（先 `this.socket = null` 再 `listen(prevPort)`）。
//!
//! 启动失败由调用方降级（不起 sidecar ⟹ `race_server_port=0` ⟹ config 走单上游），
//! **绝不阻断起核**。scope 仅节点域名解析，不碰系统 DNS。

#![forbid(unsafe_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::decoy::DecoySet;
use crate::query::MAX_DNS_UDP_BYTES;
use crate::race::{race_forward, UpstreamQuery};
use crate::upstream::ResolvedUpstreams;

/// watchdog 重建的最大连续失败次数。超过即彻底放弃（端口归 0，[`NodeDnsRaceServer::live_port`]
/// 自曝 + 触发 [`OnRaceServerDead`]），不无限重试烧 CPU。
const WATCHDOG_MAX_ATTEMPTS: u32 = 5;
/// watchdog 每次重建失败后的退避（给 OS 释放端口的时间）。
const WATCHDOG_BACKOFF: Duration = Duration::from_millis(200);

/// 同时在飞的 query 上限。
///
/// sidecar 是一个**开放式 loopback resolver**：本机任何进程都能往 `127.0.0.1:<port>` 灌任意域名的
/// query，每包都会 spawn 一个任务并对全部上游齐射。无上限 = 本地 DoS + 对上游的放大器。
/// 64 远高于内核并发解析节点域名的实际需要（Tier1 上限才 3 个上游），只用来给恶意/失控的灌包封顶。
pub const MAX_INFLIGHT_QUERIES: usize = 64;

/// watchdog 彻底放弃（端口再也绑不回来）时的回调，入参 = 已死的端口。
///
/// 存在的意义：`live_port` 归零只是**本结构体内部**可见，而 src-tauri 侧的注入态
/// （`race_server_port` / `race_upstream_ips`）此后仍是 >0 的旧值 —— 之后任何一次 config 重生成
/// 都会继续把内核指向一个没人听的口。回调让「sidecar 死了」这件事能传出去（生产实现 = 清运行期
/// 注入态，使生成侧自动回落单上游）。
pub type OnRaceServerDead = Arc<dyn Fn(u16) + Send + Sync>;

/// 当前监听 socket 的共享槽（`None` = 无监听：重建中 / 已停）。
///
/// 用槽而非到处散 `Arc<UdpSocket>` 克隆，是为了让「释放旧 socket」这件事**有唯一的落点**：
/// 只要槽清空且收发循环放掉自己那份，端口就一定可以重绑（见模块文档 fail-open 第 2 层）。
type SockSlot = Arc<Mutex<Option<Arc<UdpSocket>>>>;

fn current_socket(slot: &SockSlot) -> Option<Arc<UdpSocket>> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_socket(slot: &SockSlot, sock: Option<Arc<UdpSocket>>) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = sock;
}

/// 运行中的竞速 sidecar 句柄。`drop` 即停（不留孤儿 socket / 任务）。
pub struct NodeDnsRaceServer {
    /// 首次绑到的端口 —— **烧进 sing-box config 的就是它**，运行期恒定不变。
    port: u16,
    /// 当前真正在监听的端口；watchdog 彻底失败时归 0（供 [`Self::is_listening`] 自曝）。
    live_port: Arc<AtomicU16>,
    /// 主动停止标记：watchdog 据此区分「被 stop」与「真故障」，不复活已被停掉的 socket。
    closing: Arc<AtomicBool>,
    /// 当前监听 socket 的共享槽（与收发循环、在飞回包腿同一份）。`stop` 据此**立即**释放端口，
    /// 不必等在飞任务收尾。
    slot: SockSlot,
    /// 调用方注册的死亡回调（与 watchdog 手里那份是同一只）。句柄上留一份**只为自证接线**，
    /// 见 [`Self::dead_callback`]；运行期的触发权仍只在 watchdog。
    on_dead: Option<OnRaceServerDead>,
    task: JoinHandle<()>,
}

/// 手写 `Debug`：`on_dead` 是 `dyn Fn` 无法 derive，且回调本体也不该进日志 —— 只暴露「装没装」。
impl std::fmt::Debug for NodeDnsRaceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeDnsRaceServer")
            .field("port", &self.port)
            .field("live_port", &self.live_port.load(Ordering::SeqCst))
            .field("closing", &self.closing.load(Ordering::SeqCst))
            .field("on_dead", &self.on_dead.is_some())
            .finish_non_exhaustive()
    }
}

impl NodeDnsRaceServer {
    /// 绑 `127.0.0.1:0`（OS 分配空闲口，避开固定口被占）并起收发循环。
    ///
    /// `on_dead`：watchdog 彻底失败时的回调（见 [`OnRaceServerDead`]）。传 `None` = 只记日志。
    ///
    /// # Errors
    /// 绑定失败（回环不可用 / fd 耗尽）。调用方据此降级为单上游，不阻断起核。
    pub async fn start(
        upstreams: ResolvedUpstreams,
        query: Arc<dyn UpstreamQuery>,
        total_budget: Duration,
        on_dead: Option<OnRaceServerDead>,
        decoys: Arc<DecoySet>,
    ) -> io::Result<Self> {
        Self::start_with_limit(
            upstreams,
            query,
            total_budget,
            on_dead,
            decoys,
            MAX_INFLIGHT_QUERIES,
        )
        .await
    }

    /// [`Self::start`] + 可调在飞上限。上限只有单测需要调小（生产恒 [`MAX_INFLIGHT_QUERIES`]）——
    /// 抽出来是为了让「超限即丢弃、且不触发上游齐射」这条不变式可测，而不必真灌 65 个并发包。
    async fn start_with_limit(
        upstreams: ResolvedUpstreams,
        query: Arc<dyn UpstreamQuery>,
        total_budget: Duration,
        on_dead: Option<OnRaceServerDead>,
        decoys: Arc<DecoySet>,
        max_inflight: usize,
    ) -> io::Result<Self> {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let port = sock.local_addr()?.port();
        let live_port = Arc::new(AtomicU16::new(port));
        let closing = Arc::new(AtomicBool::new(false));
        let slot: SockSlot = Arc::new(Mutex::new(Some(sock)));
        let task = tokio::spawn(serve(ServeCtx {
            slot: Arc::clone(&slot),
            upstreams: Arc::new(upstreams),
            query,
            total_budget,
            limiter: Arc::new(Semaphore::new(max_inflight)),
            dropped: AtomicU64::new(0),
            decoys,
            watchdog: Watchdog {
                port,
                live_port: Arc::clone(&live_port),
                closing: Arc::clone(&closing),
                on_dead: on_dead.clone(),
            },
        }));
        log::info!("[dns-race] listening 127.0.0.1:{port}");
        Ok(Self {
            port,
            live_port,
            closing,
            slot,
            on_dead,
            task,
        })
    }

    /// 烧进 config 的端口（恒定）。
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 当前是否真的在监听（watchdog 彻底失败后为 `false`）。
    ///
    /// 存在的意义是让「sidecar 死了」**可观测**：否则内核会一直往一个没人听的口发查询，
    /// 表现为节点解析莫名超时，而日志里什么都没有。
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.live_port.load(Ordering::SeqCst) != 0
    }

    /// 本 sidecar 上真正注册的死亡回调（`None` = 调用方没接，死了只记日志）。
    ///
    /// **为什么要暴露**：这条接线一旦丢（传 `None`），失效是完全静默的 —— sidecar 死了、
    /// 调用方的注入态还指着死口，内核对该口的解析全部 SERVFAIL，日志里只有 crate 侧一行 error。
    /// 而真让 watchdog 死需要在 sidecar 之外**占死它的端口**，端口却是 OS 现分配、拿到手时
    /// socket 已在监听，没有可控的复现路径。故把注册的那只回调交出去，让调用方的测试直接触发它 ——
    /// 「生产调用点确实传了回调」+「回调本身做对了事」两件事一次锁死。
    #[must_use]
    pub fn dead_callback(&self) -> Option<OnRaceServerDead> {
        self.on_dead.clone()
    }

    /// 主动停止（幂等）。置 `closing` 让 watchdog 不复活，**清空 socket 槽**（端口即刻释放，不必等
    /// 在飞回包腿收尾），再 abort 收发任务。
    pub fn stop(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.live_port.store(0, Ordering::SeqCst);
        set_socket(&self.slot, None);
        self.task.abort();
    }
}

impl Drop for NodeDnsRaceServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 收包错误是否属于「对端瞬态、本 socket 健康」。
///
/// 回包给一个已关闭的内核端口会经 ICMP port-unreachable 反馈到**本 socket** 的下一次 recv
/// （Linux 语义）。重建 socket 是过度反应，且会在内核重启时把端口白白抖一遍 —— 仅继续收下一包。
/// 抽成纯函数是为了让「哪些错该重建」这条判定可单测（重建腿本身另测）。
fn is_transient_recv_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

/// watchdog：按**原端口**重建监听 + 彻底失败时自曝。
struct Watchdog {
    /// 烧进 config 的端口（重建目标，恒定）。
    port: u16,
    live_port: Arc<AtomicU16>,
    closing: Arc<AtomicBool>,
    on_dead: Option<OnRaceServerDead>,
}

impl Watchdog {
    /// 释放旧 socket → 按原端口重建。主动 stop 期间不复活（`None`，避免留下孤儿 socket）。
    ///
    /// **入参 `local` 按值传**（收发循环自己那份 `Arc`）：本函数必须先把它连同槽里的那份一起放掉，
    /// 端口才可能重绑。签名把这条顺序约束变成编译期事实 —— 调用方交出所有权后就再也用不着旧
    /// socket 了，不存在「忘了先关」的写法。
    ///
    /// 彻底失败 → `live_port=0` + [`OnRaceServerDead`]，返回 `None`（调用方须结束收发循环）。
    async fn recover(&self, slot: &SockSlot, local: Arc<UdpSocket>) -> Option<Arc<UdpSocket>> {
        set_socket(slot, None); // ① 槽先清空：在飞回包腿从此拿不到旧 socket
        drop(local); // ② 再放掉收发循环自己那份 —— 此后旧 fd 引用计数归零、端口才真的空出来
        for attempt in 1..=WATCHDOG_MAX_ATTEMPTS {
            if self.closing.load(Ordering::SeqCst) {
                return None;
            }
            match UdpSocket::bind(("127.0.0.1", self.port)).await {
                Ok(s) => {
                    let s = Arc::new(s);
                    set_socket(slot, Some(Arc::clone(&s)));
                    self.live_port.store(self.port, Ordering::SeqCst);
                    log::info!("[dns-race] 已重建监听 127.0.0.1:{}", self.port);
                    return Some(s);
                }
                Err(e) => {
                    log::warn!("[dns-race] re-listen 第 {attempt} 次失败: {e}");
                    tokio::time::sleep(WATCHDOG_BACKOFF).await;
                }
            }
        }
        self.live_port.store(0, Ordering::SeqCst);
        log::error!(
            "[dns-race] 端口 {} 重建失败（已重试 {WATCHDOG_MAX_ATTEMPTS} 次）→ \
             节点域名解析降级：内核对该口的查询将超时，请重连以重建",
            self.port
        );
        if let Some(cb) = &self.on_dead {
            cb(self.port);
        }
        None
    }
}

/// 收发循环的全部状态（参数打包：裸参数已达 clippy 上限，且这些字段本就是一体的）。
struct ServeCtx {
    slot: SockSlot,
    upstreams: Arc<ResolvedUpstreams>,
    query: Arc<dyn UpstreamQuery>,
    total_budget: Duration,
    /// 在飞 query 上限（见 [`MAX_INFLIGHT_QUERIES`]）。
    limiter: Arc<Semaphore>,
    /// 超限丢弃累计数（限频上报，见下方 warn）。
    dropped: AtomicU64,
    /// POISONED 判定用的 decoy 段集（调用方注入，默认内置；见 [`DecoySet`] 模块文档）。
    /// `Arc` 而非按值：每个在飞 query 腿都要借它，克隆 Vec 会按包数放大。
    decoys: Arc<DecoySet>,
    watchdog: Watchdog,
}

/// 收发主循环 + watchdog。
///
/// 外层每轮 = 「一只活着的 socket 的生命周期」；内层跑正常收发，遇非瞬态错即跳出去走重建腿。
/// 这么分层是为了让「旧 socket 的最后一份引用在 bind 之前离开作用域」成为结构性事实：
/// `sock` 是内层循环唯一持有者，跳出内层后被交给 [`Watchdog::recover`] 消耗掉。
async fn serve(ctx: ServeCtx) {
    let mut buf = vec![0u8; MAX_DNS_UDP_BYTES];
    loop {
        if ctx.watchdog.closing.load(Ordering::SeqCst) {
            return;
        }
        let Some(sock) = current_socket(&ctx.slot) else {
            return; // 槽被 stop 清空 = 已停
        };
        let hard_err = loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, peer)) => {
                    // 在飞封顶：拿不到令牌就丢弃本包（内核会自然重试），**绝不**放行到上游齐射。
                    let Ok(permit) = Arc::clone(&ctx.limiter).try_acquire_owned() else {
                        let seen = ctx.dropped.fetch_add(1, Ordering::Relaxed);
                        if seen.is_multiple_of(256) {
                            log::warn!(
                                "[dns-race] 在飞 query 已达上限，丢弃新 query（累计 {} 个）—— \
                                 本机有进程在灌包，或上游整体极慢",
                                seen + 1
                            );
                        }
                        continue;
                    };
                    let msg = buf[..n].to_vec();
                    // 每个 query 独立任务：一个慢上游不得阻塞后续 query 的收取
                    // （内核会并发问多个节点域名，串行化 = 竞速优势被自己吃掉）。
                    tokio::spawn(handle_one(
                        permit,
                        Arc::clone(&ctx.slot),
                        msg,
                        peer,
                        RaceArgs {
                            upstreams: Arc::clone(&ctx.upstreams),
                            query: Arc::clone(&ctx.query),
                            total_budget: ctx.total_budget,
                            decoys: Arc::clone(&ctx.decoys),
                        },
                    ));
                }
                Err(e) if ctx.watchdog.closing.load(Ordering::SeqCst) => {
                    log::debug!("[dns-race] 停止中的 socket 错误，忽略: {e}");
                    return;
                }
                Err(e) if is_transient_recv_error(&e) => {
                    log::debug!("[dns-race] 瞬态收包错误，继续: {e}");
                }
                Err(e) => break e,
            }
        };
        log::warn!(
            "[dns-race] socket 故障，尝试按原端口 {} 重建: {hard_err}",
            ctx.watchdog.port
        );
        if ctx.watchdog.recover(&ctx.slot, sock).await.is_none() {
            return;
        }
    }
}

/// 竞速一次所需的那几样（参数打包：与 [`ServeCtx`] 同一理由 —— 裸参数已达 clippy 上限，
/// 且这四样本就是一体的「怎么竞速」，per-query 变的只有报文与来源）。
struct RaceArgs {
    upstreams: Arc<ResolvedUpstreams>,
    query: Arc<dyn UpstreamQuery>,
    total_budget: Duration,
    decoys: Arc<DecoySet>,
}

/// 处理单个 query：竞速 → 回包。**绝不 panic、绝不静默**。
///
/// `race_forward` 内部已对全 FAIL 返 SERVFAIL，故正常路径必有回包；这里只兜发送失败。
///
/// **只持槽、不持 socket**：竞速最长可跑满总预算（2s），而 watchdog 的重试窗只有 5×200ms。
/// 若在飞任务各自攥着一份 `Arc<UdpSocket>`，一个慢查询就能让重建腿在整个重试窗内撞
/// `EADDRINUSE` → 端口永久死。现在它到回包那一刻才从槽里取当前 socket：重建期间取到 `None`
/// （丢弃这一次响应，内核自然重试），重建之后取到的是新 socket —— 端口不变，内核照收。
///
/// `_permit` 只为把在飞令牌的生命周期绑到本任务上（drop 即归还），不被读取。
async fn handle_one(
    _permit: OwnedSemaphorePermit,
    slot: SockSlot,
    msg: Vec<u8>,
    peer: SocketAddr,
    race: RaceArgs,
) {
    let resp = race_forward(
        &msg,
        &race.upstreams,
        race.query.as_ref(),
        race.total_budget,
        &race.decoys,
    )
    .await;
    let Some(sock) = current_socket(&slot) else {
        // 预期瞬态（套接字重建 / 已停窗口），调用方会重查 → 按条 `debug` + 计数，
        // 会话结束汇总一条（见 `stats` 模块）。
        crate::stats::record_reply_no_socket();
        log::debug!("[dns-race] 回包时无监听 socket（重建中 / 已停）→ 丢弃对 {peer} 的响应");
        return;
    };
    if let Err(e) = sock.send_to(&resp, peer).await {
        log::warn!("[dns-race] 回包给 {peer} 失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::race::DEFAULT_RACE_BUDGET;
    use crate::upstream::{resolve_upstreams, ResolveUpstream};
    use crate::wire::{
        build_answer_response, classify_dns_response, encode_dns_query, extract_answer_ip_bytes,
        AnswerRecord, DnsResponseClass, TYPE_A,
    };
    use async_trait::async_trait;

    /// 固定应答的 mock 上游（零网络）。
    struct FixedQuery(Result<Vec<u8>, String>);

    #[async_trait]
    impl UpstreamQuery for FixedQuery {
        async fn query(&self, _u: &ResolveUpstream, q: &[u8]) -> Result<Vec<u8>, String> {
            match &self.0 {
                Ok(ips) => Ok(build_answer_response(
                    q,
                    &[AnswerRecord {
                        rtype: TYPE_A,
                        rdata: ips.clone(),
                    }],
                )),
                Err(e) => Err(e.clone()),
            }
        }
    }

    fn default_pool() -> ResolvedUpstreams {
        resolve_upstreams(&["ali".to_string(), "dnspod".to_string()], &[])
    }

    /// 扮演 sing-box：往 sidecar 发 query、收响应（**全程 127.0.0.1**）。
    async fn ask(port: u16, query: &[u8]) -> io::Result<Vec<u8>> {
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        sock.send_to(query, ("127.0.0.1", port)).await?;
        let mut buf = vec![0u8; MAX_DNS_UDP_BYTES];
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sidecar 未回包"))??;
        Ok(buf[..n].to_vec())
    }

    #[tokio::test]
    async fn sidecar_answers_kernel_query_on_loopback() {
        let srv = NodeDnsRaceServer::start(
            default_pool(),
            Arc::new(FixedQuery(Ok(vec![1, 2, 3, 4]))),
            DEFAULT_RACE_BUDGET,
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环");
        assert!(srv.port() > 0 && srv.is_listening());
        let q = encode_dns_query("node.example.com", TYPE_A, 0x0abc);
        let resp = ask(srv.port(), &q).await.expect("有回包");
        assert_eq!(classify_dns_response(&resp, TYPE_A), DnsResponseClass::Hit);
        assert_eq!(&resp[..2], &q[..2], "id 回填");
        assert_eq!(extract_answer_ip_bytes(&resp), vec![vec![1, 2, 3, 4]]);
    }

    /// 【不变式：fail-open】上游全挂 → sidecar 仍**必须**回 SERVFAIL，不能沉默。
    /// 变异验证：把 `handle_one` 里的 `send_to` 删掉（或让 `race_forward` 失败时不回包）→
    /// 本测试 3s 超时转红。
    #[tokio::test]
    async fn sidecar_replies_servfail_when_all_upstreams_fail() {
        let srv = NodeDnsRaceServer::start(
            default_pool(),
            Arc::new(FixedQuery(Err("boom".into()))),
            Duration::from_millis(100),
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环");
        let q = encode_dns_query("node.example.com", TYPE_A, 0x0def);
        let resp = ask(srv.port(), &q).await.expect("全 FAIL 也必须有回包");
        assert_eq!(resp[3] & 0x0f, 2, "RCODE=SERVFAIL");
        assert_eq!(&resp[..2], &q[..2]);
    }

    #[tokio::test]
    async fn malformed_datagram_still_gets_a_reply() {
        let srv = NodeDnsRaceServer::start(
            default_pool(),
            Arc::new(FixedQuery(Ok(vec![1, 1, 1, 1]))),
            Duration::from_millis(100),
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环");
        let resp = ask(srv.port(), &[0xde, 0xad, 0xbe, 0xef])
            .await
            .expect("畸形包也回");
        assert!(resp.len() >= 12);
    }

    #[tokio::test]
    async fn stop_releases_the_port() {
        let srv = NodeDnsRaceServer::start(
            default_pool(),
            Arc::new(FixedQuery(Ok(vec![1, 1, 1, 1]))),
            DEFAULT_RACE_BUDGET,
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环");
        let port = srv.port();
        srv.stop();
        assert!(!srv.is_listening());
        drop(srv);
        // 端口释放是异步的（任务 abort → 析构），给它几轮调度再断言可复绑。
        let mut rebound = false;
        for _ in 0..50 {
            if UdpSocket::bind(("127.0.0.1", port)).await.is_ok() {
                rebound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(rebound, "stop 后端口必须被释放，否则重连会占着旧口");
    }

    #[tokio::test]
    async fn concurrent_queries_are_not_serialized_behind_a_slow_one() {
        /// 慢上游：固定 80ms 后成功。两个并发 query 若被串行化则总耗时 ≥160ms。
        struct SlowQuery;
        #[async_trait]
        impl UpstreamQuery for SlowQuery {
            async fn query(&self, _u: &ResolveUpstream, q: &[u8]) -> Result<Vec<u8>, String> {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(build_answer_response(
                    q,
                    &[AnswerRecord {
                        rtype: TYPE_A,
                        rdata: vec![8, 8, 8, 8],
                    }],
                ))
            }
        }
        let srv = NodeDnsRaceServer::start(
            default_pool(),
            Arc::new(SlowQuery),
            DEFAULT_RACE_BUDGET,
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环");
        let port = srv.port();
        let q1 = encode_dns_query("a.example.com", TYPE_A, 1);
        let q2 = encode_dns_query("b.example.com", TYPE_A, 2);
        let t0 = std::time::Instant::now();
        let (r1, r2) = tokio::join!(ask(port, &q1), ask(port, &q2));
        assert!(r1.is_ok() && r2.is_ok());
        assert!(
            t0.elapsed() < Duration::from_millis(160),
            "并发 query 不得被串行化"
        );
    }

    /// 【不变式：watchdog 按原端口重建**真的能成功**】
    ///
    /// 直接驱动生产的重建腿 [`Watchdog::recover`]（收发循环遇非瞬态错时调的就是它），断言：
    /// 旧 socket 被释放后同口重绑成功、新 socket 能真正收包、`live_port` 复位、不误触死亡回调。
    ///
    /// 变异验证（两半各锁一条）：
    /// - 删掉 `recover` 里的 `set_socket(slot, None)` → 槽仍持旧 socket → 5 次 bind 全 EADDRINUSE → `None` → 红；
    /// - 删掉 `drop(local)`（让 `local` 活到函数末尾）→ 同样 5 次全失败 → 红。
    #[tokio::test]
    async fn watchdog_rebinds_same_port_after_releasing_old_socket() {
        let old = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("绑回环"));
        let port = old.local_addr().unwrap().port();
        let slot: SockSlot = Arc::new(Mutex::new(Some(Arc::clone(&old))));
        // 模拟一个在飞的 handle_one：**只持槽**（修复后的形状），不持 socket。
        let inflight_view = Arc::clone(&slot);

        // 前提锁定（本机回环实测 errno 98）：旧 fd 还有引用时同口重绑必失败 ——
        // 这正是「重建腿在旧结构下结构性不可能成功」的根因。
        assert!(
            UdpSocket::bind(("127.0.0.1", port)).await.is_err(),
            "旧 socket 未释放时同口重绑必须失败（否则本测试锁不住任何东西）"
        );

        let live_port = Arc::new(AtomicU16::new(port));
        let dead_calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&dead_calls);
        let wd = Watchdog {
            port,
            live_port: Arc::clone(&live_port),
            closing: Arc::new(AtomicBool::new(false)),
            on_dead: Some(Arc::new(move |_p| {
                seen.fetch_add(1, Ordering::SeqCst);
            })),
        };

        let local = current_socket(&slot).expect("槽里有 socket");
        drop(old); // 只留「收发循环那一份 + 槽那一份」，与生产形状一致
        let rebuilt = wd
            .recover(&slot, local)
            .await
            .expect("释放旧 socket 后必须能按原端口重建");

        assert_eq!(
            rebuilt.local_addr().unwrap().port(),
            port,
            "必须重绑**原**端口：内核 config 里烧的就是它，换口 = 查死口"
        );
        assert_eq!(live_port.load(Ordering::SeqCst), port, "live_port 须复位");
        assert_eq!(
            dead_calls.load(Ordering::SeqCst),
            0,
            "重建成功不得触发死亡回调"
        );

        // 新 socket 真的在收包（不是「bind 上了但没接管」）。
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", ("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), rebuilt.recv_from(&mut buf))
            .await
            .expect("重建后的 socket 须能收包")
            .expect("recv 成功");
        assert_eq!(&buf[..n], b"ping");

        // 在飞腿的视角：槽里已换成新 socket → 它那份迟到的回包仍从**同一端口**发出，内核照收。
        let seen_by_inflight = current_socket(&inflight_view).expect("在飞腿仍能取到当前 socket");
        assert_eq!(seen_by_inflight.local_addr().unwrap().port(), port);
    }

    /// 【不变式：彻底失败必自曝】端口被别人占死 → 重试耗尽 → `live_port=0` **且**触发死亡回调。
    ///
    /// 没有回调时，src-tauri 的注入态仍是 >0 的旧端口，之后每次 config 重生成都继续把内核指向死口。
    /// 变异验证：删掉 `recover` 末尾的 `cb(self.port)` → 回调计数断言转红；删掉 `live_port.store(0,..)`
    /// → 端口归零断言转红。
    #[tokio::test]
    async fn watchdog_gives_up_zeroes_live_port_and_fires_on_dead() {
        // 占死目标端口：重建腿无论重试多少次都绑不上。
        let squatter = UdpSocket::bind("127.0.0.1:0").await.expect("绑回环");
        let dead_port = squatter.local_addr().unwrap().port();
        // 「旧 socket」另绑一口（同口不可能有两只），它只是被 recover 释放掉的那一份。
        let old = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("绑回环"));
        let slot: SockSlot = Arc::new(Mutex::new(Some(Arc::clone(&old))));

        let live_port = Arc::new(AtomicU16::new(dead_port));
        let reported = Arc::new(AtomicU16::new(0));
        let sink = Arc::clone(&reported);
        let wd = Watchdog {
            port: dead_port,
            live_port: Arc::clone(&live_port),
            closing: Arc::new(AtomicBool::new(false)),
            on_dead: Some(Arc::new(move |p| sink.store(p, Ordering::SeqCst))),
        };

        let local = current_socket(&slot).unwrap();
        drop(old);
        assert!(
            wd.recover(&slot, local).await.is_none(),
            "端口被占死 → 重试耗尽 → None"
        );
        assert_eq!(
            live_port.load(Ordering::SeqCst),
            0,
            "彻底失败 → live_port 归 0"
        );
        assert_eq!(
            reported.load(Ordering::SeqCst),
            dead_port,
            "必须把**死掉的端口**报给上层，否则注入态无从知道该清哪一个"
        );
        assert!(
            current_socket(&slot).is_none(),
            "失败后槽必须是空的（不留一个已死的 socket 让回包腿误用）"
        );
        drop(squatter);
    }

    /// 【判定表】哪些 recv 错误算「对端瞬态」（继续收）、哪些算 socket 故障（走重建）。
    /// 变异验证：把任一 kind 从 `is_transient_recv_error` 挪走/挪进 → 对应断言转红。
    #[test]
    fn transient_recv_errors_are_classified_apart_from_socket_faults() {
        use io::ErrorKind::*;
        for k in [ConnectionRefused, ConnectionReset, Interrupted, WouldBlock] {
            assert!(
                is_transient_recv_error(&io::Error::new(k, "x")),
                "{k:?} 是对端瞬态（ICMP port-unreachable 等），重建 socket 是过度反应"
            );
        }
        // ENOBUFS / 权限 / 已关闭 fd 等 → 视作 socket 故障，必须走重建腿。
        for k in [OutOfMemory, PermissionDenied, BrokenPipe, Other] {
            assert!(
                !is_transient_recv_error(&io::Error::new(k, "x")),
                "{k:?} 必须触发按原端口重建"
            );
        }
    }

    /// 【不变式：在飞封顶 + 超限不放大】限 1 + 慢上游：第 2 个 query 在第 1 个未完成时到达 →
    /// 被丢弃（无回包），且**上游一次都没被多问**（这才是放大面被堵住的证据，不是「少了个回包」）。
    ///
    /// 变异验证：删掉 serve 里的 `try_acquire_owned` 早退（恢复无条件 spawn）→ 上游调用计数变 2 → 转红。
    #[tokio::test]
    async fn inflight_cap_drops_excess_without_upstream_fanout() {
        struct CountingSlowQuery(Arc<AtomicU64>);
        #[async_trait]
        impl UpstreamQuery for CountingSlowQuery {
            async fn query(&self, _u: &ResolveUpstream, q: &[u8]) -> Result<Vec<u8>, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(build_answer_response(
                    q,
                    &[AnswerRecord {
                        rtype: TYPE_A,
                        rdata: vec![4, 4, 4, 4],
                    }],
                ))
            }
        }
        let hits = Arc::new(AtomicU64::new(0));
        // 单上游池 → 一个 query 恰好一次上游调用，计数即「有没有被放行」。
        let pool = resolve_upstreams(&["ali".to_string()], &[]);
        let srv = NodeDnsRaceServer::start_with_limit(
            pool,
            Arc::new(CountingSlowQuery(Arc::clone(&hits))),
            DEFAULT_RACE_BUDGET,
            None,
            Arc::new(DecoySet::builtin()),
            1, // 在飞上限 1
        )
        .await
        .expect("绑回环");
        let port = srv.port();

        let c1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c1.send_to(
            &encode_dns_query("a.example.com", TYPE_A, 1),
            ("127.0.0.1", port),
        )
        .await
        .unwrap();
        // 让第一个 query 被收下并占住唯一的令牌。
        tokio::time::sleep(Duration::from_millis(80)).await;

        let c2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c2.send_to(
            &encode_dns_query("b.example.com", TYPE_A, 2),
            ("127.0.0.1", port),
        )
        .await
        .unwrap();

        let mut buf = vec![0u8; MAX_DNS_UDP_BYTES];
        assert!(
            tokio::time::timeout(Duration::from_millis(250), c2.recv_from(&mut buf))
                .await
                .is_err(),
            "超出在飞上限的 query 必须被丢弃（内核自会重试），不得排队占内存"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "被丢弃的 query 绝不能触发上游齐射 —— 放大面就是靠这一条堵住的"
        );

        // 第一个 query 不受影响，正常拿到答案。
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), c1.recv_from(&mut buf))
            .await
            .expect("首个 query 必须正常回包")
            .expect("recv 成功");
        assert_eq!(
            classify_dns_response(&buf[..n], TYPE_A),
            DnsResponseClass::Hit
        );
    }
}
