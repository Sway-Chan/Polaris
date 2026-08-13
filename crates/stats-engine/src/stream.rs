//! gRPC 流注入抽象 —— 把 B2-b singbox-grpc 的真实 tonic 流与 B5 纯逻辑层解耦。
//!
//! 任务契约（B5 纯逻辑部分）：「流数据用 trait 注入（`trait StatsStream { fn next(&mut self) -> ... }`），
//! 测试 mock」。本 crate 不含 gRPC 流接收——B5 actor 在集成层把 tonic `Streaming<Status>` /
//! `Streaming<ConnectionEvents>` 适配成 [`StatusStream`] / [`ConnectionEventStream`]，喂给
//! [`crate::aggregator::StatsAggregator`]。
//!
//! 设计：分两条 trait（而非泛型单 trait）——Status 帧与 ConnectionEvents 帧是不同类型，且消费语义不同
//! （Status 更新 snapshot，ConnectionEvents 维护 connMap），分轨更清晰，也避开泛型分发难题
//! （对齐 singbox-grpc reconnect.rs 用 trait object 而非泛型的同一决策）。
//!
//! 同步 `next`：纯逻辑层用同步 trait（测试用 [`VecStream`] 即时返回），上层 actor 在 async 上下文
//! 里 `.await tonic 流` 后同步调 `next` 适配（流驱动循环在 actor，不在本 crate）。

use crate::types::{SingBoxConnectionEvents, SingBoxStatus};

/// Status 流注入抽象（B2-b singbox-grpc 在集成层适配）。
///
/// `next` 返回下一帧；流结束返回 None。错误由实现方自行处理/重试（对齐 singbox-grpc ReconnectingStream
/// 永不向消费方 yield 错误的语义）——本 trait 只关心「拿到帧」。
pub trait StatusStream {
    fn next(&mut self) -> Option<SingBoxStatus>;
}

/// Connections 事件流注入抽象。
pub trait ConnectionEventStream {
    fn next(&mut self) -> Option<SingBoxConnectionEvents>;
}

/// 测试用：从预置 Vec 派发的流。Vec 耗尽后返回 None。
#[derive(Debug, Default)]
pub struct VecStatusStream {
    frames: std::vec::IntoIter<SingBoxStatus>,
}

impl VecStatusStream {
    pub fn new(frames: Vec<SingBoxStatus>) -> Self {
        Self {
            frames: frames.into_iter(),
        }
    }
}

impl StatusStream for VecStatusStream {
    fn next(&mut self) -> Option<SingBoxStatus> {
        self.frames.next()
    }
}

/// 测试用：从预置 Vec 派发的 Connections 事件流。
#[derive(Debug, Default)]
pub struct VecConnectionEventStream {
    frames: std::vec::IntoIter<SingBoxConnectionEvents>,
}

impl VecConnectionEventStream {
    pub fn new(frames: Vec<SingBoxConnectionEvents>) -> Self {
        Self {
            frames: frames.into_iter(),
        }
    }
}

impl ConnectionEventStream for VecConnectionEventStream {
    fn next(&mut self) -> Option<SingBoxConnectionEvents> {
        self.frames.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_status_stream_yields_in_order_then_none() {
        let mut s = VecStatusStream::new(vec![
            SingBoxStatus {
                uplink: 1,
                ..Default::default()
            },
            SingBoxStatus {
                uplink: 2,
                ..Default::default()
            },
        ]);
        assert_eq!(s.next().unwrap().uplink, 1);
        assert_eq!(s.next().unwrap().uplink, 2);
        assert!(s.next().is_none());
    }

    #[test]
    fn vec_status_stream_empty_yields_none_immediately() {
        let mut s = VecStatusStream::new(vec![]);
        assert!(s.next().is_none());
    }

    #[test]
    fn vec_connection_event_stream_yields_then_none() {
        let mut s = VecConnectionEventStream::new(vec![
            SingBoxConnectionEvents::default(),
            SingBoxConnectionEvents {
                reset: true,
                ..Default::default()
            },
        ]);
        assert!(!s.next().unwrap().reset);
        assert!(s.next().unwrap().reset);
        assert!(s.next().is_none());
    }

    #[test]
    fn status_stream_is_object_safe() {
        // 确认可用 trait object（dyn StatusStream）——上层 actor 会装箱多种实现。
        let s: Box<dyn StatusStream> = Box::new(VecStatusStream::new(vec![SingBoxStatus {
            uplink: 42,
            ..Default::default()
        }]));
        let mut s = s;
        assert_eq!(s.next().unwrap().uplink, 42);
    }

    #[test]
    fn connection_event_stream_is_object_safe() {
        let s: Box<dyn ConnectionEventStream> = Box::new(VecConnectionEventStream::new(vec![]));
        let mut s = s;
        assert!(s.next().is_none());
    }
}
