//! 统计/连接聚合对外类型 —— 上游 `shared/types/runtime.ts` 子集 1:1 镜像。
//!
//! 锚点：
//! - [`TrafficStats`] = runtime.ts:212 `TrafficStats`（首页速率/累计/活跃连接数）。
//! - [`ConnectionEntry`] = runtime.ts:94 `ConnectionEntry`（main 裁剪后的单条连接）。
//! - [`ConnectionsSnapshot`] = runtime.ts:115 `ConnectionsSnapshot`（连接明细快照，detail topic）。
//! - [`TOPOLOGY_OTHERS_KEY`] = runtime.ts:122 `'\u0000others'`（Top-N 截断后合并的 sentinel host 名）。
//! - [`ConnectionsAggregate`] / [`ConnectionAggHost`] / [`ConnectionAggFlow`] / [`ConnectionAggOutbound`]
//!   = runtime.ts:148/131/125/138（首页拓扑聚合，issue #227）。
//!
//! gRPC 上游帧类型（Polaris singbox-api-client.ts:221/241/266/275）：[`SingBoxStatus`] /
//! [`SingBoxConnection`] / [`SingBoxConnectionEvent`] / [`SingBoxConnectionEvents`]。
//! proto 字段在 Rust 侧 prost 生成为 i64 / String，本模块用与上游一致的原生类型（非 Option 化：
//! proto3 proto 默认值即 0/""——上游 longs=String 转 number 后语义等价）。

use serde::{Deserialize, Serialize};

/// 流量统计快照（首页速率条）。1:1 `TrafficStats`（runtime.ts:212）。
///
/// **键名必须逐字等于 TS 契约**（同 [`ConnectionMetadata`] 的理由）：本结构直接 `Serialize` 出 IPC
/// （`EVENT_STATS_UPDATED` 的载荷），前端按 `uploadSpeed` / `totalUpload` 这些名字读。
/// 少了 `rename_all` 就会整帧变成 `upload_speed` 之类的下划线名，**两侧类型系统都不会报错**，
/// 表现只是状态栏五个数字全成 `undefined`。键名契约由 `traffic_stats_json_keys_match_ts_contract` 守住。
///
/// # 速率不是核给的，是我们自己按实测 Δt 算的
///
/// 早先这里写「速率由 sing-box 直接给出（无需本地 delta/dt 自算）」——**是错的**，据此接线会得到
/// 恒 0 或严重失真的读数。sing-box 的 `Status.uplink` / `downlink` 并非速率：
/// `readStatus()`（`daemon/started_service.go:417`）**从不**给这两个字段赋值，是 `SubscribeStatus`
/// 的循环里每拍算 `status.UplinkTotal - uploadTotal` 再写回去（:408-413）——即「**上一拍到这一拍的
/// 字节增量**」，而且**首帧在任何 tick 之前就 `Send`，两者恒 0**。要把它折成速率就得除以那段窗口，
/// 而窗口长度（服务端 ticker 的实际间隔，含调度抖动）**根本不在 wire 上**。
///
/// 故速率一律由消费方对 `total_upload` / `total_download` 做差分、除以**客户端实测的 Δt**
/// （见 [`crate::aggregator::StatsAggregator::on_status`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficStats {
    /// 上行速率（bytes/s）：`total_upload` 的跨帧差分 ÷ 客户端实测 Δt。**不是** `Status.uplink`。
    pub upload_speed: u64,
    /// 下行速率（bytes/s）：`total_download` 的跨帧差分 ÷ 客户端实测 Δt。**不是** `Status.downlink`。
    pub download_speed: u64,
    /// 累计上行字节（Status.uplinkTotal）。口径 = **本次核启动至今单调累加**
    /// （`trafficcontrol.Manager.Total()` 读两个只增的 `atomic.Int64`，连接关闭时 `leave()` 不减）。
    /// 换核 / 核重启后从 0 重新开始——那是新的一条核生命线，不是回退。
    pub total_upload: u64,
    /// 累计下行字节（Status.downlinkTotal）。口径同 [`Self::total_upload`]。
    pub total_download: u64,
    /// 活跃连接数。**两个写入者，取决于喂进来的是哪条流**：
    /// - [`crate::aggregator::StatsAggregator::on_status`] → `Status.connectionsIn`
    ///   （= 内核 `trafficManager.ConnectionsLen()`，即 `SubscribeConnections` 首帧里活连接的条数）；
    /// - [`crate::aggregator::StatsAggregator::on_connection_events`] → `conn_map.len()`
    ///   （我们自己维护的连接表，**已滤掉测速探测池**，故可能比上一条小）。
    ///
    /// 生产里两条 relay 各持一个聚合器实例、各只喂一种帧，故不冲突。⚠️ 将来若把两条流喂进**同一个**
    /// 实例，本字段会在「内核口径」与「过滤后口径」之间跳变——届时必须先定哪个是真值，
    /// 别指望两个写入者自洽。
    pub active_connections: u32,
}

impl TrafficStats {
    /// 全零快照（`stop()`/`resubscribe()` 归零广播基线）。
    pub const fn zeroed() -> Self {
        Self {
            upload_speed: 0,
            download_speed: 0,
            total_upload: 0,
            total_download: 0,
            active_connections: 0,
        }
    }
}

/// 连接元数据（隐私字段 source_ip/process_path 出 IPC 由渲染端在隐私模式屏蔽，决策）。
/// 1:1 `ConnectionEntry.metadata`（runtime.ts:99）。
///
/// **键名必须逐字等于 TS 契约**：本结构直接 `Serialize` 出 IPC，前端按 `destinationIP` /
/// `processPath` 这些名字读。此前整个结构没有任何 serde 重命名，于是八个字段里**只有
/// `host` / `network` 两个（恰好单词形态相同）真的送达**，其余六个前端恒读到 undefined ——
/// 连接页「目标」「进程」两列全是 `—`、源 IP 子行从不出现、L4 类型只剩 network 回落
/// （陈先生 2026-07-29 真机报「目标主要记录的是什么，当前显示的都是 -」）。
///
/// 三个名字不能靠 `rename_all = "camelCase"` 推出来，逐条钉死：`destinationIP` / `sourceIP`
/// 的 `IP` 是全大写（camelCase 会给出 `destinationIp`），`type` 是 TS 侧的字段名而非 `inboundType`。
/// 键名契约由 `connection_metadata_json_keys_match_ts_contract` 守住。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionMetadata {
    /// 目标域名（gRPC Connection.domain）。
    pub host: Option<String>,
    /// 目标 IP（拆 Connection.destination）。
    #[serde(rename = "destinationIP")]
    pub destination_ip: Option<String>,
    /// tcp/udp（Connection.network）。
    pub network: Option<String>,
    /// 入站类型（Connection.inboundType，如 Tun/HTTP/Socks）。TS 侧字段名是 `type`。
    #[serde(rename = "type")]
    pub inbound_type: Option<String>,
    /// 源 IP（拆 Connection.source，隐私字段）。
    #[serde(rename = "sourceIP")]
    pub source_ip: Option<String>,
    /// 源端口（拆 Connection.source）。
    pub source_port: Option<String>,
    /// 目标端口（拆 Connection.destination）。
    pub destination_port: Option<String>,
    /// 发起进程路径（Connection.processInfo.processPath，隐私字段）。
    pub process_path: Option<String>,
}

/// 单条连接（main 裁剪后子集）。1:1 `ConnectionEntry`（runtime.ts:94）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    pub id: String,
    pub chains: Vec<String>,
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ConnectionMetadata>,
    /// 累计上行字节（Connection.uplinkTotal）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<u64>,
    /// 累计下行字节（Connection.downlinkTotal）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<u64>,
    /// 连接建立时刻（RFC3339，由 createdAt 转换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
}

/// 连接明细快照（detail topic 订阅载荷）。1:1 `ConnectionsSnapshot`（runtime.ts:115）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsSnapshot {
    pub connections: Vec<ConnectionEntry>,
    /// 采样时刻 epoch ms。
    pub at: u64,
}

/// 已结束连接。与活跃连接表分轨保存，避免历史记录重新进入 `conn_map` 污染活动数与拓扑。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedConnectionEntry {
    pub entry: ConnectionEntry,
    /// 连接结束时刻，sing-box UnixNano。
    pub closed_at: i64,
}

/// 已结束连接历史全量快照（命令式读取 / 清空返回值）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsClosedSnapshot {
    /// 最新结束的连接在前；生产者负责有界保存。
    pub connections: Vec<ClosedConnectionEntry>,
    /// 采样时刻 epoch ms。
    pub at: u64,
}

/// 已结束连接的 closed topic 推送帧。订阅首帧 / 内核 reset / 用户清空用 `reset=true`
/// 下发完整有界快照；常态只下发本批 upsert 与被上限淘汰的 id。
///
/// 这个类型与 [`ConnectionsClosedSnapshot`] 分开：后者是命令式“读当前全量”的
/// 返回值，本类型是长驻事件管道，不应因新结束一条就重复序列化全部 1000 条。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsClosedUpdate {
    /// true = `connections` 取代前端全部历史；false = 按 id upsert。
    pub reset: bool,
    /// reset 帧是完整快照；增量帧只含新增/变更条目。
    pub connections: Vec<ClosedConnectionEntry>,
    /// 由于 1000 条上限或同步校正而需从前端移除的连接 id。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_ids: Vec<String>,
    /// 采样时刻 epoch ms。
    pub at: u64,
}

/// 拓扑「其它」分组 sentinel（runtime.ts:122 `'\u0000others'`）。
///
/// 用控制字符前缀确保绝不与真实 host/IP/rule 名冲突。渲染端 topology-layout 见此值 → 替换为
/// i18n 文案 `t('home.others')`。导出为 `&str` 供聚合/签名/断言共享。
pub const TOPOLOGY_OTHERS_KEY: &str = "\u{0}others";

/// 拓扑列 host 节点上限（与渲染端原 MAX_NODES 对齐；connections-aggregate.ts:18 `TOPOLOGY_TOP_N`）。
pub const TOPOLOGY_TOP_N: usize = 15;

/// 某目标（host）→ 单个出口的连接数分布项。1:1 `ConnectionAggFlow`（runtime.ts:125）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionAggFlow {
    pub outbound: String,
    pub count: u32,
}

/// 按目标聚合的一组连接：host/destIP/rule 显示名 → 连接数 + 各出口分布。1:1 `ConnectionAggHost`（runtime.ts:131）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionAggHost {
    pub name: String,
    pub count: u32,
    pub flows: Vec<ConnectionAggFlow>,
}

/// 按出口聚合的连接数（topology 右列节点）。1:1 `ConnectionAggOutbound`（runtime.ts:138）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionAggOutbound {
    pub name: String,
    pub count: u32,
}

/// 连接聚合快照（首页拓扑专用）。1:1 `ConnectionsAggregate`（runtime.ts:148）。
///
/// hosts 已按 count 降序、截断 Top-N（剩余并入 [`TOPOLOGY_OTHERS_KEY`]）。outbounds 按 count 降序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsAggregate {
    /// 活跃连接总数。
    pub total: u32,
    pub hosts: Vec<ConnectionAggHost>,
    pub outbounds: Vec<ConnectionAggOutbound>,
    /// 采样时刻 epoch ms（签名比对时剔除）。
    pub at: u64,
}

// ── gRPC 上游帧（Polaris singbox-api-client.ts:221/241/266/275，longs=String → 此处用 i64 原生）────

/// sing-box 进程信息（gRPC ProcessInfo）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SingBoxProcessInfo {
    pub process_id: u32,
    pub user_id: u32,
    pub user_name: String,
    pub process_path: String,
    pub package_names: Vec<String>,
}

/// gRPC Connection（singbox-api-client.ts:241）。proto3 默认值即 0/""，全字段非 Option（与上游语义一致）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SingBoxConnection {
    pub id: String,
    pub inbound: String,
    pub inbound_type: String,
    pub ip_version: i32,
    pub network: String,
    pub source: String,
    pub destination: String,
    pub domain: String,
    pub protocol: String,
    pub user: String,
    pub from_outbound: String,
    /// unix 纳秒（sing-box time.Time.UnixNano 序列化）。
    pub created_at: i64,
    /// >0 表示已关闭（历史环里的死连接，NEW 时丢弃）。
    pub closed_at: i64,
    pub uplink: i64,
    pub downlink: i64,
    pub uplink_total: i64,
    pub downlink_total: i64,
    pub rule: String,
    pub outbound: String,
    pub outbound_type: String,
    pub chain_list: Vec<String>,
    pub process_info: SingBoxProcessInfo,
}

/// gRPC Status 帧（singbox-api-client.ts:221）。字段类型逐条对齐 proto
/// （`proto/started_service.proto` 的 `message Status`），不自行改宽窄或改语义。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SingBoxStatus {
    /// proto `uint64 memory = 1`。
    pub memory: u64,
    pub goroutines: i32,
    /// 内核 `trafficManager.ConnectionsLen()` = **当前活连接数**。
    ///
    /// ⚠️ 早先本模块注释写「connectionsIn/Out 核不填、恒 0」——**是错的**：
    /// `readStatus()`（`daemon/started_service.go:417`）在 `trafficManager != nil` 时填 `ConnectionsIn`
    /// （daemon gRPC 走 `needAPIService` 分支，该 manager 必被构造，见 `box.go:245`），
    /// 在 `connectionManager != nil` 时填 `ConnectionsOut`（`box.go:233` **无条件**注册）。
    /// 两个字段都不恒 0，据「恒 0」下的任何结论都要重判。
    pub connections_in: i32,
    /// 内核 `connectionManager.Count()`。同上，不恒 0。
    pub connections_out: i32,
    /// proto `bool trafficAvailable = 5`（此前误 typed 成 `i64`）。
    ///
    /// `false` = 核没有 `trafficManager` ⇒ 本帧的 `uplink_total` / `downlink_total` / `connections_in`
    /// **全是安静的 0**（`SubscribeStatus` 不做任何前置校验、不报错）。消费方必须显式判它，
    /// 否则「0 B/s 且零报错」与「真的没流量」无从区分。
    pub traffic_available: bool,
    /// ⚠️ **不是速率**：服务端 ticker 上一拍到这一拍的字节增量，且首帧恒 0。理由与正解见
    /// [`TrafficStats`] 的「速率不是核给的」。
    pub uplink: i64,
    /// ⚠️ **不是速率**，同 [`Self::uplink`]。
    pub downlink: i64,
    /// 本次核启动至今的单调累计上行字节（`trafficcontrol.Manager.Total()`，只增不减）。
    pub uplink_total: i64,
    /// 本次核启动至今的单调累计下行字节。
    pub downlink_total: i64,
}

/// gRPC ConnectionEvent.type（proto enum，singbox-api-client.ts:267）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionEventType {
    /// proto3 默认值 = NEW（enum 0）。
    #[default]
    New,
    Update,
    Closed,
}

/// gRPC ConnectionEvent（singbox-api-client.ts:266）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SingBoxConnectionEvent {
    pub kind: ConnectionEventType,
    pub id: String,
    pub connection: Option<SingBoxConnection>,
    pub uplink_delta: i64,
    pub downlink_delta: i64,
    pub closed_at: i64,
}

/// gRPC ConnectionEvents 帧（singbox-api-client.ts:275）：增量事件 + reset 全量重置标志。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SingBoxConnectionEvents {
    pub events: Vec<SingBoxConnectionEvent>,
    pub reset: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出 IPC 的 JSON 键名必须逐字等于 TS 契约 `ConnectionEntry.metadata`
    /// （`ui/src/contracts/types/runtime.ts`）。
    ///
    /// 这条门存在的理由是它抓到过的那次回归：整个结构漏了 serde 重命名，八个字段只有
    /// `host`/`network` 送达，前端「目标」「进程」两列恒 `—`。**没有任何既有门会红** ——
    /// Rust 侧类型自洽、TS 侧类型自洽，错的只是两侧对同一个 JSON 的命名约定，
    /// 而那份 JSON 从不被任何一侧的类型系统看见。
    ///
    /// 判据是**全等**而非「包含」：多出的键同样是错（前端读不到 = 白送流量，
    /// 且说明两侧又对不上了）。
    #[test]
    fn connection_metadata_json_keys_match_ts_contract() {
        let m = ConnectionMetadata {
            host: Some("example.com".into()),
            destination_ip: Some("1.2.3.4".into()),
            network: Some("tcp".into()),
            inbound_type: Some("Tun".into()),
            source_ip: Some("192.168.1.2".into()),
            source_port: Some("54321".into()),
            destination_port: Some("443".into()),
            process_path: Some("/usr/bin/curl".into()),
        };
        let v = serde_json::to_value(&m).expect("metadata 应可序列化");
        let mut got: Vec<&str> = v
            .as_object()
            .expect("metadata 应是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        let mut want = [
            "host",
            "destinationIP",
            "network",
            "type",
            "sourceIP",
            "sourcePort",
            "destinationPort",
            "processPath",
        ];
        want.sort_unstable();
        assert_eq!(got, want, "metadata 的 JSON 键名与 TS 契约不一致");
    }

    /// 🔴 出 IPC 的 JSON 键名必须逐字等于 TS 契约 `TrafficStats`
    /// （`ui/src/contracts/types/runtime.ts:257`）。
    ///
    /// 与 `connection_metadata_json_keys_match_ts_contract` 同一类风险，只是这条更晚才成立：
    /// 本结构此前从不出 IPC（`runtime/stats.rs` 手拼 `json!` 逐个写 camelCase 键），改成直接
    /// `Serialize` 之后，缺 `rename_all` 就会整帧变成 `upload_speed` 这类下划线名 ——
    /// **Rust 侧与 TS 侧各自自洽、两边类型系统都不报错**，表现只是状态栏五个数字全空。
    ///
    /// 判据是**全等**而非「包含」：多出的键同样是错（前端读不到 = 白送流量）。
    #[test]
    fn traffic_stats_json_keys_match_ts_contract() {
        let v = serde_json::to_value(TrafficStats::zeroed()).expect("TrafficStats 应可序列化");
        let mut got: Vec<&str> = v
            .as_object()
            .expect("TrafficStats 应是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        let mut want = [
            "uploadSpeed",
            "downloadSpeed",
            "totalUpload",
            "totalDownload",
            "activeConnections",
        ];
        want.sort_unstable();
        assert_eq!(got, want, "TrafficStats 的 JSON 键名与 TS 契约不一致");
    }

    #[test]
    fn closed_update_json_keys_match_ts_contract() {
        let v = serde_json::to_value(ConnectionsClosedUpdate {
            reset: false,
            connections: Vec::new(),
            removed_ids: vec!["gone".into()],
            at: 1,
        })
        .expect("ConnectionsClosedUpdate 应可序列化");
        let mut got: Vec<&str> = v
            .as_object()
            .expect("closed update 应是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        let mut want = ["reset", "connections", "removedIds", "at"];
        want.sort_unstable();
        assert_eq!(got, want, "closed update 的 JSON 键名与 TS 契约不一致");
    }

    /// 重命名后**反序列化仍认得自己写出去的键**（Serialize/Deserialize 对称）。
    /// 不对称的话，任何「序列化落盘 → 读回」的路径都会静默丢字段。
    #[test]
    fn connection_metadata_roundtrips_through_json() {
        let m = ConnectionMetadata {
            host: Some("a.example".into()),
            destination_ip: Some("9.9.9.9".into()),
            inbound_type: Some("HTTP".into()),
            source_ip: Some("10.0.0.1".into()),
            ..Default::default()
        };
        let back: ConnectionMetadata =
            serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
