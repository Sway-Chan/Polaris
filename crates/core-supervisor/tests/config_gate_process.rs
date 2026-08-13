//! [`run_config_check`] 的**真实子进程**接线门（三平台同跑）。
//!
//! **为什么在集成测试而非单元测试**：拿到本 crate bin 目标路径的 `CARGO_BIN_EXE_<name>` 只在集成
//! 测试里由 cargo 注入（单元测试拿不到）。探针见 `src/bin/check_probe.rs`，其中也写了「为什么不拿
//! 随包真核来测」。
//!
//! **不触碰宿主网络**：探针只 print/exit，不开端口、不碰接口。被它冒充的 `sing-box check` 本身也
//! 不碰网络 —— `strace -f -e trace=socket,bind,connect,listen` 跑生产形状（TUN inbound + 管理 API +
//! 119 节点）的真 check，四类 syscall 计数为 0；正向对照同一表达式能抓到 loopback `connect`
//! （`ECONNREFUSED`），故是真的没有，不是 strace 没抓到。

use std::path::Path;

use polaris_core_supervisor::{
    run_config_check, ConfigCheckVerdict, KernelRejection, RejectedArray,
};

/// 冒充 `sing-box check` 的探针（cargo 在集成测试期注入绝对路径）。
const PROBE: &str = env!("CARGO_BIN_EXE_check_probe");

/// 🔴 **变异锁：argv 必须原样是 `--disable-color check -c <path>`**。
///
/// 这一条钉的是三件**只有真跑子进程才验得到**的事：`--disable-color` 真的传出去了（不传则内核
/// stderr 恒带 ANSI 彩色码，且 sing-box 不看 stdout/stderr 是不是 tty，管道里照样上色 ⇒ 诊断行
/// 会以 `\x1b[31mFATAL\x1b[0m…` 开头）、`check` 子命令没丢（丢了就是 `sing-box -c x` 打 usage）、
/// `-c` 与路径成对。变异：删掉 `.arg("--disable-color")` 或调换 `check`/`-c` 顺序 ⇒ 本条断。
#[tokio::test]
async fn passes_disable_color_and_check_subcommand() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("argv.json")).await;
    let ConfigCheckVerdict::Unattributable(raw) = verdict else {
        panic!("探针回声腿应落 Unattributable（内容是 argv 本身，拆不出下标）；实得 {verdict:?}");
    };
    assert_eq!(
        raw, "--disable-color check -c argv.json",
        "argv 必须逐格就位；实收：{raw:?}"
    );
}

/// rc=0 → `Accepted`。
#[tokio::test]
async fn zero_exit_is_accepted() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("accept.json")).await;
    assert_eq!(verdict, ConfigCheckVerdict::Accepted);
}

/// 🔴 **变异锁：非零退出 + 可归因诊断 → `Rejected` 且下标解得出（读的是 stderr）**。
///
/// 变异：把读 stderr 改成只读 stdout ⇒ 本条断在 `Accepted`/`Unattributable`。
#[tokio::test]
async fn nonzero_exit_with_index_is_rejected_and_read_from_stderr() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("reject.json")).await;
    assert_eq!(
        verdict,
        ConfigCheckVerdict::Rejected(KernelRejection {
            array: RejectedArray::Outbounds,
            index: 3,
            detail: "outbounds[3]: unknown outbound type: zzz".to_string(),
        })
    );
}

/// stderr 为空才回落 stdout 的兜底腿（真核实测恒走 stderr，这一支是给把日志导向 stdout 的
/// 变体/未来版本留的；没有它，那类核的诊断会整段丢失、退化成 `Unattributable`）。
#[tokio::test]
async fn falls_back_to_stdout_when_stderr_empty() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("stdout.json")).await;
    assert!(
        matches!(verdict, ConfigCheckVerdict::Rejected(ref r) if r.index == 3),
        "stderr 空时必须回落 stdout；实得 {verdict:?}"
    );
}

/// 非零退出但拆不出下标 → `Unattributable`（**不是** `Rejected`，绝不乱剥节点）。
#[tokio::test]
async fn nonzero_exit_without_index_is_unattributable() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("garbage.json")).await;
    assert_eq!(
        verdict,
        ConfigCheckVerdict::Unattributable(
            "FATAL[0000] decode config at cfg.json: duplicate outbound/endpoint tag: dup"
                .to_string()
        )
    );
}

/// 非零退出但双流全空的病态腿：必须给一句有内容的话，不能把空串当诊断上报。
#[tokio::test]
async fn nonzero_exit_with_no_output_still_reports_something() {
    let verdict = run_config_check(Path::new(PROBE), Path::new("silent.json")).await;
    let ConfigCheckVerdict::Unattributable(raw) = verdict else {
        panic!("双流全空应落 Unattributable；实得 {verdict:?}");
    };
    assert!(!raw.trim().is_empty(), "不得把空串当诊断");
}

/// 🔴 **变异锁：核不存在 → `Unavailable`（failOpen），而不是判配置无效**。
///
/// 这是整道闸门最要紧的一条口径：把「核临时读不到」判成「配置无效」，会让一次核缺失升级成
/// 「一个节点都用不了」。变异：把 spawn 失败那一支改成返回 `Unattributable` 或 `Rejected` ⇒ 本条断。
#[tokio::test]
async fn missing_binary_is_unavailable_not_invalid() {
    let missing = Path::new(PROBE).with_file_name("polaris-no-such-core-binary");
    let verdict = run_config_check(&missing, Path::new("accept.json")).await;
    assert!(
        matches!(verdict, ConfigCheckVerdict::Unavailable(_)),
        "核不存在必须判 Unavailable（failOpen）；实得 {verdict:?}"
    );
}

/// 🔴 **变异锁：超时 → `Unavailable`（failOpen）+ 子进程真的被杀掉**。
///
/// 这条门补的是一个**此前完全没有门**的分支：实测把超时腿改成返回 `Accepted`、或改成
/// `Rejected(outbounds[0])` 去剥一个无辜好节点，整套门（23 条）保持全绿。fail-open 口径此前
/// 只有 spawn-失败那一半有锁。
///
/// 第二件事更要紧：超时腿把 `output()` 的 future 直接丢掉，而 `tokio::process::Child` 的
/// `kill_on_drop` **默认 false** ⇒ 不置那一行就会留下游离的 `sing-box check`。本闸门挂在
/// **每次起核**（含每条重试腿）上，泄漏会累积。
///
/// 判据用**见证文件**而不是扫进程表：后者要按平台各写一套、且 CI 容器里未必看得见。
/// 探针睡满 400ms 才写见证 ⇒ 被杀掉的话文件永不出现。
///
/// 下面的**正向对照不可省**：没有它，「文件不存在」既可能是「进程被杀了」，也可能是
/// 「路径根本没传对、这条腿从来就写不出文件」—— 那样断言恒真、零信息量。
#[tokio::test]
async fn times_out_and_kills_the_child() {
    let dir = std::env::temp_dir().join(format!("polaris-gate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");

    // ① 正向对照：超时 > 睡眠 ⇒ 探针跑完，见证文件出现。证明这条腿本身是活的。
    let ok_witness = dir.join("ran.txt");
    let verdict = polaris_core_supervisor::run_config_check_within(
        Path::new(PROBE),
        Path::new(&format!("hang:{}", ok_witness.display())),
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(verdict, ConfigCheckVerdict::Accepted, "睡满后探针 rc=0");
    assert!(
        ok_witness.exists(),
        "正向对照失败：探针跑完了却没写见证文件 —— 路径没传对，下面那条断言会恒真"
    );

    // ② 真正要验的：超时 < 睡眠 ⇒ 判 Unavailable，且见证文件**永不出现**（子进程被杀）。
    let killed_witness = dir.join("killed.txt");
    let verdict = polaris_core_supervisor::run_config_check_within(
        Path::new(PROBE),
        Path::new(&format!("hang:{}", killed_witness.display())),
        std::time::Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(verdict, ConfigCheckVerdict::Unavailable(_)),
        "超时必须判 Unavailable（failOpen）—— 判 Accepted 会放行一份没验过的配置，\
         判 Rejected 会去剥一个无辜的好节点；实得 {verdict:?}"
    );
    // 等过探针的睡眠时长再看：活着的话这会儿早写完了。
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        !killed_witness.exists(),
        "超时后子进程仍跑完并写了见证文件 ⇒ `kill_on_drop(true)` 掉了，每次起核泄漏一个 check 进程"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
