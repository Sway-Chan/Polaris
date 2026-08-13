//! 生产 [`Connector`] 实现 —— Unix socket（mac/linux）+ 命名管道（win）。
//!
//! ## 为什么在这里
//!
//! [`client::HelperClient`](crate::client::HelperClient) 经 [`Connector`] 工厂建连接（每请求一连接，对齐
//! 上游 `net.connect(SOCKET_PATH)` 模型）。测试注入返回 [`MockStream`](crate::transport::MockStream)
//! 的 mock；**生产注入本模块的 [`UnixConnector`]（mac/linux）/ [`PipeConnector`]（win）** —— 这两处正是
//! `client.rs` / `transport.rs` 文档反复引用、但此前只有 mock、真实现缺失的落点（§2.4 装配级 missing）。
//!
//! ## 平台差异（真差异，cfg 隔离）
//!
//! - **mac/linux**：`std::os::unix::net::UnixStream::connect` + `set_read_timeout(5s)`（移植 Go
//!   `conn.SetReadDeadline`）。写完请求帧后 `shutdown(Write)` 半关闭 = 上游 `sock.end(frame)`，通知
//!   helper「请求发完」（helper 据此立即处理并回响应，不必等 5s 读超时）。
//! - **win**：命名管道 `\\.\pipe\polaris-helper`。std 无「命名管道客户端 + 读超时」的安全封装
//!   （`SetCommTimeouts`/overlapped 需 FFI，违 `forbid(unsafe_code)`），故用 `OpenOptions::open`
//!   打开管道（返回 `File`）复用 [`IoAdapter`](crate::transport::IoAdapter)（逐行读 + no-op shutdown）。
//!   **读超时缺口**（真机门）：helper 侧 5s 读超时会主动关管道 → client 读到 EOF 收口，故实践中不会永挂；
//!   但 client 侧无独立读超时，纯静默的 helper 需靠 helper 侧超时兜底（见报告真机门）。
//!
//! ## 移植纪律
//!
//! - `forbid(unsafe_code)`：`UnixStream` / `File` / `OpenOptions` 全是安全 std，无裸 syscall。
//! - trait 边界只暴露字节流原语（[`ConnectionStream`]），三平台 [`HelperClient`] 共用。

use crate::client::{ClientError, Connector};
use crate::transport::ConnectionStream;
#[cfg(unix)]
use crate::transport::READ_TIMEOUT;
#[cfg(unix)]
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

// ===== Unix socket 连接器（mac/linux）=====

/// Unix domain socket 连接器（mac/linux 生产 [`Connector`]）。
///
/// 每次 [`Connector::connect`] 打开一条到 `socket_path` 的新连接并设 5s 读超时（对齐 Go
/// `SetReadDeadline`）。连接拒绝（socket 不存在/helper 未跑）→ [`ClientError::Connect`]。
#[cfg(unix)]
pub struct UnixConnector {
    socket_path: PathBuf,
    read_timeout: Duration,
}

#[cfg(unix)]
impl UnixConnector {
    /// 用默认读超时（[`READ_TIMEOUT`] = 5s）构造。
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            read_timeout: READ_TIMEOUT,
        }
    }

    /// 自定义读超时构造。
    #[must_use]
    pub fn with_timeout(socket_path: impl Into<PathBuf>, read_timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            read_timeout,
        }
    }

    /// socket 路径。
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(unix)]
impl Connector for UnixConnector {
    fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(&self.socket_path).map_err(|e| {
            ClientError::Connect(format!("connect {}: {e}", self.socket_path.display()))
        })?;
        // Go conn.SetReadDeadline(5s) 等价：SO_RCVTIMEO，读阻塞超时返回 WouldBlock/TimedOut。
        stream
            .set_read_timeout(Some(self.read_timeout))
            .map_err(ClientError::Io)?;
        Ok(Box::new(UnixConnStream { stream }))
    }
}

/// [`UnixStream`](std::os::unix::net::UnixStream) 的 [`ConnectionStream`] 包装。
///
/// [`ConnectionStream::shutdown`] 走 `Shutdown::Write` 半关闭（= 上游 `sock.end()`）—— 这正是
/// [`IoAdapter`](crate::transport::IoAdapter) 的 no-op shutdown 无法提供、必须由本包装实现的语义。
#[cfg(unix)]
struct UnixConnStream {
    stream: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl ConnectionStream for UnixConnStream {
    fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        use std::io::Read;
        // 逐字节读到 `\n`（行协议每行 `\n` 结尾）；EOF 返回已读字节数（helper 关连接）。
        let mut byte = [0u8; 1];
        let mut total = 0;
        loop {
            match self.stream.read(&mut byte) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    buf.push(byte[0]);
                    total += 1;
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                // SO_RCVTIMEO 超时：Linux 回 WouldBlock。归一为 TimedOut（对齐 trait 契约）。
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue; // EINTR 重试，不算超时
                    }
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.stream.write_all(data)
    }

    fn shutdown(&mut self) -> io::Result<()> {
        // Polaris sock.end(frame)：半关闭写端，通知 helper「请求已发完」。
        self.stream.shutdown(std::net::Shutdown::Write)
    }
}

// ===== 命名管道连接器（win）=====

/// Windows 命名管道连接器（win 生产 [`Connector`]）。
///
/// 打开 `\\.\pipe\polaris-helper`（`OpenOptions::read+write`）复用 [`IoAdapter`](crate::transport::IoAdapter)
/// 的逐行读；写端无需半关闭（helper win 侧「裸 HANDLE 整帧读」按 `\n` 判帧完整，不依赖 EOF）。
#[cfg(windows)]
pub struct PipeConnector {
    pipe_path: PathBuf,
}

#[cfg(windows)]
impl PipeConnector {
    /// 用管道路径构造（默认 `\\.\pipe\polaris-helper`，由调用方从 [`InstallPaths`](crate::manager::InstallPaths) 取）。
    #[must_use]
    pub fn new(pipe_path: impl Into<PathBuf>) -> Self {
        Self {
            pipe_path: pipe_path.into(),
        }
    }

    /// 管道路径。
    #[must_use]
    pub fn pipe_path(&self) -> &Path {
        &self.pipe_path
    }
}

#[cfg(windows)]
impl Connector for PipeConnector {
    fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
        use crate::transport::IoAdapter;
        use std::fs::OpenOptions;
        // 命名管道客户端：双向打开（读响应 + 写请求）。管道忙（ERROR_PIPE_BUSY）/ 不存在 → Connect 错误，
        // 由 HelperClient::send_with_retry 在「装完等 daemon 起来」场景重试。
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.pipe_path)
            .map_err(|e| {
                ClientError::Connect(format!("open pipe {}: {e}", self.pipe_path.display()))
            })?;
        Ok(Box::new(IoAdapter::new(file)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use polaris_helper_proto::{Platform, Request};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// 起一个单次 accept 的 UnixListener（tempdir 内的本地 IPC socket，**非宿主网络** —— 不碰
    /// netns/veth/iptables/TUN，等同临时文件，安全隔离），读请求帧、回预置响应。
    fn spawn_echo_server(path: PathBuf, response: &'static [u8]) -> thread::JoinHandle<Vec<u8>> {
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // 读到 EOF（client shutdown(Write) 触发）或读满一小段。
            let mut got = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                match conn.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        got.extend_from_slice(&chunk[..n]);
                        // 请求帧以 `\n` 结尾；client 半关闭后我们读到 EOF，但也可提前在收到整帧后停。
                        if got.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            conn.write_all(response).unwrap();
            // 回完即 drop → client 读到响应 + EOF。
            got
        })
    }

    #[test]
    fn unix_connector_roundtrip_ping() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("helper.sock");
        let server = spawn_echo_server(sock.clone(), b"OK pong uid=0 v9\n");
        // 用 UnixConnector 建 HelperClient，发 ping，验证真连接 + 编帧 + 读响应往返。
        let connector = UnixConnector::new(sock);
        let client = crate::client::HelperClient::new(Box::new(connector), Platform::Mac, "TOK");
        let resp = client.send(&Request::Ping).unwrap();
        assert!(matches!(
            resp,
            polaris_helper_proto::Response::Ok(polaris_helper_proto::response::ResponseKind::Pong(
                _
            ))
        ));
        // 服务端收到的请求帧 = mac wire "TOK\nping\n"（验证 shutdown 半关闭让服务端读到完整帧 + EOF）。
        let got = server.join().unwrap();
        assert_eq!(got, b"TOK\nping\n");
    }

    #[test]
    fn unix_connector_connect_refused_when_no_socket() {
        // socket 路径不存在 → connect 失败（helper 未装/未跑，对齐 Polaris sock.on('error')）。
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent.sock");
        let connector = UnixConnector::new(missing);
        // Box<dyn ConnectionStream> 无 Debug，不能 unwrap_err；用 match 取错误。
        match connector.connect() {
            Err(ClientError::Connect(_)) => {}
            Ok(_) => panic!("expected connect refused"),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unix_connector_exposes_socket_path() {
        let connector = UnixConnector::new("/run/polaris/helper.sock");
        assert_eq!(
            connector.socket_path(),
            Path::new("/run/polaris/helper.sock")
        );
    }
}
