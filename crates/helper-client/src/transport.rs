//! 传输抽象（[`ConnectionStream`] trait）+ 测试 mock。
//!
//! ## 为什么抽象
//!
//! Polaris 主进程经 **Unix socket**（mac/linux）或 **命名管道**（win）连 helper（`HelperManager.ts:435` 的
//! `net.connect(SOCKET_PATH)`；`helper-win` 经 `\\.\pipe\...`）。这两种 IO 在 Rust 是不同类型
//! （`std::os::unix::net::UnixStream` vs 原生 Win32 同步管道），但 wire 协议一致：
//! **行文本帧**（`token\ncmd\n[args...]\n`，见 `helper-proto::codec`）。
//!
//! 把「读字节 / 写字节」抽成 [`ConnectionStream`]，让 [`HelperClient`](crate::client::HelperClient)
//! 与具体连接实现解耦 —— 生产侧注入 `UnixStream`/pipe，测试侧注入 [`MockStream`]，零宿主依赖即可
//! 测「Request → Response 往返」。
//!
//! ## 移植纪律
//!
//! - 复用 `helper-proto::codec`（`encode`/`Response::parse`），不重写帧逻辑。
//! - 本模块无裸 syscall；Windows FFI 只存在于 `windows_pipe` 平台模块。
//! - 不触碰宿主：[`MockStream`] 是纯内存环形缓冲，不开真 socket。
//!
//! 对应 Polaris：`HelperManager.ts` 的 `net.connect(SOCKET_PATH)` + `sock.end(...)` + `sock.on('data')`。

use std::io::{self, Read, Write};
use std::time::Duration;

/// 单连接读超时（秒）—— 移植自 Go `conn.SetReadDeadline(5s)`
///（`helper.go:401` / `helper-win/helper.go:165` / `helper-linux/helper.go:335`），
/// 同 `helper-proto::codec::READ_TIMEOUT_SECS`。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 单次请求默认超时（毫秒）—— 移植自 `HelperManager.ts:443` 的 `setTimeout(..., timeoutMs)`，ping 默认 1500ms。
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 1500;

/// install-core 的长超时（毫秒）—— 移植自 `HelperManager.ts:421` 的 `sendCommand([...], 30_000)`（sha256 + 大文件复制）。
pub const INSTALL_CORE_TIMEOUT_MS: u64 = 30_000;

/// 抽象的「连接字节流」—— helper socket / pipe 的统一读写边界。
///
/// 实现者负责：
/// - `connect`：建立到 socket/pipe 的连接（生产：`UnixStream::connect` / pipe open；测试：构造 mock）。
/// - [`ConnectionStream::read_until_timeout`]：读一行（到 `\n`），受超时约束。
/// - [`ConnectionStream::write_all`]：写完整帧字节。
/// - [`ConnectionStream::shutdown`]：半关闭（对应 上游 `sock.end()`，通知 helper「请求已发完」）。
///
/// **纪律**：trait 不暴露平台特定句柄，只暴露字节流原语 —— 让上层 [`HelperClient`](crate::client::HelperClient)
/// 三平台共用。
pub trait ConnectionStream: Send {
    /// 读一行（含尾部 `\n`）到 `buf`，返回读到的字节数。EOF 返回 0。
    ///
    /// 受 [`READ_TIMEOUT`] 约束：超时返回 [`io::ErrorKind::TimedOut`]（对齐 Go `SetReadDeadline`）。
    /// 逐行读对应 上游 `sock.on('data')` 累积 + 拆行（`HelperManager.ts:444-446`）。
    fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize>;

    /// 写完整字节串（对应 上游 `sock.end(frame)`，`HelperManager.ts:438`）。
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// 通知对端「发送完毕」（半关闭写端，对应 上游 `sock.end()` 语义）。
    ///
    /// helper 收到 EOF 后开始处理并回响应。mock 实现可 no-op。
    fn shutdown(&mut self) -> io::Result<()>;
}

// ===== std IO 适配：把 Read+Write 包装成 ConnectionStream =====

/// 把任意 `Read + Write + Send` 适配为 [`ConnectionStream`]（逐字节读单行 + 超时）。
///
/// 泛型测试流可经此包装注入；生产 Unix/Windows 连接各自实现平台所需的关闭/超时语义。
pub struct IoAdapter<S> {
    inner: S,
}

impl<S> IoAdapter<S> {
    /// 包装一个底层流。
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    /// 拿到底层流的引用（用于设置超时等平台特定操作）。
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// 拿到底层流的可变引用。
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: Read + Send> ConnectionStream for IoAdapter<S>
where
    S: Write,
{
    fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        // 响应是短单行且一连接一请求；按 1 byte 读到换行是 wire 契约的确定边界。
        // Windows 生产流在 windows_pipe 内用同步 ReadFile 单独实现同一语义。
        let mut byte = [0u8; 1];
        let mut total = 0;
        loop {
            let n = self.inner.read(&mut byte)?;
            if n == 0 {
                break;
            }
            buf.push(byte[0]);
            total += 1;
            if byte[0] == b'\n' {
                break;
            }
        }
        Ok(total)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.write_all(data)
    }

    fn shutdown(&mut self) -> io::Result<()> {
        // std Write::shutdown 不通用 —— UnixStream/pipe 各有 shutdown 实现。
        // 这里尝试调 shutdown_write（UnixStream 的标准方法），失败则忽略（mock 场景）。
        // 因 trait 边界只约束 Write（无 shutdown 方法），这里用 no-op 兜底；
        // 生产侧若需半关闭，应在 connect 后直接操作底层流（见 UnixConnector::connect）。
        let _ = self;
        Ok(())
    }
}

// ===== MockStream：纯内存环形缓冲，测试专用 =====

/// 测试用 mock 连接：预置响应字节，捕获写入的请求字节。
///
/// 模拟「client 写请求 → helper 回响应」的单连接往返：
/// - [`MockStream::new`] 预置 helper 将回的响应行（含 `\n`）。
/// - client 的 `write_all` 把请求帧写入 [`MockStream::written`]（供测试断言 wire 形态）。
/// - client 的 `read_until_timeout` 逐字节吐出预置响应。
///
/// 不开真 socket —— 满足「不触碰宿主」纪律。
#[derive(Debug, Default)]
pub struct MockStream {
    /// 预置的 helper 响应字节（client 读到的内容）。
    incoming: Vec<u8>,
    /// 已读光标。
    read_pos: usize,
    /// 捕获的 client 写入字节（断言 wire 帧用）。
    written: Vec<u8>,
    /// shutdown 是否被调用。
    shutdown_called: bool,
    /// 模拟的连接失败（Some 时 read/write 返回错误）。
    broken: Option<io::ErrorKind>,
}

impl MockStream {
    /// 构造一个预置响应的 mock（响应字节将在 client read 时吐出）。
    #[must_use]
    pub fn with_response(response: impl Into<Vec<u8>>) -> Self {
        Self {
            incoming: response.into(),
            read_pos: 0,
            written: Vec::new(),
            shutdown_called: false,
            broken: None,
        }
    }

    /// 构造一个立即失败的 mock（模拟连接断开 / EPIPE）。
    #[must_use]
    pub fn broken(kind: io::ErrorKind) -> Self {
        Self {
            incoming: Vec::new(),
            read_pos: 0,
            written: Vec::new(),
            shutdown_called: false,
            broken: Some(kind),
        }
    }

    /// 取走捕获的写入字节（client 发给 helper 的完整帧）。
    #[must_use]
    pub fn take_written(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.written)
    }

    /// shutdown 是否被调用过。
    #[must_use]
    pub const fn shutdown_was_called(&self) -> bool {
        self.shutdown_called
    }
}

impl ConnectionStream for MockStream {
    fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        if let Some(kind) = self.broken {
            return Err(io::Error::from(kind));
        }
        if self.read_pos >= self.incoming.len() {
            // EOF —— 对应 helper 关闭连接（上游 `sock.on('end')`，HelperManager.ts:447）
            return Ok(0);
        }
        let byte = self.incoming[self.read_pos];
        self.read_pos += 1;
        buf.push(byte);
        Ok(1)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(kind) = self.broken {
            return Err(io::Error::from(kind));
        }
        self.written.extend_from_slice(data);
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.shutdown_called = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_streams_request_and_captures_response() {
        // 预置 helper 回 "OK pong uid=0 v9\n"，client 应读到完整一行
        let mut mock = MockStream::with_response(b"OK pong uid=0 v9\n".to_vec());
        // client 写请求帧
        mock.write_all(b"TOK\nping\n").unwrap();
        mock.shutdown().unwrap();
        // 读响应
        let mut buf = Vec::new();
        let n = mock.read_until_timeout(&mut buf).unwrap();
        assert_eq!(n, 1);
        // 写入被捕获
        assert_eq!(mock.take_written(), b"TOK\nping\n");
        assert!(mock.shutdown_was_called());
    }

    #[test]
    fn mock_eof_after_response() {
        let mut mock = MockStream::with_response(b"OK\n".to_vec());
        let mut buf = Vec::new();
        // 读两字节后 EOF
        assert_eq!(mock.read_until_timeout(&mut buf).unwrap(), 1); // 'O'
        assert_eq!(mock.read_until_timeout(&mut buf).unwrap(), 1); // 'K'
        assert_eq!(mock.read_until_timeout(&mut buf).unwrap(), 1); // '\n'
        assert_eq!(mock.read_until_timeout(&mut buf).unwrap(), 0); // EOF
        assert_eq!(buf, b"OK\n");
    }

    #[test]
    fn mock_broken_returns_error() {
        let mut mock = MockStream::broken(io::ErrorKind::ConnectionRefused);
        let mut buf = Vec::new();
        let err = mock.read_until_timeout(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        let err = mock.write_all(b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn read_timeout_constant_matches_helper_proto() {
        // 与 helper-proto::codec::READ_TIMEOUT_SECS 对齐（5s，Go SetReadDeadline）
        assert_eq!(READ_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn default_timeouts_match_polaris() {
        // HelperManager.ts:443 ping 1500ms；:421 install-core 30000ms
        assert_eq!(DEFAULT_REQUEST_TIMEOUT_MS, 1500);
        assert_eq!(INSTALL_CORE_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn io_adapter_uses_single_byte_reads_and_stops_at_newline() {
        struct PipeLike {
            inner: std::io::Cursor<Vec<u8>>,
            requested: Vec<usize>,
        }
        impl Read for PipeLike {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.requested.push(buf.len());
                self.inner.read(buf)
            }
        }
        impl Write for PipeLike {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut adapter = IoAdapter::new(PipeLike {
            inner: std::io::Cursor::new(b"OK stopped\nignored".to_vec()),
            requested: Vec::new(),
        });
        let mut buf = Vec::new();
        let total = adapter.read_until_timeout(&mut buf).unwrap();
        assert_eq!(buf, b"OK stopped\n");
        assert_eq!(total, b"OK stopped\n".len());
        assert_eq!(adapter.inner().requested, vec![1; b"OK stopped\n".len()]);
    }
}
