//! polaris-helper-proto — core ↔ helper 共享协议 crate（§D.1 day-1 Rust 收益：消灭跨语言 wire drift）。
//!
//! Polaris 今天 core(TS)↔helper(Go) 的 line-based 文本协议在两侧手工同步（`singbox-api-client.ts:303-306`
//! 甚至用 proto 内容 hash 防上游漂移）。全 Rust 后 core 与 helper 引用本 crate，协议演进编译期强一致。
//!
//! ## 协议形态（Polaris Go 源作移植 oracle，逐序列对照）
//!
//! Polaris helper 用 **line-based 文本协议**（非 protobuf/gRPC）：每行以 `\n` 结尾，Go 源用
//! `bufio.Reader.ReadString('\n')` 读、`fmt.Fprintln(conn, ...)` 写。本 crate 把这套 wire 协议固化为
//! 类型化的 Rust 类型 —— 选 serde 而非 prost/tonic 的理由：
//! 1. **逐字移植 Polaris wire 形态**：Go 源没有 .proto 文件、没有 gRPC，只有 `fmt.Fprintf(conn, "OK ...\n")`。
//!    prost/tonic 会引入一套全新 wire 格式，与已部署 helper（迁移期共存）断协议。
//! 2. **wire drift 收益不依赖 prost**：类型化枚举 + 编译期单一定义点已经消灭 drift（core/helper 引用同一
//!    `Request::Stop` 而非各自手写字符串）。
//! 3. **最小依赖 + 审计友好**：line-based 协议仅靠 std 即可编解码，符合 Polaris helper「仅依赖标准库便于审计」
//!    的设计纪律（`helper.go:18`）。serde 用于把 [`Request`]/[`Response`] 暴露给 core 侧的 IPC 层
//!    （Tauri command 经 serde_json 序列化到 renderer），与 wire 形态解耦。
//!
//! ## protoVersion 三平台统一为 1（不移植 上游的三谱系）
//!
//! 上游的 9/5/1 是**三套独立 Go module 各自演进出的历史谱系**（mac 从 v1 加到 v9、win 加到 v5、
//! linux 停在 v1），版本号唯一的作用是让新 client 认出「机器上装着的是哪一代旧 helper」。
//! Polaris 是全新产品 + 全新 Rust helper：**世上不存在旧版 Polaris helper**，没有任何一代需要被认出，
//! 抄那三个数字只会把别人的演进史当成自己的约束。故三平台统一 [`proto_version::CURRENT`] = 1。
//!
//! 平台差异不再靠版本号表达，而是由 [`Platform`] 承载（mac/win 有 token 行、linux 走 SO_PEERCRED；
//! 命令集差异由 [`command`] 常量 + 各平台 handler 的 `case` 覆盖面表达）——本 crate 是三平台**共用**
//! 的单一 crate，编译期就保证 core 与 helper 引用同一份定义，版本号本就无需分叉。
//!
//! 将来真需要断代（wire 形态不兼容变更 + 已有用户装着旧 helper）时，把 `CURRENT` 加到 2 即可；
//! 那才是版本号该出现的时刻。
//!
//! ## 模块布局
//!
//! - [`proto_version`] / [`Platform`]：协议版本 + 平台标识（B0 建立的骨架，本批保持向后兼容）。
//! - [`command`]：wire 命令名常量（逐字对照 Go `case` 分支）。
//! - [`error`]：错误码 [`ErrorCode`] + [`Error`]（对照 Go 所有 `ERR <code>` 调用点）。
//! - [`response`]：成功响应 [`Response`] / [`ResponseKind`]（对照 Go 所有 `OK ...` 调用点）。
//! - [`request`]：请求 [`Request`] + 参数类型（对照 Go 各 `case` 的 readLine 序列）。
//! - [`codec`]：帧编解码 + 安全白名单（移植自 Go 的 ifaceAllowed/cfgAllowed/ParseCIDR 校验）。

#![forbid(unsafe_code)]

pub mod codec;
pub mod command;
pub mod error;
pub mod request;
pub mod response;

// 顶层便利重导出：让 `polaris_helper_proto::Request` 等无需钻模块路径（core/helper 两侧主用类型）。
pub use error::{Error, ErrorCode};
pub use request::{
    parse_stop_pid, stop_pid_matches, InstallCoreParams, LinuxStartParams, Request, RouteParams,
    StartParams,
};
pub use response::{FlushDns, FreePort, Pong, Response, ResponseKind, Start, Status, Stop};

/// 协议版本（**三平台统一**，单一常量 —— 见 crate 级文档「protoVersion 三平台统一为 1」）。
pub mod proto_version {
    /// 当前 wire 协议版本，mac/win/linux 共用。
    ///
    /// 唯一定义点：helper 侧的 `ping`/`version` 响应与 client 侧的握手期望都读它 → 结构上不可能分叉。
    /// **仅在 wire 形态出现不兼容变更、且线上已有旧 helper 需被认出时才 +1**；新增向后兼容的命令
    /// 不必动（旧 helper 收到不认识的命令回 `ERR unknown`，client 侧按能力降级即可）。
    pub const CURRENT: u32 = 1;
}

/// 平台标识（helper 协议谱系选择，运行期由编译 target 决定）。
///
/// 单一真值：全 workspace 仅此处定义，其余 crate（system-integration / mesh / config-engine）
/// 一律 `use polaris_helper_proto::Platform`。变体名对齐 helper 协议三谱系（mac/win/linux），
/// 三处历史重定义的别名（Macos/Windows/Darwin/Win32）已统一。
///
/// [`Platform`] 决定帧结构差异（mac/win 有 token 行，linux 经 SO_PEERCRED 无 token 行）——
/// 见 [`codec::encode_frame`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS：root LaunchDaemon + 0666 unix socket + token 行协议。
    Mac,
    /// Windows：SCM 服务 + 命名管道（SDDL）+ token 行协议。
    Win,
    /// Linux：root systemd + 0666 unix socket + SO_PEERCRED（无 token 行）。
    Linux,
    /// 未知平台兜底（freebsd/openbsd/…）。无对应 helper 实现，按 Linux 语义保守处理
    /// （无 token 行、走 Unix 路径），避免对未鉴权对端误发 token 行。
    Other,
}

impl Platform {
    /// 当前平台是否在 wire 头部带 token 行（mac/win = true，linux/other = false）。
    ///
    /// 移植自：linux `helper-linux/main.go` 经 SO_PEERCRED 取对端 uid，`handle()` 首个 `readLine` 读的是
    /// command 而非 token（对照 mac `helper.go:403-404` 的 token+command 两行）。
    ///
    /// [`Platform::Other`] 视同 Linux（无 token 行）：未知平台无对应 helper 实现，保守按 SO_PEERCRED
    /// 类语义处理，避免对未鉴权对端误发 token 行。
    #[must_use]
    pub const fn has_token_line(self) -> bool {
        matches!(self, Self::Mac | Self::Win)
    }

    /// 编译目标平台（下沉自 system-integration/dns_flush.rs 三分 cfg!）。运行期决定本机谱系。
    ///
    /// 未知 target（非 mac/win/linux）→ [`Platform::Other`]。
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Win
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    /// 平台字符串解析（下沉自 mesh/exit_route.rs，对齐 上游 `process.platform` 口径）。
    ///
    /// 非 std `FromStr`：未知串不报错，返 [`Platform::Other`]。兼容 "darwin"/"macos" 与
    /// "win32"/"windows" 两套写法（各历史调用点传参不一，合并后仍受支持）。
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "darwin" | "macos" => Self::Mac,
            "win32" | "windows" => Self::Win,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── protoVersion 契约（**前提已换**）─────────────────────────────────────────
    //
    // 旧测 `proto_versions_match_polaris` / `platforms_are_distinct` 断言 9/5/1 且三者互异，前提是
    // 「三平台各自演进的独立谱系必须原样移植」。那个前提是 **上游的历史包袱**：9/5/1 只是三套独立
    // Go module 各自加过多少次功能的计数，唯一用途是让新 client 认出机器上那代旧 helper。Polaris 是
    // 全新产品 + 全新 Rust helper，**不存在任何一代旧 Polaris helper 需要被认出** ⇒ 三谱系无对象、
    // 「必须互异」更是把别人的演进史写成自己的不变量（它会主动阻止本该做的统一）。
    //
    // 新前提：版本号只表达「wire 断代」，平台差异由 `Platform`（帧结构）+ `command`（命令集）表达。
    // 故三平台共用一个 `CURRENT`，下面两测锁的是**统一**而非互异。

    #[test]
    fn proto_version_is_unified_v1() {
        assert_eq!(
            proto_version::CURRENT,
            1,
            "Polaris 全新 helper 无历史包袱，wire 协议从 v1 起；改动此值 = 声明 wire 断代"
        );
    }

    // 曾有一条 `proto_version_does_not_vary_by_platform`：遍历四个 `Platform` 反复断言
    // `Response::Ok(Pong{ proto_version: CURRENT }).to_wire_line() == "OK pong uid=0 v1"`。**已删** ——
    // 循环变量只出现在断言消息里，`advertised` 由常量 `CURRENT` 算出、与平台无关 ⇒ 四次迭代是同一
    // 个断言的四份副本，语义等价于 `CURRENT == 1`（上面那条已覆盖）；它自称能拦「有人按 Platform
    // match 返不同值」，可新增的那个函数**根本不会被它调用**，拦不住。
    //
    // 「不得 per-platform 分叉」的真锚点在**分叉真会发生的地方** —— 三个平台各自的 `PROTO_VERSION`
    // 常量（cfg 门控模块，helper-proto 这层遍历不到），每处一条字面量断言：
    //   · `platform::macos::mod.rs`   `proto_version_is_unified_current`
    //   · `platform::windows::mod.rs` `proto_version_is_unified_current`
    //   · `platform::linux::handler.rs` `wire_forms_match_go_source`（钉死 "OK pong uid=0 v1"）
    // `to_wire_line` 的 Pong 形态另由 `response.rs::to_wire_line_matches_go_source_literals` 覆盖。

    #[test]
    fn platform_carries_frame_shape_not_version() {
        // 推翻旧前提的正面表述：三平台**唯一**的协议差异是帧结构（token 行有无），不是版本号。
        // 同一个 Request 在 mac/linux 下编出的帧不同 —— 差异由 Platform 承载，版本号无需分叉。
        let req = Request::Ping;
        let mac = String::from_utf8(codec::encode(Platform::Mac, "TOK", &req)).unwrap();
        let linux = String::from_utf8(codec::encode(Platform::Linux, "", &req)).unwrap();
        assert_eq!(mac, "TOK\nping\n", "mac 带 token 行");
        assert_eq!(linux, "ping\n", "linux 走 SO_PEERCRED，无 token 行");
        assert_ne!(mac, linux, "平台差异体现在帧结构上");
    }

    #[test]
    fn platform_token_line_semantics() {
        // mac/win 带 token 行；linux 经 SO_PEERCRED 不带（helper-linux/helper.go:333-343）
        assert!(Platform::Mac.has_token_line());
        assert!(Platform::Win.has_token_line());
        assert!(!Platform::Linux.has_token_line());
        // Other 视同 Linux：未知平台无 helper 实现，保守不带 token 行。
        assert!(!Platform::Other.has_token_line());
    }

    #[test]
    fn platform_current_matches_compile_target() {
        // current() 由编译 target 决定；CI 本机 Linux → Linux。
        let cur = Platform::current();
        if cfg!(target_os = "macos") {
            assert_eq!(cur, Platform::Mac);
        } else if cfg!(target_os = "windows") {
            assert_eq!(cur, Platform::Win);
        } else if cfg!(target_os = "linux") {
            assert_eq!(cur, Platform::Linux);
        } else {
            assert_eq!(cur, Platform::Other);
        }
    }

    #[test]
    fn platform_parse_maps_known_strings() {
        // 对齐 上游 `process.platform` 口径 + 兼容各处历史传参写法。
        assert_eq!(Platform::parse("darwin"), Platform::Mac);
        assert_eq!(Platform::parse("macos"), Platform::Mac);
        assert_eq!(Platform::parse("win32"), Platform::Win);
        assert_eq!(Platform::parse("windows"), Platform::Win);
        assert_eq!(Platform::parse("linux"), Platform::Linux);
        // 未知串 → Other（非 std FromStr，不报错）。
        assert_eq!(Platform::parse("freebsd"), Platform::Other);
        assert_eq!(Platform::parse(""), Platform::Other);
    }

    /// 端到端往返：Request → encode → Response::parse 应覆盖典型路径。
    /// 这是「core 发、helper 收」的 wire 兼容性最关键的契约 —— 锁住编码/解码对称。
    #[test]
    fn end_to_end_wire_roundtrip_ping() {
        let req = Request::Ping;
        let bytes = codec::encode(Platform::Mac, "TOK", &req);
        let wire = String::from_utf8(bytes).unwrap();
        // 模拟 helper 回复 ping
        let resp = Response::parse("OK pong uid=0 v9");
        assert!(matches!(resp, Response::Ok(ResponseKind::Pong(_))));
        // wire 形态断言
        assert_eq!(wire, "TOK\nping\n");
    }

    /// start 完整往返：args 顺序 + fwd 字符串化 + ppid 可选行。
    #[test]
    fn end_to_end_start_roundtrip() {
        let req = Request::Start(StartParams {
            cfg: "/tmp/c.json".into(),
            log: "".into(),
            fwd: true,
            parent_pid: Some(999),
        });
        // mac 帧
        let mac_bytes = codec::encode(Platform::Mac, "T", &req);
        assert_eq!(
            String::from_utf8(mac_bytes).unwrap(),
            "T\nstart\n/tmp/c.json\n\n1\n999\n"
        );
        // linux 帧（无 token 行，但 LinuxStart 多 singbox 行）
        let lreq = Request::LinuxStart(LinuxStartParams {
            singbox_path: "/core/sing-box".into(),
            common: StartParams {
                cfg: "/tmp/c.json".into(),
                log: "".into(),
                fwd: false,
                parent_pid: None,
            },
        });
        let linux_bytes = codec::encode(Platform::Linux, "", &lreq);
        assert_eq!(
            String::from_utf8(linux_bytes).unwrap(),
            "start\n/core/sing-box\n/tmp/c.json\n\n0\n"
        );
    }
}
