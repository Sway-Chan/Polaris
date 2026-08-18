use std::path::Path;

/// 随包 `resources/data/` 必须携带的 `.srs` 数量 = `builtin_geo_rulesets()` 的条目数。
///
/// **这里是副本，不是真值**：build script 不引 config-engine（避免为一次打包断言给 host 侧多挂一条
/// 依赖边），故数量在此硬编码。漂移由 `runtime/geo_seed.rs` 的
/// `build_rs_expected_count_matches_builtin_table` 守着——它读本文件、正则出这个常量、与
/// `builtin_geo_rulesets().len()` 对账，不一致即 gate 转红。**改表必改这里，否则测试先炸。**
const EXPECTED_SRS_COUNT: usize = 28;

fn main() {
    assert_bundled_geo_data();
    assert_bundled_dashboard();
    embed_test_manifest_on_windows_msvc();
    tauri_build::build();
}

/// W9：给**测试目标**嵌入 Common-Controls v6 manifest（仅 windows-msvc）。
///
/// 病理（run 32109642349 探针实证）：`tests/remote_webview_cannot_reach_app_commands` 的
/// 测试二进制导入 `comctl32.dll` 的 `TaskDialogIndirect / SetWindowSubclass /
/// RemoveWindowSubclass / DefSubclassProc`——这四个是 **v6 专属导出**。应用 exe 由
/// tauri-build 的 winres 拿到 v6 manifest（依赖声明），**测试 exe 拿不到**（winres 的
/// link-arg 只打 bins），于是加载器把 comctl32 绑到 System32 的 v5.82 ⇒ 加载期
/// `STATUS_ENTRYPOINT_NOT_FOUND`（0xC0000139），一条测试都没执行——W9 登记的
/// 「零结论不是绿」的根因。
///
/// 修法即补齐缺口：`cargo:rustc-link-arg-tests` 把同款 manifest 嵌进本包全部测试目标。
/// manifest 内容与 tauri-build 2.6.3 自带的 `windows-app-manifest.xml` 等义（多一个可选
/// XML 声明头，无语义影响；Common-Controls 6.0.0.0 依赖），刻意不引它的私有路径。
fn embed_test_manifest_on_windows_msvc() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-test-manifest.xml");
    // 本 build.rs 已有显式 rerun-if-changed 声明 ⇒ cargo 只按声明路径重跑：manifest 编辑
    // 必须自己声明，否则测试二进制静默沿用旧嵌入内容（复审 F1）。
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
        manifest.display()
    );
}

/// **随包 dashboard 完整性断言（打包期硬门）** —— [`assert_bundled_geo_data`] 的同构对等物。
///
/// 为什么从 `beforeBundleCommand` 搬到这里（2026-08-05，Windows 打包腿首次真跑时挂在那个 hook 上）：
///
/// Tauri 跑 hook 的 shell 是**按宿主平台**选的：Unix 走 `sh -c`，Windows 走 `cmd /C`。原 hook 是
///   `if [ -f scripts/ensure-dashboard.sh ]; then sh …; else sh ../scripts/…; fi`
/// —— 纯 sh 语法，在 linux/macOS 腿上成立，到 Windows 交给 `cmd` 必然 exit 1（实测 run
/// 30983415697：`beforeBundleCommand … failed with exit code 1`，且它挂在**整条 Rust 编译之后**，
/// 每验证一轮先烧 20 分钟 × 2x 计费）。
///
/// 那个 `if` 本身是为了绕开「hook 的 cwd 到底是仓库根还是 src-tauri」这个不确定性
/// （2026-07-20 的实测结论在 CLI 2.11.4 上已不成立，见 `scripts/ensure-dashboard.sh` 头注）。
///
/// 搬到 build script 后两个不确定性一起消失：**没有 shell**（不必同时满足 sh 与 cmd 两套语法），
/// **cwd 由 cargo 保证**（`CARGO_MANIFEST_DIR` 是编译期常量，与调用方 cwd 无关）。
///
/// [不选「per-platform conf 里给 Windows 覆盖一份 hook」：`verify-packaging.mjs` 的 conf 不变量
///  明写四份 per-platform conf **顶层只准 `$schema` + `bundle`、`bundle` 下只准 `resources`**
///  （变异 M10 实测过，防的是 `--config` 按 RFC 7396 覆盖 base 的 version/productName），
///  加 `build.beforeBundleCommand` 会撞这道门，而门槛不为了让改动通过而放宽]
/// [不选「hook 改成 `node scripts/…`」：打包机 mac 5.238 上 node **装了但不在 PATH 上** ——
///  它在 `/opt/homebrew/bin`，而该目录不在 ssh 会话（含 `bash -lc` 登录 shell）的 PATH 里。
///  见 `polaris-mac-deploy-recipe.md`「5.238 工具链状态（2026-07-20 订正）」：把这个现象误读成
///  「没装 Node」曾造成两轮部署失败 + 一次白编译。hook 由 tauri-cli 在**它自己的进程环境**下拉起，
///  拿到的就是那份缺目录的 PATH ⇒ `node` 仍会 not found。build script 不依赖任何解释器，绕开整类问题]
///
/// 判据用 **非空**而不是「文件存在」：0 字节 / 404 HTML 占位文件打进包里，与没有它同一后果 ——
/// 核只能回落联网下载面板（离线不可用；CWD 只读时还会刷 mkdir 报错）。
///
/// **只在 release 生效**（同 geo 那条）：release ⟺ 会被打包分发的那份。
///
/// ⚠️ **不再顺手拉取**。原 hook 在有 node 时会替你跑一次 `fetch-dashboard.mjs`；build script 里不做
/// 这件事（build script 不该联网，且它在 sandbox / 离线构建下会变成硬失败）。CI 侧本来就有独立的
/// `Fetch sing-box dashboard` 步，不受影响；本机构建按下面 panic 文案里给的命令手动拉一次即可。
fn assert_bundled_dashboard() {
    let index = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resources")
        .join("dashboard")
        .join("index.html");
    println!("cargo:rerun-if-changed={}", index.display());

    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }

    let size = std::fs::metadata(&index).map(|m| m.len()).unwrap_or(0);
    assert!(
        size > 0,
        "随包 dashboard 资源缺失或为 0 字节，拒绝出包：{}\n\
         后果：面板资源为空仍会被 bundler 原样打进安装包，核只能回落联网下载面板 —— \
         离线不可用，且 CWD 只读时还会刷 mkdir 报错。\n\
         修复（有 Node 的机器）：node scripts/fetch-dashboard.mjs\n\
         修复（无 Node 的打包机）：先在有 Node 的机器跑上面那条，再把 resources/dashboard/ rsync 过来。",
        index.display()
    );
}

/// **随包 geo 资源完整性断言（打包期硬门）**。
///
/// 为什么要有：`resources/data/` 的 28 个 `.srs` 是「智能分流能不能工作」的物理前提——缺了它们
/// `runtime_rules_dir` 种不满 → route builder fail-closed 剪掉全部 geo 规则 → 叠加回国模式即
/// **全量明文直连**（真机 2026-07-20）。此前唯一盯着这件事的是 `geo_seed.rs` 的 cargo test，而它读
/// **工作树**：本机工作树里有、但漏 `git add` 时本机 gate 照样全绿，只有 CI 干净 clone 才转红；
/// 而打包机拿的是工作树 rsync，于是「随包资源缺失」这条腿在真正出包的那台机器上**没有门**。
///
/// 另一条随包资源链（dashboard）的对等断言是本文件的 [`assert_bundled_dashboard`]
/// （2026-08-05 从 `beforeBundleCommand` 搬来，理由见那个函数的文档）。两条链现在守门强度一致、
/// 机制同源：同一个 build script、同样 release-only、同样按「内容有效」而非「文件存在」判定。
///
/// 校验用 **SRS 魔数**而非「文件存在」：0 字节 / 404 HTML 污染的占位文件打进包里，与没有它是同一个后果
/// （route builder 的 `is_valid_srs_fn` 判无效 → 照剪）。判据与运行时那侧逐字节同源。
///
/// **只在 release 生效**：debug（开发 / CI 单测）不阻断——那里由 `geo_seed.rs` 的
/// `real_bundled_resources_cover_every_builtin_tag` 按真值表逐 tag 覆盖。release ⟺ 会被打包分发的那份。
fn assert_bundled_geo_data() {
    let data_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resources")
        .join("data");
    println!("cargo:rerun-if-changed={}", data_dir.display());

    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }

    let mut valid = 0usize;
    let mut bad: Vec<String> = Vec::new();
    match std::fs::read_dir(&data_dir) {
        Ok(entries) => {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".srs") {
                    continue;
                }
                if has_srs_magic(&e.path()) {
                    valid += 1;
                } else {
                    bad.push(name);
                }
            }
        }
        Err(e) => panic!(
            "随包 geo 资源目录读不到，拒绝出包：{} ({e})\n\
             后果：runtime rules 目录种不满 → route builder fail-closed 剪掉全部 geo 分流规则；\
             叠加回国模式即全量明文直连。",
            data_dir.display()
        ),
    }

    assert!(
        bad.is_empty(),
        "随包 geo 资源魔数校验失败（0 字节 / 下载污染），拒绝出包：{}\n目录：{}",
        bad.join(", "),
        data_dir.display()
    );
    assert!(
        valid >= EXPECTED_SRS_COUNT,
        "随包 geo 资源不足：{valid} / {EXPECTED_SRS_COUNT} 个有效 .srs，拒绝出包。\n\
         目录：{}\n\
         后果：runtime rules 目录种不满 → route builder fail-closed 剪掉引用它们的分流规则；\
         叠加回国模式即全量明文直连（真机 2026-07-20）。\n\
         修复：确认 resources/data/ 已随仓库提交（git status 查 untracked），\
         或从有完整资源的机器 rsync 该目录后重打包。",
        data_dir.display()
    );
}

/// SRS 魔数（`'S' 'R' 'S'`）判定。与 `config_engine::user_config::builtin_geo_rulesets::is_valid_srs_file`
/// 同判据——build script 不引 config-engine，故此处是 3 字节的独立实现（不值得为它加一条依赖边）。
fn has_srs_magic(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 3];
    f.read_exact(&mut buf).is_ok() && &buf == b"SRS"
}
