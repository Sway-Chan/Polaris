//! wintun 适配器**存在性**探测（#327）。
//!
//! ## 地雷背景
//!
//! 就绪门（`core-supervisor::wait_for_core_ready`）的判据只有「管理 API 端口可连 + 进程活」，两条都与
//! TUN 网卡无关 —— 于是「sing-box 活着、mixed 代理正常、wintun 网卡从未创建」这一态会被判成起核成功，
//! 用户看到「已连接」而 TUN 根本不生效。
//!
//! 本模块给出那条缺失的**正向**判据 [`probe_adapter_present`]：有界轮询枚举，看见本次配置的那张适配器
//! 即成立；超时则回报「没建出来」（上层据此把本条起核腿计入重试预算）。
//!
//! ## 为什么**没有**反向的「起核前等上一张释放」
//!
//! 曾经有（#159：Windows session-0 服务模式下停核走 `TerminateProcess` 硬杀，可能残留 `polaris-tun0`，
//! 故起核前有界轮询等它消失）。**已删**，根因是那条腿要解决的问题内核自己就解决了：sing-tun
//! `tun_windows.go` 的 `New()` 里 `wintun.CreateAdapter` 撞 `os.ErrExist` 会立刻 `OpenAdapter` **复用**
//! 同名网卡，残留适配器压根不构成起核失败。那条腿的四个分支也无一阻断起核（全 fail-open 只记日志），
//! 唯一的实际效果是在残留时把起核热路径白拖最多 3s。
//!
//! ## 与 app 侧 `verify_tun_route_captured`（出口归属差分）**不重叠**
//!
//! 那条问「默认路由被谁占」，答不了「适配器压根没建出来」（网卡不存在时默认路由当然也没切走，但两者的
//! 用户动作完全不同 —— 一个是「断开别的 VPN」，一个是「wintun 驱动/权限有问题」）。
//!
//! ## 生产接线点在 app 侧，不在 helper 侧
//!
//! **本模块 Polaris 自有增强，Go helper-win 无对应**（Go 靠 sing-box 核自管 wintun TUN，无适配器探测）。
//!
//! `probe_adapter_present` 的生产调用点是 **app 侧** `src-tauri/src/runtime/proxy.rs` 的
//! `ProxyRuntime::probe_tun_adapter_present`，挂在**起核重试循环内**的就绪判定之后（每条腿验一次；健康
//! 路径首次枚举即命中，零 sleep）。逐腿而非循环外单次的理由、以及「不可断言就放行」的纪律见该函数文档。
//!
//! **helper 侧 [`crate::platform::windows::winproc::WinProcOps::start_singbox`] 仍不调用本模块**，且这是
//! 有意的：`GetAdaptersAddresses` 是免提权普通用户 API，app 进程直调即可；为它加一条 helper wire 命令
//! 只会凭空多出「协议版本 × 已部署旧 helper 不识别」的兼容面，换不来任何能力。
//!
//! ## 不触碰宿主
//!
//! 适配器枚举经 [`AdapterProbe`] trait 抽象：生产侧由 Windows IP Helper API 实现（`#[cfg(windows)]`，
//! `GetAdaptersAddresses`），测试用 [`MockAdapterProbe`]（纯内存，Linux 可测轮询骨架）。

use std::time::{Duration, Instant};

/// 把 [`std::net::Ipv4Addr`] 写成 WinSock `IN_ADDR.S_addr` 的内存序。
///
/// `S_addr` 虽然在 Rust 侧是 `u32`，WinSock 读的是这 4 个字节的**网络序**。用 `from_ne_bytes`
/// 保证字段在大小端机器上的内存字节都逐字等于 IPv4 octets；直接 `u32::from_be_bytes` 在 Windows
/// 小端机上会把 `1.2.3.4` 写成 `4.3.2.1`。
#[must_use]
fn ipv4_s_addr(ip: std::net::Ipv4Addr) -> u32 {
    u32::from_ne_bytes(ip.octets())
}

/// 探测的目标适配器名前缀（Polaris 命名谱系）。
///
/// - `polaris-`：Polaris 自身命名谱系（`polaris-tun0` 等）。
pub const PROBE_PREFIXES: &[&str] = &["polaris-"];

/// 默认探测超时（有界轮询，宁可超时回报也不无限阻塞 start）。
///
/// 选 3s：就绪门过后内核挂上 wintun 网卡通常 <1s，3s 给驱动的异步挂载留余量；超时则回报
/// [`PresenceOutcome::Absent`]（上层计入起核重试预算，不把 start 主路径拖太久）。
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(3000);

/// 默认轮询间隔（生产 200ms，测试可设更短）。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 适配器探测抽象（trait 便于测试 mock；生产用 Windows IP Helper API）。
///
/// 返回当前存在的、匹配 [`PROBE_PREFIXES`] 之一的适配器名列表。
pub trait AdapterProbe: Send + Sync {
    /// 枚举当前匹配前缀的适配器名。空 = 本机眼下没有任何 Polaris 谱系适配器。
    fn list_matching_adapters(&self) -> std::io::Result<Vec<String>>;
}

/// wintun 适配器**存在性**探测结果（#327）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceOutcome {
    /// 目标适配器已出现（在超时内枚举到同名项）。
    Present,
    /// 超时仍未见到目标适配器；`seen` 为最后一次枚举到的同前缀适配器名（可能为空）。
    Absent { seen: Vec<String> },
    /// 枚举出错（上层按「不可断言」处理：**不闸**，宁可漏检也不误拦正常起核）。
    Error(String),
}

/// 适配器名是否落在 [`PROBE_PREFIXES`] 的可枚举面内（**枚举实现与调用方共用的唯一判据**）。
///
/// # 为什么必须导出给调用方，而不是只在枚举实现里内联
///
/// [`AdapterProbe::list_matching_adapters`] 只返回前缀命中的适配器 —— 这是设计（不枚举用户的物理网卡）。
/// 于是「用户把 TUN 接口名改成 `my-tun`」时，本模块**永远**看不到那张网卡，存在性探测会恒返
/// [`PresenceOutcome::Absent`]。若调用方据此判「网卡没建出来」，后果是把一次**完全正常**的起核判成失败
/// 并杀核 —— 比它要修的缺陷严重得多。故调用方必须先用本谓词判「这个名字我方到底看不看得见」，
/// 看不见就整条跳过（不可断言）。把判据留在本模块 = 前缀表改动时两侧同步，不产生第二真值源。
#[must_use]
pub fn adapter_name_is_probeable(name: &str) -> bool {
    PROBE_PREFIXES.iter().any(|pfx| name.starts_with(pfx))
}

/// 有界轮询骨架（#327）。
///
/// 返回 `Ok((satisfied, last_list))`：`satisfied` = `done` 在超时内成立过；`last_list` = 最后一次枚举
/// 结果（调用方据此给出语境相关的诊断信息）。枚举 `Err` → `Err(msg)`，**立即**返回不重试
///（best-effort：探测手段本身坏了就交回上层，不把 start 拖在这里）。
///
/// # 为什么与 [`probe_adapter_present`] 分成两层
///
/// 这一层只管**时序**（deadline、零间隔兜底、先判后睡的顺序），谓词由调用方给。时序里最易悄悄写错的
/// 是顺序：先睡后判在健康路径上会白等一个 `poll_interval`（起核热路径上就是 200ms），而这种差异不会有
/// 任何测试自己冒出来 —— 收在一处、由 `present_immediately_when_adapter_already_up` 的零 sleep 断言钉死，
/// 比让它散在谓词旁边可靠。
fn poll_adapters<P, S>(
    probe: &P,
    timeout: Duration,
    poll_interval: Duration,
    sleep: &S,
    done: impl Fn(&[String]) -> bool,
) -> Result<(bool, Vec<String>), String>
where
    P: AdapterProbe + ?Sized,
    S: ThreadSleep,
{
    let deadline = Instant::now() + timeout;
    let interval = if poll_interval.is_zero() {
        Duration::from_millis(1)
    } else {
        poll_interval
    };
    loop {
        match probe.list_matching_adapters() {
            Ok(list) if done(&list) => return Ok((true, list)),
            Ok(list) => {
                if Instant::now() >= deadline {
                    return Ok((false, list));
                }
                sleep.sleep(interval);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// 有界轮询探测**目标适配器出现**（#327：起核后逐腿正向验证 TUN 网卡真被建出来）。
///
/// 语义：
/// - 每 `poll_interval` 调一次 [`AdapterProbe::list_matching_adapters`]。
/// - 枚举结果里出现 `expected`（ASCII 大小写不敏感）→ [`PresenceOutcome::Present`]，立即返回。
/// - `deadline` 到仍没有 → [`PresenceOutcome::Absent`]，带最后一次枚举到的同前缀适配器名。
/// - 枚举 `Err` → [`PresenceOutcome::Error`]，立即返回（上层按不可断言处理，不闸）。
///
/// # 参数
///
/// - `probe`：适配器枚举实现。
/// - `expected`：本次配置的 TUN 接口名。
/// - `timeout`：总轮询预算。
/// - `poll_interval`：两次枚举之间的 sleep。
/// - `sleep`：sleep 抽象（测试用 fake clock，生产用 [`std::thread::sleep`]）。
///
/// # 调用前必须先过 [`adapter_name_is_probeable`]
///
/// 见该谓词文档：`expected` 不在可枚举前缀面内时本函数恒返 `Absent`，那是「我方看不见」而非
/// 「网卡不存在」，两者的正确处置完全相反。
///
/// # 大小写不敏感比对的理由
///
/// Windows 适配器 FriendlyName 由我们经 sing-box config 指定，理论上逐字回读；但注册表/网络配置层
/// 历来对网卡名做大小写不敏感处理，且这里判错的代价是**杀掉一个正常的核**。取宽松侧。
pub fn probe_adapter_present<P, S>(
    probe: &P,
    expected: &str,
    timeout: Duration,
    poll_interval: Duration,
    sleep: &S,
) -> PresenceOutcome
where
    P: AdapterProbe + ?Sized,
    S: ThreadSleep,
{
    let hit = |list: &[String]| list.iter().any(|n| n.eq_ignore_ascii_case(expected));
    match poll_adapters(probe, timeout, poll_interval, sleep, hit) {
        Ok((true, _)) => PresenceOutcome::Present,
        Ok((false, seen)) => PresenceOutcome::Absent { seen },
        Err(e) => PresenceOutcome::Error(e),
    }
}

/// sleep 抽象（测试 fake clock；生产用 [`StdSleep`]）。
pub trait ThreadSleep {
    fn sleep(&self, dur: Duration);
}

/// 生产 sleep（[`std::thread::sleep`]）。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdSleep;

impl ThreadSleep for StdSleep {
    fn sleep(&self, dur: Duration) {
        std::thread::sleep(dur);
    }
}

// ===== Windows 生产实现 =====

#[cfg(windows)]
#[allow(unsafe_code)] // windows-sys FFI（GetAdaptersAddresses/解引用 widestring）必须 unsafe；每处附 SAFETY。
mod win_impl {
    use super::super::netinfo::u64_cells_for;
    use super::{adapter_name_is_probeable, ipv4_s_addr, AdapterProbe};
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GetBestInterfaceEx,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME,
        GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN,
    };

    /// 查询 Windows 路由表对目标 IPv4 选出的最佳接口索引。
    ///
    /// 与 PowerShell `Find-NetRoute -RemoteIPAddress ...` 回答同一个问题，但直接调用 IP Helper API：
    /// 不创建 PowerShell/.NET 进程，也不解析本地化文本。调用者只比较起核前后的索引是否变化，故无需
    /// 再把索引反查成可变、可本地化的 FriendlyName。
    pub fn best_route_interface_index(ip: std::net::Ipv4Addr) -> std::io::Result<u32> {
        let address = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: 0,
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: ipv4_s_addr(ip),
                },
            },
            sin_zero: [0; 8],
        };
        let mut index = 0u32;
        // SAFETY: `address` 是本栈帧内完整初始化的 IPv4 SOCKADDR，指针仅在同步调用期间借用；
        // `index` 是有效可写的 u32。GetBestInterfaceEx 不保留二者指针。
        let rc =
            unsafe { GetBestInterfaceEx((&raw const address).cast::<SOCKADDR>(), &raw mut index) };
        if rc != NO_ERROR {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        if index == 0 {
            return Err(std::io::Error::other(
                "GetBestInterfaceEx returned interface index 0",
            ));
        }
        Ok(index)
    }

    /// 承载缓冲区用 `Vec<u64>` 的**前提**：目标结构体的 align 不得超过 u64 的 align。
    /// 与 [`super::super::netinfo`] 同款断言、同款承载类型（两处对称，别一处 u8 一处 u64）。
    const _: () = assert!(
        std::mem::align_of::<IP_ADAPTER_ADDRESSES_LH>() <= std::mem::align_of::<u64>(),
        "IP_ADAPTER_ADDRESSES_LH 的对齐已超过 u64，Vec<u64> 承载不再安全"
    );

    /// Windows IP Helper 适配器枚举实现（`GetAdaptersAddresses`）。
    ///
    /// 跳过所有不需要的字段（只取 FriendlyName，按前缀匹配）。比 Go 源无对应（Go 用 sing-box 自管 wintun，
    /// 本 Rust crate 主动加这层探测以补维度7 #30）。
    pub struct WinAdapterProbe;

    impl AdapterProbe for WinAdapterProbe {
        fn list_matching_adapters(&self) -> std::io::Result<Vec<String>> {
            // SAFETY: GetAdaptersAddresses 是线程安全的 Win32 API。我们传 SizePointer 控制缓冲区，
            // 重试直到 ERROR_BUFFER_OVERFLOW 不再出现（标准两步法：先取大小，再取内容）。
            // 缓冲区由我们拥有，释放由 Vec drop 处理。指针仅在本函数作用域内有效。
            let flags: u32 = GAA_FLAG_SKIP_UNICAST
                | GAA_FLAG_SKIP_ANYCAST
                | GAA_FLAG_SKIP_MULTICAST
                | GAA_FLAG_SKIP_DNS_SERVER
                | GAA_FLAG_SKIP_FRIENDLY_NAME;
            let mut size: u32 = 0;
            let mut retries = 0u32;
            loop {
                // SAFETY: 第一次调用取所需大小（pAdapterAddresses=NULL），返回 ERROR_BUFFER_OVERFLOW。
                // 详见上方 SAFETY 注释。
                let rc = unsafe {
                    GetAdaptersAddresses(
                        0,
                        flags,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        &mut size,
                    )
                };
                if rc == ERROR_BUFFER_OVERFLOW {
                    break;
                }
                if rc == NO_ERROR && size == 0 {
                    return Ok(Vec::new()); // 无适配器
                }
                if rc != NO_ERROR {
                    return Err(std::io::Error::from_raw_os_error(rc as i32));
                }
                retries += 1;
                if retries > 3 {
                    return Err(std::io::Error::other("GetAdaptersAddresses size loop"));
                }
            }
            // 缓冲区按 u64 槽分配：结果要按 `&IP_ADAPTER_ADDRESSES_LH` 解引用，该结构体含指针/u64
            // ⇒ align = 8，而 `Vec<u8>` 只保证 align 1，从未对齐地址造引用按语言规则即 UB
            //（实践上 Windows 堆恰好 16 字节对齐所以不炸，但那是靠分配器实现细节而非语言保证）。
            let mut buf: Vec<u64> = vec![0u64; u64_cells_for(size)];
            // SAFETY: buf 容量 ≥ size 字节（u64_cells_for 向上取整）且对齐满足目标结构体（上方 const
            // 断言编译期保证）。pAdapterAddresses 指向 buf 首字节。后续遍历以 SingleLinkedList 形式
            //（Next 指针），仅读不写。
            let rc = unsafe {
                GetAdaptersAddresses(
                    0,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                    &mut size,
                )
            };
            if rc != NO_ERROR {
                return Err(std::io::Error::from_raw_os_error(rc as i32));
            }
            // 遍历链表，取 FriendlyName 匹配前缀。
            let mut out = Vec::new();
            let mut ptr = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: 链表节点由 GetAdaptersAddresses 填充，Next 为 NULL 终止。ptr 非空时解引用安全。
            while !ptr.is_null() {
                let entry: &IP_ADAPTER_ADDRESSES_LH = unsafe { &*ptr };
                if let Some(name) = read_friendly_name(entry) {
                    // 前缀判据经 `adapter_name_is_probeable` 收口 —— 存在性探测的调用方要用**同一个**
                    // 谓词判「这个名字我方看不看得见」，两处各写一遍 `starts_with` 就是第二真值源。
                    if adapter_name_is_probeable(&name) {
                        out.push(name);
                    }
                }
                ptr = entry.Next as *const IP_ADAPTER_ADDRESSES_LH;
            }
            Ok(out)
        }
    }

    /// 读 IP_ADAPTER_ADDRESSES_LH.FriendlyName（UTF-16 widestring → String）。
    ///
    /// SAFETY: 调用方保证 entry 有效；FriendlyName 指向 widestring（NULL 终止）。
    fn read_friendly_name(entry: &IP_ADAPTER_ADDRESSES_LH) -> Option<String> {
        let p = entry.FriendlyName;
        if p.is_null() {
            return None;
        }
        // SAFETY: FriendlyName 是 widestring（NULL 终止），由 GetAdaptersAddresses 填充，生命周期同 entry。
        let mut len = 0usize;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
            if len > 256 {
                return None; // 防越界（适配器名不会这么长）
            }
        }
        // SAFETY: p 指向 len 个有效 u16（widestring），由 GetAdaptersAddresses 填充。
        let slice: &[u16] = unsafe { std::slice::from_raw_parts(p, len) };
        String::from_utf16(slice).ok()
    }

    // 抑制未用警告（c_void 在签名里出现但本模块未直接命名使用）。
    const _: Option<*const c_void> = None;
}

#[cfg(windows)]
pub use win_impl::{best_route_interface_index, WinAdapterProbe};

// ===== 测试 mock =====

#[cfg(test)]
#[derive(Debug, Default)]
struct MockAdapterProbe {
    /// 适配器列表序列：每次 list_matching_adapters 返回下一个（模拟「逐渐释放」）。
    /// 用 Mutex<Vec<Vec<String>>> 模拟「每次轮询后适配器减少」。
    pub(crate) snapshots: std::sync::Mutex<std::collections::VecDeque<Vec<String>>>,
}

#[cfg(test)]
impl MockAdapterProbe {
    fn new(snapshots: Vec<Vec<String>>) -> Self {
        Self {
            snapshots: std::sync::Mutex::new(snapshots.into()),
        }
    }
}

#[cfg(test)]
impl AdapterProbe for MockAdapterProbe {
    fn list_matching_adapters(&self) -> std::io::Result<Vec<String>> {
        let mut q = self.snapshots.lock().unwrap();
        if q.len() == 1 {
            // 最后一帧一直返回（模拟稳态）
            Ok(q.front().cloned().unwrap_or_default())
        } else {
            Ok(q.pop_front().unwrap_or_default())
        }
    }
}

/// 测试 fake sleep（不真睡，记录总睡眠时长）。
#[cfg(test)]
#[derive(Debug, Default)]
struct FakeSleep {
    slept: std::sync::Mutex<Duration>,
}

#[cfg(test)]
impl ThreadSleep for FakeSleep {
    fn sleep(&self, dur: Duration) {
        *self.slept.lock().unwrap() += dur;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_s_addr_preserves_network_octets_in_memory() {
        let encoded = ipv4_s_addr(std::net::Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(encoded.to_ne_bytes(), [1, 2, 3, 4]);
    }

    #[test]
    fn prefixes_cover_polaris() {
        // 锁住：探测前缀覆盖 Polaris 自身命名谱系
        assert!(PROBE_PREFIXES.contains(&"polaris-"));
    }

    #[test]
    fn present_immediately_when_adapter_already_up() {
        // 健康路径：就绪时网卡早已在 → 首次枚举即命中，零 sleep（起核热路径不得凭空多等一个间隔）。
        let probe = MockAdapterProbe::new(vec![vec!["polaris-tun0".to_owned()]]);
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &probe,
            "polaris-tun0",
            DEFAULT_PROBE_TIMEOUT,
            DEFAULT_POLL_INTERVAL,
            &sleep,
        );
        assert_eq!(out, PresenceOutcome::Present);
        assert_eq!(*sleep.slept.lock().unwrap(), Duration::ZERO);
    }

    #[test]
    fn present_after_polls_when_adapter_appears_late() {
        // 就绪门过了但网卡还差几十毫秒才挂上 → 必须等，不能一次没看到就判失败。
        let probe = MockAdapterProbe::new(vec![vec![], vec!["polaris-tun0".to_owned()]]);
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &probe,
            "polaris-tun0",
            DEFAULT_PROBE_TIMEOUT,
            Duration::from_millis(10),
            &sleep,
        );
        assert_eq!(out, PresenceOutcome::Present);
        assert!(*sleep.slept.lock().unwrap() >= Duration::from_millis(10));
    }

    #[test]
    fn absent_when_adapter_never_appears() {
        // #327 的靶心：核活着、管理口通，但网卡自始至终没建出来。
        let probe = MockAdapterProbe::new(vec![vec![]]);
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &probe,
            "polaris-tun0",
            Duration::from_millis(50),
            Duration::from_millis(10),
            &sleep,
        );
        assert_eq!(out, PresenceOutcome::Absent { seen: vec![] });
    }

    #[test]
    fn absent_reports_other_polaris_adapters_seen() {
        // 同前缀但**不是**本次这张（如上一轮残留的 polaris-tun1）→ 仍是 Absent，且带上看到了什么，
        // 让上层日志能区分「一张都没有」与「有别的、就是没有我要的」。
        let probe = MockAdapterProbe::new(vec![vec!["polaris-tun1".to_owned()]]);
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &probe,
            "polaris-tun0",
            Duration::from_millis(50),
            Duration::from_millis(10),
            &sleep,
        );
        assert_eq!(
            out,
            PresenceOutcome::Absent {
                seen: vec!["polaris-tun1".to_owned()]
            }
        );
    }

    #[test]
    fn present_matches_case_insensitively() {
        let probe = MockAdapterProbe::new(vec![vec!["Polaris-TUN0".to_owned()]]);
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &probe,
            "polaris-tun0",
            DEFAULT_PROBE_TIMEOUT,
            DEFAULT_POLL_INTERVAL,
            &sleep,
        );
        assert_eq!(out, PresenceOutcome::Present);
    }

    #[test]
    fn presence_error_on_enum_failure() {
        struct ErrProbe;
        impl AdapterProbe for ErrProbe {
            fn list_matching_adapters(&self) -> std::io::Result<Vec<String>> {
                Err(std::io::Error::other("iphlpapi boom"))
            }
        }
        let sleep = FakeSleep::default();
        let out = probe_adapter_present(
            &ErrProbe,
            "polaris-tun0",
            DEFAULT_PROBE_TIMEOUT,
            DEFAULT_POLL_INTERVAL,
            &sleep,
        );
        match out {
            PresenceOutcome::Error(msg) => assert!(msg.contains("iphlpapi boom")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn probeable_predicate_matches_enumeration_filter() {
        // 变异对照：把它写成恒 true，则用户自定义接口名（`my-tun`）会被上层当成「网卡没建出来」
        // → 杀掉一个完全正常的核。这条锁的正是那个方向。
        assert!(adapter_name_is_probeable("polaris-tun0"));
        assert!(adapter_name_is_probeable("polaris-anything"));
        assert!(!adapter_name_is_probeable("my-tun"));
        assert!(!adapter_name_is_probeable("以太网"));
        assert!(!adapter_name_is_probeable("Polaris-tun0")); // 枚举侧是 starts_with，大小写敏感
    }
}
