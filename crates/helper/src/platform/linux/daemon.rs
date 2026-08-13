//! Linux daemon 入口（忠实迁自 上游 Go `helper-linux/main.go` 的 `main()`，能力册 L15）。
//!
//! ## Go 源结构（`helper-linux/main.go`）
//!
//! ```text
//! func main() {
//!     flag.StringVar(&sockPath, "socket", "<运行目录>/helper.sock", ...)
//!     flag.StringVar(&authFile, "authfile", "<状态目录>/authorized-uids", ...)
//!     flag.StringVar(&coreDir, "coredir", "<内核目录>", ...)
//!     flag.BoolVar(&console, "console", false, ...)
//!     flag.Parse()
//!     // MkdirAll sockDir 0755 + Chmod 0755 + Remove 旧 socket + Listen(unix) + Chmod 0666
//!     signal.Notify(sigCh, SIGTERM, SIGINT)
//!     go func() {
//!         <-sigCh
//!         if child!=nil { terminateChild(c, done) }   // 同步收割当前 child（TERM→≤5s→KILL）
//!         waitReaps(6 * time.Second)                   // 等在途后台收割跑完 KILL 升级
//!         setForward(false)                            // 退出兜底：不留全局转发态
//!         l.Close(); os.Exit(0)
//!     }()
//!     if console { fmt.Printf(...) }
//!     for { conn, _ := l.Accept(); go handle(conn) }
//! }
//! ```
//!
//! ## C6-0 落地边界
//!
//! - flag 解析（[`parse_args`]，纯逻辑单测）、`prepare_socket`（mkdir 0755 + 删旧 + bind + chmod 0666）、
//!   SIGTERM/SIGINT 收割器 + **退出兜底 `setForward(false)`**（`main.go`）**均落地**。
//! - accept 到的连接当前被 drop（不 dispatch）—— `handle()` 接线（async `UnixStream` → `Conn` adapter）
//!   属 **C6-2**（见 [`super`] 模块文档「async UnixStream → 同步 Conn adapter」段）。
//! - 收割器：C6-0 spawner 是 [`super::server::NotImplementedSpawner`]（无 child），故「有 child →
//!   terminateChild」与 `waitReaps`（等在途后台收割）暂空转 —— C6-2 接真 AmbientCaps spawner 后补。
//!   `setForward(false)` 退出兜底真实执行（不依赖 child，杜绝残留全局转发态）。

use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};

use crate::platform::linux::ops::set_forward_prod;
use crate::platform::linux::server::{prepare_socket, ConnServer, ServerConfig};
use crate::platform::linux::server::{DEFAULT_AUTH_FILE, DEFAULT_CORE_DIR, DEFAULT_SOCK_PATH};

/// 解析 daemon flag → [`ServerConfig`]（对照 Go `main.go` 的 `flag.StringVar`/`flag.BoolVar` 四项）。
///
/// 默认值取 [`server`](super::server) 常量（systemd unit / pkexec 安装器依赖这些路径，是运维契约）。
#[must_use]
pub fn parse_args<I: Iterator<Item = String>>(argv: I) -> ServerConfig {
    let m = crate::cli::parse_flags(argv, &["console"]);
    ServerConfig {
        sock_path: m
            .get("socket")
            .map_or_else(|| DEFAULT_SOCK_PATH.into(), Into::into),
        auth_file: m
            .get("authfile")
            .map_or_else(|| DEFAULT_AUTH_FILE.into(), Into::into),
        core_dir: Some(
            m.get("coredir")
                .map_or_else(|| DEFAULT_CORE_DIR.into(), Into::into),
        ),
        console: m.contains_key("console"),
    }
}

/// daemon 入口（binary `main()` 经 `cfg(target_os="linux")` 调此）。
///
/// Linux helper 全程 async（tokio）：`peer_cred()` 取 SO_PEERCRED、accept 循环、信号收割都在 runtime 内。
/// 本函数建多线程 runtime 并 `block_on` [`async_main`]（对齐 Go 的 goroutine 并发模型）。
#[must_use]
pub fn daemon_main<I: Iterator<Item = String>>(argv: I) -> ExitCode {
    let cfg = parse_args(argv);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("polaris-helper (linux): build runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async_main(cfg))
}

/// async 主体（socket serve + 信号收割器 + 退出兜底）。
async fn async_main(cfg: ServerConfig) -> ExitCode {
    // main.go: MkdirAll 0755 + 删旧 socket + Listen(unix) + Chmod 0666（std 同步 bind）。
    let std_listener = match prepare_socket(&cfg) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("polaris-helper (linux): {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!("polaris-helper (linux): set_nonblocking: {e}");
        return ExitCode::FAILURE;
    }
    let listener = match tokio::net::UnixListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("polaris-helper (linux): tokio listener: {e}");
            return ExitCode::FAILURE;
        }
    };

    if cfg.console {
        // main.go: console 模式打印监听信息（dev/test）。
        println!(
            "Polaris linux helper (console) listening on {}, proto v{}",
            cfg.sock_path.display(),
            super::PROTO_VERSION
        );
    }

    // C6-2：建跨连接共享服务（单一 HandlerState + AmbientCapsSpawner），accept 到即 dispatch。
    let server = ConnServer::new(&cfg);

    // main.go: SIGTERM/SIGINT 收割器。async 下把 Go 的「reaper goroutine + main accept 循环」折叠进
    // 单 select 循环 —— 收到信号 break 出循环走退出兜底，语义等价（更省一个通道）。
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("polaris-helper (linux): install SIGTERM: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("polaris-helper (linux): install SIGINT: {e}");
            return ExitCode::FAILURE;
        }
    };

    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            accepted = listener.accept() => {
                match accepted {
                    // C6-2：dispatch 到 handle()（捕获 SO_PEERCRED → std 阻塞流 + 5s 读超时 → spawn_blocking）。
                    Ok((stream, _addr)) => server.dispatch(stream),
                    Err(_) => continue, // main.go: accept 错 continue。
                }
            }
        }
    }

    // main.go 退出兜底（:46-57）：
    // C6-2：先同步 terminate 当前 child（TERM→≤5s→KILL）+ waitReaps(6s) 等在途后台 terminate 归零
    // （杜绝留下带 CAP_NET_ADMIN 的孤儿核），再 setForward(false) 复位全局转发态。
    server.reap_on_shutdown();
    set_forward_prod(false);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv(args: &[&str]) -> std::vec::IntoIter<String> {
        let mut v = vec!["polaris-helper".to_owned()];
        v.extend(args.iter().map(|s| (*s).to_owned()));
        v.into_iter()
    }

    #[test]
    fn parse_args_defaults_match_ops_contract() {
        // 缺 flag → 运维契约默认路径（systemd unit / pkexec 安装器依赖）。
        let cfg = parse_args(argv(&[]));
        assert_eq!(cfg.sock_path, PathBuf::from(DEFAULT_SOCK_PATH));
        assert_eq!(cfg.auth_file, PathBuf::from(DEFAULT_AUTH_FILE));
        assert_eq!(cfg.core_dir, Some(PathBuf::from(DEFAULT_CORE_DIR)));
        assert!(!cfg.console);
    }

    #[test]
    fn parse_args_all_flags_override() {
        let cfg = parse_args(argv(&[
            "--socket",
            "/tmp/x.sock",
            "--authfile",
            "/tmp/uids",
            "--coredir",
            "/tmp/core",
            "--console",
        ]));
        assert_eq!(cfg.sock_path, PathBuf::from("/tmp/x.sock"));
        assert_eq!(cfg.auth_file, PathBuf::from("/tmp/uids"));
        assert_eq!(cfg.core_dir, Some(PathBuf::from("/tmp/core")));
        assert!(cfg.console);
    }

    #[test]
    fn parse_args_console_is_bool_not_value() {
        // --console 不吞下一 token。
        let cfg = parse_args(argv(&["--console", "--socket", "/s.sock"]));
        assert!(cfg.console);
        assert_eq!(cfg.sock_path, PathBuf::from("/s.sock"));
    }
}
