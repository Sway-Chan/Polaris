//! 管理 API 生产实现：把 switch-engine 的 [`ManagementApi`] trait 接到真实 gRPC 客户端。
//!
//! # 这是「两扇门之间的缝」（§K7.1）
//!
//! `switch-engine::executor` 定义了 [`ManagementApi`] trait 并**只有 `#[cfg(test)]` 的
//! `MockManagementApi` 实现**；`singbox-grpc::SingBoxApiClient` 有 `select_outbound` 等真方法。
//! 两侧各自有测试，但**没有任何生产代码把它们接起来** —— 热切换 1815 行因此从未被调用过。
//! 本文件就是那条缺失的接线，也因此**必须由真机测试覆盖**（见 `proxy.rs` 的
//! `real_core_hot_switch_keeps_pid`：真核 + 真 gRPC + 真 PUT，不套 mock）。
//!
//! # 为什么适配器在 src-tauri 而不在 switch-engine
//!
//! switch-engine 是纯逻辑 crate（`Cargo.toml` 无 gRPC 依赖，移植纪律 #1「gRPC 经 trait 抽象」）。
//! 让它依赖 singbox-grpc 会把 tonic/hyper 拖进决策层、并让其单测需要真 channel。
//! src-tauri 的 `runtime/` 是既定的依赖注入点（`src-tauri/Cargo.toml`：「纯逻辑/trait crate；
//! src-tauri 在 runtime/ 注入真实实现」）——两个 crate 都已在其依赖表内，**无需新增依赖**。

//! # 为什么手写 `Pin<Box<dyn Future>>` 而不用 `#[async_trait]`
//!
//! [`ManagementApi`] 用 `#[async_trait]` 声明，实现方通常也挂同一宏 —— 但那要求 src-tauri 新增
//! `async-trait` 依赖，本批**禁止引入新依赖**。`#[async_trait]` 只是把 `async fn` 脱糖成返回
//! `Pin<Box<dyn Future + Send>>` 的普通方法，手写脱糖形态**语义完全等价**且只用 std
//! （简约阶梯：stdlib 能表达就不加依赖）。签名以 `executor.rs` 的 trait 声明为准。

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;

use polaris_singbox_grpc::{ClientError, SingBoxApiClient};
use polaris_switch_engine::{ConnectionSnapshot, ManagementApi, ManagementError};

/// [`ManagementApi`] 的 gRPC 生产实现。
///
/// `client=None` = 管理 API 未就绪（核未起 / 连接失败）→ 所有调用返
/// [`ManagementError::NotReady`]，executor 据此返回 `ClientNotReady`、上层退回重启兜底。
/// 对齐 上游 `hotSwitchSelector` 在 `client=null` 时返 false 的语义。
pub struct GrpcManagementApi {
    client: Option<SingBoxApiClient>,
}

impl GrpcManagementApi {
    /// 就绪实例（已连上管理 API）。
    pub fn new(client: SingBoxApiClient) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// 未就绪实例（核未起 / 连不上）→ 全部调用 NotReady。
    pub fn not_ready() -> Self {
        Self { client: None }
    }

    /// 取客户端，未就绪 → NotReady。
    fn client(&self) -> Result<&SingBoxApiClient, ManagementError> {
        self.client.as_ref().ok_or(ManagementError::NotReady)
    }

    /// 读回各 group 的**运行期选择**（`SubscribeGroups` 首帧）。
    ///
    /// # 为什么是 inherent 方法而不进 [`ManagementApi`] trait
    ///
    /// 那个 trait 是 **switch-engine 的热切换执行器**的依赖面（PUT + 断连三件套），它的每个方法都
    /// 对应 executor 的一步决策。读回运行期选择不属于热切换决策，属**起核后自证**，消费者是
    /// `runtime/proxy.rs` 而非 executor。塞进 trait 只会逼 switch-engine 的 `MockManagementApi`
    /// 与所有 executor 用例陪跑一个它们永不调用的方法（简约阶梯：不为一个消费者扩公共契约）。
    pub async fn groups_snapshot(&self) -> Result<Vec<GroupSelection>, ManagementError> {
        let groups = self
            .client()?
            .first_groups_snapshot()
            .await
            .map_err(map_err)?;
        Ok(groups
            .into_iter()
            .map(|g| GroupSelection {
                tag: g.tag,
                selected: g.selected,
            })
            .collect())
    }
}

/// 一个出站 group 的运行期选择（`daemon::Group` 的最小投影）。
///
/// 只留 `tag`/`selected` 两轴：自证要问的就是「这个 group 现在指着谁」。`items`（成员表 + 测速
/// 历史）不投影 —— 那是节点列表 UI 的数据面，混进来会让本类型跟着 sing-box 的 UI 字段一起漂。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSelection {
    /// group 自身的 outbound tag（如 `proxy-selector` / `rule-sel-r1`）。
    pub tag: String,
    /// 运行期实际选中的成员 tag（服务端 `iGroup.Now()`）。
    pub selected: String,
}

/// gRPC 错误 → trait 错误。
///
/// [`ClientError::SnapshotTimeout`] 单独映射到 [`ManagementError::SnapshotTimeout`]（executor 据此
/// 跳过断连而非误判 PUT 失败）；其余（transport / tonic Status，含 Unauthenticated / DeadlineExceeded）
/// 统一 [`ManagementError::Call`] 并**保留原文**供日志定位。
fn map_err(e: ClientError) -> ManagementError {
    match e {
        ClientError::SnapshotTimeout => ManagementError::SnapshotTimeout,
        other => ManagementError::Call(other.to_string()),
    }
}

impl ManagementApi for GrpcManagementApi {
    fn select_outbound<'a, 'b, 'c, 'async_trait>(
        &'a self,
        selector_tag: &'b str,
        member_tag: &'c str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ManagementError>> + Send + 'async_trait>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        'c: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.client()?
                .select_outbound(selector_tag, member_tag)
                .await
                .map_err(map_err)
        })
    }

    fn close_connection<'a, 'b, 'async_trait>(
        &'a self,
        id: &'b str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ManagementError>> + Send + 'async_trait>>
    where
        'a: 'async_trait,
        'b: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.client()?.close_connection(id).await.map_err(map_err) })
    }

    fn first_connection_snapshot<'a, 'async_trait>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<ConnectionSnapshot>, ManagementError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let conns = self
                .client()?
                .first_connection_snapshot()
                .await
                .map_err(map_err)?;
            // daemon::Connection → ConnectionSnapshot（switch-engine 的最小投影）。
            // chain_list 保序：precision_disconnect 的 pair 谓词按 chains 命中判定，顺序即语义。
            // closed_at 原样透传：>0 = 死连接（历史环幽灵），由 select_connections_to_close 过滤。
            Ok(conns
                .into_iter()
                .map(|c| ConnectionSnapshot {
                    id: c.id,
                    chains: c.chain_list,
                    closed_at: c.closed_at,
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未就绪实例：三个方法全部 NotReady（而非 panic / 静默 Ok）。
    /// NotReady 是 executor 区分「核未起」与「PUT 报错」的依据，两者都退回重启但日志不同。
    #[tokio::test]
    async fn not_ready_client_returns_not_ready_for_all_methods() {
        let api = GrpcManagementApi::not_ready();
        assert!(matches!(
            api.select_outbound("proxy-selector", "tagB").await,
            Err(ManagementError::NotReady)
        ));
        assert!(matches!(
            api.close_connection("c1").await,
            Err(ManagementError::NotReady)
        ));
        assert!(matches!(
            api.first_connection_snapshot().await,
            Err(ManagementError::NotReady)
        ));
    }

    /// 读侧同样必须 NotReady 而**不是空快照**：空快照 = 「核确实没有 group」，NotReady = 「没读到」。
    /// 压成前者会让起核自证把「读不到」误当成「查无此 group」，从而对真分叉保持沉默。
    ///
    /// **变异锁**：把 `groups_snapshot` 的 `self.client()?` 换成 `unwrap_or_default()` 式回落
    /// （返回 `Ok(vec![])`）→ 转红。
    #[tokio::test]
    async fn not_ready_client_returns_not_ready_for_groups_snapshot() {
        let api = GrpcManagementApi::not_ready();
        assert!(matches!(
            api.groups_snapshot().await,
            Err(ManagementError::NotReady)
        ));
    }

    /// SnapshotTimeout 必须单独成态，不得被压成 Call —— executor 对二者的处置不同
    /// （超时 → 跳过断连；Call → 也跳过但日志语义不同），且上层据此判「核 wedged」。
    #[test]
    fn snapshot_timeout_maps_to_dedicated_variant() {
        assert!(matches!(
            map_err(ClientError::SnapshotTimeout),
            ManagementError::SnapshotTimeout
        ));
    }

    /// tonic Status → Call 且**保留原文**（丢了原文，真机 PUT 失败时无从定位是 Unauthenticated 还是 Unavailable）。
    #[test]
    fn tonic_status_maps_to_call_preserving_message() {
        let e = map_err(ClientError::Status(
            polaris_singbox_grpc::tonic::Status::unauthenticated("bad secret"),
        ));
        match e {
            ManagementError::Call(msg) => assert!(
                msg.contains("bad secret"),
                "必须保留 gRPC 原文，实得：{msg}"
            ),
            other => panic!("expected Call, got {other:?}"),
        }
    }
}
