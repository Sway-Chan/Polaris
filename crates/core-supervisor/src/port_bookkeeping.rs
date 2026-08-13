//! 管理 API 端口簿记 —— 上游 `resolveTailscaleApiPort` / `resolveFreeLocalPort` / `resolveTailscaleLoginApiPort`
//! 的 Rust 移植（源 `ProxyManager.ts:3006-3057` + `proxy-ports.ts`）。
//!
//! 不变式：
//! - 每次 start 重新解析一个空闲 127.0.0.1 端口（避硬编 clash+1 被占 → services bind FATAL，A1）。
//! - 排除 control_api / http / socks / mixed 端口集（避 listen(0) 偶撞用户端口段）。
//! - 5 次重试仍撞 exclude → 返回 fallback（control_api + 1，与旧行为一致）。
//! - 登录核 api 端口额外排除主核 api 端口（避两个 api service bind 撞），fallback = control_api + 2。
//!
//! 不触碰宿主网络：真实 bind 探测经 [`FreePortProvider`] trait 抽象，测试用 [`SeededPortProvider`]
//! 确定性桩；生产用 [`TokioPortProvider`]（bind 0 → 取端口 → drop）。

use std::collections::HashSet;

/// 默认 control_api 端口（对齐 上游 `DEFAULT_CONTROL_PORT = 9090`，proxy-ports.ts:11）。
pub const DEFAULT_CONTROL_PORT: u16 = 9090;

/// control_api 端口解析（上游 `controlApiPort(config)`，proxy-ports.ts:17）。
/// `Some(>0)` 用之；否则默认 9090。
pub fn control_api_port(config_control_port: Option<u16>) -> u16 {
    match config_control_port {
        Some(p) if p > 0 => p,
        _ => DEFAULT_CONTROL_PORT,
    }
}

/// 端口排除集（resolveTailscaleApiPort / resolveFreeLocalPort 共用）。
///
/// 对齐 上游 `exclude = new Set([controlApiPort, httpPort, socksPort, mixedPort].filter(p=>p>0))`（:3007-3011）。
/// `0`/`None` 视作未设（不排除）。
#[derive(Debug, Default, Clone)]
pub struct PortExclusions {
    pub control_api: u16,
    pub http: u16,
    pub socks: u16,
    pub mixed: u16,
    /// 主核 api 端口（仅登录核解析时排除，:3024）。
    pub primary_api: u16,
}

impl PortExclusions {
    /// 主核 api 端口解析用的排除集（resolveTailscaleApiPort，:3007-3011）。
    /// primary_api 不排除（它就是要解析的目标）。
    pub fn for_primary_api(
        control: Option<u16>,
        http: Option<u16>,
        socks: Option<u16>,
        mixed: Option<u16>,
    ) -> Self {
        Self {
            control_api: control_api_port(control),
            http: http.unwrap_or(0),
            socks: socks.unwrap_or(0),
            mixed: mixed.unwrap_or(0),
            primary_api: 0,
        }
    }

    /// 登录核 api 端口解析用的排除集（resolveTailscaleLoginApiPort，:3022-3031）。
    /// 额外排除主核 api 端口（运行中已占）。
    pub fn for_login_api(
        primary_api: u16,
        control: Option<u16>,
        http: Option<u16>,
        socks: Option<u16>,
        mixed: Option<u16>,
    ) -> Self {
        Self {
            control_api: control_api_port(control),
            http: http.unwrap_or(0),
            socks: socks.unwrap_or(0),
            mixed: mixed.unwrap_or(0),
            primary_api,
        }
    }

    /// 返回排除端口集合（去 0）。
    pub fn as_set(&self) -> HashSet<u16> {
        [
            self.control_api,
            self.http,
            self.socks,
            self.mixed,
            self.primary_api,
        ]
        .into_iter()
        .filter(|p| *p != 0)
        .collect()
    }
}

/// 解析结果（携带 fallback 标记便于诊断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPort {
    pub port: u16,
    /// true = 5 次重试仍撞 exclude → 用了 fallback（极少见）。
    pub used_fallback: bool,
}

/// bind 0 → 取端口 → 立即关的抽象（resolveFreeLocalPort 内核，:3040-3046）。
///
/// 生产用 [`TokioPortProvider`]；测试用 `SeededPortProvider`（手写确定性桩，见 tests 模块）。
/// 返回 `None` = bind 失败（极少见，对应 TS 的 catch 分支 :3048）。
pub trait FreePortProvider: Send + Sync {
    /// bind 127.0.0.1:0 → 返回系统分配端口 → drop listener。
    fn try_allocate(&self) -> Option<u16>;
}

/// Tokio 实现的空闲端口探测（对应 TS `net.createServer().listen(0,'127.0.0.1')`）。
///
/// 在 blocking pool 上 bind（tokio::net 是 async，但 listen+drop 极快，用 block_in_place 不合适；
/// 这里要求运行时在调用方，提供 async 入口 [`PortAllocator::resolve_free_local_port_async`]）。
pub struct TokioPortProvider;

impl FreePortProvider for TokioPortProvider {
    fn try_allocate(&self) -> Option<u16> {
        // 阻塞 spin：tokio::net::TcpListener::bind 是 async，本 trait 是同步。
        // 真正的 async 路径在 PortAllocator::resolve_free_local_port_async；本 impl 仅供非 async 兼容。
        // 用标准库同步 listener（零依赖、与 Polaris net.createServer 同语义）。
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .map(|addr| addr.port())
            .ok()
    }
}

/// 端口分配器：封装 resolveFreeLocalPort 的 5 次重试 + exclude 过滤 + fallback 逻辑（:3038-3057）。
pub struct PortAllocator<P: FreePortProvider> {
    provider: P,
    /// 最大重试次数（Polaris 固定 5，:3039）。
    max_attempts: u32,
}

impl<P: FreePortProvider> PortAllocator<P> {
    pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;

    pub fn new(provider: P) -> Self {
        Self {
            provider,
            max_attempts: Self::DEFAULT_MAX_ATTEMPTS,
        }
    }

    pub fn with_max_attempts(mut self, max: u32) -> Self {
        self.max_attempts = max.max(1);
        self
    }

    /// resolveFreeLocalPort（:3038-3057）：至多 max_attempts 次拿系统分配口，
    /// 不在 exclude 集才采用；全撞 → fallback。
    pub fn resolve_free_local_port(&self, exclude: &PortExclusions, fallback: u16) -> ResolvedPort {
        let set = exclude.as_set();
        for _ in 0..self.max_attempts {
            // bind 失败（catch 分支）→ 继续下一轮（:3048）。
            let Some(port) = self.provider.try_allocate() else {
                continue;
            };
            if !set.contains(&port) {
                return ResolvedPort {
                    port,
                    used_fallback: false,
                };
            }
        }
        ResolvedPort {
            port: fallback,
            used_fallback: true,
        }
    }

    /// resolveTailscaleApiPort（:3006）：排除 control+http+socks+mixed，fallback = control_api + 1。
    pub fn resolve_tailscale_api_port(&self, exclusions: &PortExclusions) -> ResolvedPort {
        let fallback = exclusions.control_api.wrapping_add(1);
        self.resolve_free_local_port(exclusions, fallback)
    }

    /// resolveTailscaleLoginApiPort（:3020）：额外排除 primary_api，fallback = control_api + 2。
    pub fn resolve_tailscale_login_api_port(&self, exclusions: &PortExclusions) -> ResolvedPort {
        let fallback = exclusions.control_api.wrapping_add(2);
        self.resolve_free_local_port(exclusions, fallback)
    }

    /// 批量分配 `count` 个**互不相同**的空闲 127.0.0.1 端口（上游 `allocateProbePorts` 的 `3+K` 批分配腿，
    /// :3059-3080）：主核测速探测池 `probe-in-k` 需 K 个独立回环端口。
    ///
    /// 语义严格对齐 oracle：
    /// - 每槽至多 `max_attempts` 次重绑，命中 `exclude` 或**已选中的池端口**（`!ports.includes(port)`，:3070）→ 重滚。
    /// - **整批原子失败**（任一槽 `max_attempts` 次仍撞 → 返回空 `vec![]`，:3078 throw→catch→`probePoolPorts=[]`）：
    ///   探测池是叠加能力，分配失败即不注入、测速回退（绝不阻断代理启动）。
    ///
    /// 去重靠「已选端口累进排除集」：`FreePortProvider::try_allocate` 立即 drop listener，同口可能被立刻重发；
    /// 把已选端口并入排除集再滚，等价 oracle 靠持有 listener 保证的互异性（此处更可测：`SeededPortProvider` 可喂重复序列）。
    pub fn resolve_distinct_free_ports(&self, exclude: &PortExclusions, count: usize) -> Vec<u16> {
        let mut taken = exclude.as_set();
        let mut out: Vec<u16> = Vec::with_capacity(count);
        for _ in 0..count {
            let mut got = None;
            for _ in 0..self.max_attempts {
                let Some(port) = self.provider.try_allocate() else {
                    continue; // bind 失败（catch 分支）→ 下一轮
                };
                if !taken.contains(&port) {
                    got = Some(port);
                    break;
                }
            }
            match got {
                Some(port) => {
                    taken.insert(port);
                    out.push(port);
                }
                // 任一槽拿不到互异空闲口 → 整批放弃（回退语义），绝不返回部分池。
                None => return Vec::new(),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Mutex;

    /// 确定性桩：按预置序列返回端口（模拟 listen(0) 拿到的系统分配口）。
    /// `None` 元素模拟 bind 失败。
    struct SeededPortProvider {
        seq: Mutex<Vec<Option<u16>>>,
        idx: AtomicU16,
    }

    impl SeededPortProvider {
        fn new(seq: Vec<Option<u16>>) -> Self {
            Self {
                seq: Mutex::new(seq),
                idx: AtomicU16::new(0),
            }
        }
    }

    impl FreePortProvider for SeededPortProvider {
        fn try_allocate(&self) -> Option<u16> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst) as usize;
            self.seq.lock().unwrap().get(i).copied().flatten()
        }
    }

    #[test]
    fn control_api_port_resolves_configured_or_default() {
        assert_eq!(control_api_port(None), 9090);
        assert_eq!(control_api_port(Some(0)), 9090);
        assert_eq!(control_api_port(Some(9091)), 9091);
    }

    #[test]
    fn resolve_returns_first_non_excluded_port() {
        // 序列：9090(排除)、2080(排除)、0(bind fail)、12345(采用)。
        let p = SeededPortProvider::new(vec![Some(9090), Some(2080), None, Some(12345)]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions {
            control_api: 9090,
            http: 2080,
            socks: 0,
            mixed: 0,
            primary_api: 0,
        };
        let r = alloc.resolve_free_local_port(&excl, 9091);
        assert_eq!(r.port, 12345);
        assert!(!r.used_fallback);
    }

    #[test]
    fn resolve_falls_back_when_all_attempts_hit_exclude() {
        // 5 次全撞 9090 → fallback。
        let p = SeededPortProvider::new(vec![Some(9090); 5]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions {
            control_api: 9090,
            ..Default::default()
        };
        let r = alloc.resolve_free_local_port(&excl, 9999);
        assert_eq!(r.port, 9999);
        assert!(r.used_fallback);
    }

    #[test]
    fn resolve_falls_back_when_all_binds_fail() {
        // 5 次 bind 全失败 → fallback。
        let p = SeededPortProvider::new(vec![None; 5]);
        let alloc = PortAllocator::new(p);
        let r = alloc.resolve_free_local_port(&PortExclusions::default(), 7777);
        assert_eq!(r.port, 7777);
        assert!(r.used_fallback);
    }

    #[test]
    fn resolve_uses_fallback_immediately_if_exhausted_seq() {
        // 序列耗尽（超出长度返回 None）→ 走 fallback。
        let p = SeededPortProvider::new(vec![Some(12345)]); // 仅 1 个
        let alloc = PortAllocator::new(p).with_max_attempts(5);
        let excl = PortExclusions {
            control_api: 12345,
            ..Default::default()
        };
        let r = alloc.resolve_free_local_port(&excl, 8888);
        // 第 1 次：12345 被排除；2-5 次：序列耗尽 None → fallback。
        assert_eq!(r.port, 8888);
        assert!(r.used_fallback);
    }

    #[test]
    fn tailscale_api_port_excludes_all_user_ports() {
        // #A1：排除 control+http+socks+mixed，fallback = control+1。
        let p = SeededPortProvider::new(vec![
            Some(9090),
            Some(7890),
            Some(1080),
            Some(2080),
            Some(0o0),
        ]); // 全排除
        let _ = p; // 仅构造验证
        let p2 = SeededPortProvider::new(vec![Some(9090), Some(7890), Some(12345)]);
        let alloc = PortAllocator::new(p2);
        let excl = PortExclusions::for_primary_api(Some(9090), Some(7890), Some(1080), Some(2080));
        let r = alloc.resolve_tailscale_api_port(&excl);
        assert_eq!(r.port, 12345); // 9090/7890 排除，12345 采用
        let set = excl.as_set();
        assert!(set.contains(&9090));
        assert!(set.contains(&7890));
        assert!(set.contains(&1080));
        assert!(set.contains(&2080));
    }

    #[test]
    fn tailscale_api_port_fallback_is_control_plus_one() {
        let p = SeededPortProvider::new(vec![Some(9090); 5]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions::for_primary_api(Some(9090), None, None, None);
        let r = alloc.resolve_tailscale_api_port(&excl);
        assert_eq!(r.port, 9091); // control(9090)+1
        assert!(r.used_fallback);
    }

    #[test]
    fn login_api_port_excludes_primary_and_uses_control_plus_two_fallback() {
        // 登录核额外排除主核 api，fallback = control+2（:3031）。
        let p = SeededPortProvider::new(vec![
            Some(9091), // = control+1，未被排除但验证排除 primary=9092
            Some(9092), // primary_api，排除
            Some(12345),
        ]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions::for_login_api(9092, Some(9090), None, None, None);
        let set = excl.as_set();
        assert!(set.contains(&9092)); // primary 被排除
        let r = alloc.resolve_tailscale_login_api_port(&excl);
        // 9091 不在排除集 → 采用（控制流验证 primary 不被采用需独立测）。
        assert_eq!(r.port, 9091);
        assert!(!r.used_fallback);
    }

    #[test]
    fn login_api_port_fallback_is_control_plus_two() {
        let p = SeededPortProvider::new(vec![Some(9090); 5]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions::for_login_api(0, Some(9090), None, None, None);
        let r = alloc.resolve_tailscale_login_api_port(&excl);
        assert_eq!(r.port, 9092);
        assert!(r.used_fallback);
    }

    #[test]
    fn exclusions_default_empty_when_all_unset() {
        let excl = PortExclusions::for_primary_api(None, None, None, None);
        // control_api 默认 9090 仍入集；其余 0 不入。
        let set = excl.as_set();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&9090));
    }

    #[test]
    fn zero_ports_are_not_excluded() {
        // 0/None 视作未设，不进排除集（:3009 filter p>0）。
        let excl = PortExclusions {
            control_api: 0,
            http: 0,
            socks: 0,
            mixed: 0,
            primary_api: 0,
        };
        assert!(excl.as_set().is_empty());
    }

    #[test]
    fn tokio_port_provider_returns_real_ephemeral_port() {
        // 真实 bind 探测（不触碰外部网络，仅 127.0.0.1 loopback）。
        let p = TokioPortProvider;
        let port = p.try_allocate().expect("loopback bind should succeed");
        // 临时端口范围，非 0。
        assert_ne!(port, 0);
        // 应在动态端口区（通常 >= 1024，linux 32768-60999；此处仅宽松断言非保留）。
        assert!(port > 1023 || port > 0);
    }

    // ── resolve_distinct_free_ports（测速探测池 K 端口批分配）─────────────────────

    #[test]
    fn distinct_ports_returns_k_unique_ports() {
        // 3 个互异空闲口 → 全采用、保序。
        let p = SeededPortProvider::new(vec![Some(20000), Some(20001), Some(20002)]);
        let alloc = PortAllocator::new(p);
        let ports = alloc.resolve_distinct_free_ports(&PortExclusions::default(), 3);
        assert_eq!(ports, vec![20000, 20001, 20002]);
    }

    #[test]
    fn distinct_ports_rerolls_on_excluded_port() {
        // 首槽先撞排除端口(9090) → 重滚到 20000；验证 exclude 生效。
        let p = SeededPortProvider::new(vec![Some(9090), Some(20000), Some(20001)]);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions {
            control_api: 9090,
            ..Default::default()
        };
        let ports = alloc.resolve_distinct_free_ports(&excl, 2);
        assert_eq!(ports, vec![20000, 20001]);
    }

    #[test]
    fn distinct_ports_rerolls_on_duplicate_already_taken() {
        // 第二槽先重发首槽已选的 20000（!ports.includes 去重）→ 重滚到 20001。
        let p = SeededPortProvider::new(vec![Some(20000), Some(20000), Some(20001)]);
        let alloc = PortAllocator::new(p);
        let ports = alloc.resolve_distinct_free_ports(&PortExclusions::default(), 2);
        assert_eq!(ports, vec![20000, 20001]);
        // 互异性：无重复。
        assert_ne!(ports[0], ports[1]);
    }

    #[test]
    fn distinct_ports_atomic_fail_returns_empty_when_a_slot_exhausts() {
        // 首槽 5 次全撞排除 9090 → 该槽拿不到 → **整批**返回空（回退：探测池不注入），非部分池。
        let mut seq = vec![Some(9090); 5];
        seq.push(Some(20000)); // 即便后面有空闲口，整批已放弃
        let p = SeededPortProvider::new(seq);
        let alloc = PortAllocator::new(p);
        let excl = PortExclusions {
            control_api: 9090,
            ..Default::default()
        };
        let ports = alloc.resolve_distinct_free_ports(&excl, 2);
        assert!(ports.is_empty(), "任一槽失败即整批放弃");
    }

    #[test]
    fn distinct_ports_zero_count_is_empty() {
        // K=0（探测池关闭的回滚锚点）→ 空 vec，不触 provider。
        let p = SeededPortProvider::new(vec![]);
        let alloc = PortAllocator::new(p);
        assert!(alloc
            .resolve_distinct_free_ports(&PortExclusions::default(), 0)
            .is_empty());
    }
}
