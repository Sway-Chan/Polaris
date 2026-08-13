//! 系统操作 trait 抽象（移植自 `helper-win/winproc.go` 的全部副作用原语）。
//!
//! ## 为什么 trait
//!
//! Go 源 `winproc.go` 全是直接 syscall（`windows.OpenProcess` / `CreateToolhelp32Snapshot` /
//! `TerminateProcess` / `GetExtendedTcpTable` / 注册表写）—— 在 Linux CI 上无法编译，更无法单测。
//! 本模块把这些副作用原语抽象为 trait，让 [`crate::platform::windows::helper`] 的协议分派逻辑能脱离真实 Windows 测试：
//! - 生产实现：[`crate::platform::windows::winproc::WinProcOps`]（`#[cfg(windows)]`，真实 FFI）。
//! - 测试 mock：本模块的 [`MockProcOps`]（纯内存，Linux 上跑）。
//!
//! ## trait 边界
//!
//! 每个 trait 方法对应一个 Go `winproc.go` 函数，签名镜像其语义（返回值、best-effort 语义）。错误处理对齐 Go：
//! 大多是 best-effort（失败不阻断主流程），trait 方法返回 `Result` 但调用方据场景决定是否忽略。

use crate::platform::windows::logic::{self, ListenEntry};

/// 进程操作抽象（对应 `winproc.go` 的进程/Job/freeport-kill/singbox-spawn/旁路 spawn 原语）。
///
/// 生产实现见 [`crate::platform::windows::winproc::WinProcOps`]。所有方法对齐 Go 源的 best-effort 语义。
pub trait ProcOps: Send + Sync {
    /// 判 ppid 是否存活（`winproc.go:123-143` `processAlive`）。
    ///
    /// 「确定已死」返回 `false`；不确定（句柄打开失败等非「确定已死」）按 Go「宁漏勿误」返回 `true`。
    fn process_alive(&self, ppid: u32) -> bool;

    /// 硬杀指定 pid（`winproc.go:146-153` `terminatePid`）。
    ///
    /// 成功返回 `Ok(())`；pid 不存在/无权限 → `Err`（调用方按场景忽略或回报）。
    fn terminate_pid(&self, pid: u32) -> std::io::Result<()>;

    /// 取 pid 的可执行映像全路径（`winproc.go:157-169` `processImageName`）。失败返回空串。
    ///
    /// 仅映像路径、不含命令行参数（镜像 macOS `ps -o comm=`），避免「参数里碰巧含 sing-box」被误判。
    fn process_image_name(&self, pid: u32) -> String;

    /// 杀所有运行中的「锁定 singbox」实例（`winproc.go:184-214` `killAllSingbox`）。
    ///
    /// 按映像全路径大小写不敏感匹配 `singbox_bin` → TerminateProcess。返回杀掉的实例数。
    fn kill_all_singbox(&self, singbox_bin: &str) -> usize;

    /// 启动 sing-box 子进程（`winproc.go:21-49` `startSingbox`）。
    ///
    /// 返回子进程 pid（Go 返回 `*exec.Cmd`，本 trait 只需 pid + 可终止语义）。
    /// `log_path` 非空 → 子进程 stdout/stderr 重定向到此文件（早期启动诊断）。
    /// `fwd`（allowLan）= true → 启动前 best-effort 开 IP 转发（`enableIPForwarding`）。
    fn start_singbox(
        &self,
        singbox_bin: &str,
        cfg: &str,
        log_path: &str,
        fwd: bool,
    ) -> std::io::Result<u32>;

    /// 优雅停信号 → 等宽限 → 硬杀（`helper.go:111-123` `terminateChild` 的等价物）。
    ///
    /// Go 源：`sendCtrlBreak(pid)` → `select { done | 2s timeout }` → `Process.Kill`。
    /// session-0 无 console → CTRL_BREAK 是 no-op → 硬杀是预期收割手段。
    /// 本方法封装「对指定 pid 的收割序列」，宽限窗口由实现决定（生产 = 2s，测试可缩短）。
    fn reap_child(&self, pid: u32);

    /// 派生自卸载旁路 cmd（`winproc.go:233-245` `spawnSelfUninstall`）。
    ///
    /// best-effort：失败只记日志（Go `_ = c.Start()`）。旁路须比 helper 活得久（DETACHED_PROCESS、
    /// 不 assignToJob、继承 SYSTEM token）。
    fn spawn_self_uninstall(&self, service_name: &str, support_dir: &str);

    /// 开 IP 转发（`winproc.go:414-427` `enableIPForwarding`）。
    ///
    /// IPv4：注册表 `IPEnableRouter=1`（持久写入，stop/卸载不还原 —— 已知限制，镜像 Go 现状）。
    /// IPv6：`netsh interface ipv6 set global forwarding=enabled`（运行时开关）。
    /// best-effort，失败不阻塞 start。
    fn enable_ip_forwarding(&self);

    /// route-add / route-del 执行（`helper.go:222-240`）：对单个已校验 CIDR 跑
    /// `netsh interface <ipv4|ipv6> add|delete route prefix=<cidr> interface=<iface> store=active`。
    ///
    /// `del`=false → add，true → delete。地址族由 `cidr` 是否含 `:` 决定（Go 同款）。best-effort
    /// （Go `_ = exec.Command(...).Run()`，失败不阻断）。调用方（helper 分派层）已过 iface 白名单 + CIDR 校验。
    fn apply_route(&self, iface: &str, cidr: &str, del: bool);

    /// 阻塞式收割（`helper.go:400-410` `reapChildOnExit` → 同步 `terminateChild`）。
    ///
    /// 与 [`reap_child`](ProcOps::reap_child) 的区别：本方法**同步**跑完收割序列（优雅信号→宽限→硬杀）
    /// 再返回 —— 供服务停止/关机路径用（进程即将退出，须在返回前确保 child 已被杀，杜绝孤儿）。
    /// 命令路径（stop/cleanup/uninstall）用异步 [`reap_child`](ProcOps::reap_child)（快回复，不阻塞管道）。
    fn reap_child_blocking(&self, pid: u32);

    /// 启动父死看护线程（`helper.go:131-159` `watchParent` + `helper.go:387-388` 的 `go watchParent`）。
    ///
    /// 每秒探测父 app（`ppid`）存活；父死且 `is_current(child_pid)` 仍为真 → `on_parent_dead(child_pid)`
    /// 收割 child。退出条件（防线程泄漏）：`is_current` 返 false（child 已被 stop/cleanup/新 start 摘除）
    /// 或父死收割完成。生产侧起后台线程（`&self` 不被线程捕获，用无状态 pid FFI）；mock 单次评估
    /// （测试双，无线程/sleep）。**调用方须在释放 child 锁后调用**（闭包会重入 child 锁）。
    fn spawn_watch_parent(
        &self,
        ppid: u32,
        child_pid: u32,
        is_current: Box<dyn Fn(u32) -> bool + Send>,
        on_parent_dead: Box<dyn Fn(u32) + Send>,
    );
}

/// 网络表操作抽象（对应 `winproc.go:300-406` `listenPidsForPort` 的 `GetExtendedTcpTable`）。
///
/// 生产实现见 [`crate::platform::windows::winproc::WinProcOps`]。把「枚举 LISTEN 持有者」与「Windows 字节布局」解耦：
/// 生产侧调 `GetExtendedTcpTable` 解析 IPv4+IPv6 表为 [`ListenEntry`] 列表；测试 mock 直接构造。
pub trait NetTableOps: Send + Sync {
    /// 返回所有 LISTEN 持有者（IPv4 + IPv6 合并，去重，port 为网络序→主机序已转换）。
    ///
    /// 镜像 macOS `lsof -ti tcp:<port> -sTCP:LISTEN`：仅 LISTEN 持有者，不含 ESTABLISHED 连接方。
    /// Go 源 `collect` 闭包已在此把 state==LISTEN 过滤 + 网络序 port 转 host 序。
    fn list_listen_entries(&self) -> std::io::Result<Vec<ListenEntry>>;

    /// 便利：返回在 `port` 上 LISTEN 的去重 pid 列表（`winproc.go:302-406` `listenPidsForPort`）。
    ///
    /// 默认实现基于 [`list_listen_entries`](NetTableOps::list_listen_entries) + [`logic::filter_listen_pids`]，
    /// 生产侧与测试 mock 共用同一过滤逻辑。
    fn listen_pids_for_port(&self, port: u16) -> std::io::Result<Vec<u32>> {
        let entries = self.list_listen_entries()?;
        Ok(logic::filter_listen_pids(&entries, port))
    }
}

// ===== 测试 mock =====

/// mock 共享内部状态（Arc 后，使 [`MockProcOps`] 可 clone 共享同一计数器）。
#[derive(Debug, Default)]
struct MockProcOpsInner {
    /// `start_singbox` 累计调用次数。
    pub start_calls: std::sync::atomic::AtomicUsize,
    /// `start_singbox` 下一次返回的 pid（递增，模拟每次新起子进程）。
    pub next_pid: std::sync::atomic::AtomicU32,
    /// `kill_all_singbox` 上次返回的杀掉数。
    pub kill_all_return: std::sync::atomic::AtomicUsize,
    /// `terminate_pid` 累计调用次数。
    pub terminate_calls: std::sync::atomic::AtomicUsize,
    /// `reap_child` 累计调用次数。
    pub reap_calls: std::sync::atomic::AtomicUsize,
    /// `spawn_self_uninstall` 累计调用次数。
    pub spawn_uninstall_calls: std::sync::atomic::AtomicUsize,
    /// `enable_ip_forwarding` 累计调用次数。
    pub ip_forward_calls: std::sync::atomic::AtomicUsize,
    /// `process_alive` 返回值（默认 true=存活，宁漏勿误）。
    pub alive_return: std::sync::atomic::AtomicBool,
    /// `process_image_name` 的 pid → 映像路径映射（freeport 测试用）。
    pub images: std::sync::Mutex<std::collections::HashMap<u32, String>>,
    /// `start_singbox` 失败时返回此错误（模拟启动失败 → `ERR start`）。
    pub start_error: std::sync::Mutex<Option<std::io::Error>>,
    /// 最近一次 `reap_child` 的 pid（断言用）。
    pub last_reaped_pid: std::sync::atomic::AtomicU32,
    /// 最近一次 `spawn_self_uninstall` 的参数（断言用）。
    pub last_spawn_args: std::sync::Mutex<Option<(String, String)>>,
    /// `apply_route` 累计调用次数（W9 netsh route 断言用）。
    pub route_calls: std::sync::atomic::AtomicUsize,
    /// 最近一次 `apply_route` 的参数（iface, cidr, del）（断言用）。
    pub last_route: std::sync::Mutex<Option<(String, String, bool)>>,
    /// `spawn_watch_parent` 累计调用次数（W15 看护接线断言用）。
    pub watch_parent_calls: std::sync::atomic::AtomicUsize,
    /// 最近一次 `spawn_watch_parent` 的 (ppid, child_pid)（断言用）。
    pub last_watch_args: std::sync::Mutex<Option<(u32, u32)>>,
}

/// 内存 mock 的 [`ProcOps`]（测试用）。
///
/// 记录所有调用供断言；行为可预设（start 返回的 pid、process_alive 的存活判定等）。
///
/// `Clone` 共享底层计数器（`Arc`）—— 测试可 `clone()` 一份传进 helper，原件保留读计数（helper take ownership
/// 后仍可经 clone 读到更新）。
#[derive(Debug, Default, Clone)]
pub struct MockProcOps {
    inner: std::sync::Arc<MockProcOpsInner>,
}

impl MockProcOps {
    /// 构造默认 mock（alive=true、next_pid=1000、images 空）。
    #[must_use]
    pub fn new() -> Self {
        let inner = MockProcOpsInner {
            next_pid: std::sync::atomic::AtomicU32::new(1000),
            alive_return: std::sync::atomic::AtomicBool::new(true),
            ..MockProcOpsInner::default()
        };
        Self {
            inner: std::sync::Arc::new(inner),
        }
    }

    /// 预设 pid → 映像路径（freeport 测试用）。
    pub fn set_image(&self, pid: u32, image: &str) {
        self.inner
            .images
            .lock()
            .unwrap()
            .insert(pid, image.to_owned());
    }

    /// 预设 `start_singbox` 失败（模拟 `ERR start`）。
    pub fn set_start_error(&self, e: std::io::Error) {
        *self.inner.start_error.lock().unwrap() = Some(e);
    }

    /// 预设 `process_alive` 返回值（父死看护测试用；false = 模拟父已死）。
    pub fn set_alive(&self, v: bool) {
        self.inner
            .alive_return
            .store(v, std::sync::atomic::Ordering::SeqCst);
    }

    /// 最近一次 `apply_route` 的参数（iface, cidr, del）（测试断言）。
    #[must_use]
    pub fn last_route(&self) -> Option<(String, String, bool)> {
        self.inner.last_route.lock().unwrap().clone()
    }

    /// 最近一次 `spawn_watch_parent` 的 (ppid, child_pid)（测试断言）。
    #[must_use]
    pub fn last_watch_args(&self) -> Option<(u32, u32)> {
        *self.inner.last_watch_args.lock().unwrap()
    }

    /// 取调用计数快照（测试断言用）。
    #[must_use]
    pub fn snapshot(&self) -> MockSnapshot {
        use std::sync::atomic::Ordering;
        MockSnapshot {
            start_calls: self.inner.start_calls.load(Ordering::SeqCst),
            kill_all_return: self.inner.kill_all_return.load(Ordering::SeqCst),
            terminate_calls: self.inner.terminate_calls.load(Ordering::SeqCst),
            reap_calls: self.inner.reap_calls.load(Ordering::SeqCst),
            spawn_uninstall_calls: self.inner.spawn_uninstall_calls.load(Ordering::SeqCst),
            ip_forward_calls: self.inner.ip_forward_calls.load(Ordering::SeqCst),
            route_calls: self.inner.route_calls.load(Ordering::SeqCst),
            watch_parent_calls: self.inner.watch_parent_calls.load(Ordering::SeqCst),
        }
    }

    /// 最近一次 `reap_child` 的 pid（测试断言）。
    #[must_use]
    pub fn last_reaped_pid(&self) -> u32 {
        self.inner
            .last_reaped_pid
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 最近一次 `spawn_self_uninstall` 的参数（测试断言）。
    #[must_use]
    pub fn last_spawn_args(&self) -> Option<(String, String)> {
        self.inner.last_spawn_args.lock().unwrap().clone()
    }

    /// 预设 `kill_all_singbox` 返回的杀掉数。
    pub fn set_kill_all_return(&self, n: usize) {
        self.inner
            .kill_all_return
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

/// mock 调用计数快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MockSnapshot {
    pub start_calls: usize,
    pub kill_all_return: usize,
    pub terminate_calls: usize,
    pub reap_calls: usize,
    pub spawn_uninstall_calls: usize,
    pub ip_forward_calls: usize,
    pub route_calls: usize,
    pub watch_parent_calls: usize,
}

impl ProcOps for MockProcOps {
    fn process_alive(&self, _ppid: u32) -> bool {
        // 宁漏勿误：默认 true（存活），测试父死看护时设 false
        self.inner
            .alive_return
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn terminate_pid(&self, _pid: u32) -> std::io::Result<()> {
        self.inner
            .terminate_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn process_image_name(&self, pid: u32) -> String {
        self.inner
            .images
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }

    fn kill_all_singbox(&self, _singbox_bin: &str) -> usize {
        self.inner
            .kill_all_return
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn start_singbox(
        &self,
        _singbox_bin: &str,
        _cfg: &str,
        _log_path: &str,
        fwd: bool,
    ) -> std::io::Result<u32> {
        self.inner
            .start_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if fwd {
            // 模拟生产：fwd=true 会调 enable_ip_forwarding（Go: if fwd=="1" { enableIPForwarding() }）
            self.enable_ip_forwarding();
        }
        if let Some(e) = self.inner.start_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(self
            .inner
            .next_pid
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }

    fn reap_child(&self, pid: u32) {
        self.inner
            .reap_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .last_reaped_pid
            .store(pid, std::sync::atomic::Ordering::SeqCst);
    }

    fn spawn_self_uninstall(&self, service_name: &str, support_dir: &str) {
        self.inner
            .spawn_uninstall_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.inner.last_spawn_args.lock().unwrap() =
            Some((service_name.to_owned(), support_dir.to_owned()));
    }

    fn enable_ip_forwarding(&self) {
        self.inner
            .ip_forward_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn apply_route(&self, iface: &str, cidr: &str, del: bool) {
        self.inner
            .route_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.inner.last_route.lock().unwrap() = Some((iface.to_owned(), cidr.to_owned(), del));
    }

    fn reap_child_blocking(&self, pid: u32) {
        // mock 里同步收割与异步 reap_child 等价记账（都计入 reap_calls）。
        self.inner
            .reap_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .last_reaped_pid
            .store(pid, std::sync::atomic::Ordering::SeqCst);
    }

    fn spawn_watch_parent(
        &self,
        ppid: u32,
        child_pid: u32,
        is_current: Box<dyn Fn(u32) -> bool + Send>,
        on_parent_dead: Box<dyn Fn(u32) + Send>,
    ) {
        self.inner
            .watch_parent_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.inner.last_watch_args.lock().unwrap() = Some((ppid, child_pid));
        // 测试双：单次评估父死看护逻辑（无后台线程 / 无 sleep）。父死（process_alive=false）且
        // child 仍当前 → on_parent_dead 收割。alive_return 默认 true → 不触发（仅验接线计数）。
        if !self.process_alive(ppid) && is_current(child_pid) {
            on_parent_dead(child_pid);
        }
    }
}

/// 内存 mock 的 [`NetTableOps`]（测试用）。
/// 内存 mock 的 [`NetTableOps`]（测试用）。`Clone` 共享底层 `Arc`（同 [`MockProcOps`]）。
#[derive(Debug, Default, Clone)]
pub struct MockNetTableOps {
    /// 预设的 LISTEN 表（测试构造任意占用场景）。
    entries: std::sync::Arc<std::sync::Mutex<Vec<ListenEntry>>>,
}

impl MockNetTableOps {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 预设 LISTEN 表。
    pub fn set_entries(&self, entries: Vec<ListenEntry>) {
        *self.entries.lock().unwrap() = entries;
    }
}

impl NetTableOps for MockNetTableOps {
    fn list_listen_entries(&self) -> std::io::Result<Vec<ListenEntry>> {
        Ok(self.entries.lock().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_proc_ops_records_calls() {
        let ops = MockProcOps::new();
        assert!(ops.process_alive(1234)); // 默认存活（宁漏勿误）
        let snap = ops.snapshot();
        let pid = ops
            .start_singbox("/x/sing-box", "/c/cfg.json", "", false)
            .unwrap();
        assert_eq!(pid, 1000);
        assert_eq!(ops.snapshot().start_calls, snap.start_calls + 1);
        ops.reap_child(1000);
        assert_eq!(ops.last_reaped_pid(), 1000);
    }

    #[test]
    fn mock_proc_ops_start_failure_returns_err() {
        let ops = MockProcOps::new();
        ops.set_start_error(std::io::Error::new(std::io::ErrorKind::NotFound, "ENOENT"));
        let r = ops.start_singbox("/x", "/c", "", false);
        assert!(r.is_err());
        // 第二次无预设错误 → 成功（模拟瞬时失败）
        assert!(ops.start_singbox("/x", "/c", "", false).is_ok());
    }

    #[test]
    fn mock_net_table_listen_pids_filters_target_port() {
        let ops = MockNetTableOps::new();
        ops.set_entries(vec![
            ListenEntry {
                pid: 100,
                port: 9090,
            },
            ListenEntry {
                pid: 200,
                port: 9090,
            },
            ListenEntry { pid: 300, port: 80 },
        ]);
        assert_eq!(ops.listen_pids_for_port(9090).unwrap(), vec![100, 200]);
        assert!(ops.listen_pids_for_port(9999).unwrap().is_empty());
    }
}
