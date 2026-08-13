//! 零 UAC 自毁卸载旁路命令行拼装（移植自 `helper-win/selfuninstall.go`）。
//!
//! 这是整个 helper-win crate 里**最危险**的纯字符串逻辑 —— 它拼装一条以 SYSTEM 身份执行 `cmd /c` 的命令行，
//! 故任何 cmd 元字符注入即提权。Go 源的 `selfuninstall.go` 之所以**无 build tag**（其余 helper-win 文件均
//! `//go:build windows`），正是为了让这套引号/转义逻辑在 Linux CI 上可测（先前一处 cmd 引号 bug 因只在
//! Windows 上能编译而溜过 review）。本 Rust 模块同理：纯字符串、跨平台、Linux 上有完整单测。
//!
//! ## 为何手工拼命令行
//!
//! Go `spawnSelfUninstall` 经 `SysProcAttr.CmdLine` 原样下发命令行，绕开 `syscall.EscapeArg`（后者会把内层
//! `"` 转义成 `\"`，而 cmd.exe 不识别 `\"` 反转义）。本 Rust 实现生产侧走 [`crate::platform::windows::winproc`] 的
//! `CreateProcessW` + `lpCommandLine` 原样下发（与 Go `CmdLine` 等价），同样绕开 argv 转义 —— 故本函数产出
//! 的命令行必须用**真双引号**包裹 rmdir 路径，绝不出现 `\"`。
//!
//! ## 安全前置假设
//!
//! `service_name` / `support_dir` 当前为安装期固定常量（`SERVICE_NAME` / `DEFAULT_SUPPORT_DIR`，非可达输入）。
//! 纵深防御：[`is_safe_support_dir`] 校验 support_dir，命中 cmd 元字符（`" & | < > % ^`）即改用
//! [`crate::platform::windows::SAFE_PLACEHOLDER_DIR`]（rmdir 落空，关闭 SYSTEM 注入窗口）。若未来 `--support` 变可达输入，
//! 本校验是最后的注入防线。

/// 构造自卸载旁路的完整命令行（移植自 `selfuninstall.go:22-39` 的 `selfUninstallCmdLine`）。
///
/// 产出的命令行形如（以默认参为例）：
///
/// ```text
/// cmd /c "ping 127.0.0.1 -n 3 >nul & sc stop PolarisHelper >nul 2>&1 & sc delete PolarisHelper >nul 2>&1 & ping 127.0.0.1 -n 2 >nul & rmdir /s /q "C:\ProgramData\Polaris" >nul 2>&1 & rmdir /s /q "C:\ProgramData\Polaris" >nul 2>&1"
/// ```
///
/// 顺序语义（Go 源 `selfuninstall.go:29-37`，对应自毁收尾的依赖链）：
/// 1. `ping 127.0.0.1 -n 3 >nul` —— 延迟 ~2s 等 helper 退出解锁 exe（DETACHED 下不能用 `timeout`）。
/// 2. `sc stop PolarisHelper >nul 2>&1` —— 停 SCM 服务（`sc delete` 要服务先停）。
/// 3. `sc delete PolarisHelper >nul 2>&1` —— 删 SCM 服务（要服务主进程先消失）。
/// 4. `ping 127.0.0.1 -n 2 >nul` —— 再等一拍让 SCM 反映 delete。
/// 5. `rmdir /s /q "<support_dir>" >nul 2>&1`（两次）—— 删目录（含外置 helper.exe 自身 + helper.token），
///    两次兜 exe 解锁竞态（旁路 ping 期间 helper 可能仍在退出）。
///
/// # 参数
///
/// - `service_name`：SCM 服务名（当前固定 `PolarisHelper`，常量）。
/// - `support_dir`：support 目录全路径（当前固定 `C:\ProgramData\Polaris`，常量）。
///
/// # 安全
///
/// `support_dir` 经 [`is_safe_support_dir`] 校验，不安全则改用 [`crate::platform::windows::SAFE_PLACEHOLDER_DIR`]
/// （rmdir 落空，绝不把 cmd 元字符拼进 SYSTEM 命令行）。`service_name` 当前为常量、未校验 ——
/// 若未来变可达输入须同样校验。
///
/// # 返回
///
/// 完整命令行字符串（生产侧经 `CreateProcessW` 的 `lpCommandLine` 原样下发）。
#[must_use]
pub fn self_uninstall_cmd_line(service_name: &str, support_dir: &str) -> String {
    // 纵深防御：本旁路以 SYSTEM 执行 cmd /c。support_dir 当前为安装期固定常量（非可达输入），但若未来
    // --support 变可达，须防 cmd 元字符（" & | < > % ^）注入即提权。校验不过则用安全占位（rmdir 落空，不注入）。
    // Go 源（selfuninstall.go:25-27）同款逻辑。
    let dir = if is_safe_support_dir(support_dir) {
        support_dir
    } else {
        crate::platform::windows::SAFE_PLACEHOLDER_DIR
    };
    let rmdir = format!(r#"rmdir /s /q "{dir}""#);
    // 顺序：等 helper 退出解锁 exe → 停服务 → 删服务 → 再等一拍让 SCM 反映 → 删目录（含 exe 自身）→ 兜底再删一次。
    // Go 源 strings.Join(..., " & ") —— cmd 用 ` & ` 串联无条件顺序执行（非 `&&`，前序失败不阻断后续）。
    let steps = [
        "ping 127.0.0.1 -n 3 >nul".to_string(),
        format!("sc stop {service_name} >nul 2>&1"),
        format!("sc delete {service_name} >nul 2>&1"),
        "ping 127.0.0.1 -n 2 >nul".to_string(),
        format!("{rmdir} >nul 2>&1"),
        format!("{rmdir} >nul 2>&1"),
    ];
    let script = steps.join(" & ");
    format!(r#"cmd /c "{script}""#)
}

/// `support_dir` 路径安全校验（移植自 `selfuninstall.go:43-56` 的 `isSafeSupportDir`）。
///
/// 仅允许路径安全字符：字母数字 `: \ / . _ -` 与空格 + Unicode 字母（中文等）。命中 cmd 元字符
/// （`" & | < > % ^` 等）即判不安全 → [`self_uninstall_cmd_line`] 改用占位路径，关闭 SYSTEM 注入窗口。
///
/// # 为什么允许 Unicode 字母
///
/// Go 源用 `unicode.IsLetter(c)` 显式允许中文等 Unicode 字母路径（如 `C:\应用\Polaris`），仅拒绝 cmd 元字符。
/// 本实现用 [`char::is_alphabetic`]（等价 Rust 的 Unicode 字母判定）保持一致。
///
/// # 返回
///
/// `true` = 路径仅含安全字符（可拼进 SYSTEM 命令行）；`false` = 含 cmd 元字符或为空。
#[must_use]
pub fn is_safe_support_dir(p: &str) -> bool {
    // Go 源（selfuninstall.go:45-46）：空串直接 false。
    if p.is_empty() {
        return false;
    }
    for c in p.chars() {
        // Go 源逐字符白名单：a-zA-Z0-9 : \ / . _ - 空格 + unicode.IsLetter。
        let ok = matches!(c,
            'a'..='z' | 'A'..='Z' | '0'..='9'
            | ':' | '\\' | '/' | '.' | '_' | '-' | ' ')
            || c.is_alphabetic(); // 允许中文等 Unicode 字母路径（cmd 元字符 & | < > " % ^ 仍被拒）
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    // 逐条移植 Go selfuninstall_test.go（同款断言）—— 这是 cmd 引号 HIGH bug 的回归护栏。

    #[test]
    fn cmd_line_uses_real_quotes_around_rmdir_path() {
        // 必须用**真双引号**包裹 rmdir 路径（非 \" 转义）：cmd 不识别 \" 反转义，转义版会令 rmdir 收到
        // 非法路径 \C:\…\ → 目录删不掉（selfuninstall.go:10-18 的 HIGH bug）。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\ProgramData\Polaris");
        assert!(
            cl.contains(r#"rmdir /s /q "C:\ProgramData\Polaris""#),
            "rmdir 路径未用真双引号包裹: {cl:?}"
        );
        assert!(
            !cl.contains(r#"\""#),
            "命令行含 \\\" 转义引号（cmd 不反转义→路径非法）: {cl:?}"
        );
    }

    #[test]
    fn cmd_line_wrapped_in_cmd_c_quotes() {
        // 整行经 cmd /c 包裹（首尾真引号）：cmd strip 首尾各一引号后内层真引号原样存活。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\ProgramData\Polaris");
        assert!(
            cl.starts_with(r#"cmd /c ""#) && cl.ends_with('"'),
            r#"未用 cmd /c "…" 包裹: {cl:?}"#
        );
    }

    #[test]
    fn cmd_line_step_order_is_stop_delete_rmdir() {
        // 停服务在删服务前、删服务在 rmdir 前（sc delete 要服务先停；rmdir 要 exe 先随 helper 退出解锁）。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\ProgramData\Polaris");
        let i_stop = cl.find("sc stop PolarisHelper");
        let i_del = cl.find("sc delete PolarisHelper");
        let i_rmdir = cl.find("rmdir");
        assert!(
            i_stop.is_some() && i_del.is_some() && i_rmdir.is_some()
                && i_stop < i_del && i_del < i_rmdir,
            "命令顺序应为 stop→delete→rmdir: stop={i_stop:?} del={i_del:?} rmdir={i_rmdir:?} ({cl:?})"
        );
    }

    #[test]
    fn cmd_line_has_two_rmdir_and_ping_delay() {
        // rmdir 出现两次（解锁竞态兜底）；删目录前有 ping 延迟（等 exe 解锁，DETACHED 下不能用 timeout）。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\ProgramData\Polaris");
        assert_eq!(
            cl.matches("rmdir /s /q").count(),
            2,
            "rmdir 应出现两次（兜底）: {cl:?}"
        );
        assert!(cl.contains("ping 127.0.0.1"), "缺 ping 延迟: {cl:?}");
    }

    #[test]
    fn cmd_line_handles_spaced_path() {
        // 服务名/路径含空格时仍正确包裹（虽默认无空格，验证不退化）。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\Program Data\Polaris");
        assert!(
            cl.contains(r#"rmdir /s /q "C:\Program Data\Polaris""#),
            "含空格路径未用真双引号包裹: {cl:?}"
        );
        assert!(!cl.contains(r#"\""#), "含空格路径出现转义引号: {cl:?}");
    }

    #[test]
    fn cmd_line_unsafe_path_uses_placeholder() {
        // 恶意 support_dir（含 cmd 元字符）走安全占位，关闭 SYSTEM 注入窗口。
        let cl = self_uninstall_cmd_line("PolarisHelper", r"C:\evil & calc.exe");
        // 原恶意片段不得出现在最终命令行（防 & 触发 cmd 注入即提权）。
        assert!(
            !cl.contains("evil & calc"),
            "恶意 support_dir 未被占位替换，注入面仍开: {cl:?}"
        );
        assert!(
            cl.contains("safe-placeholder-nonexistent"),
            "恶意路径应改用安全占位: {cl:?}"
        );
    }

    #[test]
    fn is_safe_support_dir_boundary_cases() {
        // Go selfuninstall_test.go TestIsSafeSupportDir 的全部用例。
        let cases: &[(&str, bool)] = &[
            (r"C:\ProgramData\Polaris", true),
            (r"C:\Program Data\Polaris", true), // 空格允许
            ("", false),
            (r"C:\evil & calc", false), // &
            (r#"C:\x"y"#, false),       // 引号
            (r"C:\a|b", false),         // 管道
            (r"D:/path", true),         // 正斜杠允许
            (r"C:\应用\Polaris", true), // 中文 Unicode 允许
        ];
        for &(input, want) in cases {
            assert_eq!(
                is_safe_support_dir(input),
                want,
                "is_safe_support_dir({input:?}) mismatch"
            );
        }
    }

    #[test]
    fn is_safe_support_dir_rejects_all_cmd_metacharacters() {
        // 兜底测全部 cmd 元字符（Go 源用白名单，本测补强黑名单视角）。
        for &bad in &["%", "^", "<", ">", "(", ")", ";", "=", "\t"] {
            assert!(
                !is_safe_support_dir(&format!("C:\\x{bad}y")),
                "应拒绝 cmd 元字符 {bad:?}"
            );
        }
    }

    #[test]
    fn cmd_line_default_args_match_production_constants() {
        // 锁住：用 crate 常量（SERVICE_NAME / DEFAULT_SUPPORT_DIR）拼出的命令行 = 生产路径。
        let cl = self_uninstall_cmd_line(
            crate::platform::windows::SERVICE_NAME,
            crate::platform::windows::DEFAULT_SUPPORT_DIR,
        );
        assert!(cl.contains("sc stop PolarisHelper"));
        assert!(cl.contains("sc delete PolarisHelper"));
        assert!(cl.contains(r#"rmdir /s /q "C:\ProgramData\Polaris""#));
    }
}
