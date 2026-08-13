//! Handler 进程状态 + Core spawn 抽象（对应 上游 `helper-linux/helper.go` 的全局 `child`/`childDone` + AmbientCaps 拉核）。
//!
//! ## 设计
//!
//! Go 源用包级全局 `child *exec.Cmd` + `childDone chan struct{}` + `mu sync.Mutex` 持有当前 sing-box 子进程。
//! 本实现把它们实例化为 [`HandlerState`]（可在测试中独立构造，不依赖全局可变状态）。
//!
//! Core spawn（start 命令）经 [`CoreSpawner`] trait 抽象：
//! - 生产实现（`AmbientCapsSpawner`，§helper-rust-evaluation B3 真机项）：fork → setuid 回对端登录用户 →
//!   raise ambient CAP_NET_ADMIN/RAW/BIND_SERVICE → execve coreDir/sing-box。这是 Linux 安全模型的核心地雷。
//! - 测试 mock：返回固定 pid，记录 spawn/terminate/kill 调用。
//!
//! 本 crate 不实现真实 AmbientCaps fork 链（B3 真机复验项），仅提供 trait + mock；
//! 真实实现见后续集成（`AmbientCapsSpawner` 占位，todo!()）。

use std::path::PathBuf;

/// 已 spawn 的 sing-box 子进程句柄（对应 Go `child *exec.Cmd`）。
#[derive(Debug, Clone)]
pub struct CoreHandle {
    /// 子进程 pid（Go `child.Process.Pid`）。
    pub pid: u32,
}

/// start 命令的 spawn 请求（对照 Go `exec.Command(coreBin(), "run", "-c", cfg)` + Credential + AmbientCaps，:431-442）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCoreRequest {
    /// sing-box 二进制路径（已校验 == coreDir/sing-box）。
    pub binary: PathBuf,
    /// 配置文件路径（已校验属主 == 对端 uid）。
    pub config: PathBuf,
    /// 日志文件路径（None = 不重定向）。
    pub log: Option<PathBuf>,
    /// allowLan 转发开关。
    pub fwd: bool,
    /// 父 app PID（父死看护；None = 不启看护）。
    pub parent_pid: Option<u32>,
    /// 降权目标 uid（对端登录用户）。
    pub uid: u32,
    /// 降权目标 gid（对端登录组）。
    pub gid: u32,
    /// 补充组 gid 列表（对端登录用户所属全部组，`setgroups` 用；对照 Go `Credential.Groups`，:435-439）。
    ///
    /// 在 fork 前于父进程经 [`supplementary_groups`](crate::platform::linux::auth::supplementary_groups)
    /// 解析（不在拉核子进程碰 NSS）。空 = `setgroups(&[])` 清空补充组（Go `Groups: nil` 等价，见该函数文档）。
    pub groups: Vec<u32>,
}

/// spawn 错误（对应 Go `c.Start()` 失败，:452-458）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    /// sing-box 启动失败（fork/execve/权限）。
    #[error("start {detail}")]
    Spawn { detail: String },
}

/// Core spawn 抽象（trait 便于测试 mock；生产用 AmbientCaps fork+setuid+execve）。
///
/// 对照 Go 源 start 分支的 `c.Start()`（:452）+ stop 的 `terminateChild`（:246-256）+ cleanup 的 `Kill`（:383）。
pub trait CoreSpawner: Send + Sync {
    /// spawn sing-box 子进程（AmbientCaps 拉核）。
    fn spawn(&self, req: &SpawnCoreRequest) -> Result<CoreHandle, SpawnError>;
    /// 优雅终止：SIGTERM → ≤5s → SIGKILL（Go `terminateChild`，:246-256）。
    fn terminate(&self, h: &CoreHandle);
    /// 强杀 SIGKILL（Go `child.Process.Kill()`，:383）。
    fn kill(&self, h: &CoreHandle);
}

/// Handler 进程状态（对应 Go 全局 `child`/`childDone`，实例化可测）。
#[derive(Debug)]
pub struct HandlerState {
    /// 当前 sing-box 子进程（None = stopped）。
    pub child: Option<CoreHandle>,
}

impl HandlerState {
    /// 构造空状态（无 child）。
    #[must_use]
    pub fn new() -> Self {
        Self { child: None }
    }
}

impl Default for HandlerState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_state_new_has_no_child() {
        let s = HandlerState::new();
        assert!(s.child.is_none());
    }

    #[test]
    fn handler_state_default_equals_new() {
        let a = HandlerState::new();
        let b = HandlerState::default();
        assert!(a.child.is_none());
        assert!(b.child.is_none());
    }

    #[test]
    fn spawn_request_carries_all_fields() {
        let r = SpawnCoreRequest {
            binary: PathBuf::from("/core/sing-box"),
            config: PathBuf::from("/tmp/c.json"),
            log: Some(PathBuf::from("/tmp/l.log")),
            fwd: true,
            parent_pid: Some(999),
            uid: 1000,
            gid: 1000,
            groups: vec![1000, 27, 44],
        };
        assert_eq!(r.binary, PathBuf::from("/core/sing-box"));
        assert_eq!(r.config, PathBuf::from("/tmp/c.json"));
        assert!(r.fwd);
        assert_eq!(r.parent_pid, Some(999));
        assert_eq!(r.uid, 1000);
        assert_eq!(r.gid, 1000);
        assert_eq!(r.groups, vec![1000, 27, 44]);
    }

    #[test]
    fn spawn_error_display_matches_wire() {
        // wire 形态 "ERR start <detail>" 的 detail 部分应与 Display 输出一致。
        let e = SpawnError::Spawn {
            detail: "exit status 1".into(),
        };
        assert_eq!(e.to_string(), "start exit status 1");
    }

    /// 静态断言：CoreSpawner 是对象安全 + Send + Sync（生产注入用 Box<dyn>）。
    #[allow(dead_code)]
    fn _assert_core_spawner_object_safe(_s: Box<dyn CoreSpawner>) {}
}
