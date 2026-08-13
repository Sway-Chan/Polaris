//! 测试专用探针二进制：把 spawner 的真实接线变成**可断言的 stdout**。不进任何发布产物。
//!
//! **为什么需要它**：`spawner` 的真实 spawn 门原先拿 `/bin/echo`、`/bin/sleep`、临时 shell 脚本
//! 当「核」——这三样在 Windows 上都不存在，于是那几条门要么 `eprintln!("[skip]") + return`
//! （跳过型门 = 没有门），要么直接 `panic!`（Windows CI 恒红，实际发生过：release gate 首次跑满
//! 3-OS 矩阵即挂在 `tokio_spawner_passes_run_subcommand_to_child`）。换成本 crate 自己的 bin
//! 目标后，三平台跑的是同一条真实 spawn 路径，门不再有平台盲区。
//!
//! **模式由 argv 选**：`SpawnRequest::argv()` 恒为 `run -c <config>`，没有别的旋钮可拧——
//! `<config>` 那一格就是模式选择器（探针从不真的打开这个路径，只按字面量分派）。
//!
//! | argv 含 | 行为 | 服务于 |
//! |---|---|---|
//! | `--pwd` | 打印当前工作目录 | `working_dir` 接线 |
//! | `--sleep` | 常驻 30s | pid 可读 + 进程可被 kill |
//! | 其余 | 把 `argv[1..]` 空格连接打回 stdout | argv 完整传递（含 `run` 子命令） |

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--pwd") {
        println!(
            "{}",
            std::env::current_dir().expect("读当前工作目录").display()
        );
    } else if args.iter().any(|a| a == "--sleep") {
        std::thread::sleep(std::time::Duration::from_secs(30));
    } else {
        println!("{}", args.join(" "));
    }
}
