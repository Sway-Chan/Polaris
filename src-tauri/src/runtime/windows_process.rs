//! Windows 进程探活与身份令牌。
//!
//! 主进程原先每次探活都启动 `tasklist /FI ...`；`.207` 真机单次约 3.5 秒，既阻塞 TUN
//! 起核收口，也让崩溃监测持续制造外部进程。这里复用 helper 已验证的
//! `OpenProcess + GetExitCodeProcess` 原语，并用创建时间作为比映像名更强的 PID 身份令牌。

#![allow(unsafe_code)]

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_INVALID_PARAMETER, FALSE, FILETIME, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

const STILL_ACTIVE_CODE: u32 = 259;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    Dead,
    Unknown,
}

/// 只把 Win32 明确给出的“不存在/已退出”当死亡；权限或探针失败均保守判未知。
fn liveness_from_probe(open_error: Option<u32>, exit_code: Option<u32>) -> Liveness {
    if open_error == Some(ERROR_INVALID_PARAMETER) {
        return Liveness::Dead;
    }
    if open_error.is_some() || exit_code.is_none() {
        return Liveness::Unknown;
    }
    if exit_code == Some(STILL_ACTIVE_CODE) {
        Liveness::Alive
    } else {
        Liveness::Dead
    }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

/// 返回 `(存活判据, 创建时间 tick)`；创建时间只在明确存活且 API 可读时提供。
fn probe(pid: u32) -> (Liveness, Option<u64>) {
    if pid == 0 {
        return (Liveness::Dead, None);
    }
    // SAFETY: OpenProcess 只请求查询/同步权限，不修改目标；pid 来自 helper 或本进程记账。
    let handle =
        unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if handle.is_null() {
        // SAFETY: 紧邻失败的 OpenProcess，线程未穿插其它 Win32 调用。
        let error = unsafe { GetLastError() };
        return (liveness_from_probe(Some(error), None), None);
    }
    // SAFETY: OpenProcess 成功返回尚未被 Rust 所有者接管的独占 HANDLE。
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

    let mut exit_code = 0u32;
    // SAFETY: handle 在调用期间有效，exit_code 是可写的本栈对象。
    let exit_ok = unsafe { GetExitCodeProcess(raw(&handle), &mut exit_code) };
    let liveness = liveness_from_probe(None, if exit_ok == 0 { None } else { Some(exit_code) });
    if liveness != Liveness::Alive {
        return (liveness, None);
    }

    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: handle 明确存活且在调用期间有效；四个 FILETIME 均为可写本栈对象。
    let times_ok = unsafe {
        GetProcessTimes(
            raw(&handle),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let creation_ticks = (times_ok != 0)
        .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime));
    (liveness, creation_ticks)
}

/// 无死亡证据即判活，保持原跨权限安全方向；只移除外部 `tasklist` 进程。
pub(crate) fn is_alive(pid: u32) -> bool {
    probe(pid).0 != Liveness::Dead
}

/// PID 身份令牌：Windows 进程创建时间（100ns tick）。同 PID 被复用时该值必变化。
pub(crate) fn creation_identity(pid: u32) -> Option<String> {
    let (liveness, created) = probe(pid);
    (liveness == Liveness::Alive)
        .then_some(created)
        .flatten()
        .map(|ticks| format!("{ticks:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_only_treats_explicit_absence_as_dead() {
        assert_eq!(
            liveness_from_probe(Some(ERROR_INVALID_PARAMETER), None),
            Liveness::Dead
        );
        assert_eq!(liveness_from_probe(Some(5), None), Liveness::Unknown);
        assert_eq!(liveness_from_probe(None, None), Liveness::Unknown);
        assert_eq!(
            liveness_from_probe(None, Some(STILL_ACTIVE_CODE)),
            Liveness::Alive
        );
        assert_eq!(liveness_from_probe(None, Some(0)), Liveness::Dead);
    }
}
