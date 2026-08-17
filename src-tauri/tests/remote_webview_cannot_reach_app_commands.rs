//! **远程源 webview 够不到 app command** —— IPC 授权边界的常驻门。
//!
//! # 守的不变式
//!
//! 一个加载**远程源**（非本应用资源源）的 webview，即便它的 label 没有任何 capability，
//! 也**拿得到** `window.__TAURI_INTERNALS__`（Tauri 的注入脚本无 local/remote 分支，
//! `tauri-2.11.5/src/manager/webview.rs:157-224`）。挡住它的不是 label、不是「没发 capability」，
//! 而是 **origin**。本门钉住这条：
//!
//! > `singbox-dashboard` 窗从它自己的 `http://127.0.0.1:<clash_api_port>` 源发起的 IPC，
//! > **必须**被 ACL 拒 —— 包括 `config_get` 这种会下发 `clashApiSecret`、订阅 URL、全部节点凭据的命令。
//!
//! 现实里那个窗是 `commands/misc.rs::open_singbox_dashboard` 用 `WebviewUrl::External` 起的，
//! 加载核 serve 的**第三方**面板 UI。威胁模型是「面板被投毒 / 面板有 XSS」，不是「任意网页」。
//!
//! # 最容易被读反的一条
//!
//! **判据是 origin，不是 label。** 同一个 `singbox-dashboard` label 换成本地源就**放行**
//! （见下方节 2）。由此两个推论：
//!
//!  - 「给这个 label 补发一份 capability」**改变不了任何事**（空 capability 实测零影响，见下方变异记录）；
//!  - 反过来，`main` 有完整 capability，从远程源来照样被拒 —— capability 默认 `local: true`。
//!
//! # 翻转条件（这门为什么必须常驻）
//!
//! 边界的翻转是**配置性的、静默的**：下面每一条都不会让今天的任何别的门转红。
//!
//!  1. **任一 capability 加 `remote.urls`**（`tauri-utils/src/acl/capability.rs:146`）。
//!     实测：给 `singbox-dashboard` 加 `remote.urls + core:event:default`，
//!     节 3 的前两格立刻翻转。
//!  2. **出现 `src-tauri/permissions/`（app ACL manifest）**。那会让
//!     `has_app_acl_manifest` 变 true，`webview/mod.rs:1823` 那个三析取式的**第二项**独立成立
//!     ⇒ app command 全面开始受 ACL 管，节 2 的三格会全部翻成拒
//!     （届时必须显式给 `main` 授权，否则主窗直接坏）。同时它把本门主判据的保护性质
//!     从「结构性不可能」降级为「靠配置写对」—— 因为那时才**存在**可以授给远程源的 app permission 名。
//!  3. **面板 URL 变成与 app url 同源**（`is_local_url` 命中，`webview/mod.rs:1698-1739`）。
//!     今天不会：面板 host 硬编码 `127.0.0.1`（`runtime/proxy.rs:7453`），
//!     而 app url 是 `tauri://localhost` / devUrl `http://localhost:5173`，host 恒不等。
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
//! 三个析取项各自独立触发 ACL 强制。Polaris 今天无 `src-tauri/permissions/`
//! ⇒ `has_app_acl_manifest == false` ⇒ **local origin 的 app command 确实免 ACL**；
//! 而 `!is_local` 是与 label 无关的独立闸 —— 本门把这两条腿分别钉住。
//!
//! # 本文件测的是什么、不是什么
//!
//! 测 **Tauri 的 ACL 判定**（`Webview::on_message`），加载的是**真实的 `src-tauri/capabilities/`**
//! （经 `generate_context!()` 编译期解析）。命令体是 stub：ACL 只按「命令名 × window/webview label ×
//! origin」判，与实现无关，故同名 stub 与真 `config_get` 在 ACL 面前逐字等价。
//! **本文件不断言 Polaris 的任何业务行为。** 纯进程内 `MockRuntime`：不起 GUI、不起核、不碰网络。
//!
//! # 断言的粒度（刻意不逐字钉消息）
//!
//! ACL 拒绝消息在 debug 下含 capability 名与 window 列表，逐字断言那串会让任何一次 capability
//! 改名误红。故只判三值 [`Verdict`]，其中「被 ACL 拒」认关键子串 `not allowed` ——
//! 它在两种编译模式下都在：debug 走 `resolve_access_message`（`ipc/authority.rs:229`），
//! release 走 `format!("Command {} not allowed by ACL", …)`（`webview/mod.rs:1850`）。
//!
//! # 这门为什么可信（变异自检，2026-08-17 实跑）
//!
//! 「被拒」是**阴性观测**，需要正向对照证明不是「测试压根没接上」：
//!
//!  - **M1** 给 `singbox-dashboard` 发一份**空** capability ⇒ 八格输出**逐字不变**；
//!  - **M3** 同一份改成 `remote.urls:["http://127.0.0.1:*"] + core:event:default`
//!    ⇒ core 命令的 local/remote **两格同时翻转**。这证明 capabilities 确实被编译进 `Resolved`，
//!    M1 的「不变」才有信息量；而同一次里 `config_get` 的 remote 那格**依然被拒** ——
//!    远程源即便被显式打开，app command 也进不来。
//!  - **M5** 试图引用 `allow-config-get` ⇒ 编译期失败，错误列出的可用 permission 全集里
//!    **只有 `core:*` 与插件的**，没有任何 app command 条目 —— 没有 app ACL manifest 时，
//!    app command 在 ACL 里根本没有可被引用的名字（即上面翻转条件 2 的由来）。
//!
//! # 编译前提
//!
//! `tauri.linux.conf.json` 声明的 `../resources/{linux,dashboard}/` 必须存在（tauri-build 校验
//! 路径存在性）。这两个目录是 gitignore 的构建期产物，干净 clone 上要先跑相应的 fetch 脚本 ——
//! 该前提对本 crate 的**既有**测试同样成立，不是本文件引入的。

use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// 与 Polaris 真实命令同名的 stub（见头注：ACL 判定与命令体无关）。
#[tauri::command]
fn config_get() -> &'static str {
    "SECRETS_WOULD_BE_HERE"
}

/// 面板窗 label，与 `commands/misc.rs::DASHBOARD_WINDOW_LABEL` 逐字一致。
const DASHBOARD_LABEL: &str = "singbox-dashboard";

/// 本地源：Linux/macOS 生产形态是 `tauri://localhost`（Windows 是 `http://tauri.localhost`）。
const LOCAL_ORIGIN: &str = if cfg!(any(windows, target_os = "android")) {
    "http://tauri.localhost"
} else {
    "tauri://localhost"
};

/// 面板窗真实加载的源形态：本机 clash api service（host 硬编码，见 `runtime/proxy.rs:7453`）。
const DASHBOARD_ORIGIN: &str = "http://127.0.0.1:9090";

/// `tauri.conf.json` 的 `build.devUrl`（dev 形态下 main 窗的真实源）。
const DEV_ORIGIN: &str = "http://localhost:5173";

/// 一条 core 命令。选 `listen` 是因为它在 `default`/`tray`/`update-popup` 三份 capability 里都被授过，
/// 于是「有 capability 的 label」与「没有的 label」能在同一条命令上对照。
const CORE_COMMAND: &str = "plugin:event|listen";

/// IPC 请求的三种归宿。三值而非二值 —— 「过了 ACL 但死在别处」是正向对照的判据，
/// 把它和「被 ACL 拒」混成一档，对照就失效了。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// 命令跑到了实现体并成功返回。
    Allowed,
    /// 被 ACL 挡下。
    DeniedByAcl,
    /// 过了 ACL，死在后面的环节（参数反序列化等）。
    PassedAclFailedElsewhere,
}

fn build_app() -> App<MockRuntime> {
    mock_builder()
        .invoke_handler(tauri::generate_handler![config_get])
        // 无参 `generate_context!()` ⇒ 读本 crate 的 tauri.conf.json + capabilities/（真实那份）。
        .build(tauri::generate_context!())
        .expect("build app with the repo's real capabilities")
}

/// 建一个远程源 webview（面板窗的等价物）和一个 app 源 webview（主窗的等价物）。
fn build_webviews(
    app: &App<MockRuntime>,
) -> (WebviewWindow<MockRuntime>, WebviewWindow<MockRuntime>) {
    let dashboard = WebviewWindowBuilder::new(
        app,
        DASHBOARD_LABEL,
        WebviewUrl::External(DASHBOARD_ORIGIN.parse().expect("valid dashboard url")),
    )
    .build()
    .expect("build dashboard webview");

    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("build main webview");

    (dashboard, main)
}

fn request(cmd: &str, origin: &str) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        // `on_message` 就是拿这个字段过 `is_local_url`；真实链路里它来自 custom-protocol IPC 的
        // `Origin` 请求头（`ipc/protocol.rs:488-496`）或 postMessage 回退时 webview 引擎给的当前 URL
        // （`wry/src/webkitgtk/mod.rs:646` / `webview2/mod.rs:898` / `wkwebview/…/delegate.rs:52`）。
        // 三条都由引擎填，页面 JS 够不到 —— 故此处直接给 origin 是保真的。
        url: origin.parse().expect("valid origin url"),
        body: InvokeBody::default(),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// 跑一格并归类。打印保留：门转红时这行就是现场。
fn verdict_of(webview: &WebviewWindow<MockRuntime>, cmd: &str, origin: &str) -> Verdict {
    let (verdict, detail) = match get_ipc_response(webview, request(cmd, origin)) {
        Ok(body) => (
            Verdict::Allowed,
            format!("{:?}", body.deserialize::<serde_json::Value>().ok()),
        ),
        // 只认关键子串，不钉整段（见头注「断言的粒度」）。
        Err(e) if e.to_string().contains("not allowed") => (Verdict::DeniedByAcl, e.to_string()),
        Err(e) => (Verdict::PassedAclFailedElsewhere, e.to_string()),
    };
    println!(
        "[acl] webview={:<18} origin={:<24} cmd={:<20} -> {verdict:?} {detail}",
        webview.label(),
        origin,
        cmd,
    );
    verdict
}

/// 断言一格，失败文案说清「这一格翻了意味着什么」。
fn assert_cell(
    webview: &WebviewWindow<MockRuntime>,
    cmd: &str,
    origin: &str,
    expected: Verdict,
    means: &str,
) {
    let actual = verdict_of(webview, cmd, origin);
    assert_eq!(
        actual,
        expected,
        "\n  格：webview={} origin={origin} cmd={cmd}\n  期望 {expected:?}，实得 {actual:?}\n  这一格翻转意味着：{means}\n  翻转条件与判据见本文件头注。",
        webview.label(),
    );
}

/// 八格全表。
///
/// **刻意是单个 `#[test]`**：`MockRuntime` 建 App 会摸 GTK/tray 的进程级初始化，两个 test 并行建
/// 各自的 App 会撞（实测三 test 版在默认并行下失败、`--test-threads=1` 才绿）。把「绿」建立在
/// 调用方传对 flag 上是脆的，故收成一格一格断言的单测；分节即原来的三个命题。
#[test]
fn remote_webview_cannot_reach_app_commands() {
    let app = build_app();
    let (dashboard, main) = build_webviews(&app);

    // ── 节 1｜主判据：远程源够不到 app command ──
    // 这两格是 `open_singbox_dashboard` 那个第三方面板窗与 Polaris 凭据面之间**唯一**的那道闸。
    assert_cell(
        &dashboard,
        "config_get",
        DASHBOARD_ORIGIN,
        Verdict::DeniedByAcl,
        "面板窗从它自己的 http://127.0.0.1 源调到了 config_get —— clashApiSecret、订阅 URL、\
         全部节点凭据对第三方面板代码敞开。这是本门存在的全部理由，翻了就是安全边界被打穿，\
         不要改期望值让它变绿",
    );

    assert_cell(
        &main,
        "config_get",
        DASHBOARD_ORIGIN,
        Verdict::DeniedByAcl,
        "有 capability 的 label 也从远程源放行了 —— 说明 capability 的 local-only 默认\
         （`local: true`）被改掉了，或某处加了 `remote.urls`",
    );

    // ── 节 2｜闸门按 origin 判，不按 label —— 本门最容易被读反的一条 ──
    // 三格全 `Allowed` 同时是**正向对照**：它证明 harness 真的接上了 invoke 链路，
    // 节 1 那两格的「拒」才不是「测试压根没跑起来」。删掉这三格，主判据就退化成阴性观测。
    assert_cell(
        &main,
        "config_get",
        LOCAL_ORIGIN,
        Verdict::Allowed,
        "正向对照塌了：连 main 窗从本地源都调不到 app command。要么 harness 没接上\
         （此时主判据那两格的「拒」无信息量），要么出现了 app ACL manifest（头注翻转条件 2）",
    );

    assert_cell(
        &main,
        "config_get",
        DEV_ORIGIN,
        Verdict::Allowed,
        "dev 形态（devUrl 源）下主窗调不到 app command —— 同上",
    );

    assert_cell(
        &dashboard,
        "config_get",
        LOCAL_ORIGIN,
        Verdict::Allowed,
        "面板 label 在本地源下被拒了 —— 说明闸门判据从 origin 变成了 label。\
         结论要整个改写：本门头注、两份 capability 的 description 都建立在「按 origin 判」之上",
    );

    // ── 节 3｜core 命令（`plugin:` 前缀）无条件过 ACL —— 与 app command 那条腿的对照 ──
    assert_cell(
        &dashboard,
        CORE_COMMAND,
        LOCAL_ORIGIN,
        Verdict::DeniedByAcl,
        "没有任何 capability 的 label 拿到了 core 命令 —— 要么有人给 singbox-dashboard 发了\
         capability，要么某份 capability 的 windows/webviews 通配符扩大到了它",
    );

    assert_cell(
        &dashboard,
        CORE_COMMAND,
        DASHBOARD_ORIGIN,
        Verdict::DeniedByAcl,
        "远程源拿到了 core 命令 —— 最可能是某份 capability 加了 `remote.urls`\
         （头注翻转条件 1），它同时会打开该 capability 授出的全部 core 权限",
    );

    // 这格钉的是**有 capability 的 label 也不得从远程源拿到 core 命令**。没有它，
    // 「给 `default.json` 加 `remote.urls`」这条改动（等于把主窗的全部 core 权限开给远程源）
    // 不会让本门的任何一格翻转 —— 覆盖缺口。
    assert_cell(
        &main,
        CORE_COMMAND,
        DASHBOARD_ORIGIN,
        Verdict::DeniedByAcl,
        "`default` capability 被开给了远程源（多半是加了 `remote.urls`）—— 主窗授出的\
         全部 core 权限对该远程源敞开",
    );

    // 这格**不期望成功**：`listen` 需要 `event` 参数，本门不传，必死在参数反序列化上。
    // 判据是「死法不是 ACL」—— 它证明 `main` 的 `core:event:default` 确实在生效，
    // 从而让上面两格的「拒」有对照：同一条命令，换个 label 就过不去。
    assert_cell(
        &main,
        CORE_COMMAND,
        LOCAL_ORIGIN,
        Verdict::PassedAclFailedElsewhere,
        "main 的 core 命令死在了 ACL 上（而非参数校验）—— `default` capability 的 \
         `core:event:default` 丢了，主窗的事件订阅会整体失效",
    );
}
