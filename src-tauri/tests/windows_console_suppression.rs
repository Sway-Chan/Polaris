//! Windows 控制台窗口抑制的接线门。
//!
//! # 守的是什么
//!
//! 宿主是 GUI 子系统进程（`src-tauri/src/main.rs` 的 `windows_subsystem = "windows"`）⇒ 自身无控制台。
//! 无控制台的父进程起 **console 子系统**程序时，`CreateProcess` 会新分配一个控制台窗口（黑框）。
//! std 与 tokio 都**没有**隐含抑制 —— tokio 的 `creation_flags` 只是往 std 透传
//! （实测 tokio-1.53.1 `src/process/mod.rs:675-677`）。
//!
//! # 为什么必须是源码级门
//!
//! 这件事**在 Linux 上没有任何运行期表征**：`#[cfg(windows)]` 的分支根本不参与编译，
//! 纯函数单测测不到「有没有挂标志」，而唯一能观察到黑框的地方是 Windows 真机。
//! 三份现成教训都指向同一形状：`spawner.rs` 曾写着「tokio::process 在 Windows 默认不显示控制台窗口」
//! —— 一句**错误的注释**让这条缺陷在起核路径上潜伏了整个迁移期，没有任何门会红。
//!
//! # 与既有门的分工
//!
//! `core_build_matrix`（编了什么）/ `core_schema_surface`（配置形状与取值域）/ 起核 `check`（这份配置收不收）
//! 三道门都不看**进程怎么被创建**。本门只管这一格。

use std::collections::BTreeMap;

/// 仓库根（`src-tauri/` 的上一级）。
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri 必有上级目录")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 {}: {e}", p.display()))
}

/// 一个被守的调用点：锚点串之后的窗口内，必须同时出现 `Command::new(` 与抑制标记。
struct Guarded {
    file: &'static str,
    /// 唯一定位串。同名函数有多个 cfg 变体时**连 `#[cfg(...)]` 一起写**，否则锚点不唯一。
    anchor: &'static str,
    /// 抑制形态（可执行形，不是裸标识符）。
    suppressor: &'static str,
    /// 窗口自检串：窗口里必须先有它，否则说明窗口没盖住要守的东西 ⇒ 抑制断言恒真。
    /// 多数点是 `Command::new(`；`win_console.rs` 那两个函数**接收**已构造好的 `Command`，
    /// 它们自己不构造 ⇒ 自检改钉 cfg 门（抑制必须只在 Windows 生效，别在 Linux 上编不过）。
    self_check: &'static str,
    /// 从锚点起看多少行。各函数都远短于此；放宽只会让门更松，故取够用的最小值。
    window: usize,
}

/// 全部「Windows 可达 + 目标是 console 程序」的子进程构造点。
///
/// **不在表里 = 声称该调用点在 Windows 上不可达**。目前的豁免全部有 cfg 佐证：
/// `/bin/ps`（macos/linux 腿）、`pgrep`（`cfg(unix)`）、`route -n monitor`（仅 mac 守卫会调，
/// 见 `dns_watcher_loop` 文档）、`mesh.rs::run_command_stdout`（mac `ifconfig` 反查）、
/// `uninstall.rs::spawn_uninstaller`（拉起的是 Windows 卸载程序**自己的 GUI**，抑制窗口反而不对）。
const GUARDED: &[Guarded] = &[
    // ---- 本 crate：经 runtime/win_console.rs 收口 ----
    Guarded {
        file: "src-tauri/src/runtime/win_console.rs",
        anchor: "pub(crate) fn no_console_window(",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "#[cfg(windows)]",
        window: 14,
    },
    Guarded {
        file: "src-tauri/src/runtime/win_console.rs",
        anchor: "pub(crate) fn no_console_window_async(",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "#[cfg(windows)]",
        window: 10,
    },
    Guarded {
        file: "src-tauri/src/runtime/proxy.rs",
        anchor: "fn core_version_first_line(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 6,
    },
    Guarded {
        file: "src-tauri/src/runtime/proxy.rs",
        anchor: "#[cfg(windows)]\npub(crate) fn send_signal(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 8,
    },
    Guarded {
        file: "src-tauri/src/runtime/proxy.rs",
        anchor: "#[cfg(windows)]\nfn process_identity_impl(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 10,
    },
    Guarded {
        file: "src-tauri/src/runtime/proxy.rs",
        anchor: "#[cfg(windows)]\npub(crate) fn pid_alive(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 16,
    },
    Guarded {
        file: "src-tauri/src/runtime/updater.rs",
        anchor: "pub fn read_core_version_line(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 10,
    },
    Guarded {
        file: "src-tauri/src/runtime/core_swap.rs",
        anchor: "pub fn extract_archive(",
        suppressor: "no_console_window(",
        self_check: "Command::new(",
        window: 20,
    },
    Guarded {
        file: "src-tauri/src/runtime/tailscale_login_core.rs",
        anchor: "impl ConfigChecker for SingBoxConfigChecker {",
        suppressor: "no_console_window_async(",
        self_check: "Command::new(",
        window: 20,
    },
    Guarded {
        file: "src-tauri/src/commands/proxy.rs",
        anchor: "async fn run_probe_check(",
        suppressor: "no_console_window_async(",
        self_check: "Command::new(",
        window: 14,
    },
    // ---- 另外三个 crate：与本 crate 无共同依赖，各自持等价实现 ----
    Guarded {
        file: "crates/system-integration/src/exec.rs",
        anchor: "impl CommandRunner for StdCommandRunner {",
        suppressor: "creation_flags(CREATE_NO_WINDOW)",
        self_check: "Command::new(",
        window: 20,
    },
    Guarded {
        file: "crates/core-supervisor/src/spawner.rs",
        anchor: "impl SingBoxSpawner for TokioSpawner {",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "Command::new(",
        window: 30,
    },
    Guarded {
        file: "crates/core-supervisor/src/config_gate.rs",
        anchor: "pub async fn run_config_check_within(",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "Command::new(",
        window: 30,
    },
    Guarded {
        file: "crates/helper-client/src/manager.rs",
        anchor: "fn sc_command(",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "Command::new(",
        window: 8,
    },
    Guarded {
        file: "crates/helper-client/src/privilege.rs",
        anchor: "impl Executor for StdExecutor {",
        suppressor: "creation_flags(0x0800_0000)",
        self_check: "Command::new(",
        window: 18,
    },
];

/// 去掉行注释（`//` 之后）—— 判据必须落在**可执行形态**上。
///
/// 本文件自己的模块头就反复写着 `creation_flags` 与 `CREATE_NO_WINDOW`，被守文件的文档注释同理；
/// 不剥注释的话，把生产调用整个删掉、注释留下，门照样绿（本仓 2026-08-07 起同型撞过四次）。
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_windows_reachable_spawn_suppresses_the_console() {
    let mut sources: BTreeMap<&str, String> = BTreeMap::new();
    for g in GUARDED {
        let src = sources
            .entry(g.file)
            .or_insert_with(|| strip_comments(&read(g.file)));
        let at = src.find(g.anchor).unwrap_or_else(|| {
            panic!(
                "{}：锚点 `{}` 消失（改名/删除？）——门已失去判据，不是「通过」",
                g.file, g.anchor
            )
        });
        assert!(
            src[at + g.anchor.len()..].find(g.anchor).is_none(),
            "{}：锚点 `{}` 不唯一，窗口可能落在另一个 cfg 变体上",
            g.file,
            g.anchor
        );
        let window: String = src[at..]
            .lines()
            .take(g.window)
            .collect::<Vec<_>>()
            .join("\n");
        // 自检：窗口里必须真有子进程构造，否则说明窗口太小 / 锚点漂了，下面那条断言就没有意义。
        assert!(
            window.contains(g.self_check),
            "{}：`{}` 之后 {} 行内没有 `{}` —— 窗口没盖住要守的东西，抑制断言恒真",
            g.file,
            g.anchor,
            g.window,
            g.self_check
        );
        assert!(
            window.contains(g.suppressor),
            "{}：`{}` 的子进程构造没挂 `{}` —— Windows 上会弹控制台窗口",
            g.file,
            g.anchor,
            g.suppressor
        );
    }
}

/// 已知会在 Windows 上执行的 console 程序名（字面量形态）。按**程序名反查**，与上面的清单互补：
/// 清单防「已守的被删」，本条防「新增一个 `tasklist` 调用却忘了挂标志」。
const CONSOLE_PROGRAMS: &[&str] = &[
    "\"tasklist\"",
    "\"taskkill\"",
    "\"sc\"",
    "\"netsh\"",
    "\"reg\"",
];

/// 允许出现裸调用的位置（测试夹具 / 纯字符串常量表）。
fn is_scannable(rel: &str) -> bool {
    rel.ends_with(".rs") && !rel.contains("/tests/") && !rel.contains("target/")
}

#[test]
fn no_new_console_program_spawn_escapes_the_suppression() {
    let root = repo_root();
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    let mut hits = 0usize;
    let mut sighted: Vec<String> = Vec::new();
    for dir in ["src-tauri/src", "crates"] {
        for entry in walk(&root.join(dir)) {
            let rel = entry
                .strip_prefix(&root)
                .unwrap_or(&entry)
                .to_string_lossy()
                .replace('\\', "/");
            if !is_scannable(&rel) {
                continue;
            }
            // helper 是 Windows **服务**（session 0，无交互桌面）⇒ 它起的子进程本就无窗口可弹，
            // 且那边已自带 `CREATE_NO_WINDOW`（`winproc/win.rs:296`）。不纳入本门射程。
            if rel.starts_with("crates/helper/") {
                continue;
            }
            let raw = std::fs::read_to_string(&entry).unwrap_or_default();
            scanned += 1;
            // **不切 `#[cfg(test)]`**：`proxy.rs` 里生产码与测试模块交替出现（实测顶层 5 处），
            // 切第一处会把后面全部真调用点一起丢掉 —— 实测本门第一版就是这么静默漏掉 4 处的。
            // 测试夹具起的是 `powershell` / `sleep`，都不在 [`CONSOLE_PROGRAMS`] 里，故无需切。
            let prod = strip_comments(&raw);
            for (i, line) in prod.lines().enumerate() {
                // 只认 `process::Command::new(`（std / tokio 都带这个前缀）。
                // `polaris_system_integration::exec::Command::new(program, args)` 是两参数的**命令描述**，
                // 真正的 spawn 在 `StdCommandRunner::run` 里、已在 GUARDED 表中单独守着。
                if !line.contains("process::Command::new(") {
                    continue;
                }
                if !CONSOLE_PROGRAMS.iter().any(|p| line.contains(p)) {
                    continue;
                }
                hits += 1;
                sighted.push(format!("{rel}: {}", line.trim()));
                let lo = i.saturating_sub(6);
                let ctx: String = prod
                    .lines()
                    .skip(lo)
                    .take(i - lo + 14)
                    .collect::<Vec<_>>()
                    .join("\n");
                let guarded = ctx.contains("no_console_window")
                    || ctx.contains("creation_flags")
                    || ctx.contains("sc_command(");
                if !guarded {
                    offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        scanned > 50,
        "只扫到 {scanned} 个文件 —— 遍历坏了，绿没有信息量"
    );
    // 具名自检比数量更有信息量：那条 **1Hz** 的 `tasklist`（helper 腿探活）是本门最初要守的东西，
    // 它掉出扫描面就说明遍历/匹配坏了 —— 而那正是「绿了却什么都没查」的形状。
    assert!(
        sighted
            .iter()
            .any(|s| s.starts_with("src-tauri/src/runtime/proxy.rs") && s.contains("\"tasklist\"")),
        "扫描面里没有 `proxy.rs` 的 tasklist（1Hz 探活腿）—— 遍历或匹配坏了。实际命中 {hits} 处：\n{}",
        sighted.join("\n")
    );
    assert!(
        offenders.is_empty(),
        "以下 console 程序调用点没有窗口抑制（Windows 上会弹黑框）：\n{}",
        offenders.join("\n")
    );
}

/// 四份实现散在四个无共同依赖的 crate 里 —— 值必须逐字一致，否则「改了一处以为全改了」。
#[test]
fn the_four_crates_agree_on_the_flag_value() {
    let bearers = [
        "src-tauri/src/runtime/win_console.rs",
        "crates/system-integration/src/exec.rs",
        "crates/core-supervisor/src/spawner.rs",
        "crates/helper-client/src/manager.rs",
    ];
    for f in bearers {
        let src = strip_comments(&read(f));
        assert!(
            src.contains("0x0800_0000"),
            "{f}：`CREATE_NO_WINDOW` 的值不见了（或被写成了别的字面形态）"
        );
    }
}

/// 上面两条门读的是**源码文本**，它们答不了「`#[cfg(windows)]` 里的东西编不编得过」。
/// 那一格由 CI 的交叉 check 步骤答 —— 而那一步在 Linux 腿上是平台特定代码的**唯一**检出通道
/// （Linux 编译单元根本不含那些分支，本地 `cargo build/clippy/test` 对它们的检出力恒为 0）。
///
/// 实测正向对照：往 `manager.rs` 的 `#[cfg(target_os = "windows")]` 分支塞一个类型错误，
/// `cargo build`（Linux）**照样绿**，`cargo check --target x86_64-pc-windows-msvc` 红。
#[test]
fn the_cross_target_check_is_still_wired_in_ci() {
    let raw = read(".github/workflows/ci.yml");
    // 剥注释：本步骤的解释性注释里也会出现「cargo check」「lib.exe」这些词，
    // 判据必须落在 `run:` 的可执行内容上（本仓 `ci_step_still_wired` 踩过这个坑）。
    let yml: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        yml.contains("rustup target add x86_64-pc-windows-msvc x86_64-apple-darwin"),
        "ci.yml 不再安装交叉 target —— 平台特定代码在 Linux 腿上重新变成零检出"
    );
    for p in [
        "-p polaris-system-integration",
        "-p polaris-core-supervisor",
        "-p polaris-helper-client",
        // helper 是平台特定代码最密集的 crate（Windows SCM + 命名管道 + wintun；macOS launchd + sysctl）。
        // 它一度被排除在交叉 check 外，理由是「build script 要 C 工具链」——**那是推断，实测秒过**。
        "-p polaris-helper",
    ] {
        assert!(
            yml.contains(p),
            "ci.yml 的交叉 check 不再覆盖 `{p}` —— 该 crate 的 cfg(windows)/cfg(macos) 分支重新无人验"
        );
    }
    assert!(
        yml.contains("cargo check --target"),
        "ci.yml 里没有 `cargo check --target` —— 交叉 check 步骤被删了"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
