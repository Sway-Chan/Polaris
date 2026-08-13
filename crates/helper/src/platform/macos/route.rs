//! 路由管理（移植自 上游 `helper/helper.go:450-491`）。
//!
//! ## 两个命令
//!
//! 1. **route-add / route-del**（proto v7，`helper.go:450-480`）：在 System 内核接口
//!    （polaris-ts/polaris-wg/utunN）上装/清 ifscope 拆半默认路由 —— sing-box 不为 TS exit_node 装出口
//!    路由（真机实证），需 helper 补。约束：iface 白名单 + CIDR 校验（防注入），ifscope 作用域到该接口。
//! 2. **default-restore**（proto v8，`helper.go:481-491`）：补回停核后被 sing-tun setRoutes EEXIST
//!    善后误删的 en0 全局默认路由（system WG 全隧道场景）。约束：网关须合法 IPv4。
//!
//! ## 移植纪律
//!
//! Go 源对每个 CIDR 调 `/sbin/route -n [add|delete] [-inet6] -ifscope <iface> -net <cidr> -interface <iface>`。
//! 本模块把「构造 argv」与「执行」分离 —— [`build_route_argv`] 是纯函数（跨平台可测），执行委托
//! [`CommandRunner`](crate::platform::macos::exec::CommandRunner)。幂等 best-effort：add 忽略 file-exists、delete 忽略
//! not-in-table（Go 源 `_ = execRun(...)` 忽略所有错误，最终由 app 侧 netstat 校验）。

use polaris_helper_proto::codec;

/// route-add / route-del 的单个 CIDR 对应的 argv（移植自 `helper.go:472-478`）。
///
/// 对照 Go：
/// ```text
/// args := []string{"-n", op}                  // op = "add" | "delete"
/// if strings.Contains(c, ":") {
///     args = append(args, "-inet6")
/// }
/// args = append(args, "-ifscope", iface, "-net", c, "-interface", iface)
/// ```
///
/// 返回 `Some(argv)` 当 cidr 合法（过 [`codec::is_valid_cidr`]）；非法返回 None（Go 源 `continue` 跳过）。
/// iface 须由调用方先过 [`crate::platform::macos::whitelist::iface_allowed`]（白名单外的接口直接 ERR iface-denied）。
#[must_use]
pub fn build_route_argv(op: RouteOp, iface: &str, cidr: &str) -> Option<Vec<String>> {
    // helper.go:470: 非法 CIDR 跳过（防注入）
    if !codec::is_valid_cidr(cidr) {
        return None;
    }
    let mut args = vec!["-n".to_owned(), op.as_route_subcmd().to_owned()];
    // helper.go:474: 含 ":" 判 IPv6 → 加 -inet6
    if cidr.contains(':') {
        args.push("-inet6".to_owned());
    }
    // helper.go:477: -ifscope iface -net cidr -interface iface
    args.push("-ifscope".to_owned());
    args.push(iface.to_owned());
    args.push("-net".to_owned());
    args.push(cidr.to_owned());
    args.push("-interface".to_owned());
    args.push(iface.to_owned());
    Some(args)
}

/// route-add / route-del 的操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOp {
    /// `route-add` → `route ... add ...`（`helper.go:461`）。
    Add,
    /// `route-del` → `route ... delete ...`（`helper.go:463`）。
    Delete,
}

impl RouteOp {
    /// 映射到 macOS `route` 子命令（Go `helper.go:461-464`：add→"add", del→"delete"）。
    #[must_use]
    pub const fn as_route_subcmd(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Delete => "delete",
        }
    }
}

/// default-restore 的 argv（移植自 `helper.go:490`）。
///
/// 对照 Go：`/sbin/route -n add -inet default <gw>`。
/// 网关须合法 IPv4（Go `net.ParseIP(gw).To4() != nil`，`helper.go:486`），否则调用方应返 `ERR bad-gateway`。
#[must_use]
pub fn build_default_restore_argv(gateway_ipv4: &str) -> Option<Vec<String>> {
    // helper.go:486: ip := net.ParseIP(gw); ip == nil || ip.To4() == nil → ERR bad-gateway
    if !codec::is_valid_ipv4(gateway_ipv4) {
        return None;
    }
    Some(vec![
        "-n".to_owned(),
        "add".to_owned(),
        "-inet".to_owned(),
        "default".to_owned(),
        gateway_ipv4.to_owned(),
    ])
}

/// route 命令的程序路径（macOS，`/sbin/route`，`helper.go:478,490`）。
pub const ROUTE_BIN: &str = "/sbin/route";

#[cfg(test)]
mod tests {
    use super::*;

    // ===== build_route_argv（逐字对照 helper.go:472-478）=====

    #[test]
    fn route_argv_ipv4_add() {
        // helper.go:472-478，op=add，IPv4 CIDR（无 -inet6）
        let argv = build_route_argv(RouteOp::Add, "polaris-ts", "10.0.0.0/8").unwrap();
        assert_eq!(
            argv,
            vec![
                "-n",
                "add",
                "-ifscope",
                "polaris-ts",
                "-net",
                "10.0.0.0/8",
                "-interface",
                "polaris-ts"
            ]
        );
    }

    #[test]
    fn route_argv_ipv6_delete() {
        // helper.go:474: 含 ":" → 加 -inet6；op=delete
        let argv = build_route_argv(RouteOp::Delete, "utun3", "2001:db8::/32").unwrap();
        assert_eq!(
            argv,
            vec![
                "-n",
                "delete",
                "-inet6",
                "-ifscope",
                "utun3",
                "-net",
                "2001:db8::/32",
                "-interface",
                "utun3"
            ]
        );
    }

    #[test]
    fn route_argv_invalid_cidr_returns_none() {
        // helper.go:470: 非法 CIDR continue 跳过
        assert!(build_route_argv(RouteOp::Add, "polaris-ts", "not-a-cidr").is_none());
        assert!(build_route_argv(RouteOp::Add, "polaris-ts", "999.0.0.0/8").is_none());
        assert!(build_route_argv(RouteOp::Add, "polaris-ts", "10.0.0.0/33").is_none());
    }

    #[test]
    fn route_op_subcmd_matches_go() {
        // helper.go:461,463
        assert_eq!(RouteOp::Add.as_route_subcmd(), "add");
        assert_eq!(RouteOp::Delete.as_route_subcmd(), "delete");
    }

    #[test]
    fn route_argv_multiple_cidrs_build_independently() {
        // 模拟 helper.go:465 循环：对 cidrsLine split(',') 后逐个 build
        let cidrs = "10.0.0.0/8,172.16.0.0/12,invalid,192.168.0.0/16";
        let argvs: Vec<_> = cidrs
            .split(',')
            .filter_map(|c| build_route_argv(RouteOp::Add, "polaris-wg", c.trim()))
            .collect();
        assert_eq!(argvs.len(), 3, "invalid 项被跳过");
        // 每条都带 -ifscope polaris-wg
        assert!(argvs.iter().all(|a| a.contains(&"-ifscope".to_owned())));
    }

    // ===== build_default_restore_argv（逐字对照 helper.go:486,490）=====

    #[test]
    fn default_restore_argv_valid_ipv4() {
        // helper.go:490: /sbin/route -n add -inet default <gw>
        let argv = build_default_restore_argv("192.168.1.1").unwrap();
        assert_eq!(argv, vec!["-n", "add", "-inet", "default", "192.168.1.1"]);
    }

    #[test]
    fn default_restore_argv_invalid_gateway_none() {
        // helper.go:486: ParseIP.To4 == nil → ERR bad-gateway
        assert!(build_default_restore_argv("not-an-ip").is_none());
        assert!(build_default_restore_argv("::1").is_none(), "IPv6 不接受");
        assert!(build_default_restore_argv("256.1.1.1").is_none());
        assert!(build_default_restore_argv("").is_none());
    }

    #[test]
    fn route_bin_is_sbin_route() {
        // helper.go:478,490: /sbin/route
        assert_eq!(ROUTE_BIN, "/sbin/route");
    }
}
