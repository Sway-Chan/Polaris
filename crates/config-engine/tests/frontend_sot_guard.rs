//! **SoT 守门**：证明前端不再持有 Rust 已拥有的数据表。
//!
//! # 这扇门的形态与「对拍」不同
//!
//! 对拍的前提是两侧都有表。本批把表收敛到 Rust 一份后，门的形态变成**证明另一侧不存在** ——
//! 前端若再长出一张同构表（复制粘贴 / 回退 commit / 新人不知情地"补个常量"），本门转红。
//!
//! 迁移当时的对拍证据（一次性、跑完即删的 `zz_migration_parity_probe.rs`）：
//! 预设 16/16、catalog 33/33 逐字段全等，且对 Rust 表做变异验证 6/6 转红 → 确认 Rust 那份是
//! 忠实转录而非有损手抄，方才删 TS 表。本门是那份证据的**常驻延续**。
//!
//! # 为何扫描前必须剥注释
//!
//! 首版本门直接 `src.contains("APP_PRESETS")` → **被自己的文档注释误伤**：新文件头写着
//! 「这里曾有 `APP_PRESETS` 全表……」正是在解释迁移，却被判成「又长出第二份表」。
//! 会对散文误报的门，最后一定是被人删掉、而不是被人遵守。故扫描前剥掉块注释与整行行注释：
//! **只看代码，不看散文**。

/// 剥 TS 注释：`/* … */`（含 JSDoc `/** … */`）+ 整行 `//` 行注释。
///
/// 刻意**不**处理行尾 `//`：TS 里 `https://` 与正则 `/\//` 都含 `//`，按行尾切会误伤代码；
/// 而本门只需判「标识符是否出现在代码里」，散文全在块注释与整行注释中，剥这两类已足够。
fn strip_ts_comments(src: &str) -> String {
    let mut out = String::new();
    let mut chars = src.chars().peekable();
    let mut in_block = false;
    while let Some(c) = chars.next() {
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block = true;
            continue;
        }
        out.push(c);
    }
    out.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn frontend_code(rel: &str) -> String {
    let path = format!("{}/../../ui/src/{rel}", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读不到前端文件 {path}: {e}（文件移动请同步本测试）"));
    strip_ts_comments(&src)
}

#[test]
fn strip_ts_comments_removes_prose_but_keeps_code() {
    // 门的自检：剥注释器本身得对，否则整个门要么恒绿（假安全）要么恒红。
    let src = r#"
/**
 * 历史：这里曾有 APP_PRESETS 全表。
 */
// 行注释里也提 APP_PRESETS
const x = 1;
const re = /\/(geo|geo-lite)\//i; // 行尾注释含 APP_PRESETS
export const KEEP_ME = 2;
"#;
    let out = strip_ts_comments(src);
    assert!(!out.contains("历史"), "块注释未剥净");
    assert!(!out.contains("行注释里也提"), "整行行注释未剥净");
    assert!(out.contains("const x = 1;"), "代码被误删");
    assert!(out.contains("KEEP_ME"), "代码被误删");
    assert!(out.contains("geo-lite"), "含 // 的正则代码被误删");
}

#[test]
fn frontend_holds_no_second_preset_table() {
    // 前端曾持有 APP_PRESETS 全表 + QURE_BASE 图标基址（16 条 ↔ Rust 16 条，两处维护必然漂移，
    // 且 Rust 那份文件头还自认「来源 = 本 TS 文件」→ 两边都以为对方是抄本）。
    let code = frontend_code("domain/app-rules-preset.ts");
    for marker in ["APP_PRESETS", "QURE_BASE", "jsdelivr", "geositeTags: ["] {
        assert!(
            !code.contains(marker),
            "前端 app-rules-preset.ts 的**代码**里又出现 {marker:?} —— 内置预设表的 SoT 在 Rust \
             (config-engine/user_config/app_rules_preset_data.rs)，前端只应经 app_presets_list 拉取"
        );
    }
    // 反向自检：文件被清空/改名时上面的 !contains 会平凡通过 → 断言它仍是那个模块。
    assert!(
        code.contains("getAppPreset") && code.contains("mergeAppPresets"),
        "前端 app-rules-preset.ts 已非预期模块，本门失效"
    );
}

#[test]
fn frontend_holds_no_second_catalog_table() {
    let code = frontend_code("domain/rule-resource-catalog.ts");
    for marker in [
        "RULE_RESOURCE_CATALOG",
        "MRD_RAW_BASE",
        "raw.githubusercontent.com",
    ] {
        assert!(
            !code.contains(marker),
            "前端 rule-resource-catalog.ts 的**代码**里又出现 {marker:?} —— catalog 的 SoT 在 Rust \
             (config-engine/user_config/rule_resource_catalog.rs)，前端只应经 rule_resources_get_catalog 拉取"
        );
    }
    assert!(
        code.contains("deriveResourceMeta"),
        "前端 rule-resource-catalog.ts 已非预期模块，本门失效"
    );
}

#[test]
fn frontend_screen_renders_from_store_not_static_table() {
    // AppPolicyScreen 曾 `import { APP_PRESETS }` 直连静态表，且**内联重实现**了一份 merge
    // （与 getAppPreset 那份行为还不一致 —— category 一个用占位 'tools' 一个用真值）。
    let code = frontend_code("components/screens/apppolicy/AppPolicyScreen.tsx");
    assert!(
        !code.contains("APP_PRESETS"),
        "AppPolicyScreen 又直连静态预设表 —— 应订阅 useAppPresetsStore（Rust SoT）"
    );
    assert!(
        code.contains("mergeAppPresets") && code.contains("useAppPresetsStore"),
        "AppPolicyScreen 未走 store + 单一 merge 收口 —— 内联重实现 merge 是本批修掉的前置坑"
    );
}

/// 判断 TS 代码里是否**声明**了字段 `name`（形如 `name: T` 或 `name?: T`）。
///
/// 刻意不用 `code.contains(name)`：**首版就是这么写的，被变异验证抓了个假阴性** —— 把 Rust DTO 的
/// `labelKey` 改名成 `label`，`contains("label")` 仍命中前端的 `labelKey` → 门恒绿。
/// 故此处两侧都卡死：前面不能接标识符字符（`label` 不得命中 `labelKey`），后面必须是 `:` 或 `?:`。
fn declares_field(code: &str, name: &str) -> bool {
    code.match_indices(name).any(|(i, _)| {
        let before_ok = code[..i]
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$');
        let mut rest = code[i + name.len()..].chars();
        let after_ok = match rest.next() {
            Some(':') => true,
            Some('?') => rest.next() == Some(':'),
            _ => false,
        };
        before_ok && after_ok
    })
}

#[test]
fn declares_field_rejects_substring_of_longer_identifier() {
    // 门的自检（这条正是变异验证 M4 逼出来的）。
    let code = "export interface AppPreset { labelKey: string; iconUrl?: string; id: string; }";
    assert!(declares_field(code, "labelKey"));
    assert!(declares_field(code, "iconUrl"), "`?:` 形态应认");
    assert!(declares_field(code, "id"));
    assert!(!declares_field(code, "label"), "label 不得命中 labelKey");
    assert!(!declares_field(code, "icon"), "icon 不得命中 iconUrl");
    assert!(!declares_field(code, "AppPreset"), "类型名不是字段声明");
}

#[test]
fn app_presets_list_channel_is_synced_across_three_places() {
    // §N（灾难级）的复发面：前端 138 个 channel 常量里 136 个含冒号（Electron 遗产），
    // 能对上 Rust command 名的 **0 个** → 真机 `Command config:get not found`，而
    // **tsc 绿、vite build 绿**（类型全对，只是运行时名字对不上），错误又被 `.catch(()=>{})` 吞掉。
    //
    // 「加 command 要同步三处」靠人记 = 迟早再断。本门把三处钉死：
    //   ① 前端 channel 常量值  ② Rust `#[tauri::command]` fn 名  ③ generate_handler! 实际注册
    // 声明了没注册 = 一样调不到，故 ③ 必须单独验。
    const FN: &str = "app_presets_list";

    let root = env!("CARGO_MANIFEST_DIR");
    let channels = std::fs::read_to_string(format!("{root}/../../ui/src/domain/ipc-channels.ts"))
        .expect("读 ipc-channels.ts");
    let cmd_src = std::fs::read_to_string(format!("{root}/../../src-tauri/src/commands/rules.rs"))
        .expect("读 commands/rules.rs");
    let main_rs =
        std::fs::read_to_string(format!("{root}/../../src-tauri/src/main.rs")).expect("读 main.rs");

    // ① 前端常量值必须**恰好**是 snake_case fn 名。
    assert!(
        channels.contains(&format!("APP_PRESETS_LIST: '{FN}'")),
        "前端 APP_PRESETS_LIST 的值必须是 '{FN}'（= Rust fn 名）"
    );
    // Tauri command 名 = Rust 标识符 → 冒号不合法。（event 名才是自由字符串，冒号合法，勿混。）
    let line = channels
        .lines()
        .find(|l| l.contains("APP_PRESETS_LIST:"))
        .expect("找不到 APP_PRESETS_LIST 常量");
    let value = line.split('\'').nth(1).expect("常量值解析失败");
    assert!(
        !value.contains(':'),
        "channel 值 {value:?} 含冒号 —— Tauri command 名 = Rust 函数名，冒号在标识符里不合法"
    );

    // ② Rust 侧确实有这个 #[tauri::command]。
    assert!(
        cmd_src.contains(&format!("pub fn {FN}(")),
        "commands/rules.rs 里找不到 `pub fn {FN}`"
    );
    // ③ generate_handler! 里确实注册了（声明≠注册）。
    assert!(
        main_rs.contains(&format!("            {FN},")),
        "main.rs 的 generate_handler![] 未注册 {FN} —— 声明了没注册，前端一样调不到"
    );
}

#[test]
fn frontend_preset_interface_matches_rust_dto_fields() {
    // DTO 键名 = 前端 AppPreset interface 字段名。任一侧改名而另一侧未跟 → 前端拿不到数据，
    // 且 **tsc 不报**（invoke 返回值是 as-cast）→ 只能靠本门守。
    let code = frontend_code("domain/app-rules-preset.ts");
    let dto = &polaris_config_engine::user_config::all_presets_dto()[0];
    let json = serde_json::to_value(dto).expect("DTO 序列化");
    for key in json.as_object().expect("DTO 应为对象").keys() {
        assert!(
            declares_field(&code, key),
            "Rust DTO 有字段 {key:?}，但前端 AppPreset interface **未声明**它 → 跨语言漂移\
             （前端渲染会拿到 undefined，且 tsc 不报）"
        );
    }
}
