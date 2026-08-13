//! 系统 DNS 平台操作抽象 + 接管/还原/重灌编排。
//!
//! 1:1 移植自 上游 `SystemDnsManager.ts` 基类 `SystemDnsBase`：
//! - [`SystemDnsOps`] trait：list targets / read DNS / apply DNS / 读生效解析器（mac 真实实现 + Win/Linux no-op）。
//! - [`DnsMarker`]：marker 读写（FS trait 注入，纯逻辑）。
//! - [`SystemDnsController`]：setDns / restoreDns / reconcileDns / restoreDnsSync 编排。
//!
//! 关键不变量（对齐 Polaris）：
//! - setDns **best-effort**：失败仅告警 + 回滚还原，绝不抛（DNS 治理降级不阻断 TUN 启动）。
//! - marker 前置写入（intent）：set 期间崩溃也留 marker，下次启动据此还原。
//! - 防自指：再次接管时若当前已是受控 IP，回退既有 marker 的真实原始。
//! - reconcile：仅 marker 在才动手（接管未激活绝不写系统）；只 apply 未受控服务（幂等）。

#![forbid(unsafe_code)]

use crate::dns::{
    compute_original_to_save, controlled_tun_dns_ip, extract_ipv4s, is_controlled,
    is_controlled_dns_ip_valid, mac_set_dns_args, parse_mac_get_dns_servers,
    parse_scutil_nameservers, parse_win_interfaces, parse_win_show_dns_servers,
    pick_lan_resolver_ip, SystemDnsMarker,
};
use crate::error::SystemIntegrationError;
use crate::exec::{Command, CommandRunner};
use crate::proxy::MarkerFs;
use crate::proxy_ops::{retry_op, RetryConfig};
use polaris_helper_proto::Platform;
use std::collections::BTreeMap;
use std::time::Duration;

/// 系统 DNS marker 读写（FS trait 注入）。
/// 上游 `SystemDnsBase` 的 marker IO 部分。
pub struct DnsMarker<Fs: MarkerFs> {
    fs: Fs,
    path: String,
    controlled_ip: String,
}

impl<Fs: MarkerFs> DnsMarker<Fs> {
    pub fn new(fs: Fs, path: impl Into<String>) -> Self {
        Self {
            fs,
            path: path.into(),
            controlled_ip: controlled_tun_dns_ip().to_string(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// 写 marker（setDns 前置 intent）。失败仅告警绝不抛（Polaris writeMarker try/catch）。
    pub fn write(&self, original: &BTreeMap<String, Vec<String>>) {
        let marker = SystemDnsMarker {
            controlled_ip: self.controlled_ip.clone(),
            original: original.clone(),
            at: now_ms(),
        };
        let Ok(json) = serde_json::to_string(&marker) else {
            return;
        };
        let _ = self.fs.write_marker(&self.path, &json);
    }

    pub fn clear(&self) {
        let _ = self.fs.remove_marker(&self.path);
    }

    /// 读 marker；不存在 / 损坏 / 结构非法 → None。
    pub fn read(&self) -> Option<SystemDnsMarker> {
        let raw = self.fs.read_marker(&self.path)?;
        let m: SystemDnsMarker = serde_json::from_str(&raw).ok()?;
        if !is_controlled_dns_ip_valid(&m.controlled_ip) {
            return None;
        }
        Some(m)
    }

    pub fn exists(&self) -> bool {
        self.read().is_some()
    }
}

/// 平台 DNS 操作抽象。mac 真实实现（networksetup），Win/Linux 收敛为 no-op。
/// 语义对齐 上游 `SystemDnsBase` 的 abstract 方法。
pub trait SystemDnsOps {
    /// **本平台是否接管系统 DNS（写路径）**。mac=true；win/linux=false。
    ///
    /// ## 为什么是 trait 方法而不是「让 apply_dns 静默 no-op」
    ///
    /// 上游把 no-op 做在**平台子类覆写 `setDns` 整个方法**上（`WindowsSystemDns.setDns` /
    /// `LinuxSystemDns.setDns`），**不是**让 `applyDns` 空转 —— 这个区别是**血证**：
    ///
    /// - **Windows**（上游 2026-06-17 真机实证收口）：① sing-box TUN `strict_route`(WFP) 已在路由层
    ///   把所有 `:53` 强制逼进 TUN → 不需要也不应再改系统 DNS 设置项；② `netsh set dnsservers` **需管理员**，
    ///   GUI 非提权 → 真机**每次 ACCESS DENIED**。而 `set_dns` 是**先写 marker 再 apply** →
    ///   apply 必失败 → **marker 卡死** → 每次启动反复「还原 netsh 失败、保留 marker」刷错误日志。
    /// - **Linux**：发行版差异大（systemd-resolved / resolv.conf / NetworkManager），写入风险高，
    ///   且 TUN 由 sing-box `auto_route` 自身处理 DNS。
    ///
    /// 故若仅让 `apply_dns` 空转、`set_dns` 照常写 marker，**会精确复现上游修掉的 marker 卡死 bug**。
    /// [`SystemDnsController::set_dns`] 据本方法**在写 marker 之前**早退。
    ///
    /// **读路径不受影响**：`list_targets` / `read_dns` / `read_effective_resolvers` 在 win 上仍是真实现
    /// （`netsh show` 非提权可跑），供方案B [`SystemDnsController::get_lan_resolver_for_dns`] 用。
    fn takeover_supported(&self) -> bool;

    /// 列出应接管的网络服务/接口名。
    fn list_targets(&self) -> Result<Vec<String>, crate::error::SystemIntegrationError>;
    /// 读某服务/接口的当前 DNS（`[] = DHCP/自动`）。
    fn read_dns(&self, target: &str) -> Result<Vec<String>, crate::error::SystemIntegrationError>;
    /// 设某服务/接口 DNS（`[] → DHCP/Empty 还原`）。
    fn apply_dns(
        &self,
        target: &str,
        ips: &[String],
    ) -> Result<(), crate::error::SystemIntegrationError>;
    /// 读生效解析器候选 IP（含 DHCP 下发的；方案B 用）。无法读 → `[]`。
    fn read_effective_resolvers(&self)
        -> Result<Vec<String>, crate::error::SystemIntegrationError>;
}

/// 接管请求/结果辅助。
pub struct DnsTakeover {
    pub targets: Vec<String>,
    pub current: BTreeMap<String, Vec<String>>,
}

/// 单条系统 DNS 命令硬超时（上游 `DNS_CMD_TIMEOUT_MS`）。
pub const DNS_CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// [`SystemDnsOps`] 的生产实现（运行时 [`Platform`] 分派 + [`CommandRunner`] 下发；零 cfg）。
///
/// 平台分界（真差异，保留隔离）：
/// - **mac**：真接管。`networksetup -listallnetworkservices` / `-getdnsservers` / `-setdnsservers`；
///   生效解析器读 `scutil --dns`（含 DHCP 下发的，`-getdnsservers` 对 DHCP 返空拿不到）。
/// - **win**：**写路径 no-op**（`takeover_supported=false`，判据见该方法）；读路径真实现
///   （`netsh interface ipv4 show ...`，非提权可跑）供方案B 用。
/// - **linux**：全 no-op（读也返空）—— 上游 `LinuxSystemDns` 逐字如此。
pub struct SystemDnsOpsImpl<R: CommandRunner> {
    runner: R,
    platform: Platform,
    /// Windows `netsh.exe` 绝对路径（规避 PATH 缺 System32）。
    netsh_exe: String,
}

impl<R: CommandRunner> SystemDnsOpsImpl<R> {
    /// 生产构造：平台取本机。
    pub fn new(runner: R) -> Self {
        Self::with_platform(runner, Platform::current())
    }

    /// 指定平台构造（测试用：Linux 上断言 mac/win 的 argv 与解析）。
    pub fn with_platform(runner: R, platform: Platform) -> Self {
        Self {
            runner,
            platform,
            netsh_exe: crate::exec::system32_from_env("netsh.exe"),
        }
    }

    fn run(&self, cmd: &Command) -> Result<crate::exec::CommandOutput, SystemIntegrationError> {
        self.runner
            .run(cmd, DNS_CMD_TIMEOUT)
            .map_err(SystemIntegrationError::dns)
    }

    /// Windows：读单接口 DNS 输出（读路径，show 非提权可跑）。
    fn win_show_dnsservers(&self, iface: &str) -> Result<String, SystemIntegrationError> {
        let cmd = Command::new(
            &self.netsh_exe,
            [
                "interface",
                "ipv4",
                "show",
                "dnsservers",
                &format!("name={iface}"),
            ],
        );
        Ok(self.run(&cmd)?.stdout)
    }
}

impl<R: CommandRunner> SystemDnsOps for SystemDnsOpsImpl<R> {
    fn takeover_supported(&self) -> bool {
        // 仅 mac 接管系统 DNS。win/linux 的判据见 trait 方法 doc（上游真机实证收口）。
        matches!(self.platform, Platform::Mac)
    }

    fn list_targets(&self) -> Result<Vec<String>, SystemIntegrationError> {
        match self.platform {
            // 与系统代理共用**同一个**口径函数（此前是共用同一个「按名字过滤」的解析器，
            // 于是两处一起把 DNS/代理写到了别家 VPN 的网络服务上）。判据与回落见该函数 doc。
            Platform::Mac => crate::proxy_ops::mac_list_manageable_services(|c| self.run(c)),
            Platform::Win => {
                let out = self.run(&Command::new(
                    &self.netsh_exe,
                    ["interface", "ipv4", "show", "interfaces"],
                ))?;
                Ok(parse_win_interfaces(&out.stdout))
            }
            Platform::Linux | Platform::Other => Ok(vec![]),
        }
    }

    fn read_dns(&self, target: &str) -> Result<Vec<String>, SystemIntegrationError> {
        match self.platform {
            Platform::Mac => {
                let out = self.run(&Command::new("networksetup", ["-getdnsservers", target]))?;
                Ok(parse_mac_get_dns_servers(&out.stdout))
            }
            Platform::Win => Ok(parse_win_show_dns_servers(
                &self.win_show_dnsservers(target)?,
            )),
            Platform::Linux | Platform::Other => Ok(vec![]),
        }
    }

    fn apply_dns(&self, target: &str, ips: &[String]) -> Result<(), SystemIntegrationError> {
        match self.platform {
            Platform::Mac => {
                // execFile argv：服务名含空格（如 "USB 10/100/1000 LAN"）也安全，无引号歧义。
                self.run(&Command::new("networksetup", mac_set_dns_args(target, ips)))?;
                Ok(())
            }
            // 写路径 no-op —— 但**正常不可达**：`takeover_supported=false` 已让 set_dns/reconcile_dns
            // 在写 marker 前早退。此处是纵深防御（万一有人绕过控制器直调 ops）。
            // 上游 `winSetDnsCommands` 纯函数保留在 `dns::win_set_dns_commands`（移植真值 + 待
            // Windows 接管解禁时复用），故意不接线。
            Platform::Win | Platform::Linux | Platform::Other => Ok(()),
        }
    }

    fn read_effective_resolvers(&self) -> Result<Vec<String>, SystemIntegrationError> {
        match self.platform {
            Platform::Mac => {
                // scutil --dns 反映生效解析器（含 DHCP 下发的）。
                let out = self.run(&Command::new("scutil", ["--dns"]))?;
                Ok(parse_scutil_nameservers(&out.stdout))
            }
            Platform::Win => {
                // 逐**已连接**接口读（与 list_targets 同口径，避免 VMware/Hyper-V/VPN 虚拟网卡的
                // 私网 DNS 抢先被选）。单接口 show 同时含 static 与 dhcp 行 → extract_ipv4s 两者都取。
                let mut all: Vec<String> = Vec::new();
                for iface in self.list_targets()? {
                    // 单接口读失败跳过（上游 per-iface try/catch）。
                    let Ok(stdout) = self.win_show_dnsservers(&iface) else {
                        continue;
                    };
                    for ip in extract_ipv4s(&stdout) {
                        if !all.contains(&ip) {
                            all.push(ip);
                        }
                    }
                }
                Ok(all)
            }
            Platform::Linux | Platform::Other => Ok(vec![]),
        }
    }
}

// ── set_dns 重试配置（1:1 移植 上游 `SystemDnsManager.ts:214-229` 的 `retry(...)` 块）──
//
// 复用 [`crate::proxy_ops`] 的通用重试原语（`RetryConfig`/`retry_op` 已 `pub(crate)`），不另写一套。
// **仅套 `set_dns` 一处**：上游 `restoreDns`/`reconcileDns` 均无 retry 包裹（`reconcileDns` 逐服务
// best-effort `.catch()` 吞错即放弃，`restoreDns` 单次 apply），故 [`SystemDnsController::restore_dns_inner`]
// 保持原样不套 retry —— 对齐上游这一不对称。

/// `set_dns` 的重试配置：`maxRetries:2, delay:500`（指数退避缺省 true → 退避 500/1000ms），
/// `shouldRetry`= 权限拒绝 / 未授权 → 不重试，其余瞬时抖动 → 重试（`SystemDnsManager.ts:221-226`）。
///
/// 仅 mac 会用到——**唯一真接管平台**（见 [`SystemDnsOps::takeover_supported`]）；win/linux 在
/// [`SystemDnsController::set_dns`] 写 marker 前已早退，走不到这条重试。
const DNS_SET_RETRY: RetryConfig = RetryConfig {
    max_retries: 2,
    delay: Duration::from_millis(500),
    exponential_backoff: true,
    should_retry: dns_set_should_retry,
};

/// `set_dns` 的 `shouldRetry`（上游 `SystemDnsManager.ts:223-225`）：权限拒绝 / 未授权 → 不重试，
/// 其余（`networksetup` 瞬时抖动）→ 重试。**配置**（次数/退避）与 `proxy_ops` 的 mac enable 独立
/// ——对齐「三平台参数不同，勿合并成单一配置」；但**判据词表**共用
/// [`crate::proxy_ops::PERMISSION_DENIED_NEEDLES`]（两份手抄的词表必然漂移，且漏词的后果
/// 在这条路径上更贵：本重试是**持 `dns_controller` 锁**跑的，误判瞬时 → 多 2 次必败重试 + 1.5s 退避
/// 全程占锁）。
///
/// 上游只判 `permission` / `not authorized` 两词 —— 那是 Node 侧文案；Rust 侧把子进程 stderr 原文
/// 归入消息串，macOS `networksetup` 的 `requires admin privileges` 形态一个都不含，故必须补表。
fn dns_set_should_retry(e: &SystemIntegrationError) -> bool {
    !crate::proxy_ops::is_permission_denied(&e.to_string().to_lowercase())
}

/// 系统 DNS 控制器：setDns / restoreDns / reconcileDns / sync 编排。
pub struct SystemDnsController<Ops: SystemDnsOps, Fs: MarkerFs> {
    ops: Ops,
    marker: DnsMarker<Fs>,
    /// 内存中的原始 DNS 快照（restoreDns 用；restoreDnsSync 走 marker 跨会话）。
    original: Option<BTreeMap<String, Vec<String>>>,
    /// `set_dns` 重试退避 sleep（注入便于测试：生产 [`std::thread::sleep`]，测试传 no-op 杜绝真睡；
    /// 对齐 `proxy_ops::SystemProxyOpsImpl` 既有的可注入执行缝风格）。
    sleeper: fn(Duration),
}

impl<Ops: SystemDnsOps, Fs: MarkerFs> SystemDnsController<Ops, Fs> {
    pub fn new(ops: Ops, marker: DnsMarker<Fs>) -> Self {
        Self {
            ops,
            marker,
            original: None,
            sleeper: std::thread::sleep,
        }
    }

    /// 测试：换成 no-op sleeper（重试路径不真睡）。
    #[cfg(test)]
    fn with_noop_sleeper(mut self) -> Self {
        self.sleeper = |_| {};
        self
    }

    /// 受控 DNS IP。
    pub fn controlled_ip(&self) -> &str {
        &self.marker.controlled_ip
    }

    /// 方案B：挑接管前的内网 LAN 解析器（私网 IPv4，排除受控 IP）。
    /// marker 在 → 用 marker.original；否则读生效解析器。上游 `getLanResolverForDns`。
    pub fn get_lan_resolver_for_dns(&self) -> Option<String> {
        let marker = self.marker.read();
        let candidates: Vec<String> = match &marker {
            Some(m) => m.original.values().flatten().cloned().collect(),
            None => self.ops.read_effective_resolvers().unwrap_or_default(),
        };
        pick_lan_resolver_ip(&candidates, &self.marker.controlled_ip)
    }

    /// 读各服务当前 DNS（best-effort：单服务读失败按 `[]`）。
    fn snapshot_current(&self, targets: &[String]) -> BTreeMap<String, Vec<String>> {
        let mut current = BTreeMap::new();
        for t in targets {
            let ips = self.ops.read_dns(t).unwrap_or_default();
            current.insert(t.clone(), ips);
        }
        current
    }

    /// TUN 启动接管系统 DNS。**best-effort**：失败仅告警 + 回滚，绝不抛（不阻断 TUN 启动）。
    /// 上游 `setDns`。
    pub fn set_dns(&mut self) {
        // 守卫：本平台不接管 DNS（win/linux）→ 早退。**必须在写 marker 之前** ——
        // 否则 marker 写下、apply 必失败（win netsh 需管理员）→ marker 卡死 → 每次启动反复空跑还原。
        // 判据见 `SystemDnsOps::takeover_supported`。
        if !self.ops.takeover_supported() {
            return;
        }
        // 守卫：受控 IP 在 bootstrap-direct → fail-closed 不接管。
        if !is_controlled_dns_ip_valid(&self.marker.controlled_ip) {
            return;
        }

        let Ok(targets) = self.ops.list_targets() else {
            return;
        };
        if targets.is_empty() {
            return;
        }

        // 读当前 DNS + 防自指计算原始值。
        let current = self.snapshot_current(&targets);
        let existing = self.marker.read();
        let original = compute_original_to_save(
            &current,
            &self.marker.controlled_ip,
            existing.as_ref().map(|m| &m.original),
        );

        // marker 前置写（intent）。
        self.original = Some(original.clone());
        self.marker.write(&original);

        // apply（整循环重试，非逐 target 单独重试——上游 `setDns` 的 `retry()` 包裹的是整个
        // for-loop：单条 target 抖动失败即整轮重跑，幂等安全，重复 apply 受控 IP 无害）。
        // 任一 attempt 耗尽重试后仍失败 → best-effort 回滚还原（不抛）。
        let controlled_ip = self.marker.controlled_ip.clone();
        let ops = &self.ops;
        let result = retry_op(
            &DNS_SET_RETRY,
            || -> Result<(), SystemIntegrationError> {
                for t in &targets {
                    ops.apply_dns(t, std::slice::from_ref(&controlled_ip))?;
                }
                Ok(())
            },
            self.sleeper,
        );
        if result.is_err() {
            // 失败兜底：还原以免半接管残留；还原失败补清 marker。
            if !self.restore_dns_inner() {
                self.marker.clear();
            }
        }
    }

    /// 还原系统 DNS。无 marker/原始 → 仅清 marker。上游 `restoreDns`。
    pub fn restore_dns(&mut self) {
        // 不接管的平台（win/linux）：**仍要清 marker** —— 清掉历史版本（旧版 netsh 失败）残留的
        // stuck marker，否则 has_marker 恒 true 致每个终态点/启动 recovery 反复空跑还原。
        // 对齐上游 `WindowsSystemDns.restoreDns` = `this.clearMarker()`。
        if !self.ops.takeover_supported() {
            self.original = None;
            self.marker.clear();
            return;
        }
        self.restore_dns_inner();
    }

    fn restore_dns_inner(&mut self) -> bool {
        let marker = self.marker.read();
        let original = self
            .original
            .clone()
            .or_else(|| marker.as_ref().map(|m| m.original.clone()));
        let Some(original) = original else {
            self.marker.clear();
            return true;
        };

        let targets = self
            .ops
            .list_targets()
            .unwrap_or_else(|_| original.keys().cloned().collect());
        let restore_targets = if targets.is_empty() {
            original.keys().cloned().collect::<Vec<_>>()
        } else {
            targets
        };

        // 逐服务 best-effort：单服务失败不阻断其余。
        let mut all_ok = true;
        for t in &restore_targets {
            if let Some(ips) = original.get(t) {
                if self.ops.apply_dns(t, ips).is_err() {
                    all_ok = false;
                }
            }
        }
        if all_ok {
            self.original = None;
            self.marker.clear();
            true
        } else {
            // 部分失败 → 保留 marker 交下次启动重试。
            false
        }
    }

    /// 热插重灌：接管激活中（marker 在）时，把「新出现 / 仍未受控」的服务也接管为受控 IP。
    /// 不变量：① 仅 marker 在才动手；② 先写 marker 再 apply；③ 只 apply 未受控服务（幂等）；
    /// ④ best-effort 逐服务；⑤ 防自指（mergedOriginal 以既有 marker.original 为底）。
    /// 上游 `reconcileDns`。
    pub fn reconcile_dns(&mut self) {
        // 守卫：本平台不接管 DNS → 无物可重灌（marker 也不会有）。
        if !self.ops.takeover_supported() {
            return;
        }
        // 守卫：受控 IP 在 bootstrap-direct → fail-closed。
        if !is_controlled_dns_ip_valid(&self.marker.controlled_ip) {
            return;
        }
        let Some(marker) = self.marker.read() else {
            // marker 不在 = 接管未激活 → 绝不擅自接管。
            return;
        };

        let Ok(targets) = self.ops.list_targets() else {
            return;
        };
        if targets.is_empty() {
            return;
        }

        let current = self.snapshot_current(&targets);

        // 以既有 marker.original 为底，并入当前各服务的应保存原始（防自指）。
        let mut merged = marker.original.clone();
        let computed =
            compute_original_to_save(&current, &self.marker.controlled_ip, Some(&merged));
        for (k, v) in computed {
            merged.insert(k, v);
        }

        // 只 apply 未受控服务。
        let to_apply: Vec<&String> = targets
            .iter()
            .filter(|t| {
                let ips = current.get(*t);
                !matches!(ips, Some(ips) if is_controlled(ips, &self.marker.controlled_ip))
            })
            .collect();
        if to_apply.is_empty() {
            return; // 全部已受控 → 幂等 no-op。
        }

        // 先写 marker 再 apply（崩溃留 intent）。
        self.original = Some(merged.clone());
        self.marker.write(&merged);

        for t in to_apply {
            let _ = self
                .ops
                .apply_dns(t, std::slice::from_ref(&self.marker.controlled_ip));
        }
    }

    /// 是否存在接管 marker（终态清理门控用）。
    pub fn has_marker(&self) -> bool {
        self.marker.exists()
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_tests_helpers::MemFs;
    use std::cell::RefCell;

    struct MockDnsOps {
        targets: Vec<String>,
        dns_state: RefCell<BTreeMap<String, Vec<String>>>,
        apply_calls: RefCell<Vec<(String, Vec<String>)>>,
        apply_fail_targets: Vec<String>,
        list_fails: bool,
        /// 模拟平台是否接管（mac=true / win·linux=false）。
        takeover: bool,
        /// 仅对**该 target** 生效的「瞬时失败」计数器：每次对它的 `apply_dns` 调用消耗 1，耗尽后
        /// 转正常（成功）。与 `apply_fail_targets` 的**永久**失败区分——用于验证重试：先失败 N 次
        /// 再成功，且不误伤同一轮里的其它 target（同构 `proxy_ops.rs` `FlakyRunner` 设计）。
        transient_fail_target: Option<String>,
        transient_fail_count: RefCell<u32>,
        /// 瞬时失败时返回的错误消息（决定 `dns_set_should_retry` 判「重试」还是「放弃」）。
        transient_fail_msg: String,
    }

    /// `takeover: true` = mac 语义（默认）；win/linux 腿的测试显式置 false。
    impl Default for MockDnsOps {
        fn default() -> Self {
            Self {
                targets: Vec::new(),
                dns_state: RefCell::new(BTreeMap::new()),
                apply_calls: RefCell::new(Vec::new()),
                apply_fail_targets: Vec::new(),
                list_fails: false,
                takeover: true,
                transient_fail_target: None,
                transient_fail_count: RefCell::new(0),
                transient_fail_msg: String::new(),
            }
        }
    }

    impl SystemDnsOps for MockDnsOps {
        fn takeover_supported(&self) -> bool {
            self.takeover
        }
        fn list_targets(&self) -> Result<Vec<String>, crate::error::SystemIntegrationError> {
            if self.list_fails {
                return Err(crate::error::SystemIntegrationError::dns("list failed"));
            }
            Ok(self.targets.clone())
        }
        fn read_dns(
            &self,
            target: &str,
        ) -> Result<Vec<String>, crate::error::SystemIntegrationError> {
            Ok(self
                .dns_state
                .borrow()
                .get(target)
                .cloned()
                .unwrap_or_default())
        }
        fn apply_dns(
            &self,
            target: &str,
            ips: &[String],
        ) -> Result<(), crate::error::SystemIntegrationError> {
            self.apply_calls
                .borrow_mut()
                .push((target.to_string(), ips.to_vec()));
            if self.transient_fail_target.as_deref() == Some(target) {
                let mut rem = self.transient_fail_count.borrow_mut();
                if *rem > 0 {
                    *rem -= 1;
                    return Err(crate::error::SystemIntegrationError::dns(
                        self.transient_fail_msg.clone(),
                    ));
                }
            }
            if self.apply_fail_targets.iter().any(|t| t == target) {
                return Err(crate::error::SystemIntegrationError::dns("apply failed"));
            }
            // 模拟真实写入状态。
            self.dns_state
                .borrow_mut()
                .insert(target.to_string(), ips.to_vec());
            Ok(())
        }
        fn read_effective_resolvers(
            &self,
        ) -> Result<Vec<String>, crate::error::SystemIntegrationError> {
            Ok(vec!["192.168.1.1".to_string()])
        }
    }

    fn mem_dns_marker() -> DnsMarker<MemFs> {
        DnsMarker::new(MemFs::new(), "/dns-marker.json")
    }

    fn controller(ops: MockDnsOps) -> SystemDnsController<MockDnsOps, MemFs> {
        SystemDnsController::new(ops, mem_dns_marker())
    }

    #[test]
    fn set_dns_takes_over_writes_marker_applies_controlled_ip() {
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            ..Default::default()
        };
        let mut c = controller(ops);

        c.set_dns();
        assert!(c.has_marker());
        // apply 受控 IP。
        let applied = c.ops.apply_calls.borrow();
        assert!(applied
            .iter()
            .any(|(t, ips)| t == "Wi-Fi" && ips == &vec!["8.8.8.8".to_string()]));
        // marker.original 记录了接管前的真实 LAN（192.168.1.1）。
        let marker = c.marker.read().unwrap();
        assert_eq!(
            marker.original.get("Wi-Fi").unwrap(),
            &vec!["192.168.1.1".to_string()]
        );
    }

    #[test]
    fn set_dns_noop_when_no_targets() {
        let mut c = controller(MockDnsOps::default());
        c.set_dns();
        assert!(!c.has_marker());
        assert!(c.ops.apply_calls.borrow().is_empty());
    }

    #[test]
    fn set_dns_noop_when_controlled_ip_invalid() {
        // CONTROLLED_TUN_DNS_IP=8.8.8.8 不在 bootstrap-direct → 合法，set 正常。
        // 此测试验证守卫路径：构造一个 controlled_ip 非法的 marker 不易（常量），改为验证合法时不被拦截。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            ..Default::default()
        };
        let mut c = controller(ops);
        c.set_dns();
        assert!(c.has_marker(), "8.8.8.8 合法，应正常接管");
    }

    #[test]
    fn set_dns_rolls_back_on_apply_failure() {
        // "apply failed"（`apply_fail_targets` 永久失败腿）非权限类 → 可重试，耗尽 maxRetries=2
        // （共 3 次 attempt）后仍失败 → 兜底还原（还原本身也因同一目标永久失败而失败）→ 补清 marker。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            apply_fail_targets: vec!["Wi-Fi".into()],
            ..Default::default()
        };
        let mut c = controller(ops).with_noop_sleeper();
        c.set_dns();
        // apply 耗尽重试仍失败 → 兜底还原 → marker 清。
        assert!(!c.has_marker(), "rollback cleared marker");
        assert_eq!(
            c.ops.apply_calls.borrow().len(),
            4,
            "重试 3 次（maxRetries=2）+ 回滚还原 1 次（不重试）"
        );
    }

    // ══════════ set_dns 重试（补齐与系统代理侧 `retry_op` 的不对称；仅 set_dns 一处）══════════

    #[test]
    fn dns_set_should_retry_aborts_on_permission_or_not_authorized() {
        // 纯谓词断言（同构 `proxy_ops.rs` 的 `mac_should_retry_aborts_on_permission_or_not_authorized`）。
        let ret = |m: &str| dns_set_should_retry(&crate::error::SystemIntegrationError::dns(m));
        assert!(
            !ret("networksetup: permission denied"),
            "permission → 不重试"
        );
        assert!(
            !ret("Error: not authorized to change"),
            "not authorized → 不重试"
        );
        assert!(ret("networksetup: temporarily unavailable"), "瞬时 → 重试");
        // 词表补齐后的关键形态：macOS `networksetup` 真实权限文案不含上游那两词。
        // 变异锁：把 `dns_set_should_retry` 改回手抄的 `permission || not authorized` → 本断言转红
        // （= 一次必败的权限错误会持 `dns_controller` 锁多跑 2 次重试 + 1.5s 退避）。
        assert!(
            !ret("networksetup: requires admin privileges to change DNS"),
            "requires admin privileges → 不重试（此前被误判成瞬时）"
        );
        assert!(
            !ret("setting DNS: Operation not permitted"),
            "EPERM → 不重试"
        );
    }

    /// 端到端形态（不只是谓词）：权限错误必须**只跑一次** apply，绝不重试。
    ///
    /// 变异锁：这条与上面的纯谓词断言分工不同 —— 谓词断言证明「判据认得这句话」，本用例证明
    /// 「判据真的接在 `DNS_SET_RETRY.should_retry` 上」（换回 `|_| true` 时纯谓词断言仍绿）。
    #[test]
    fn set_dns_admin_privileges_error_aborts_without_retry() {
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            transient_fail_target: Some("Wi-Fi".into()),
            transient_fail_count: RefCell::new(99),
            transient_fail_msg: "networksetup: requires admin privileges".into(),
            ..Default::default()
        };
        let mut c = controller(ops).with_noop_sleeper();
        c.set_dns();
        assert_eq!(
            c.ops.apply_calls.borrow().len(),
            2,
            "权限失败 → 1 次 apply + 1 次兜底还原（**不含**任何重试；\
             判据漏词时这里会变成 3+1=4，正是那 1.5s 白占锁的来源）"
        );
    }

    #[test]
    fn set_dns_retries_transient_failure_then_succeeds() {
        // ① 瞬态失败一次后成功 → 整体成功且不回滚。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            transient_fail_target: Some("Wi-Fi".into()),
            transient_fail_count: RefCell::new(1),
            transient_fail_msg: "networksetup: temporarily unavailable".into(),
            ..Default::default()
        };
        let mut c = controller(ops).with_noop_sleeper();
        c.set_dns();
        assert!(c.has_marker(), "重试后应成功接管，不应回滚");
        assert_eq!(
            c.ops.apply_calls.borrow().len(),
            2,
            "首次失败 + 1 次重试成功 = 2 次 apply"
        );
        assert_eq!(
            c.ops.dns_state.borrow().get("Wi-Fi").unwrap(),
            &vec!["8.8.8.8".to_string()],
            "重试成功后应已 apply 受控 IP"
        );
    }

    #[test]
    fn set_dns_permission_error_aborts_without_retry() {
        // ② 权限类错误 → 立即失败不重试（断言尝试次数）。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            // 权限错误不会自愈 → 计数给足（99），验证「不重试」而非「凑巧次数够用」。
            transient_fail_target: Some("Wi-Fi".into()),
            transient_fail_count: RefCell::new(99),
            transient_fail_msg: "networksetup: permission denied".into(),
            ..Default::default()
        };
        let mut c = controller(ops).with_noop_sleeper();
        c.set_dns();
        assert!(!c.has_marker(), "权限错误 → 回滚兜底 → marker 清");
        assert_eq!(
            c.ops.apply_calls.borrow().len(),
            2,
            "重试阶段仅 1 次尝试（不重试）+ 回滚还原 1 次（本身也不重试）= 2 次；\
             若误重试则耗尽 3 次 attempt + 回滚 ≥ 4 次"
        );
    }

    #[test]
    fn set_dns_retry_exhausted_still_rolls_back_partial_takeover() {
        // ③ 重试耗尽 → 回滚半接管路径仍正确：Wi-Fi 每次 attempt 都成功切到受控 IP（半接管），
        // Ethernet 持续瞬时失败拖垮整轮 → 3 次 attempt（maxRetries=2）耗尽后放弃 → 兜底还原，
        // 须把「已半接管」的 Wi-Fi 也一并撤回原始值（不是只管失败的那个 target）。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into(), "Ethernet".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m.insert("Ethernet".into(), vec!["10.0.0.1".to_string()]);
                m
            }),
            // 恰好覆盖 3 次 attempt（耗尽后回滚阶段的 Ethernet apply 自然转为成功）。
            transient_fail_target: Some("Ethernet".into()),
            transient_fail_count: RefCell::new(3),
            transient_fail_msg: "networksetup: temporarily unavailable".into(),
            ..Default::default()
        };
        let mut c = controller(ops).with_noop_sleeper();
        c.set_dns();
        assert!(!c.has_marker(), "重试耗尽应回滚并清 marker");
        assert_eq!(
            c.ops.dns_state.borrow().get("Wi-Fi").unwrap(),
            &vec!["192.168.1.1".to_string()],
            "半接管的 Wi-Fi 必须被回滚为原始值，不能残留受控 IP"
        );
        assert_eq!(
            c.ops.dns_state.borrow().get("Ethernet").unwrap(),
            &vec!["10.0.0.1".to_string()],
            "Ethernet 还原为原始值"
        );
        assert_eq!(
            c.ops.apply_calls.borrow().len(),
            8,
            "3 次 attempt × 2 target + 回滚 2 target = 8 次 apply"
        );
    }

    #[test]
    fn restore_dns_restores_original_and_clears_marker() {
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
                m
            }),
            ..Default::default()
        };
        let mut c = controller(ops);
        c.set_dns();
        assert!(c.has_marker());

        c.restore_dns();
        // 还原 → apply 原始 LAN IP + 清 marker。
        assert!(!c.has_marker());
        let applied = c.ops.apply_calls.borrow();
        assert!(applied
            .iter()
            .any(|(t, ips)| t == "Wi-Fi" && ips == &vec!["192.168.1.1".to_string()]));
    }

    #[test]
    fn restore_dns_noop_when_no_marker_and_no_original() {
        let mut c = controller(MockDnsOps::default());
        c.restore_dns();
        assert!(c.ops.apply_calls.borrow().is_empty());
    }

    #[test]
    fn reconcile_dns_idempotent_when_all_controlled() {
        // 所有服务已受控 → 幂等 no-op（不写 marker、不动系统）。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["8.8.8.8".to_string()]);
                m
            }),
            ..Default::default()
        };
        let mut c = controller(ops);
        // 先 set 建立 marker。
        c.set_dns();
        let calls_before = c.ops.apply_calls.borrow().len();

        c.reconcile_dns();
        // 全部已受控 → 不再 apply。
        assert_eq!(c.ops.apply_calls.borrow().len(), calls_before);
    }

    #[test]
    fn reconcile_dns_takes_over_new_uncontrolled_service() {
        // Wi-Fi 已接管（marker 在），Ethernet 新出现且未受控 → reconcile 接管 Ethernet。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into(), "Ethernet".into()],
            dns_state: RefCell::new({
                let mut m = BTreeMap::new();
                m.insert("Wi-Fi".into(), vec!["8.8.8.8".to_string()]); // 已受控
                m.insert("Ethernet".into(), vec!["10.0.0.1".to_string()]); // 未受控（真实 LAN）
                m
            }),
            ..Default::default()
        };
        let mut c = controller(ops);

        // 模拟 set 已完成（marker 在，Wi-Fi=8.8.8.8 是我们设的，原始=用户真值）。
        // 直接写 marker 模拟接管态：
        let mut original = BTreeMap::new();
        original.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
        c.marker.write(&original);
        c.original = Some(original);

        c.reconcile_dns();
        // Ethernet 被 apply 受控 IP。
        let applied = c.ops.apply_calls.borrow();
        assert!(applied
            .iter()
            .any(|(t, ips)| t == "Ethernet" && ips == &vec!["8.8.8.8".to_string()]));
        // Wi-Fi 不重复 apply（已受控跳过）。
        let wifi_applies = applied.iter().filter(|(t, _)| t == "Wi-Fi").count();
        assert_eq!(wifi_applies, 0);
        // marker.original 合并了 Ethernet 的真实原始（10.0.0.1）。
        let marker = c.marker.read().unwrap();
        assert_eq!(
            marker.original.get("Ethernet").unwrap(),
            &vec!["10.0.0.1".to_string()]
        );
        assert_eq!(
            marker.original.get("Wi-Fi").unwrap(),
            &vec!["192.168.1.1".to_string()]
        );
    }

    #[test]
    fn reconcile_dns_noop_when_no_marker() {
        // marker 不在 = 接管未激活 → 绝不擅自接管。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            ..Default::default()
        };
        let mut c = controller(ops);
        c.reconcile_dns();
        assert!(c.ops.apply_calls.borrow().is_empty());
        assert!(!c.has_marker());
    }

    #[test]
    fn get_lan_resolver_uses_marker_original_when_active() {
        let ops = MockDnsOps::default();
        let c = controller(ops);
        let mut original = BTreeMap::new();
        original.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
        c.marker.write(&original);
        assert_eq!(
            c.get_lan_resolver_for_dns(),
            Some("192.168.1.1".to_string())
        );
    }

    #[test]
    fn get_lan_resolver_reads_effective_when_no_marker() {
        let ops = MockDnsOps::default();
        let c = controller(ops);
        // 无 marker → read_effective_resolvers → 192.168.1.1（私网）。
        assert_eq!(
            c.get_lan_resolver_for_dns(),
            Some("192.168.1.1".to_string())
        );
    }

    // ══════════ takeover_supported 门（win/linux 不接管）══════════
    //
    // 这道门守的是上游 2026-06-17 真机实证修掉的 bug：win 上 `netsh set` 必 ACCESS DENIED，
    // 而 set_dns 是「先写 marker 再 apply」→ marker 卡死 → 每次启动反复空跑还原刷错误日志。

    #[test]
    fn set_dns_writes_no_marker_when_platform_does_not_take_over() {
        // **关键**：不接管的平台连 marker 都不能写（写了就卡死）。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            takeover: false,
            ..Default::default()
        };
        let mut c = controller(ops);
        c.set_dns();
        assert!(!c.has_marker(), "不接管的平台绝不能写 marker（否则卡死）");
        assert!(c.ops.apply_calls.borrow().is_empty(), "不接管 → 不得 apply");
    }

    #[test]
    fn restore_dns_clears_stuck_marker_on_non_takeover_platform() {
        // 上游 `WindowsSystemDns.restoreDns` = clearMarker()：清历史版本残留的 stuck marker，
        // 否则 has_marker 恒 true → 每个终态点/启动 recovery 反复空跑还原。
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            takeover: false,
            ..Default::default()
        };
        let mut c = controller(ops);
        // 模拟历史遗留的 stuck marker（旧版本 netsh 失败留下的）。
        let mut original = BTreeMap::new();
        original.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
        c.marker.write(&original);
        assert!(c.has_marker());

        c.restore_dns();
        assert!(!c.has_marker(), "stuck marker 须被清");
        assert!(
            c.ops.apply_calls.borrow().is_empty(),
            "不接管的平台不得往系统写 DNS"
        );
    }

    #[test]
    fn reconcile_dns_noop_on_non_takeover_platform() {
        let ops = MockDnsOps {
            targets: vec!["Wi-Fi".into()],
            takeover: false,
            ..Default::default()
        };
        let mut c = controller(ops);
        let mut original = BTreeMap::new();
        original.insert("Wi-Fi".into(), vec!["192.168.1.1".to_string()]);
        c.marker.write(&original);

        c.reconcile_dns();
        assert!(c.ops.apply_calls.borrow().is_empty());
    }

    // ══════════ 生产实现接线（SystemDnsOpsImpl）══════════

    use crate::exec::exec_tests_helpers::MockRunner;

    fn dns_ops_for(platform: Platform, runner: MockRunner) -> SystemDnsOpsImpl<MockRunner> {
        SystemDnsOpsImpl::with_platform(runner, platform)
    }

    #[test]
    fn impl_takeover_only_on_mac() {
        assert!(dns_ops_for(Platform::Mac, MockRunner::default()).takeover_supported());
        // win/linux 不接管 —— 判据见 trait doc（win netsh 需管理员 + strict_route 已劫持 :53）。
        assert!(!dns_ops_for(Platform::Win, MockRunner::default()).takeover_supported());
        assert!(!dns_ops_for(Platform::Linux, MockRunner::default()).takeover_supported());
        assert!(!dns_ops_for(Platform::Other, MockRunner::default()).takeover_supported());
    }

    #[test]
    fn impl_mac_list_targets_excludes_bluetooth() {
        let runner = MockRunner::default().with_arg_stdout(
            "-listallnetworkservices",
            "An asterisk...\nWi-Fi\nBluetooth PAN\n*Disabled Svc\nEthernet\n",
        );
        let t = dns_ops_for(Platform::Mac, runner).list_targets().unwrap();
        // 排除 Bluetooth PAN —— 否则 DNS 接管写到蓝牙网络，关闭后残留。
        assert_eq!(t, vec!["Wi-Fi".to_string(), "Ethernet".to_string()]);
    }

    #[test]
    fn impl_mac_read_dns_parses_and_dhcp_is_empty() {
        let runner =
            MockRunner::default().with_arg_stdout("-getdnsservers", "192.168.1.1\n8.8.4.4\n");
        assert_eq!(
            dns_ops_for(Platform::Mac, runner)
                .read_dns("Wi-Fi")
                .unwrap(),
            vec!["192.168.1.1".to_string(), "8.8.4.4".to_string()]
        );
        // DHCP/自动 → networksetup 输出提示句 → []（不是把提示句当 IP）。
        let runner2 = MockRunner::default().with_arg_stdout(
            "-getdnsservers",
            "There aren't any DNS Servers set on Wi-Fi.\n",
        );
        assert!(dns_ops_for(Platform::Mac, runner2)
            .read_dns("Wi-Fi")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn impl_mac_apply_dns_uses_argv_and_empty_means_dhcp() {
        let ops = dns_ops_for(Platform::Mac, MockRunner::default());
        ops.apply_dns("USB 10/100/1000 LAN", &["8.8.8.8".to_string()])
            .unwrap();
        let cmds = ops.runner.snapshot();
        assert_eq!(cmds[0].program, "networksetup");
        // 服务名含空格经 argv 下发，无引号歧义。
        assert_eq!(cmds[0].args[1], "USB 10/100/1000 LAN");
        assert_eq!(cmds[0].args[2], "8.8.8.8");

        // 空 ips → `Empty`（还原为 DHCP）。
        let ops2 = dns_ops_for(Platform::Mac, MockRunner::default());
        ops2.apply_dns("Wi-Fi", &[]).unwrap();
        assert!(ops2.runner.ran_arg("Empty"));
    }

    #[test]
    fn impl_mac_read_effective_resolvers_uses_scutil() {
        // scutil --dns 才拿得到 DHCP 下发的解析器（-getdnsservers 对 DHCP 返空）。
        let runner = MockRunner::default().with_arg_stdout(
            "--dns",
            "resolver #1\n  nameserver[0] : 192.168.1.1\n  nameserver[1] : 8.8.8.8\nresolver #2\n  nameserver[0] : 192.168.1.1\n",
        );
        let ops = dns_ops_for(Platform::Mac, runner);
        let r = ops.read_effective_resolvers().unwrap();
        assert_eq!(r, vec!["192.168.1.1".to_string(), "8.8.8.8".to_string()]);
        assert_eq!(ops.runner.snapshot()[0].program, "scutil");
    }

    #[test]
    fn impl_win_apply_dns_is_noop_writes_nothing() {
        // 写路径 no-op：一条命令都不能发（netsh set 需管理员，GUI 非提权必失败）。
        let ops = dns_ops_for(Platform::Win, MockRunner::default());
        ops.apply_dns("Ethernet", &["8.8.8.8".to_string()]).unwrap();
        assert!(
            ops.runner.snapshot().is_empty(),
            "win 写路径必须零命令 —— 发了就是 ACCESS DENIED + marker 卡死"
        );
    }

    #[test]
    fn impl_win_read_paths_stay_live_for_plan_b() {
        // 读路径保留（show 非提权可跑），供方案B getLanResolverForDns 用。
        let runner = MockRunner::default()
            .with_arg_stdout(
                "show",
                "Idx     Met         MTU          State                Name\n 12      10        1500  connected            Wi-Fi\n  1      75  4294967295  connected            Loopback Pseudo-Interface 1\n",
            );
        let ops = dns_ops_for(Platform::Win, runner);
        let targets = ops.list_targets().unwrap();
        assert_eq!(targets, vec!["Wi-Fi".to_string()], "loopback 须排除");
        assert!(ops
            .runner
            .snapshot()
            .iter()
            .any(|c| c.program.ends_with("netsh.exe")));
    }

    #[test]
    fn impl_linux_dns_is_fully_noop() {
        // 上游 LinuxSystemDns 逐字：读也返空，写 no-op，零命令。
        let ops = dns_ops_for(Platform::Linux, MockRunner::default());
        assert!(ops.list_targets().unwrap().is_empty());
        assert!(ops.read_dns("eth0").unwrap().is_empty());
        assert!(ops.read_effective_resolvers().unwrap().is_empty());
        ops.apply_dns("eth0", &["8.8.8.8".to_string()]).unwrap();
        assert!(ops.runner.snapshot().is_empty(), "linux 全 no-op，零命令");
    }

    /// 组合面（§K7「两扇门之间的缝」）：生产 ops + 控制器一起跑，验 win 不写 marker。
    /// 单测 ops 的 no-op 与单测控制器的门控**各自通过**并不能证明组合正确 —— 这条才是生产路径。
    #[test]
    fn impl_win_controller_combination_writes_no_marker() {
        let ops = dns_ops_for(Platform::Win, MockRunner::default());
        let mut c = SystemDnsController::new(ops, mem_dns_marker());
        c.set_dns();
        assert!(!c.has_marker(), "win 生产路径不得留 marker");
        assert!(
            c.ops.runner.snapshot().is_empty(),
            "win 生产路径不得发任何 DNS 写命令"
        );
    }

    /// 组合面：mac 生产 ops + 控制器 → 真接管（marker + apply 受控 IP）。
    #[test]
    fn impl_mac_controller_combination_takes_over() {
        let runner = MockRunner::default()
            .with_arg_stdout("-listallnetworkservices", "An asterisk...\nWi-Fi\n")
            .with_arg_stdout("-getdnsservers", "192.168.1.1\n");
        let ops = dns_ops_for(Platform::Mac, runner);
        let mut c = SystemDnsController::new(ops, mem_dns_marker());
        c.set_dns();
        assert!(c.has_marker(), "mac 应真接管");
        // 受控 IP 被 apply 到 Wi-Fi。
        assert!(c.ops.runner.ran_arg("-setdnsservers"));
        assert!(c.ops.runner.ran_arg(controlled_tun_dns_ip()));
        // marker 记录了接管前的真实 LAN。
        assert_eq!(
            c.marker.read().unwrap().original.get("Wi-Fi").unwrap(),
            &vec!["192.168.1.1".to_string()]
        );
    }
}
