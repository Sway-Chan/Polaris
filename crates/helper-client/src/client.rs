//! [`HelperClient`] —— 主进程侧 helper socket 客户端。
//!
//! ## 职责（移植自 上游 `HelperManager.ts:433-457` 的 `sendCommand`）
//!
//! 连接 helper socket/pipe → 发 [`Request`]（复用 `helper-proto::codec::encode` 编帧）→ 读单行
//! [`Response`]（复用 `helper-proto::Response::parse` 解码）→ 关连接。每次请求一连接（Polaris 即此模型：
//! `net.connect` → `sock.end(frame)` → 读 `data`+`end` → 销毁，无长连接）。
//!
//! ## 连接抽象
//!
//! 生产侧 socket/pipe 与测试 mock 经 [`ConnectionStream`](crate::transport::ConnectionStream) trait 解耦。
//! [`HelperClient`] 持有一个 [`Connector`]（连接工厂）：每次 [`HelperClient::send`] 调
//! [`Connector::connect`] 建新连接。生产注入 `UnixConnector`/pipe connector，测试注入返回
//! [`MockStream`](crate::transport::MockStream) 的闭包。
//!
//! ## token 行鉴权
//!
//! mac/win 带 token 行（[`Platform::has_token_line`]），linux 无（SO_PEERCRED）。token 由调用方提供
//! （从 [`token::read_token`] 读到的 app 侧 token 文件）。
//!
//! ## 重连 / 超时
//!
//! - **超时**：单请求超时由调用方传入（默认 [`DEFAULT_REQUEST_TIMEOUT_MS`]，install-core 用 [`INSTALL_CORE_TIMEOUT_MS`]）。
//!   超时返回 [`ClientError::Timeout`]（对齐 上游 `helper socket 超时`，HelperManager.ts:441）。
//! - **重连**：[`HelperClient::send_with_retry`] 在连接失败 / 超时时按策略重试（默认 0 次 —— Polaris sendCommand
//!   不重试，调用方决定。重试用于「刚 install 完等 daemon 起来」场景，对齐 Polaris install 后轮询就绪
//!   `HelperManager.ts:519-522`）。
//!
//! ## 移植纪律
//!
//! 1. 复用 `helper-proto::codec::encode` + `Response::parse`，不重写帧。
//! 2. socket/pipe 经 trait 抽象，测试 mock（不碰宿主）。
//! 3. `forbid(unsafe_code)`。

#[cfg(test)]
use crate::transport::INSTALL_CORE_TIMEOUT_MS;
use crate::transport::{ConnectionStream, DEFAULT_REQUEST_TIMEOUT_MS};
use polaris_helper_proto::codec;
use polaris_helper_proto::{Platform, Request, Response};
#[cfg(test)]
use std::io;
use std::time::{Duration, Instant};

/// helper 连接工厂 trait —— 每次调用返回一个新的已连接 [`ConnectionStream`]。
///
/// 抽象 `net.connect(SOCKET_PATH)`（上游 `HelperManager.ts:435`）。生产实现打开 Unix socket / 命名管道；
/// 测试实现返回 [`MockStream`](crate::transport::MockStream)。
pub trait Connector: Send {
    /// 建立一条新连接。失败返回 [`ClientError`]（连接拒绝 = helper 未装/未跑，对齐 上游 `sock.on('error')`）。
    fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError>;
}

/// helper 客户端错误（对齐 Polaris 的 reject 路径：超时 / 连接错误 / IO 错误 / 协议错误）。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 连接 helper 失败（socket/pipe 不存在 = helper 未安装或未运行）。
    #[error("连接 helper 失败: {0}")]
    Connect(String),
    /// 读响应超时（对齐 上游 `helper socket 超时`，HelperManager.ts:441）。
    #[error("helper socket 超时")]
    Timeout,
    /// IO 错误（读写失败、对端关闭等）。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    /// 空响应（连接建立但未读到数据，对齐 上游 `helper 无响应`）。
    #[error("helper 无响应")]
    EmptyResponse,
}

/// helper 客户端 —— 发送 [`Request`] 并接收 [`Response`]。
///
/// 持有 [`Connector`]（连接工厂）+ 平台标识 + token。
///
/// # 用法
///
/// ```ignore
/// use polaris_helper_client::{HelperClient, Platform, transport::MockStream};
///
/// // 生产：注入 UnixConnector / pipe connector
/// // 测试：注入返回 MockStream 的闭包
/// let client = HelperClient::new(
///     platform_connector,
///     Platform::Mac,
///     "my-token",
/// );
/// let resp = client.send(&Request::Ping).unwrap();
/// ```
pub struct HelperClient {
    connector: Box<dyn Connector>,
    platform: Platform,
    token: String,
}

impl HelperClient {
    /// 构造客户端。`connector` 负责建连接，`token` 用于 mac/win 鉴权行（linux 忽略）。
    pub fn new(
        connector: Box<dyn Connector>,
        platform: Platform,
        token: impl Into<String>,
    ) -> Self {
        Self {
            connector,
            platform,
            token: token.into(),
        }
    }

    /// 发送一个请求，默认超时 [`DEFAULT_REQUEST_TIMEOUT_MS`]（ping/status 等短命令）。
    ///
    /// 一次连接一次请求（对齐 Polaris sendCommand 模型）。
    pub fn send(&self, req: &Request) -> Result<Response, ClientError> {
        self.send_with_timeout(req, Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS))
    }

    /// 发送一个请求，自定义超时。
    ///
    /// install-core 用 [`INSTALL_CORE_TIMEOUT_MS`]（sha256 + 大文件复制耗时长，HelperManager.ts:421）。
    pub fn send_with_timeout(
        &self,
        req: &Request,
        timeout: Duration,
    ) -> Result<Response, ClientError> {
        let deadline = Instant::now() + timeout;
        // 1. 建连接
        let mut conn = self.connector.connect().map_err(|e| {
            log::warn!("helper 连接失败: {e}");
            e
        })?;
        // 2. 编帧（复用 helper-proto codec）
        let frame = codec::encode(self.platform, &self.token, req);
        // 3. 写帧 + shutdown（对齐 Polaris sock.end(frame)，HelperManager.ts:438）
        conn.write_all(&frame).map_err(ClientError::Io)?;
        conn.shutdown().map_err(ClientError::Io)?;
        // 4. 读单行响应（行协议：响应一行 \n 结尾）
        let line = read_response_line(&mut *conn, deadline)?;
        // 5. 解析响应（复用 helper-proto Response::parse）
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(ClientError::EmptyResponse);
        }
        Ok(Response::parse(trimmed))
    }

    /// 带重试的发送：连接失败 / 超时时按 `retry_delay` 间隔重试 `max_retries` 次。
    ///
    /// 用于「install 后等 daemon 起来」场景（上游 `HelperManager.ts:519-522` 轮询就绪）：
    /// daemon 注册到 launchd/systemd/SCM 后绑定 socket 需要时间，首次 ping 可能 ECONNREFUSED。
    pub fn send_with_retry(
        &self,
        req: &Request,
        timeout: Duration,
        max_retries: u32,
        retry_delay: Duration,
    ) -> Result<Response, ClientError> {
        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                std::thread::sleep(retry_delay);
            }
            match self.send_with_timeout(req, timeout) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    log::debug!("helper 请求第 {attempt} 次失败: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or(ClientError::EmptyResponse))
    }

    /// 更新 token（install/uninstall 后 app 侧 token 文件变化时）。
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// 当前 token。
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// 当前平台。
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }
}

/// 读完整一行响应（到 `\n` 或 EOF），受 deadline 超时约束。
///
/// 行协议：helper 回单行 `\n` 结尾（`fmt.Fprintln(conn, ...)`）。读到 `\n` 即完整响应。
fn read_response_line(
    conn: &mut dyn ConnectionStream,
    deadline: Instant,
) -> Result<String, ClientError> {
    let mut buf = Vec::new();
    loop {
        // 超时检查
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        let mut byte_buf = Vec::new();
        match conn.read_until_timeout(&mut byte_buf)? {
            0 => {
                // EOF —— helper 关闭连接，返回当前已读内容（可能为空）
                break;
            }
            n => {
                buf.extend_from_slice(&byte_buf[..n]);
                // 检查是否读到行尾
                if buf.last() == Some(&b'\n') {
                    break;
                }
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockStream;
    use polaris_helper_proto::response::{ResponseKind, Status};
    use std::sync::{Arc, Mutex};

    /// 测试用 connector：每次 connect 返回一个预置的 MockStream。
    struct MockConnector {
        streams: Arc<Mutex<Vec<MockStream>>>,
    }

    impl MockConnector {
        fn new(streams: Vec<MockStream>) -> Self {
            Self {
                streams: Arc::new(Mutex::new(streams)),
            }
        }
    }

    impl Connector for MockConnector {
        fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
            let mut guard = self.streams.lock().unwrap();
            if guard.is_empty() {
                return Err(ClientError::Connect("no mock stream".into()));
            }
            Ok(Box::new(guard.remove(0)))
        }
    }

    fn client_with(streams: Vec<MockStream>) -> (HelperClient, Arc<Mutex<Vec<MockStream>>>) {
        let c = MockConnector::new(streams);
        let streams_ref = c.streams.clone();
        let client = HelperClient::new(Box::new(c), Platform::Mac, "TOK");
        (client, streams_ref)
    }

    /// 连接级失败 connector：connect() 直接返回 Connect 错误（模拟 helper 未装/未跑）。
    struct FailingConnector {
        message: &'static str,
    }
    impl Connector for FailingConnector {
        fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
            Err(ClientError::Connect(self.message.into()))
        }
    }

    /// 序列 connector：按预设顺序返回「连接成功（MockStream）」或「连接失败（Connect 错误）」。
    /// 用于重连测试：首次失败、二次成功等。
    enum ConnAttempt {
        Ok(MockStream),
        Fail(String),
    }
    struct SequenceConnector {
        attempts: Arc<Mutex<Vec<ConnAttempt>>>,
    }
    impl Connector for SequenceConnector {
        fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
            let mut g = self.attempts.lock().unwrap();
            if g.is_empty() {
                return Err(ClientError::Connect("no attempts".into()));
            }
            match g.remove(0) {
                ConnAttempt::Ok(s) => Ok(Box::new(s)),
                ConnAttempt::Fail(m) => Err(ClientError::Connect(m)),
            }
        }
    }

    #[test]
    fn ping_roundtrip_parses_pong() {
        // Polaris helper.go:423: OK pong uid=0 v9
        let mock = MockStream::with_response(b"OK pong uid=0 v9\n".to_vec());
        let (client, _) = client_with(vec![mock]);
        let resp = client.send(&Request::Ping).unwrap();
        match resp {
            Response::Ok(ResponseKind::Pong(p)) => {
                assert_eq!(p.uid, 0);
                assert_eq!(p.proto_version, 9);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn status_roundtrip_parses_running() {
        // Polaris helper.go:427: OK running <pid>
        let mock = MockStream::with_response(b"OK running 4242\n".to_vec());
        let (client, _) = client_with(vec![mock]);
        let resp = client.send(&Request::Status).unwrap();
        match resp {
            Response::Ok(ResponseKind::Status(Status::Running { pid })) => {
                assert_eq!(pid, 4242);
            }
            other => panic!("expected Status Running, got {other:?}"),
        }
    }

    #[test]
    fn err_response_routes_to_err_variant() {
        // Polaris helper.go:406: ERR auth（token 不匹配）
        let mock = MockStream::with_response(b"ERR auth\n".to_vec());
        let (client, _) = client_with(vec![mock]);
        let resp = client.send(&Request::Ping).unwrap();
        assert!(matches!(resp, Response::Err(_)));
    }

    #[test]
    fn written_frame_matches_wire_protocol() {
        // 验证 client 编帧正确：mac 帧 = "TOK\nping\n"（对照 helper-proto codec test）
        // 用一个共享结构捕获写入字节（MockStream 写入后会被 client 消费，无法直接读回）
        struct CapturingConnector {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl Connector for CapturingConnector {
            fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
                let cap = self.captured.clone();
                Ok(Box::new(CapturingMock { captured: cap }))
            }
        }
        struct CapturingMock {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl ConnectionStream for CapturingMock {
            fn read_until_timeout(&mut self, _buf: &mut Vec<u8>) -> io::Result<usize> {
                Ok(0) // EOF
            }
            fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
                self.captured.lock().unwrap().extend_from_slice(data);
                Ok(())
            }
            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let captured = Arc::new(Mutex::new(Vec::new()));
        let client = HelperClient::new(
            Box::new(CapturingConnector {
                captured: captured.clone(),
            }),
            Platform::Mac,
            "TOK",
        );
        // 即使读 EOF 报 EmptyResponse，写入侧已捕获帧
        let _ = client.send(&Request::Ping);
        let written = captured.lock().unwrap().clone();
        assert_eq!(written, b"TOK\nping\n");
    }

    #[test]
    fn linux_frame_omits_token_line() {
        // linux 经 SO_PEERCRED 无 token 行（helper-linux/helper.go:343）
        struct CaptureConnector {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl Connector for CaptureConnector {
            fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
                let cap = self.captured.clone();
                Ok(Box::new(CaptureMock { captured: cap }))
            }
        }
        struct CaptureMock {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl ConnectionStream for CaptureMock {
            fn read_until_timeout(&mut self, _buf: &mut Vec<u8>) -> io::Result<usize> {
                Ok(0)
            }
            fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
                self.captured.lock().unwrap().extend_from_slice(data);
                Ok(())
            }
            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let captured = Arc::new(Mutex::new(Vec::new()));
        let client = HelperClient::new(
            Box::new(CaptureConnector {
                captured: captured.clone(),
            }),
            Platform::Linux,
            "ignored-token",
        );
        let _ = client.send(&Request::Ping);
        // linux 帧无 token 行：直接 "ping\n"
        assert_eq!(*captured.lock().unwrap(), b"ping\n");
    }

    #[test]
    fn start_roundtrip_full_frame() {
        // 验证 start 完整帧（mac: TOK/start/cfg/log/fwd/ppid）
        struct CaptureConnector {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl Connector for CaptureConnector {
            fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
                let cap = self.captured.clone();
                Ok(Box::new(CaptureMock { captured: cap }))
            }
        }
        struct CaptureMock {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl ConnectionStream for CaptureMock {
            fn read_until_timeout(&mut self, _buf: &mut Vec<u8>) -> io::Result<usize> {
                Ok(0)
            }
            fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
                self.captured.lock().unwrap().extend_from_slice(data);
                Ok(())
            }
            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        use polaris_helper_proto::StartParams;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let client = HelperClient::new(
            Box::new(CaptureConnector {
                captured: captured.clone(),
            }),
            Platform::Mac,
            "TOK",
        );
        let req = Request::Start(StartParams {
            cfg: "/tmp/c.json".into(),
            log: "/tmp/l.log".into(),
            fwd: true,
            parent_pid: Some(1000),
        });
        let _ = client.send(&req);
        // 对照 helper-proto::tests::mac_start_frame_full
        assert_eq!(
            *captured.lock().unwrap(),
            b"TOK\nstart\n/tmp/c.json\n/tmp/l.log\n1\n1000\n"
        );
    }

    #[test]
    fn connect_failure_returns_connect_error() {
        // helper 未装 / 未跑 → 连接拒绝（connect() 阶段失败）
        let client = HelperClient::new(
            Box::new(FailingConnector {
                message: "connection refused",
            }),
            Platform::Mac,
            "TOK",
        );
        let err = client.send(&Request::Ping).unwrap_err();
        assert!(matches!(err, ClientError::Connect(_)));
    }

    #[test]
    fn empty_response_returns_error() {
        // helper 连上但无响应（上游 `helper 无响应`，HelperManager.ts:321）
        let mock = MockStream::with_response(b"".to_vec());
        let (client, _) = client_with(vec![mock]);
        let err = client.send(&Request::Ping).unwrap_err();
        assert!(matches!(err, ClientError::EmptyResponse));
    }

    #[test]
    fn retry_succeeds_on_second_attempt() {
        // install 后等 daemon 起来：首次连接失败，二次就绪
        let conn = SequenceConnector {
            attempts: Arc::new(Mutex::new(vec![
                ConnAttempt::Fail("connection refused".into()),
                ConnAttempt::Ok(MockStream::with_response(b"OK pong uid=0 v9\n".to_vec())),
            ])),
        };
        let client = HelperClient::new(Box::new(conn), Platform::Mac, "TOK");
        let resp = client
            .send_with_retry(
                &Request::Ping,
                Duration::from_millis(500),
                3,
                Duration::from_millis(1),
            )
            .unwrap();
        assert!(matches!(resp, Response::Ok(ResponseKind::Pong(_))));
    }

    #[test]
    fn retry_exhausted_returns_last_error() {
        let conn = SequenceConnector {
            attempts: Arc::new(Mutex::new(vec![
                ConnAttempt::Fail("connection refused".into()),
                ConnAttempt::Fail("connection refused".into()),
            ])),
        };
        let client = HelperClient::new(Box::new(conn), Platform::Mac, "TOK");
        let err = client
            .send_with_retry(
                &Request::Ping,
                Duration::from_millis(500),
                1,
                Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(err, ClientError::Connect(_)));
    }

    #[test]
    fn install_core_uses_long_timeout() {
        // install-core 默认超时 30s（HelperManager.ts:421）
        let mock = MockStream::with_response(b"OK installed\n".to_vec());
        let (client, _) = client_with(vec![mock]);
        use polaris_helper_proto::InstallCoreParams;
        let req = Request::InstallCore(InstallCoreParams {
            src_dir: "/tmp/staging".into(),
            want_hash: "a".repeat(64),
        });
        let resp = client
            .send_with_timeout(&req, Duration::from_millis(INSTALL_CORE_TIMEOUT_MS))
            .unwrap();
        assert!(matches!(resp, Response::Ok(ResponseKind::Installed)));
    }

    #[test]
    fn set_token_updates_auth_token() {
        let mock = MockStream::with_response(b"OK pong uid=0 v9\n".to_vec());
        let (mut client, _) = client_with(vec![mock]);
        client.set_token("new-token");
        assert_eq!(client.token(), "new-token");
    }

    #[test]
    fn platform_accessor() {
        let mock = MockStream::with_response(b"OK\n".to_vec());
        let (client, _) = client_with(vec![mock]);
        assert_eq!(client.platform(), Platform::Mac);
    }

    #[test]
    fn partial_response_assembled_across_reads() {
        // 模拟 helper 响应分多个 TCP 段到达（Polaris sock.on('data') 累积，HelperManager.ts:444-446）
        struct ChunkedMock {
            chunks: Vec<Vec<u8>>,
            pos: usize,
        }
        impl ConnectionStream for ChunkedMock {
            fn read_until_timeout(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
                if self.pos >= self.chunks.len() {
                    return Ok(0);
                }
                let chunk = &self.chunks[self.pos];
                self.pos += 1;
                buf.extend_from_slice(chunk);
                Ok(chunk.len())
            }
            fn write_all(&mut self, _data: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct ChunkedConnector;
        impl Connector for ChunkedConnector {
            fn connect(&self) -> Result<Box<dyn ConnectionStream>, ClientError> {
                Ok(Box::new(ChunkedMock {
                    chunks: vec![b"OK pong".to_vec(), b" uid=0 v9\n".to_vec()],
                    pos: 0,
                }))
            }
        }
        let client = HelperClient::new(Box::new(ChunkedConnector), Platform::Mac, "T");
        let resp = client.send(&Request::Ping).unwrap();
        match resp {
            Response::Ok(ResponseKind::Pong(p)) => {
                assert_eq!(p.proto_version, 9);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }
}
