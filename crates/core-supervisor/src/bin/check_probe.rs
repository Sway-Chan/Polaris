//! 测试专用探针二进制：冒充 `sing-box check`，把 [`run_config_check`] 的真实接线变成可断言的行为。
//! 不进任何发布产物。
//!
//! **为什么需要它**（同 `argv_probe.rs` 的理由，这里再具体一层）：`run_config_check` 那段是纯接线——
//! argv 拼装、`--disable-color` 有没有真传出去、读的是 stderr 还是 stdout、退出码怎么映射三态。
//! 这些**一条都不是纯逻辑**，只能靠真跑一个子进程来验；而拿随包 `resources/linux/sing-box` 来验不行：
//! 那个二进制**不入库**（`.gitignore` 的 `/resources/*`，由 `scripts/` 拉取），于是在没跑过 fetch-core
//! 的机器与 CI 上，依赖它的测试只能写成「文件不在就 skip」= 一条**永远不会红**的门，比没有门更坏。
//! 本 crate 自己的 bin 目标三平台恒在，门没有平台盲区、也没有「资源没拉就静默失效」的盲区。
//!
//! **模式由 argv 里的配置路径选**（`run_config_check` 恒发 `--disable-color check -c <path>`，
//! 唯一能由调用方控制的格是那个路径，故它就是模式选择器；探针从不真的打开这个路径）：
//!
//! | 配置路径含 | 行为 | 服务于 |
//! |---|---|---|
//! | `accept`   | rc=0，无输出                          | `Accepted` 腿 |
//! | `reject`   | rc=1，stderr 吐 decode 期 FATAL 行     | `Rejected` 腿 + 「读的是 stderr」 |
//! | `stdout`   | rc=1，同样的 FATAL 行只吐 **stdout**   | 「stderr 空才回落 stdout」那条兜底 |
//! | `garbage`  | rc=1，stderr 吐一行拆不出下标的话       | `Unattributable` 腿 |
//! | `silent`   | rc=1，双流全空                         | 「非零退出但无输出」的病态腿 |
//! | `argv`     | rc=1，stderr 打回完整 argv              | `--disable-color` / `check` / `-c` 真的传出去了 |
//! | `hang`     | 睡 400ms **然后**建一个见证文件再 rc=0  | 超时腿 + 「超时后子进程真的被杀掉」 |
//! | 其余       | rc=0                                   | 缺省视同通过 |
//!
//! `hang` 腿的见证文件是**跨平台**证明子进程死了的办法：路径由调用方经配置路径传进来
//! （`…hang…<witness>`），只有睡满之后才写。超时短于睡眠 ⇒ 文件永不出现 ⇒ 进程确实没跑完；
//! 超时长于睡眠 ⇒ 文件出现（正向对照，证明这条腿本身是活的，不是路径写错了所以永远没文件）。
//! 不用「扫进程表找残留」是因为那要按平台各写一套，且在 CI 的容器里未必看得见。

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let joined = args.join(" ");
    if let Some(rest) = joined.split("hang:").nth(1) {
        let witness = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        std::thread::sleep(std::time::Duration::from_millis(400));
        // 睡满才落见证。被 kill 掉的话这一行永远不执行。
        let _ = std::fs::write(&witness, b"done");
        return;
    }
    // 逐字取自随包 sing-box 1.14.0-beta.7 对「未知 outbound type」坏 config 的真实 stderr
    // （带 `--disable-color` 时无 ANSI 前缀）。
    const FATAL: &str =
        "FATAL[0000] decode config at cfg.json: outbounds[3]: unknown outbound type: zzz";
    if joined.contains("accept") {
        return;
    }
    if joined.contains("reject") {
        eprintln!("{FATAL}");
    } else if joined.contains("stdout") {
        println!("{FATAL}");
    } else if joined.contains("garbage") {
        eprintln!("FATAL[0000] decode config at cfg.json: duplicate outbound/endpoint tag: dup");
    } else if joined.contains("argv") {
        eprintln!("{joined}");
    } else if !joined.contains("silent") {
        return;
    }
    std::process::exit(1);
}
