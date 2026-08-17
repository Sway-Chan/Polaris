//! Tailscale 按需瞬态登录核（纯逻辑部分）。上游 `src/main/services/tailscale-login-core.ts` 1:1 移植。
//!
//! 「登录专用 sing-box」与运行中主核并存、零提权、无 TUN/监听代理端口，仅为拉起一个 tailscale
//! endpoint 拿交互登录 URL —— 且**必带**一个独立的 1.14 管理 API service（[`TailscaleLoginApiService`]）。
//!
//! ## 为什么管理 API 是必选而非可选
//!
//! 登录 URL 与「登录成功」两件事都只有一个真值源：瞬态核管理 API 的 `SubscribeTailscaleStatus` 流
//! （`TailscaleEndpointStatus.authURL` / `backendState`）。核 stdout 那行
//! `endpoint/tailscale[<tag>]: Waiting for authentication: <url>` 是**日志文案**（上游
//! `protocol/tailscale/endpoint.go` 的 `logger.Info("Waiting for authentication: ", authURL)`），
//! 没有任何兼容承诺；而 `authURL` 是 proto 字段，字段号由 `crates/singbox-grpc` 的两道门
//! （build.rs 对随包核 descriptor 对账 + `tests/bundled_core_wire.rs`）机械看守。两个来源并存
//! 只会让「哪个是真值」重新变成靠记忆维持的东西 —— 故 stdout 解析已整段删除，`api` 入参也从
//! `Option` 收成必填：**构造得出一份没有 STATUS 腿的登录配置，这条路本身被类型堵死**。
//!
//! 本模块只放**纯函数**：config 生成 + 双写防护判定 + 登录状态机，便于单测、与进程生命周期解耦。
//! 进程 spawn/kill、gRPC STATUS 订阅属宿主应用层（`src-tauri/src/runtime/tailscale_login_core.rs`）。

#![forbid(unsafe_code)]

use std::path::Path;

use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};
use serde_json::{Map, Value};

/// 瞬态登录核管理 API（1.14 services[]）入参：独立空闲端口 + 随机 secret，使瞬态核暴露 STATUS 流。
/// 上游 `TailscaleLoginApiService`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleLoginApiService {
    /// 本地空闲端口（须独立于主核 api 端口，避 bind 冲突；由 startTailscaleLogin resolve 后传入）。
    pub port: u16,
    /// 每次随机 secret（gRPC Bearer 鉴权；空串退化免认证）。
    pub secret: String,
}

/// 登录专用 config 的最小形状（无 inbound / 无 route，仅 tailscale endpoint + direct + 管理 api service）。
/// 上游 `TailscaleLoginConfig`。以 serde_json::Value 输出（与 sing-box check 输入一致；builder 由上层序列化）。
#[derive(Debug, Clone, PartialEq)]
pub struct TailscaleLoginConfig {
    /// log 恒 `{ level: "info", timestamp: true }`（不读用户 logLevel/隐私）。
    pub log: Map<String, Value>,
    /// 仅该节点的 tailscale endpoint（state_directory 复用 tailscale-state，与主核一致）。
    pub endpoints: Vec<Value>,
    /// 单个 direct outbound。
    pub outbounds: Vec<Value>,
    /// 1.14 管理 API（恒一条）：瞬态核据此暴露 `SubscribeTailscaleStatus` —— 登录 URL 与登录成功
    /// 判据的**唯一**来源（见模块头）。
    pub services: Vec<Value>,
}

/// 生成登录专用 config：仅含该节点的 tailscale endpoint（state_directory 复用 tailscale-state）+ 一个 direct
/// outbound + 管理 api service。**auth_key 永不写入**（有 authKey 就不需要交互登录）。
///
/// log.level 强制 info + timestamp:true → 核侧诊断行不受日志等级摆布（**不再**是 URL 来源，只是日志）。
/// 不含 inbound/route：瞬态无监听代理端口，故与主核并存无冲突（api service 端口由调用方独立解析）。
///
/// controlUrl/hostname 等身份字段从 tailscaleSettings 透传（与 buildTailscaleEndpoint 同语义），但只透传
/// 登录相关的少量字段——瞬态核只为拿 URL + 落 state，不承载路由/出口。
///
/// 上游 `buildTailscaleLoginConfig`。`user_data` 由调用方注入（state_directory = tailscale_state_dir）。
pub fn build_tailscale_login_config(
    server: &ServerConfig,
    user_data: &Path,
    api: &TailscaleLoginApiService,
) -> TailscaleLoginConfig {
    let ts = server.tailscale_settings.as_ref();
    let mut endpoint = Map::new();
    endpoint.insert("type".to_string(), Value::String("tailscale".to_string()));
    endpoint.insert("tag".to_string(), Value::String(server.name.clone()));
    let state_dir = crate::tailscale_state::tailscale_state_dir(user_data, &server.id);
    endpoint.insert(
        "state_directory".to_string(),
        Value::String(state_dir.to_string_lossy().to_string()),
    );
    // auth_key 故意不写入：本核只服务交互登录（无 key）。
    if let Some(ts) = ts {
        if let Some(control_url) = ts.control_url.as_deref() {
            let trimmed = control_url.trim();
            if !trimmed.is_empty() {
                endpoint.insert(
                    "control_url".to_string(),
                    Value::String(trimmed.to_string()),
                );
            }
        }
        if let Some(hostname) = ts.hostname.as_deref() {
            let trimmed = hostname.trim();
            if !trimmed.is_empty() {
                endpoint.insert("hostname".to_string(), Value::String(trimmed.to_string()));
            }
        }
        if ts.ephemeral == Some(true) {
            endpoint.insert("ephemeral".to_string(), Value::Bool(true));
        }
    }

    let mut log = Map::new();
    log.insert("level".to_string(), Value::String("info".to_string()));
    log.insert("timestamp".to_string(), Value::Bool(true));

    let mut direct = Map::new();
    direct.insert("type".to_string(), Value::String("direct".to_string()));
    direct.insert("tag".to_string(), Value::String("direct".to_string()));

    let mut svc = Map::new();
    svc.insert("type".to_string(), Value::String("api".to_string()));
    svc.insert("listen".to_string(), Value::String("127.0.0.1".to_string()));
    svc.insert("listen_port".to_string(), Value::Number(api.port.into()));
    let secret = api.secret.trim();
    if !secret.is_empty() {
        svc.insert("secret".to_string(), Value::String(secret.to_string()));
    }
    let services = vec![Value::Object(svc)];

    TailscaleLoginConfig {
        log,
        endpoints: vec![Value::Object(endpoint)],
        outbounds: vec![Value::Object(direct)],
        services,
    }
}

/// 序列化 login config 为 sing-box JSON（顶层对象）。便于直接喂 sing-box check / 写盘。
///
/// `services` **恒写入**（不再有「空则省略」的分支）：[`build_tailscale_login_config`] 的 api 入参已是必填，
/// 省略腿在生产上不可达，留着只会让「这份配置到底有没有 STATUS 腿」重新变成要读代码才知道的事。
pub fn login_config_to_json(cfg: &TailscaleLoginConfig) -> Value {
    let mut root = Map::new();
    root.insert("log".to_string(), Value::Object(cfg.log.clone()));
    root.insert("endpoints".to_string(), Value::Array(cfg.endpoints.clone()));
    root.insert("outbounds".to_string(), Value::Array(cfg.outbounds.clone()));
    root.insert("services".to_string(), Value::Array(cfg.services.clone()));
    Value::Object(root)
}

/// 双写防护判定（关键正确性）：该节点的 tailscale endpoint **是否已在运行中的主核里**。
///
/// 两个 sing-box 进程同时写同一 state_directory（tailscaled.state 等）会冲突 → 若主核已带该 endpoint，
/// 则**不要**再起瞬态核。1.14 起 buildOutbounds 对 TS endpoint 已改为 always-emit（无就绪/选中门控）：
/// 只要节点在运行配置里，主核就带它的 endpoint。故本判定 = 核在运行 且 该 TS 节点在运行配置里，
/// 不再看 selected/authKey/stateExists。
///
/// 返回 true（已在主核）时，调用方拒绝起瞬态核——此时该节点的登录由主核 always-emit 路径承载。
///
/// 上游 `tailscaleEndpointInRunningCore`。
pub fn tailscale_endpoint_in_running_core(
    server_id: &str,
    is_running: bool,
    running_config: Option<&UserConfig>,
) -> bool {
    if !is_running {
        return false;
    }
    let config = match running_config {
        Some(c) => c,
        None => return false,
    };
    // always-emit：主核带每个配置的 TS endpoint → 节点在运行配置里即已在主核。
    config
        .servers
        .iter()
        .any(|s| s.id == server_id && s.protocol == Protocol::Tailscale)
}

/// 登录状态机的状态。两个事件源都来自瞬态核管理 API 的 STATUS 流（gRPC 订阅属宿主层；
/// 此处只定义状态机的纯逻辑输入/状态，便于单测驱动）。
///
/// 状态语义（对齐 1.14 登录态判定）：
/// - `Idle`：未开始；
/// - `AwaitingAuth(url)`：STATUS 帧带非空 `authURL`（交互登录 URL，用户须去浏览器完成）；
/// - `LoggedIn`：STATUS 帧 `backendState == "Running"`（1.14 起登录成功不再靠 stateExists 启发式，
///   stateExists 误判未认证为已登录是 #132 根因）。
///
/// **没有「无 STATUS 流」这一态**：登录配置的管理 api service 已是必填（见模块头），构造不出没有
/// STATUS 腿的瞬态核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    Idle,
    AwaitingAuth(String),
    LoggedIn,
}

/// 登录状态机事件。
///
/// - `AuthUrlSeen(url)`：STATUS 帧的 `authURL` 非空 → `AwaitingAuth(url)`（或保持 LoggedIn 若已登录）；
/// - `StatusRunning`：STATUS 帧 `backendState == "Running"` → `LoggedIn`；
/// - `Reset`：回到 `Idle`（停核/取消）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// STATUS 帧带非空 `authURL`。
    AuthUrlSeen(String),
    /// STATUS 帧报 backendState=Running（1.14 登录成功信号）。
    StatusRunning,
    /// 复位（停核/取消）。
    Reset,
}

/// 推进登录状态机。纯函数。上游 无显式状态机（散在 ProxyManager），此为纯逻辑提炼。
///
/// 同一个 URL 反复到达（STATUS 每帧都带 `authURL`，核只在变更时才换 URL）会推进到**同一个状态**，
/// 调用方据「状态是否变化」判要不要再发一次事件即可，无需自备去重表。
pub fn advance_login_state(current: &LoginState, event: &LoginEvent) -> LoginState {
    match event {
        LoginEvent::Reset => LoginState::Idle,
        LoginEvent::StatusRunning => LoginState::LoggedIn,
        LoginEvent::AuthUrlSeen(url) => match current {
            // 已登录则保持（后到的 authURL 不应回退登录态）。
            LoginState::LoggedIn => LoginState::LoggedIn,
            _ => LoginState::AwaitingAuth(url.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_config_engine::user_config::server_config::TailscaleSettings;

    fn ts_server(over: TailscaleSettings) -> ServerConfig {
        ServerConfig {
            id: "ts1".to_string(),
            name: "myts".to_string(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(Box::new(over)),
            ..Default::default()
        }
    }

    fn api() -> TailscaleLoginApiService {
        TailscaleLoginApiService {
            port: 51234,
            secret: "rand-secret".to_string(),
        }
    }

    #[test]
    fn login_config_minimal_endpoint_state_dir_and_direct() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = build_tailscale_login_config(&server, Path::new("/ud"), &api());
        assert_eq!(cfg.log["level"], "info");
        assert_eq!(cfg.log["timestamp"], true);
        assert_eq!(cfg.endpoints.len(), 1);
        let ep = cfg.endpoints[0].as_object().unwrap();
        assert_eq!(ep["type"], "tailscale");
        assert_eq!(ep["tag"], "myts");
        // 生产侧把 `Path::join` 结果 `to_string_lossy` 进 JSON → Windows 上是 `/ud\tailscale\ts1`
        // （sing-box 在 Windows 上本就该收反斜杠）。用同样的 join 语义构造期望值，仍钉住
        // 「user_data / "tailscale" / server.id 三段及其顺序」。
        let want = Path::new("/ud").join("tailscale").join("ts1");
        assert_eq!(
            ep["state_directory"].as_str().unwrap(),
            want.to_string_lossy().as_ref()
        );
        // auth_key 永不写入。
        assert!(ep.get("auth_key").is_none());
        // 无 control_url/hostname/ephemeral 时不写入。
        assert!(ep.get("control_url").is_none());
        assert!(ep.get("hostname").is_none());
        assert!(ep.get("ephemeral").is_none());
        assert_eq!(cfg.outbounds.len(), 1);
        let ob = cfg.outbounds[0].as_object().unwrap();
        assert_eq!(ob["type"], "direct");
        assert_eq!(ob["tag"], "direct");
        // 管理 api service 恒注入（登录 URL / 登录成功的唯一真值源，见模块头）。
        assert_eq!(cfg.services.len(), 1);
    }

    #[test]
    fn login_config_passes_login_related_fields_only() {
        let ts = TailscaleSettings {
            control_url: Some("  https://headscale.example  ".to_string()),
            hostname: Some("node-1".to_string()),
            ephemeral: Some(true),
            // 这些运行期字段不应被 login config 透传（即使设了）。
            exit_node: Some("100.x".to_string()),
            ..Default::default()
        };
        let server = ts_server(ts);
        let cfg = build_tailscale_login_config(&server, Path::new("/ud"), &api());
        let ep = cfg.endpoints[0].as_object().unwrap();
        assert_eq!(ep["control_url"], "https://headscale.example"); // trim
        assert_eq!(ep["hostname"], "node-1");
        assert_eq!(ep["ephemeral"], true);
        // exit_node 不在 login config。
        assert!(ep.get("exit_node").is_none());
    }

    #[test]
    fn login_config_injects_management_service_with_listen_and_secret() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = build_tailscale_login_config(&server, Path::new("/ud"), &api());
        assert_eq!(cfg.services.len(), 1);
        let svc = cfg.services[0].as_object().unwrap();
        assert_eq!(svc["type"], "api");
        assert_eq!(svc["listen"], "127.0.0.1"); // 只回环，不对外
        assert_eq!(svc["listen_port"], 51234);
        assert_eq!(svc["secret"], "rand-secret");
    }

    #[test]
    fn login_config_api_empty_secret_omits_secret_field() {
        let server = ts_server(TailscaleSettings::default());
        let api = TailscaleLoginApiService {
            port: 51234,
            secret: "   ".to_string(),
        };
        let cfg = build_tailscale_login_config(&server, Path::new("/ud"), &api);
        let svc = cfg.services[0].as_object().unwrap();
        assert!(svc.get("secret").is_none());
    }

    /// 顶层键齐全，且 `services` **必写**——瞬态核没有这条 api service 就没有 STATUS 流，
    /// 而 STATUS 流是登录 URL 与登录成功的唯一来源。变异：把 `services` 的写入改回
    /// `if !cfg.services.is_empty()` 之外的任何「可省略」形态 → 末条断言转红。
    #[test]
    fn login_config_to_json_always_carries_management_service() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = build_tailscale_login_config(&server, Path::new("/ud"), &api());
        let json = login_config_to_json(&cfg);
        assert!(json.get("log").is_some());
        assert!(json.get("endpoints").is_some());
        assert!(json.get("outbounds").is_some());
        assert_eq!(json["services"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["services"][0]["listen_port"], 51234);
    }

    fn user_config_with(server: ServerConfig) -> UserConfig {
        UserConfig {
            servers: vec![server],
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_in_running_core_false_when_not_running() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = user_config_with(server);
        assert!(!tailscale_endpoint_in_running_core(
            "ts1",
            false,
            Some(&cfg)
        ));
    }

    #[test]
    fn endpoint_in_running_core_false_when_no_running_config() {
        assert!(!tailscale_endpoint_in_running_core("ts1", true, None));
    }

    #[test]
    fn endpoint_in_running_core_true_when_ts_node_present_and_running() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = user_config_with(server);
        assert!(tailscale_endpoint_in_running_core("ts1", true, Some(&cfg)));
    }

    #[test]
    fn endpoint_in_running_core_ignores_non_ts_protocol() {
        let s = ServerConfig {
            id: "v1".to_string(),
            name: "v".to_string(),
            protocol: Protocol::Vless,
            ..Default::default()
        };
        let cfg = user_config_with(s);
        assert!(!tailscale_endpoint_in_running_core("v1", true, Some(&cfg)));
    }

    #[test]
    fn endpoint_in_running_core_unknown_id_false() {
        let server = ts_server(TailscaleSettings::default());
        let cfg = user_config_with(server);
        assert!(!tailscale_endpoint_in_running_core(
            "other",
            true,
            Some(&cfg)
        ));
    }

    // ── 登录状态机 ──────────────────────────────────────────────

    #[test]
    fn state_machine_idle_to_awaiting_on_auth_url() {
        let next = advance_login_state(&LoginState::Idle, &LoginEvent::AuthUrlSeen("u".into()));
        assert_eq!(next, LoginState::AwaitingAuth("u".into()));
    }

    #[test]
    fn state_machine_status_running_to_logged_in() {
        let next = advance_login_state(
            &LoginState::AwaitingAuth("u".into()),
            &LoginEvent::StatusRunning,
        );
        assert_eq!(next, LoginState::LoggedIn);
    }

    #[test]
    fn state_machine_logged_in_not_regressed_by_late_auth_url() {
        // 后到的 AUTH_URL 行不应回退已登录态。
        let next = advance_login_state(&LoginState::LoggedIn, &LoginEvent::AuthUrlSeen("u".into()));
        assert_eq!(next, LoginState::LoggedIn);
    }

    /// 同 URL 反复到达 → 状态不变（调用方据此免去自备去重表；STATUS 每帧都带 authURL）。
    /// 换了 URL → 状态变（重登录会换 URL，必须重新发给用户）。
    /// 变异：把 `AuthUrlSeen` 分支改成恒返回 `AwaitingAuth(url)` 之外的东西 / 让它带上帧序号之类
    /// 的可变量 → 首条断言转红。
    #[test]
    fn state_machine_same_auth_url_is_idempotent_new_url_is_not() {
        let s1 = advance_login_state(&LoginState::Idle, &LoginEvent::AuthUrlSeen("u1".into()));
        let s2 = advance_login_state(&s1, &LoginEvent::AuthUrlSeen("u1".into()));
        assert_eq!(s1, s2, "同 URL 重复到达 → 同一状态（=不重复通知用户）");
        let s3 = advance_login_state(&s2, &LoginEvent::AuthUrlSeen("u2".into()));
        assert_ne!(s2, s3, "URL 变了 → 状态必须变（新授权页要送到用户手上）");
        assert_eq!(s3, LoginState::AwaitingAuth("u2".into()));
    }

    #[test]
    fn state_machine_reset_returns_idle() {
        let next = advance_login_state(&LoginState::LoggedIn, &LoginEvent::Reset);
        assert_eq!(next, LoginState::Idle);
    }
}
