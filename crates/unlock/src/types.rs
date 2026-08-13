//! 检测状态/结果类型 —— main/renderer 共用 SoT。
//!
//! 1:1 移植自 上游 `src/shared/unlock-detection.ts`（纯类型 + 常量，零逻辑）。
//! 业务含义见 Polaris 同名文件注释；此处仅说明 Rust 化的差异。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 检测状态。
///
/// - `Idle`：初始/未检测/gating（代理未运行）态。
/// - `Checking`：编排器/hook 置位（检测在飞）。
/// - `Ok`：可解锁（Netflix 语义下 = 非自制内容也可看）。
/// - `Partial`：部分解锁（Netflix = 仅自制剧；Disney = inSupportedLocation:false）。
/// - `Blocked`：命中**显式地区封禁 marker** 才判（地区不提供 → 换国家/地区节点）。
/// - `Restricted`：检测请求被**风控/反爬拦截**（CF 挑战 / 1020 / IP 信誉），**非地区问题**。语义 ≠「用户被封」：
///   CF 挑战对真浏览器通常自动过，对裸 HTTP 检测器是墙 → 无法代表用户体验判定；出口 IP 信誉可能较差。
///   用户行动 = 换 IP 质量更好的出口节点（同国家即可），实际可用性以浏览器访问为准。
/// - `Timeout`：网络错误/超时/无法判定的统一兜底。
///
/// 对应 上游 `UnlockStatus`。`serde` rename_all 为 lowercase 以对齐 TS 字面量（'idle' / 'ok' / 'restricted' ...），
/// 便于跨进程序列化（IPC/JSON）时与 Polaris 前端契约一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnlockStatus {
    Idle,
    Checking,
    Ok,
    Partial,
    Blocked,
    /// 风控/反爬拦截（CF 挑战 / 1020 / IP 信誉）→ 序列化 `"restricted"`，对齐前端联合类型。
    Restricted,
    Timeout,
}

/// 单个服务的检测结果。
/// `region` = 该服务边缘节点判定的地区码（ISO-3166-1 alpha-2，可空）。
///
/// 对应 上游 `UnlockResult`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlockResult {
    pub status: UnlockStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

impl UnlockResult {
    pub const fn new(status: UnlockStatus) -> Self {
        Self {
            status,
            region: None,
        }
    }

    pub const fn with_region(status: UnlockStatus, region: Option<String>) -> Self {
        Self { status, region }
    }

    /// 工厂：`Timeout`（无 region）—— checker/编排 兜底最常用。
    pub const fn timeout() -> Self {
        Self::new(UnlockStatus::Timeout)
    }
}

/// 检测**已实现**的服务 id 全集（≠ 上线集，见 [`ServiceId::ALL`] / [`ServiceId::PENDING_CALIBRATION`]）。
///
/// 对应前端 `ui/src/contracts/unlock-detection.ts` 的 `SERVICE_IDS`（字面量联合）。
/// **顺序即编排/展示顺序**（与前端 `SERVICE_IDS` 数组逐一对齐）：
/// `chatgpt/claude/gemini/grok/netflix/disney/tiktok/spotify`。
///
/// ## `grok` 弱检测（设计 §3，诚实边界）—— **实现保留、当前不上线**
/// grok 无社区判定标准可 1:1 移植（唯一覆盖它的 `HsukqiLee/MediaUnlockTest` 也只是「trace loc +
/// 硬编码制裁名单 + 403 启发」，且靠 TLS 指纹伪装过 WAF——本仓不引那条路），且其限制重心在
/// L2（IP 信誉）+ 模型级 gating，**几乎无静态 L1 geo marker**。故本 crate 只做**诚实弱检测**：站点可达(Ok)/
/// 风控拦截(Restricted)/超时(Timeout) + 出口国家码——**不测**登录后模型可用性（裸 HTTP 不可见）。
/// Blocked 的 geo marker 查无 → 空规则待真机标定（见 [`GrokEndpoints`](crate::endpoints::GrokEndpoints)）。
/// 空 G3 意味着**永不判 Blocked** → 受限地区会被呈现为「已解锁」，误报方向是谎报绿灯，故归
/// [`ServiceId::PENDING_CALIBRATION`]（checker/端点/单测全保留，只是不进上线集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceId {
    Chatgpt,
    Claude,
    Gemini,
    /// xAI Grok 弱检测；序列化 `"grok"`，对齐前端字面量。
    Grok,
    Netflix,
    Disney,
    /// `#[serde(rename_all = "lowercase")]` 下序列化为 `"tiktok"`，对齐前端字面量。
    Tiktok,
    Spotify,
}

impl ServiceId {
    /// 【上线集 · 唯一开关】用户可见 + 后端真发探测的服务集，顺序 = 前端 `ENABLED_SERVICE_IDS`。
    ///
    /// 生产编排（`src-tauri/src/runtime/unlock.rs` 主轮 / settle-retry / warm 补测）与
    /// [`crate::detector::detect_all`] 全部只遍历本数组 —— **改这一处即改上线面**，
    /// 不在 ALL 里的服务一次网络请求都不会发出（`detector.rs::detect_skips_pending_calibration_services`
    /// 用真请求记录钉死这一点）。
    ///
    /// **启用 [`ServiceId::PENDING_CALIBRATION`] 里的服务** = 把它从那个数组挪到本数组的展示位
    /// （grok 在 gemini 与 netflix 之间），并同步删掉前端
    /// `ui/src/contracts/unlock-detection.ts::PENDING_CALIBRATION_SERVICE_IDS` 的对应字面量。
    ///
    /// **双向锁在哪**：本文件的契约测试比对的是测试体内**硬编码的前端副本**——它只锁「改 Rust 忘了
    /// 改前端」这一个方向。反方向（只改前端）由 `ui/src/contracts/unlock-detection.test.ts` 锁住：
    /// 那条 vitest **直接读本文件源码**解析 `ALL` / `PENDING_CALIBRATION`，与前端三个数组逐一比对，
    /// 故只改任一侧都会转红。（此前只有 Rust 这一侧，「只改前端删 `'grok'`」= 徽章渲染而后端永不探测
    /// ⇒ 恒 idle 且全部 gate 照绿。）前置条件见 `PENDING_CALIBRATION` 文档。
    pub const ALL: &'static [ServiceId] = &[
        ServiceId::Chatgpt,
        ServiceId::Claude,
        ServiceId::Gemini,
        ServiceId::Netflix,
        ServiceId::Disney,
        ServiceId::Tiktok,
        ServiceId::Spotify,
    ];

    /// 【停飞集】checker/端点常量/单测/变异锁**全部保留**，但不进 [`ServiceId::ALL`]：
    /// 既不渲染徽章，后端也不为它发探测。
    ///
    /// ## 当前唯一项：`Grok`
    /// - **为何停飞**：判据是弱检测（可达性 + 风控拦截），且 G3 地区封禁规则为**空**（永不判 `Blocked`）
    ///   → 受限地区会被显示成「已解锁」，误报方向是**谎报绿灯**，比不显示更糟。
    /// - **决策出处**：`~/docs/polaris/design/polaris-unlock-calibration.md:49-52`
    ///   ——「若未来要重做，须先有真机哨兵……别用可达性冒充解锁检测」。
    /// - **启用前置条件（硬）**：真机哨兵标定完成 ——
    ///   `~/docs/polaris/design/polaris-unlock-challenge-and-grok.md` §10 T7：US/EU 双出口匿名
    ///   `POST https://grok.com/rest/models` 取 modelId 差集，两轮一致才算稳定哨兵；据此回填 G3′ 强规则
    ///   （`checkers.rs::check_grok` 的 G3 分支）后，才谈把它移回 `ALL`。**空 G3 期间禁止上线。**
    pub const PENDING_CALIBRATION: &'static [ServiceId] = &[ServiceId::Grok];

    /// 字面量标识（'chatgpt' / 'claude' / ...），对齐前端 ServiceId 字面量联合的字符串形式。
    pub const fn as_str(self) -> &'static str {
        match self {
            ServiceId::Chatgpt => "chatgpt",
            ServiceId::Claude => "claude",
            ServiceId::Gemini => "gemini",
            ServiceId::Grok => "grok",
            ServiceId::Netflix => "netflix",
            ServiceId::Disney => "disney",
            ServiceId::Tiktok => "tiktok",
            ServiceId::Spotify => "spotify",
        }
    }
}

/// 当前代理出口（cdn-cgi/trace 打点得到，兼作缓存 key）。`region` = countryCode，可空。
///
/// 对应 上游 `UnlockEgress`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlockEgress {
    pub ip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// gating 短路原因。
///
/// 对应 上游 `UnlockBlockedReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnlockBlockedReason {
    /// 代理核未运行。
    ProxyNotRunning,
    /// 选中 TS 出口 API 直判无效（未选出口设备 / 出口离线 / 未广告）。
    ExitInvalid,
}

/// 一次检测的完整快照（unlock:run / unlock:get 的返回体）。
///
/// 对应 上游 `UnlockSnapshot`。`results` 用 BTreeMap 以获得稳定序列化顺序（按 ServiceId 字面量排序），
/// 与 Polaris 的对象顺序行为等价（前端不依赖具体 key 顺序）。
///
/// **`rename_all = "camelCase"` 是前端契约的硬要求**：前端 `ui/src/shared/unlock-detection.ts`
/// 的 `UnlockSnapshot` 读 `checkedAt`/`blockedReason`/`notReady`/`lowConfidence`（camelCase）。
/// 若缺此属性，serde 默认发 snake_case（`checked_at` …）→ 前端 `snap.checkedAt` 恒 undefined，
/// UI 时间戳/受限提示全失效（B7 接线时实证，与 §O3 serde 键漂移同族）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockSnapshot {
    /// serviceId -> 结果；gating 短路时为空。
    #[serde(default)]
    pub results: BTreeMap<String, UnlockResult>,
    /// 完成时间戳（Unix 毫秒）；`None` = 未检测（gating 短路）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// 出口信息；trace 失败为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<UnlockEgress>,
    /// 非空 = 未真正检测（如代理未运行）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<UnlockBlockedReason>,
    /// true = 核已 running 但 inbound 尚未就绪（就绪门耗尽）→ 本轮未跑 checker。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_ready: Option<bool>,
    /// true = 全部 checker 均 timeout（egress flap 期典型，低置信）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,
}

impl UnlockSnapshot {
    /// 全 timeout 占位快照（无法建代理会话时用）。
    pub fn all_timeout() -> Self {
        let mut results = BTreeMap::new();
        for id in ServiceId::ALL {
            results.insert(id.as_str().to_string(), UnlockResult::timeout());
        }
        Self {
            results,
            checked_at: None,
            egress: None,
            blocked_reason: None,
            not_ready: None,
            low_confidence: None,
        }
    }

    /// gating 短路快照（核未运行 / 出口无效）。
    pub fn blocked(reason: UnlockBlockedReason) -> Self {
        Self {
            blocked_reason: Some(reason),
            ..Self::default()
        }
    }
}

/// 单个 checker settle 时的增量推送（`EVENT_UNLOCK_PROGRESS` 载荷）。
///
/// 对应 上游 `UnlockProgress`。`rename_all = "camelCase"` 同 [`UnlockSnapshot`]：
/// 前端读 `serviceId`（非 `service_id`），缺此属性则逐服务点亮的 badge 恒收不到 id。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockProgress {
    pub service_id: String,
    pub result: UnlockResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_kebab() {
        let json = serde_json::to_string(&UnlockStatus::Blocked).unwrap();
        assert_eq!(json, "\"blocked\"");
        let back: UnlockStatus = serde_json::from_str("\"partial\"").unwrap();
        assert_eq!(back, UnlockStatus::Partial);
    }

    /// `Restricted` 变体序列化契约：必须是 `"restricted"`（前端联合类型同字面量）。
    /// 打断 rename_all 或改字面量 → 前端 `status === 'restricted'` 分支收不到 → 转红。
    #[test]
    fn restricted_serializes_lowercase() {
        let json = serde_json::to_string(&UnlockStatus::Restricted).unwrap();
        assert_eq!(json, "\"restricted\"");
        let back: UnlockStatus = serde_json::from_str("\"restricted\"").unwrap();
        assert_eq!(back, UnlockStatus::Restricted);
    }

    /// 顺序契约：上线集必须**逐一等于**前端 `ENABLED_SERVICE_IDS` 全序列
    /// （= `SERVICE_IDS` 去掉 `PENDING_CALIBRATION_SERVICE_IDS`）。
    ///
    /// **射程边界（务必读）**：下面的期望值是前端数组的**硬编码副本**，故本测只锁「改了 Rust 忘了改
    /// 前端」这一个方向。反方向（只改前端、Rust 不动）本测**看不见** —— 由
    /// `ui/src/contracts/unlock-detection.test.ts` 锁住：那条 vitest 直接读本文件源码解析 `ALL` /
    /// `PENDING_CALIBRATION` 再与前端三个数组比对。两条合起来才是双向锁；删掉任一条就有一个方向裸奔。
    #[test]
    fn service_ids_order_matches_frontend() {
        let got: Vec<&str> = ServiceId::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            got,
            vec!["chatgpt", "claude", "gemini", "netflix", "disney", "tiktok", "spotify"]
        );
    }

    /// 上线集 ⊎ 停飞集 == 前端 `SERVICE_IDS` 全集（无重叠、无遗漏）。
    ///
    /// 守的是「服务被**静默**弄丢」：某个 id 既不在 `ALL` 也不在 `PENDING_CALIBRATION`，
    /// 就成了有 checker 有单测、却谁也不知道它没上线的孤儿。
    #[test]
    fn shipped_plus_pending_covers_frontend_service_ids() {
        // ui/src/contracts/unlock-detection.ts::SERVICE_IDS（已实现全集，含停飞项）
        const FRONTEND: &[&str] = &[
            "chatgpt", "claude", "gemini", "grok", "netflix", "disney", "tiktok", "spotify",
        ];
        for pending in ServiceId::PENDING_CALIBRATION {
            assert!(
                !ServiceId::ALL.contains(pending),
                "{} 同时在上线集与停飞集 —— 开关自相矛盾",
                pending.as_str()
            );
        }
        let mut got: Vec<&str> = ServiceId::ALL
            .iter()
            .chain(ServiceId::PENDING_CALIBRATION)
            .map(|s| s.as_str())
            .collect();
        got.sort_unstable();
        let mut want = FRONTEND.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "上线集+停飞集 必须等于前端 SERVICE_IDS 全集");
    }

    /// `Grok`/`Tiktok` 字面量契约：前端是 `'grok'` / `'tiktok'`（非 `'xai'` / `'tikTok'`）——
    /// `rename_all = "lowercase"` 保证对齐。打断任一 → 前端 `unlock.results['tiktok']` 恒 undefined
    /// → 徽章恒 idle（静默不显，比报错隐蔽）。grok 当前停飞（[`ServiceId::PENDING_CALIBRATION`]），
    /// 这条替它把字面量锁住，标定后翻开关即用，不必再对一遍序列化名。
    #[test]
    fn grok_and_tiktok_serialize_lowercase() {
        assert_eq!(serde_json::to_string(&ServiceId::Grok).unwrap(), "\"grok\"");
        assert_eq!(ServiceId::Grok.as_str(), "grok");
        assert_eq!(
            serde_json::to_string(&ServiceId::Tiktok).unwrap(),
            "\"tiktok\""
        );
        assert_eq!(ServiceId::Tiktok.as_str(), "tiktok");
    }

    /// 后端上线集必须是前端 `SERVICE_IDS` 的**子序列**（顺序一致、无多余项）——防未来加服务时序漂移；
    /// 停飞集摘出后仍成立（子序列 ≠ 全等，正是「少几项但不许乱序」这条约束）。
    #[test]
    fn service_ids_are_subsequence_of_frontend() {
        const FRONTEND: &[&str] = &[
            "chatgpt", "claude", "gemini", "grok", "netflix", "disney", "tiktok", "spotify",
        ];
        let mut fe = FRONTEND.iter();
        for id in ServiceId::ALL {
            assert!(
                fe.any(|f| *f == id.as_str()),
                "{} 不在前端 SERVICE_IDS 中或顺序错乱",
                id.as_str()
            );
        }
    }

    #[test]
    fn result_skip_none_region() {
        let json = serde_json::to_string(&UnlockResult::timeout()).unwrap();
        assert_eq!(json, "{\"status\":\"timeout\"}");
    }

    /// 前端契约门：快照/进度序列化**必须**是 camelCase（前端读 `checkedAt`/`notReady`/
    /// `lowConfidence`/`serviceId`）。删掉 `#[serde(rename_all="camelCase")]` → 本测试转红。
    /// 这是「接了就亮」的地基：键不对前端拿到 undefined，UI 静默不显（比报错更隐蔽）。
    #[test]
    fn snapshot_and_progress_serialize_camel_case_for_frontend_contract() {
        let mut snap = UnlockSnapshot::all_timeout();
        snap.checked_at = Some(42);
        snap.low_confidence = Some(true);
        let v: serde_json::Value = serde_json::to_value(&snap).unwrap();
        assert!(
            v.get("checkedAt").is_some(),
            "前端读 checkedAt（非 checked_at）"
        );
        assert!(v.get("checked_at").is_none(), "不得发 snake_case 键");
        assert!(v.get("lowConfidence").is_some(), "前端读 lowConfidence");

        let mut blocked = UnlockSnapshot::blocked(UnlockBlockedReason::ProxyNotRunning);
        blocked.not_ready = Some(true);
        let bv: serde_json::Value = serde_json::to_value(&blocked).unwrap();
        assert_eq!(
            bv.get("blockedReason").and_then(|x| x.as_str()),
            Some("proxy-not-running")
        );
        assert!(bv.get("notReady").is_some(), "前端读 notReady");

        let prog = UnlockProgress {
            service_id: "chatgpt".into(),
            result: UnlockResult::new(UnlockStatus::Ok),
        };
        let pv: serde_json::Value = serde_json::to_value(&prog).unwrap();
        assert_eq!(
            pv.get("serviceId").and_then(|x| x.as_str()),
            Some("chatgpt")
        );
        assert!(
            pv.get("service_id").is_none(),
            "进度键须 serviceId（非 service_id）"
        );
    }

    #[test]
    fn snapshot_all_timeout_covers_every_service() {
        let s = UnlockSnapshot::all_timeout();
        assert_eq!(s.results.len(), ServiceId::ALL.len());
        for v in s.results.values() {
            assert_eq!(v.status, UnlockStatus::Timeout);
        }
        assert!(s.checked_at.is_none());
    }
}
