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

    /// 取 `match` 里**某一条臂**的臂体：从臂头之后起、到**下一条臂头**为止。
    ///
    /// # 为什么必须封顶（同一形态本仓已踩过两次）
    ///
    /// 切到函数体尾的臂切片，射程由**臂的书写顺序**决定，而不是由判据决定：只要别的臂里有一句
    /// 形状相同的代码，它就会替被守的那条臂作证。实测过两次 —— `update_open_releases(app,
    /// popup.version.clone())` 在 `ViewLog` 与 `ManualDownload` 两臂里逐字相同（把后者搬到前者
    /// 之前、实参换成 `None`，编译通过、全仓测试全绿）；`force_popup_state(...progress(0))` 挪进
    /// `ViewLog` 臂后，守它的门同样全绿。
    ///
    /// # 判据是**臂头形状**，不是缩进宽度
    ///
    /// 第一版按 `"\n            PopupAction::"`（写死 12 空格）封顶，而实际臂缩进是 8 ⇒ needle 恒不
    /// 命中 ⇒ 封顶那行代码**本身是哑的**。故改为逐行 `trim_start()` 后判「以 `<enum>::` 起手且
    /// 含 `=>`」，与缩进无关。
    ///
    /// **输出自检不可省**（P1-2：抽本 helper 时它一度被合成单测顶替掉，那是换了型不是搬了家）。
    /// 单测检的是「**列举到的**几种输入形状」，自检检的是**输出**（切出来的片段里不得再有臂头），
    /// 形状无关 —— 二者不可互相替代。实证：rustfmt 把长 or-pattern 臂头折行后（`|` 起手续行），
    /// 逐行前缀判据认不出下一条臂 ⇒ 静默吞掉它；那是合成单测没列举到、而本自检必红的形态。
    ///
    /// 不对称也是理由本身：**臂头找不到 ⇒ panic（响）；封顶失效 ⇒ 静默多切（哑）**。哑的那一半
    /// 必须自己长嘴。
    ///
    /// `arm_head` 传臂头的**前缀**（如 `"PopupAction::Update | PopupAction::Retry =>"`），
    /// `variant_prefix` 传该 match 所有臂头的公共前缀（如 `"PopupAction::"`）用于识别下一条臂。
    /// 找不到臂头一律 panic —— 守卫失去判据必须转红。
    ///
    /// # 射程：单行臂头
    ///
    /// 识别「下一条臂」靠**逐行**前缀匹配 ⇒ 只认**写在一行里**的臂头（含单行 or-pattern
    /// `A::Y | A::Z =>`）。臂头被折成多行时前缀判据失效，此时兜住它的是上面那条输出自检
    /// （泄漏进来的下一条臂必然含 `variant_prefix`）—— 封顶会红，不会哑。
    pub(crate) fn match_arm_body(body: &str, arm_head: &str, variant_prefix: &str) -> String {
        let at = body
            .find(arm_head)
            .unwrap_or_else(|| panic!("锚点消失，守卫已失去判据（臂头不见了）: {arm_head}"));
        let rest = &body[at + arm_head.len()..];
        let mut out = String::new();
        for line in rest.lines() {
            let t = line.trim_start();
            if t.starts_with(variant_prefix) && t.contains("=>") {
                break;
            }
            out.push_str(line);
            out.push('\n');
        }
        assert!(
            !out.contains(variant_prefix),
            "臂切片里还有下一个臂头（{variant_prefix}）—— 封顶失效，调用点守卫已退化成\
             「函数体里有没有这句话」，别的臂会替被守的那条作证"
        );
        out
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

    /// 剥掉块注释（`/* … */`、JSDoc `/** … */`、JSX `{/* … */}`），整行换成空行（保留行数与行序，
    /// 跟 [`strip_line_comments`] 同一约定）。
    ///
    /// 只用于 TS/TSX 源码取材（`config.rs` 的 `every_consumer_discards_the_payload`）——**不**扩展
    /// [`strip_line_comments`] 本身来做这件事：那是 [`top_level_fn_body`] 的共用地基（调用点跨十余
    /// 个文件，全部喂 **Rust** 源码——具体数字不锁在这里，锁数字上一次改动就把它改错过一回），块
    /// 注释在 Rust 里的分布/语义与 TS 的 JSDoc/JSX 注释不
    /// 同源，扩它风险面过宽；故新开一个明确命名的 helper。[`strip_line_comments`] 自己的**直接**
    /// 调用点其实只有 3 处：`top_level_fn_body` 内部一处（喂 Rust），本文件 `config.rs` 两处（喂 TS，
    /// 且都在本函数刚新增的调用点上）——都不构成扩它的理由，反而是「该独立就独立」的证据。
    ///
    /// 剥的理由与 [`strip_line_comments`] 同一条：调用点计数对注释文本敏感，JSDoc/JSX 注释里提到
    /// 调用形态的字面量（如 `` `configApi.onChanged` ``）会喂饱/顶红判据。
    ///
    /// # 判据是「整行起手」，不是「文本任意位置出现 `/*`」
    ///
    /// 一行 `trim_start()` 后以 `/*` 或 `{/*` 开头才算块注释起点；字符串/glob/正则字面量里的 `/*`
    /// （如 `'ui/**'`、`'https://x/*'`、`/[/*]/`）从不出现在行首，天然不会被误当成起笔——本仓三份
    /// 取材源码实测：全部块注释（含 JSX 注释）都独占一行起笔，没有一例内嵌在表达式中段。
    ///
    /// 起笔行往下逐行找 `*/`：找到即把起笔行到该行**整段清空**（不保留该行 `*/` 之后的尾巴——
    /// 本仓实测该尾巴恒为空或纯 JSX 语法糖 `}`）。**找不到闭合就停止清空、原样保留剩余行**——不
    /// panic：既然起笔判据已经把「字符串里巧合出现 `/*`」的概率压得很低，真出现「找不到闭合」时，
    /// 宁可把没被剥掉的内容当真代码留在原地，让 `sites == 1` 这类数量断言去暴露异常（多计或少计
    /// 都会转红），也不要在归因方向上撒谎——此前「块注释未闭合 —— 取材器判据已过期」这句 panic
    /// 文案在「字符串里的 `/*` 巧合触发」这类场景下纯属指错方向。
    ///
    /// 不保证字节偏移/列位置与原文一致（CJK 字符与 ASCII 空格不等宽，逐字符替换不守恒），只保证
    /// 行数与行序不变；也不处理「一行内先闭合一个块注释、又新开一个未闭合的」这类同行内多段注释
    /// （本仓三份源码实测没有这种写法）——这是有意为之的简化，不是遗漏。
    pub(crate) fn strip_block_comments(src: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        let mut lines = src.lines();
        while let Some(line) = lines.next() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("/*") && !trimmed.starts_with("{/*") {
                out.push(line);
                continue;
            }
            if line.contains("*/") {
                // 单行块注释（起笔即闭合）。
                out.push("");
                continue;
            }
            let mut consumed = Vec::new();
            let mut closed = false;
            for cont in lines.by_ref() {
                let is_close = cont.contains("*/");
                consumed.push(cont);
                if is_close {
                    closed = true;
                    break;
                }
            }
            if closed {
                out.push("");
                out.extend(std::iter::repeat_n("", consumed.len()));
            } else {
                // 找不到闭合：不清空，原样保留起笔行 + 已扫过的后续行，交给数量断言去暴露。
                out.push(line);
                out.extend(consumed);
            }
        }
        out.join("\n")
    }

    /// **守卫的守卫**：证明 [`strip_block_comments`] 三种形态都按 doc 说的方式处理。
    ///
    /// 用 `.lines().collect()` 逐行比较，而不是整串 `assert_eq!`——本函数与
    /// [`strip_line_comments`] 同款 `lines()` + `join("\n")`，天生不保证输出带原文的尾随换行符，
    /// 逐行比较才是这类取材器实际被消费的方式（[`config_changed_payload_tests`] 只做
    /// `match_indices`/`contains`，从不比对整串字节）。
    ///
    /// 变异锁：把开启判据从 `trimmed.starts_with("/*") || trimmed.starts_with("{/*")` 放宽成
    /// `line.contains("/*")` → [`strip_block_comments_ignores_slash_star_mid_line`] 转红。
    #[test]
    fn strip_block_comments_handles_single_line_multi_line_and_jsx_forms() {
        // 单行：起笔即闭合。
        let single = strip_block_comments("a();\n/* 忽略 */\nb();\n");
        let lines: Vec<&str> = single.lines().collect();
        assert_eq!(lines, ["a();", "", "b();"]);

        // 多行 JSDoc：起笔行不闭合，中间行任意内容，闭合行清空——整段都要清空，不能只清首尾两行。
        let jsdoc = "x();\n/**\n * see `configApi.onChanged(cb)`\n */\ny();\n";
        let stripped = strip_block_comments(jsdoc);
        assert_eq!(
            stripped.lines().collect::<Vec<_>>(),
            ["x();", "", "", "", "y();"],
            "行数与行序必须原样保留，注释体必须整段清空"
        );
        assert!(
            !stripped.contains("onChanged"),
            "多行块注释中间行必须被清空，不能只清起笔/闭合两行"
        );

        // JSX 多行注释：起笔是 `{/*`（不是裸 `/*`），闭合行带尾巴 `*/}`。
        let jsx = "{/* 状态卡\n    第二行 */}\n<div />\n";
        assert_eq!(
            strip_block_comments(jsx).lines().collect::<Vec<_>>(),
            ["", "", "<div />"]
        );
    }

    /// **守卫的守卫**：字符串/glob/URL 里出现的 `/*` 不在行首，不得被误当成块注释起笔。
    ///
    /// 故意在 glob 字面量之后再放一个**真**块注释（`/* trailing */`）：如果开启判据被放宽成
    /// 「整行任意位置出现 `/*`」，玩具字符串那行会被误判成起笔，一路吞到后面这个真注释的 `*/`
    /// 才闭合，中间那行含 `.onChanged(` 的真代码就会被整段清空——这正是「假起笔 + 真闭合 = 静默
    /// 吞掉真代码」的复现场景，比「假起笔 + 永远没有真闭合」（另一条用例覆盖）更危险：那种至少会
    /// 触发「找不到闭合」的原样保留兜底，这种则悄无声息。
    #[test]
    fn strip_block_comments_ignores_slash_star_mid_line() {
        let src = "const pattern = 'ui/**';\n\
                   const off = api.onChanged(() => {});\n\
                   /* trailing */\n";
        let stripped = strip_block_comments(src);
        assert!(
            stripped.contains("'ui/**'") && stripped.contains(".onChanged("),
            "`/*` 出现在行中段（字符串/glob 字面量）不是块注释起笔，不该一路吞到后面真注释的 `*/`：\
             {stripped:?}"
        );
    }

    /// **守卫的守卫**：找不到闭合 `*/` 不 panic，原样保留剩余行，交给上层的数量断言去暴露。
    #[test]
    fn strip_block_comments_leaves_unclosed_comment_untouched_instead_of_panicking() {
        let src = "/* 没有闭合\nconst off = api.onChanged(() => {});\n";
        let stripped = strip_block_comments(src);
        assert!(
            stripped.contains(".onChanged("),
            "未闭合块注释必须原样保留（不清空、不 panic），让 `.onChanged(` 继续可见：{stripped:?}"
        );
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

    /// **守卫的守卫**：[`match_arm_body`] 三种形态各来一格。
    ///
    /// 合成 body 而不是拿真源码：真源码里被守的那条臂**恰好是末臂**（`ManualDownload`），封顶循环
    /// 永不 `break` ⇒ 封顶与它的自检**零执行覆盖**，靠「它恰好排在最后」兜住。本轮复审实测把封顶
    /// 判据打死后那道门仍 ok，正是这个盲区。这里用合成样本把三格都跑到。
    ///
    /// 变异锁：把 `t.starts_with(variant_prefix) && t.contains("=>")` 换回按缩进宽度匹配
    /// （或整个删掉 `break`）⇒ 「臂在中间」那格转红。
    ///
    /// ⚠️ **本单测只覆盖列举到的输入形状，不担保封顶的正确性** —— 那由 [`match_arm_body`] 自身的
    /// 输出自检担保（形状无关）。实证：把封顶判据打死时，若只有本单测，三道真门全绿、只有这里红；
    /// 而折行 or-pattern 臂头这一格本单测根本列举不到。两者同时在，才既说得出「哪种输入」也拦得住
    /// 「没列举到的输入」。
    #[test]
    fn match_arm_body_stops_at_the_next_arm() {
        let body = "\
        A::First => {
            first_call();
        }
        A::Middle => {
            middle_call();
        }
        A::Last => {
            last_call();
        }
";
        // ① 臂在中间：必须封到下一条臂头，不得把后面的臂吞进来。
        let mid = match_arm_body(body, "A::Middle =>", "A::");
        assert!(mid.contains("middle_call()"), "臂体自己的内容没了");
        assert!(
            !mid.contains("last_call()"),
            "**封顶失效**：吞掉了下一条臂 → 别的臂会替被守的那条作证"
        );
        assert!(
            !mid.contains("first_call()"),
            "切片起点错了：不该包含臂头之前的内容"
        );

        // ② 臂在末尾：没有下一条臂头，切到结尾即可（不得 panic、不得吐空）。
        let last = match_arm_body(body, "A::Last =>", "A::");
        assert!(last.contains("last_call()"));
        assert!(!last.contains("middle_call()"));

        // ③ **单行** or-pattern 臂头（且缩进不同）照样识别为「下一条臂」。折行 or-pattern 不在
        //    本格射程内 —— 那一形由 `match_arm_body` 的输出自检兜住（见其 doc 的「射程」段）。
        let or_body = "\
    A::X => {
        x_call();
    }
        A::Y | A::Z => {
            yz_call();
        }
";
        let x = match_arm_body(or_body, "A::X =>", "A::");
        assert!(x.contains("x_call()"));
        assert!(
            !x.contains("yz_call()"),
            "or-pattern 臂头（且缩进不同）没被认出来 —— 判据别再回到缩进宽度"
        );
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
