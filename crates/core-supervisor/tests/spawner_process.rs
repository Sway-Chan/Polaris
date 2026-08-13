//! `spawner` 的**真实子进程**接线门（三平台同跑）。
//!
//! **为什么在集成测试而非单元测试**：这些断言需要一个真实的子进程二进制，而拿到本 crate bin
//! 目标路径的 `CARGO_BIN_EXE_<name>` 只在集成测试 / bench 里由 cargo 注入（单元测试拿不到）。
//! 原先这几条住在 `src/spawner.rs` 的单元测试里、拿 `/bin/echo` `/bin/sleep` 顶替，
//! Windows 上两条静默 skip、一条硬 panic —— 平台盲区加 CI 恒红。探针见 `src/bin/argv_probe.rs`。
//!
//! 不触碰宿主网络：探针只打印/睡觉，不开任何端口、不碰任何接口。

use polaris_core_supervisor::{SingBoxSpawner, SpawnRequest, TokioSpawner};

/// 探针二进制绝对路径（cargo 在集成测试期注入）。
const PROBE: &str = env!("CARGO_BIN_EXE_argv_probe");

/// 真实 spawn 一个常驻子进程：pid 可读 + 可被 kill。
#[tokio::test]
async fn tokio_spawner_real_child_spawns_and_pid_present() {
    let spawner = TokioSpawner::new();
    let req = SpawnRequest::new(PROBE, "--sleep");
    let mut spawned = spawner.spawn(&req).expect("spawn 探针应成功");
    assert!(spawned.pid().is_some(), "pid 应可读");
    // 清理：杀掉子进程（探针最长驻留 30s，不留残骸）。
    let _ = spawned.child.start_kill();
    let _ = spawned.child.wait().await;
}

/// 回归：`TokioSpawner::spawn` 必须把 **完整 argv（含 `run` 子命令）** 交给子进程。
///
/// 此前 `.args(&req.argv()[1..])` 误按 C 约定当 argv\[0\] 是程序名切掉首元素 → `run` 丢失 →
/// 真核收到 `sing-box -c cfg` 打 usage 即退，上层只看到「启动期退出」。
///
/// **为什么既有的 argv 单测没抓到**：`spawn_request_argv_*` 只测 `argv()` 本身（对的），
/// 真实 spawn 的那几条为图省事**绕开 SpawnRequest 直接用 Command** —— 于是「argv 生成」与
/// 「spawn 传参」各自有门，二者的**组合**（真正的生产路径）无门。本测试专补这个组合。
#[tokio::test]
async fn tokio_spawner_passes_run_subcommand_to_child() {
    let spawner = TokioSpawner::new();
    let req = SpawnRequest::new(PROBE, "/tmp/polaris-argv-probe.json");
    let spawned = spawner.spawn(&req).expect("spawn 探针应成功");
    let out = spawned.child.wait_with_output().await.expect("收探针输出");
    let printed = String::from_utf8_lossy(&out.stdout);
    let printed = printed.trim();
    assert_eq!(
        printed, "run -c /tmp/polaris-argv-probe.json",
        "子进程必须收到完整 argv（含 run 子命令）；实收：{printed:?}"
    );
}

/// 验证 stdio 配置：stdin=null、stdout/stderr=piped —— stdout 可读即证 pipe 接上。
#[tokio::test]
async fn tokio_spawner_pipes_stdio_config() {
    let spawner = TokioSpawner::new();
    let req = SpawnRequest::new(PROBE, "polaris-test-marker");
    let spawned = spawner.spawn(&req).expect("spawn 探针应成功");
    let out = spawned.child.wait_with_output().await.expect("收探针输出");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("polaris-test-marker"));
}

/// 回归·CWD 接线：`working_dir` 设定 → 子进程真的 chdir 到该目录。
///
/// **变异门**：删掉 `if let Some(cwd) … cmd.current_dir(cwd)` → CWD 回落测试进程 CWD → 断言转红。
/// 两侧都 canonicalize 再比：macOS 上 `/tmp` 是 `/private/tmp` 符号链接，Windows 上有 `\\?\` 前缀。
#[tokio::test]
async fn tokio_spawner_applies_working_dir() {
    let dir = std::env::temp_dir().join(format!("polaris-cwd-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建探针工作目录");
    let expected = std::fs::canonicalize(&dir).expect("canonicalize 期望 CWD");

    let spawner = TokioSpawner::new();
    let mut req = SpawnRequest::new(PROBE, "--pwd");
    req.working_dir = Some(dir.clone());
    let spawned = spawner.spawn(&req).expect("spawn 探针应成功");
    let out = spawned.child.wait_with_output().await.expect("收探针输出");
    let printed = String::from_utf8_lossy(&out.stdout);
    let actual = std::fs::canonicalize(printed.trim()).expect("canonicalize 实得 CWD");

    assert_eq!(
        actual, expected,
        "子进程 CWD 应为 working_dir；实得：{printed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
