//! sing-box spawn —— 上游 `ProxyManager.startSingBoxProcess` 直接 spawn 路径（systemProxy / Linux setcap）
//! 的 Rust 移植（源 `ProxyManager.ts:4789-4836`）。
//!
//! 不变式：
//! - 直接以 app 用户 spawn 非提权进程（command = singbox_path, args = ['run', '-c', config_path]）。
//! - stdio = ['ignore', 'pipe', 'pipe']（stdin ignore，stdout/stderr pipe 供日志采集 + 就绪判据）。
//! - windowsHide = true（GUI 进程 spawn 控制台程序隐藏黑窗；macOS/Linux 忽略）。
//! - spawn 失败（ENOENT/EACCES）→ [`SpawnError`]，上层进 retry 链（不在此处自动重启）。
//!
//! 不触碰宿主网络/sing-box：本文件的单元测试全是纯桩（只测 `argv()` 生成与 spawn 错误映射）。
//! **真实 spawn 的接线门在 `tests/spawner_process.rs`**——它拿本 crate 的测试探针 bin
//! （`src/bin/argv_probe.rs`）当「核」，三平台同跑；放集成测试是因为取探针路径的
//! `CARGO_BIN_EXE_<name>` 只在集成测试期由 cargo 注入。

use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Child;

/// spawn 请求（直接 spawn 路径，:4789-4791）。
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// sing-box 二进制绝对路径。
    pub binary: PathBuf,
    /// 配置文件绝对路径。
    pub config: PathBuf,
    /// 额外 CLI 参数（默认仅 `run -c <config>`）。
    pub extra_args: Vec<String>,
    /// 子进程工作目录（`None` = 继承父进程 CWD）。
    ///
    /// **为什么需要**：GUI 从 Finder/launchd 拉起时父进程 CWD = `/`（只读）。sing-box 对配置里**相对**路径
    /// （唯一一处：`clash_api`/`services[].dashboard` 省略 `path` 时的联网下载兜底目录 `dashboard`）按 CWD 解析
    /// → 落 `/dashboard` → 只读 mkdir 每次起核报一条噪音。设为可写 config 目录即消噪。**Polaris 生成的其余路径
    /// （cache.db / singbox.log / rules / rule-resource / custom-rules / tailscale state）全为绝对路径**，不受
    /// CWD 影响（见 `ProxyRuntime::generate_deps`）——故此改仅影响 dashboard 下载兜底这一条相对路径。
    pub working_dir: Option<PathBuf>,
}

impl SpawnRequest {
    /// 构造默认请求：`sing-box run -c <config>`。
    pub fn new(binary: impl Into<PathBuf>, config: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            config: config.into(),
            extra_args: Vec::new(),
            working_dir: None,
        }
    }

    /// 完整 CLI 参数序列（`[run, -c, <config>, ...extra]`）。
    pub fn argv(&self) -> Vec<String> {
        let mut v = vec![
            "run".to_string(),
            "-c".to_string(),
            self.config.to_string_lossy().into_owned(),
        ];
        v.extend(self.extra_args.iter().cloned());
        v
    }
}

/// spawn 出的子进程句柄（tokio::process::Child 包装）。
pub struct SpawnedChild {
    pub child: Child,
}

impl std::fmt::Debug for SpawnedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedChild")
            .field("pid", &self.child.id())
            .finish()
    }
}

impl SpawnedChild {
    /// 子进程 pid（spawn 后立即可用；None = 已退出或未拿到）。
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// spawn 错误（对应 NodeJS spawn ENOENT/EACCES → 上层 retry 链）。
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// 二进制不存在 / 无执行权限（ENOENT/EACCES）。
    #[error("sing-box spawn 失败（{bin:?}）: {source}")]
    Spawn {
        bin: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// spawn 抽象（trait 便于测试 mock；生产用 [`TokioSpawner`]）。
pub trait SingBoxSpawner: Send + Sync {
    /// 执行 spawn，返回子进程句柄。失败抛 [`SpawnError`]。
    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedChild, SpawnError>;
}

/// tokio::process 直接 spawn 实现（systemProxy / Linux setcap 路径，:4801）。
///
/// stdio = [ignore, pipe, pipe]（:4802）；windowsHide 在 Rust 侧对应不创建控制台
/// （tokio::process 默认不继承控制台，行为与 windowsHide=true 等价）。
#[derive(Debug, Default, Clone)]
pub struct TokioSpawner;

impl TokioSpawner {
    pub fn new() -> Self {
        Self
    }
}

impl SingBoxSpawner for TokioSpawner {
    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedChild, SpawnError> {
        let mut cmd = tokio::process::Command::new(&req.binary);
        cmd
            // 全量传 argv：`argv()[0]` 是 **`run` 子命令**，不是 C 约定里的程序名——
            // `Command::new(program)` 不吃 argv[0]，故此处若切掉首元素会丢掉 `run`，
            // sing-box 收到 `sing-box -c cfg.json` → 无子命令 → 打 usage 后立即退出
            // （表征为「启动期退出」，极难从上层归因）。回归防护见
            // `tokio_spawner_passes_run_subcommand_to_child`。
            .args(req.argv())
            .stdin(Stdio::null()) // stdin ignore（:4802）
            .stdout(Stdio::piped()) // stdout pipe（供日志/就绪）
            .stderr(Stdio::piped()); // stderr pipe（lastErrorOutput，:4830）
                                     // 工作目录（可写 config 目录）：消掉 CWD=`/` 下 dashboard 下载兜底的只读 mkdir 噪音（见 SpawnRequest.working_dir）。
        if let Some(cwd) = &req.working_dir {
            cmd.current_dir(cwd);
        }
        // windowsHide 等价。**原注释「tokio::process 在 Windows 默认不显示控制台窗口」是错的**：
        // tokio 只把 `creation_flags` 透传给 std（tokio-1.53.1 `src/process/mod.rs:675-677`），
        // 自身零默认。宿主是 GUI 子系统进程（`windows_subsystem = "windows"`）⇒ 起 console 程序
        // （sing-box 正是）会新分配一个控制台窗口。必须显式加 `CREATE_NO_WINDOW`（winbase.h 0x0800_0000）。
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let child = cmd.spawn().map_err(|source| SpawnError::Spawn {
            bin: req.binary.clone(),
            source,
        })?;
        Ok(SpawnedChild { child })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    #[test]
    fn spawn_request_argv_is_run_c_config_plus_extras() {
        // 对齐 Polaris 直接 spawn：`['run', '-c', configPath]`（:4790）。
        let req = SpawnRequest::new("/usr/local/bin/sing-box", "/tmp/cfg.json");
        assert_eq!(
            req.argv(),
            vec![
                "run".to_string(),
                "-c".to_string(),
                "/tmp/cfg.json".to_string()
            ]
        );
    }

    #[test]
    fn spawn_request_argv_appends_extra_args() {
        let mut req = SpawnRequest::new("/bin/sing-box", "/tmp/c.json");
        req.extra_args = vec!["--debug".to_string(), "--legacy".to_string()];
        assert_eq!(
            req.argv(),
            vec![
                "run".to_string(),
                "-c".to_string(),
                "/tmp/c.json".to_string(),
                "--debug".to_string(),
                "--legacy".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn tokio_spawner_returns_spawn_error_for_missing_binary() {
        // ENOENT → SpawnError::Spawn（上层据此判 retry）。
        let spawner = TokioSpawner::new();
        let req = SpawnRequest::new("/nonexistent/sing-box-xyz", "/tmp/c.json");
        let r = spawner.spawn(&req);
        assert!(r.is_err());
        match r {
            Err(SpawnError::Spawn { bin, source }) => {
                assert_eq!(bin, PathBuf::from("/nonexistent/sing-box-xyz"));
                // io::Error kind 应为 NotFound。
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected SpawnError::Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawned_child_pid_api_is_callable() {
        // SpawnedChild::pid 转发 child.id()。仅静态验证 API 存在；
        // 真实 pid 读取由 tokio_spawner_real_child_spawns_and_pid_present 覆盖。
        fn takes_spawned(s: &SpawnedChild) -> Option<u32> {
            s.pid()
        }
        // 引用以避免未用警告；不构造真实 Child（pid 语义由集成测试覆盖）。
        let _ = takes_spawned as fn(&SpawnedChild) -> Option<u32>;
    }
}
