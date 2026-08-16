//! Tauri command 注册层：Polaris 136 IPC channel → `#[tauri::command]` 映射。
//!
//! 按类别组织（对齐 上游 `main/ipc/handlers/` 的文件划分）：
//! - [`config`]：配置管理（config:get/save/getValue/setValue/updateMode + privacy）
//! - [`server`]：节点管理（server:add/update/delete/getAll/switch + warp + tailscale）
//! - [`proxy`]：代理控制（proxy:start/stop/restart/getStatus + pending-changes + connections）
//! - [`subscription`]：订阅（subscription:add/update/delete/preview + localImport）
//! - [`rules`]：路由规则（rules:getAll/add/update/delete/reorder + ruleResources）
//! - [`stats`]：stats 订阅（stats:subscribe/unsubscribe）
//! - [`system`]：系统能力（system:listProcesses + systemProxy + dns）
//! - [`helper`]：提权 helper（helper:getStatus/install/uninstall）
//! - [`mesh`]：mesh 节点（tailscale + warp 状态）
//! - [`unlock`]：解锁检测（unlock:run/get）
//! - [`speedtest`]：测速（server:speedTest）
//! - [`updater`]：App / 内核更新（update:* / core-update:*）
//! - [`window`]：窗口控制（window:minimize/maximizeToggle/close + app 排序）
//! - [`misc`]：杂项（logs/version/shell/backup/diagnostic/autostart/ipinfo/singbox-dashboard）
//!
//! 所有 command 统一返回 [`crate::response::ApiResponse<T>`]（Polaris 信封），序列化形与 Polaris 前端契约一致。
//! generate_handler! 列表见 `main.rs`。

/// 源码扫描式**调用点守卫**的共用工具（仅测试编译）。
///
/// 本层有若干条不变式无法用普通单测覆盖 —— 被守的函数持 `State<'_, AppRuntime>` / `AppHandle`，
/// 单测构造不出 Tauri 运行时（如 `backup_import_apply` 必须调
/// [`config::enforce_backend_authoritative_fields`]、`server_speed_test` 的回退腿必须在 await 前
/// 捕获让位基准）。这类不变式改用**源码扫描**锁调用点，工具收在此处避免各文件各抄一份。
#[cfg(test)]
pub(crate) mod guard_scan {
    /// 取顶层函数体源码切片：从签名锚点起、到**该函数自己的**右花括号（列 0 的 `\n}\n`）止。
    ///
    /// # 封顶是刚需，不是洁癖
    ///
    /// 切到 **EOF** 的调用点守卫只在「今天这个文件布局」下有牙：把被守的调用从该函数删掉、再在这个
    /// 1000+ 行文件的**任意后续位置**加一个（哪怕是个 `#[cfg(test)]` 里的死函数），守卫照样绿。
    /// 按列 0 的 `\n}\n` 封顶后，射程被锁在被守函数自己的作用域内。
    ///
    /// 锚点 / 闭合花括号缺失一律 panic —— 守卫**失去判据时必须转红**，而不是静默退化成
    /// 「扫了个空字符串、断言恒真」（那正是 return 型门 = 没门的形态）。
    ///
    /// # 为什么还要**剥掉整行注释**（与 `runtime/proxy.rs::method_body` 对齐）
    ///
    /// 切出来的函数体里含**体内注释**，而共用本工具的守卫两个方向都对注释敏感：
    /// - **正面断言**（`helper.rs` 的接线守卫 `find`/`contains`、`config.rs` 的顺序守卫）：把被守的调用
    ///   删掉、再在原处留一行 `// enforce_backend_authoritative_fields(...)` 就能让 `contains` 恒真 ——
    ///   接线没了，守卫仍绿（本仓已实测过这类假绿）；
    /// - **负面断言**（`main.rs` 的 tray gate 禁 `.await` 等）：注释里出现禁词就会**误红**，逼后人把
    ///   断言改宽 = 门被磨钝。
    ///
    /// 只剥**整行**注释（`trim_start().starts_with("//")`）：行尾注释要剥就得先分辨字符串字面量里的
    /// `//`，那是把守卫的取材器写成半个词法分析器，代价与收益不成比例。剥后按行 `join` 保持行序与
    /// 相对位置，故 `find()` 比大小的顺序断言语义不变（被剥的行留空串，不会把两侧的行粘在一起）。
    pub(crate) fn top_level_fn_body(src: &str, signature: &str) -> String {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("锚点消失，守卫已失去判据: {signature}"));
        let rest = &src[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("找不到 {signature} 的右花括号（列 0 的换行+右括号+换行）"));
        strip_line_comments(&rest[..end])
    }

    /// 把整行注释换成空行（保留行数与行序）。[`top_level_fn_body`] 与各文件的二次封顶取材器共用。
    pub(crate) fn strip_line_comments(body: &str) -> String {
        body.lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **守卫的守卫（第二条）**：证明 [`top_level_fn_body`] 真的剥掉了整行注释。
    ///
    /// 不剥的两种假绿都在这里钉死：正面 `contains` 被注释里的锚点文本喂饱（删了调用仍绿）、
    /// 负面 `!contains` 被注释里的禁词误红（门被逼着改宽）。
    ///
    /// **变异锁**：去掉 `top_level_fn_body` 里的 `strip_line_comments(...)` → 三条断言全红。
    #[test]
    fn top_level_fn_body_strips_whole_line_comments() {
        let src = "pub fn target() {\n    // enforce_backend_authoritative_fields(cfg);\n\
                   \x20   let s = \"has // inside a literal\";\n        // .await\n    real_call();\n}\n";
        let body = top_level_fn_body(src, "pub fn target(");
        assert!(
            !body.contains("enforce_backend_authoritative_fields("),
            "**正面断言假绿**：注释里的锚点文本被数进来了 —— 删掉真调用、留一行注释即可骗过守卫"
        );
        assert!(
            !body.contains(".await"),
            "**负面断言误红**：注释里的禁词会把 tray gate 这类 `!contains` 守卫顶红"
        );
        assert!(
            body.contains("real_call()") && body.contains("has // inside a literal"),
            "只剥整行注释：真代码行（含字符串字面量里的 `//`）必须原样保留"
        );
    }

    /// **守卫的守卫**：证明 [`top_level_fn_body`] 真的封了顶，而不是又切到 EOF。
    ///
    /// 没有这条，「我把切片封顶了」只是一句注释 —— 而本轮复审报的正是「文档声称有牙、实际没有」。
    #[test]
    fn top_level_fn_body_stops_at_the_functions_own_brace() {
        let src = "pub fn target() {\n    inside();\n}\n\npub fn later() {\n    outside();\n}\n";
        let body = top_level_fn_body(src, "pub fn target(");
        assert!(body.contains("inside()"), "必须包含被守函数自己的函数体");
        assert!(
            !body.contains("outside()"),
            "**封顶失效**：切到了后续函数 → 调用点守卫可被「删这里、加那里」骗过"
        );

        // 函数体内的嵌套块（缩进的右花括号）不得被误当作函数结束锚。
        let nested = "pub fn target() {\n    if x {\n        inside();\n    }\n    tail();\n}\n\npub fn later() {\n    outside();\n}\n";
        let body = top_level_fn_body(nested, "pub fn target(");
        assert!(
            body.contains("tail()"),
            "缩进的右花括号不是函数结束，不得据此提前截断"
        );
        assert!(!body.contains("outside()"));
    }

    /// 锚点消失必须 panic（转红），而不是返回空切片让断言恒真。
    #[test]
    #[should_panic(expected = "锚点消失")]
    fn missing_anchor_panics_instead_of_silently_passing() {
        top_level_fn_body("fn other() {\n}\n", "pub fn nonexistent(");
    }
}

pub mod config;
pub mod helper;
pub mod icon;
pub mod misc;
pub mod proxy;
pub mod rules;
pub mod server;
pub mod speedtest;
pub mod stats;
pub mod subscription;
pub mod system;
pub mod taildrop;
pub mod unlock;
pub mod updater;
pub mod window;

pub use config::*;
pub use helper::*;
pub use icon::*;
pub use misc::*;
pub use proxy::*;
pub use rules::*;
pub use server::*;
pub use speedtest::*;
pub use stats::*;
pub use subscription::*;
pub use system::*;
pub use taildrop::*;
pub use unlock::*;
pub use updater::*;
pub use window::*;
