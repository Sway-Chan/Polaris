//! 系统操作 trait 抽象 —— systemctl / ip route / TUN tuntap 的可测试边界。
//!
//! ## 设计动机（§D 特权矩阵 + 移植纪律 #4）
//!
//! Polaris Go 源 `helper-linux/helper.go` 用 AmbientCaps 把 CAP_NET_ADMIN 挂在 sing-box 进程上，
//! **TUN/路由由核 sing-tun 自己装**（非 helper 装路由）。但本任务要求把所有系统操作用 trait 抽象、测试 mock、
//! 不碰宿主网络。故把 helper 侧的系统副作用归类为三组 trait：
//!
//! - [`SystemdOps`]：systemd unit 安装/启停（任务职责 1，对照 §D.3 systemd 行）。
//! - [`TunOps`]：TUN 接口创建/销毁（任务职责 3，对照 上游 `ip tuntap` / ioctl）。
//! - [`RouteOps`]：路由表操作（任务职责 4，对照 上游 `ip route`）。
//!
//! **DNS 刷新不在此列**（2026-07-16 调和）：上游 Linux helper 无 DNS 命令，且刷缓存非提权操作 →
//! 单一真值在 `system-integration::dns_flush`（app 进程侧）。判据见下方「系统 DNS 刷新」段。
//!
//! 每组 trait 有生产实现（经 `tokio::process::Command` 调 `systemctl`/`ip`）与
//! mock 实现（记录调用、可断言），让命令处理逻辑在不碰宿主的前提下全路径测试。
//!
//! 不变式：所有命令处理函数只依赖 trait（不直接 Command::new），测试注入 mock 即可断言副作用。
//!
//! ## DESIGN-REVIEW(linux-ops-dormant)：TunOps / RouteOps / SystemdOps **忠实休眠**（C6-2 决策）
//!
//! Go 源 `helper-linux/helper.go` 的命令集 = ping|version|status|start|stop|cleanup|freeport|install-core
//! —— **无 route / tun / systemd 命令**：核 sing-tun 自建 TUN + 自装路由（CAP_NET_ADMIN 在 ambient set，
//! 见 [`server::apply_privilege_drop`](crate::platform::linux::server)），helper 侧不碰路由。故本三组 trait
//! 是 Polaris 自有增强（range-expansion，[[polaris-code-audit]] §3.3）：**保留但不接 `handler` dispatch**
//! （铁律：非缺陷不删自有）。价值待未来（手动 tuntap 模式 / helper 自管 systemd unit）兑现时接线。
//! `SystemdOps` 仍在 [`HandlerDeps`](crate::platform::linux::handler::HandlerDeps) 里占位（同休眠），
//! 无命令消费。

// std::path::Path 在本模块的测试中被引用（path_type_referenced 测试）。
#[cfg(test)]
use std::path::Path;

// ===== systemd 服务管理（任务职责 1）=====

/// systemd unit 操作请求（安装/启动/停止 helper 服务）。
///
/// 对照 §D.3 systemd 行：helper 作为 root system service，装一次（pkexec 一次授权）后
/// 普通用户 app 经 socket 零提权启停 sing-box。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemdAction {
    /// 安装 unit 文件 + daemon-reload + enable（首次部署）。
    Install,
    /// systemctl start <unit>。
    Start,
    /// systemctl stop <unit>。
    Stop,
    /// systemctl restart <unit>。
    Restart,
    /// 自卸载：stop + disable + remove unit + daemon-reload。
    Uninstall,
}

impl SystemdAction {
    /// 对应的 systemctl 子命令名（便于生产实现拼 argv）。
    #[must_use]
    pub const fn systemctl_verb(self) -> &'static str {
        match self {
            Self::Install => "enable",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Uninstall => "disable",
        }
    }
}

/// systemd 操作结果（成功无 payload；失败带 stderr 尾部文本供诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdResult {
    /// 是否成功（systemctl exit code == 0）。
    pub ok: bool,
    /// 失败时的 stderr/stdout 合并文本（trim 后）。
    pub detail: String,
}

impl Default for SystemdResult {
    /// 默认 = 成功无 payload（mock 构造用，对齐 [`SystemdResult::ok`]）。
    fn default() -> Self {
        Self {
            ok: true,
            detail: String::new(),
        }
    }
}

impl SystemdResult {
    /// 成功（无 payload）。
    #[must_use]
    pub const fn ok() -> Self {
        Self {
            ok: true,
            detail: String::new(),
        }
    }

    /// 失败，带诊断文本。
    #[must_use]
    pub fn err(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

/// systemd 操作抽象（trait 便于测试 mock；生产用 [`TokioSystemd`]）。
pub trait SystemdOps: Send + Sync {
    /// 对指定 unit 执行 `action`。
    fn run(&self, unit: &str, action: SystemdAction) -> SystemdResult;
}

/// tokio::process 调 systemctl 的生产实现。
#[derive(Debug, Default, Clone)]
pub struct TokioSystemd;

impl TokioSystemd {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl SystemdOps for TokioSystemd {
    fn run(&self, unit: &str, action: SystemdAction) -> SystemdResult {
        // systemctl <verb> <unit>。Install/Uninstall 额外需要 daemon-reload，但本 helper 侧
        // 假定 unit 文件由安装器（pkexec 一次性）部署，helper 只做运行期 start/stop/restart。
        // verb 选用 systemctl_verb（enable/disable/start/stop/restart）。
        let output = std::process::Command::new("systemctl")
            .arg(action.systemctl_verb())
            .arg(unit)
            .output();
        match output {
            Ok(o) if o.status.success() => SystemdResult::ok(),
            Ok(o) => SystemdResult::err(trim_lossy(&o)),
            Err(e) => SystemdResult::err(e.to_string()),
        }
    }
}

// ===== TUN 接口（任务职责 3）=====

/// TUN 接口操作（对照 上游 `ip tuntap add` / sing-tun 自动建 tun）。
///
/// 注：Polaris Linux 用 AmbientCaps 让 sing-box 自建 TUN（CAP_NET_ADMIN 在核进程）。
/// 本 trait 抽象 helper 侧若需手动建/毁 TUN 的边界（如未来手动 tuntap 模式），
/// 当前主路径仍是核自建 —— trait 保留以覆盖 §D 特权矩阵的可测试性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunAction {
    /// `ip tuntap add dev <name> mode tun`。
    Create { name: String },
    /// `ip tuntap del dev <name> mode tun`。
    Destroy { name: String },
}

/// TUN 操作抽象。
pub trait TunOps: Send + Sync {
    /// 执行 TUN 创建/销毁。成功返回空；失败返回诊断文本。
    fn run(&self, action: &TunAction) -> Result<(), String>;
}

/// 生产实现：`ip tuntap add/del`。
#[derive(Debug, Default, Clone)]
pub struct TokioTun;

impl TunOps for TokioTun {
    fn run(&self, action: &TunAction) -> Result<(), String> {
        let (verb, name) = match action {
            TunAction::Create { name } => ("add", name.as_str()),
            TunAction::Destroy { name } => ("del", name.as_str()),
        };
        let out = std::process::Command::new("ip")
            .args(["tuntap", verb, "dev", name, "mode", "tun"])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(trim_lossy(&out))
        }
    }
}

// ===== 路由表操作（任务职责 4）=====

/// 路由操作请求（对照 上游 `ip route add/del`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteAction {
    /// add 或 del。
    pub verb: RouteVerb,
    /// 目标 CIDR（如 `10.0.0.0/8`）。
    pub cidr: String,
    /// 下一跳 / 出口接口（如 `dev polaris-ts` 或 `via 10.0.0.1`）。
    pub via: String,
}

/// 路由增删动词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteVerb {
    Add,
    Del,
}

impl RouteVerb {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Del => "del",
        }
    }
}

/// 路由操作抽象（trait 便于测试 mock；生产用 [`TokioRoute`]）。
pub trait RouteOps: Send + Sync {
    /// 执行 `ip route add/del <cidr> <via>`。成功返回空；失败返回诊断文本。
    fn run(&self, action: &RouteAction) -> Result<(), String>;
}

/// 生产实现：`ip route add/del`。
#[derive(Debug, Default, Clone)]
pub struct TokioRoute;

impl RouteOps for TokioRoute {
    fn run(&self, action: &RouteAction) -> Result<(), String> {
        let out = std::process::Command::new("ip")
            .args(["route", action.verb.as_str(), &action.cidr])
            .arg(&action.via)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(trim_lossy(&out))
        }
    }
}

// ===== 系统 DNS 刷新：不在 Linux helper 的职责内（2026-07-16 调和，勿重新加回）=====
//
// 此处曾有 `DnsOps` / `TokioDns` / `DnsFlushOutcome`（resolvectl + resolvconf -u 回退），已删。
// 判据（逐条对上游实证，非偏好）：
//
// 1. **无上游**：Polaris Go 源 `helper-linux/helper.go` 的命令集是
//    ping|version|status|stop|cleanup|freeport|install-core|start —— **没有任何 DNS 命令**。
//    `flush-dns` 只存在于 **macOS** helper（`helper/helper.go:492`），因为那里真需要 root
//    （`killall -HUP mDNSResponder`）。被删代码的 `OK flushed` / `OK flushed-partial` 结果枚举
//    正是从 mac helper 抄来的 —— 那是 `dscacheutil` + `HUP mDNSResponder` 两层缓存的语义，Linux 无对应物。
// 2. **权限层级错位**：Linux 的 `resolvectl flush-caches` 由 **app 进程非提权直接调**
//    （上游 `os-dns-flush.ts:82`）。放进 root helper 等于为一个不需要 root 的缓存刷新
//    加一次 IPC 往返 + 提权面。
// 3. **`resolvconf -u` 上游零出现**（全仓 .go/.ts grep 无命中），且它**不是缓存刷新** ——
//    它是从 resolvconf 数据库重新生成 /etc/resolv.conf。无 systemd-resolved 的机器通常
//    根本没有 OS 级 DNS 缓存（glibc 不缓存）→ 无物可刷。
// 4. **该回退在其立论场景里不可达**：`Command::output()` 只在**二进制缺失/无法 spawn** 时返 `Err`。
//    resolvectl 存在但 resolved 未运行 → 返 `Ok(非零退出)` → 走 `FlushedPartial`，**永不落到 resolvconf**。
//    即「systemd-resolved 装了但没跑」这个唯一值得回退的场景，回退根本不触发。
// 5. **零调用点**：`handler.rs` 从不 dispatch 到它，仅 `mod.rs` 重导出。
//
// 单一真值 → `crates/system-integration/src/dns_flush.rs`（1:1 移植 `os-dns-flush.ts`，app 进程侧，
// 三平台 + mac helper 通道）。**已知缺口**（上游同样没有，如实登记而非静默补）：非 systemd 的 Linux
// 若跑 nscd/dnsmasq 本地缓存，`resolvectl` 缺失 → 不刷。上游行为一致（仅 log warn）。

// ===== 辅助：stderr/stdout trim 为 String（utf8 lossy，对齐 Go string(out)）=====

fn trim_lossy(o: &std::process::Output) -> String {
    let mut s = String::new();
    if !o.stdout.is_empty() {
        s.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    if !o.stderr.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&String::from_utf8_lossy(&o.stderr));
    }
    s.trim().to_string()
}

/// IP 转发开关（移植自 Go `setForward`，:172-179）。
///
/// allowLan 时开 IPv4+IPv6 转发（直写 /proc/sys）；stop 复位为 0，使转发态严格跟随运行中的核。
/// best-effort（写失败静默忽略，Go: `_ = os.WriteFile(...)`）。
///
/// 抽象为闭包注入便于测试；生产用 [`set_forward_prod`]。
pub fn set_forward_prod(on: bool) {
    let v = if on { b"1" } else { b"0" };
    // best-effort：忽略失败（非 root / proc 未挂载等）。
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", v);
    let _ = std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", v);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::sync::Mutex;

    // ===== SystemdOps mock + 测试 =====

    /// 记录所有 systemctl 调用的 mock（线程安全，可断言副作用序）。
    #[derive(Debug, Default)]
    struct MockSystemd {
        calls: Mutex<Vec<(String, SystemdAction)>>,
        /// 固定返回值（每次 run 都返回此）。
        result: SystemdResult,
    }

    impl MockSystemd {
        fn succeeding() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: SystemdResult::ok(),
            }
        }

        fn failing(detail: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: SystemdResult::err(detail),
            }
        }

        fn snapshot(&self) -> Vec<(String, SystemdAction)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SystemdOps for MockSystemd {
        fn run(&self, unit: &str, action: SystemdAction) -> SystemdResult {
            self.calls.lock().unwrap().push((unit.to_string(), action));
            self.result.clone()
        }
    }

    #[test]
    fn systemd_action_verb_mapping() {
        // systemctl 子命令名是 wire 兼容/运维契约（改名 = 断 systemctl 调用）。
        assert_eq!(SystemdAction::Install.systemctl_verb(), "enable");
        assert_eq!(SystemdAction::Start.systemctl_verb(), "start");
        assert_eq!(SystemdAction::Stop.systemctl_verb(), "stop");
        assert_eq!(SystemdAction::Restart.systemctl_verb(), "restart");
        assert_eq!(SystemdAction::Uninstall.systemctl_verb(), "disable");
    }

    #[test]
    fn mock_systemd_records_calls_and_returns_ok() {
        let m = MockSystemd::succeeding();
        let r = m.run("polaris-helper.service", SystemdAction::Start);
        assert!(r.ok);
        assert_eq!(
            m.snapshot(),
            vec![("polaris-helper.service".to_string(), SystemdAction::Start)]
        );
    }

    #[test]
    fn mock_systemd_returns_failure_detail() {
        let m = MockSystemd::failing("unit not loaded");
        let r = m.run("polaris-helper.service", SystemdAction::Stop);
        assert!(!r.ok);
        assert_eq!(r.detail, "unit not loaded");
    }

    #[test]
    fn mock_systemd_records_sequence_of_actions() {
        // 验证 install → start → stop 的副作用序（对应 helper 生命周期）。
        let m = MockSystemd::succeeding();
        m.run("u", SystemdAction::Install);
        m.run("u", SystemdAction::Start);
        m.run("u", SystemdAction::Stop);
        let snap = m.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].1, SystemdAction::Install);
        assert_eq!(snap[1].1, SystemdAction::Start);
        assert_eq!(snap[2].1, SystemdAction::Stop);
    }

    // ===== TunOps mock + 测试 =====

    #[derive(Debug, Default)]
    struct MockTun {
        calls: Mutex<Vec<TunAction>>,
        fail: bool,
    }

    // bool 的默认值 false 即 MockTun 的成功路径。

    impl TunOps for MockTun {
        fn run(&self, action: &TunAction) -> Result<(), String> {
            self.calls.lock().unwrap().push(action.clone());
            if self.fail {
                Err("tuntap busy".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn tun_action_create_destroy_roundtrip() {
        let m = MockTun::default();
        m.run(&TunAction::Create {
            name: "polaris-ts".into(),
        })
        .unwrap();
        m.run(&TunAction::Destroy {
            name: "polaris-ts".into(),
        })
        .unwrap();
        let snap = m.calls.lock().unwrap().clone();
        assert_eq!(snap.len(), 2);
        assert!(matches!(&snap[0], TunAction::Create { name } if name == "polaris-ts"));
        assert!(matches!(&snap[1], TunAction::Destroy { name } if name == "polaris-ts"));
    }

    #[test]
    fn tun_action_failure_propagates() {
        let m = MockTun {
            calls: Mutex::new(Vec::new()),
            fail: true,
        };
        let r = m.run(&TunAction::Create { name: "x".into() });
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), "tuntap busy");
    }

    // ===== RouteOps mock + 测试 =====

    #[derive(Debug, Default)]
    struct MockRoute {
        calls: Mutex<Vec<RouteAction>>,
    }

    impl RouteOps for MockRoute {
        fn run(&self, action: &RouteAction) -> Result<(), String> {
            self.calls.lock().unwrap().push(action.clone());
            Ok(())
        }
    }

    #[test]
    fn route_verb_as_str() {
        assert_eq!(RouteVerb::Add.as_str(), "add");
        assert_eq!(RouteVerb::Del.as_str(), "del");
    }

    #[test]
    fn route_action_recorded() {
        let m = MockRoute::default();
        m.run(&RouteAction {
            verb: RouteVerb::Add,
            cidr: "10.0.0.0/8".into(),
            via: "dev polaris-ts".into(),
        })
        .unwrap();
        let snap = m.calls.lock().unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].cidr, "10.0.0.0/8");
        assert_eq!(snap[0].via, "dev polaris-ts");
    }

    // ===== trim_lossy =====

    #[test]
    fn trim_lossy_combines_stdout_stderr() {
        let o = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"out line".to_vec(),
            stderr: b"err line".to_vec(),
        };
        let s = trim_lossy(&o);
        assert_eq!(s, "out line err line");
    }

    #[test]
    fn trim_lossy_empty_when_no_output() {
        let o = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(trim_lossy(&o), "");
    }

    // ===== set_forward_prod（best-effort，非 root 不 panic）=====

    #[test]
    fn set_forward_prod_does_not_panic_when_not_root() {
        // 写 /proc/sys 需要 root；非 root 环境应静默忽略（best-effort，对齐 Go `_ =`）。
        set_forward_prod(true);
        set_forward_prod(false);
        // 不 panic 即通过。
    }

    // ===== SystemdResult helpers =====

    #[test]
    fn systemd_result_ok_no_detail() {
        let r = SystemdResult::ok();
        assert!(r.ok);
        assert!(r.detail.is_empty());
    }

    #[test]
    fn systemd_result_err_carries_detail() {
        let r = SystemdResult::err("boom");
        assert!(!r.ok);
        assert_eq!(r.detail, "boom");
    }

    /// 静态断言：trait 是对象安全的（可 `Box<dyn Trait>`，生产环境注入用）。
    #[allow(dead_code)]
    fn _assert_object_safety(_s: Box<dyn SystemdOps>, _t: Box<dyn TunOps>, _r: Box<dyn RouteOps>) {}

    /// 静态断言：Send + Sync 约束满足（tokio spawn 跨 await 需要）。
    #[allow(dead_code)]
    fn _assert_send_sync(
        _s: &(dyn SystemdOps + Send + Sync),
        _t: &(dyn TunOps + Send + Sync),
        _r: &(dyn RouteOps + Send + Sync),
    ) {
    }

    /// Path 引用避免未用 import 警告（owned_by 等用 Path，此模块仅类型引用）。
    #[test]
    fn path_type_referenced() {
        let _ = Path::new("/tmp");
    }
}
