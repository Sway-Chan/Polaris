//! 代理控制类 command（上游 `proxy-handlers.ts`）。
//!
//! 映射 channel：
//! - `proxy:start` → [`proxy_start`]
//! - `proxy:stop` → [`proxy_stop`]
//! - `proxy:restart` → [`proxy_restart`]
//! - `proxy:getStatus` → [`proxy_get_status`]
//! - `proxy:getPendingChanges` → [`proxy_get_pending_changes`]
//! - `proxy:applyPendingChanges` → [`proxy_apply_pending_changes`]
//! - `kernel:probeOutbound` → [`kernel_probe_outbound`]（config-engine 兼容性 probe）
//! - `connections:close` / `connections:closeAll` → [`connections_close`] / [`connections_close_all`]
//! - `systemProxy:disable` → [`system_proxy_disable`]
//! - `systemProxy:getStatus` → [`system_proxy_get_status`]（活态：OS 代理是否仍指向本进程 mixed 入站）
//!
//! 启动/停止广播 event:proxyStarted / event:proxyStopped（Polaris 双轨：ProxyManager + trayStateCallback）。

#![allow(clippy::needless_pass_by_value)]

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use polaris_config_engine::user_config::ProxyModeType;
use polaris_singbox_grpc::{Endpoint, SingBoxApiClient};

use crate::events::channel::{EVENT_PROXY_STARTED, EVENT_PROXY_STOPPED};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::proxy::{PendingChangesSummary, ProxyStatus, StartError};
use crate::runtime::AppRuntime;

/// 上游 `PROXY_START`：启动 sing-box 进程 + 系统代理 + 广播 event:proxyStarted。
///
/// async：起核含就绪门（最长 12s 轮询等管理 API 可连），绝不可阻塞 Tauri 命令线程。
///
/// # 起核配置**由后端自己读盘**，不收渲染端载荷（否则起的是「用户上次看到的那份」）
///
/// 此前签名收 `config: Value`，渲染端传的是 `app-store.config` —— 一份只靠 `event:configChanged`
/// → `loadConfig(true)` 异步刷新的内存副本。而 `ProxyRuntime::start_inner` **从不读盘**，逐字用
/// 调用方给的那份（`proxy.rs` 的 `serde_json::from_value(config)`）。两件事合起来：
/// 任何一次「写盘 → 立刻点启动」都会用**写之前**的配置起核，因为回声还没绕回来
/// （写盘 → emit → IPC 送达 → `config_get` 往返 → `set({config})`，每一跳都是异步的）。
/// 连带 `attest_selected_exit` 从**盘**读 `selectedServerId` 而生成用的是陈旧内存值 ⇒
/// 可撞出 `EXIT_MISMATCH`（假报「流量未按预期经过代理」）。
///
/// 渲染端那份还是**有损**的：`config_get` 会 strip 隐私密码哈希，故它连磁盘的完整内容都不是。
///
/// **只改命令层，不动 [`ProxyRuntime::start`] 的签名** —— 那个 `Value` 参数有正当用途，砸掉会伤：
/// - `commands/updater.rs::swap_core_with_restart` 在**停核之前**就把配置钉住
///   （「停完再读若失败就没法把用户的代理恢复回去」），读盘下沉进 `start_inner` 会毁掉这条保证；
/// - 起核失败腿的单测靠注入压根无法落盘的配置（`bad_config()` 是非对象 JSON，
///   sanitize/validate 不会让它原样进盘），下沉后那些腿将无从触达。
///
/// 内部驱动的起停本来就已经读盘或按需钉住（去抖重启回落 `config.current()`、崩溃自愈复用 cfg、
/// 启动自动连接读盘），**渲染端载荷是唯一的陈旧来源**，故收口点就在这里。
#[tauri::command]
pub async fn proxy_start(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<()>, ()> {
    // 盘是单一真值（`current()` = 进程内权威缓存，每次 `save_full` 同刻刷新；缓存冷则读盘）。
    let config = match state.config().current() {
        Ok(c) => c,
        Err(e) => {
            // 诊断收口在此：调用方里有 `let _ = …`（托盘原生菜单）会把返回值丢掉，
            // 只靠 ApiResponse 传就等于那条腿读盘失败时全静默。
            log::warn!("proxy:start 读配置失败 → 未启动: {e}");
            return Ok(ApiResponse::err(format!("读取配置失败，未启动: {e}")));
        }
    };
    // Arc clone：start 需 `&Arc<Self>`（去抖重启回调要把运行时移进 spawn 的 task）。
    let proxy = state.proxy.clone();
    // **此处刻意不再前置拦截 helper 未装**（此前有一份 `tun_helper_missing_for_config` 早退）：
    // 那道门只守住「点连接按钮」一条腿，托盘切模式 / 启动自动连接 / switchMode 去抖重启全绕过它；
    // 更糟的是它**先于** `start()` 返回，使 `start()` 内新增的 TUN 提权引导门（弹框 → 就地授权安装 →
    // 原地续起核）在这条最主要的入口上永远跑不到 —— 用户点「安装」只会跳设置页。门已下沉到
    // `ProxyRuntime::start_inner` 的唯一汇流点，这里放行即可。
    Ok(match proxy.start(config).await {
        // **只在核真起来了才广播 proxyStarted**。让位腿（本次起核被 stop 取消 / 被更新的 start 接管）
        // 也返 `Ok`，但返回的是 `status()` 快照、`running:false` —— 此前一律 emit，等于告诉渲染端和托盘
        // 「已连接」，而核根本没起：用户点取消后会看到一次「已连接」提示、托盘也会翻成运行态，随后才被
        // 真实状态刷回去。取消腿的收口信号由那次 `proxy_stop` 的 `proxyStopped` 负责，此处**沉默**才是
        // 诚实的（本腿没有任何可断言的成功事实）。
        Ok(status) if status.running => {
            let _ = app.emit(EVENT_PROXY_STARTED, json!({}));
            ok_void()
        }
        Ok(_yielded) => {
            log::info!("proxy:start 让位/取消腿（核未运行）→ 不广播 proxyStarted");
            ok_void()
        }
        Err(e) => start_err_response(e),
    })
}

/// 起核失败 → 渲染端信封：把**本次失败自带的**结构化码带回去。
///
/// 分类（HELPER_NOT_INSTALLED / HELPER_GATE_ABORTED / STARTUP_FAILED）随 [`StartError`] 一起出栈；
/// 不带回来前端就只能拿 message 猜分类 = 伪造分类，可操作引导（去装 helper vs 用户已取消）无从分流。
/// `code: None` = 本腿没有可诚实断言的分类 → 回落无码错误，**绝不**替它编一个。
///
/// **为什么是独立函数、且不持有 `ProxyRuntime`**（A1 根因）：此前这里回读 `proxy.status().error_code`。
/// 全局 `error_code` 只有 `stop()` 会清，而 `start` 有多条 Err 腿根本不经 `set_error`（config 解析 /
/// 生成 / 建目录 / 写盘，见 `ProxyRuntime::set_error` 文档「不覆盖的腿」）。回读全局 ⇒「门弹出 → 用户
/// 取消（留下 HELPER_GATE_ABORTED，本路径无 stop 可清）→ 去设置页装好 helper → 再点连接 → 这次栽在
/// 配置生成腿」会被贴上 HELPER_GATE_ABORTED 回给渲染端，`HomeScreen` 命中「用户取消」分支弹中性 info
/// 并 `return`，`setConnectError(true)` 被跳过、真实错误整条吞掉。
/// 签名里拿不到运行时 ⇒ **回读全局在此处物理上不可能**，不是靠纪律守。
fn start_err_response(e: StartError) -> ApiResponse<()> {
    match e.code {
        Some(code) => ApiResponse::err_with_code(e.message, code),
        None => ApiResponse::err(e.message),
    }
}

/// 上游 `PROXY_STOP`：清系统代理 + kill 进程 + 广播 event:proxyStopped。
///
/// async：停核含 SIGTERM 宽限期 + 等子进程收割。
#[tauri::command]
pub async fn proxy_stop(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<()>, ()> {
    let proxy = state.proxy.clone();
    Ok(match proxy.stop().await {
        Ok(()) => {
            let _ = app.emit(EVENT_PROXY_STOPPED, json!({}));
            ok_void()
        }
        Err(e) => ApiResponse::err(e),
    })
}

/// 上游 `PROXY_RESTART`：stop + start（force-restart 入核）。
///
/// 同 [`proxy_start`]：配置由后端读盘。渲染端此前是「自己先 `config:get` 一趟再把结果传回来」，
/// 那两次 IPC 之间同样有一个能被别人写盘挤进去的窗口，且平白多一次往返。
#[tauri::command]
pub async fn proxy_restart(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<()>, ()> {
    let config = match state.config().current() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("proxy:restart 读配置失败 → 未重启: {e}");
            return Ok(ApiResponse::err(format!("读取配置失败，未重启: {e}")));
        }
    };
    let proxy = state.proxy.clone();
    Ok(match proxy.restart(config).await {
        // 与 `proxy_start` 同门：让位/取消腿返 Ok 但 `running:false` → 不广播假的 proxyStarted。
        Ok(status) if status.running => {
            let _ = app.emit(EVENT_PROXY_STARTED, json!({}));
            ok_void()
        }
        Ok(_yielded) => {
            log::info!("proxy:restart 让位/取消腿（核未运行）→ 不广播 proxyStarted");
            ok_void()
        }
        // 与 `proxy_start` 对称走 [`start_err_response`]：`restart_inner` = `stop_inner` + `start`，
        // 出栈的是**同一个** [`StartError`]，携带同一套分类码。走 `ApiResponse::err(e)` 会经
        // `From<StartError> for String` 只取 message、把 `e.code` 丢在半路 ⇒ 渲染端拿不到
        // HELPER_NOT_INSTALLED / HELPER_GATE_ABORTED，只能回退「猜 message」= 伪造分类。
        Err(e) => start_err_response(e),
    })
}

/// 上游 `PROXY_GET_STATUS`：取代理运行状态。
#[tauri::command]
pub fn proxy_get_status(state: State<'_, AppRuntime>) -> ApiResponse<ProxyStatus> {
    ApiResponse::ok(state.proxy().status())
}

/// `PROXY_GET_PENDING_CHANGES`：待应用节点差集（config 相对运行核起核快照）。
///
/// 返回体**就是** [`PendingChangesSummary`]（`{added, modified, removed}`）——与 push 腿
/// （`event:proxyPendingChanges`）同一个类型，不存在 pull/push 两种形状。
#[tauri::command]
pub fn proxy_get_pending_changes(
    state: State<'_, AppRuntime>,
) -> ApiResponse<PendingChangesSummary> {
    ApiResponse::ok(state.proxy().pending_changes())
}

/// 上游 `PROXY_APPLY_PENDING_CHANGES`：立即应用待应用变更（force-restart）。
///
/// 返回 `{ ok, status }`，status ∈ `applied | deferred | skipped`（对齐 上游 proxy-handlers.ts:122-127）。
/// status 由**真实运行态 + lifecycle 在飞判定**得出（此前硬编码 `"applied"` → UI 误报成功）。
#[tauri::command]
pub async fn proxy_apply_pending_changes(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    let proxy = state.proxy.clone();
    let status = proxy.apply_pending().await;
    Ok(ApiResponse::ok(
        json!({ "ok": status != "skipped", "status": status }),
    ))
}

// ── C10：自定义协议兼容性 probe（上游 `ProxyManager.probeOutbound`，:3749-3818）─────────────
//
// **为何不复用 C3 的 `probe_through_proxy` / `measure_latency`**：C10 与 C3 回答**两个不同问题**，机制不可
// 互换。C10 = **静态内核兼容性**：把用户 JSON 出站包成最小 config（该出站 + direct）跑 `sing-box check`，
// verdict 完全由内核 config 解析器决定（`unknown outbound type: snell` → 不支持、需第三方内核）——
// **不碰网络、不需运行核、不经代理**。C3 = **运行期连通性**：经**在跑**的 mixed 口对节点发真 HTTP / 测 TCP
// 延迟，需真起核 + 真出网。复用 C3 会把「内核是否认得此协议」误变成「此刻能否连通」= 失真。故 C10 忠实
// 复用 **`sing-box check` 子进程**（同 `runtime/tailscale_login_core.rs::SingBoxConfigChecker` 范式），非
// C3 网络 probe。真跑 check 需真核（本机无核 → failOpen indeterminate）；**pure 决策面单测 + failOpen 门**。

/// probe 的 sing-box check 超时（check 是静态校验，通常 <1s；给慢盘留余量）。
const PROBE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// probe 的 sing-box check 三态结果（**failOpen ≠ 不兼容**）。
enum ProbeCheck {
    /// check 通过 → 内核支持该协议。
    Supported,
    /// 核缺失 / spawn 失败 / 超时（failOpen）→ **无法判定**（中性 indeterminate，非红色「不支持」）。
    Indeterminate,
    /// check 非零退出 → 内核拒绝，透传结构化诊断（[`ProbeDiagnostic`]：键路径 + 消息 + 原始全文），
    /// 取代此前 `raw.chars().take(300)` 的原样截断。
    Unsupported(ProbeDiagnostic),
}

/// C10 诊断结构化提取的结果：从 `sing-box check` 的 stderr（为空则 stdout）里拆出
/// 「哪个键错了 + 错在哪」，见 [`parse_probe_diagnostic`] 的完整解析规则与实测依据。
///
/// `path` 为空 = 没能解析出键路径（如纯 JSON 语法错误、或全然陌生的行）——**绝不编造**，这种情况下
/// `message` 回落为该行的完整原文，前端仍能读到全部信息，只是没有「键」可高亮。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeDiagnostic {
    /// 键路径，如 `outbounds[0].tls.utls.fingerprint`（decode 类，逐级）或 `outbound[0]` / `router`
    /// （initialize 类，粒度天生更粗）。解析不出 → `None`。
    path: Option<String>,
    /// 人类可读消息（go 侧原话，不翻译——第三方内核吐的英文诊断，翻译了反而对不上用户搜到的 issue）。
    message: String,
    /// 完整原始输出（ANSI 转义已剥离、已 trim，未截断）：结构化提取失败或用户想看全貌时的兜底展示。
    raw: String,
}

/// 校验 probe 入参（= 上游 `probeOutbound` 头部守卫）：必须是**对象**且含 string `type`。
/// 非法 → Err（直接 fail，**不触发 check**）。数组 / 标量 / 无 type / type 非串 一律拒。
///
/// **谓词本体在 config-engine**（`user_config::protocol_settings::custom_outbound_type`），本处只负责
/// 把 `None` 翻成给前端的那句英文。此前这里是自成一套的内联判断，与生成路径「窄 struct
/// `from_value`」的判据**互不相干** —— 于是按钮可以报 `{ok:true}`（raw JSON 送 `sing-box check` 过了），
/// 而真正生成运行配置时用户那些没建模的键早被吃掉，两者的结论可以不一致。生成路径改成真透传之后，
/// 剩下的唯一判据就是这个谓词，两边同调它 ⇒ 分叉面为零。
fn validate_probe_outbound(outbound: &Value) -> Result<(), String> {
    match polaris_config_engine::user_config::protocol_settings::custom_outbound_type(outbound) {
        Some(_) => Ok(()),
        None => Err("invalid outbound: must be an object with a string \"type\"".to_string()),
    }
}

/// 组最小 sing-box config（= 上游 `minimal`）：注入 `tag:'probe'`；outbound 路径 outbounds=[probe, direct]；
/// endpoint 路径 endpoints=[probe] + outbounds=[direct]（无 probe 混入 outbounds）；两者 route.final='direct'
/// + log.level='fatal'。
fn build_probe_config(outbound: &Value, is_endpoint: bool) -> Value {
    let mut probe = outbound.clone();
    if let Some(obj) = probe.as_object_mut() {
        obj.insert("tag".to_string(), Value::String("probe".to_string()));
    }
    let direct = json!({ "type": "direct", "tag": "direct" });
    if is_endpoint {
        json!({
            "log": { "level": "fatal" },
            "endpoints": [probe],
            "outbounds": [direct],
            "route": { "final": "direct" },
        })
    } else {
        json!({
            "log": { "level": "fatal" },
            "outbounds": [probe, direct],
            "route": { "final": "direct" },
        })
    }
}

/// verdict → 前端 `{ ok, indeterminate?, error?, errorPath?, errorRaw? }`（= 上游 probeOutbound 返回
/// 形态 + 本批新增的结构化字段）。`error` 对 Unsupported 腿现在是**解析出的 message**（不再是截断的
/// 原始 300 字符）；`errorPath` 只在解析出键路径时才出现（`None` → 该键整个不下发，前端按 `in`/`?.`
/// 判「没有」，不是空字符串）；`errorRaw` 恒随 Unsupported 下发，供前端保留兜底展示。
fn probe_verdict(check: ProbeCheck) -> Value {
    match check {
        ProbeCheck::Supported => json!({ "ok": true }),
        ProbeCheck::Indeterminate => {
            json!({ "ok": false, "indeterminate": true, "error": "内核不可用或超时，无法判定兼容性" })
        }
        ProbeCheck::Unsupported(diag) => {
            let mut v = json!({ "ok": false, "error": diag.message, "errorRaw": diag.raw });
            if let Some(path) = diag.path {
                v["errorPath"] = Value::String(path);
            }
            v
        }
    }
}

/// 跑 `sing-box check -c <file>` 并映射三态。**failOpen（spawn 失败 / 超时）→ Indeterminate**（核缺失/慢盘
/// ≠ 不兼容）；非零退出 → `Unsupported(`[`parse_probe_diagnostic`]` 提取的结构化诊断)`，
/// 取代此前 `.chars().take(300)`（= 上游 `.slice(0,300)`）的原样截断——那种截法零结构化，纯靠用户
/// 自己在一坨文本里肉眼找哪个键错了，长键路径 + go 消息经常整段超出 300 字符被腰斩。
async fn run_probe_check(binary: &std::path::Path, config_path: &std::path::Path) -> ProbeCheck {
    let mut builder = tokio::process::Command::new(binary);
    builder
        .arg("check")
        .arg("-c")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let fut = crate::runtime::win_console::no_console_window_async(&mut builder).output();
    match tokio::time::timeout(PROBE_CHECK_TIMEOUT, fut).await {
        Err(_) => ProbeCheck::Indeterminate,     // 超时 → failOpen
        Ok(Err(_)) => ProbeCheck::Indeterminate, // spawn 失败（核缺失 / 无权限）→ failOpen
        Ok(Ok(out)) if out.status.success() => ProbeCheck::Supported,
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            // stderr 优先、为空才落回 stdout —— 与此前行为一致（sing-box 恒写 stderr，留 stdout 兜底
            // 给理论上把日志导向 stdout 的变体 / 未来版本）。
            let raw = if stderr.trim().is_empty() {
                stdout.as_ref()
            } else {
                stderr.as_ref()
            };
            ProbeCheck::Unsupported(parse_probe_diagnostic(raw))
        }
    }
}

/// 剥离 ANSI CSI 转义序列（`ESC '[' … 终止字节`）。
///
/// **不在任务描述里、真跑随包二进制才发现的事实**：`resources/linux/sing-box check` 恒发彩色码
/// （`\x1b[31mFATAL\x1b[0m[0000] …`），且**不看 stdout/stderr 是否为 tty**——用干净的 Python
/// `subprocess.PIPE`（非终端管道，非 shell 继承的 pty）验证过，字节原样吐出，不因非交互环境而关闭
/// 着色。不剥离的话「保留原始文本兜底展示」展示给用户的就是一坨不可打印控制字符（`raw` 进了 JSON
/// 传到前端，渲染成什么取决于前端容器，最好情况也是无意义的方块/空白）。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            let mut probe = chars.clone();
            if probe.next() == Some('[') {
                chars.next(); // 消费 '['
                              // CSI 终止字节落在 0x40..=0x7E（SGR 彩色码以 'm' 收尾，如本例的 `31m`/`0m`）。
                for p in chars.by_ref() {
                    if ('@'..='~').contains(&p) {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// 候选串是否形如键路径：点号分节，段名为字母/数字/下划线（首字符非数字），可选尾随 `[数字]` 下标。
///
/// **为什么需要这道判据，而不是「冒号+空格切两刀」就直接采信**：`decode config at <file>: X: Y` 里
/// `X` 不一定是键路径——真机实测过 `decode config at /tmp/psb/e.json: duplicate outbound/endpoint
/// tag: dup`（`X`="duplicate outbound/endpoint tag"，带空格带斜杠，是消息的一部分；`Y`="dup" 是那个
/// 重复的 tag 值，也不构成「消息」）。此判据把这种「凑巧被冒号+空格分开、实际不是键路径」的候选挡
/// 回去——挡住后整段 `X: Y` 原样并回 message，不强行拆分、不编造。
fn looks_like_keypath(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    s.split('.').all(|seg| {
        let (name, idx) = seg.find('[').map_or((seg, ""), |i| seg.split_at(i));
        !name.is_empty()
            && name.chars().next().is_some_and(|c| !c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && (idx.is_empty()
                || (idx.len() > 2
                    && idx.ends_with(']')
                    && idx[1..idx.len() - 1].bytes().all(|b| b.is_ascii_digit())))
    })
}

/// 把 [`run_probe_check`] 拿到的原始 stderr/stdout 解析成 [`ProbeDiagnostic`]。**纯函数**，不碰
/// 进程/IO——单测直接喂字符串，样本来自真机跑随包 `resources/linux/sing-box`（`version` 自报
/// `1.14.0-beta.7`）构造的 6 组坏 config（见 `probe_diagnostic_tests` 模块内联注释逐条溯源）；
/// Windows 场景本机无法执行 `.exe`，按已用 Linux 等价场景验证过的分隔符规律手工构造，非拍脑袋瞎写
/// （见下方「文件路径含冒号」小节）。
///
/// # 两类错误形态
///
/// - **decode 类**：`decode config at <file>: <keypath>: <go message>`（JSON 反序列化期）。
/// - **initialize 类**：`initialize <粗粒度 path>: <message>`（语义校验期，如
///   `initialize outbound[0]: uTLS is required by reality client`）。粒度天生比 decode 粗——只切一层
///   就不再往下切：`initialize router: parse rule-set[0]: open …: no such file or directory` 给出
///   `path="router"`，`parse rule-set[0]: open …` 整段留在 message 里。不是没做全，是 observed 到的
///   格式本就只有这一层稳定结构，再往下切（比如把 `parse rule-set[0]` 也拆出来）就是在没有依据的
///   情况下编格式。
///
/// # 文件路径含冒号（Windows `C:\...`）为什么用 `": "`（冒号+空格）切不会切错
///
/// sing-box 的分隔符恒为 `": "`；Windows 盘符冒号后紧跟 `\`，从不跟空格。真机验证过等价场景
/// （Linux 下把探测临时文件放进一个名字本身带冒号的目录 `weird:dir/`）：`decode config at
/// .../weird:dir/t12.json: outbounds[0].bogus_field: …` 用 `split_once(": ")` 精确切在文件名之后，
/// 目录名里的冒号完全不干扰——因为它后面跟的是字母不是空格。本函数并不使用切出来的文件路径
/// （探测用的是我们自己写的临时文件，路径对用户无意义，只会引出「这是我的文件吗」的困惑），
/// 定位它只是为了正确跳过。
///
/// # 完全陌生的行
///
/// 两个 marker 都找不到 → `path=None`，`message` = 整行原文，**不猜**。
///
/// # 多行输出取哪一条
///
/// 取**最后一条非空行**。真机 6 组构造坏 config（decode/initialize 两类、JSON 语法错误、同时触发
/// 两处 decode 错误的复合场景）恒只吐一行——Go `log.Fatal` 语义是记一行立即 `os.Exit`，未观测到
/// 「先若干行 WARN 再一行 FATAL」的样本。取最后一行是防御性选择而非臆测：真正终止进程的诊断永远是
/// 最后一行；若未来版本在 FATAL 前加了 WARN/INFO 前置噪声，取最后一行仍然对，取第一行则会被任何
/// 前置噪声顶替。
fn parse_probe_diagnostic(raw: &str) -> ProbeDiagnostic {
    const NO_OUTPUT_MSG: &str = "check failed"; // 沿用此前的兜底文案（check 非零退出但双流全空的病态腿）。
    let clean = strip_ansi(raw);
    let clean = clean.trim();
    if clean.is_empty() {
        return ProbeDiagnostic {
            path: None,
            message: NO_OUTPUT_MSG.to_string(),
            raw: String::new(),
        };
    }
    let line = clean
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(clean)
        .trim();

    const DECODE_MARKER: &str = "decode config at ";
    const INIT_MARKER: &str = "initialize ";

    // 两个 marker 都用 `find`（子串搜索，不锚定行首）而非 `strip_prefix`：日志框架前缀
    // `FATAL[0000] `（真机还带 ANSI 色码，见 `strip_ansi` 文档）挡在 marker 前面，锚定行首会
    // 直接错过两类错误。
    let (path, message) = if let Some(after_marker) = line
        .find(DECODE_MARKER)
        .map(|i| &line[i + DECODE_MARKER.len()..])
    {
        // after_marker = "<file>: <keypath>: <msg>" 或 "<file>: <msg 不含 keypath>"。
        match after_marker.split_once(": ") {
            Some((_file, after_file)) => match after_file.split_once(": ") {
                Some((candidate, msg)) if looks_like_keypath(candidate) => {
                    (Some(candidate.trim().to_string()), msg.trim().to_string())
                }
                _ => (None, after_file.trim().to_string()),
            },
            None => (None, after_marker.trim().to_string()),
        }
    } else if let Some(after_marker) = line
        .find(INIT_MARKER)
        .map(|i| &line[i + INIT_MARKER.len()..])
    {
        match after_marker.split_once(": ") {
            Some((candidate, msg)) if looks_like_keypath(candidate) => {
                (Some(candidate.trim().to_string()), msg.trim().to_string())
            }
            _ => (None, after_marker.trim().to_string()),
        }
    } else {
        (None, line.to_string())
    };

    ProbeDiagnostic {
        path,
        message,
        raw: clean.to_string(),
    }
}

/// 唯一临时文件名后缀（pid + 单调计数 + 纳秒），避免并发 probe 撞名。
fn probe_tmp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{n}-{nanos}", std::process::id())
}

/// 上游 `KERNEL_PROBE_OUTBOUND`（`kernel:probeOutbound`）：自定义协议兼容性 probe。
///
/// 「内核即权威」：把用户 JSON 出站包成最小 config 跑 `sing-box check`，verdict 完全由内核决定（见本段
/// 上方「为何不复用 C3」）。failOpen（核缺失 / 超时）→ indeterminate（UI 灰显、不阻断自定义协议保存）。
/// 缓存（上游 probeCache LRU 2048）是**性能优化非正确性**，本批未做（低频设置动作，每次真跑 check 可接受）
/// —— DESIGN-REVIEW(c10-probe-cache) 待接线批按需补。
#[tauri::command]
pub async fn kernel_probe_outbound(
    state: State<'_, AppRuntime>,
    outbound: Value,
    is_endpoint: Option<bool>,
) -> Result<ApiResponse<Value>, ()> {
    // 非法入参 → 直接 fail（不触发 check），= 上游 头部守卫。
    if let Err(e) = validate_probe_outbound(&outbound) {
        return Ok(ApiResponse::ok(json!({ "ok": false, "error": e })));
    }
    let cfg = build_probe_config(&outbound, is_endpoint.unwrap_or(false));
    // userData 目录（await 前取 owned，不跨 await 持 State 借用）。
    let dir = state.config().dir().to_path_buf();

    // 核缺失 → failOpen indeterminate（本机无核即走此路径；**不谎报「不支持」**）。
    let binary = match crate::runtime::proxy::resolve_core_binary() {
        Ok(b) => b,
        Err(_) => return Ok(ApiResponse::ok(probe_verdict(ProbeCheck::Indeterminate))),
    };

    // 写最小 config → check → 删（临时文件用完即删）。
    let tmp = dir.join(format!("probe-{}.json", probe_tmp_suffix()));
    let bytes = match serde_json::to_vec(&cfg) {
        Ok(b) => b,
        Err(e) => {
            return Ok(ApiResponse::ok(
                json!({ "ok": false, "error": format!("序列化探测配置失败: {e}") }),
            ))
        }
    };
    let verdict = match std::fs::write(&tmp, &bytes) {
        Ok(()) => {
            let check = run_probe_check(&binary, &tmp).await;
            let _ = std::fs::remove_file(&tmp); // best-effort 清理
            probe_verdict(check)
        }
        Err(e) => json!({ "ok": false, "error": format!("写探测配置失败: {e}") }),
    };
    Ok(ApiResponse::ok(verdict))
}

/// 取管理 API 端点 `(port, secret)`；核未运行/端口未解析 → Err（命令据此回落 clean error，不发假 ok）。
///
/// `pub(crate)`：`commands/misc.rs` 的 `logs_runtime_level` 走同一条「连管理 API 读一下」的配方，
/// 端点读法必须与此处**同源**——两份各读各的，迟早有一份忘了跟上 `clashApiSecret` 的取法。
///
/// 与 `runtime/proxy.rs::management_api()` / `runtime/stats.rs::read_clash_secret` 同源读法
/// （`management_api()` 私有、且其返回的 `GrpcManagementApi` 只暴露 `close_connection`、无 close-all
/// —— close-all 在 `SingBoxApiClient` 上，故这里直接按同一配方建 h2c 客户端，不新增依赖）。
pub(crate) fn management_endpoint(state: &AppRuntime) -> Result<(u16, String), String> {
    let status = state.proxy().status();
    if !status.running || status.clash_api_port == 0 {
        return Err("核未运行".to_string());
    }
    let secret = state
        .config()
        .current()
        .ok()
        .and_then(|c| {
            c.get("clashApiSecret")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    Ok((status.clash_api_port, secret))
}

/// 上游 `CONNECTIONS_CLOSE`：关单条连接（经管理 API gRPC `CloseConnection`，clash `DELETE /connections/{id}` 等价）。
///
/// 核未运行 / gRPC 失败 → clean error（`ApiResponse::err`，前端 invoke 层 reject）；成功 → `{ ok: true }`。
/// 绝不回 `{ ok: false }` 假成功。
#[tauri::command]
pub async fn connections_close(
    state: State<'_, AppRuntime>,
    id: String,
) -> Result<ApiResponse<Value>, ()> {
    let (port, secret) = match management_endpoint(&state) {
        Ok(v) => v,
        Err(e) => return Ok(ApiResponse::err(e)),
    };
    let client = match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret).await {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("管理 API 连接失败: {e}"))),
    };
    Ok(match client.close_connection(id).await {
        Ok(()) => ApiResponse::ok(json!({ "ok": true })),
        Err(e) => ApiResponse::err(format!("关闭连接失败: {e}")),
    })
}

/// 上游 `CONNECTIONS_CLOSE_ALL`：关全部连接（管理 API gRPC `CloseAllConnections`，clash `DELETE /connections` 等价，触发 ResetNetwork）。
///
/// 核未运行 / gRPC 失败 → clean error；成功 → `{ ok: true }`。
#[tauri::command]
pub async fn connections_close_all(state: State<'_, AppRuntime>) -> Result<ApiResponse<Value>, ()> {
    let (port, secret) = match management_endpoint(&state) {
        Ok(v) => v,
        Err(e) => return Ok(ApiResponse::err(e)),
    };
    let client = match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret).await {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("管理 API 连接失败: {e}"))),
    };
    Ok(match client.close_all_connections().await {
        Ok(()) => ApiResponse::ok(json!({ "ok": true })),
        Err(e) => ApiResponse::err(format!("关闭全部连接失败: {e}")),
    })
}

/// 上游 `SYSTEM_PROXY_DISABLE`：用户主动清理系统代理残留设置（TUN 残留提示的一键恢复动作）。
///
/// 收口到与「start 失败腿」同一个 marker 门控控制器（`ProxyRuntime::clear_system_proxy`）：
/// 仅当系统代理确由我们设置且仍指向我们才清（不误清用户自配的第三方代理）。async + `spawn_blocking`
/// 内跑（会 exec `networksetup`/`gsettings`/`reg`）。`cleared` 回传是否真清了（无 marker → false）。
#[tauri::command]
pub async fn system_proxy_disable(state: State<'_, AppRuntime>) -> Result<ApiResponse<Value>, ()> {
    // State 借用不跨 await：先 clone Arc 再 await（对齐 proxy_start 等异步命令）。
    let proxy = state.proxy.clone();
    let cleared = proxy.clear_system_proxy().await;
    Ok(ApiResponse::ok(json!({ "ok": true, "cleared": cleared })))
}

// ── 系统代理活态查询（`systemProxy:getStatus`）─────────────────────────────────────────
//
// # 它回答的问题，以及为什么 `proxy_get_status` 回答不了
//
// 首页/状态栏的连接态在 systemProxy 接管下要分叉判定（契约 L17）：「核在跑」≠「流量经核」。
// 此前渲染端唯一可得的信号是**起核那一刻**落的 `ProxyStatus.error_code = SYSTEM_PROXY_FAILED`。
// 它有两条朝**漏报**（绿灯 + 明文直连）的腿，都不是前端能补的：
//
//  1. **运行期**用户在系统设置里手动关掉 / 改掉代理 —— 起核那一刻是成功的，`error_code` 干净；
//  2. `error_code` 是**单槽**：起核后再来一条非终态错误（如 `RULE_RESOURCES_MISSING`）就把
//     `SYSTEM_PROXY_FAILED` 覆盖掉，降级态提前消失。
//
// 本命令直接读 OS 的代理设置并与本进程 mixed 入站比对 —— 它是**此刻的地面真相**，而不是
// 「历史上某一刻的记录」，故对上面两条是同一个根治。渲染端把它作为 systemProxy 分支的
// 首选判据、`errorCode` 退为「活态未知时」的回落（见 `ui/.../home/connection-state.ts`）。
//
// # 为什么不复用 `ProxyRuntime` 的 `proxy_clearer`
//
// 那条持有接管状态机 + marker（`enable`/`ensure_cleared` 会**写**系统代理），而本查询是
// **纯只读**。经 `production_system_proxy_live_status` 现造一次性 ops：无 marker、无状态、
// 不与接管流程共享锁 —— 轮询它绝不可能因为抢锁而拖住起核/停核。

/// 活态查询应答。`enabled`/`httpProxy`/`httpsProxy`/`socksProxy` 与前端既有 `SystemProxyStatus`
/// 逐字段对齐；`pointsToUs`/`expected` 是本命令新增的**判据本体**。
///
/// **消费方一律读 `pointsToUs`，不要自己拿 `enabled` 判**：`enabled=true` 只说明 OS 层开着代理，
/// 指向的可能是**别人**（第三方代理软件 / 用户手配），那时我们的流量同样没经本地核。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemProxyLiveResponse {
    /// OS 层是否启用了系统代理（**不是**「是否指向我们」）。
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub https_proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_proxy: Option<String>,
    /// **判据本体**：当前 OS 代理是否仍指向本进程的 mixed 入站（`expected`）。
    pub points_to_us: bool,
    /// 比对基准 `127.0.0.1:<mixedPort>`（诊断展示用）。
    pub expected: String,
}

impl From<polaris_system_integration::proxy_ops::SystemProxyLiveStatus>
    for SystemProxyLiveResponse
{
    fn from(v: polaris_system_integration::proxy_ops::SystemProxyLiveStatus) -> Self {
        Self {
            enabled: v.status.enabled,
            http_proxy: v.status.http_proxy,
            https_proxy: v.status.https_proxy,
            socks_proxy: v.status.socks_proxy,
            points_to_us: v.points_to_us,
            expected: v.expected,
        }
    }
}

/// `systemProxy:getStatus`：**当前 OS 代理是否仍指向本进程 mixed 入站**的活态查询。
///
/// 三平台读取面（只读，绝不写）：mac `networksetup -getwebproxy/...`（首个在用网络服务）、
/// Windows `reg query ...\Internet Settings`、Linux `gsettings get org.gnome.system.proxy{,.http…}`
/// （**先读 `mode`**：mode≠manual 时 host/port 残值无效）。详见
/// [`SystemProxyOpsImpl::read_active_proxy`](polaris_system_integration::proxy_ops::SystemProxyOpsImpl::read_active_proxy)。
///
/// **失败一律 `Err` 信封，绝不折成「未生效」**：读不到（非 GNOME 桌面、PATH 缺 `reg.exe`、核未运行
/// 因而无 mixed 口可比对）≠ 系统代理没生效。折成 `false` 会让这些环境稳定误亮降级黄灯；出栈为错误
/// → 渲染端折成「未知」并回落既有 `errorCode` 判据（与本次改动前行为一致）。
///
/// async + `spawn_blocking`：内部 exec `networksetup`/`gsettings`/`reg`，绝不阻塞命令线程。
#[tauri::command]
pub async fn system_proxy_get_status(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<SystemProxyLiveResponse>, ()> {
    let status = state.proxy().status();
    // 核未运行 / 端口未解析 → 没有「本进程的 mixed 入站」可比对，任何判定都是编的。
    if !status.running || status.mixed_port == 0 {
        return Ok(ApiResponse::err("核未运行，无 mixed 入站可比对"));
    }
    // 两代状态不得拼判据：结构性切换会先落磁盘新模式，再去抖重启旧核；此外 `running:true` 会在
    // `start_inner` 内核就绪后先落，而 `starting` 要到整条接管事务返回才归零。两种窗口里 OS 设置都还
    // 不能代表新会话，须返回 unknown（失败信封由前端折 unknown），绝不能编成 not-effective。
    if status.starting {
        return Ok(ApiResponse::err("代理接管仍在启动，系统代理活态尚未落定"));
    }
    if state.proxy().running_proxy_mode_type() != Some(ProxyModeType::SystemProxy) {
        return Ok(ApiResponse::err("当前运行核不是系统代理接管模式"));
    }
    let mixed_port = status.mixed_port;
    // 比对基准与 `runtime/proxy.rs::maybe_enable_system_proxy` 的 `ProxyEnableRequest.address` 同源
    // （那里恒 `127.0.0.1`）。两处不一致 = 恒判「未生效」，改一处必须改另一处。
    let outcome = tokio::task::spawn_blocking(move || {
        polaris_system_integration::production_system_proxy_live_status("127.0.0.1", mixed_port)
    })
    .await;
    Ok(match outcome {
        Ok(Ok(live)) => ApiResponse::ok(SystemProxyLiveResponse::from(live)),
        Ok(Err(e)) => ApiResponse::err(format!("读取系统代理设置失败: {e}")),
        Err(e) => ApiResponse::err(format!("系统代理活态查询 join 失败: {e}")),
    })
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    // ── C10 pure 决策面（对齐 上游 custom-protocol-probe.test.ts 的包裹/判定断言）──

    #[test]
    fn validate_rejects_non_object_and_missing_or_nonstring_type() {
        // 对象 + string type → Ok。
        assert!(validate_probe_outbound(&json!({ "type": "snell", "server": "1.2.3.4" })).is_ok());
        // 空串 type 仍是 string → 放行到 check（check 会拒），= 上游 `typeof type === 'string'`。
        assert!(validate_probe_outbound(&json!({ "type": "" })).is_ok());
        // 非对象（数组 / 标量 / 串）→ Err，不触发 check。
        assert!(validate_probe_outbound(&json!([1, 2, 3])).is_err());
        assert!(validate_probe_outbound(&json!("snell")).is_err());
        assert!(validate_probe_outbound(&json!(42)).is_err());
        // 对象但无 type / type 非串 → Err。
        assert!(validate_probe_outbound(&json!({ "server": "1.2.3.4" })).is_err());
        assert!(validate_probe_outbound(&json!({ "type": 4 })).is_err());
        // 错误文案含 "type"（前端据此提示）。
        assert!(validate_probe_outbound(&json!({}))
            .unwrap_err()
            .contains("type"));
    }

    #[test]
    fn build_probe_config_outbound_path_wraps_probe_and_direct() {
        let ob = json!({ "type": "snell", "server": "1.2.3.4", "psk": "k", "version": 4 });
        let cfg = build_probe_config(&ob, false);
        // outbounds:[{...snell, tag:'probe'}, {direct}]，route.final='direct'，log.level='fatal'。
        assert_eq!(cfg["outbounds"][0]["type"], "snell");
        assert_eq!(cfg["outbounds"][0]["tag"], "probe");
        assert_eq!(cfg["outbounds"][0]["psk"], "k", "原字段须保留");
        assert!(cfg["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["type"] == "direct"));
        assert_eq!(cfg["route"]["final"], "direct");
        assert_eq!(cfg["log"]["level"], "fatal");
        // 非 endpoint 路径不产 endpoints 键。
        assert!(cfg.get("endpoints").is_none());
    }

    #[test]
    fn build_probe_config_endpoint_path_uses_endpoints_and_direct_only_outbounds() {
        let ep = json!({ "type": "wireguard", "server": "1.2.3.4" });
        let cfg = build_probe_config(&ep, true);
        assert_eq!(cfg["endpoints"][0]["type"], "wireguard");
        assert_eq!(cfg["endpoints"][0]["tag"], "probe");
        // endpoint 路径 outbounds 仅兜底 direct（无 probe 节点混入 outbounds）。
        let obs = cfg["outbounds"].as_array().unwrap();
        assert!(obs.iter().all(|o| o["type"] == "direct"));
        assert_eq!(cfg["route"]["final"], "direct");
    }

    #[test]
    fn probe_verdict_maps_three_states_distinctly() {
        // Supported → {ok:true}，无 indeterminate/error。
        let ok = probe_verdict(ProbeCheck::Supported);
        assert_eq!(ok["ok"], true);
        assert!(ok.get("indeterminate").is_none());
        assert!(ok.get("error").is_none());
        // Indeterminate → {ok:false, indeterminate:true}（中性无法判定，**非** error-only 的红色不支持）。
        let ind = probe_verdict(ProbeCheck::Indeterminate);
        assert_eq!(ind["ok"], false);
        assert_eq!(ind["indeterminate"], true);
        // Unsupported → {ok:false, error, errorRaw}，**无 indeterminate**（红色不支持，透传诊断）。
        let no = probe_verdict(ProbeCheck::Unsupported(ProbeDiagnostic {
            path: None,
            message: "unknown outbound type: snell".to_string(),
            raw: "unknown outbound type: snell".to_string(),
        }));
        assert_eq!(no["ok"], false);
        assert!(
            no.get("indeterminate").is_none(),
            "不支持不得带 indeterminate"
        );
        assert!(no["error"]
            .as_str()
            .unwrap()
            .contains("unknown outbound type"));
        assert_eq!(no["errorRaw"], "unknown outbound type: snell");
        // path=None → `errorPath` 键整个不下发（不是空串）：前端用 `in`/`?.` 判「没有」才不会
        // 把 "" 误当成一个（空的）键路径展示出来。
        assert!(
            no.get("errorPath").is_none(),
            "未解析出键路径时 errorPath 不得下发"
        );
    }

    /// `probe_verdict` 对带路径的 Unsupported：`errorPath` 逐字带回。
    #[test]
    fn probe_verdict_unsupported_carries_error_path_when_present() {
        let v = probe_verdict(ProbeCheck::Unsupported(ProbeDiagnostic {
            path: Some("outbounds[0].tls.utls.fingerprint".to_string()),
            message: "json: cannot unmarshal number into Go struct field \
                      OutboundUTLSOptions.OutboundTLSOptionsContainer.tls.utls.fingerprint \
                      of type string"
                .to_string(),
            raw: "FATAL[0000] decode config at /tmp/x.json: outbounds[0].tls.utls.fingerprint: \
                  json: cannot unmarshal number into Go struct field \
                  OutboundUTLSOptions.OutboundTLSOptionsContainer.tls.utls.fingerprint of type string"
                .to_string(),
        }));
        assert_eq!(v["errorPath"], "outbounds[0].tls.utls.fingerprint");
        assert!(v["error"].as_str().unwrap().contains("cannot unmarshal"));
        assert!(v["errorRaw"].as_str().unwrap().contains("decode config at"));
    }

    /// **failOpen 门**：核缺失（spawn 失败）→ Indeterminate（不碰网络、不需真核；确定性可跑）。
    /// 打断 `Ok(Err(_)) => Indeterminate`（改成 Unsupported）→ 本测转红：把「核缺失」谎报成「不支持」。
    #[tokio::test]
    async fn run_probe_check_core_missing_is_indeterminate_not_unsupported() {
        let check = run_probe_check(
            std::path::Path::new("/nonexistent/polaris-sing-box-xyz"),
            std::path::Path::new("/nonexistent/probe.json"),
        )
        .await;
        assert!(
            matches!(check, ProbeCheck::Indeterminate),
            "核缺失（spawn ENOENT）必须 failOpen → Indeterminate，绝不谎报 Unsupported"
        );
        // verdict 形态：indeterminate:true（中性）。
        assert_eq!(probe_verdict(check)["indeterminate"], true);
    }

    // ── A1 起核失败 → 信封映射（码来自 Err 自身，绝不回读全局 status）───────────────────────

    /// 带码腿：分类原样落进信封 `code`，渲染端据此分流可操作引导。
    #[test]
    fn start_err_response_carries_code_from_the_error_itself() {
        let r = start_err_response(StartError::from("x".to_string()));
        assert!(!r.success);
        // 无码腿必须**没有** code：`HomeScreen` 的取消腿按 code 命中，编一个码 = 把真错误伪装成用户取消。
        assert_eq!(r.code, None, "无码腿不得凭空补码");
        assert_eq!(r.error.as_deref(), Some("x"));
    }

    /// 有码腿：逐字带回（变异：改成 `ApiResponse::err(e.message)` 丢码 → 红，前端只能拿 message 猜分类）。
    #[test]
    fn start_err_response_preserves_structured_code() {
        use crate::runtime::proxy::code;
        let e = StartError {
            message: "已取消".to_string(),
            code: Some(code::HELPER_GATE_ABORTED),
        };
        let r = start_err_response(e);
        assert_eq!(r.code.as_deref(), Some(code::HELPER_GATE_ABORTED));
        assert_eq!(r.error.as_deref(), Some("已取消"));
    }
}

/// [`parse_probe_diagnostic`] 结构化提取门。样本来源标注在每条测试里：**真机**＝真跑随包
/// `resources/linux/sing-box`（`version` 自报 `1.14.0-beta.7`）对构造出的坏 config 执行
/// `check -c` 拿到的原始 stderr（未做任何清理，含真实 ANSI 色码），**构造**＝Windows 场景本机无法
/// 跑 `.exe`，按已用 Linux 等价场景（含冒号目录名）验证过的分隔符规律手工拼的字符串。
#[cfg(test)]
mod probe_diagnostic_tests {
    use super::*;

    /// 真机：`decode config at`「cannot unmarshal」类，最深的一条（4 级键路径 + 长 go 消息）。
    /// 命令：对 `{"outbounds":[{"type":"vless",...,"tls":{"utls":{"fingerprint":123}}},{"direct"}]}`
    /// 跑 `sing-box check`。
    #[test]
    fn decode_cannot_unmarshal_extracts_nested_keypath() {
        let raw = "\x1b[31mFATAL\x1b[0m[0000] decode config at /tmp/psb/a.json: \
                    outbounds[0].tls.utls.fingerprint: json: cannot unmarshal number into Go \
                    struct field OutboundUTLSOptions.OutboundTLSOptionsContainer.tls.utls.fingerprint \
                    of type string\n";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(
            diag.path.as_deref(),
            Some("outbounds[0].tls.utls.fingerprint")
        );
        assert_eq!(
            diag.message,
            "json: cannot unmarshal number into Go struct field \
             OutboundUTLSOptions.OutboundTLSOptionsContainer.tls.utls.fingerprint of type string"
        );
        // raw 兜底：ANSI 已剥离（不含 `\x1b`），但文件路径 / FATAL 标签原样保留（全貌兜底）。
        assert!(!diag.raw.contains('\u{1b}'), "raw 不得残留 ANSI 转义字节");
        assert!(diag.raw.contains("FATAL[0000] decode config at"));
    }

    /// 真机：`decode config at`「unknown field」类（keypath 落在字段名本身，不再往下嵌套）。
    #[test]
    fn decode_unknown_field_extracts_keypath() {
        let raw = "\x1b[31mFATAL\x1b[0m[0000] decode config at /tmp/psb/b.json: \
                    outbounds[0].bogus_field: json: unknown field \"bogus_field\"\n";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path.as_deref(), Some("outbounds[0].bogus_field"));
        assert_eq!(diag.message, "json: unknown field \"bogus_field\"");
    }

    /// 真机：`dns.servers[0].server_port` —— 验证 keypath 判据在**顶层非 outbounds 的**路径
    /// （dns 而非 outbounds/endpoints）上同样成立，不是针对 outbounds 专写的特判。
    #[test]
    fn decode_cannot_unmarshal_under_dns_path() {
        let raw = "\x1b[31mFATAL\x1b[0m[0000] decode config at /tmp/psb/c.json: \
                    dns.servers[0].server_port: json: cannot unmarshal string into Go struct \
                    field RemoteDNSServerOptions.DNSServerAddressOptions.server_port of type \
                    uint16\n";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path.as_deref(), Some("dns.servers[0].server_port"));
        assert!(diag.message.starts_with("json: cannot unmarshal string"));
    }

    /// 真机：`initialize` 类（语义校验期，非 decode）——单层 keypath `outbound[0]`，message 是
    /// go 侧一整句人话，不含更多冒号可切。命令：reality 开启但缺 uTLS 必需项。
    #[test]
    fn initialize_class_extracts_coarse_keypath() {
        let raw = "\x1b[31mFATAL\x1b[0m[0000] initialize outbound[0]: uTLS is required by \
                    reality client\n";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path.as_deref(), Some("outbound[0]"));
        assert_eq!(diag.message, "uTLS is required by reality client");
    }

    /// 真机：`initialize` 类嵌套阶段（`initialize router: parse rule-set[0]: open …`）。
    /// 只切一层——`path="router"`（粗粒度），`parse rule-set[0]: open …` 整段留在 message，
    /// 不强行再切一次（第二段 `parse rule-set[0]` 带空格，`looks_like_keypath` 本就会挡住）。
    /// 命令：`route.rule_set` 指向不存在的本地 `.srs` 文件。
    #[test]
    fn initialize_class_nested_stage_keeps_coarse_granularity() {
        let raw = "FATAL[0000] initialize router: parse rule-set[0]: open \
                    /nonexistent/rs1.srs: no such file or directory";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path.as_deref(), Some("router"));
        assert_eq!(
            diag.message,
            "parse rule-set[0]: open /nonexistent/rs1.srs: no such file or directory"
        );
    }

    /// 真机：`duplicate outbound/endpoint tag: dup` —— decode 类里「冒号+空格切出来的候选其实不是
    /// keypath」的假阳性防线。若没有 `looks_like_keypath` 判据，朴素两刀切会把
    /// `path="duplicate outbound/endpoint tag"`、`message="dup"` 错误地拆出来（`dup` 单独看没有
    /// 任何诊断价值）。命令：两个 outbound 撞了同一个 tag。
    #[test]
    fn decode_class_message_with_colon_is_not_misparsed_as_keypath() {
        let raw = "FATAL[0000] decode config at /tmp/psb/e.json: duplicate outbound/endpoint \
                    tag: dup";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(
            diag.path, None,
            "「duplicate ... tag」带空格/斜杠，不是键路径"
        );
        assert_eq!(
            diag.message, "duplicate outbound/endpoint tag: dup",
            "整段原样回落，不腰斩"
        );
    }

    /// 真机：纯 JSON 语法错误（decode 类但压根没有 keypath 可言，候选串带空格/引号）。
    /// 命令：config 文件内容不是合法 JSON（`{ this is not json`）。
    #[test]
    fn decode_class_json_syntax_error_has_no_keypath() {
        let raw = "FATAL[0000] decode config at /tmp/psb/f.json: invalid character 't' \
                    looking for beginning of object key string: row 1, column 3";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path, None);
        // message 保留完整句子，含末尾 "row 1, column 3"（没有被第二次误切掉）。
        assert_eq!(
            diag.message,
            "invalid character 't' looking for beginning of object key string: row 1, column 3"
        );
    }

    /// 构造：Windows 盘符路径（`C:\Users\...`）——冒号后紧跟反斜杠，不是空格，故不会被
    /// `split_once(": ")` 误当成「文件路径与后续内容」的分隔符。真机验证过等价场景（Linux 下
    /// 目录名本身含冒号 `weird:dir/`，输出为 `decode config at .../weird:dir/t12.json:
    /// outbounds[0].bogus_field: json: unknown field "bogus_field"`，切分结果与本用例同构）——
    /// 本机不能跑 `sing-box.exe` 才退而按已验证的分隔符规律构造，非拍脑袋编。
    #[test]
    fn decode_class_file_path_with_windows_drive_colon_does_not_break_split() {
        let raw = r#"FATAL[0000] decode config at C:\Users\sway\AppData\Local\Temp\probe-1.json: outbounds[0].bogus_field: json: unknown field "bogus_field""#;
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(
            diag.path.as_deref(),
            Some("outbounds[0].bogus_field"),
            "盘符冒号不得被误判为 keypath 分隔边界"
        );
        assert_eq!(diag.message, "json: unknown field \"bogus_field\"");
    }

    /// 完全不认识的行（既非 decode 也非 initialize 前缀）：`path` 必须是 `None`，`message` 必须是
    /// **整行原文**（不裁剪、不猜测键路径）——这是「吃不下就如实回落」的字面验证。
    #[test]
    fn unrecognized_line_falls_back_to_raw_with_no_fabricated_path() {
        let raw = "some future sing-box error format nobody has seen yet: whatever happened here";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path, None);
        assert_eq!(diag.message, raw, "陌生格式必须原样回落，不得编造结构");
    }

    /// 多行输出：取**最后一条非空行**（见 [`parse_probe_diagnostic`] 文档「多行输出取哪一条」）。
    /// 构造场景：真机 6 组坏 config 均只吐一行，故用手工拼的多行样本覆盖「防未来加前置 WARN 噪声」
    /// 这条防御性选择——首行是无关噪声，真正的 FATAL 诊断在最后一行，且首尾都不能是空行干扰。
    #[test]
    fn multiline_output_picks_the_last_non_empty_line() {
        let raw = "\nWARN[0000] some deprecated option, ignoring\n\
                    FATAL[0000] initialize outbound[0]: uTLS is required by reality client\n\n";
        let diag = parse_probe_diagnostic(raw);
        assert_eq!(diag.path.as_deref(), Some("outbound[0]"));
        assert_eq!(diag.message, "uTLS is required by reality client");
        // raw 兜底保留全部行（含被跳过的 WARN 噪声），不是只留被选中的那一行。
        assert!(
            diag.raw.contains("WARN[0000]"),
            "raw 兜底须留存完整多行原文"
        );
    }

    /// 双流全空的病态腿（check 非零退出但 stdout/stderr 都没写东西）：回落既有文案 "check failed"，
    /// 不 panic、不产生空字符串消息。
    #[test]
    fn empty_output_falls_back_to_check_failed() {
        let diag = parse_probe_diagnostic("   \n  \n");
        assert_eq!(diag.path, None);
        assert_eq!(diag.message, "check failed");
        assert_eq!(diag.raw, "");
    }

    // ── looks_like_keypath 判据本体 ──────────────────────────────────────────────────

    #[test]
    fn looks_like_keypath_accepts_dotted_segments_with_optional_index() {
        assert!(looks_like_keypath("outbounds[0].tls.utls.fingerprint"));
        assert!(looks_like_keypath("outbounds[0].bogus_field"));
        assert!(looks_like_keypath("dns.servers[0].server_port"));
        assert!(looks_like_keypath("outbound[0]"));
        assert!(looks_like_keypath("router")); // initialize 类的单段粗粒度路径。
    }

    #[test]
    fn looks_like_keypath_rejects_prose_and_malformed_indices() {
        assert!(!looks_like_keypath(""));
        assert!(
            !looks_like_keypath("duplicate outbound/endpoint tag"),
            "带空格带斜杠"
        );
        assert!(
            !looks_like_keypath("invalid character 't' looking for beginning of object key string"),
            "带空格带引号"
        );
        assert!(!looks_like_keypath("outbounds[]"), "空下标");
        assert!(!looks_like_keypath("outbounds[abc]"), "下标非数字");
        assert!(!looks_like_keypath("0utbound"), "首字符是数字");
        assert!(!looks_like_keypath("a..b"), "空分节");
    }

    // ── strip_ansi ────────────────────────────────────────────────────────────────

    /// 真机字节级验证：`strip_ansi` 后不得残留 `\x1b`，且非转义内容（含方括号日志级别标签
    /// `[0000]`，它不是转义序列，不能被连带吃掉）逐字保留。
    #[test]
    fn strip_ansi_removes_real_sing_box_color_codes_verbatim() {
        let raw = "\x1b[31mFATAL\x1b[0m[0000] decode config at /tmp/x.json: a.b: msg";
        let cleaned = strip_ansi(raw);
        assert_eq!(
            cleaned,
            "FATAL[0000] decode config at /tmp/x.json: a.b: msg"
        );
        assert!(!cleaned.contains('\u{1b}'));
    }

    #[test]
    fn strip_ansi_is_noop_on_plain_text() {
        let raw = "no escapes here at all";
        assert_eq!(strip_ansi(raw), raw);
    }
}

#[cfg(test)]
mod system_proxy_live_tests {
    use super::*;
    use polaris_system_integration::proxy_ops::SystemProxyLiveStatus;
    use polaris_system_integration::SystemProxyStatus;

    /// 应答映射不得把 `enabled` 与 `pointsToUs` 混为一谈 —— 「OS 层开着代理」与「代理指向我们」
    /// 是两件事，混掉就退回本命令要修的那条漏报（绿灯 + 流量走第三方代理）。
    #[test]
    fn response_keeps_enabled_and_points_to_us_distinct() {
        let live = SystemProxyLiveStatus {
            status: SystemProxyStatus {
                enabled: true,
                http_proxy: Some("proxy.corp:3128".into()),
                https_proxy: Some("proxy.corp:3128".into()),
                socks_proxy: None,
                bypass_domains: None,
            },
            points_to_us: false,
            expected: "127.0.0.1:7890".into(),
        };
        let r = SystemProxyLiveResponse::from(live);
        assert!(r.enabled, "OS 层确实开着代理");
        assert!(!r.points_to_us, "但它指向第三方 → 我们的流量没经本地核");
        assert_eq!(r.expected, "127.0.0.1:7890");
        assert_eq!(r.https_proxy.as_deref(), Some("proxy.corp:3128"));
    }

    /// 序列化必须是 camelCase（前端 `SystemProxyStatus` 契约字段名），且 `pointsToUs` 恒下发
    /// （非 `skip_serializing_if` —— 缺字段会让前端 `s.pointsToUs` 恒 undefined ⇒ 恒判「未生效」）。
    #[test]
    fn response_serializes_camel_case_with_verdict_always_present() {
        let r = SystemProxyLiveResponse::from(SystemProxyLiveStatus {
            status: SystemProxyStatus {
                enabled: true,
                http_proxy: Some("127.0.0.1:7890".into()),
                https_proxy: Some("127.0.0.1:7890".into()),
                socks_proxy: None,
                bypass_domains: None,
            },
            points_to_us: true,
            expected: "127.0.0.1:7890".into(),
        });
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["pointsToUs"], true);
        assert_eq!(v["httpProxy"], "127.0.0.1:7890");
        assert_eq!(v["expected"], "127.0.0.1:7890");
        assert!(
            v.get("socksProxy").is_none(),
            "未设的腿省略（对齐 TS 可选字段）"
        );
        // 蛇形名不得出现（前端按 camelCase 读）。
        assert!(v.get("http_proxy").is_none());
        assert!(v.get("points_to_us").is_none());
    }
}

#[cfg(test)]
mod start_payload_guard {
    use crate::commands::guard_scan::top_level_fn_body;

    /// **起核配置必须由后端读盘**（接线守卫，锚点失配自带 panic）。
    ///
    /// 缺陷原形：签名收 `config: Value`，渲染端传 `app-store.config` —— 一份只靠
    /// `event:configChanged` → `loadConfig(true)` 异步刷新的内存副本。而 `start_inner` 从不读盘，
    /// 逐字用调用方给的那份 ⇒ 「写盘 → 立刻点启动」用的是**写之前**的配置（连带
    /// `attest_selected_exit` 从盘读 `selectedServerId` 而生成用陈旧值 → 可撞 `EXIT_MISMATCH`）。
    ///
    /// 本守卫钉两件事：① 命令体内真的向 `state.config()` 取值 —— **只断言取值来源，不锁具体方法**
    /// （`current()` 与 `load_full()` 都是合法的「读盘」，锁死其中一个会让另一种写法误红）；
    /// ② 签名里**不得**再出现 `config: Value`。
    ///
    /// ②「重新加回参数」这个变异形态其实**编译器已经先拦下了**（内部调用点不再传第三个实参 ⇒
    /// E0061）—— 实跑确认过。故 ② 真正独立覆盖的是「加回参数**并且**把各调用点重新接上」那种
    /// 能编译的退化；留着它是因为那种退化恰恰会静默恢复本缺陷。
    ///
    /// 射程边界：**只管命令层**。[`ProxyRuntime::start`] 的 `Value` 参数是刻意保留的 ——
    /// `commands/updater.rs::swap_core_with_restart` 要在停核**之前**钉住配置（「停完再读若失败
    /// 就没法把用户的代理恢复回去」），起核失败腿的单测也要注入压根无法落盘的配置
    /// （`bad_config()` 是非对象 JSON）。把读盘下沉进 `start_inner` 会同时砸掉这两者。
    ///
    /// 端到端那一半（真点一次启动、核真拿到新配置）需要真核，列为真机确认项，不在此假装覆盖。
    #[test]
    fn start_and_restart_read_the_config_from_disk_not_from_the_caller() {
        const SRC: &str = include_str!("proxy.rs");
        for sig in ["pub async fn proxy_start(", "pub async fn proxy_restart("] {
            let body = top_level_fn_body(SRC, sig);
            assert!(
                body.contains("state.config()"),
                "{sig} 必须自己向 state.config() 取起核配置 —— 信调用方传的那份就是本缺陷"
            );
            assert!(
                !body.contains("config: Value"),
                "{sig} 不得再收 `config: Value` 参数：留着它，渲染端的陈旧副本就还能被喂进来"
            );
        }
    }
}

#[cfg(test)]
mod system_proxy_live_guard {
    use crate::commands::guard_scan::top_level_fn_body;

    /// 活态查询只能消费同一代、已稳定的运行态；两道门都必须早于任何 OS 读取。
    #[test]
    fn live_query_rejects_starting_and_non_system_running_sessions_before_os_read() {
        let body = top_level_fn_body(
            include_str!("proxy.rs"),
            "pub async fn system_proxy_get_status(",
        );
        let starting = body
            .find("if status.starting")
            .expect("系统代理活态查询必须拒绝 starting 半成品");
        let running_mode = body
            .find("running_proxy_mode_type() != Some(ProxyModeType::SystemProxy)")
            .expect("活态查询必须核对运行核模式，不能读先行落盘的新配置");
        let os_read = body
            .find("production_system_proxy_live_status")
            .expect("活态查询必须保留真实 OS 读取");
        assert!(
            starting < os_read && running_mode < os_read,
            "starting / 运行核模式两道门必须先于 reg/networksetup/gsettings"
        );
    }
}
