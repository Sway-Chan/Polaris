//! Wire 命令名常量（Polaris 行协议第二行 `<command>` 的全部取值，逐平台对照 Go 源 `case "<cmd>":` 分支）。
//!
//! 这些常量是 wire 兼容性的硬约束：改名 = 协议破坏（已部署 helper 不识别）。三平台共用的命令定义在
//! [`common`]；平台专属的（mac 的 `flush-dns`/`install-core`/`default-restore`、win 的 `uninstall`/`iface-metric`、
//! linux 的 `install-core`）分别定义在各自模块 —— 调用方据此裁剪（[`Command`] 平台标记不可做的部分由
//! helper-client / 各 helper crate 的平台 trait 承接）。

#![allow(clippy::module_inception)]

/// 三平台共用的命令名（mac/win/linux 的 `case` 分支同名）。
///
/// 对照：
/// - mac `helper/helper.go:421-444,507,580`：ping/version/start/stop/status/cleanup + freeport/route-add/route-del。
/// - win `helper-win/helper.go:177-211,212-275,295-337,338`：ping/version/start/stop/status/cleanup +
///   freeport/route-add/route-del（注意 win 的 route-add/del 在 mac 后追加，proto v2）。
/// - linux `helper-linux/helper.go:345-482`：ping/version/start/stop/status/cleanup/freeport/install-core。
pub mod common {
    /// `OK pong uid=<n> v<ver> [build=<id>]`（三平台握手；build 为 Polaris 向后兼容扩展）。
    pub const PING: &str = "ping";
    /// `OK <ver>`（mac/win/linux，纯协议版本查询）。
    pub const VERSION: &str = "version";
    /// `OK started <pid>` / `OK already <pid>` / `ERR ...`（启核，参数最多）。
    pub const START: &str = "start";
    /// `OK stopped <pid>` / `OK notrunning`（停核，TERM→等→KILL 后台收割）。
    pub const STOP: &str = "stop";
    /// `OK running <pid>` / `OK stopped`（查核状态）。
    pub const STATUS: &str = "status";
    /// `OK cleaned`（兜底清所有锁定二进制实例 + 摘 child）。
    pub const CLEANUP: &str = "cleanup";
    /// `OK free` / `OK killed <pids>` / `OK foreign <names>` / `ERR bad-port`（按端口定位 LISTEN 持有者）。
    pub const FREEPORT: &str = "freeport";
    /// `OK route`（出口托管接口装 ifscope/per-iface 路由，mac proto v7 / win proto v2）。
    pub const ROUTE_ADD: &str = "route-add";
    /// `OK route`（同上的删除侧，幂等 best-effort）。
    pub const ROUTE_DEL: &str = "route-del";
}

/// macOS 专属命令（移植自 `helper/helper.go`）。
pub mod mac {
    /// `OK installed` / `ERR ...`（把 app 下载+预检的临时核 sha256 校验后 root 写锁定 coreDir + 签名 + 清 quarantine；
    /// mac proto v5）。
    pub const INSTALL_CORE: &str = "install-core";
    /// `OK default-restore` / `ERR bad-gateway`（补回停核后被 sing-tun setRoutes 误删的 en0 全局默认路由；mac proto v8）。
    pub const DEFAULT_RESTORE: &str = "default-restore";
    /// `OK flushed` / `OK flushed-partial ...` / `ERR dscacheutil ...`（root 刷系统 DNS 缓存：
    /// dscacheutil + HUP mDNSResponder；mac proto v9）。
    pub const FLUSH_DNS: &str = "flush-dns";
}

/// Windows 专属命令（移植自 `helper-win/helper.go`）。
pub mod win {
    /// `OK uninstalling`（自卸载：收割 child → 派生 SYSTEM 旁路停删 SCM 服务 + 删 supportDir 含 helper.exe 自身；
    /// 零 UAC 主路径，`helper-win/helper.go:276-294`）。
    pub const UNINSTALL: &str = "uninstall";
    /// `OK iface-metric` / `ERR iface-denied` / `ERR bad-metric` / `ERR set-metric ...`（退役保留兼容：把内核接口
    /// metric 设高，PowerShell `Set-NetIPInterface`；win proto v3-v5，新客户端 EXPECTED_PROTO 已降回 v1 不再调用）。
    pub const IFACE_METRIC: &str = "iface-metric";
    // 注意：Windows **无** install-core（macOS 专属内核持久化，Windows 由 app 侧 NSIS 安装器处理；
    // `helper-win/main.go:22` coreDir flag 接受并忽略仅为镜像 mac 形态）。
}

/// Linux 专属命令（移植自 `helper-linux/helper.go`）。
pub mod linux {
    /// `OK installed` / `ERR ...`（与 mac 同构：临时核 sha256 校验后 root 写锁定 coreDir，逐文件 .new+rename 原子就位；
    /// linux proto v1）。
    pub const INSTALL_CORE: &str = "install-core";
    // 注意：Linux **无** route-add/route-del/uninstall —— TUN 路由由核 sing-tun 自己装（CAP_NET_ADMIN 在 ambient set），
    // 卸载走 systemd unit disable+remove（非 socket 命令）。
}

#[cfg(test)]
mod tests {
    use super::*;

    // wire 命令名是已部署 helper 的硬约束 —— 改名 = 协议破坏。本测锁住移植正确性（逐字对照 Go 源）。
    #[test]
    fn common_command_names_match_polaris_go_source() {
        // mac helper/helper.go:421-507；win helper-win/helper.go:177-338；linux helper-linux/helper.go:345-499
        assert_eq!(common::PING, "ping");
        assert_eq!(common::VERSION, "version");
        assert_eq!(common::START, "start");
        assert_eq!(common::STOP, "stop");
        assert_eq!(common::STATUS, "status");
        assert_eq!(common::CLEANUP, "cleanup");
        assert_eq!(common::FREEPORT, "freeport");
        assert_eq!(common::ROUTE_ADD, "route-add");
        assert_eq!(common::ROUTE_DEL, "route-del");
    }

    #[test]
    fn mac_specific_commands_match_polaris_go_source() {
        // helper/helper.go:481-585（default-restore v8、flush-dns v9、install-core v5）
        assert_eq!(mac::INSTALL_CORE, "install-core");
        assert_eq!(mac::DEFAULT_RESTORE, "default-restore");
        assert_eq!(mac::FLUSH_DNS, "flush-dns");
    }

    #[test]
    fn win_specific_commands_match_polaris_go_source() {
        // helper-win/helper.go:242-294（iface-metric v3-v5 退役保留、uninstall）
        assert_eq!(win::UNINSTALL, "uninstall");
        assert_eq!(win::IFACE_METRIC, "iface-metric");
    }

    #[test]
    fn linux_specific_commands_match_polaris_go_source() {
        // helper-linux/helper.go:396-399（install-core v1）
        assert_eq!(linux::INSTALL_CORE, "install-core");
    }
}
