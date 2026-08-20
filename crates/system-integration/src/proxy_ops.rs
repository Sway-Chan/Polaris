//! 系统代理平台操作抽象 + 接管/释放状态机。
//!
//! 1:1 移植自 上游 `SystemProxyManager.ts` 三平台实现：
//! - [`SystemProxyOps`] trait：读状态 / 设代理 / 清代理 / 恢复原始 / 同步清（紧急退出）。
//! - 三平台实现 cfg-gated（Linux 本机编译，mac/win 真实系统调用 cfg-gated）。
//! - 命令构造（argv / registry 行 / gsettings 元组）抽为纯函数，跨平台可单测（`#[cfg(test)]` mock ops）。
//! - [`SystemProxyController`]：编排 enable/disable + marker 前置写/成功清 + 防自指 + 失败兜底回滚 +
//!   **维度7 #8 marker 崩溃恢复**（`recover_from_marker`）。
//!
//! ## 状态机（对齐 Polaris）
//!
//! ```text
//! enable:  writeMarker(intent) → saveOriginal(stripSelf) → ops.set → 成功 / 失败兜底 disable
//! disable: ops.restore(original) | ops.clear → 成功 clearMarker
//! 启动:    recover_from_marker → 若 marker 在 → ops.clear（防死端口断网）→ clearMarker
//! ```

#![forbid(unsafe_code)]

use crate::bypass::format_bypass_for_windows;
use crate::error::SystemIntegrationError;
use crate::exec::CommandRunner;
use crate::proxy::{strip_self, MarkerFs, ProxyMarker, ProxyMarkerData, SystemProxyStatus};
use polaris_config_engine::user_config::system_proxy_bypass::{
    format_bypass_for_linux, format_bypass_for_mac,
};
use polaris_helper_proto::Platform;
use std::time::Duration;

/// 单条系统代理命令硬超时。上游用 `execFileAsync` 默认无超时，但挂起的 `networksetup`/`gsettings`
/// 会把同步的接管流程钉死 → 统一给 10s 上限（远宽于这些命令的正常耗时，仅防挂起）。
pub const PROXY_EXEC_TIMEOUT: Duration = Duration::from_secs(10);

/// Windows 旧版 QUIC 防火墙规则清理的独立预算。
///
/// 这条命令只是在代理启停时清扫旧版本可能遗留的 `Polaris_Block_QUIC`，不是系统代理成立条件；
/// 规则不存在或普通用户无权删除时都应 best-effort 让位。若与必要的注册表事务共用 10s 预算，
/// `netsh advfirewall` 在防火墙服务繁忙时会把已经成功的连接动作额外钉住十余秒
/// （Windows 真机 2026-08-20：15_254ms），用户看到的是“代理启动卡死”。750ms 足够健康本机命令
/// 完成，同时把可选清理的最坏墙钟锁在首次点击可接受的范围内；停止/恢复腿也复用同一预算。
const WINDOWS_QUIC_CLEANUP_TIMEOUT: Duration = Duration::from_millis(750);

// ── 平台命令构造（纯函数，跨平台可单测；对齐 Polaris 三平台 enable/disable argv）──

// `Command` 的单一真值在 `exec`（此前 proxy_ops::Command 与 dns_flush::FlushCommand 是两份逐字相同的
// `{program, args}` —— 典型假差异，已合并）。此处重导出保持既有路径可用。
pub use crate::exec::Command;

/// 代理设置请求。上游 `enableProxy(address, httpPort, socksPort, bypassList?)`。
#[derive(Debug, Clone)]
pub struct ProxyEnableRequest {
    pub address: String,
    pub http_port: u16,
    pub socks_port: u16,
    pub bypass_list: Vec<String>,
}

impl ProxyEnableRequest {
    pub fn our_host_port(&self) -> String {
        format!("{}:{}", self.address, self.http_port)
    }
}

// ── Windows 命令构造（Polaris WindowsSystemProxy）──

/// Windows Internet Settings 注册表路径。
pub const WIN_REG_PATH: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// Windows 设代理命令序列（reg add ProxyServer / ProxyEnable / ProxyOverride）。
/// 关键：只设 http/https，不设 socks=（Chromium 内核会把 WebSocket 经 SOCKS5 本地解析 DNS 被污染）。
///
/// QUIC 旧规则清理由 [`windows_clear_quic_command`] 单独构造并 best-effort 执行：规则本来就不存在时
/// `netsh delete rule` 也会以 exit=1 退出，不能把这个幂等成功态混进代理注册表事务。
pub fn windows_enable_commands(reg_exe: &str, req: &ProxyEnableRequest) -> Vec<Command> {
    let proxy_server = format!(
        "http={addr}:{http};https={addr}:{http}",
        addr = req.address,
        http = req.http_port
    );
    let proxy_override = format_bypass_for_windows(&req.bypass_list, None);
    vec![
        Command {
            program: reg_exe.to_string(),
            args: vec![
                "add".into(),
                WIN_REG_PATH.into(),
                "/v".into(),
                "ProxyServer".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                proxy_server,
                "/f".into(),
            ],
        },
        Command {
            program: reg_exe.to_string(),
            args: vec![
                "add".into(),
                WIN_REG_PATH.into(),
                "/v".into(),
                "ProxyEnable".into(),
                "/t".into(),
                "REG_DWORD".into(),
                "/d".into(),
                "1".into(),
                "/f".into(),
            ],
        },
        Command {
            program: reg_exe.to_string(),
            args: vec![
                "add".into(),
                WIN_REG_PATH.into(),
                "/v".into(),
                "ProxyOverride".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                proxy_override,
                "/f".into(),
            ],
        },
    ]
}

/// Windows 简单禁用（无原始可恢复时）：ProxyEnable=0。
/// 上游 `WindowsSystemProxy.disableProxy` else 分支。
pub fn windows_disable_commands(reg_exe: &str) -> Command {
    Command {
        program: reg_exe.to_string(),
        args: vec![
            "add".into(),
            WIN_REG_PATH.into(),
            "/v".into(),
            "ProxyEnable".into(),
            "/t".into(),
            "REG_DWORD".into(),
            "/d".into(),
            "0".into(),
            "/f".into(),
        ],
    }
}

/// Windows netsh 清 QUIC 规则（禁用时务必清，上游 `disableProxy` 首行）。
pub fn windows_clear_quic_command(netsh_exe: &str) -> Command {
    Command {
        program: netsh_exe.to_string(),
        args: vec![
            "advfirewall".into(),
            "firewall".into(),
            "delete".into(),
            "rule".into(),
            "name=Polaris_Block_QUIC".into(),
        ],
    }
}

/// Windows 恢复原始代理命令序列（回写 ProxyServer 串 + ProxyEnable=1）。
/// 上游 `WindowsSystemProxy.restoreProxySettings` 的 if 分支（enabled 且有实际代理）。
///
/// 调用前提：`original.enabled && original.has_any_proxy()`（否则该走 [`windows_disable_commands`]）。
pub fn windows_restore_commands(reg_exe: &str, original: &SystemProxyStatus) -> Vec<Command> {
    let mut parts = Vec::new();
    if let Some(p) = &original.http_proxy {
        parts.push(format!("http={p}"));
    }
    if let Some(p) = &original.https_proxy {
        parts.push(format!("https={p}"));
    }
    if let Some(p) = &original.socks_proxy {
        parts.push(format!("socks={p}"));
    }
    let mut cmds = Vec::new();
    if !parts.is_empty() {
        cmds.push(Command::new(
            reg_exe,
            [
                "add",
                WIN_REG_PATH,
                "/v",
                "ProxyServer",
                "/t",
                "REG_SZ",
                "/d",
                &parts.join(";"),
                "/f",
            ],
        ));
    }
    // 回写原始 ProxyServer 后再置 ProxyEnable=1（顺序对齐上游：先值后开关）。
    cmds.push(Command::new(
        reg_exe,
        [
            "add",
            WIN_REG_PATH,
            "/v",
            "ProxyEnable",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ],
    ));
    cmds
}

/// macOS 恢复单服务原始代理 argv 序列：设了的 → set+state on；没设的 → state off（对称撤销）。
/// 上游 `MacOSSystemProxy.restoreProxySettings` 的 `settings.enabled` 分支。
///
/// `host:port` 拆分复用 [`crate::proxy::split_host_port`]（与 Linux restorePlan 同一真值），
/// 拆不出（畸形原始值）→ 该协议按「未设」关掉，不把畸形值喂给 networksetup。
pub fn mac_service_restore_commands(service: &str, original: &SystemProxyStatus) -> Vec<Command> {
    // (读取子命令前缀, set 子命令, state 子命令)
    const SPEC: [(&str, &str); 3] = [
        ("-setwebproxy", "-setwebproxystate"),
        ("-setsecurewebproxy", "-setsecurewebproxystate"),
        ("-setsocksfirewallproxy", "-setsocksfirewallproxystate"),
    ];
    let values = [
        original.http_proxy.as_deref(),
        original.https_proxy.as_deref(),
        original.socks_proxy.as_deref(),
    ];

    let mut cmds = Vec::new();
    for ((set_sub, state_sub), value) in SPEC.iter().zip(values) {
        match crate::proxy::split_host_port(value) {
            Some(hp) => {
                cmds.push(Command::new(
                    "networksetup",
                    [set_sub, service, &hp.host, &hp.port.to_string()],
                ));
                cmds.push(Command::new("networksetup", [state_sub, service, "on"]));
            }
            None => {
                cmds.push(Command::new("networksetup", [state_sub, service, "off"]));
            }
        }
    }
    // bypass 还原：enable 时整表覆盖过，这里必须写回原值。
    //
    // `None` = 捕获阶段没读到（旧 marker / 读失败）⇒ **什么都不做**。把它折成「写 Empty」会在
    // 读失败时反过来清掉用户的清单 —— 那比不还原更糟。
    // `Some(vec![])` = 用户本来就没有条目 ⇒ 写 `Empty` 哨兵清空（不能什么都不传，参数不足会被拒）。
    if let Some(domains) = original.bypass_domains.as_ref() {
        let mut args = vec!["-setproxybypassdomains".to_owned(), service.to_owned()];
        if domains.is_empty() {
            args.push(MAC_BYPASS_EMPTY_SENTINEL.to_owned());
        } else {
            args.extend(domains.iter().cloned());
        }
        cmds.push(Command {
            program: "networksetup".into(),
            args,
        });
    }
    cmds
}

/// Windows 读代理状态命令：`reg query <path> /v <value>`。
/// 上游 `WindowsSystemProxy.getProxyStatus`（原为 shell execAsync 拼串，此处 argv 化）。
pub fn windows_query_command(reg_exe: &str, value: &str) -> Command {
    Command::new(reg_exe, ["query", WIN_REG_PATH, "/v", value])
}

/// 解析 `reg query ... /v ProxyEnable` 输出 → 是否启用（含 `0x1` 即启用）。
/// 上游 `getProxyStatus`：`enableResult.stdout.includes('0x1')`。
pub fn parse_win_proxy_enable(stdout: &str) -> bool {
    stdout.contains("0x1")
}

/// 解析 `reg query ... /v ProxyServer` 输出 → 三协议代理。
///
/// 值有**两种**合法形态，都必须认（漏认第二种 = 稳定误亮降级黄灯）：
///
/// 1. **逐协议**（我们自己 enable 时写的、也是 上游 唯一处理的形态）：
///    `http=127.0.0.1:8080;https=127.0.0.1:8080;socks=127.0.0.1:1080`；
/// 2. **裸 `host:port`**（**Windows 设置 UI「手动设置代理」输入框**写出来的形态，无 `=`）：
///    `127.0.0.1:7890` —— 语义是「**全协议**都用这个」（WinINET 对无 scheme 前缀的值即按 all 处理）。
///
/// 只认形态 1 的后果不是「少读一点信息」而是**判定反转**：用户在系统设置里手填了我们的
/// `127.0.0.1:<mixed>`（一个完全正常的用法），三条腿全解析成 `None` →
/// [`points_to_mixed_inbound`] 找不到任何「指向我们」的证据 → 判未生效 → 稳定误亮黄灯。
///
/// 裸形态只填 http/https 两腿、**不填 socks**：与我们自己 enable 的写法一致（Windows 侧从不设
/// `socks=`），且 `points_to_mixed_inbound` 把 `None` 腿视作「未设 ≠ 指向别处」，多填 socks 反而会
/// 在用户另设了 socks 时引入假象。
///
/// 取不到 ProxyServer 行 → `enabled:true` 但三协议全空（上游 `if (!proxyServerMatch) return { enabled: true }`）。
///
/// 注：上游用正则 `/ProxyServer\s+REG_SZ\s+(.+)/`；此处等价手写（本 crate 不引 regex 依赖）。
pub fn parse_win_proxy_server(stdout: &str) -> SystemProxyStatus {
    let mut status = SystemProxyStatus {
        enabled: true,
        ..Default::default()
    };
    // 找 `ProxyServer` + 空白 + `REG_SZ` + 空白 + 值。
    let Some(value) = stdout.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("ProxyServer")?;
        if !rest.starts_with(char::is_whitespace) {
            return None; // 防匹配到 ProxyServerFoo
        }
        let rest = rest.trim_start().strip_prefix("REG_SZ")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let v = rest.trim();
        (!v.is_empty()).then(|| v.to_string())
    }) else {
        return status;
    };

    // 形态 2：整串无 `=` → 裸 `host:port`，作用于全协议（见函数文档）。先判整串再拆分号：
    // `;` 分隔是形态 1 专有语法，裸形态里出现 `;` 本身就不合法，不必为它编个部分解析。
    if !value.contains('=') {
        let bare = value.trim();
        if !bare.is_empty() {
            status.http_proxy = Some(bare.to_string());
            status.https_proxy = Some(bare.to_string());
        }
        return status;
    }

    for part in value.split(';') {
        let Some((protocol, address)) = part.split_once('=') else {
            continue;
        };
        let (protocol, address) = (protocol.trim(), address.trim());
        if protocol.is_empty() || address.is_empty() {
            continue;
        }
        match protocol.to_lowercase().as_str() {
            "http" => status.http_proxy = Some(address.to_string()),
            "https" => status.https_proxy = Some(address.to_string()),
            "socks" => status.socks_proxy = Some(address.to_string()),
            _ => {}
        }
    }
    status
}

// ── macOS 命令构造（Polaris MacOSSystemProxy）──

/// macOS 列网络服务命令。
pub fn mac_list_services_command() -> Command {
    Command::new("networksetup", ["-listallnetworkservices"])
}

/// macOS 列「服务顺序 + 硬件端口 + BSD 设备名」命令（`-listallnetworkservices` **没有**设备名）。
///
/// 输出形如：
/// ```text
/// An asterisk (*) denotes that a network service is disabled.
/// (1) Wi-Fi
/// (Hardware Port: Wi-Fi, Device: en0)
///
/// (2) Thunderbolt Bridge
/// (Hardware Port: Thunderbolt Bridge, Device: bridge0)
/// ```
/// 供 [`parse_mac_service_order`] 建「设备名 → 服务名」映射，把默认路由的接口翻译回服务名。
pub fn mac_list_service_order_command() -> Command {
    Command::new("networksetup", ["-listnetworkserviceorder"])
}

/// macOS 查默认路由出接口命令（`route -n get default`）。
///
/// 为什么不用 reviewer 建议的 `scutil` 查 `State:/Network/Global/IPv4` 的 `PrimaryService`：
/// `scutil` 的 `show` 子命令**只接受 stdin 交互输入**，而本 crate 的执行缝
/// （[`crate::exec::CommandRunner`]）刻意只走 argv、`stdin(Stdio::null())`（杜绝 shell 插值）。
/// 要走 scutil 就得给执行缝加 stdin 通道或退回 `sh -c` 管道 —— 前者动的是全 crate 的唯一 OS 交互点，
/// 后者把好不容易关掉的 shell 插值面重新打开。`route + -listnetworkserviceorder` 是纯 argv 的等价问法，
/// 答的是同一件事：**此刻流量从哪个接口出去、那个接口属于哪个网络服务**。
///
/// 输出解析复用 [`crate::route_ops::parse_mac_route_get_interface`]（同一条 `interface:` 行，
/// 本 crate 已有唯一实现，不另写第二份）。
pub fn mac_default_route_command() -> Command {
    Command::new("route", ["-n", "get", "default"])
}

/// 解析 `networksetup -listnetworkserviceorder` 输出 → `(服务名, 设备名)` 有序对。
///
/// 只收「`(N) 服务名` 紧跟 `(Hardware Port: …, Device: dev)`」的成对行；`(*)`/`(N) *名` 标记的
/// **停用**服务直接丢弃（停用服务不承载流量）。缺设备名（如某些 VPN 服务）的条目一并丢弃 ——
/// 本函数唯一的用途就是按设备名反查，没设备名的条目对它无意义。
pub fn parse_mac_service_order(stdout: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        // `(1) Wi-Fi` / `(2) *Ethernet`（停用）。注意 `(Hardware Port: …)` 行也以 `(` 开头，
        // 靠「首段必须是纯数字序号」区分。
        let Some(rest) = line.strip_prefix('(') else {
            continue;
        };
        let Some((idx, name)) = rest.split_once(')') else {
            continue;
        };
        if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let name = name.trim();
        if name.starts_with('*') {
            continue; // 停用服务不承载流量
        }
        // 设备名在**下一行**：`(Hardware Port: Wi-Fi, Device: en0)`。
        let Some(dev_line) = lines.get(i + 1) else {
            continue;
        };
        let Some(dev) = dev_line
            .rsplit_once("Device:")
            .map(|(_, d)| d.trim_end_matches(')').trim())
            .filter(|d| !d.is_empty())
        else {
            continue;
        };
        if !name.is_empty() {
            out.push((name.to_string(), dev.to_string()));
        }
    }
    out
}

/// mac「哪些网络服务该被我们接管」的**唯一口径** —— DNS 接管与系统代理共用这一条。
///
/// # 为什么不能用 `-listallnetworkservices`
///
/// 那条命令只给名字，于是判据只能落在名字上（旧口径就是「跳 `*` 停用 + 跳含 Bluetooth 的」）。
/// 后果实测于 p101（2026-08-08，只读取证）：**7 个服务全被改写成 8.8.8.8，其中两个是别家 VPN 的**
/// —— `Tailscale`（`io.tailscale.ipn.macsys`）与 `Shadowrocket`（`com.liguangming.Shadowrocket`）。
/// 我们不但覆盖了它们的解析器，还把还原责任揽到自己的 marker 上：Polaris 崩溃即别家 VPN 的 DNS
/// 停在 8.8.8.8。系统代理侧同理（两处此前共用同一个名字口径）。
///
/// # 判据：有没有底层 BSD 设备名
///
/// `-listnetworkserviceorder` 多给一行 `(Hardware Port: …, Device: …)`。实测同一台机器：
/// 五个物理服务分别是 `en7` / `en9` / `en11` / `bridge0` / `en0`，而两个 VPN 服务的 **Device 为空**
/// （NetworkExtension 提供的服务没有 BSD 设备）。这是「这个服务**是什么**」的属性，
/// 不是它叫什么 —— 换个名字、换个语言、装个没见过的 VPN，判据都还成立；名字黑名单做不到。
///
/// 复用既有的 [`parse_mac_service_order`]（它本来就丢弃空 Device 的条目，doc 里点名「如某些 VPN 服务」），
/// 不新写第二份解析。
///
/// # 失败方向与回落
///
/// 漏掉一个**物理**服务 = 该网卡的 DNS 没被接管 = 泄漏（重）；多接管一个虚拟服务 = 本次要修的问题（轻）。
/// 物理服务恒有 Device，故新判据不会误跳过物理口。但为防「某机型/未来 macOS 输出形态变了导致全空」，
/// **过滤后为空时回落到旧口径并告警** —— 「一个都不接管」比「多接管两个」错得更离谱。
pub fn mac_list_manageable_services<F>(mut run: F) -> Result<Vec<String>, SystemIntegrationError>
where
    F: FnMut(&Command) -> Result<crate::exec::CommandOutput, SystemIntegrationError>,
{
    let order = run(&mac_list_service_order_command())?;
    let picked: Vec<String> = parse_mac_service_order(&order.stdout)
        .into_iter()
        // 蓝牙沿用旧口径排除：写进蓝牙网络的设置在关闭后可能残留（该理由与设备名无关，故按名字排）。
        .filter(|(name, _dev)| !name.contains("Bluetooth"))
        .map(|(name, _dev)| name)
        .collect();
    if !picked.is_empty() {
        return Ok(picked);
    }
    log::warn!(
        "networksetup -listnetworkserviceorder 未解析出任何带设备名的服务 —— \
         回落到 -listallnetworkservices 旧口径（会把无底层设备的虚拟服务一并纳入）"
    );
    let all = run(&mac_list_services_command())?;
    Ok(parse_mac_network_services(&all.stdout))
}

/// macOS 读单服务某协议代理命令（`sub` ∈ `-getwebproxy` / `-getsecurewebproxy` / `-getsocksfirewallproxy`）。
/// 上游 `MacOSSystemProxy.readServiceProxy`。
pub fn mac_read_proxy_command(sub: &str, service: &str) -> Command {
    Command::new("networksetup", [sub, service])
}

/// macOS 三协议读取子命令（顺序 = http / https / socks，与 [`SystemProxyStatus`] 字段对应）。
pub const MAC_PROXY_READ_SUBS: [&str; 3] = [
    "-getwebproxy",
    "-getsecurewebproxy",
    "-getsocksfirewallproxy",
];

/// macOS bypass 清单读取子命令（`-setproxybypassdomains` 的对偶）。
pub const MAC_BYPASS_READ_SUB: &str = "-getproxybypassdomains";

/// `networksetup` 用来表示「清空 bypass 清单」的哨兵实参。
///
/// 写空清单不能什么都不传（`-setproxybypassdomains <svc>` 参数不足会被拒），必须显式给 `Empty`。
pub const MAC_BYPASS_EMPTY_SENTINEL: &str = "Empty";

/// 解析 `networksetup -getproxybypassdomains <svc>` 输出 → 清单。
///
/// 输出形态：每行一个条目；一条都没有时是一句英文提示
/// （`There aren't any bypass domains set on <svc>.`）。
///
/// **提示句必须与真条目区分开**：它没有前导空白、含空格、且不是合法域名/CIDR。判据取
/// 「整行不含空白字符」—— bypass 条目（域名 / `*.suffix` / CIDR）本身不可能含空格，
/// 而任何英文提示句必然含空格。比匹配英文原文稳（`networksetup` 的提示文案随系统版本变）。
#[must_use]
pub fn parse_mac_bypass_domains(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.chars().any(char::is_whitespace))
        .map(str::to_owned)
        .collect()
}

/// 解析 `networksetup -getwebproxy <svc>` 输出 → `host:port`（未启用 → None）。
/// 上游 `MacOSSystemProxy.readServiceProxy` 的 `read` 闭包。
pub fn parse_mac_service_proxy(stdout: &str) -> Option<String> {
    if !stdout.contains("Enabled: Yes") {
        return None;
    }
    let field = |key: &str| -> Option<String> {
        stdout.lines().find_map(|l| {
            let v = l.trim().strip_prefix(key)?.trim();
            (!v.is_empty()).then(|| v.to_string())
        })
    };
    let server = field("Server:")?;
    let port = field("Port:")?;
    // 端口须为纯数字（上游正则 `/Port: (\d+)/`）。
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{server}:{port}"))
}

// ── Linux 读取命令构造 + gsettings 输出解析（Polaris LinuxSystemProxy.getProxyStatus）──

/// Linux 读 gsettings 某 schema 某 key。
pub fn linux_gsettings_get_command(schema: &str, key: &str) -> Command {
    Command::new(
        "gsettings",
        ["get", &format!("org.gnome.system.proxy.{schema}"), key],
    )
}

/// 解析 `gsettings get ...proxy.<schema> host` 输出 → host（空 → None）。
/// gsettings 字符串带单引号 → 剥引号。Polaris：`.replace(/'/g, '').trim()`。
pub fn parse_gsettings_host(stdout: &str) -> Option<String> {
    let host = stdout.replace('\'', "");
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// 解析 `gsettings get ...proxy.<schema> port` 输出 → 端口串。
///
/// **必须剥 GVariant 前缀**：gsettings 对 guint 返回形如 `uint32 8080`。上游注释明写此坑 ——
/// 不剥则 `splitHostPort` 的 parseInt 恒 NaN → **恢复分支永不触发**（假绿测试绕过）。
pub fn parse_gsettings_port(stdout: &str) -> String {
    let s = stdout.trim();
    // 剥 `uint32 ` / `uint16 ` 等前缀（上游正则 `/^uint\d+\s+/i`）。
    let stripped = s
        .strip_prefix("uint")
        .and_then(|rest| {
            let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
            if digits_end == 0 {
                return None; // `uint` 后无数字 → 非该前缀
            }
            let after = &rest[digits_end..];
            after.starts_with(char::is_whitespace).then(|| after.trim())
        })
        .unwrap_or(s);
    stripped.to_string()
}

/// Linux 读全局代理 `mode`（`gsettings get org.gnome.system.proxy mode`）。
///
/// **只有活态查询需要它**：`get_proxy_status`（残留检测）刻意不读 mode —— 它问的是「系统里还有没有
/// 我们留下的 host/port 值」，即便 mode 已被改成 none，那些残值也该被清。活态查询问的是**此刻流量
/// 会不会走代理**，mode≠manual 时 GNOME 根本不下发代理，host/port 留着也无效
/// （见 [`SystemProxyOpsImpl::read_active_proxy`] 的 Linux 腿）。
pub fn linux_gsettings_mode_get_command() -> Command {
    Command::new("gsettings", ["get", "org.gnome.system.proxy", "mode"])
}

/// 解析 `gsettings get org.gnome.system.proxy mode` 输出 → 模式串（`manual` / `none` / `auto`）。
/// gsettings 返回带单引号（`'manual'`）→ 剥引号 + trim（与 [`parse_gsettings_host`] 同口径，
/// 但本函数**不**把空串折成 None：空 mode 就是「读不出模式」，由调用方判为非 manual）。
pub fn parse_gsettings_mode(stdout: &str) -> String {
    stdout.replace('\'', "").trim().to_string()
}

/// macOS 单网络服务设代理 argv 序列：web/secureweb/socks 各 set + state on，外加 bypass。
/// 上游 `MacOSSystemProxy.enableProxy` per-service 块（execFile argv 参数化）。
pub fn mac_service_enable_commands(service: &str, req: &ProxyEnableRequest) -> Vec<Command> {
    let mut cmds = Vec::new();
    let http_port = req.http_port.to_string();
    let socks_port = req.socks_port.to_string();

    // HTTP 代理
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec![
            "-setwebproxy".into(),
            service.into(),
            req.address.clone(),
            http_port.clone(),
        ],
    });
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec!["-setwebproxystate".into(), service.into(), "on".into()],
    });
    // HTTPS 代理
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec![
            "-setsecurewebproxy".into(),
            service.into(),
            req.address.clone(),
            http_port.clone(),
        ],
    });
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec![
            "-setsecurewebproxystate".into(),
            service.into(),
            "on".into(),
        ],
    });
    // SOCKS 代理
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec![
            "-setsocksfirewallproxy".into(),
            service.into(),
            req.address.clone(),
            socks_port,
        ],
    });
    cmds.push(Command {
        program: "networksetup".into(),
        args: vec![
            "-setsocksfirewallproxystate".into(),
            service.into(),
            "on".into(),
        ],
    });
    // bypass（argv，原样接受 CIDR + 域名 + 通配）
    let mut bypass_args = vec!["-setproxybypassdomains".into(), service.into()];
    bypass_args.extend(format_bypass_for_mac(&req.bypass_list));
    cmds.push(Command {
        program: "networksetup".into(),
        args: bypass_args,
    });
    cmds
}

/// macOS 禁用单服务代理（三协议 state off）。
/// 上游 `MacOSSystemProxy.disableProxy` else 分支 per-service。
pub fn mac_service_disable_commands(service: &str) -> Vec<Command> {
    vec![
        Command {
            program: "networksetup".into(),
            args: vec!["-setwebproxystate".into(), service.into(), "off".into()],
        },
        Command {
            program: "networksetup".into(),
            args: vec![
                "-setsecurewebproxystate".into(),
                service.into(),
                "off".into(),
            ],
        },
        Command {
            program: "networksetup".into(),
            args: vec![
                "-setsocksfirewallproxystate".into(),
                service.into(),
                "off".into(),
            ],
        },
    ]
}

/// 解析 macOS `networksetup -listallnetworkservices` 输出 → 网络服务名列表。
/// 跳过首行提示 + 空行 + 以 `*` 开头的禁用服务 + Bluetooth PAN。
/// 上游 `MacOSSystemProxy.getNetworkServices`。
pub fn parse_mac_network_services(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip(1) // 首行提示 "An asterisk (*) denotes..."
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('*') && !l.contains("Bluetooth"))
        .map(str::to_string)
        .collect()
}

// ── Linux 命令构造（Polaris LinuxSystemProxy）──

/// Linux gsettings 设代理 argv 序列：mode manual + http/https/socks host/port + ignore-hosts。
/// 上游 `LinuxSystemProxy.enableProxy` retry 块（execFile argv 参数化，杜绝注入）。
pub fn linux_enable_commands(req: &ProxyEnableRequest) -> Vec<Command> {
    let http_port = req.http_port.to_string();
    let socks_port = req.socks_port.to_string();
    let hosts = format_bypass_for_linux(&req.bypass_list);
    // GVariant 字符串数组：['a', 'b']
    let quoted: Vec<String> = hosts.iter().map(|h| format!("'{h}'")).collect();
    let ignore_list = format!("[{}]", quoted.join(", "));

    vec![
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy".into(),
                "mode".into(),
                "'manual'".into(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.http".into(),
                "host".into(),
                req.address.clone(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.http".into(),
                "port".into(),
                http_port.clone(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.http".into(),
                "enabled".into(),
                "true".into(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.https".into(),
                "host".into(),
                req.address.clone(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.https".into(),
                "port".into(),
                http_port,
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.socks".into(),
                "host".into(),
                req.address.clone(),
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy.socks".into(),
                "port".into(),
                socks_port,
            ],
        },
        Command {
            program: "gsettings".into(),
            args: vec![
                "set".into(),
                "org.gnome.system.proxy".into(),
                "ignore-hosts".into(),
                ignore_list,
            ],
        },
    ]
}

/// Linux 简单禁用（无原始可恢复时）：mode none。
/// 上游 `LinuxSystemProxy.disableProxy` else 分支。
pub fn linux_disable_command() -> Command {
    Command {
        program: "gsettings".into(),
        args: vec![
            "set".into(),
            "org.gnome.system.proxy".into(),
            "mode".into(),
            "none".into(),
        ],
    }
}

/// Linux 恢复单 schema 的 argv 序列（set：host+port[+enabled]；clear：host=''[+enabled false]）。
/// 上游 `LinuxSystemProxy.restoreOriginalProxyAsync` / `disableProxySync` gset 块。
pub fn linux_restore_schema_commands(entry: &crate::proxy::RestorePlanEntry) -> Vec<Command> {
    let base = format!("org.gnome.system.proxy.{}", entry.schema);
    match &entry.hp {
        Some(hp) => {
            let mut cmds = vec![
                Command {
                    program: "gsettings".into(),
                    args: vec!["set".into(), base.clone(), "host".into(), hp.host.clone()],
                },
                Command {
                    program: "gsettings".into(),
                    args: vec![
                        "set".into(),
                        base.clone(),
                        "port".into(),
                        hp.port.to_string(),
                    ],
                },
            ];
            // 仅 http schema 有 enabled 键。
            if entry.schema == "http" {
                cmds.push(Command {
                    program: "gsettings".into(),
                    args: vec!["set".into(), base, "enabled".into(), "true".into()],
                });
            }
            cmds
        }
        None => {
            let mut cmds = vec![Command {
                program: "gsettings".into(),
                args: vec!["set".into(), base.clone(), "host".into(), String::new()],
            }];
            if entry.schema == "http" {
                cmds.push(Command {
                    program: "gsettings".into(),
                    args: vec!["set".into(), base, "enabled".into(), "false".into()],
                });
            }
            cmds
        }
    }
}

/// Linux 恢复前先置 mode manual（若有任一 schema 有值）。
pub fn linux_set_mode_manual_command() -> Command {
    Command {
        program: "gsettings".into(),
        args: vec![
            "set".into(),
            "org.gnome.system.proxy".into(),
            "mode".into(),
            "manual".into(),
        ],
    }
}

// ── 重试原语（1:1 移植 上游 `src/main/utils/retry.ts` + 三平台 enableProxy retry 块）──
//
// FX-proxy-ops-retry（审查表 row69）：上游三平台 `enableProxy` 都用 `retry(...)` 包裹「设代理命令序列」，
// 单条命令瞬时抖动（Win reg/netsh 占用、mac networksetup 竞态、gsettings 瞬时失败）不误判失败回滚。
// 此前 Polaris `set_proxy` 是无重试单次 `run_all` → 一次抖动即失败回滚。以下按上游**逐字**迁移退避 /
// 重试上限 / shouldRetry 谓词（三平台参数各异，勿合并成单一配置）。

/// 单次 enable 的重试配置。对齐上游 `RetryOptions`（省略 `onRetry` 日志钩子——纯观察、无行为影响，
/// 且本 crate 此层无 logger seam）。三平台参数不同：见 [`WIN_ENABLE_RETRY`] / [`MAC_ENABLE_RETRY`] /
/// [`LINUX_ENABLE_RETRY`]。
///
/// `pub(crate)`（含字段）：DNS 侧 `dns_ops::SystemDnsController::set_dns` 复用本原语与类型
/// （上游 `SystemDnsManager.setDns` 同样用 `retry()` 包裹 apply 循环，见该 mod 内 `DNS_SET_RETRY`）。
pub(crate) struct RetryConfig {
    /// 最大重试次数（**不含**首次尝试）。总执行 = `max_retries + 1`
    /// （逐字对齐上游 `for (attempt=0; attempt<=maxRetries; attempt++)`，`retry.ts:58`）。
    pub(crate) max_retries: u32,
    /// 基础退避延迟（上游三平台均 `delay: 500`）。
    pub(crate) delay: Duration,
    /// 指数退避（上游 `exponentialBackoff` 缺省 **true**，三处均未显式关闭）：第 n 次重试前 `sleep(delay * 2^n)`
    /// → 500ms、1000ms……（**非**固定 500ms；`retry.ts:50,72`）。
    pub(crate) exponential_backoff: bool,
    /// 可重试谓词：`false` = 立即放弃（权限拒绝 / 命令未找到 / 非瞬时错误）。对齐上游 `shouldRetry`。
    pub(crate) should_retry: fn(&SystemIntegrationError) -> bool,
}

/// Windows `enableProxy` retry 块（上游 `SystemProxyManager.ts:252-265`）：`maxRetries:2, delay:500`
/// （指数退避缺省 true → 退避 500/1000ms），`shouldRetry`= 权限拒绝 / 命令未找到 → 不重试，其余瞬时 → 重试。
const WIN_ENABLE_RETRY: RetryConfig = RetryConfig {
    max_retries: 2,
    delay: Duration::from_millis(500),
    exponential_backoff: true,
    should_retry: win_enable_should_retry,
};

/// macOS `enableProxy` retry 块（上游 `SystemProxyManager.ts:518-531`）：`maxRetries:2, delay:500`
/// （指数退避缺省 true → 退避 500/1000ms），`shouldRetry`= 权限 / 未授权 → 不重试。
const MAC_ENABLE_RETRY: RetryConfig = RetryConfig {
    max_retries: 2,
    delay: Duration::from_millis(500),
    exponential_backoff: true,
    should_retry: mac_enable_should_retry,
};

/// Linux `enableProxy` retry 块（上游 `SystemProxyManager.ts:764`）：`{ maxRetries: 1, delay: 500 }`
/// —— **未传 `shouldRetry` → 用上游 `defaultShouldRetry`**（仅瞬时网络类错误重试）。指数退避缺省 true。
const LINUX_ENABLE_RETRY: RetryConfig = RetryConfig {
    max_retries: 1,
    delay: Duration::from_millis(500),
    exponential_backoff: true,
    should_retry: default_should_retry,
};

/// 通用重试（1:1 移植上游 `retry()`，`retry.ts:43-89`）。循环语义逐字对齐 `for (attempt=0; attempt<=maxRetries; attempt++)`：
/// 失败后 `attempt >= max_retries` → 放弃并返回**最后一次**错误；`!should_retry` → 立即放弃；否则
/// `sleep(退避)` 后重试。`sleep` 注入便于测试（生产 [`std::thread::sleep`]，测试传 no-op / 记录器，
/// 杜绝真睡 —— 对齐 crate 既有「可注入执行缝」风格）。
///
/// `pub(crate)`：`dns_ops::SystemDnsController::set_dns` 复用（DNS apply 循环重试，见该处调用点）。
pub(crate) fn retry_op<T>(
    cfg: &RetryConfig,
    mut op: impl FnMut() -> Result<T, SystemIntegrationError>,
    mut sleep: impl FnMut(Duration),
) -> Result<T, SystemIntegrationError> {
    let mut attempt: u32 = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                // 已到上限（首次 + max_retries 次重试全败）→ 抛最后一次错误。
                if attempt >= cfg.max_retries {
                    return Err(e);
                }
                // 不可重试（权限/命令未找到/非瞬时）→ 立即放弃，不浪费退避。
                if !(cfg.should_retry)(&e) {
                    return Err(e);
                }
                let backoff = if cfg.exponential_backoff {
                    cfg.delay * 2u32.pow(attempt)
                } else {
                    cfg.delay
                };
                sleep(backoff);
                attempt += 1;
            }
        }
    }
}

/// 错误消息（小写）—— shouldRetry 谓词按上游对 `error.message.toLowerCase()` 的子串判定。
/// `Display` 形如 `system proxy error: <msg>`，前缀不含任何目标子串，判定不受污染。
fn err_message_lower(e: &SystemIntegrationError) -> String {
    e.to_string().to_lowercase()
}

/// 上游 `defaultShouldRetry`（`retry.ts:20-41`）：仅瞬时网络类错误重试。上游另判结构化
/// `error.code`（'ENOENT'/9009 等）；Rust 侧无结构化 code，执行缝把原因归入消息串 → 统一按消息子串判。
fn default_should_retry(e: &SystemIntegrationError) -> bool {
    const TEMPORARY_ERRORS: [&str; 9] = [
        "timeout",
        "timed out",
        "econnrefused",
        "econnreset",
        "etimedout",
        "enetunreach",
        "ehostunreach",
        "enotfound",
        "temporary failure",
    ];
    let msg = err_message_lower(e);
    TEMPORARY_ERRORS.iter().any(|p| msg.contains(p))
}

/// Windows enable 的 `shouldRetry`（上游 `SystemProxyManager.ts:255-264`）：权限拒绝 / 命令未找到 →
/// 不重试（重试无意义，直给对症诊断），其余瞬时错误 → 重试。
fn win_enable_should_retry(e: &SystemIntegrationError) -> bool {
    let msg = err_message_lower(e);
    if msg.contains("access denied") || is_permission_denied(&msg) {
        return false;
    }
    if is_command_not_found(&msg) {
        return false;
    }
    true
}

/// 「权限被拒」消息判据（**唯一词表**，mac enable / DNS set 共用）。
///
/// # 为什么词表必须比上游宽
///
/// 上游只判 `permission` / `not authorized` 两词，那是 Electron 侧 `execFileAsync` 抛出的 Node 错误
/// 文案；Rust 侧的执行缝（[`crate::exec::StdCommandRunner`]）把**子进程 stderr 原文**归入消息串 ——
/// 而 macOS `networksetup` 权限失败的常见原文是 **`requires admin privileges`** 形态，
/// 一个目标词都不含。漏判的代价不是「少一条日志」：`should_retry` 会把它当瞬时抖动，
/// **多跑 2 次必败重试 + 1.5s 指数退避**，而 DNS 那条重试是**持 `dns_controller` 锁**跑的
/// （见 `dns_ops::DNS_SET_RETRY`）—— 一次必败的权限错误会把锁多占 1.5 秒。
///
/// 词表按「消息里出现即可判定权限」筛，宁窄勿宽：多判一个词只会让某个**真瞬时**错误少重试 2 次
/// （代价有限、可观测）；少判一个词则是上面那条静默的锁占用。
///
/// TODO(真机采集)：macOS 各版本 `networksetup` / `scutil` 的权限失败原文尚未在真机逐条采样，
/// 本表是「已知形态 + EPERM/root 通用形态」的保守并集。真机采到新文案时补进本表（**只加不改**：
/// 现有词各自对应一种已知形态，删词等于把那种形态放回误重试路径）。
pub(crate) const PERMISSION_DENIED_NEEDLES: [&str; 7] = [
    "permission",               // "permission denied"（EACCES 通用文案 + 上游原判据）
    "not authorized",           // 上游原判据（Electron 侧文案）
    "not permitted",            // EPERM: "Operation not permitted"
    "requires admin",           // "requires admin privileges" / "requires administrator privileges"
    "administrator privileges", // 同上的另一种措辞（不含 "requires" 前缀时）
    "must be root",             // "You must be root to ..."
    "as root",                  // "You must be running as root to ..." / "run as root"
];

/// 消息（**已 lowercase**）是否命中 [`PERMISSION_DENIED_NEEDLES`]。
pub(crate) fn is_permission_denied(msg_lower: &str) -> bool {
    PERMISSION_DENIED_NEEDLES
        .iter()
        .any(|p| msg_lower.contains(p))
}

/// macOS enable 的 `shouldRetry`（上游 `SystemProxyManager.ts:521-526`）：权限 / 未授权 → 不重试。
/// 判据词表见 [`PERMISSION_DENIED_NEEDLES`]（与 DNS set 共用，两处口径不可分叉）。
fn mac_enable_should_retry(e: &SystemIntegrationError) -> bool {
    !is_permission_denied(&err_message_lower(e))
}

/// 命令未找到判定（移植上游 `win-system32.ts:isCommandNotFoundError` 的消息子串）。上游还判
/// `code==='ENOENT'||9009`；Polaris 侧 reg/netsh 已绝对路径化，缺失表现为 spawn 失败，经
/// [`crate::exec::StdCommandRunner`] 归一为「`<program> 启动失败: …`」→ 补该本地标记作 Rust 侧 ENOENT 等价。
fn is_command_not_found(msg_lower: &str) -> bool {
    const NEEDLES: [&str; 6] = [
        "不是内部或外部命令",   // cmd zh-CN: 'X' 不是内部或外部命令
        "is not recognized",    // cmd en: 'X' is not recognized
        "command not found",    // POSIX shell
        "系统找不到指定的路径", // cmd zh-CN: 绝对路径不存在
        "cannot find the path", // cmd en
        "启动失败",             // Rust spawn ENOENT（StdCommandRunner 措辞）
    ];
    NEEDLES.iter().any(|n| msg_lower.contains(n))
}

// ── 平台操作 trait（系统调用经此抽象；测试 mock）──

/// 平台系统代理操作抽象。三平台实现 cfg-gated；测试用 mock 实现。
/// 语义对齐 上游 `ISystemProxyManager`。
pub trait SystemProxyOps {
    /// 读当前代理状态（上游 `getProxyStatus`）。
    ///
    /// **口径：残留检测**——macOS 上扫**全部**网络服务（代理可能设在非首服务上）。
    /// 用于 `ensure_cleared` 门控 2 与 `detect_foreign_proxy`。**不要**拿它做 enable 前的原始快照
    /// 捕获，那条走 [`capture_original_status`](Self::capture_original_status)（口径不同，见其文档）。
    fn get_proxy_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError>;

    /// 读 **enable 前的原始代理快照**（disable 时回写的真值来源）。
    ///
    /// # 为什么与 [`get_proxy_status`](Self::get_proxy_status) 刻意分家
    ///
    /// 两者问的是**不同问题**，合用一个实现会让其中一个必然错：
    ///
    /// - `get_proxy_status` 问「**系统里还有没有**代理残留」→ 必须扫全部服务，漏一个就误判「无残留」。
    /// - 本方法问「**待会儿要往回写什么**」→ 只需 `restore_proxy` 的回写目标（macOS = `services[0]`）
    ///   的状态。扫全部服务在这里既慢（7 服务 × 3 协议 = 21 次 `networksetup` exec，实测 ~34ms/次
    ///   ≈ 0.7s 全压在启动关键路径上），又**不正确**：扫到的可能是**第二个**服务的代理，回写却落在
    ///   首个服务上 —— 把 B 的设置写进 A。
    ///
    /// 对齐 上游 `SystemProxyManager.ts:472`（原始快照只读首个服务）。
    ///
    /// # 与 `restore_proxy` 的成对不变式（**改一个必须改另一个**）
    ///
    /// 「捕获源」与「回写目标」必须是**同一个**服务，否则就是跨服务污染。当前约定：
    /// **macOS 两端都锚在 `services[0]`**。Win/Linux 无逐服务概念（注册表 / gsettings 全局），
    /// 故默认实现直接委托 `get_proxy_status`。
    fn capture_original_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
        self.get_proxy_status()
    }

    /// 列出应设代理的网络服务（macOS 用；Win/Linux 返回单元素占位）。
    /// Polaris macOS `getNetworkServices`。
    fn list_network_services(&self) -> Result<Vec<String>, SystemIntegrationError>;

    /// 设代理（apply 平台命令）。
    /// Polaris 三平台 enableProxy retry 块的实际执行。
    fn set_proxy(&self, req: &ProxyEnableRequest) -> Result<(), SystemIntegrationError>;

    /// 清/禁用代理（无原始可恢复时）。
    /// Polaris 三平台 disableProxy else 分支。
    fn clear_proxy(&self) -> Result<(), SystemIntegrationError>;

    /// 恢复原始代理设置。
    /// Polaris 三平台 disableProxy if 分支 / restoreProxySettings。
    fn restore_proxy(&self, original: &SystemProxyStatus) -> Result<(), SystemIntegrationError>;
}

// ── 生产实现（运行时 Platform 分派 + CommandRunner 下发；零 cfg）──

/// [`SystemProxyOps`] 的生产实现。
///
/// **平台分派靠运行时 [`Platform`] 枚举，不靠 `#[cfg]`** —— 三平台的命令构造/输出解析都是纯数据，
/// 全平台编译无害，换来 Linux CI 100% 跑测三平台逻辑（审计 §M1 第二形态）。构造时传 `platform`，
/// 生产用 [`Platform::current()`]，测试可任意指定 → 一台 Linux 上就能断言 mac/win 的 argv 与解析。
///
/// **本结构体只做「跑哪条命令 + 把输出交给纯函数解析」**，不含判定逻辑（判定全在上面的纯函数里）。
pub struct SystemProxyOpsImpl<R: CommandRunner> {
    runner: R,
    platform: Platform,
    /// Windows `reg.exe` 绝对路径（规避 PATH 缺 System32，见 [`crate::exec::system32`]）。
    reg_exe: String,
    /// Windows `netsh.exe` 绝对路径。
    netsh_exe: String,
    /// 重试退避 sleep（注入便于测试：生产 [`std::thread::sleep`]，测试传 no-op 杜绝真睡）。
    sleeper: fn(Duration),
}

impl<R: CommandRunner> SystemProxyOpsImpl<R> {
    /// 生产构造：平台取本机，Windows 二进制路径按本机 env 解析。
    pub fn new(runner: R) -> Self {
        Self::with_platform(runner, Platform::current())
    }

    /// 指定平台构造（测试用：Linux 上构造 Mac/Win ops 断言其 argv 与解析）。
    pub fn with_platform(runner: R, platform: Platform) -> Self {
        Self {
            runner,
            platform,
            reg_exe: crate::exec::system32_from_env("reg.exe"),
            netsh_exe: crate::exec::system32_from_env("netsh.exe"),
            sleeper: std::thread::sleep,
        }
    }

    /// 测试：换成 no-op sleeper（重试路径不真睡）。
    #[cfg(test)]
    fn with_noop_sleeper(mut self) -> Self {
        self.sleeper = |_| {};
        self
    }

    fn run(&self, cmd: &Command) -> Result<crate::exec::CommandOutput, SystemIntegrationError> {
        self.run_with_timeout(cmd, PROXY_EXEC_TIMEOUT)
    }

    /// 复用唯一命令执行缝，只为明确属于 best-effort 的动作收紧墙钟；必要事务仍走 [`Self::run`]。
    fn run_with_timeout(
        &self,
        cmd: &Command,
        timeout: Duration,
    ) -> Result<crate::exec::CommandOutput, SystemIntegrationError> {
        self.runner
            .run(cmd, timeout)
            .map_err(SystemIntegrationError::proxy)
    }

    /// 逐条跑；任一失败即返回（enable 的 argv 序列是整体，半套=坏状态）。
    fn run_all(&self, cmds: &[Command]) -> Result<(), SystemIntegrationError> {
        for c in cmds {
            self.run(c)?;
        }
        Ok(())
    }

    /// macOS：读单服务三协议代理。
    fn mac_read_service(&self, service: &str) -> SystemProxyStatus {
        let mut st = SystemProxyStatus::default();
        // best-effort 逐协议：单协议读失败按「未设」（上游 readServiceProxy 由外层 try/catch 兜）。
        let read = |sub: &str| -> Option<String> {
            let out = self.run(&mac_read_proxy_command(sub, service)).ok()?;
            parse_mac_service_proxy(&out.stdout)
        };
        st.http_proxy = read(MAC_PROXY_READ_SUBS[0]);
        st.https_proxy = read(MAC_PROXY_READ_SUBS[1]);
        st.socks_proxy = read(MAC_PROXY_READ_SUBS[2]);
        // bypass 清单：enable 会整表覆盖它，不在这里捕获就永远还不回去。
        // 读失败 → `None`（**没捕获过**），restore 据此不碰 bypass —— 绝不把读失败折成「本来就是空的」。
        st.bypass_domains = self
            .run(&mac_read_proxy_command(MAC_BYPASS_READ_SUB, service))
            .ok()
            .map(|out| parse_mac_bypass_domains(&out.stdout));
        st.enabled = st.has_any_proxy();
        st
    }

    /// Linux：读单 schema 的 `host:port`（host 空 → None，不把 http 扇出到未设的 https/socks）。
    fn linux_collect_schema(&self, schema: &str) -> Option<String> {
        let host_out = self
            .run(&linux_gsettings_get_command(schema, "host"))
            .ok()?;
        let host = parse_gsettings_host(&host_out.stdout)?;
        let port_out = self
            .run(&linux_gsettings_get_command(schema, "port"))
            .ok()?;
        let port = parse_gsettings_port(&port_out.stdout);
        Some(format!("{host}:{port}"))
    }
}

impl<R: CommandRunner> SystemProxyOps for SystemProxyOpsImpl<R> {
    fn get_proxy_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
        match self.platform {
            Platform::Win => {
                // ProxyEnable 未启用 → 直接 disabled（上游 getProxyStatus 早退）。
                let Ok(enable_out) = self.run(&windows_query_command(&self.reg_exe, "ProxyEnable"))
                else {
                    return Ok(SystemProxyStatus::default()); // 上游 catch → { enabled: false }
                };
                if !parse_win_proxy_enable(&enable_out.stdout) {
                    return Ok(SystemProxyStatus::default());
                }
                let Ok(server_out) = self.run(&windows_query_command(&self.reg_exe, "ProxyServer"))
                else {
                    // 上游：ProxyServer 读不到但 enabled=true → { enabled: true }（无协议明细）。
                    return Ok(SystemProxyStatus {
                        enabled: true,
                        ..Default::default()
                    });
                };
                Ok(parse_win_proxy_server(&server_out.stdout))
            }
            Platform::Mac => {
                // 逐服务检查：代理可能设在非首个服务上（以太网优先 / VPN / 多网卡）。任一服务有启用
                // 代理即返回 —— 只看 services[0] 会漏检非首服务上的残留（上游 macOS 误判「无残留」的修复）。
                for service in self.list_network_services()? {
                    let st = self.mac_read_service(&service);
                    if st.enabled {
                        return Ok(st);
                    }
                }
                Ok(SystemProxyStatus::default())
            }
            Platform::Linux => {
                let http_proxy = self.linux_collect_schema("http");
                let https_proxy = self.linux_collect_schema("https");
                let socks_proxy = self.linux_collect_schema("socks");
                // 三者全空 = 无实际代理（用户清了 host）→ 不误报 enabled（否则 advisory 弹 ":port"）。
                if http_proxy.is_none() && https_proxy.is_none() && socks_proxy.is_none() {
                    return Ok(SystemProxyStatus::default());
                }
                Ok(SystemProxyStatus {
                    enabled: true,
                    http_proxy,
                    https_proxy,
                    socks_proxy,
                    // Linux（gsettings）没有 per-service bypass 清单这个概念。
                    bypass_domains: None,
                })
            }
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }

    /// macOS：**只读首个**网络服务（= [`restore_proxy`](Self::restore_proxy) 的回写目标），
    /// 不扫全部（对齐 上游 `SystemProxyManager.ts:472`）。Win/Linux 全局设置无逐服务概念 → 委托
    /// `get_proxy_status`。为什么口径与 `get_proxy_status` 不同，见 trait 方法文档。
    fn capture_original_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
        if self.platform != Platform::Mac {
            return self.get_proxy_status();
        }
        // 无网络服务（无网卡 / 解析空）→ 无可捕获也无可回写 → 空快照（disable 退化为 clear）。
        let Some(first) = self.list_network_services()?.into_iter().next() else {
            return Ok(SystemProxyStatus::default());
        };
        Ok(self.mac_read_service(&first))
    }

    fn list_network_services(&self) -> Result<Vec<String>, SystemIntegrationError> {
        match self.platform {
            Platform::Mac => mac_list_manageable_services(|c| self.run(c)),
            // Win/Linux 的代理是全局设置（注册表 / gsettings），无「逐服务」概念 → 单元素占位
            // （与 trait doc 一致；调用方按单目标遍历即可）。
            Platform::Win | Platform::Linux => Ok(vec![String::new()]),
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }

    fn set_proxy(&self, req: &ProxyEnableRequest) -> Result<(), SystemIntegrationError> {
        // retry 边界对齐上游：包**整个平台 enable 命令序列**（不含 marker/getProxyStatus——那在
        // `SystemProxyController::enable` 里、上游同样在 retry 外）。瞬时抖动整序重试，不误判失败回滚。
        match self.platform {
            Platform::Win => retry_op(
                &WIN_ENABLE_RETRY,
                || {
                    // 三条注册表写是系统代理成立条件，任一失败都让本 attempt 失败并由 retry/上层
                    // rollback 处理。QUIC 规则只是旧版本可能留下的可选清理：不存在时 netsh 本身也
                    // 返回 exit=1（doveh 真机已证），故与 clear/restore 两腿保持同一 best-effort 语义。
                    self.run_all(&windows_enable_commands(&self.reg_exe, req))?;
                    if let Err(err) = self.run_with_timeout(
                        &windows_clear_quic_command(&self.netsh_exe),
                        WINDOWS_QUIC_CLEANUP_TIMEOUT,
                    ) {
                        log::warn!(
                            "Windows QUIC legacy firewall-rule cleanup skipped after proxy enable: {err}"
                        );
                    }
                    Ok(())
                },
                self.sleeper,
            ),
            Platform::Mac => retry_op(
                &MAC_ENABLE_RETRY,
                || {
                    // 逐服务设（与 getProxyStatus/disable 遍历同口径）。getNetworkServices 在 retry 内
                    // 重取——对齐上游 mac retry 闭包（`SystemProxyManager.ts:485`）。
                    let services = self.list_network_services()?;
                    for svc in &services {
                        self.run_all(&mac_service_enable_commands(svc, req))?;
                    }
                    Ok(())
                },
                self.sleeper,
            ),
            Platform::Linux => retry_op(
                &LINUX_ENABLE_RETRY,
                || self.run_all(&linux_enable_commands(req)),
                self.sleeper,
            ),
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }

    fn clear_proxy(&self) -> Result<(), SystemIntegrationError> {
        match self.platform {
            Platform::Win => {
                // 禁用时务必先清 QUIC 规则（上游 disableProxy 首行）。best-effort：清不掉不阻断禁用
                // —— 关代理是断网防线，不能被一条防火墙规则清理失败拖住。
                let _ = self.run_with_timeout(
                    &windows_clear_quic_command(&self.netsh_exe),
                    WINDOWS_QUIC_CLEANUP_TIMEOUT,
                );
                self.run(&windows_disable_commands(&self.reg_exe))
                    .map(|_| ())
            }
            Platform::Mac => {
                let services = self.list_network_services()?;
                for svc in &services {
                    self.run_all(&mac_service_disable_commands(svc))?;
                }
                Ok(())
            }
            Platform::Linux => self.run(&linux_disable_command()).map(|_| ()),
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }

    fn restore_proxy(&self, original: &SystemProxyStatus) -> Result<(), SystemIntegrationError> {
        // 无实际原始代理 → 等价于「关」（对齐上游 disableProxy 的 else 分支 / restorePlan 全空腿）。
        if !original.enabled || !original.has_any_proxy() {
            return self.clear_proxy();
        }
        match self.platform {
            Platform::Win => {
                let _ = self.run_with_timeout(
                    &windows_clear_quic_command(&self.netsh_exe),
                    WINDOWS_QUIC_CLEANUP_TIMEOUT,
                );
                // 回写原始 ProxyServer 串 + ProxyEnable=1。
                self.run_all(&windows_restore_commands(&self.reg_exe, original))
            }
            Platform::Mac => {
                // **只往捕获源（services[0]）回写原始，其余服务一律关**。
                //
                // 为什么不能逐服务全铺（这是修掉的真实缺陷）：`original` 是**单个**服务的快照
                // （见 `capture_original_status`），而 `set_proxy` 把代理设到了**全部**服务上。
                // 若 disable 时把这份快照铺回全部服务，那些**本来就没设代理**的服务（Ethernet /
                // Thunderbolt / VPN…）会被平白写上一份用户从未配过的代理并 `state on` ——
                // 用户的网络配置被我们污染，且比接管前更糟（接管前它们是干净的）。
                //
                // 对称性：enable 在**全部**服务上留了痕 → disable 必须在**全部**服务上撤干净；
                // 但「撤」对捕获源是「回到原值」，对其余服务是「关」（它们的原值就是关）。
                let services = self.list_network_services()?;
                let mut it = services.iter();
                if let Some(first) = it.next() {
                    self.run_all(&mac_service_restore_commands(first, original))?;
                }
                for svc in it {
                    self.run_all(&mac_service_disable_commands(svc))?;
                }
                Ok(())
            }
            Platform::Linux => {
                // capture-three + 对称撤销：set 原本设了的、clear 原本未设的。
                self.run(&linux_set_mode_manual_command())?;
                for entry in crate::proxy::restore_plan(Some(original)) {
                    self.run_all(&linux_restore_schema_commands(&entry))?;
                }
                Ok(())
            }
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }
}

// ── 活态查询：当前 OS 代理是否仍指向本进程的 mixed 入站 ─────────────────────────────────
//
// # 这是本 crate 里关于系统代理的**第三个**问题（前两个见 `SystemProxyOps::get_proxy_status` /
// `capture_original_status` 的文档）
//
// | 谁 | 问的是 | macOS 读取面 | Linux 读 mode |
// |---|---|---|---|
// | `get_proxy_status` | 系统里**还有没有**代理残留（清理门控） | **全部**服务，任一有即返 | 否（残值也要清） |
// | `capture_original_status` | 待会儿要往**哪**回写、回写**什么** | `services[0]`（= 回写目标） | 否 |
// | `read_active_proxy`（本节） | **此刻流量实际会不会走我们** | **primary service**（默认路由出接口所属服务），查不到才回落 `services[0]` | **是**（mode≠manual 即不生效） |
//
// 三者合用一个实现必然让其中两个错：残留检测漏扫非首服务 = 误判「无残留」；活态查询扫全部服务
// 则会在「用户把主服务的代理关了、某个闲置服务上还留着指向我们的值」时谎报「仍生效」——
// 那正是本查询要抓的漏报形态。
//
// 活态查询与另两个的 macOS 读取面**也不同**：`capture_original_status` 问的是「回写目标」，那由
// `restore_proxy` 的写入口径（`services[0]`）定义，二者必须同源；活态查询问的是「流量走哪」，
// 那由**默认路由**定义。`-listallnetworkservices` 的顺序是配置优先级、`*` 只标停用不标未连接，
// 拿它当「在用服务」会在「雷电桥/USB 网卡排在 Wi-Fi 前」这种寻常配置上直接漏报（见
// `read_active_proxy` 的 Mac 分支注释）。
//
// # 为什么必须有活态查询（前端 `connection-state.ts` 的 DESIGN-REVIEW 两条漏报腿）
//
// 起核那一刻的 `SYSTEM_PROXY_FAILED` 只能证明「本轮 enable 失败」。它测不出：
//  1. **运行期**用户在系统设置里手动关掉/改掉代理（起核时是成功的，错误码干净）；
//  2. `error_code` 是单槽，起核后再来一条非终态错误会把 `SYSTEM_PROXY_FAILED` 覆盖掉。
// 两条都朝**漏报**（绿灯 + 明文直连）。活态查询直接读 OS、与本进程 mixed 入站比对，是这两条的
// 共同根治：它不是「历史上某一刻的记录」，而是**此刻的地面真相**。

/// 活态系统代理判定结果。
///
/// **判据不是「系统代理是否开着」，而是「它是否仍指向本进程的 mixed 入站」** —— 指向别的代理
/// 同样意味着我们的流量没走本地核（用户读到的「已连接」与真相相反）。见 [`points_to_mixed_inbound`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemProxyLiveStatus {
    /// 读到的 OS 代理设置原样（诊断/展示用；判定一律看 `points_to_us`）。
    pub status: SystemProxyStatus,
    /// **本结构的核心**：当前 OS 代理是否仍指向 `expected`。
    pub points_to_us: bool,
    /// 比对基准 `address:mixed_port`（如 `127.0.0.1:7890`）。
    pub expected: String,
}

/// 「当前 OS 代理是否仍指向本进程 mixed 入站」的**唯一**判据（纯函数）。
///
/// 三条缺一不可：
/// 1. `enabled` —— 关着的代理不导流（Windows 注册表在 `ProxyEnable=0` 时仍留 `ProxyServer` 值，
///    只看串会误判）。
/// 2. **至少一条协议腿等于 `address:mixed_port`** —— 端口必须逐字比对。只比 host 会把
///    `127.0.0.1:9999`（用户改了端口 / 另一个本地代理软件）判成「仍指向我们」，那是本函数
///    存在意义的反面（变异锁：`live_status_rejects_port_mismatch` 专锁这条）。
/// 3. **不得有任何一条腿指向别处** —— 我们 enable 时把 http/https(/socks) 全部指向同一个 mixed
///    端口；若某条腿被改成别的代理，该协议的流量就绕开了本地核 = 部分明文/第三方转发，
///    对「已连接」这个断言而言同样是假的。未设（`None`）的腿不算指向别处（Windows 从不设 socks=）。
pub fn points_to_mixed_inbound(status: &SystemProxyStatus, address: &str, mixed_port: u16) -> bool {
    if !status.enabled {
        return false;
    }
    let ours = format!("{address}:{mixed_port}");
    let mut matched = false;
    for leg in [&status.http_proxy, &status.https_proxy, &status.socks_proxy] {
        match leg {
            Some(p) if *p == ours => matched = true,
            // 指向别的代理 / 别的端口 → 该协议的流量不经我们，整体判未生效。
            Some(_) => return false,
            None => {}
        }
    }
    matched
}

impl<R: CommandRunner> SystemProxyOpsImpl<R> {
    /// **活态读**：此刻流量实际会走的 OS 代理设置。口径与另两个读法的分工见本节顶部表格。
    ///
    /// 与 `get_proxy_status` 的另一处刻意差异：**读失败一律 `Err`，绝不折成「未启用」**。
    /// `get_proxy_status` 把读失败折成 `default()`（对清理门控是安全方向：读不到就别动手）；
    /// 活态查询若也这么折，非 GNOME 桌面（`gsettings` 无该 schema）、PATH 缺 `reg.exe` 等
    /// 环境会被稳定判成「系统代理未生效」→ 每次都亮降级黄灯。**读不到 ≠ 没生效**，
    /// 让 `Err` 出栈、由调用方折成「未知」并回落既有信号，才是诚实的。
    ///
    /// # 已知盲区：PAC / 自动代理配置（**朝漏报**，与本查询原有方向一致）
    ///
    /// 本方法只读**静态代理**设置。若用户另开了 PAC（mac `networksetup -getautoproxyurl`、
    /// Windows `AutoConfigURL`、Linux `mode='auto'`），实际选路由 PAC 脚本决定 ——
    /// Windows/mac 上「静态代理指向我们」与「PAC 把流量导去别处」可以并存，此时本方法会回
    /// `points_to_us=true` 而流量其实没经核。Linux 无此洞（`mode='auto'` 已被 mode 闸门判非 manual）。
    /// 未补是因为判读 PAC 需要**执行** JS 脚本才能得出实际选路，那不是查询该干的事；
    /// 记在这里而不是假装覆盖了。要补的话应在此新增一路「PAC 已启用」信号交前端另行提示。
    pub fn read_active_proxy(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
        match self.platform {
            Platform::Win => {
                // 注册表是全局设置，无逐服务概念 → 与 get_proxy_status 同一读法，只是失败不折。
                let enable_out = self.run(&windows_query_command(&self.reg_exe, "ProxyEnable"))?;
                if !parse_win_proxy_enable(&enable_out.stdout) {
                    return Ok(SystemProxyStatus::default());
                }
                let server_out = self.run(&windows_query_command(&self.reg_exe, "ProxyServer"))?;
                Ok(parse_win_proxy_server(&server_out.stdout))
            }
            Platform::Mac => {
                // 读**主服务**（primary service = 默认路由出接口所属的网络服务），不是
                // `-listallnetworkservices` 的首项。
                //
                // 为什么首项是错的（这是修掉的真实缺陷）：`-listallnetworkservices` 的顺序是
                // **服务优先级配置序**，不是「谁在承载流量」。它列出的是「配置里排第几」，
                // 而 `*` 前缀只标**停用**（disabled），**不标未连接**（inactive）—— 一块插着线但没插
                // 网线的 USB 网卡 / 雷电桥 / 虚拟网卡完全可以排在 Wi-Fi 前面且不带 `*`。
                //
                // 后果是双向的，且都指向本查询存在的理由：
                //  - **漏报**（危险方向）：Wi-Fi 实际承载流量、用户在 Wi-Fi 上手关了代理，而首项是没插线的
                //    雷电桥、上面还留着我们 enable 时写的值（`set_proxy` 写的是**全部**服务，见 `:1069`）
                //    → `points_to_us=true` → 绿灯 + 明文直连，正是这条查询要抓的形态；
                //  - **误报**：反过来首项没设、主服务设了 → 稳定误亮降级黄灯。
                //
                // 判不出主服务时（无默认路由 / `route` 不可用 / 设备名映射不上）**回落首项** ——
                // 那是改动前的行为，不比它差；读不出来一律不谎报「未生效」（见方法文档）。
                let primary = self.mac_primary_service();
                let services;
                let target = match &primary {
                    Some(svc) => svc.as_str(),
                    None => {
                        services = self.list_network_services()?;
                        match services.first() {
                            Some(f) => f.as_str(),
                            // 无可用网络服务 → 读不出「流量会走哪」，按读失败处理（不谎报未生效）。
                            None => {
                                return Err(SystemIntegrationError::proxy(
                                    "无可用网络服务，无法判定系统代理是否生效",
                                ))
                            }
                        }
                    }
                };
                self.mac_read_service_strict(target)
            }
            Platform::Linux => {
                // **必须先读 mode**：mode=none/auto 时 GNOME 不下发代理，而 http/https/socks 的
                // host/port 残值仍在 —— 只读 host/port 会把「用户已关代理」判成「仍指向我们」，
                // 正是本查询要抓的漏报形态。
                let mode_out = self.run(&linux_gsettings_mode_get_command())?;
                if parse_gsettings_mode(&mode_out.stdout) != "manual" {
                    return Ok(SystemProxyStatus::default());
                }
                let http_proxy = self.linux_collect_schema_strict("http")?;
                let https_proxy = self.linux_collect_schema_strict("https")?;
                let socks_proxy = self.linux_collect_schema_strict("socks")?;
                if http_proxy.is_none() && https_proxy.is_none() && socks_proxy.is_none() {
                    return Ok(SystemProxyStatus::default());
                }
                Ok(SystemProxyStatus {
                    enabled: true,
                    http_proxy,
                    https_proxy,
                    socks_proxy,
                    // Linux（gsettings）没有 per-service bypass 清单这个概念。
                    bypass_domains: None,
                })
            }
            Platform::Other => Err(SystemIntegrationError::UnsupportedPlatform(
                "system proxy".into(),
            )),
        }
    }

    /// 活态查询完整入口：读 OS 设置 + 与 `address:mixed_port` 比对。
    pub fn live_status(
        &self,
        address: &str,
        mixed_port: u16,
    ) -> Result<SystemProxyLiveStatus, SystemIntegrationError> {
        let status = self.read_active_proxy()?;
        let points_to_us = points_to_mixed_inbound(&status, address, mixed_port);
        Ok(SystemProxyLiveStatus {
            status,
            points_to_us,
            expected: format!("{address}:{mixed_port}"),
        })
    }

    /// macOS：**主服务**（primary service）—— 默认路由出接口所属的网络服务名。
    ///
    /// 两跳纯 argv 查询：`route -n get default` 取出接口 BSD 名（`en0`）→
    /// `networksetup -listnetworkserviceorder` 建「设备名 → 服务名」映射反查。
    /// 任一跳失败 / 无默认路由 / 设备名不在映射里 → `None`（调用方回落 `services[0]`，见
    /// [`Self::read_active_proxy`] 的 Mac 分支）。
    ///
    /// **best-effort 且只读**：本方法一律不 `Err` 出栈 —— 它是「更准的目标选择」，不是新的失败面；
    /// 让它能 Err 会把「查不到主服务」升级成「活态查询失败」，比回落首项更糟。
    fn mac_primary_service(&self) -> Option<String> {
        let dev_out = self.run(&mac_default_route_command()).ok()?;
        let device = crate::route_ops::parse_mac_route_get_interface(&dev_out.stdout)?;
        let order_out = self.run(&mac_list_service_order_command()).ok()?;
        parse_mac_service_order(&order_out.stdout)
            .into_iter()
            .find(|(_, dev)| *dev == device)
            .map(|(svc, _)| svc)
    }

    /// macOS：读单服务三协议代理，**任一读失败即 Err**（对照 best-effort 的 `mac_read_service`）。
    ///
    /// `mac_read_service` 把单协议读失败当「未设」，那对残留检测是可接受的降级；活态查询里
    /// 三条腿全读失败会得到 `enabled=false` → 谎报「系统代理未生效」→ 稳定误亮降级黄灯。
    fn mac_read_service_strict(
        &self,
        service: &str,
    ) -> Result<SystemProxyStatus, SystemIntegrationError> {
        let read = |sub: &str| -> Result<Option<String>, SystemIntegrationError> {
            let out = self.run(&mac_read_proxy_command(sub, service))?;
            Ok(parse_mac_service_proxy(&out.stdout))
        };
        let mut st = SystemProxyStatus {
            http_proxy: read(MAC_PROXY_READ_SUBS[0])?,
            https_proxy: read(MAC_PROXY_READ_SUBS[1])?,
            socks_proxy: read(MAC_PROXY_READ_SUBS[2])?,
            enabled: false,
            // 严格版同样要捕获 —— 少了它，走这条路径拿到的快照 restore 时还不回 bypass。
            // 这里读失败按严格语义上抛（与三协议同）。
            bypass_domains: Some(parse_mac_bypass_domains(
                &self
                    .run(&mac_read_proxy_command(MAC_BYPASS_READ_SUB, service))?
                    .stdout,
            )),
        };
        st.enabled = st.has_any_proxy();
        Ok(st)
    }

    /// Linux：读单 schema 的 `host:port`，**命令失败即 Err**（对照吞错的 `linux_collect_schema`）。
    /// host 为空 → `Ok(None)`（该协议真的没设，不是读失败）。
    fn linux_collect_schema_strict(
        &self,
        schema: &str,
    ) -> Result<Option<String>, SystemIntegrationError> {
        let host_out = self.run(&linux_gsettings_get_command(schema, "host"))?;
        let Some(host) = parse_gsettings_host(&host_out.stdout) else {
            return Ok(None);
        };
        let port_out = self.run(&linux_gsettings_get_command(schema, "port"))?;
        let port = parse_gsettings_port(&port_out.stdout);
        Ok(Some(format!("{host}:{port}")))
    }
}

// ── 接管/释放状态机（marker + 防自指 + 失败兜底 + 崩溃恢复）──

/// 系统代理控制器：编排 enable/disable + marker 生命周期。
/// 1:1 移植 上游 `SystemProxyBase` + 三平台 enable/disable 的 marker 编排逻辑。
pub struct SystemProxyController<Ops: SystemProxyOps, Fs: MarkerFs> {
    ops: Ops,
    marker: ProxyMarker<Fs>,
    /// enable 前保存的原始代理快照（disable 恢复用）。
    original: Option<SystemProxyStatus>,
}

impl<Ops: SystemProxyOps, Fs: MarkerFs> SystemProxyController<Ops, Fs> {
    pub fn new(ops: Ops, marker: ProxyMarker<Fs>) -> Self {
        Self {
            ops,
            marker,
            original: None,
        }
    }

    /// 当前保存的原始代理快照（测试 / 诊断用）。
    pub fn original_snapshot(&self) -> Option<&SystemProxyStatus> {
        self.original.as_ref()
    }

    /// 启用系统代理（接管）。
    /// 步骤（对齐 Polaris 三平台 enableProxy）：
    /// 1. 前置写 marker（intent：enable 期间崩溃也留 marker）。
    /// 2. 读当前状态 → stripSelf 防自指 → 保存原始。
    /// 3. ops.set_proxy。
    /// 4. 失败兜底：调 disable（恢复或简单关），失败再清 marker。
    pub fn enable(&mut self, req: &ProxyEnableRequest) -> Result<(), SystemIntegrationError> {
        // 1. 前置写 marker（intent）。
        self.marker.write(&req.our_host_port(), None);

        // 2. 保存原始（防自指）。
        // 走 `capture_original_status` 而非 `get_proxy_status`：口径是「回写目标的当前值」
        // （macOS 只读 services[0]），与 `restore_proxy` 的回写目标成对锚定 —— 顺带把 21 次
        // `networksetup` 读砍到 3 次（启动关键路径 ~0.6s）。二者的分工见 trait 方法文档。
        match self.ops.capture_original_status() {
            Ok(status) => {
                let marker_host = self.marker.read_our_host_port();
                self.original = strip_self(
                    Some(&status),
                    &req.address,
                    req.http_port,
                    marker_host.as_deref(),
                );
            }
            Err(_) => {
                // 无法获取原始 → 继续（original 保持 None，disable 时简单关）。
            }
        }

        // 3. 设代理。
        if let Err(e) = self.ops.set_proxy(req) {
            // 4. 失败兜底（fail-closed）：经 disable 统一收口。
            if self.disable().is_err() {
                // disable 也失败 → 补清 marker（杜绝自指残留）。
                self.marker.clear();
            }
            return Err(e);
        }

        Ok(())
    }

    /// 禁用系统代理（释放）。
    /// 步骤（对齐 Polaris 三平台 disableProxy）：
    /// - 有原始 → restore；无原始 → clear。
    /// - 成功 → 清 marker + 清内存 original。
    /// - 失败 → 不清 marker（保留供下次启动重试，跨平台 review M3）。
    pub fn disable(&mut self) -> Result<(), SystemIntegrationError> {
        let result = if let Some(original) = self.original.take() {
            self.ops.restore_proxy(&original)
        } else {
            self.ops.clear_proxy()
        };

        match result {
            Ok(()) => {
                self.original = None;
                self.marker.clear();
                Ok(())
            }
            Err(e) => {
                // 失败保留 marker（交下次启动 recovery 重试），不静默丢回滚信号。
                Err(e)
            }
        }
    }

    /// **维度7 #8：marker 崩溃恢复**。
    /// 启动时调用：读 marker，若存在（上次崩溃/强杀残留）→ 清除残留代理（防死端口断网）→ 清 marker。
    /// 返回 `Some(marker)` 表示执行了恢复（记录的 our_host_port + original），`None` 表示无残留。
    ///
    /// Polaris 启动恢复路径（marker 残留 → disableProxy 清理）。本方法是 #8 的可测入口。
    pub fn recover_from_marker(&mut self) -> Option<ProxyMarkerData> {
        let marker = self.marker.read()?;
        // 有 marker → 上次未正常 disable（崩溃/强杀）。清除残留代理：
        // 有 original → 恢复原始；无 original → 简单关（杜绝指向我们死端口的代理残留致断网）。
        let original = marker.original_settings.clone();
        let _ = match original {
            Some(orig) => {
                self.original = Some(orig.clone());
                self.ops.restore_proxy(&orig)
            }
            None => self.ops.clear_proxy(),
        };
        // 恢复成功与否都清 marker（启动一次性恢复，不反复触发；真实实现 Polaris clearMarkerFile 静态入口）。
        self.marker.clear();
        self.original = None;
        Some(marker)
    }

    /// **维度7 #8：终态统一清系统代理**（`ensureSystemProxyCleared` 等价物）。
    ///
    /// ## 不变量（为什么必须有）
    ///
    /// 重启 / 切模式 / 起核失败时，**旧会话的系统代理仍指向现已死的端口 → 全网断**。
    /// 故所有「核已死」终态点都必须过这里。上游 `ProxyManager.ts:592-607`：start 的 public 包装
    /// catch 腿统一收口，覆盖全部 start 入口（IPC / 托盘 / 自动连接）与 restart 的 start 腿。
    ///
    /// ## 门控（三层，缺一不可）
    ///
    /// 1. **marker 在**才动手 —— 杜绝误清**用户自配**的代理（marker = 「这代理是我们设的」的唯一凭证）。
    /// 2. **实查仍指向我们**（`points_to_us`：精确 `host:port` 或 `host` 匹配 —— 后者兜 mac
    ///    socks 端口与 http 端口不同的情形）才 disable；否则只清失真 marker。
    /// 3. **marker 删除竞态防护**：清失真 marker 前重读，若期间已被新一轮 enable 写了**新** marker
    ///    （`our_host_port` 变了）则保留 —— 否则会删掉新会话的 marker 致其兜底全瞎（上游 C1）。
    ///
    /// ## 幂等
    ///
    /// 无 marker → no-op（**fresh start 无 marker，故正常启动路径调它零副作用**）。
    /// 已清过 → marker 已删 → 再调仍 no-op。故可在每个终态点无脑调，重复调用安全。
    ///
    /// ## 边界：`stopping` 守卫不在此
    ///
    /// 上游 `ensureSystemProxyCleared` 首行是 `if (this.stopping) return`（主动停止/重启中跳过，
    /// 避免清了又被 start reconcile 设回的 C1 竞态）。那是 **lifecycle 状态**，属调用方
    /// （`ProxyRuntime` / `LifecycleGate`）的知识，本 crate 不持有 → **调用方须在非 stopping 语境调用**。
    /// 同理「单飞」（上游 `clearingSystemProxy`）也属调用方：本方法自身幂等，重复调用只是多读一次
    /// marker，不会重复 disable（第一次已清 marker → 第二次门控 1 即返）。
    ///
    /// 返回 `true` = 真的执行了 disable（曾指向我们）；`false` = 无需动作 / 仅清失真 marker。
    pub fn ensure_cleared(&mut self) -> bool {
        // 门控 1：无 marker = 系统代理不是我们设的（或已清）→ 绝不动手。
        let Some(marker) = self.marker.read() else {
            return false;
        };

        // 门控 2：实查当前状态是否仍指向我们的（已死的）端口。
        // 读不到状态 → 保守视为「不指向我们」，只走清失真 marker 腿（不盲目 disable 用户的代理）。
        let status = self.ops.get_proxy_status().ok();
        if !points_to_us(status.as_ref(), &marker.our_host_port) {
            // 已关 / 用户手改指向别处 → 仅清失真 marker。
            // 门控 3（C1 竞态）：重读，仅当仍是**同一个** marker 才清。
            if let Some(cur) = self.marker.read() {
                if cur.our_host_port == marker.our_host_port {
                    self.marker.clear();
                    self.original = None;
                }
            }
            return false;
        }

        // 系统代理仍指向我们的死端口 → disable（内部：有原始则恢复，无则简单关 + 清 marker）。
        // original 优先取内存，回退 marker 里持久化的快照（跨会话崩溃恢复路径）。
        if self.original.is_none() {
            self.original = marker.original_settings.clone();
        }
        let _ = self.disable();
        true
    }

    /// 是否存在接管 marker（终态清理门控用）。
    pub fn has_marker(&self) -> bool {
        self.marker.exists()
    }

    /// 检测「**不是我们设的**系统代理」，返回其 `host:port`（无则 `None`）。
    ///
    /// TUN 模式下另有系统代理开着 → 遵循系统代理的应用会绕开 TUN 走那个代理（它可能是别的工具设的、
    /// 也可能是用户自配的），表现为「连上了但部分应用异常」。上层据此发一次性提示（**只提示不动手**：
    /// 动手清用户自配的代理正是 marker 门控立意要禁的 stomp）。
    ///
    /// 判定与 [`ensure_cleared`](Self::ensure_cleared) 的门控 1 **互补而非重复**：
    /// - 有 marker → 系统代理是我们设的 → 不是「别人的」，`None`（此时该管的是 `ensure_cleared`）。
    /// - 无 marker + 实查确有代理 → 别人的，报出去。
    ///
    /// 读不到状态（exec 失败）→ `None`：**宁可不提示，也不拿猜测吓用户**。
    pub fn detect_foreign_proxy(&self) -> Option<String> {
        if self.marker.exists() {
            return None;
        }
        let status = self.ops.get_proxy_status().ok()?;
        // `enabled` 与 `has_any_proxy()` **都要**：三平台 get_proxy_status 目前已各自早退（Win 的
        // ProxyEnable=0 / mac 的 !st.enabled / Linux 的三 host 全空），故此处理论上冗余——但那是
        // **它们的**不变式，不是本函数的。显式再判一次，将来任一腿改成「回填 server 但 enabled=false」
        // （Win 注册表 ProxyServer 在 ProxyEnable=0 时依然留值，正是这个形态）也不会退化成误报。
        if !status.enabled || !status.has_any_proxy() {
            return None;
        }
        // 展示优先级 http → https → socks（与 marker 记 `address:http_port` 同口径，取首个非空即可）。
        status
            .http_proxy
            .or(status.https_proxy)
            .or(status.socks_proxy)
    }
}

/// 当前系统代理是否仍指向 marker 记录的我们（`host:port` 精确匹配，或 `host` 匹配）。
///
/// **为什么也认 host 匹配**：mac 的 socks 端口与 http 端口不同（`socks_port` ≠ `http_port`），
/// 而 marker 只记 `address:http_port` → 仅按 `host:port` 精确匹配会漏判 socks 腿的残留。
/// 与启动期 marker 恢复同口径（上游 `ensureSystemProxyCleared` 的 `pointsToUs`）。
fn points_to_us(status: Option<&SystemProxyStatus>, marker_host_port: &str) -> bool {
    let Some(status) = status else {
        return false;
    };
    if !status.enabled {
        return false;
    }
    let marker_host = marker_host_port
        .split(':')
        .next()
        .unwrap_or(marker_host_port);
    let hit = |p: &Option<String>| -> bool {
        match p {
            Some(proxy) => {
                proxy == marker_host_port
                    || proxy.split(':').next().unwrap_or(proxy.as_str()) == marker_host
            }
            None => false,
        }
    };
    hit(&status.http_proxy) || hit(&status.https_proxy) || hit(&status.socks_proxy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// 记录所有调用的 mock ops（不触碰宿主网络）。
    #[derive(Default)]
    struct MockOps {
        calls: RefCell<Vec<&'static str>>,
        status: RefCell<SystemProxyStatus>,
        set_fails: bool,
        clear_fails: bool,
        restore_fails: bool,
    }
    impl SystemProxyOps for MockOps {
        fn get_proxy_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
            self.calls.borrow_mut().push("get_status");
            Ok(self.status.borrow().clone())
        }
        /// 与 `get_proxy_status` 分开记账，便于断言 enable 走的是**捕获**口径而非残留检测口径。
        fn capture_original_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
            self.calls.borrow_mut().push("capture");
            Ok(self.status.borrow().clone())
        }
        fn list_network_services(&self) -> Result<Vec<String>, SystemIntegrationError> {
            Ok(vec!["Wi-Fi".into()])
        }
        fn set_proxy(&self, _req: &ProxyEnableRequest) -> Result<(), SystemIntegrationError> {
            self.calls.borrow_mut().push("set");
            if self.set_fails {
                return Err(SystemIntegrationError::proxy("set failed"));
            }
            Ok(())
        }
        fn clear_proxy(&self) -> Result<(), SystemIntegrationError> {
            self.calls.borrow_mut().push("clear");
            if self.clear_fails {
                return Err(SystemIntegrationError::proxy("clear failed"));
            }
            Ok(())
        }
        fn restore_proxy(&self, _o: &SystemProxyStatus) -> Result<(), SystemIntegrationError> {
            self.calls.borrow_mut().push("restore");
            if self.restore_fails {
                return Err(SystemIntegrationError::proxy("restore failed"));
            }
            Ok(())
        }
    }

    fn mem_marker() -> ProxyMarker<crate::proxy::proxy_tests_helpers::MemFs> {
        ProxyMarker::new(
            crate::proxy::proxy_tests_helpers::MemFs::new(),
            "/marker.json",
        )
    }

    fn req() -> ProxyEnableRequest {
        ProxyEnableRequest {
            address: "127.0.0.1".into(),
            http_port: 8080,
            socks_port: 1080,
            bypass_list: vec!["10.0.0.0/8".into(), "localhost".into()],
        }
    }

    // ── 接管/释放状态机 ──

    #[test]
    fn enable_writes_marker_then_set_clears_on_disable() {
        let ops = MockOps::default();
        let mut controller = SystemProxyController::new(ops, mem_marker());

        // enable：mock status 默认 enabled=false → stripSelf 返回 Some(disabled) 作为 original。
        controller.enable(&req()).unwrap();
        assert!(controller.has_marker());
        assert!(controller.ops.calls.borrow().contains(&"set"));

        // disable：original=Some(disabled) → restore 被调（恢复原始=禁用态）。
        controller.disable().unwrap();
        assert!(!controller.has_marker());
        assert!(controller.ops.calls.borrow().contains(&"restore"));
    }

    #[test]
    fn enable_captures_original_via_capture_path_not_residue_scan() {
        // R0.5 接线断言：enable 的原始快照走 `capture_original_status`（macOS 只读 services[0]），
        // **不**走 `get_proxy_status`（残留检测，扫全部服务）。走错口径 = 启动多 18 次 exec，
        // 且可能捕获到非首服务的代理却回写到首个服务上。
        let ops = MockOps::default();
        let mut controller = SystemProxyController::new(ops, mem_marker());
        controller.enable(&req()).unwrap();

        let calls = controller.ops.calls.borrow().clone();
        assert!(calls.contains(&"capture"), "enable 必须走捕获口径");
        assert!(
            !calls.contains(&"get_status"),
            "enable 不得走残留检测口径（扫全部服务）"
        );
    }

    #[test]
    fn enable_failure_rolls_back_via_disable() {
        let ops = MockOps {
            set_fails: true,
            ..Default::default()
        };
        let mut controller = SystemProxyController::new(ops, mem_marker());
        let err = controller.enable(&req()).unwrap_err();
        assert!(err.to_string().contains("set failed"));
        // 失败兜底：disable 被调用（original=Some(disabled) → restore）→ marker 清。
        assert!(controller.ops.calls.borrow().contains(&"restore"));
        assert!(!controller.has_marker());
    }

    #[test]
    fn disable_keeps_marker_on_failure_for_retry() {
        // original 会是 Some(disabled)（mock status 默认 enabled=false）→ disable 走 restore；
        // 让 restore 失败以验证 marker 保留。
        let ops = MockOps {
            restore_fails: true,
            ..Default::default()
        };
        let mut controller = SystemProxyController::new(ops, mem_marker());
        controller.enable(&req()).unwrap();
        assert!(controller.has_marker());

        // disable 失败（restore_fails）→ marker 保留（供下次启动重试）。
        controller.disable().unwrap_err();
        assert!(controller.has_marker());
    }

    #[test]
    fn enable_strips_self_referential_original() {
        // 当前代理已指向我们自己 → stripSelf → original=None（disable 不会恢复死端口）。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:8080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut controller = SystemProxyController::new(ops, mem_marker());
        controller.enable(&req()).unwrap();
        assert!(controller.original_snapshot().is_none());
    }

    #[test]
    fn enable_preserves_real_external_original() {
        let external = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            ..Default::default()
        };
        let ops = MockOps {
            status: RefCell::new(external.clone()),
            ..Default::default()
        };
        let mut controller = SystemProxyController::new(ops, mem_marker());
        controller.enable(&req()).unwrap();
        // 真实第三方代理被保留为 original → disable 时会 restore。
        assert_eq!(controller.original_snapshot(), Some(&external));
    }

    // ── 维度7 #8：marker 崩溃恢复（核心验收）──

    #[test]
    fn recover_from_marker_clears_residual_proxy() {
        // 场景：上次会话 enable 写了 marker，进程崩溃（未 disable）→ marker 残留。
        // 重启新会话 → recover_from_marker 读到 → 清除残留代理 → 清 marker。
        let ops = MockOps::default();
        let mut controller = SystemProxyController::new(ops, mem_marker());

        // 模拟崩溃残留：直接写 marker（绕过 enable，仿佛上个进程写的）。
        controller.marker.write("127.0.0.1:8080", None);
        assert!(controller.has_marker());

        // 重启后恢复。
        let recovered = controller.recover_from_marker();
        assert!(recovered.is_some());
        assert_eq!(recovered.unwrap().our_host_port, "127.0.0.1:8080");
        // 无 original → clear_proxy 被调（清除指向死端口的残留代理）。
        assert!(controller.ops.calls.borrow().contains(&"clear"));
        // marker 已清（下次启动不再误恢复）。
        assert!(!controller.has_marker());
    }

    #[test]
    fn recover_from_marker_restores_original_when_present() {
        // marker 携带 original（Linux 写入路径）→ 恢复原始代理而非简单关。
        let ops = MockOps::default();
        let mut controller = SystemProxyController::new(ops, mem_marker());

        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.lan:3128".into()),
            ..Default::default()
        };
        controller.marker.write("127.0.0.1:8080", Some(&original));

        let recovered = controller.recover_from_marker().expect("marker present");
        assert_eq!(
            recovered.original_settings.unwrap().http_proxy,
            Some("proxy.lan:3128".to_string())
        );
        // restore 被调（恢复用户原始代理）。
        assert!(controller.ops.calls.borrow().contains(&"restore"));
        assert!(!controller.has_marker());
    }

    #[test]
    fn recover_from_marker_noop_when_no_marker() {
        let ops = MockOps::default();
        let mut controller = SystemProxyController::new(ops, mem_marker());
        // 无 marker → 不动作。
        assert!(controller.recover_from_marker().is_none());
        assert!(controller.ops.calls.borrow().is_empty());
    }

    #[test]
    fn crash_recovery_full_cycle_two_sessions() {
        // 端到端：会话1 enable → 崩溃 → 会话2 recover → 干净。
        // 用 Clone-共享 FS 模拟跨会话同一磁盘 marker 文件（MemFs 内部 Rc 共享状态）。
        use crate::proxy::proxy_tests_helpers::MemFs;
        let fs = MemFs::new();
        // 会话1：写 marker 后「崩溃」（未 disable）。
        let marker1 = ProxyMarker::new(fs.clone(), "/m");
        marker1.write("127.0.0.1:8080", None);

        // 「重启」：新 ProxyMarker 读同一文件（FS 状态跨「进程」存活）。
        let marker2 = ProxyMarker::new(fs, "/m");
        assert!(marker2.read().is_some(), "marker survived crash");
        marker2.clear();
        assert!(marker2.read().is_none(), "marker cleared after recovery");
    }

    // ── 三平台命令构造测试 ──

    /// 从 reg add 命令里取 /d 后的值（形如 add REG_PATH /v K /t T /d <VAL> /f）。
    fn reg_value(cmd: &Command) -> &String {
        // /d 的下一项即值。
        let idx = cmd.args.iter().position(|a| a == "/d").expect("/d present");
        cmd.args.get(idx + 1).expect("value after /d")
    }

    #[test]
    fn windows_enable_commands_no_socks_in_proxyserver() {
        let cmds = windows_enable_commands("reg.exe", &req());
        // ProxyServer 行：只 http/https，无 socks（Chromium SOCKS5 DNS 污染防护）。
        let proxy_server = cmds
            .iter()
            .find(|c| c.args.get(3) == Some(&"ProxyServer".to_string()))
            .expect("ProxyServer cmd");
        let val = reg_value(proxy_server);
        assert!(val.contains("http=127.0.0.1:8080"));
        assert!(val.contains("https=127.0.0.1:8080"));
        assert!(
            !val.contains("socks="),
            "must not set socks= in ProxyServer"
        );
        // ProxyEnable=1
        let enable = cmds
            .iter()
            .find(|c| c.args.get(3) == Some(&"ProxyEnable".to_string()))
            .unwrap();
        assert_eq!(reg_value(enable), "1");
        assert_eq!(cmds.len(), 3, "代理事务只包含三条必要的注册表写");
        assert!(
            !cmds
                .iter()
                .any(|c| c.args.contains(&"name=Polaris_Block_QUIC".to_string())),
            "可选 QUIC 清理不得混进必要事务"
        );
    }

    #[test]
    fn windows_disable_sets_proxyenable_zero() {
        let cmd = windows_disable_commands("reg.exe");
        assert_eq!(reg_value(&cmd), "0");
        assert_eq!(cmd.args.get(3), Some(&"ProxyEnable".to_string()));
    }

    #[test]
    fn mac_enable_commands_per_service_all_protocols() {
        let cmds = mac_service_enable_commands("Wi-Fi", &req());
        // web/secureweb/socks set + state on + bypass
        assert!(cmds
            .iter()
            .any(|c| c.args[0] == "-setwebproxy" && c.args[1] == "Wi-Fi"));
        assert!(cmds.iter().any(|c| c.args[0] == "-setsecurewebproxy"));
        assert!(cmds.iter().any(|c| c.args[0] == "-setsocksfirewallproxy"));
        assert!(cmds
            .iter()
            .any(|c| c.args[0] == "-setwebproxystate" && c.args.last() == Some(&"on".to_string())));
        assert!(cmds.iter().any(|c| c.args[0] == "-setproxybypassdomains"));
    }

    #[test]
    fn mac_disable_commands_all_off() {
        let cmds = mac_service_disable_commands("Ethernet");
        assert_eq!(cmds.len(), 3);
        assert!(cmds
            .iter()
            .all(|c| c.args.last() == Some(&"off".to_string())));
    }

    #[test]
    fn parse_mac_network_services_filters() {
        let stdout = "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*Bluetooth PAN\nEthernet\n\nBluetooth PAN\n";
        let svcs = parse_mac_network_services(stdout);
        assert_eq!(svcs, vec!["Wi-Fi".to_string(), "Ethernet".to_string()]);
    }

    #[test]
    fn linux_enable_commands_gnome_manual() {
        let cmds = linux_enable_commands(&req());
        // mode manual
        assert!(cmds
            .iter()
            .any(|c| { c.args[1..4] == ["org.gnome.system.proxy", "mode", "'manual'"] }));
        // http host/port/enabled
        assert!(cmds.iter().any(|c| {
            c.args[1] == "org.gnome.system.proxy.http"
                && c.args[2] == "host"
                && c.args[3] == "127.0.0.1"
        }));
        assert!(cmds.iter().any(|c| {
            c.args[1] == "org.gnome.system.proxy.http"
                && c.args[2] == "enabled"
                && c.args[3] == "true"
        }));
        // socks
        assert!(cmds.iter().any(|c| {
            c.args[1] == "org.gnome.system.proxy.socks"
                && c.args[2] == "port"
                && c.args[3] == "1080"
        }));
        // ignore-hosts（GVariant 数组）
        let ignore = cmds.iter().find(|c| c.args[2] == "ignore-hosts").unwrap();
        let val = ignore.args.last().unwrap();
        assert!(val.starts_with("['") && val.ends_with("']"));
        assert!(val.contains("'10.0.0.0/8'"));
        assert!(val.contains("'localhost'"));
    }

    #[test]
    fn linux_disable_sets_mode_none() {
        let cmd = linux_disable_command();
        assert_eq!(cmd.args[3], "none");
    }

    // ── 三平台状态解析（纯函数，Linux 上跑测 win/mac 解析）──

    #[test]
    fn parse_win_proxy_enable_detects_0x1() {
        assert!(parse_win_proxy_enable(
            "\r\nHKEY_CURRENT_USER\\...\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n"
        ));
        assert!(!parse_win_proxy_enable(
            "    ProxyEnable    REG_DWORD    0x0\r\n"
        ));
        assert!(!parse_win_proxy_enable(""));
    }

    #[test]
    fn parse_win_proxy_server_splits_protocols() {
        let stdout = "\r\nHKEY_CURRENT_USER\\Software\\...\\Internet Settings\r\n    ProxyServer    REG_SZ    http=127.0.0.1:8080;https=127.0.0.1:8080;socks=127.0.0.1:1080\r\n";
        let st = parse_win_proxy_server(stdout);
        assert!(st.enabled);
        assert_eq!(st.http_proxy, Some("127.0.0.1:8080".into()));
        assert_eq!(st.https_proxy, Some("127.0.0.1:8080".into()));
        assert_eq!(st.socks_proxy, Some("127.0.0.1:1080".into()));
    }

    #[test]
    fn parse_win_proxy_server_missing_line_keeps_enabled_true() {
        // 上游：`if (!proxyServerMatch) return { enabled: true }` —— 有 ProxyEnable=1 但读不到明细。
        let st = parse_win_proxy_server("some unrelated output");
        assert!(st.enabled);
        assert!(!st.has_any_proxy());
    }

    #[test]
    fn parse_win_proxy_server_ignores_similar_key_name() {
        // 防前缀误匹配（ProxyServerBackup 不是 ProxyServer）。
        let st = parse_win_proxy_server("    ProxyServerBackup    REG_SZ    http=evil:1\r\n");
        assert!(!st.has_any_proxy(), "不得匹配 ProxyServerBackup");
    }

    /// **裸 `host:port`（Windows 设置 UI 手填形态）必须被认成「全协议同值」。**
    ///
    /// 变异锁：删掉 `parse_win_proxy_server` 里那段 `if !value.contains('=')` 早退 → 三腿全 `None`
    /// → 下面的 `points_to_mixed_inbound` 断言当场转红（= 用户手填了我们的地址却被判「未生效」，
    /// 稳定误亮降级黄灯）。
    #[test]
    fn parse_win_proxy_server_accepts_bare_hostport_as_all_protocols() {
        let st = parse_win_proxy_server("    ProxyServer    REG_SZ    127.0.0.1:7890\r\n");
        assert!(st.enabled);
        assert_eq!(st.http_proxy.as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(st.https_proxy.as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(
            st.socks_proxy, None,
            "裸形态不填 socks 腿：未设 ≠ 指向别处，多填会在用户另设 socks 时造假象"
        );
        // 真正要守的终态：手填我们的地址 → 活态判定必须说「生效」。
        assert!(
            points_to_mixed_inbound(&st, "127.0.0.1", 7890),
            "裸形态指向我们的 mixed 口 → 必须判生效"
        );
        // 反向不受影响：手填了别的代理仍判未生效。
        let other = parse_win_proxy_server("    ProxyServer    REG_SZ    proxy.corp:3128\r\n");
        assert!(!points_to_mixed_inbound(&other, "127.0.0.1", 7890));

        // 空值 / 纯空白仍按「读不到明细」处理（不造出一条 `Some("")` 的假腿）。
        let blank = parse_win_proxy_server("    ProxyServer    REG_SZ       \r\n");
        assert!(!blank.has_any_proxy());
    }

    #[test]
    fn parse_mac_service_proxy_reads_server_port() {
        let stdout =
            "Enabled: Yes\nServer: 127.0.0.1\nPort: 8080\nAuthenticated Proxy Enabled: 0\n";
        assert_eq!(
            parse_mac_service_proxy(stdout),
            Some("127.0.0.1:8080".into())
        );
    }

    #[test]
    fn parse_mac_service_proxy_none_when_disabled() {
        let stdout = "Enabled: No\nServer:\nPort: 0\n";
        assert_eq!(parse_mac_service_proxy(stdout), None);
    }

    #[test]
    fn parse_gsettings_host_strips_quotes_and_empty() {
        assert_eq!(
            parse_gsettings_host("'127.0.0.1'\n"),
            Some("127.0.0.1".into())
        );
        // 用户清了 host → gsettings 返回 '' → None（不误报 enabled，否则 advisory 弹 ":port"）。
        assert_eq!(parse_gsettings_host("''\n"), None);
        assert_eq!(parse_gsettings_host("\n"), None);
    }

    /// gsettings guint 端口带 GVariant 前缀（`uint32 8080`）。不剥 → split_host_port 的 parse 恒失败
    /// → **恢复分支永不触发**（上游注释明写此坑）。本测试是那条坑的守门人。
    #[test]
    fn parse_gsettings_port_strips_gvariant_prefix() {
        assert_eq!(parse_gsettings_port("uint32 8080\n"), "8080");
        assert_eq!(parse_gsettings_port("uint16 3128\n"), "3128");
        assert_eq!(parse_gsettings_port("8080\n"), "8080");
        // 端口剥完须能被 split_host_port 吃下（组合面，防「两扇门之间的缝」）。
        let hp = crate::proxy::split_host_port(Some(&format!(
            "127.0.0.1:{}",
            parse_gsettings_port("uint32 8080\n")
        )));
        assert_eq!(hp.map(|h| h.port), Some(8080));
    }

    // ── 恢复命令构造 ──

    #[test]
    fn windows_restore_commands_rebuild_proxyserver_and_enable() {
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            socks_proxy: Some("socks.corp:1080".into()),
            ..Default::default()
        };
        let cmds = windows_restore_commands("reg.exe", &original);
        let server = cmds
            .iter()
            .find(|c| c.args.get(3) == Some(&"ProxyServer".to_string()))
            .expect("ProxyServer cmd");
        let val = reg_value(server);
        assert!(val.contains("http=proxy.corp:3128"));
        assert!(val.contains("socks=socks.corp:1080"));
        assert!(!val.contains("https="), "原始未设 https → 不得凭空造出");
        // ProxyEnable=1 且在 ProxyServer 之后（先值后开关）。
        let enable_idx = cmds
            .iter()
            .position(|c| c.args.get(3) == Some(&"ProxyEnable".to_string()))
            .unwrap();
        let server_idx = cmds
            .iter()
            .position(|c| c.args.get(3) == Some(&"ProxyServer".to_string()))
            .unwrap();
        assert!(server_idx < enable_idx);
        assert_eq!(reg_value(&cmds[enable_idx]), "1");
    }

    #[test]
    fn mac_service_restore_commands_symmetric_undo() {
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            ..Default::default()
        };
        let cmds = mac_service_restore_commands("Wi-Fi", &original);
        // 设了的 → set + state on
        assert!(cmds.iter().any(|c| c.args[0] == "-setwebproxy"
            && c.args[2] == "proxy.corp"
            && c.args[3] == "3128"));
        assert!(cmds
            .iter()
            .any(|c| c.args[0] == "-setwebproxystate" && c.args[2] == "on"));
        // 没设的 → state off（对称撤销，不把 http 扇出到 https/socks）
        assert!(cmds
            .iter()
            .any(|c| c.args[0] == "-setsecurewebproxystate" && c.args[2] == "off"));
        assert!(cmds
            .iter()
            .any(|c| c.args[0] == "-setsocksfirewallproxystate" && c.args[2] == "off"));
        assert!(!cmds.iter().any(|c| c.args[0] == "-setsecurewebproxy"));
    }

    #[test]
    fn linux_restore_schema_set_and_clear() {
        // set：有 host:port
        let set_entry = crate::proxy::RestorePlanEntry {
            schema: "http",
            hp: Some(crate::proxy::HostPort {
                host: "proxy.lan".into(),
                port: 3128,
            }),
        };
        let set_cmds = linux_restore_schema_commands(&set_entry);
        assert!(set_cmds
            .iter()
            .any(|c| c.args[2] == "host" && c.args[3] == "proxy.lan"));
        assert!(set_cmds
            .iter()
            .any(|c| c.args[2] == "port" && c.args[3] == "3128"));
        assert!(set_cmds
            .iter()
            .any(|c| c.args[2] == "enabled" && c.args[3] == "true"));

        // clear：无 hp
        let clear_entry = crate::proxy::RestorePlanEntry {
            schema: "https",
            hp: None,
        };
        let clear_cmds = linux_restore_schema_commands(&clear_entry);
        assert!(clear_cmds
            .iter()
            .any(|c| c.args[2] == "host" && c.args[3].is_empty()));
        // 非 http schema 不写 enabled
        assert!(!clear_cmds.iter().any(|c| c.args[2] == "enabled"));
    }

    // ══════════ 生产实现接线（SystemProxyOpsImpl）══════════
    //
    // 全部在 Linux 上跑测三平台 —— 靠运行时 Platform 枚举 + MockRunner 注入，不碰宿主网络。
    // 这正是审计 §M1 判「运行时枚举优于 cfg」的兑现处：若这些分派是 #[cfg]，以下测试在 Linux 上
    // 一条都跑不到。

    use crate::exec::exec_tests_helpers::MockRunner;

    fn ops_for(platform: Platform, runner: MockRunner) -> SystemProxyOpsImpl<MockRunner> {
        SystemProxyOpsImpl::with_platform(runner, platform)
    }

    // ── Windows 腿 ──

    #[test]
    fn impl_win_get_status_disabled_short_circuits() {
        // ProxyEnable=0x0 → 早退，不再查 ProxyServer（上游 getProxyStatus 早退腿）。
        let ops = ops_for(
            Platform::Win,
            MockRunner::default().with_arg_stdout("ProxyEnable", "ProxyEnable REG_DWORD 0x0"),
        );
        let st = ops.get_proxy_status().unwrap();
        assert!(!st.enabled);
        assert!(
            !ops.runner.ran_arg("ProxyServer"),
            "disabled 时不该查 ProxyServer"
        );
    }

    #[test]
    fn impl_win_get_status_parses_enabled_proxy() {
        let runner = MockRunner::default()
            .with_arg_stdout("ProxyEnable", "ProxyEnable REG_DWORD 0x1")
            .with_arg_stdout(
                "ProxyServer",
                "    ProxyServer    REG_SZ    http=127.0.0.1:8080;socks=127.0.0.1:1080",
            );
        let st = ops_for(Platform::Win, runner).get_proxy_status().unwrap();
        assert!(st.enabled);
        assert_eq!(st.http_proxy, Some("127.0.0.1:8080".into()));
        assert_eq!(st.socks_proxy, Some("127.0.0.1:1080".into()));
    }

    #[test]
    fn impl_win_get_status_falls_back_to_disabled_on_command_failure() {
        // 上游 getProxyStatus 整体 try/catch → { enabled: false }。
        let runner = MockRunner {
            fail_args: vec!["ProxyEnable".into()],
            ..Default::default()
        };
        let st = ops_for(Platform::Win, runner).get_proxy_status().unwrap();
        assert!(!st.enabled);
    }

    #[test]
    fn impl_win_set_proxy_runs_reg_add_sequence_via_runner() {
        let ops = ops_for(Platform::Win, MockRunner::default());
        ops.set_proxy(&req()).unwrap();
        // reg add ProxyServer / ProxyEnable / ProxyOverride + netsh QUIC 清理，全经 runner 下发。
        assert!(ops.runner.ran_arg("ProxyServer"));
        assert!(ops.runner.ran_arg("ProxyEnable"));
        assert!(ops.runner.ran_arg("ProxyOverride"));
        assert!(ops.runner.ran_arg("Polaris_Block_QUIC"));
        assert_eq!(
            ops.runner.timeout_for_arg("Polaris_Block_QUIC"),
            Some(WINDOWS_QUIC_CLEANUP_TIMEOUT),
            "可选 QUIC 清理必须使用独立短预算，不能再把启动主链钉住 10s"
        );
        assert_eq!(
            ops.runner.timeout_for_arg("ProxyServer"),
            Some(PROXY_EXEC_TIMEOUT),
            "必要注册表事务仍保留宽预算"
        );
        // 用 System32 绝对路径（PATH 缺 System32 的设备也能跑）。
        assert!(ops
            .runner
            .snapshot()
            .iter()
            .any(|c| c.program.ends_with("reg.exe")));
    }

    #[test]
    fn impl_win_set_proxy_survives_quic_cleanup_failure() {
        // `netsh delete rule` 在规则本来就不存在时返回 exit=1；这已经是目标状态，不能让三条
        // 注册表写成功后的系统代理事务被反判失败。真实注册表写失败仍由 run_all/retry 返回 Err。
        let runner = MockRunner {
            fail_args: vec!["Polaris_Block_QUIC".into()],
            ..Default::default()
        };
        let ops = ops_for(Platform::Win, runner);
        ops.set_proxy(&req())
            .expect("可选 QUIC 清理失败不得阻断系统代理启用");
        assert!(ops.runner.ran_arg("Polaris_Block_QUIC"));
        assert!(ops.runner.ran_arg("ProxyServer"));
        assert!(ops.runner.ran_arg("ProxyEnable"));
        assert!(ops.runner.ran_arg("ProxyOverride"));
    }

    #[test]
    fn impl_win_set_proxy_still_fails_on_required_registry_write_failure() {
        // best-effort 只放宽 QUIC 清理；任何必要注册表写失败仍须让整个 attempt 失败并走既有重试。
        let runner = MockRunner {
            fail_args: vec!["ProxyEnable".into()],
            ..Default::default()
        };
        let ops = ops_for(Platform::Win, runner).with_noop_sleeper();
        assert!(
            ops.set_proxy(&req()).is_err(),
            "必要注册表写失败必须继续向上报错"
        );
        assert_eq!(
            ops.runner.count_arg("ProxyEnable"),
            3,
            "首次 + 两次 retry 都应在必要写失败处中止"
        );
        assert!(
            !ops.runner.ran_arg("ProxyOverride"),
            "失败后的必要写不得继续"
        );
        assert!(
            !ops.runner.ran_arg("Polaris_Block_QUIC"),
            "必要事务未完成时不得提前做可选清理"
        );
    }

    #[test]
    fn impl_win_clear_proxy_survives_quic_cleanup_failure() {
        // QUIC 规则清理失败**不得**阻断禁用 —— 关代理是断网防线。
        let runner = MockRunner {
            fail_args: vec!["Polaris_Block_QUIC".into()],
            ..Default::default()
        };
        let ops = ops_for(Platform::Win, runner);
        ops.clear_proxy()
            .expect("QUIC 清理失败不该阻断 ProxyEnable=0");
        assert!(ops.runner.ran_arg("ProxyEnable"));
    }

    // ── macOS 腿 ──

    const MAC_SERVICES: &str =
        "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\nEthernet\n";

    /// 与 [`MAC_SERVICES`] 等价的 `-listnetworkserviceorder` 形态（两个服务各带 BSD 设备名）。
    /// mac 枚举口径 2026-08-08 起以本命令为主、`-listallnetworkservices` 仅作回落，
    /// 故这些测试要喂它才不会走进回落腿（走了就多一次 exec，掩盖「只读首个服务」这类计数断言）。
    const MAC_SERVICE_ORDER: &str = "An asterisk (*) denotes that a network service is disabled.\n\
(1) Wi-Fi\n\
(Hardware Port: Wi-Fi, Device: en0)\n\
\n\
(2) Ethernet\n\
(Hardware Port: Ethernet, Device: en4)\n";

    #[test]
    fn impl_mac_get_status_scans_all_services_not_just_first() {
        // 代理设在**第二个**服务（Ethernet）上 —— 只看 services[0] 会漏检（上游修过的 macOS 误判）。
        let runner = MockRunner::default().with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
        // Wi-Fi 三协议均返回空（Enabled: No）；Ethernet 的 -getwebproxy 返回启用。
        runner.by_arg.borrow_mut().insert(
            "Ethernet".to_string(),
            "Enabled: Yes\nServer: 10.0.0.1\nPort: 3128\n".to_string(),
        );
        let ops = ops_for(Platform::Mac, runner);
        let st = ops.get_proxy_status().unwrap();
        assert!(st.enabled, "非首服务上的代理必须被检出");
        assert_eq!(st.http_proxy, Some("10.0.0.1:3128".into()));
    }

    #[test]
    fn impl_mac_capture_original_reads_only_first_service() {
        // R0.5：原始快照只读 services[0]（回写目标），不扫全部 —— 7 服务时省 18 次 networksetup exec。
        // 与上一个测试成对：`get_proxy_status` 仍扫全部（残留检测），两条口径互不塌陷。
        let runner = MockRunner::default()
            .with_arg_stdout("-listnetworkserviceorder", MAC_SERVICE_ORDER)
            .with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
        let ops = ops_for(Platform::Mac, runner);
        ops.capture_original_status().unwrap();

        // 三协议 + bypass 各读一次，且**只**读首个服务。
        assert_eq!(ops.runner.count_arg_exact("-getwebproxy"), 1);
        assert_eq!(ops.runner.count_arg_exact("-getsecurewebproxy"), 1);
        assert_eq!(ops.runner.count_arg_exact("-getsocksfirewallproxy"), 1);
        // bypass 清单：enable 会整表覆盖它 ⇒ 不捕获就还不回去（2026-08-09 补）。
        assert_eq!(ops.runner.count_arg_exact("-getproxybypassdomains"), 1);
        assert!(ops.runner.ran_arg("Wi-Fi"));
        assert_eq!(
            ops.runner.count_arg("Ethernet"),
            0,
            "非首服务不得被读 —— 扫全部正是本条要砍掉的启动开销"
        );
        // 总 exec = 1 次服务枚举 + 3 次协议读 + 1 次 bypass 读（扫全部会是 1 + 8）。
        // 这个数是**成本棘轮**：每加一次读都要在这里显式认账，别让捕获阶段悄悄变胖
        // —— mac 起核耗时里 networksetup 串行调用本就是大头。
        assert_eq!(ops.runner.snapshot().len(), 5);
        // 顺带钉死：枚举走的是带设备名那条，没落进 `-listallnetworkservices` 回落腿
        // （落进去就是 5 次 exec，且会把无底层设备的虚拟服务一并纳入）。
        assert!(!ops.runner.ran_arg("-listallnetworkservices"));
    }

    /// **读失败必须落 `None`，不得折成空清单** —— 折成空 = restore 时写 `Empty` = 把用户清单清掉。
    ///
    /// 这条是「两者必须可分辨」那句文档的**可执行版本**。第一版门只测了纯函数
    /// `mac_service_restore_commands`，于是「把 `.ok()` 补个 `.or(Some(vec![]))`」这个变异逃逸了
    /// —— 判据落在被调函数上、没落在捕获腿上。
    #[test]
    fn mac_bypass_read_failure_is_not_an_empty_list() {
        let runner = MockRunner {
            // 只让 bypass 那条读失败，三协议照常成功 —— 单独钉住这一格。
            fail_args: vec!["-getproxybypassdomains".into()],
            ..Default::default()
        }
        .with_arg_stdout("-listnetworkserviceorder", MAC_SERVICE_ORDER)
        .with_arg_stdout("-getwebproxy", "Enabled: Yes\nServer: h\nPort: 80\n");
        let ops = ops_for(Platform::Mac, runner);
        let st = ops.capture_original_status().unwrap();

        assert!(
            st.bypass_domains.is_none(),
            "bypass 读失败被折成了 {:?} —— restore 会据此写 Empty，把用户自定义的清单清掉",
            st.bypass_domains
        );
        // 自检：这次确实尝试读过（否则「是 None」只说明压根没读）。
        assert!(
            ops.runner.ran_arg("-getproxybypassdomains"),
            "根本没读 bypass —— 上一条断言恒真"
        );
        // 正向对照：同一次捕获里三协议是读成功的，证明失败注入只打中了 bypass 这一条。
        assert_eq!(st.http_proxy.as_deref(), Some("h:80"));
    }

    #[test]
    fn impl_mac_restore_does_not_leak_proxy_onto_untouched_services() {
        // **核心缺陷断言**：original 是 services[0] 的快照，绝不能铺到本来没设代理的其余服务上。
        // 退回「逐服务全铺」→ Ethernet 会被写上 proxy.lan 并 state on（污染用户网络配置）→ 本测试转红。
        let runner = MockRunner::default().with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
        let ops = ops_for(Platform::Mac, runner);
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.lan:3128".into()),
            ..Default::default()
        };
        ops.restore_proxy(&original).unwrap();
        let cmds = ops.runner.snapshot();

        // 1) 回写只发生一次，且落在捕获源 Wi-Fi 上。
        assert_eq!(
            ops.runner.count_arg_exact("-setwebproxy"),
            1,
            "回写必须只落在捕获源（services[0]），不得逐服务全铺"
        );
        assert!(cmds
            .iter()
            .any(|c| c.args.iter().any(|a| a == "-setwebproxy")
                && c.args.iter().any(|a| a == "Wi-Fi")
                && c.args.iter().any(|a| a == "proxy.lan")));

        // 2) 任何提到非首服务的命令都不得携带原始代理的 host —— 这是「误铺」的直接指纹。
        assert!(
            !cmds.iter().any(|c| c.args.iter().any(|a| a == "Ethernet")
                && c.args.iter().any(|a| a.contains("proxy.lan"))),
            "本来没设代理的服务被写入了原始代理值 = 污染用户网络配置"
        );

        // 3) 但非首服务仍须被**关**掉（enable 在全部服务上留了痕，disable 必须全部撤干净）。
        assert!(cmds
            .iter()
            .any(|c| c.args.iter().any(|a| a == "-setwebproxystate")
                && c.args.iter().any(|a| a == "Ethernet")
                && c.args.iter().any(|a| a == "off")));
        assert!(cmds.iter().any(
            |c| c.args.iter().any(|a| a == "-setsocksfirewallproxystate")
                && c.args.iter().any(|a| a == "Ethernet")
                && c.args.iter().any(|a| a == "off")
        ));
    }

    #[test]
    fn impl_mac_capture_original_with_no_services_is_empty_snapshot() {
        // 无网络服务（无网卡）→ 无可捕获也无可回写 → 空快照（disable 退化为 clear，不 panic/不越界）。
        let runner = MockRunner::default()
            .with_arg_stdout("-listallnetworkservices", "An asterisk (*) denotes...\n");
        let ops = ops_for(Platform::Mac, runner);
        let st = ops.capture_original_status().unwrap();
        assert!(!st.enabled);
        assert!(!st.has_any_proxy());
    }

    #[test]
    fn impl_mac_set_proxy_applies_to_every_service() {
        let runner = MockRunner::default().with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
        let ops = ops_for(Platform::Mac, runner);
        ops.set_proxy(&req()).unwrap();
        // 两个服务各一套 set（Wi-Fi + Ethernet）。精确匹配 —— `-setwebproxystate` 含 `-setwebproxy`。
        assert_eq!(ops.runner.count_arg_exact("-setwebproxy"), 2);
        assert!(ops.runner.ran_arg("Wi-Fi"));
        assert!(ops.runner.ran_arg("Ethernet"));
        assert!(ops.runner.ran_arg("-setproxybypassdomains"));
    }

    #[test]
    fn impl_mac_list_services_filters_disabled_and_bluetooth() {
        let runner = MockRunner::default().with_arg_stdout(
            "-listallnetworkservices",
            "An asterisk (*) denotes...\nWi-Fi\n*Thunderbolt Bridge\nBluetooth PAN\nEthernet\n",
        );
        let svcs = ops_for(Platform::Mac, runner)
            .list_network_services()
            .unwrap();
        assert_eq!(svcs, vec!["Wi-Fi".to_string(), "Ethernet".to_string()]);
    }

    // ── Linux 腿 ──

    #[test]
    fn impl_linux_get_status_reads_gsettings_with_uint_port() {
        // 读取序：http host → http port → https host（空，不再读 port）→ socks host（空）。
        let runner = MockRunner {
            stdouts: RefCell::new(vec![
                "'127.0.0.1'\n".into(), // http host
                "uint32 8080\n".into(), // http port
                "''\n".into(),          // https host（未设）
                "''\n".into(),          // socks host（未设）
            ]),
            ..Default::default()
        };
        let st = ops_for(Platform::Linux, runner).get_proxy_status().unwrap();
        assert!(st.enabled);
        // uint32 前缀已剥 → 端口是纯数字（不剥则恢复分支永不触发）。
        assert_eq!(st.http_proxy, Some("127.0.0.1:8080".into()));
        assert_eq!(st.https_proxy, None, "host 空 → 不得扇出 http 的值");
        assert_eq!(st.socks_proxy, None);
    }

    #[test]
    fn impl_linux_get_status_all_hosts_empty_is_disabled() {
        // 三 schema host 全空 = 用户清了 → 不误报 enabled（否则 advisory 弹 ":port"）。
        let runner = MockRunner {
            stdouts: RefCell::new(vec!["''\n".into(), "''\n".into(), "''\n".into()]),
            ..Default::default()
        };
        let st = ops_for(Platform::Linux, runner).get_proxy_status().unwrap();
        assert!(!st.enabled);
        assert!(!st.has_any_proxy());
    }

    #[test]
    fn impl_linux_set_and_clear_via_gsettings() {
        let ops = ops_for(Platform::Linux, MockRunner::default());
        ops.set_proxy(&req()).unwrap();
        assert!(ops.runner.ran_arg("org.gnome.system.proxy.http"));
        assert!(ops.runner.ran_arg("ignore-hosts"));

        let ops2 = ops_for(Platform::Linux, MockRunner::default());
        ops2.clear_proxy().unwrap();
        assert!(ops2.runner.ran_arg("none"), "clear → mode none");
    }

    #[test]
    fn impl_linux_restore_uses_capture_three_symmetric_undo() {
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.lan:3128".into()),
            ..Default::default()
        };
        let ops = ops_for(Platform::Linux, MockRunner::default());
        ops.restore_proxy(&original).unwrap();
        // mode manual + http 回写 + https/socks 清空（对称撤销）。
        assert!(ops.runner.ran_arg("manual"));
        assert!(ops.runner.ran_arg("proxy.lan"));
        let cmds = ops.runner.snapshot();
        // https host 被置空串。
        assert!(cmds.iter().any(|c| c.args.len() >= 4
            && c.args[1] == "org.gnome.system.proxy.https"
            && c.args[2] == "host"
            && c.args[3].is_empty()));
    }

    // ── 跨平台：restore 无原始 → 退化为 clear ──

    #[test]
    fn impl_restore_with_empty_original_degrades_to_clear() {
        for platform in [Platform::Win, Platform::Mac, Platform::Linux] {
            let runner =
                MockRunner::default().with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
            let ops = ops_for(platform, runner);
            // enabled=false 的原始 → 等价「关」，绝不回写空代理串。
            ops.restore_proxy(&SystemProxyStatus::default()).unwrap();
            let cmds = ops.runner.snapshot();
            assert!(!cmds.is_empty(), "{platform:?} 应执行清除动作");
            // 不得出现「回写原始」的痕迹（win: ProxyEnable=1 / linux: mode manual）。
            assert!(
                !ops.runner.ran_arg("manual"),
                "{platform:?}: 无原始不该置 manual"
            );
        }
    }

    #[test]
    fn impl_other_platform_is_unsupported_not_silent_noop() {
        let ops = ops_for(Platform::Other, MockRunner::default());
        // 未知平台须显式报错，不得静默假装成功（否则 UI 显示「已接管」而系统毫无变化）。
        assert!(matches!(
            ops.get_proxy_status(),
            Err(SystemIntegrationError::UnsupportedPlatform(_))
        ));
        assert!(ops.set_proxy(&req()).is_err());
        assert!(ops.clear_proxy().is_err());
        assert!(ops.runner.snapshot().is_empty(), "不该跑任何命令");
    }

    // ══════════ FX-proxy-ops-retry（row69）：重试原语 + 三平台 enable 重试 ══════════

    // ── retry_op 纯函数：次数 / 指数退避 / shouldRetry 短路 / 上限 ──

    #[test]
    fn retry_op_retries_until_success_and_backs_off_exponentially() {
        let attempts = Cell::new(0u32);
        let slept: RefCell<Vec<Duration>> = RefCell::new(vec![]);
        let cfg = RetryConfig {
            max_retries: 2,
            delay: Duration::from_millis(500),
            exponential_backoff: true,
            should_retry: |_| true,
        };
        let out: Result<u32, SystemIntegrationError> = retry_op(
            &cfg,
            || {
                let n = attempts.get() + 1;
                attempts.set(n);
                if n < 3 {
                    Err(SystemIntegrationError::proxy("transient"))
                } else {
                    Ok(n)
                }
            },
            |d| slept.borrow_mut().push(d),
        );
        assert_eq!(out.unwrap(), 3, "第 3 次尝试成功");
        assert_eq!(attempts.get(), 3, "首次 + 2 次重试 = 3 次执行");
        // 指数退避：500ms, 1000ms（第 0/1 次重试前）。锁死 exponential_backoff=true。
        assert_eq!(
            *slept.borrow(),
            vec![Duration::from_millis(500), Duration::from_millis(1000)]
        );
    }

    #[test]
    fn retry_op_gives_up_after_max_retries_plus_one_attempts() {
        let attempts = Cell::new(0u32);
        let cfg = RetryConfig {
            max_retries: 2,
            delay: Duration::from_millis(1),
            exponential_backoff: false,
            should_retry: |_| true,
        };
        let out: Result<(), SystemIntegrationError> = retry_op(
            &cfg,
            || {
                attempts.set(attempts.get() + 1);
                Err(SystemIntegrationError::proxy("always fails"))
            },
            |_| {},
        );
        assert!(out.is_err());
        assert_eq!(attempts.get(), 3, "总尝试 = max_retries + 1");
    }

    #[test]
    fn retry_op_aborts_immediately_when_should_retry_false() {
        let attempts = Cell::new(0u32);
        let cfg = RetryConfig {
            max_retries: 3,
            delay: Duration::from_millis(1),
            exponential_backoff: false,
            should_retry: |_| false,
        };
        let out: Result<(), SystemIntegrationError> = retry_op(
            &cfg,
            || {
                attempts.set(attempts.get() + 1);
                Err(SystemIntegrationError::proxy("not retryable"))
            },
            |_| panic!("shouldRetry=false 不该 sleep"),
        );
        assert!(out.is_err());
        assert_eq!(attempts.get(), 1, "shouldRetry=false → 只跑首次，不重试");
    }

    #[test]
    fn retry_op_fixed_backoff_when_exponential_disabled() {
        let slept: RefCell<Vec<Duration>> = RefCell::new(vec![]);
        let cfg = RetryConfig {
            max_retries: 2,
            delay: Duration::from_millis(500),
            exponential_backoff: false,
            should_retry: |_| true,
        };
        let _: Result<(), SystemIntegrationError> = retry_op(
            &cfg,
            || Err(SystemIntegrationError::proxy("always")),
            |d| slept.borrow_mut().push(d),
        );
        // 固定退避：两次都 500ms（与指数分支区分）。
        assert_eq!(
            *slept.borrow(),
            vec![Duration::from_millis(500), Duration::from_millis(500)]
        );
    }

    // ── shouldRetry 谓词（逐字对齐三平台）──

    #[test]
    fn win_should_retry_aborts_on_permission_and_command_not_found() {
        let ret = |m: &str| win_enable_should_retry(&SystemIntegrationError::proxy(m));
        assert!(
            !ret("reg.exe 退出码 1: Access Denied"),
            "access denied → 不重试"
        );
        assert!(
            !ret("ProxyServer requires permission"),
            "permission → 不重试"
        );
        assert!(
            !ret("reg.exe 启动失败: No such file"),
            "命令未找到 → 不重试"
        );
        assert!(
            ret("reg.exe 退出码 1: being used by another process"),
            "瞬时占用 → 重试"
        );
    }

    #[test]
    fn mac_should_retry_aborts_on_permission_or_not_authorized() {
        let ret = |m: &str| mac_enable_should_retry(&SystemIntegrationError::proxy(m));
        assert!(
            !ret("networksetup: permission denied"),
            "permission → 不重试"
        );
        assert!(
            !ret("Error: not authorized to change"),
            "not authorized → 不重试"
        );
        assert!(ret("networksetup connection timed out"), "瞬时 → 重试");
    }

    /// **变异锁（权限词表）**：把 [`PERMISSION_DENIED_NEEDLES`] 缩回上游那两词
    /// （`permission` / `not authorized`）→ 本用例的 `requires admin privileges` 等断言立刻转红。
    ///
    /// 守的是「必败错误被当成瞬时抖动」这一形态：mac enable 会多跑 2 次必败重试 + 1.5s 退避，
    /// DNS set 更贵 —— 那 1.5s 是**持 `dns_controller` 锁**空耗的。
    #[test]
    fn permission_needles_cover_macos_admin_privileges_wording() {
        // 真机 macOS `networksetup` 的常见权限失败原文（Rust 侧把子进程 stderr 原文归入消息串）。
        for msg in [
            "networksetup: requires admin privileges to change proxy settings",
            "** Error: requires administrator privileges.",
            "setting DNS: Operation not permitted",
            "You must be root to run this command",
            "You must be running as root to modify network configuration",
            "networksetup: permission denied",
            "Error: not authorized to change",
        ] {
            assert!(
                !mac_enable_should_retry(&SystemIntegrationError::proxy(msg)),
                "权限类错误必须立即放弃（重试 100 次也不会变好）: {msg}"
            );
        }
        // 真瞬时错误不得被误判成权限（词表宁窄勿宽的另一半）。
        for msg in [
            "networksetup connection timed out",
            "reg.exe 退出码 1: being used by another process",
            "resource temporarily unavailable",
        ] {
            assert!(
                mac_enable_should_retry(&SystemIntegrationError::proxy(msg)),
                "瞬时错误必须仍可重试: {msg}"
            );
        }
    }

    #[test]
    fn linux_default_should_retry_only_on_temporary_patterns() {
        let ret = |m: &str| default_should_retry(&SystemIntegrationError::proxy(m));
        assert!(
            ret("gsettings 超时: connection timed out"),
            "timed out → 重试"
        );
        assert!(ret("ETIMEDOUT while setting"), "ETIMEDOUT → 重试");
        assert!(!ret("gsettings: No such schema"), "非瞬时错误 → 不重试");
    }

    // ── set_proxy 整体重试（经 FlakyRunner 注入「前 N 次失败、其后成功」，Linux 上跑测三平台）──

    /// 前 N 次「首命令」失败、其后全部成功的瞬时抖动 mock。
    ///
    /// `run_all` 遇首个失败即中止 → 每个失败 attempt 恰好消耗 1 次命令调用 → `remaining` 每 attempt 减 1，
    /// 故 `remaining=k` 精确模拟「前 k 次 attempt 失败」。`fail_msg` 决定 shouldRetry 走「重试」还是「放弃」。
    struct FlakyRunner {
        calls: RefCell<Vec<Command>>,
        remaining_failures: RefCell<u32>,
        fail_msg: String,
        /// 成功路径的 argv 子串 → stdout（如 mac `-listallnetworkservices`）。
        by_arg: HashMap<String, String>,
    }

    impl FlakyRunner {
        fn new(fail_first: u32, fail_msg: &str) -> Self {
            Self {
                calls: RefCell::new(vec![]),
                remaining_failures: RefCell::new(fail_first),
                fail_msg: fail_msg.to_string(),
                by_arg: HashMap::new(),
            }
        }
        fn with_arg_stdout(mut self, arg_substr: &str, stdout: &str) -> Self {
            self.by_arg
                .insert(arg_substr.to_string(), stdout.to_string());
            self
        }
        fn count_arg(&self, substr: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.args.iter().any(|a| a.contains(substr)))
                .count()
        }
        fn ran_arg(&self, substr: &str) -> bool {
            self.calls
                .borrow()
                .iter()
                .any(|c| c.args.iter().any(|a| a.contains(substr)))
        }
    }

    impl CommandRunner for FlakyRunner {
        fn run(
            &self,
            cmd: &Command,
            _timeout: Duration,
        ) -> Result<crate::exec::CommandOutput, String> {
            self.calls.borrow_mut().push(cmd.clone());
            {
                let mut rem = self.remaining_failures.borrow_mut();
                if *rem > 0 {
                    *rem -= 1;
                    return Err(self.fail_msg.clone());
                }
            }
            for (k, v) in &self.by_arg {
                if cmd.args.iter().any(|a| a.contains(k)) {
                    return Ok(crate::exec::CommandOutput {
                        stdout: v.clone(),
                        stderr: String::new(),
                    });
                }
            }
            Ok(crate::exec::CommandOutput::default())
        }
    }

    #[test]
    fn set_proxy_win_retries_transient_then_succeeds() {
        // 前 2 次瞬时失败（占用类），第 3 次成功 —— maxRetries=2 恰够。
        let runner = FlakyRunner::new(2, "reg.exe 退出码 1: being used by another process");
        let ops = SystemProxyOpsImpl::with_platform(runner, Platform::Win).with_noop_sleeper();
        ops.set_proxy(&req()).expect("2 次瞬时失败后应重试成功");
        // 首命令 ProxyServer 被尝试 3 次（首次 + 2 重试）；成功 attempt 跑完整序列。
        assert_eq!(ops.runner.count_arg("ProxyServer"), 3);
        assert!(ops.runner.ran_arg("ProxyEnable"));
        assert!(ops.runner.ran_arg("ProxyOverride"));
    }

    #[test]
    fn set_proxy_win_exhausts_after_max_retries_plus_one() {
        // 永远失败 → 总尝试 = maxRetries(2) + 1 = 3，锁死 Windows maxRetries=2。
        let runner = FlakyRunner::new(99, "reg.exe 退出码 1: being used by another process");
        let ops = SystemProxyOpsImpl::with_platform(runner, Platform::Win).with_noop_sleeper();
        assert!(ops.set_proxy(&req()).is_err(), "耗尽重试仍失败");
        assert_eq!(
            ops.runner.count_arg("ProxyServer"),
            3,
            "maxRetries=2 → 3 次尝试"
        );
    }

    #[test]
    fn set_proxy_win_access_denied_aborts_without_retry() {
        // 权限拒绝 → shouldRetry=false → 仅 1 次尝试，绝不重试。
        let runner = FlakyRunner::new(99, "reg.exe 退出码 1: Access Denied");
        let ops = SystemProxyOpsImpl::with_platform(runner, Platform::Win).with_noop_sleeper();
        assert!(ops.set_proxy(&req()).is_err());
        assert_eq!(
            ops.runner.count_arg("ProxyServer"),
            1,
            "access denied → 立即放弃，仅 1 次"
        );
    }

    #[test]
    fn set_proxy_mac_retries_transient_then_succeeds() {
        // mac：首命令是 -listnetworkserviceorder（2026-08-08 起的枚举口径；在 retry 闭包内重取）。
        // 前 2 次失败、第 3 次成功。
        let runner = FlakyRunner::new(2, "networksetup: temporarily unavailable")
            .with_arg_stdout("-listnetworkserviceorder", MAC_SERVICE_ORDER)
            .with_arg_stdout("-listallnetworkservices", MAC_SERVICES);
        let ops = SystemProxyOpsImpl::with_platform(runner, Platform::Mac).with_noop_sleeper();
        ops.set_proxy(&req()).expect("mac 瞬时失败后应重试成功");
        assert_eq!(
            ops.runner.count_arg("-listnetworkserviceorder"),
            3,
            "首命令被尝试 3 次（maxRetries=2）"
        );
        assert!(
            ops.runner.ran_arg("-setwebproxy"),
            "成功 attempt 跑完设代理序列"
        );
    }

    #[test]
    fn set_proxy_linux_retries_temporary_but_not_generic() {
        // Linux 用 defaultShouldRetry：仅瞬时网络类错误重试。首命令 = gsettings ... mode manual。
        // (a) "timed out" 属瞬时 → maxRetries=1 → 首次失败后第 2 次成功。
        let r1 = FlakyRunner::new(1, "gsettings 超时: connection timed out");
        let ops1 = SystemProxyOpsImpl::with_platform(r1, Platform::Linux).with_noop_sleeper();
        ops1.set_proxy(&req()).expect("timed out 属瞬时 → 重试成功");
        assert_eq!(
            ops1.runner.count_arg("manual"),
            2,
            "首次失败 + 1 重试成功 = 2 次"
        );

        // (b) 非瞬时错误 → defaultShouldRetry=false → 不重试（即便 maxRetries=1），仅 1 次。
        let r2 = FlakyRunner::new(1, "gsettings: No such schema org.gnome.system.proxy");
        let ops2 = SystemProxyOpsImpl::with_platform(r2, Platform::Linux).with_noop_sleeper();
        assert!(ops2.set_proxy(&req()).is_err(), "非瞬时错误不重试 → 失败");
        assert_eq!(
            ops2.runner.count_arg("manual"),
            1,
            "非瞬时 → 立即放弃，仅 1 次（锁死 Linux 用 defaultShouldRetry 而非 always-retry）"
        );
    }

    // ══════════ 维度7 #8：ensure_cleared 终态收口 ══════════

    #[test]
    fn ensure_cleared_noop_without_marker() {
        // **fresh start 路径**：无 marker → 零副作用（故可在每个 start 失败腿无脑调）。
        let ops = MockOps::default();
        let mut c = SystemProxyController::new(ops, mem_marker());
        assert!(!c.ensure_cleared());
        assert!(c.ops.calls.borrow().is_empty(), "无 marker 不得读状态/动手");
    }

    #[test]
    fn ensure_cleared_disables_when_still_pointing_at_our_dead_port() {
        // 核心不变式：旧会话系统代理仍指向现已死的端口 → 必须清，否则全网断。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:8080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);

        assert!(c.ensure_cleared(), "指向我们 → 应执行 disable");
        assert!(c.ops.calls.borrow().contains(&"clear"));
        assert!(!c.has_marker(), "清完须删 marker");
    }

    #[test]
    fn ensure_cleared_restores_original_from_marker_across_sessions() {
        // 崩溃跨会话：marker 里带着 enable 前的用户原始代理 → 恢复它而非简单关。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:8080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            ..Default::default()
        };
        c.marker.write("127.0.0.1:8080", Some(&original));

        assert!(c.ensure_cleared());
        assert!(
            c.ops.calls.borrow().contains(&"restore"),
            "marker 带原始 → 恢复用户代理，不是简单关"
        );
        assert!(!c.has_marker());
    }

    #[test]
    fn ensure_cleared_never_touches_user_configured_proxy() {
        // 门控 1：无 marker = 代理不是我们设的 → 即便系统代理开着也绝不动。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("proxy.corp:3128".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        assert!(!c.ensure_cleared());
        assert!(
            !c.ops.calls.borrow().contains(&"clear"),
            "绝不误清用户自配代理"
        );
    }

    #[test]
    fn ensure_cleared_only_drops_stale_marker_when_proxy_moved_elsewhere() {
        // 门控 2：marker 在但用户已手改代理指向别处 → 只清失真 marker，不 disable 用户的新代理。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("proxy.corp:3128".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);

        assert!(!c.ensure_cleared(), "未指向我们 → 不 disable");
        assert!(
            !c.ops.calls.borrow().contains(&"clear"),
            "不得动用户改到别处的代理"
        );
        assert!(!c.has_marker(), "失真 marker 应被清");
    }

    #[test]
    fn ensure_cleared_keeps_newer_marker_from_concurrent_enable() {
        // 门控 3（C1 竞态）：清失真 marker 前重读；若已被新一轮 enable 写了**新** marker → 保留，
        // 否则会删掉新会话的 marker 致其兜底全瞎。
        struct RewritingOps {
            marker_fs: crate::proxy::proxy_tests_helpers::MemFs,
        }
        impl SystemProxyOps for RewritingOps {
            fn get_proxy_status(&self) -> Result<SystemProxyStatus, SystemIntegrationError> {
                // 模拟：读状态期间，另一轮 enable 写了新 marker（新 host:port）。
                ProxyMarker::new(self.marker_fs.clone(), "/marker.json")
                    .write("127.0.0.1:9999", None);
                // 返回「指向别处」的状态 → 走清失真 marker 腿。
                Ok(SystemProxyStatus {
                    enabled: true,
                    http_proxy: Some("proxy.corp:3128".into()),
                    ..Default::default()
                })
            }
            fn list_network_services(&self) -> Result<Vec<String>, SystemIntegrationError> {
                Ok(vec![])
            }
            fn set_proxy(&self, _r: &ProxyEnableRequest) -> Result<(), SystemIntegrationError> {
                Ok(())
            }
            fn clear_proxy(&self) -> Result<(), SystemIntegrationError> {
                Ok(())
            }
            fn restore_proxy(&self, _o: &SystemProxyStatus) -> Result<(), SystemIntegrationError> {
                Ok(())
            }
        }
        let fs = crate::proxy::proxy_tests_helpers::MemFs::new();
        let mut c = SystemProxyController::new(
            RewritingOps {
                marker_fs: fs.clone(),
            },
            ProxyMarker::new(fs.clone(), "/marker.json"),
        );
        c.marker.write("127.0.0.1:8080", None); // 旧 marker

        c.ensure_cleared();
        // 新 marker（9999）必须存活 —— 它属于新会话。
        let cur = ProxyMarker::new(fs, "/marker.json").read();
        assert_eq!(
            cur.map(|m| m.our_host_port),
            Some("127.0.0.1:9999".to_string()),
            "不得删掉并发 enable 写的新 marker"
        );
    }

    #[test]
    fn ensure_cleared_is_idempotent() {
        // 幂等：多路终态并发/重复调用安全（第一次清了 marker → 后续门控 1 即返）。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:8080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);

        assert!(c.ensure_cleared());
        let calls_after_first = c.ops.calls.borrow().len();
        // 再调两次 → 不得再 disable。
        assert!(!c.ensure_cleared());
        assert!(!c.ensure_cleared());
        assert_eq!(
            c.ops.calls.borrow().len(),
            calls_after_first,
            "重复调用不得重复 disable"
        );
    }

    #[test]
    fn ensure_cleared_matches_by_host_when_socks_port_differs() {
        // mac：socks 端口 ≠ http 端口，而 marker 只记 address:http_port。
        // 仅按 host:port 精确匹配会漏判 socks 腿的残留 → 必须也认 host 匹配。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                socks_proxy: Some("127.0.0.1:1080".into()), // 端口与 marker 的 8080 不同
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);

        assert!(
            c.ensure_cleared(),
            "socks 端口不同但 host 相同 → 仍是我们的残留，必须清"
        );
        assert!(!c.has_marker());
    }

    #[test]
    fn ensure_cleared_ignores_disabled_status() {
        // 系统代理已关（enabled=false）→ 无需 disable，仅清失真 marker。
        let ops = MockOps::default(); // status 默认 enabled=false
        let mut c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);
        assert!(!c.ensure_cleared());
        assert!(!c.has_marker());
    }

    // ── detect_foreign_proxy（EVENT_SYSTEM_PROXY_RESIDUAL 的真值源）─────────────────

    #[test]
    fn detect_foreign_proxy_reports_others_proxy_when_no_marker() {
        // 无 marker（不是我们设的）+ 系统里确有启用的代理 → 报出其 host:port。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("192.168.1.2:7890".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let c = SystemProxyController::new(ops, mem_marker());
        assert_eq!(
            c.detect_foreign_proxy(),
            Some("192.168.1.2:7890".into()),
            "无 marker + 有代理 = 别人的残留，应报出"
        );
    }

    #[test]
    fn detect_foreign_proxy_none_when_marker_present() {
        // 有 marker = 系统代理是我们设的 → 不是「别人的」，绝不误报（该场景归 ensure_cleared）。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:8080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let c = SystemProxyController::new(ops, mem_marker());
        c.marker.write("127.0.0.1:8080", None);
        assert_eq!(
            c.detect_foreign_proxy(),
            None,
            "有 marker → 是我们设的，不算残留"
        );
        // 有 marker 时早退：连状态都不读（省一次 exec）。
        assert!(
            c.ops.calls.borrow().is_empty(),
            "有 marker 应门控 1 即返，不查状态"
        );
    }

    #[test]
    fn detect_foreign_proxy_none_when_no_proxy_set() {
        // 无 marker 且系统无代理 → None（干净环境不打扰用户）。
        let ops = MockOps::default(); // status 默认 enabled=false / 全空
        let c = SystemProxyController::new(ops, mem_marker());
        assert_eq!(c.detect_foreign_proxy(), None);
    }

    #[test]
    fn detect_foreign_proxy_none_when_server_present_but_disabled() {
        // 显式守卫：enabled=false 但 http_proxy 有值（Win 注册表 ProxyServer 在 ProxyEnable=0 时留值的形态）
        // → 不得误报。锁死 detect_foreign_proxy 里 `!status.enabled || ...` 的 enabled 判据。
        let ops = MockOps {
            status: RefCell::new(SystemProxyStatus {
                enabled: false,
                http_proxy: Some("10.0.0.9:1080".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let c = SystemProxyController::new(ops, mem_marker());
        assert_eq!(
            c.detect_foreign_proxy(),
            None,
            "enabled=false 的残留 server 值不算启用中的代理"
        );
    }

    #[test]
    fn points_to_us_unit() {
        let st = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("127.0.0.1:8080".into()),
            ..Default::default()
        };
        assert!(points_to_us(Some(&st), "127.0.0.1:8080"));
        assert!(points_to_us(Some(&st), "127.0.0.1:1080")); // host 匹配
        assert!(!points_to_us(Some(&st), "10.0.0.1:8080"));
        assert!(!points_to_us(None, "127.0.0.1:8080"));
        // 关着的代理不算指向我们。
        let off = SystemProxyStatus {
            enabled: false,
            http_proxy: Some("127.0.0.1:8080".into()),
            ..Default::default()
        };
        assert!(!points_to_us(Some(&off), "127.0.0.1:8080"));
    }
}

#[cfg(test)]
mod live_status_tests {
    //! 活态查询（「OS 代理是否仍指向本进程 mixed 入站」）三平台解析 + 判定。
    //!
    //! 全部经 [`ArgvMockRunner`] 注入命令输出 —— **不触碰宿主系统代理**（本机绝不真跑
    //! `networksetup`/`gsettings`/`reg`，更不改任何系统设置）。`with_platform` 让 Linux CI
    //! 同时跑通 mac/win 两套解析（本 crate 零 `#[cfg(target_os)]` 的既有纪律）。

    use super::*;
    use crate::exec::{CommandOutput, CommandRunner};
    use std::cell::RefCell;

    /// 按「argv 必须**同时**含全部指定项（**逐字相等**，非子串）」匹配 stdout 的 mock。
    ///
    /// 为什么不复用共享的 `exec_tests_helpers::MockRunner`（它按单个**子串**匹配）：
    /// 1. Linux gsettings 的读取键是 (schema, key) 二元组，单个子串区分不了「读 mode」与「读 http.host」；
    /// 2. **子串匹配在此处会串台成假绿**（实测踩到）——`org.gnome.system.proxy` 是
    ///    `org.gnome.system.proxy.http` 的前缀，后者又是 `...proxy.https` 的前缀，https 的读取会拿到
    ///    http 的桩输出。逐字相等把这两层前缀陷阱一并堵死。
    #[derive(Default)]
    struct ArgvMockRunner {
        rules: Vec<(Vec<&'static str>, String)>,
        fails: Vec<Vec<&'static str>>,
        calls: RefCell<Vec<Command>>,
    }

    impl ArgvMockRunner {
        fn on(mut self, needles: &[&'static str], stdout: impl Into<String>) -> Self {
            self.rules.push((needles.to_vec(), stdout.into()));
            self
        }
        /// argv 同时含这些项的调用直接失败（模拟 schema 不存在 / 命令缺失 / 无权限）。
        fn failing(mut self, needles: &[&'static str]) -> Self {
            self.fails.push(needles.to_vec());
            self
        }
        /// 是否跑过 argv 含该**逐字**参数的命令（同上：子串会让 `.http` 命中 `.https` 的调用）。
        fn ran_arg(&self, needle: &str) -> bool {
            self.calls
                .borrow()
                .iter()
                .any(|c| c.args.iter().any(|a| a == needle))
        }
    }

    impl CommandRunner for ArgvMockRunner {
        fn run(&self, cmd: &Command, _t: Duration) -> Result<CommandOutput, String> {
            self.calls.borrow_mut().push(cmd.clone());
            let hit = |ns: &[&str]| ns.iter().all(|n| cmd.args.iter().any(|a| a == n));
            if self.fails.iter().any(|ns| hit(ns)) {
                return Err("mock failure".into());
            }
            for (ns, out) in &self.rules {
                if hit(ns) {
                    return Ok(CommandOutput {
                        stdout: out.clone(),
                        stderr: String::new(),
                    });
                }
            }
            Ok(CommandOutput::default())
        }
    }

    /// 本进程 mixed 入站（全部用例的比对基准）。
    const OUR_ADDR: &str = "127.0.0.1";
    const OUR_PORT: u16 = 7890;

    fn live(
        runner: ArgvMockRunner,
        platform: Platform,
    ) -> (
        Result<SystemProxyLiveStatus, SystemIntegrationError>,
        SystemProxyOpsImpl<ArgvMockRunner>,
    ) {
        let ops = SystemProxyOpsImpl::with_platform(runner, platform);
        let r = ops.live_status(OUR_ADDR, OUR_PORT);
        (r, ops)
    }

    // ── 纯判定 `points_to_mixed_inbound`（三平台共用的唯一判据）─────────────────────────

    #[test]
    fn points_to_mixed_inbound_requires_enabled_exact_hostport_and_no_foreign_leg() {
        let ours = |legs: [Option<&str>; 3], enabled: bool| SystemProxyStatus {
            enabled,
            http_proxy: legs[0].map(str::to_string),
            https_proxy: legs[1].map(str::to_string),
            socks_proxy: legs[2].map(str::to_string),
            bypass_domains: None,
        };
        let ok = "127.0.0.1:7890";

        // 三腿全指向我们 → 生效。
        assert!(points_to_mixed_inbound(
            &ours([Some(ok), Some(ok), Some(ok)], true),
            OUR_ADDR,
            OUR_PORT
        ));
        // Windows 从不设 socks= → socks 为 None 不算「指向别处」。
        assert!(points_to_mixed_inbound(
            &ours([Some(ok), Some(ok), None], true),
            OUR_ADDR,
            OUR_PORT
        ));
        // enabled=false（注册表 ProxyEnable=0 仍留 ProxyServer 值的形态）→ 未生效。
        assert!(!points_to_mixed_inbound(
            &ours([Some(ok), Some(ok), Some(ok)], false),
            OUR_ADDR,
            OUR_PORT
        ));
        // 端口不匹配 → 未生效（**别只比 host**，见函数文档第 2 条）。
        assert!(!points_to_mixed_inbound(
            &ours([Some("127.0.0.1:9999"), None, None], true),
            OUR_ADDR,
            OUR_PORT
        ));
        // 指向别的代理 → 未生效。
        assert!(!points_to_mixed_inbound(
            &ours([Some("proxy.corp:3128"), None, None], true),
            OUR_ADDR,
            OUR_PORT
        ));
        // 一腿指向我们、另一腿被改到别处 → 该协议绕开本地核 → 整体未生效。
        assert!(!points_to_mixed_inbound(
            &ours([Some(ok), Some("proxy.corp:3128"), None], true),
            OUR_ADDR,
            OUR_PORT
        ));
        // 三腿全空（enabled 但没有实际服务器）→ 无「指向我们」的证据 → 未生效。
        assert!(!points_to_mixed_inbound(
            &ours([None, None, None], true),
            OUR_ADDR,
            OUR_PORT
        ));
    }

    // ── macOS：networksetup -getwebproxy / -getsecurewebproxy / -getsocksfirewallproxy ──

    const MAC_SERVICES: &str =
        "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\nEthernet\n";

    fn mac_on(server: &str, port: u16) -> String {
        format!("Enabled: Yes\nServer: {server}\nPort: {port}\nAuthenticated Proxy Enabled: 0\n")
    }
    const MAC_OFF: &str = "Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n";

    fn mac_runner(http: String, https: String, socks: String) -> ArgvMockRunner {
        ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES)
            .on(&["-getwebproxy"], http)
            .on(&["-getsecurewebproxy"], https)
            .on(&["-getsocksfirewallproxy"], socks)
    }

    #[test]
    fn mac_live_status_effective_when_all_legs_point_at_mixed_inbound() {
        let (r, _) = live(
            mac_runner(
                mac_on("127.0.0.1", 7890),
                mac_on("127.0.0.1", 7890),
                mac_on("127.0.0.1", 7890),
            ),
            Platform::Mac,
        );
        let s = r.expect("读取成功");
        assert!(s.points_to_us);
        assert_eq!(s.expected, "127.0.0.1:7890");
        assert_eq!(s.status.http_proxy.as_deref(), Some("127.0.0.1:7890"));
    }

    #[test]
    fn mac_live_status_not_effective_when_user_turned_proxy_off() {
        // 形态①「未开启」：运行期用户在「系统设置 › 网络 › 代理」里把开关关掉 —— 起核那一刻是成功的、
        // `SYSTEM_PROXY_FAILED` 干净，只有活态查询能看见。
        let (r, _) = live(
            mac_runner(MAC_OFF.into(), MAC_OFF.into(), MAC_OFF.into()),
            Platform::Mac,
        );
        let s = r.expect("读取成功");
        assert!(!s.points_to_us, "代理已关 → 未生效");
        assert!(!s.status.enabled);
    }

    #[test]
    fn mac_live_status_not_effective_when_pointing_at_another_proxy() {
        // 形态②「指向别的代理」：开着，但指向第三方 → 我们的流量同样没走本地核。
        let (r, _) = live(
            mac_runner(
                mac_on("proxy.corp", 3128),
                mac_on("proxy.corp", 3128),
                MAC_OFF.into(),
            ),
            Platform::Mac,
        );
        let s = r.expect("读取成功");
        assert!(!s.points_to_us, "指向第三方代理 → 未生效");
        assert!(s.status.enabled, "OS 层确实开着（只是不指向我们）");
    }

    /// **变异锁（比对端口）**：把 [`points_to_mixed_inbound`] 里的 `*p == ours` 改成只比 host
    /// （如 `p.split(':').next() == Some(address)`），本用例立刻转红 —— `127.0.0.1:9999` 会被
    /// 判成「仍指向我们」，而那是另一个本地代理软件 / 用户手改端口，流量根本不到我们的 mixed 口。
    #[test]
    fn mac_live_status_rejects_port_mismatch() {
        let (r, _) = live(
            mac_runner(
                mac_on("127.0.0.1", 9999),
                mac_on("127.0.0.1", 9999),
                mac_on("127.0.0.1", 9999),
            ),
            Platform::Mac,
        );
        let s = r.expect("读取成功");
        assert!(
            !s.points_to_us,
            "host 对但端口不是我们的 mixed 口 → 必须判未生效"
        );
    }

    #[test]
    fn mac_live_status_reads_only_one_service() {
        // 活态口径 = **单个**在用服务（不是全部）。扫全部服务会在「主服务代理被关、闲置服务
        // 留着指向我们的残值」时谎报「仍生效」。本例无 route 桩 → 回落 services[0] = Wi-Fi。
        let (_, ops) = live(
            mac_runner(
                mac_on("127.0.0.1", 7890),
                mac_on("127.0.0.1", 7890),
                mac_on("127.0.0.1", 7890),
            ),
            Platform::Mac,
        );
        assert!(ops.runner.ran_arg("Wi-Fi"), "须读在用服务");
        assert!(
            !ops.runner.ran_arg("Ethernet"),
            "活态查询不得扫非在用服务（那是 get_proxy_status 的残留检测口径）"
        );
    }

    // ── macOS primary service（默认路由 → 服务名）────────────────────────────────────

    /// `-listallnetworkservices`：**雷电桥排在 Wi-Fi 前**、且都不带 `*`（未插线 ≠ 停用）。
    const MAC_SERVICES_BRIDGE_FIRST: &str =
        "An asterisk (*) denotes that a network service is disabled.\nThunderbolt Bridge\nWi-Fi\n";

    /// `-listnetworkserviceorder`：服务名 ↔ BSD 设备名的成对输出（真机格式）。
    const MAC_SERVICE_ORDER: &str = "An asterisk (*) denotes that a network service is disabled.\n\
        \n(1) Thunderbolt Bridge\n(Hardware Port: Thunderbolt Bridge, Device: bridge0)\n\
        \n(2) Wi-Fi\n(Hardware Port: Wi-Fi, Device: en0)\n\
        \n(3) *Old Ethernet\n(Hardware Port: Ethernet, Device: en5)\n";

    fn mac_route(device: &str) -> String {
        format!(
            "   route to: default\ndestination: default\n       mask: default\n\
             gateway: 192.168.1.1\n  interface: {device}\n      flags: <UP,GATEWAY,DONE>\n"
        )
    }

    #[test]
    fn parse_mac_service_order_pairs_names_with_devices_and_drops_disabled() {
        let got = parse_mac_service_order(MAC_SERVICE_ORDER);
        assert_eq!(
            got,
            vec![
                ("Thunderbolt Bridge".to_string(), "bridge0".to_string()),
                ("Wi-Fi".to_string(), "en0".to_string()),
            ],
            "停用服务（`(3) *Old Ethernet`）不得进映射：它不承载流量"
        );
        // 无设备名的条目（部分 VPN 服务）不得进映射，也不得让解析崩掉。
        assert!(parse_mac_service_order("(1) VPN\n(Hardware Port: VPN)\n").is_empty());
        assert!(parse_mac_service_order("").is_empty());
    }

    /// 默认路由行的解析复用 `route_ops` 那一份；此处只钉「`route -n get default` 的真机输出形态
    /// 确实能被它吃下」（两模块共用同一解析器 → 不再有第二份会漂移的实现）。
    #[test]
    fn default_route_output_is_parsed_by_the_shared_route_parser() {
        assert_eq!(
            crate::route_ops::parse_mac_route_get_interface(&mac_route("en0")).as_deref(),
            Some("en0")
        );
        // 无默认路由（`route: writing to routing socket: not in table`）→ None，不是 panic。
        assert_eq!(
            crate::route_ops::parse_mac_route_get_interface(
                "   route: writing to routing socket: not in table\n"
            ),
            None
        );
    }

    /// **变异锁（本条 review 的核心）**：把 `read_active_proxy` 的 Mac 分支改回
    /// `list_network_services()?.first()` → 本用例立刻转红。
    ///
    /// 场景 = reviewer 给的恶性样例：雷电桥（未插线、不带 `*`）排在 `-listallnetworkservices` 首位，
    /// 流量实际走 Wi-Fi；用户在 **Wi-Fi** 上手关了代理，而雷电桥上还留着我们 enable 时写的值
    /// （`set_proxy` 写全部服务）。读首项 → `points_to_us=true` → **漏报**（绿灯 + 明文直连）。
    #[test]
    fn mac_live_status_follows_default_route_not_service_list_order() {
        let runner = ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES_BRIDGE_FIRST)
            .on(&["-listnetworkserviceorder"], MAC_SERVICE_ORDER)
            .on(&["default"], mac_route("en0"))
            // 雷电桥（首项）上留着指向我们的残值；Wi-Fi（主服务）上用户已手关。
            .on(
                &["-getwebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsecurewebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsocksfirewallproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(&["-getwebproxy", "Wi-Fi"], MAC_OFF)
            .on(&["-getsecurewebproxy", "Wi-Fi"], MAC_OFF)
            .on(&["-getsocksfirewallproxy", "Wi-Fi"], MAC_OFF);
        let (r, ops) = live(runner, Platform::Mac);
        let s = r.expect("读取成功");
        assert!(
            !s.points_to_us,
            "主服务（Wi-Fi，默认路由 en0）上代理已关 → 必须判未生效；读首项会漏报成「仍生效」"
        );
        assert!(ops.runner.ran_arg("Wi-Fi"), "须读默认路由所属服务");
        assert!(
            !ops.runner.ran_arg("Thunderbolt Bridge"),
            "不得去读非承载流量的服务"
        );
    }

    /// 反向：主服务上确实指向我们，而排在前面的闲置服务没设 —— 读首项会**误亮黄灯**。
    #[test]
    fn mac_live_status_effective_when_only_the_primary_service_points_at_us() {
        let runner = ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES_BRIDGE_FIRST)
            .on(&["-listnetworkserviceorder"], MAC_SERVICE_ORDER)
            .on(&["default"], mac_route("en0"))
            .on(&["-getwebproxy", "Wi-Fi"], mac_on("127.0.0.1", 7890))
            .on(&["-getsecurewebproxy", "Wi-Fi"], mac_on("127.0.0.1", 7890))
            .on(
                &["-getsocksfirewallproxy", "Wi-Fi"],
                mac_on("127.0.0.1", 7890),
            );
        let s = live(runner, Platform::Mac).0.expect("读取成功");
        assert!(
            s.points_to_us,
            "主服务指向我们 → 生效（读首项会误报未生效）"
        );
    }

    /// 查不到主服务（无默认路由 / `route` 不可用）→ **回落 `services[0]`**，不升级成 Err。
    /// 回落是改动前的行为，不比它差；把「查不到主服务」升成查询失败会平白多一路黄灯。
    #[test]
    fn mac_live_status_falls_back_to_first_service_when_primary_unresolvable() {
        let runner = ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES_BRIDGE_FIRST)
            .on(&["-listnetworkserviceorder"], MAC_SERVICE_ORDER)
            .failing(&["default"]) // route 不可用
            .on(
                &["-getwebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsecurewebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsocksfirewallproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            );
        let (r, ops) = live(runner, Platform::Mac);
        assert!(r.expect("读取成功").points_to_us, "回落首项后照常判定");
        assert!(ops.runner.ran_arg("Thunderbolt Bridge"), "回落读首项");

        // 设备名映射不上（默认路由走 utun3，服务顺序表里没有）→ 同样回落，不 Err。
        let runner = ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES_BRIDGE_FIRST)
            .on(&["-listnetworkserviceorder"], MAC_SERVICE_ORDER)
            .on(&["default"], mac_route("utun3"))
            .on(
                &["-getwebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsecurewebproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            )
            .on(
                &["-getsocksfirewallproxy", "Thunderbolt Bridge"],
                mac_on("127.0.0.1", 7890),
            );
        let (r, ops) = live(runner, Platform::Mac);
        assert!(r.expect("读取成功").points_to_us);
        assert!(
            ops.runner.ran_arg("Thunderbolt Bridge"),
            "映射不上也回落首项"
        );
    }

    #[test]
    fn mac_live_status_read_failure_is_err_not_false() {
        // **读不到 ≠ 没生效**：折成「未生效」会在读取受阻的环境里稳定误亮降级黄灯。
        let runner = ArgvMockRunner::default()
            .on(&["-listallnetworkservices"], MAC_SERVICES)
            .failing(&["-getwebproxy"]);
        let (r, _) = live(runner, Platform::Mac);
        assert!(r.is_err(), "读失败必须出栈为 Err（由上层折成「未知」）");
    }

    // ── Windows：reg query Internet Settings ────────────────────────────────────────

    const WIN_ON: &str =
        "\r\nHKEY_CURRENT_USER\\...\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n";
    const WIN_OFF: &str = "\r\n    ProxyEnable    REG_DWORD    0x0\r\n";

    fn win_server(value: &str) -> String {
        format!("\r\n    ProxyServer    REG_SZ    {value}\r\n")
    }

    #[test]
    fn win_live_status_effective_when_registry_points_at_mixed_inbound() {
        let runner = ArgvMockRunner::default().on(&["ProxyEnable"], WIN_ON).on(
            &["ProxyServer"],
            win_server("http=127.0.0.1:7890;https=127.0.0.1:7890"),
        );
        let s = live(runner, Platform::Win).0.expect("读取成功");
        assert!(s.points_to_us);
        // 我们从不设 socks=（Chromium 经 SOCKS5 本地解析 DNS 会被污染）→ 该腿 None，不影响判定。
        assert_eq!(s.status.socks_proxy, None);
    }

    #[test]
    fn win_live_status_not_effective_when_proxy_enable_is_zero() {
        // 形态①「未开启」：ProxyEnable=0 —— 注意 ProxyServer 值仍留在注册表里，只看串会误判。
        let runner = ArgvMockRunner::default().on(&["ProxyEnable"], WIN_OFF).on(
            &["ProxyServer"],
            win_server("http=127.0.0.1:7890;https=127.0.0.1:7890"),
        );
        let s = live(runner, Platform::Win).0.expect("读取成功");
        assert!(
            !s.points_to_us,
            "ProxyEnable=0 → 未生效（残留 server 值不算数）"
        );
    }

    #[test]
    fn win_live_status_not_effective_when_pointing_at_another_proxy() {
        let runner = ArgvMockRunner::default().on(&["ProxyEnable"], WIN_ON).on(
            &["ProxyServer"],
            win_server("http=proxy.corp:3128;https=proxy.corp:3128"),
        );
        let s = live(runner, Platform::Win).0.expect("读取成功");
        assert!(!s.points_to_us);
        assert!(s.status.enabled);
    }

    /// 变异锁（比对端口）的 Windows 腿。
    #[test]
    fn win_live_status_rejects_port_mismatch() {
        let runner = ArgvMockRunner::default().on(&["ProxyEnable"], WIN_ON).on(
            &["ProxyServer"],
            win_server("http=127.0.0.1:9999;https=127.0.0.1:9999"),
        );
        let s = live(runner, Platform::Win).0.expect("读取成功");
        assert!(!s.points_to_us, "端口不匹配 → 必须判未生效");
    }

    #[test]
    fn win_live_status_read_failure_is_err_not_false() {
        let runner = ArgvMockRunner::default().failing(&["ProxyEnable"]);
        assert!(live(runner, Platform::Win).0.is_err());
    }

    // ── Linux：gsettings org.gnome.system.proxy ─────────────────────────────────────

    fn linux_runner(
        mode: &str,
        http: (&str, u16),
        https: (&str, u16),
        socks: (&str, u16),
    ) -> ArgvMockRunner {
        let mut r = ArgvMockRunner::default()
            .on(&["org.gnome.system.proxy", "mode"], format!("'{mode}'\n"));
        for (schema, (host, port)) in [("http", http), ("https", https), ("socks", socks)] {
            let base: &'static str = match schema {
                "http" => "org.gnome.system.proxy.http",
                "https" => "org.gnome.system.proxy.https",
                _ => "org.gnome.system.proxy.socks",
            };
            r = r
                .on(&[base, "host"], format!("'{host}'\n"))
                // GVariant 前缀必须能被剥掉（`uint32 7890`），否则端口恒解析失败。
                .on(&[base, "port"], format!("uint32 {port}\n"));
        }
        r
    }

    #[test]
    fn linux_live_status_effective_when_gsettings_points_at_mixed_inbound() {
        let s = live(
            linux_runner(
                "manual",
                ("127.0.0.1", 7890),
                ("127.0.0.1", 7890),
                ("127.0.0.1", 7890),
            ),
            Platform::Linux,
        )
        .0
        .expect("读取成功");
        assert!(s.points_to_us);
        assert_eq!(s.status.http_proxy.as_deref(), Some("127.0.0.1:7890"));
    }

    /// 形态①「未开启」的 Linux 形态 —— 并且是**只读 host/port 抓不到**的那一种：
    /// 用户把 mode 改回 `none`，三个 schema 的 host/port **残值仍在**。
    /// 变异锁：删掉 `read_active_proxy` 里的 mode 闸门 → 残值会被判成「仍指向我们」→ 本用例转红。
    #[test]
    fn linux_live_status_not_effective_when_mode_is_none_despite_residual_host() {
        let (r, ops) = live(
            linux_runner(
                "none",
                ("127.0.0.1", 7890),
                ("127.0.0.1", 7890),
                ("127.0.0.1", 7890),
            ),
            Platform::Linux,
        );
        let s = r.expect("读取成功");
        assert!(!s.points_to_us, "mode=none → GNOME 不下发代理 → 未生效");
        assert!(!s.status.enabled);
        assert!(
            !ops.runner.ran_arg("org.gnome.system.proxy.http"),
            "mode 非 manual 即早退，不必再读三 schema"
        );
    }

    #[test]
    fn linux_live_status_not_effective_when_pointing_at_another_proxy() {
        // http 指向我们、https 被改到第三方 → 该协议绕开本地核 → 整体未生效。
        let s = live(
            linux_runner(
                "manual",
                ("127.0.0.1", 7890),
                ("proxy.corp", 3128),
                ("127.0.0.1", 7890),
            ),
            Platform::Linux,
        )
        .0
        .expect("读取成功");
        assert!(!s.points_to_us);
        assert_eq!(s.status.https_proxy.as_deref(), Some("proxy.corp:3128"));
    }

    /// 变异锁（比对端口）的 Linux 腿。
    #[test]
    fn linux_live_status_rejects_port_mismatch() {
        let s = live(
            linux_runner(
                "manual",
                ("127.0.0.1", 9999),
                ("127.0.0.1", 9999),
                ("127.0.0.1", 9999),
            ),
            Platform::Linux,
        )
        .0
        .expect("读取成功");
        assert!(!s.points_to_us, "端口不匹配 → 必须判未生效");
    }

    #[test]
    fn linux_live_status_read_failure_is_err_not_false() {
        // 非 GNOME 桌面：`gsettings get org.gnome.system.proxy mode` 报「无此 schema」。
        let runner = ArgvMockRunner::default().failing(&["org.gnome.system.proxy", "mode"]);
        assert!(
            live(runner, Platform::Linux).0.is_err(),
            "读不到 ≠ 没生效：必须 Err，否则非 GNOME 环境恒亮降级黄灯"
        );
    }

    #[test]
    fn other_platform_is_err() {
        assert!(live(ArgvMockRunner::default(), Platform::Other).0.is_err());
    }

    #[test]
    fn parse_gsettings_mode_strips_quotes() {
        assert_eq!(parse_gsettings_mode("'manual'\n"), "manual");
        assert_eq!(parse_gsettings_mode("  'none' \n"), "none");
        assert_eq!(parse_gsettings_mode(""), "");
    }
}

#[cfg(test)]
mod mac_service_enum_tests {
    //! mac「该接管哪些网络服务」的口径门。
    //!
    //! 缺陷来历（2026-08-08，p101 只读取证）：旧口径按**名字**过滤（跳 `*` 停用 + 跳含 Bluetooth 的），
    //! 于是 7 个网络服务全被接管改写成 `8.8.8.8` —— 其中 `Tailscale` 与 `Shadowrocket` 是**别家 VPN**
    //! 由 NetworkExtension 提供的服务。我们不但覆盖了它们的解析器，还把还原责任揽到自己的 marker 上。
    //! 系统代理侧同型（两处此前共用同一个按名字过滤的解析器）。
    //!
    //! 夹具是那台机器 `-listnetworkserviceorder` 的**真实输出**，不是手搓的理想形状。

    use super::*;
    use crate::exec::exec_tests_helpers::MockRunner;
    use crate::exec::CommandRunner;
    use std::time::Duration;

    /// p101 实测输出（2026-08-08）。五个物理服务带 `en*`/`bridge0`，两个 VPN 服务 **Device 为空**。
    const ORDER_REAL: &str = "An asterisk (*) denotes that a network service is disabled.\n\
(1) USB 10/100/1G/2.5G LAN\n\
(Hardware Port: USB 10/100/1G/2.5G LAN, Device: en7)\n\
\n\
(2) F50 Pro\n\
(Hardware Port: F50 Pro, Device: en9)\n\
\n\
(3) USB 10/100/1000 LAN\n\
(Hardware Port: USB 10/100/1000 LAN, Device: en11)\n\
\n\
(4) Thunderbolt Bridge\n\
(Hardware Port: Thunderbolt Bridge, Device: bridge0)\n\
\n\
(5) Wi-Fi\n\
(Hardware Port: Wi-Fi, Device: en0)\n\
\n\
(6) Shadowrocket\n\
(Hardware Port: com.liguangming.Shadowrocket, Device: )\n\
\n\
(7) Tailscale\n\
(Hardware Port: io.tailscale.ipn.macsys, Device: )\n";

    fn enumerate(runner: &MockRunner) -> Vec<String> {
        mac_list_manageable_services(|c| {
            runner
                .run(c, Duration::from_secs(5))
                .map_err(SystemIntegrationError::proxy)
        })
        .expect("枚举不应失败")
    }

    #[test]
    fn real_output_drops_vpn_services_and_keeps_every_physical_one() {
        let runner = MockRunner::default().with_arg_stdout("-listnetworkserviceorder", ORDER_REAL);
        let got = enumerate(&runner);

        // 正向：五个物理服务一个不少。漏掉任何一个 = 该网卡 DNS 不被接管 = 泄漏，比误接管严重。
        assert_eq!(
            got,
            vec![
                "USB 10/100/1G/2.5G LAN",
                "F50 Pro",
                "USB 10/100/1000 LAN",
                "Thunderbolt Bridge",
                "Wi-Fi",
            ],
            "物理服务必须全部保留且保持顺序"
        );
        // 反向：两个 VPN 服务一个不进。
        assert!(!got.iter().any(|s| s == "Tailscale"), "不得接管 Tailscale");
        assert!(
            !got.iter().any(|s| s == "Shadowrocket"),
            "不得接管 Shadowrocket"
        );
    }

    #[test]
    fn never_falls_back_when_service_order_parses() {
        // 负向对照：否则「结果看着对」也可能是因为一直在跑旧口径。
        let runner = MockRunner::default().with_arg_stdout("-listnetworkserviceorder", ORDER_REAL);
        let _ = enumerate(&runner);
        assert!(
            !runner.ran_arg("-listallnetworkservices"),
            "顺序命令解析成功时不得再跑 -listallnetworkservices"
        );
    }

    #[test]
    fn disabled_and_bluetooth_services_excluded() {
        let order = "An asterisk (*) denotes that a network service is disabled.\n\
(1) Wi-Fi\n\
(Hardware Port: Wi-Fi, Device: en0)\n\
\n\
(2) *Ethernet\n\
(Hardware Port: Ethernet, Device: en4)\n\
\n\
(3) Bluetooth PAN\n\
(Hardware Port: Bluetooth PAN, Device: en5)\n";
        let runner = MockRunner::default().with_arg_stdout("-listnetworkserviceorder", order);
        assert_eq!(enumerate(&runner), vec!["Wi-Fi"]);
    }

    #[test]
    fn falls_back_to_legacy_enumeration_when_nothing_has_a_device() {
        // 防「未来 macOS 改输出形态 ⇒ 过滤后全空 ⇒ 一个服务都不接管 ⇒ 全量泄漏」。
        let runner = MockRunner::default()
            .with_arg_stdout("-listnetworkserviceorder", "totally unexpected output\n")
            .with_arg_stdout(
                "-listallnetworkservices",
                "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*Ethernet\n",
            );
        let got = enumerate(&runner);
        assert_eq!(got, vec!["Wi-Fi"], "回落后应拿到旧口径结果");
        assert!(
            runner.ran_arg("-listallnetworkservices"),
            "回落必须真的去跑旧命令"
        );
    }

    #[test]
    fn dns_takeover_and_system_proxy_share_one_enumeration() {
        // 这两处此前共用的是**按名字过滤**的解析器，于是同一个缺陷有两个面。
        // 判据落在「调用了哪个函数」，而不是「文件里出现过某个词」—— 后者会被注释骗过去（本仓踩过）。
        let dir = env!("CARGO_MANIFEST_DIR");
        // 🔴 **必须先切掉测试区再判**：本断言的字面量自己就住在 `proxy_ops.rs` 里，
        // 不切的话把生产调用点整个删掉、断言字符串留下，这条照样绿 —— 实测如此（M2 变异第一次没红）。
        // 与前一天 `ci_step_still_wired` 被自己的注释骗过是同一形状：
        // **源码级判据必须先把「判据自身所在的区域」排除掉。**
        let production_src = |file: &str| -> String {
            let full = std::fs::read_to_string(format!("{dir}/{file}"))
                .unwrap_or_else(|e| panic!("读不到 {file}: {e}"));
            match full.find("\n#[cfg(test)]") {
                Some(i) => full[..i].to_string(),
                None => full,
            }
        };
        for (file, what) in [
            ("src/dns_ops.rs", "DNS 接管"),
            ("src/proxy_ops.rs", "系统代理"),
        ] {
            let src = production_src(file);
            assert!(
                src.contains("mac_list_manageable_services(|c| self.run(c))"),
                "{what}（{file}）没有走统一口径函数"
            );
        }
    }

    // ── bypass 清单的捕获与还原（2026-08-09）──────────────────────────────────

    /// `-getproxybypassdomains` 的两种输出形态必须可分辨：真条目 vs 「一条都没有」的英文提示句。
    #[test]
    fn mac_bypass_parse_separates_entries_from_the_empty_notice() {
        // 有条目：每行一个，含域名 / 通配 / CIDR 三种形态。
        let listed = "intranet.corp.com\n*.local\n192.168.0.0/16\n";
        assert_eq!(
            parse_mac_bypass_domains(listed),
            vec!["intranet.corp.com", "*.local", "192.168.0.0/16"]
        );

        // 空清单：networksetup 回一句英文提示 —— **绝不能**被当成一个条目写回去。
        let empty = "There aren't any bypass domains set on Wi-Fi.\n";
        assert!(
            parse_mac_bypass_domains(empty).is_empty(),
            "提示句被当成 bypass 条目了"
        );
        // 提示文案随系统版本变，判据不能锚在英文原文上 —— 换一句同样得判空。
        assert!(parse_mac_bypass_domains("No bypass domains configured\n").is_empty());

        assert!(parse_mac_bypass_domains("").is_empty());
    }

    /// restore 必须把 bypass 写回原值；**没捕获过**（None）时一个字都不能碰。
    #[test]
    fn mac_restore_writes_bypass_back_only_when_captured() {
        let base = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("h:80".into()),
            https_proxy: None,
            socks_proxy: None,
            bypass_domains: None,
        };

        // ① 没捕获过 → 不得出现任何 bypass 写命令（读失败折成「清空」比不还原更糟）。
        let cmds = mac_service_restore_commands("Wi-Fi", &base);
        assert!(
            !cmds
                .iter()
                .any(|c| c.args.iter().any(|a| a == "-setproxybypassdomains")),
            "没捕获到原值却去写 bypass —— 会把用户的清单清掉"
        );

        // ② 捕获到条目 → 原样写回（顺序保持）。
        let with = SystemProxyStatus {
            bypass_domains: Some(vec!["intranet.corp.com".into(), "*.local".into()]),
            ..base.clone()
        };
        let cmds = mac_service_restore_commands("Wi-Fi", &with);
        let bypass = cmds
            .iter()
            .find(|c| c.args.first().map(String::as_str) == Some("-setproxybypassdomains"))
            .expect("捕获到了却没写回");
        assert_eq!(
            bypass.args,
            vec![
                "-setproxybypassdomains",
                "Wi-Fi",
                "intranet.corp.com",
                "*.local"
            ]
        );

        // ③ 捕获到空 → 必须写 Empty 哨兵（什么都不传会被 networksetup 判参数不足）。
        let empty = SystemProxyStatus {
            bypass_domains: Some(vec![]),
            ..base
        };
        let cmds = mac_service_restore_commands("Wi-Fi", &empty);
        let bypass = cmds
            .iter()
            .find(|c| c.args.first().map(String::as_str) == Some("-setproxybypassdomains"))
            .expect("捕获到空清单也要写回（清空）");
        assert_eq!(
            bypass.args,
            vec!["-setproxybypassdomains", "Wi-Fi", "Empty"]
        );
    }

    /// enable 写了 bypass、restore 就必须能写回 —— 两侧的子命令必须成对存在。
    ///
    /// 这条守的是「只写不撤」这个**形状**本身，比逐条断言参数更难被绕过：
    /// 谁把 restore 那条删掉，或者给 enable 加一条新的「只写不撤」的 set 子命令，都会红。
    #[test]
    fn every_mac_set_subcommand_has_a_restore_counterpart() {
        let req = ProxyEnableRequest {
            address: "127.0.0.1".into(),
            http_port: 7890,
            socks_port: 7890,
            bypass_list: vec!["10.0.0.0/8".into()],
        };
        let enable = mac_service_enable_commands("Wi-Fi", &req);
        let captured = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("h:80".into()),
            https_proxy: Some("h:80".into()),
            socks_proxy: Some("h:80".into()),
            bypass_domains: Some(vec!["x.corp".into()]),
        };
        let restore = mac_service_restore_commands("Wi-Fi", &captured);

        let subs = |cmds: &[Command]| -> std::collections::BTreeSet<String> {
            cmds.iter()
                .filter_map(|c| c.args.first().cloned())
                .filter(|a| a.starts_with("-set"))
                .map(|a| a.trim_end_matches("state").to_owned())
                .collect()
        };
        let enabled_subs = subs(&enable);
        let restore_subs = subs(&restore);
        assert!(!enabled_subs.is_empty(), "enable 一条 set 都没有？判据失效");
        for sub in &enabled_subs {
            assert!(
                restore_subs.contains(sub),
                "enable 下发了 `{sub}` 却没有对应的还原 —— 这正是 bypass 当初漏掉的形状\n\
                 enable={enabled_subs:?}\nrestore={restore_subs:?}"
            );
        }
    }
}
