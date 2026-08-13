//! `reap_sequence` 的优雅停核腿必须保持 G3 探针实测出来的那个形状。
//!
//! # 为什么是源码级判据
//!
//! 这段代码 `#[cfg(windows)]`，本机（Linux）编不了也跑不了；真要跑它得起一个 Windows 服务
//! 再拉一个子进程。那件事由 G3 探针在 CI 的 windows runner 上做过一次（run `31591517111`），
//! 结论确定后探针本身已移除 —— 它是一次性实验装置，不是长期资产。
//! 所以这里守的不是「行为对不对」（那由那次实测背书），而是
//! **「实现有没有偏离那个已被实测背书的形状」**。形状一偏，那次实测的结论就不再适用于这段代码。
//!
//! ⚠️ 装置已经不在了 ⇒ 这道门是该结论**唯一**的守卫。要重做实验得先把探针写回来。
//!
//! # 守的三件事
//!
//! 1. 第二条路还在，且被 `reap_sequence` 调用（删掉它 = 回到「Windows 上永远硬杀」）；
//! 2. **不得**出现 `SetConsoleCtrlHandler(None, …)` —— 那个形式只忽略 CTRL+C，对 CTRL_BREAK
//!    无效，用它等于让 helper 可能被自己发出的事件带走（探针首跑就是这么静默退出的）；
//! 3. 调用顺序与探针一致（Free → Attach → 装自己的 handler → 投递 → Free → 摘 handler）。
//!
//! 判据取**去注释后的源码**：本文件与 win.rs 的注释里都写着这些 API 名，不剔会把
//! 「注释里提到过」当成「代码里做了」。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/helper 之上应有仓根")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 逐行剔掉 `//` 之后的内容。够用：本仓这两个文件没有块注释里的代码。
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 截 `fn <name>(` 到下一个顶层 `\n}` 的函数体。
fn fn_body(src: &str, name: &str) -> String {
    let needle = format!("fn {name}(");
    let i = src
        .find(&needle)
        .unwrap_or_else(|| panic!("找不到 `{needle}` —— 改名或删了，先确认再动本门"));
    let rest = &src[i..];
    let end = rest.find("\n}").expect("函数没有收口");
    rest[..end].to_string()
}

/// 断言若干片段在文中**按给定次序**出现。
fn assert_ordered(hay: &str, needles: &[&str], what: &str) {
    let mut from = 0usize;
    for n in needles {
        match hay[from..].find(n) {
            Some(k) => from += k + n.len(),
            None => panic!("{what}：`{n}` 没有出现在它该在的位置（顺序：{needles:?}）"),
        }
    }
}

const WIN: &str = "crates/helper/src/platform/windows/winproc/win.rs";

/// 🔴 第二条路存在，且 `reap_sequence` 真的走它 + 仍有硬杀兜底。
#[test]
fn reap_sequence_tries_the_child_console_then_falls_back_to_kill() {
    let src = strip_comments(&read(WIN));
    let body = fn_body(&src, "reap_sequence");
    assert!(
        body.contains("send_ctrl_break(pid)"),
        "reap_sequence 不再尝试直接投递 CTRL_BREAK"
    );
    assert!(
        body.contains("send_ctrl_break_via_child_console(pid)"),
        "reap_sequence 不再走「借子进程 console」那条路 —— \
         服务模式下这是**唯一**能走通的优雅通道，删掉它等于回到「Windows 上永远硬杀」"
    );
    assert!(
        body.contains("terminate_pid_raw(pid)"),
        "硬杀兜底没了 —— 优雅通道任一步失败时进程就留下来了"
    );
    assert_ordered(
        &body,
        &[
            "send_ctrl_break(pid)",
            "send_ctrl_break_via_child_console",
            "terminate_pid_raw",
        ],
        "reap_sequence 的三段次序",
    );
}

/// 🔴 **不得**用 `SetConsoleCtrlHandler(None, …)`。
///
/// MSDN 对它的定义只有「忽略 CTRL+C」；`CTRL_BREAK` 不在其中，走默认处理器就是终止本进程。
/// G3 探针首跑（run `31590160361`）3.5 秒静默退出，这是头号嫌疑。
#[test]
fn never_uses_the_ctrl_c_only_handler_form() {
    {
        let src = strip_comments(&read(WIN));
        assert!(
            !src.contains("SetConsoleCtrlHandler(None"),
            "{WIN} 里出现了 SetConsoleCtrlHandler(None, …) —— 它只忽略 CTRL+C，\
             对 CTRL_BREAK 无效，等于让本进程可能被自己发出的事件带走"
        );
    }
}

/// 🔴 实现的调用顺序与探针实测的那一版一致。
///
/// 探针是**规格**：run `31591517111` 背书的是它那串调用。实现偏离了，那份实测就不再适用。
#[test]
fn implementation_follows_the_probe_verified_order() {
    let win = strip_comments(&read(WIN));
    let body = fn_body(&win, "send_ctrl_break_via_child_console");
    assert_ordered(
        &body,
        &[
            "FreeConsole()",
            "AttachConsole(pid)",
            "SetConsoleCtrlHandler(Some(",
            "GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)",
            "FreeConsole()",
            "SetConsoleCtrlHandler(Some(",
        ],
        "借用子进程 console 的调用序列",
    );

    // 这串顺序此前还与探针源码对拍过一次（探针是本实现的规格来源）。探针已随实验结束移除，
    // 那条对拍也就没了对象 —— 上面这串**就是**那次实测背书的形状，改它等于让结论失效。
}

/// 🔴 `send_ctrl_break` 的文档必须把「第二条路」指出来。
///
/// 这条守的是**那句错注释不要复活**：旧文案「服务模式无 console → 返回 0，无害」里的「无害」
/// 让整条优雅通道被判了死刑、几个月没人再查。留一个指针，下一个读到这里的人才知道还有下文。
#[test]
fn stale_no_op_claim_is_replaced_by_a_pointer() {
    let src = read(WIN);
    let i = src
        .find("fn send_ctrl_break(")
        .expect("send_ctrl_break 改名了");
    let doc = &src[i.saturating_sub(1600)..i];
    assert!(
        doc.contains("send_ctrl_break_via_child_console"),
        "send_ctrl_break 的文档里没有指向第二条路的指针"
    );
}
