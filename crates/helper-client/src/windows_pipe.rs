//! Windows 同步命名管道客户端。
//!
//! 与 helper 服务端 `platform/windows/service/win.rs` 使用同一组同步 Win32 原语。
//! 该模块是 crate 中唯一允许 unsafe 的边界；上层仍只看 `ConnectionStream`。

#![allow(unsafe_code)]

use crate::transport::ConnectionStream;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_BROKEN_PIPE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

/// 独占拥有一条双向同步命名管道连接。
pub(crate) struct WinPipeStream {
    handle: OwnedHandle,
}

impl WinPipeStream {
    /// 以与真机原生探针相同的 access/share/flags 打开现有管道实例。
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` 在调用期间存活且以 NUL 结尾；security/template 均为空；返回的有效
        // HANDLE 立即转交 OwnedHandle 独占，失败值不进入所有权包装。
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW 成功返回一枚尚未被任何 Rust 所有者接管的独占 HANDLE。
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
        Ok(Self { handle })
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle().cast()
    }
}

impl ConnectionStream for WinPipeStream {
    fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let mut byte = 0u8;
            let mut read = 0u32;
            // SAFETY: handle 由 self 独占且在调用期间有效；byte/read 均为可写的本栈对象；
            // lpOverlapped=NULL 明确选择同步 ReadFile，与服务端及真机探针一致。
            let ok = unsafe { ReadFile(self.raw(), &mut byte, 1, &mut read, std::ptr::null_mut()) };
            if ok == 0 {
                // SAFETY: 紧邻失败的 ReadFile，线程未穿插其它 Win32 调用。
                let error = unsafe { GetLastError() };
                if error == ERROR_BROKEN_PIPE {
                    break;
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            if read == 0 {
                break;
            }
            buf.push(byte);
            total += 1;
            if byte == b'\n' {
                break;
            }
        }
        Ok(total)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let len = u32::try_from(data.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut written = 0u32;
        // SAFETY: handle 由 self 独占且有效；data 在调用期间不可变且长度为 len；written 可写；
        // lpOverlapped=NULL 明确选择同步单次 WriteFile，满足服务端“一次写完整帧”的 wire 约束。
        let ok = unsafe {
            WriteFile(
                self.raw(),
                data.as_ptr(),
                len,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written != len {
            return Err(io::ErrorKind::WriteZero.into());
        }
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        // Windows duplex pipe 无写半关闭；服务端按单次完整帧读取，不依赖 EOF。
        Ok(())
    }
}
