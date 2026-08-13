//! h2c（HTTP/2 cleartext，非 TLS）通道。
//!
//! sing-box 管理 API 本地端点（127.0.0.1:&lt;port&gt;）走 h2c——不上 TLS。tonic 的
//! [`tonic::transport::Channel`] 默认连接器假定 TLS；h2c 需用 `connect_with_connector(_lazy)`
//! 传入一个**只做 TCP 连接**的 [`tower::Service<Uri>`]，tonic 内部经
//! `hyper::client::conn::http2::Builder::handshake` 跑明文 HTTP/2 握手（prior knowledge，无 ALPN）。
//!
//! 故本连接器只负责 `host:port` → `TcpStream`；h2 body 适配 / stream 多路复用全归 tonic+hyper。
//! 对齐 Polaris：`grpc.credentials.createInsecure()` 在 h2c 上跑 HTTP/2 明文。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::transport::Channel;
use tower_service::Service;
// 与 `reconnect.rs` 同一理由换 `log` 门面：本仓只给 `log` 接了 sink（`src-tauri/src/logging.rs`），
// `tracing` 无 subscriber ⇒ 恒为 no-op。同 crate 内不留两套门面，否则「这条日志到底会不会落盘」
// 又要逐文件确认一次。
use log::debug;

/// 连接到 sing-box 管理 API（h2c，明文 HTTP/2）。
///
/// `target` = `host:port`（裸 IPv6 字面量已方括号包裹）。返回一条 **lazy** tonic Channel——
/// 不立即建连，首个 RPC 时才握 TCP + h2。复用同 channel 的所有 RPC 走同一条 h2 连接。
pub(crate) async fn connect_h2c(target: &str) -> Result<Channel, tonic::transport::Error> {
    // tonic 0.14：from_shared 接收 Into<Bytes>（字符串即可）。h2c = `http://` scheme（明文）。
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{target}"))?
        // 关键：用自定义 TCP 连接器覆盖默认 TLS 连接器；tonic 内部跑 h2 明文握手（prior knowledge）。
        .connect_with_connector_lazy(TcpConnector);
    Ok(endpoint)
}

/// 明文 TCP 连接器：把 `http://host:port` Uri 解析出 host:port，TCP connect，返回字节流。
/// tonic 自行在其上跑 hyper h2 明文握手。
#[derive(Clone)]
struct TcpConnector;

impl Service<http::Uri> for TcpConnector {
    type Response = hyper_util::rt::TokioIo<tokio::net::TcpStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // 总是就绪——每连接独立 TCP 握手，无背压。
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| io("h2c connector: uri missing host"))?;
            let port = uri.port_u16().unwrap_or(80);
            // 去掉 IPv6 字面量的方括号（`[::1]` → `::1`），传给 tokio 解析。
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let addr = format!("{host}:{port}");

            debug!("h2c：TCP 拨号 {addr}");
            let tcp = tokio::net::TcpStream::connect(&addr)
                .await
                .map_err(|e| io(&format!("h2c TCP connect {addr}: {e}")))?;
            // TokioIo 适配 tokio AsyncRead/Write → hyper rt::Read/Write（tonic 要求）。
            Ok(hyper_util::rt::TokioIo::new(tcp))
        })
    }
}

fn io(msg: &str) -> std::io::Error {
    std::io::Error::other(msg)
}
