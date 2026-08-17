//! **PROBE（取证用，非生产测试面）** —— sing-box 面板窗能不能 invoke 到 app command。
//!
//! # 被证的命题
//!
//! `commands/misc.rs::open_singbox_dashboard` 用 `WebviewUrl::External` 起了 label
//! `singbox-dashboard` 的窗，加载核 serve 的第三方面板 UI（`http://127.0.0.1:<clash_api_port>/…`）。
//! 该 label **不在** `capabilities/` 任何一份的 `windows`/`webviews` 里。
//! 仓内 `tray.json` / `update-popup.json` 的 description 两次断言「app command 不受 ACL 限制」——
//! 若该断言无条件成立，面板 JS 就能 `invoke('config_get')` 拿到 `clashApiSecret` + 订阅 URL + 全部节点凭据。
//!
//! # 本文件测的是什么、不是什么
//!
//! 测 **Tauri 2.11.5 的 ACL 判定**（`Webview::on_message`，`webview/mod.rs:1787-1852`），
//! 加载的是**真实的 `src-tauri/capabilities/`**（经 `generate_context!()` 编译期解析）。
//! 命令体是 stub：ACL 只按「命令名 + window/webview label + Origin」判，与实现无关，
//! 故同名 stub 与真 `config_get` 在 ACL 面前逐字等价。**本文件不断言 Polaris 的任何业务行为。**
//!
//! 纯进程内：`MockRuntime`，不起 GUI、不起核、不碰网络。
//!
//! # 这道门为什么可信（变异自检，2026-08-17 实跑）
//!
//! 「面板窗被拒」这件事本身是**阴性观测**，需要正向对照证明不是「测试压根没接上」：
//!
//!  - **M1** 给 `singbox-dashboard` 加一份**空** capability（`permissions: []`）⇒ 八格输出**逐字不变**；
//!  - **M3** 同一份 capability 改成 `remote.urls: ["http://127.0.0.1:*"] + core:event:default`
//!    ⇒ `plugin:event|listen` 的 local / remote **两格同时翻转**（ACL 拒 → 参数校验拒）。
//!    这证明 capabilities 目录确实被编译进 `Resolved`，M1 的「不变」才有信息量；
//!    而同一次 M3 里 `config_get` 的 remote 那格**依然被拒**——远程源即便被显式打开，app command 也进不来。
//!  - **M5** 试图在 capability 里引用 `allow-config-get` ⇒ 编译期失败，错误把可用 permission 全集列了出来，
//!    里面**只有 `core:*` 与插件的**，没有任何 app command 条目 —— 没有 app ACL manifest 时，
//!    app command 在 ACL 里根本没有可被引用的名字。
//!
//! 另外腿 5 的拒绝消息逐字打出了 `capability: default / tray / update-popup` 与
//! `windows: "main" / "tray" / "update-popup"`，即本仓三份真实 capability 的内容——加载的确实是它们。
//!
//! # 编译前提
//!
//! `tauri.linux.conf.json` 声明的 `../resources/{linux,dashboard}/` 必须存在（tauri-build 校验路径存在性）。
//! 这两个目录是 gitignore 的构建期产物，干净 clone 上要先跑相应的 fetch 脚本——该前提对本 crate 的
//! **既有**测试同样成立，不是本文件引入的。
//!
//! # 判据（vendor 源码，tauri-2.11.5）
//!
//! `webview/mod.rs:1823`：
//! ```text
//! if (plugin_command.is_some() || has_app_acl_manifest || !is_local)
//!   && request.cmd != FETCH_CHANNEL_DATA_COMMAND
//!   && invoke.acl.is_none()
//! { reject }
//! ```
//! 三个析取项各自独立触发 ACL 强制。Polaris 无 `src-tauri/permissions/` ⇒ `has_app_acl_manifest == false`
//! ⇒ **local origin 的 app command 确实不过 ACL**（仓内那两句断言在这个前提下成立）；
//! 但 `!is_local` 是独立闸，与 label 无关。本 probe 就是把这两条腿分别打出来。

use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewUrl, WebviewWindowBuilder};

/// 与 Polaris 真实命令同名的 stub（见本文件头注：ACL 判定与命令体无关）。
#[tauri::command]
fn config_get() -> &'static str {
    "SECRETS_WOULD_BE_HERE"
}

/// 本地源：Linux/macOS 生产形态是 `tauri://localhost`（Windows 是 `http://tauri.localhost`）。
const LOCAL_ORIGIN: &str = if cfg!(any(windows, target_os = "android")) {
    "http://tauri.localhost"
} else {
    "tauri://localhost"
};

/// 面板窗真实加载的源形态：本机 clash api service。
const DASHBOARD_ORIGIN: &str = "http://127.0.0.1:9090";

/// `tauri.conf.json` 的 `build.devUrl`（dev 形态下 main 窗的真实源）。
const DEV_ORIGIN: &str = "http://localhost:5173";

fn build_app() -> App<MockRuntime> {
    mock_builder()
        .invoke_handler(tauri::generate_handler![config_get])
        // 无参 `generate_context!()` ⇒ 读本 crate 的 tauri.conf.json + capabilities/（真实那份）。
        .build(tauri::generate_context!())
        .expect("build app with the repo's real capabilities")
}

fn request(cmd: &str, origin: &str) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        // `on_message` 就是拿这个字段过 `is_local_url`；真实链路里它来自
        // custom-protocol IPC 的 `Origin` 请求头（`ipc/protocol.rs:488-496`，浏览器强制、JS 不可伪造）。
        url: origin.parse().expect("valid origin url"),
        body: InvokeBody::default(),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// 返回 `Ok(负载)` / `Err(拒绝原因)`，两者都打印，便于人读取证输出。
fn probe(
    webview: &tauri::WebviewWindow<MockRuntime>,
    cmd: &str,
    origin: &str,
) -> Result<String, String> {
    let out = match get_ipc_response(webview, request(cmd, origin)) {
        Ok(b) => Ok(format!("{:?}", b.deserialize::<serde_json::Value>().ok())),
        Err(e) => Err(e.to_string()),
    };
    println!(
        "[probe] webview={:<18} origin={:<24} cmd={:<20} -> {}",
        webview.label(),
        origin,
        cmd,
        match &out {
            Ok(v) => format!("ALLOWED  {v}"),
            Err(e) => format!("REJECTED {e}"),
        }
    );
    out
}

#[test]
fn dashboard_webview_cannot_invoke_app_commands_from_its_remote_origin() {
    let app = build_app();

    // label 与 `commands/misc.rs::DASHBOARD_WINDOW_LABEL` 逐字一致。
    let dashboard = WebviewWindowBuilder::new(
        &app,
        "singbox-dashboard",
        WebviewUrl::External(DASHBOARD_ORIGIN.parse().unwrap()),
    )
    .build()
    .expect("build dashboard webview");

    let main = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("build main webview");

    // ── 腿 1：main + 本地源 —— 正向对照，证明 app command 在无 app-acl-manifest 时确实免 ACL ──
    assert!(
        probe(&main, "config_get", LOCAL_ORIGIN).is_ok(),
        "正向对照塌了：main 窗本地源都调不到 config_get，说明这套 probe 没测到该测的东西"
    );
    assert!(probe(&main, "config_get", DEV_ORIGIN).is_ok(), "dev 源同上");

    // ── 腿 2：面板 label + 本地源 —— 证明拦截「不是」按 label 做的 ──
    // 这条通过 ⇒ 「给 singbox-dashboard 发一个空 capability」对 app command 无效（空集不改变 acl.is_none()）。
    assert!(
        probe(&dashboard, "config_get", LOCAL_ORIGIN).is_ok(),
        "若这条被拒，说明 label 本身就是闸，结论要改写"
    );

    // ── 腿 3：面板 label + 面板真实远程源 —— 本 probe 的主命题 ──
    let remote = probe(&dashboard, "config_get", DASHBOARD_ORIGIN);
    assert!(
        remote.is_err(),
        "面板窗从它自己的 http://127.0.0.1 源调到了 config_get —— 凭据下发面被打穿"
    );

    // ── 腿 4：main label + 远程源 —— 证明「有 capability」也救不了远程源 ──
    assert!(
        probe(&main, "config_get", DASHBOARD_ORIGIN).is_err(),
        "main 有 default capability，但它是 local-only，remote origin 不该匹配"
    );

    // ── 腿 5：core 命令（plugin: 前缀）两侧对照 ──
    // plugin 命令无条件过 ACL，故面板 label 在**本地源**下也该被拒（它没有任何 capability）。
    assert!(
        probe(&dashboard, "plugin:event|listen", LOCAL_ORIGIN).is_err(),
        "core 命令必须无条件过 ACL"
    );
    assert!(
        probe(&dashboard, "plugin:event|listen", DASHBOARD_ORIGIN).is_err(),
        "core 命令 + 远程源，双重拒"
    );
    // 这条**不期望成功**：`listen` 需要 `event` 参数，本 probe 不传，必死在参数校验上。
    // 判据是「死法不是 ACL」——ACL 拒的消息带 "not allowed"，参数校验拒的不带。
    let main_listen = probe(&main, "plugin:event|listen", LOCAL_ORIGIN);
    assert!(
        matches!(&main_listen, Err(e) if !e.contains("not allowed")),
        "main 有 core:event:default，core 命令该过 ACL（可以死在参数校验上，不能死在 ACL 上）：{main_listen:?}"
    );
}
