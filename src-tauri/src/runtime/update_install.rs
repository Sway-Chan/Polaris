//! App 自更新的**安装计划决策 + 平台脚本生成**（移植 上游 `update-install-script.ts`）。
//!
//! # 结构纪律：决策与执行切开
//!
//! 本模块 99% 是**纯函数**（形态判定 / 计划决策 / 脚本文本生成），可在本机零副作用单测；
//! 真正的执行腿只有一个 [`spawn_detached_script`]（写脚本 + `spawn(detached)`），且**本机绝不跑**
//! （改写宿主应用本体 + 需提权，属真机门）。
//!
//! # 为什么是「自写脚本」而不是 updater 框架
//!
//! 上游 全程**不用任何 updater 框架、不用任何应用级签名密钥对**（`UpdateService.ts` 1212 行手写）。
//! Polaris 同理：不引 `tauri-plugin-updater`，因而**不需要 minisign 密钥对**。真伪由 OS 层负责
//! （见下方「代码签名：ad-hoc 的后果」）。
//!
//! # 代码签名：ad-hoc 的后果（**用户已拍板走 ad-hoc，不买证书**）
//!
//! 没有 Developer ID / Authenticode ⇒ OS 层的「真伪」校验会拦下来。两平台表现与对策：
//!
//! | 平台 | 症状 | 应用内能否消除 | 本模块的处置 |
//! |---|---|---|---|
//! | macOS | 下载件带 `com.apple.quarantine` → Gatekeeper「来自身份不明的开发者」，**装完打不开** | ✅ **能** | 安装脚本在替换成功后**必跑** `xattr -dr com.apple.quarantine`（对齐 上游 `update-install-script.ts:259`）+ ad-hoc 重签兜底；失败分支给出「右键→打开」指引 |
//! | Windows | SmartScreen「未知发布者」 | ❌ **不能**（唯一解是买证书 / 攒信誉） | 安装**前**经 [`install_advisory`] 返 [`InstallAdvisory::WindowsSmartScreen`]，UI 弹说明框告知「更多信息 → 仍要运行」的具体点法 |
//!
//! **诚实原则**：无法在应用内消除拦截的平台，**必须**在安装前把用户可执行的下一步说清楚
//! （不是装完了事）。该判定是纯函数 [`install_advisory`]，有单测 + 变异用例钉死。
//!
//! # 移植对照
//!
//! | 平台 | 上游 | 本模块 |
//! |---|---|---|
//! | Windows 便携/NSIS | `buildWindowsUpdateVbs`（UTF-16LE+BOM 的 `.vbs`，`wscript` 无窗口跑） | [`build_install_script`] → [`ScriptSpec`] |
//! | macOS | `buildMacUpdateScript`（`hdiutil attach` → `ditto` 暂存 → mv-swap 原子替换 → `xattr -dr`） | 同上 |
//! | Linux AppImage | `buildLinuxAppImageScript`（覆盖 `$APPIMAGE` + chmod +x） | 同上 |
//! | Linux deb | `buildLinuxDebScript`（`pkexec apt-get install`） | 同上 |
//! | 形态错配 | 不强制 root，回退 `shell.openPath` 交系统（`UpdateService.ts:427-436`） | [`InstallReject::FormMismatch`]，command 层回退 `shell.open` |

use std::path::{Path, PathBuf};

// ── 运行形态 / 资产形态（纯判定）────────────────────────────────────────────

/// 应用运行形态（= 上游的 loose vs installed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunForm {
    /// 便携/免安装（Windows portable、Linux AppImage、macOS `.app`）。
    Loose,
    /// 安装态（Windows NSIS、Linux deb）。
    Installed,
}

/// 下载件的资产形态（由文件名后缀判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerKind {
    /// Windows 可执行安装器 / 便携包。
    WinExe,
    /// macOS 磁盘镜像。
    Dmg,
    /// Linux AppImage。
    AppImage,
    /// Linux Debian 包。
    Deb,
}

/// 由资产文件名判定形态（**纯函数**）。无法识别 → `None`。
///
/// 大小写：`.AppImage` 是官方命名（驼峰），但用户重命名/镜像改名很常见 → 一律按小写比较。
#[must_use]
pub fn classify_installer(file_name: &str) -> Option<InstallerKind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".exe") {
        Some(InstallerKind::WinExe)
    } else if lower.ends_with(".dmg") {
        Some(InstallerKind::Dmg)
    } else if lower.ends_with(".appimage") {
        Some(InstallerKind::AppImage)
    } else if lower.ends_with(".deb") {
        Some(InstallerKind::Deb)
    } else {
        None
    }
}

/// 运行形态判定（**纯函数**：形态证据由调用方注入，本函数不读环境、不碰文件系统）。
///
/// - Linux：`APPIMAGE` 由 AppImage 运行时注入（真值，Electron/Tauri 通用）→ Loose；否则 deb 安装态。
/// - Windows：便携判据 = **exe 同级的 `portable.marker` 标记文件** —— 打包侧
///   `.github/workflows/package.yml` 的 `Build Windows portable zip` 步写进 zip 根，运行侧
///   `commands/updater.rs::is_portable_layout` 探测。**判定只在那一处实现**，本函数只消费其结果：
///   `Some(便携 exe 路径)` → Loose；`None` → Installed（选 NSIS setup，保守且不会误覆盖）。
///   判据**不是** electron-builder 的 `PORTABLE_EXECUTABLE_FILE`（那是它自解压 stub 注入的，
///   Polaris 的便携版是纯 zip、无 stub ⇒ 该 env 恒不存在，成因详见 `is_portable_layout` 文档）。
/// - macOS：`.app` 恒 Loose（不分形态）。
///
/// ⚠️ **已知边界（未解决，如实登记）**：用户手动删掉 `portable.marker` 后判定退回 Installed，
/// 该用户会重新被推 NSIS 安装器。方向是失败安全的那一侧（安装器能装、不会砸掉便携副本），
/// 但**没有任何自动手段能守住它** —— 兜底只有标记文件内写明的「勿删」。
#[must_use]
pub fn detect_run_form(
    os: &str,
    appimage_env: Option<&Path>,
    portable_exe: Option<&Path>,
) -> RunForm {
    match os {
        "linux" => {
            if appimage_env.is_some() {
                RunForm::Loose
            } else {
                RunForm::Installed
            }
        }
        "windows" => {
            if portable_exe.is_some() {
                RunForm::Loose
            } else {
                RunForm::Installed
            }
        }
        "macos" => RunForm::Loose,
        _ => RunForm::Installed,
    }
}

// ── 安装计划 ────────────────────────────────────────────────────────────────

/// 安装路径（平台 × 形态的笛卡儿积收敛后的五种真实执行路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPlatform {
    /// Windows 便携：把新版本名文件移入原目录 + 删旧版本名。
    WindowsPortable,
    /// Windows 安装态：跑 NSIS setup 原位升级。
    WindowsSetup,
    /// macOS：挂 DMG → 暂存 → mv-swap 原子替换 `.app` → 清 quarantine。
    Macos,
    /// Linux AppImage：覆盖 `$APPIMAGE` 原位。
    LinuxAppImage,
    /// Linux deb：`pkexec apt-get install` 原位升级。
    LinuxDeb,
}

/// 安装计划（[`build_install_script`] 的唯一输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub platform: InstallPlatform,
    /// 已下载的安装件路径。
    pub installer_path: PathBuf,
    /// 当前应用可执行文件路径（重启用）。
    pub exe_path: PathBuf,
    /// Windows 便携：原便携 exe 路径（被覆盖的目标）。
    pub portable_target: Option<PathBuf>,
    /// Windows 便携：新版本名文件在原目录的落点（保留 release 的带版本号命名）。
    pub portable_new_path: Option<PathBuf>,
    /// macOS：`.app` 包路径；`None` = 定位不到 → 回退 `open` DMG 手动拖拽。
    pub app_bundle_path: Option<PathBuf>,
    /// Linux AppImage：`$APPIMAGE` 原位路径。
    pub appimage_target: Option<PathBuf>,
}

/// 安装计划被拒的原因（**不是错误，是安全兜底**：拒绝即回退交系统处理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallReject {
    /// 资产形态与当前运行形态不匹配（如 AppImage 运行 + `.deb` 资产）。
    ///
    /// **绝不强制 root 安装**（= 上游 `UpdateService.ts:427-436`）：错配下自动提权装 deb
    /// 会在 AppImage 用户机器上装出第二份、且要 root，是最坏结果。回退 `shell.open` 交系统。
    FormMismatch {
        installer: String,
        os: String,
        form: RunForm,
    },
    /// 文件名无法识别为任何已知安装件形态。
    UnknownAsset { file_name: String },
}

/// 从可执行路径推导 `.app` 包路径（**纯函数**，= 上游 `macAppBundleFromExe`）。
///
/// `/Applications/Polaris.app/Contents/MacOS/polaris` → `/Applications/Polaris.app`；不匹配返 `None`。
#[must_use]
pub fn mac_app_bundle_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let s = exe_path.to_str()?;
    let idx = s.find(".app/Contents/MacOS/")?;
    let bundle = &s[..idx + 4];
    // 尾段必须是单层文件名（`/Contents/MacOS/<name>`，name 内无 `/`）。
    let tail = &s[idx + ".app/Contents/MacOS/".len()..];
    if tail.is_empty() || tail.contains('/') {
        return None;
    }
    Some(PathBuf::from(bundle))
}

/// 安装计划决策（**纯函数**：全部环境真值由参数注入 → 三平台真值表可在 Linux 上单测）。
///
/// # Errors
///
/// - [`InstallReject::UnknownAsset`]：文件名后缀不认识。
/// - [`InstallReject::FormMismatch`]：资产形态与 OS/运行形态错配。
pub fn decide_install_plan(
    os: &str,
    run_form: RunForm,
    installer_path: &Path,
    exe_path: &Path,
    appimage_env: Option<&Path>,
    portable_exe: Option<&Path>,
) -> Result<InstallPlan, InstallReject> {
    let file_name = installer_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(kind) = classify_installer(&file_name) else {
        return Err(InstallReject::UnknownAsset { file_name });
    };
    let mismatch = || InstallReject::FormMismatch {
        installer: file_name.clone(),
        os: os.to_string(),
        form: run_form,
    };

    let base = |platform: InstallPlatform| InstallPlan {
        platform,
        installer_path: installer_path.to_path_buf(),
        exe_path: exe_path.to_path_buf(),
        portable_target: None,
        portable_new_path: None,
        app_bundle_path: None,
        appimage_target: None,
    };

    match (os, kind) {
        ("windows", InstallerKind::WinExe) => {
            if run_form == RunForm::Loose {
                // 便携：新版本名文件落在**原 exe 所在目录**（保留 release 的带版本号命名）。
                let target = portable_exe
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| exe_path.to_path_buf());
                let new_path = target
                    .parent()
                    .map(|d| d.join(&file_name))
                    .unwrap_or_else(|| target.clone());
                Ok(InstallPlan {
                    portable_target: Some(target),
                    portable_new_path: Some(new_path),
                    ..base(InstallPlatform::WindowsPortable)
                })
            } else {
                Ok(base(InstallPlatform::WindowsSetup))
            }
        }
        ("macos", InstallerKind::Dmg) => Ok(InstallPlan {
            app_bundle_path: mac_app_bundle_from_exe(exe_path),
            ..base(InstallPlatform::Macos)
        }),
        ("linux", InstallerKind::AppImage) => {
            // AppImage 资产只在 AppImage 运行形态下原位覆盖；deb 安装态拿到 AppImage 属错配。
            if run_form != RunForm::Loose {
                return Err(mismatch());
            }
            let Some(target) = appimage_env else {
                return Err(mismatch());
            };
            Ok(InstallPlan {
                appimage_target: Some(target.to_path_buf()),
                ..base(InstallPlatform::LinuxAppImage)
            })
        }
        ("linux", InstallerKind::Deb) => {
            // **关键安全闸**：AppImage 运行形态拿到 .deb → 拒绝（绝不 pkexec 装 deb）。
            if run_form != RunForm::Installed {
                return Err(mismatch());
            }
            Ok(base(InstallPlatform::LinuxDeb))
        }
        _ => Err(mismatch()),
    }
}

// ── 安装前告知（ad-hoc 签名 / 提权的用户可执行下一步）───────────────────────

/// 安装前必须告知用户的事项（**纯函数判定**，见模块文档「代码签名：ad-hoc 的后果」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAdvisory {
    /// Linux deb：即将弹 polkit 提权框（= 上游 `confirmDebElevation`）。
    ///
    /// **必须在停代理之前弹**：用户取消即真 no-op，不留「代理被停但没更新」坏态
    /// （上游 `UpdateService.ts:306-315` 明确注释）。
    DebElevation,
    /// Windows：无 Authenticode 证书 → SmartScreen「未知发布者」。
    ///
    /// **应用内无法消除**（唯一解是买证书或攒 reputation）⇒ 只能提前把「更多信息 → 仍要运行」讲清楚。
    WindowsSmartScreen,
    /// macOS：ad-hoc 签名 → 下载件带 quarantine。
    ///
    /// 脚本会自动 `xattr -dr com.apple.quarantine` 清除；**万一失败**要告诉用户「右键 → 打开」。
    MacosGatekeeper,
}

/// 该计划是否需要安装前告知用户（**纯函数**）。
///
/// 返回 `Some` 时 command 层必须先把说明交给 UI 确认，**确认后**才停代理 + 起脚本。
#[must_use]
pub fn install_advisory(plan: &InstallPlan) -> Option<InstallAdvisory> {
    match plan.platform {
        InstallPlatform::LinuxDeb => Some(InstallAdvisory::DebElevation),
        InstallPlatform::WindowsPortable | InstallPlatform::WindowsSetup => {
            Some(InstallAdvisory::WindowsSmartScreen)
        }
        InstallPlatform::Macos => Some(InstallAdvisory::MacosGatekeeper),
        // AppImage 原位覆盖：无签名校验、无提权，装完直接跑 → 无需额外告知。
        InstallPlatform::LinuxAppImage => None,
    }
}

impl InstallAdvisory {
    /// 前端 i18n key 后缀（UI 据此取 `settings.update.advisory.<key>` 文案）。
    ///
    /// **Rust 侧不建 i18n 框架**（YAGNI）：这里只出 key，文案全在 `ui/src/i18n/locales/*.json`。
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::DebElevation => "debElevation",
            Self::WindowsSmartScreen => "windowsSmartScreen",
            Self::MacosGatekeeper => "macosGatekeeper",
        }
    }
}

// ── 脚本生成（纯字符串）──────────────────────────────────────────────────────

/// 生成的安装脚本规格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSpec {
    /// 脚本文件名（落到临时目录）。
    pub file_name: String,
    /// 脚本字节（**Windows 是 UTF-16LE + BOM**，故是字节而非 String）。
    pub bytes: Vec<u8>,
    /// 解释器程序。
    pub program: String,
    /// 解释器参数（脚本路径由 command 层追加到末尾）。
    pub leading_args: Vec<String>,
}

/// POSIX shell 单引号转义（= 上游 `shq`）。
#[must_use]
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// VBS 字符串字面量：反斜杠双写（Windows 容忍双反斜杠路径，= 上游 `vbsPath`）。
#[must_use]
pub fn vbs_path(s: &str) -> String {
    s.replace('\\', r"\\")
}

/// VBS 字符串字面量：双引号双写（= 上游 `vbsStr`）。
#[must_use]
pub fn vbs_str(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// UTF-16LE + BOM 编码（**Windows `.vbs` 必须**）。
///
/// # 为什么非它不可（上游的血泪注释，`UpdateService.ts:354-356`）
///
/// `wscript.exe` 按**系统代码页**解释无 BOM 的脚本。中文用户名路径
/// （`C:\Users\张三\AppData\Local\Temp\...`）在 UTF-8 字节被按 GBK 解释时会乱码 →
/// `fso.CopyFile` 找不到文件 → 更新静默失败。带 BOM 的 UTF-16LE 让 wscript 走 Unicode 路径。
#[must_use]
pub fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE]; // BOM
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// 脚本里的用户可见文案（**由 command 层按 locale 注入**；Rust 侧不建 i18n 框架）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTexts {
    /// Windows 便携覆盖失败时的 MsgBox 提示。
    pub portable_manual_replace: String,
    /// 产品名（MsgBox 标题）。
    pub product: String,
}

impl Default for InstallTexts {
    fn default() -> Self {
        Self {
            portable_manual_replace:
                "Polaris could not replace the portable executable. The new version was downloaded to the path below — please replace it manually:"
                    .to_string(),
            product: "Polaris".to_string(),
        }
    }
}

/// 按计划生成安装脚本（**纯函数**：同一 plan 恒得同一字节序列，可快照断言）。
#[must_use]
pub fn build_install_script(plan: &InstallPlan, texts: &InstallTexts) -> ScriptSpec {
    match plan.platform {
        InstallPlatform::WindowsPortable | InstallPlatform::WindowsSetup => {
            let text = build_windows_vbs(plan, texts);
            ScriptSpec {
                file_name: "polaris-update.vbs".to_string(),
                bytes: utf16le_with_bom(&text),
                program: "wscript.exe".to_string(),
                leading_args: vec![],
            }
        }
        InstallPlatform::Macos => ScriptSpec {
            file_name: "polaris-update.sh".to_string(),
            bytes: build_mac_script(plan).into_bytes(),
            program: "/bin/bash".to_string(),
            leading_args: vec![],
        },
        InstallPlatform::LinuxAppImage => ScriptSpec {
            file_name: "polaris-update.sh".to_string(),
            bytes: build_linux_appimage_script(plan).into_bytes(),
            program: "/bin/bash".to_string(),
            leading_args: vec![],
        },
        InstallPlatform::LinuxDeb => ScriptSpec {
            file_name: "polaris-update.sh".to_string(),
            bytes: build_linux_deb_script(plan).into_bytes(),
            program: "/bin/bash".to_string(),
            leading_args: vec![],
        },
    }
}

/// Windows 更新 VBS（移植 `buildWindowsUpdateVbs`）。行分隔符是 `\r\n`（wscript 要求）。
fn build_windows_vbs(plan: &InstallPlan, texts: &InstallTexts) -> String {
    let src_raw = plan.installer_path.to_string_lossy().into_owned();
    let src = vbs_path(&src_raw);

    let Some(old_exe_p) = plan.portable_target.as_ref() else {
        // NSIS 安装态：跑 setup 原位升级 + 删自身。
        return [
            "WScript.Sleep 2000".to_string(),
            "Set WshShell = CreateObject(\"WScript.Shell\")".to_string(),
            // 🔴 `/UPDATE`，**不是 上游的 `--updated`**（2026-08-05 修）：`--updated` 是
            // **electron-builder** 的约定（它的模板里由 `${isUpdated}` 消费），换到 Tauri 后不成立。
            // Tauri 的 NSIS 模板解析的是 `/UPDATE`（tauri-cli 2.11.4 内嵌模板逐字：
            // `${GetOptions} $CMDLINE "/UPDATE" $UpdateMode`，安装器与卸载器双侧各一处），
            // 且 `$UpdateMode` 有真实语义：
            //   · `${If} $UpdateMode = 1 → Goto reinst_done` —— 跳过「卸载旧版 / 重装」选择页
            //   · `${If} $UpdateMode <> 1` 才装 WebView2 —— 升级时不重跑 WebView2 安装
            //   · 卸载器侧三处 `${If} $UpdateMode <> 1` 清理闸 —— 升级时不做整卸清理
            // `${GetOptions}` 匹配不到只置 error flag、`$UpdateMode` 停在 0 ⇒ 传错 flag 不会报错，
            // 而是**静默降级成全新安装模式**。
            format!("WshShell.Run \"\"\"{src}\"\" /UPDATE\", 1, False"),
            "Set fso = CreateObject(\"Scripting.FileSystemObject\")".to_string(),
            "fso.DeleteFile WScript.ScriptFullName, True".to_string(),
        ]
        .join("\r\n");
    };

    let old_exe = vbs_path(&old_exe_p.to_string_lossy());
    let new_exe = vbs_path(
        &plan
            .portable_new_path
            .as_ref()
            .unwrap_or(old_exe_p)
            .to_string_lossy(),
    );
    let msg = vbs_str(&texts.portable_manual_replace);
    let product = vbs_str(&texts.product);
    // MsgBox 展示用**单**反斜杠原路径（双写路径给文件操作「容忍」用，直接展示/粘贴资源管理器不友好）。
    let src_display = vbs_str(&src_raw);

    [
        "WScript.Sleep 2000".to_string(),
        "Set WshShell = CreateObject(\"WScript.Shell\")".to_string(),
        "Set fso = CreateObject(\"Scripting.FileSystemObject\")".to_string(),
        format!("src = \"{src}\""),
        format!("oldExe = \"{old_exe}\""),
        format!("newExe = \"{new_exe}\""),
        "On Error Resume Next".to_string(),
        // 清上次残留 .old（新旧两路径都清）。
        "If fso.FileExists(newExe & \".old\") Then fso.DeleteFile newExe & \".old\", True"
            .to_string(),
        "If fso.FileExists(oldExe & \".old\") Then fso.DeleteFile oldExe & \".old\", True"
            .to_string(),
        "Err.Clear".to_string(),
        // 新版本名文件若已存在（重装同版本/残留被锁）→ rename 挪开腾出原名。
        "If fso.FileExists(newExe) Then fso.MoveFile newExe, newExe & \".old\"".to_string(),
        "Err.Clear".to_string(),
        "fso.CopyFile src, newExe, True".to_string(),
        "If Err.Number = 0 Then".to_string(),
        "  Err.Clear".to_string(),
        // 删旧版本名文件：被 stub 锁 → DeleteFile 失败则 rename 到 .old，下次启动清。
        "  If LCase(oldExe) <> LCase(newExe) Then".to_string(),
        "    fso.DeleteFile oldExe, True".to_string(),
        "    If Err.Number <> 0 Then".to_string(),
        "      Err.Clear".to_string(),
        "      fso.MoveFile oldExe, oldExe & \".old\"".to_string(),
        "    End If".to_string(),
        "  End If".to_string(),
        "  Err.Clear".to_string(),
        "  WshShell.Run \"\"\"\" & newExe & \"\"\"\", 1, False".to_string(),
        "  fso.DeleteFile src, True".to_string(),
        "Else".to_string(),
        // 写新名失败（原目录只读）→ **不静默**：跑临时新版 + 明确提示手动替换。
        "  Err.Clear".to_string(),
        "  WshShell.Run \"\"\"\" & src & \"\"\"\", 1, False".to_string(),
        format!("  MsgBox \"{msg}\" & vbCrLf & \"{src_display}\", 48, \"{product}\""),
        "End If".to_string(),
        "On Error Goto 0".to_string(),
        "fso.DeleteFile WScript.ScriptFullName, True".to_string(),
    ]
    .join("\r\n")
}

/// macOS 更新脚本（移植 `buildMacUpdateScript`）。
///
/// # ad-hoc 签名的关键一步
///
/// 替换成功后**必跑** `xattr -dr com.apple.quarantine "$DEST"`：ad-hoc 签名的 `.app` 一旦带
/// quarantine 属性，Gatekeeper 直接拒绝启动（「来自身份不明的开发者」）——**用户点了更新、装完打不开**，
/// 比不做更新还糟。再加一道 `codesign` 校验 + ad-hoc 重签兜底；两步都失败才落到「右键→打开」指引。
fn build_mac_script(plan: &InstallPlan) -> String {
    let dmg = sh_quote(&plan.installer_path.to_string_lossy());
    let Some(bundle) = plan.app_bundle_path.as_ref() else {
        // 定位不到 `.app` → 回退手动拖拽（**不猜路径**）。
        return format!("#!/bin/bash\nsleep 2\nopen {dmg}\n");
    };
    let dest = sh_quote(&bundle.to_string_lossy());
    [
        "#!/bin/bash",
        "sleep 2",
        &format!("DMG={dmg}"),
        &format!("DEST={dest}"),
        "BAK=\"$DEST.bak-$$\"",
        "STAGE=\"$(dirname \"$DEST\")/.polaris-update-$$.app\"",
        // 清历史中断遗留的暂存（防 /Applications 下垃圾堆积）。
        "rm -rf \"$(dirname \"$DEST\")\"/.polaris-update-*.app 2>/dev/null",
        "MNT=$(hdiutil attach \"$DMG\" -nobrowse -noautoopen -mountrandom /tmp 2>/dev/null | grep -o \"/tmp/[^[:space:]]*\" | tail -1)",
        "[ -z \"$MNT\" ] && { open \"$DMG\"; exit 0; }",
        "SRC=$(/usr/bin/find \"$MNT\" -maxdepth 1 -name \"*.app\" | head -1)",
        "[ -z \"$SRC\" ] && { hdiutil detach \"$MNT\" >/dev/null 2>&1; rm -f \"$DMG\"; exit 0; }",
        "rm -rf \"$STAGE\"",
        "if ! ditto \"$SRC\" \"$STAGE\"; then hdiutil detach \"$MNT\" >/dev/null 2>&1; rm -rf \"$STAGE\"; rm -f \"$DMG\"; exit 0; fi",
        "hdiutil detach \"$MNT\" >/dev/null 2>&1",
        // mv-swap 原子替换（**绝不先毁后建**）；失败回滚旧版并保留 $BAK 可人工恢复。
        "replace() {",
        "  mv \"$DEST\" \"$BAK\" 2>/dev/null || return 1",
        "  if mv \"$STAGE\" \"$DEST\" 2>/dev/null; then rm -rf \"$BAK\"; return 0; fi",
        "  mv \"$BAK\" \"$DEST\" 2>/dev/null; return 1",
        "}",
        "if ! replace; then",
        // 无提权失败 → 同一 mv-swap 落盘成脚本，osascript 一次性 root 跑（系统原生密码框，不依赖常驻 helper）。
        "  ELEV=\"$(dirname \"$DEST\")/.polaris-elev-$$.sh\"",
        "  cat > \"$ELEV\" <<EOF",
        "#!/bin/bash",
        "mv $(printf %q \"$DEST\") $(printf %q \"$BAK\") || exit 1",
        "mv $(printf %q \"$STAGE\") $(printf %q \"$DEST\") || { mv $(printf %q \"$BAK\") $(printf %q \"$DEST\"); exit 1; }",
        "rm -rf $(printf %q \"$BAK\")",
        "EOF",
        "  osascript -e \"do shell script \\\"/bin/bash '$ELEV'\\\" with administrator privileges\" 2>/dev/null",
        "  rm -f \"$ELEV\"",
        "fi",
        // 成功判据取 **$STAGE 是否已被移走**，不取 `[ ! -d "$BAK" ]`。
        //
        // `$BAK` 不在场是**三种状态共有**的，其中两种是失败：
        //   ① 真成功（`mv "$STAGE" "$DEST"` 后 `rm -rf "$BAK"`）
        //   ② 第二步 mv 失败 → 脚本自己回滚（`mv "$BAK" "$DEST"`）⇒ $BAK 也不在，而 $DEST 是**旧版**
        //   ③ 第一步 mv 就失败（非管理员账户 / .app 在只读位置）或提权密码框被取消 ⇒ $BAK 从未产生
        // ②③ 落进成功分支的后果是：删掉已解压好的新版与刚下载的 DMG、把**旧版**重新拉起来，
        // 而调用方在 spawn 后立即 `app.exit(0)` 并向前端回 success ⇒ 用户看到「更新完成、应用重启」，
        // 版本号却没变，且手动拖拽的退路（else 分支的 `open "$DMG"`）被绕过、安装包已被删。
        //
        // `$STAGE` 没有这个歧义：成功时它被 `mv` 移走，**任何**失败腿上它都还在
        //（第一步失败没动过它；第二步失败后回滚也没动过它；提权腿的 mv 失败同理）。
        "if [ -d \"$DEST\" ] && [ ! -d \"$STAGE\" ]; then",
        // ── ad-hoc 签名必需的两步（顺序不可换）──
        // ① 清 quarantine：不清则 Gatekeeper 拦「身份不明的开发者」→ 装完打不开。
        "  xattr -dr com.apple.quarantine \"$DEST\" 2>/dev/null",
        // ② 校验签名；ad-hoc 签名在替换过程中可能被破坏 → 就地重签（`-s -` = ad-hoc，无需任何证书）。
        "  if ! codesign --verify --deep --strict \"$DEST\" >/dev/null 2>&1; then",
        "    codesign --force --deep --sign - \"$DEST\" >/dev/null 2>&1",
        "  fi",
        "  rm -rf \"$STAGE\" 2>/dev/null",
        "  rm -f \"$DMG\"",
        "  open \"$DEST\"",
        // ③ 兜底：`open` 若仍被 Gatekeeper 拦，把 .app 所在目录亮出来，用户可「右键 → 打开」放行。
        "  sleep 3",
        "  pgrep -f \"$DEST\" >/dev/null 2>&1 || open -R \"$DEST\"",
        "else",
        "  open \"$DMG\"",
        "fi",
        "",
    ]
    .join("\n")
}

/// Linux AppImage 原位覆盖脚本（移植 `buildLinuxAppImageScript`）。
fn build_linux_appimage_script(plan: &InstallPlan) -> String {
    let src = sh_quote(&plan.installer_path.to_string_lossy());
    let dst = sh_quote(
        &plan
            .appimage_target
            .as_ref()
            .unwrap_or(&plan.exe_path)
            .to_string_lossy(),
    );
    [
        "#!/bin/bash",
        "sleep 2",
        &format!("NEW={src}"),
        &format!("DEST={dst}"),
        // 只覆盖 AppImage 这一个文件；`~/.config/polaris` 不动 → 配置 + 已更新内核零丢失。
        "if cp -f \"$NEW\" \"$DEST\" 2>/dev/null; then",
        "  chmod +x \"$DEST\" 2>/dev/null",
        "  rm -f \"$NEW\" 2>/dev/null",
        "  nohup \"$DEST\" >/dev/null 2>&1 &",
        "else",
        // 覆盖失败（原 AppImage 只读/无权限）→ 跑临时新版，至少本次拿到新版（不删临时件）。
        "  chmod +x \"$NEW\" 2>/dev/null",
        "  nohup \"$NEW\" >/dev/null 2>&1 &",
        "fi",
        "",
    ]
    .join("\n")
}

/// Linux deb 原位升级脚本（移植 `buildLinuxDebScript`）。
fn build_linux_deb_script(plan: &InstallPlan) -> String {
    let deb = sh_quote(&plan.installer_path.to_string_lossy());
    let exe = sh_quote(&plan.exe_path.to_string_lossy());
    [
        "#!/bin/bash",
        "sleep 2",
        &format!("DEB={deb}"),
        &format!("EXE={exe}"),
        // apt-get install 本地 deb（apt 1.1+ 支持绝对路径）：解依赖 + 同包名版本升级。
        // Ubuntu 24.04+ 默认 .deb 处理器是 App Center，对本地 deb 只按包名判「已安装」、不比较版本
        // → `xdg-open` 是死路，故必须走 apt。
        "if pkexec apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold \"$DEB\"; then",
        "  rm -f \"$DEB\" 2>/dev/null",
        "  nohup \"$EXE\" >/dev/null 2>&1 &",
        "else",
        // 用户取消授权 / apt 失败 → 打开下载目录让用户手动（**不**回退到 App Center 死路）。
        "  xdg-open \"$(dirname \"$DEB\")\" >/dev/null 2>&1 &",
        "fi",
        "",
    ]
    .join("\n")
}

// ── 唯一的执行腿（**真机门**：本机绝不调用）────────────────────────────────

/// 写脚本到临时目录并 `spawn(detached)`（**执行腿**）。
///
/// 调用方必须**先**停代理（Windows 文件占用会让替换失败），**后**退出应用。
///
/// # Errors
///
/// 写脚本 / spawn 失败。
pub fn spawn_detached_script(dir: &Path, spec: &ScriptSpec) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建脚本目录失败 {}: {e}", dir.display()))?;
    let path = dir.join(&spec.file_name);
    std::fs::write(&path, &spec.bytes)
        .map_err(|e| format!("写安装脚本失败 {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.leading_args).arg(&path);
    // detached：父进程随即 exit(0)，脚本必须活下去。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // setsid：脱离本进程会话，父退出不带走脚本。
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()
        .map_err(|e| format!("启动安装脚本失败 {}: {e}", spec.program))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // ── 资产形态分类 ──

    #[test]
    fn classify_installer_covers_all_four_shapes_case_insensitively() {
        assert_eq!(
            classify_installer("Polaris-Setup-1.0.exe"),
            Some(InstallerKind::WinExe)
        );
        assert_eq!(
            classify_installer("Polaris-1.0.dmg"),
            Some(InstallerKind::Dmg)
        );
        assert_eq!(
            classify_installer("Polaris-1.0.AppImage"),
            Some(InstallerKind::AppImage)
        );
        assert_eq!(
            classify_installer("polaris-1.0.appimage"),
            Some(InstallerKind::AppImage)
        );
        assert_eq!(
            classify_installer("polaris_1.0_amd64.deb"),
            Some(InstallerKind::Deb)
        );
        // 认不出的一律 None（**不猜**：猜错就是拿错脚本去改宿主应用本体）。
        assert_eq!(classify_installer("polaris-1.0.tar.gz"), None);
        assert_eq!(classify_installer("README"), None);
    }

    // ── 运行形态判定 ──

    #[test]
    fn detect_run_form_truth_table() {
        assert_eq!(
            detect_run_form("linux", Some(Path::new("/a.AppImage")), None),
            RunForm::Loose
        );
        assert_eq!(detect_run_form("linux", None, None), RunForm::Installed);
        assert_eq!(
            detect_run_form("windows", None, Some(Path::new("C:\\p.exe"))),
            RunForm::Loose
        );
        // 无便携标记（`portable.marker` 不在 exe 同级，或被用户删了）→ Installed（保守：推安装器）。
        assert_eq!(detect_run_form("windows", None, None), RunForm::Installed);
        assert_eq!(detect_run_form("macos", None, None), RunForm::Loose);
    }

    // ── .app 包路径推导 ──

    #[test]
    fn mac_app_bundle_from_exe_matches_only_real_bundle_layout() {
        assert_eq!(
            mac_app_bundle_from_exe(Path::new(
                "/Applications/Polaris.app/Contents/MacOS/polaris"
            )),
            Some(p("/Applications/Polaris.app"))
        );
        // 非 bundle 布局 → None（**不瞎猜**，回退 open DMG 手动拖拽）。
        assert_eq!(
            mac_app_bundle_from_exe(Path::new("/usr/local/bin/polaris")),
            None
        );
        assert_eq!(
            mac_app_bundle_from_exe(Path::new("/A/Polaris.app/Contents/MacOS/")),
            None
        );
        // 尾段含 `/`（多层）→ 不匹配。
        assert_eq!(
            mac_app_bundle_from_exe(Path::new("/A/Polaris.app/Contents/MacOS/sub/polaris")),
            None
        );
    }

    // ── 安装计划真值表（含跨形态错配逃逸用例）──

    #[test]
    fn plan_windows_portable_vs_setup() {
        // ⚠️ 本测试跑在 Linux gate 上，而 `Path::parent` 的分隔符语义是**编译目标平台**的
        // （Linux 上 `C:\App\x.exe` 是单个组件，parent 为空）。故这里用 `/` 分隔——Windows API
        // 同样接受正斜杠，且这样断言的是「新包落在原 exe 同目录」这条真正的业务规则，
        // 而不是宿主平台的分隔符解析。反斜杠路径的真实拆分属真机门（§8.3）。
        let plan = decide_install_plan(
            "windows",
            RunForm::Loose,
            Path::new("C:/Temp/Polaris-1.2-win-portable.exe"),
            Path::new("C:/App/Polaris-1.1-win-portable.exe"),
            None,
            Some(Path::new("C:/App/Polaris-1.1-win-portable.exe")),
        )
        .unwrap();
        assert_eq!(plan.platform, InstallPlatform::WindowsPortable);
        assert_eq!(
            plan.portable_target,
            Some(p("C:/App/Polaris-1.1-win-portable.exe"))
        );
        // 新版本名文件落在**原目录**，保留 release 的带版本号命名。
        assert_eq!(
            plan.portable_new_path,
            Some(p("C:/App/Polaris-1.2-win-portable.exe"))
        );

        let plan = decide_install_plan(
            "windows",
            RunForm::Installed,
            Path::new("C:/Temp/Polaris-1.2-win-setup.exe"),
            Path::new("C:/Program Files/Polaris/polaris.exe"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.platform, InstallPlatform::WindowsSetup);
        assert!(plan.portable_target.is_none());
    }

    #[test]
    fn plan_macos_carries_bundle_or_falls_back() {
        let plan = decide_install_plan(
            "macos",
            RunForm::Loose,
            Path::new("/tmp/Polaris-1.2-mac-arm64.dmg"),
            Path::new("/Applications/Polaris.app/Contents/MacOS/polaris"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.platform, InstallPlatform::Macos);
        assert_eq!(plan.app_bundle_path, Some(p("/Applications/Polaris.app")));

        // 定位不到 bundle → 计划仍成立，但 app_bundle_path=None（脚本走 open DMG 手动拖拽）。
        let plan = decide_install_plan(
            "macos",
            RunForm::Loose,
            Path::new("/tmp/x.dmg"),
            Path::new("/usr/local/bin/polaris"),
            None,
            None,
        )
        .unwrap();
        assert!(plan.app_bundle_path.is_none());
    }

    #[test]
    fn plan_linux_appimage_requires_appimage_env() {
        let plan = decide_install_plan(
            "linux",
            RunForm::Loose,
            Path::new("/tmp/Polaris-1.2.AppImage"),
            Path::new("/tmp/.mount_x/polaris"),
            Some(Path::new("/home/u/Apps/Polaris.AppImage")),
            None,
        )
        .unwrap();
        assert_eq!(plan.platform, InstallPlatform::LinuxAppImage);
        assert_eq!(
            plan.appimage_target,
            Some(p("/home/u/Apps/Polaris.AppImage"))
        );

        // **逃逸用例**：loose 形态但 $APPIMAGE 缺失 → 无覆盖目标，必须拒绝（否则会覆盖到 exe_path，
        // 而 AppImage 运行时的 exe_path 在 /tmp/.mount_* 只读挂载里 —— 覆盖它毫无意义且必失败）。
        let r = decide_install_plan(
            "linux",
            RunForm::Loose,
            Path::new("/tmp/Polaris-1.2.AppImage"),
            Path::new("/tmp/.mount_x/polaris"),
            None,
            None,
        );
        assert!(matches!(r, Err(InstallReject::FormMismatch { .. })));
    }

    #[test]
    fn plan_rejects_cross_form_mismatch_and_never_escalates_to_root() {
        // **最要紧的逃逸用例**（§8.1 点名）：AppImage 运行形态 + .deb 资产。
        // 若这里放行，就会在 AppImage 用户机器上 `pkexec apt-get install` —— 提权装出第二份。
        let r = decide_install_plan(
            "linux",
            RunForm::Loose,
            Path::new("/tmp/polaris_1.2_amd64.deb"),
            Path::new("/tmp/.mount_x/polaris"),
            Some(Path::new("/home/u/Polaris.AppImage")),
            None,
        );
        match r {
            Err(InstallReject::FormMismatch {
                ref installer,
                ref form,
                ..
            }) => {
                assert_eq!(installer, "polaris_1.2_amd64.deb");
                assert_eq!(*form, RunForm::Loose);
            }
            other => panic!("AppImage 形态拿到 .deb 必须拒绝，实得: {other:?}"),
        }

        // 反向：deb 安装态 + AppImage 资产 → 同样拒绝。
        assert!(matches!(
            decide_install_plan(
                "linux",
                RunForm::Installed,
                Path::new("/tmp/Polaris.AppImage"),
                Path::new("/usr/bin/polaris"),
                None,
                None,
            ),
            Err(InstallReject::FormMismatch { .. })
        ));

        // 跨 OS 错配：Linux 上拿到 .dmg / .exe → 拒绝。
        for name in ["/tmp/x.dmg", "/tmp/x.exe"] {
            assert!(
                matches!(
                    decide_install_plan(
                        "linux",
                        RunForm::Installed,
                        Path::new(name),
                        Path::new("/usr/bin/polaris"),
                        None,
                        None
                    ),
                    Err(InstallReject::FormMismatch { .. })
                ),
                "{name} 在 Linux 上必须被拒"
            );
        }
        // macOS 上拿到 .deb → 拒绝。
        assert!(matches!(
            decide_install_plan(
                "macos",
                RunForm::Loose,
                Path::new("/tmp/x.deb"),
                Path::new("/A/P.app/Contents/MacOS/p"),
                None,
                None
            ),
            Err(InstallReject::FormMismatch { .. })
        ));
    }

    #[test]
    fn plan_rejects_unknown_asset() {
        assert!(matches!(
            decide_install_plan(
                "linux",
                RunForm::Installed,
                Path::new("/tmp/x.tar.gz"),
                Path::new("/usr/bin/p"),
                None,
                None
            ),
            Err(InstallReject::UnknownAsset { .. })
        ));
    }

    // ── 安装前告知（ad-hoc 签名 / 提权）──

    fn plan_of(platform: InstallPlatform) -> InstallPlan {
        InstallPlan {
            platform,
            installer_path: p("/tmp/x"),
            exe_path: p("/usr/bin/polaris"),
            portable_target: None,
            portable_new_path: None,
            app_bundle_path: Some(p("/Applications/Polaris.app")),
            appimage_target: Some(p("/home/u/P.AppImage")),
        }
    }

    #[test]
    fn advisory_is_required_wherever_os_will_block_or_prompt() {
        // 用户拍板走 ad-hoc 签名 ⇒ mac/win 都会被 OS 拦一道，**必须**提前告知可执行的下一步。
        assert_eq!(
            install_advisory(&plan_of(InstallPlatform::Macos)),
            Some(InstallAdvisory::MacosGatekeeper)
        );
        assert_eq!(
            install_advisory(&plan_of(InstallPlatform::WindowsSetup)),
            Some(InstallAdvisory::WindowsSmartScreen)
        );
        assert_eq!(
            install_advisory(&plan_of(InstallPlatform::WindowsPortable)),
            Some(InstallAdvisory::WindowsSmartScreen)
        );
        // deb：polkit 提权框（= 上游 confirmDebElevation），必须在停代理**之前**确认。
        assert_eq!(
            install_advisory(&plan_of(InstallPlatform::LinuxDeb)),
            Some(InstallAdvisory::DebElevation)
        );
        // AppImage：无签名校验、无提权 → 唯一无需告知的路径。
        assert_eq!(
            install_advisory(&plan_of(InstallPlatform::LinuxAppImage)),
            None
        );
    }

    #[test]
    fn advisory_keys_are_distinct_and_stable() {
        // key 是前端 i18n 的契约面；三者必须互不相同（撞 key = 弹错说明）。
        let keys = [
            InstallAdvisory::DebElevation.key(),
            InstallAdvisory::WindowsSmartScreen.key(),
            InstallAdvisory::MacosGatekeeper.key(),
        ];
        let mut sorted = keys.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "advisory key 必须互不相同");
    }

    // ── 脚本生成 ──

    #[test]
    fn windows_vbs_is_utf16le_with_bom() {
        // **变异防线**：若 utf16le_with_bom 退化成 `s.into_bytes()`（UTF-8），中文用户名路径会被
        // wscript 按系统代码页解释 → 找不到文件 → 更新静默失败。
        let plan = InstallPlan {
            portable_target: Some(p("C:\\用户\\Polaris.exe")),
            portable_new_path: Some(p("C:\\用户\\Polaris-1.2.exe")),
            ..plan_of(InstallPlatform::WindowsPortable)
        };
        let spec = build_install_script(&plan, &InstallTexts::default());
        assert_eq!(
            &spec.bytes[..2],
            &[0xFF, 0xFE],
            "VBS 必须以 UTF-16LE BOM 开头"
        );
        assert_eq!(
            spec.program, "wscript.exe",
            "必须用 wscript（无窗口），非 cscript"
        );
        // UTF-16LE：ASCII 字符后必跟 0x00。
        assert_eq!(spec.bytes[2], b'W');
        assert_eq!(spec.bytes[3], 0x00);
        // 解回文本验证内容。
        let units: Vec<u16> = spec.bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16(&units).unwrap();
        assert!(
            text.contains("C:\\\\用户\\\\Polaris.exe"),
            "路径反斜杠须双写"
        );
        assert!(text.contains("\r\n"), "VBS 行分隔符须是 CRLF");
        assert!(
            text.contains("MsgBox"),
            "覆盖失败必须提示用户手动替换，不得静默"
        );
    }

    #[test]
    fn windows_setup_script_passes_update_flag() {
        let spec = build_install_script(
            &plan_of(InstallPlatform::WindowsSetup),
            &InstallTexts::default(),
        );
        let units: Vec<u16> = spec.bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16(&units).unwrap();
        // `/UPDATE` 不可省：Tauri NSIS 模板的 `$UpdateMode` 靠它，缺了会让应用内更新静默降级成
        // 全新安装模式（弹卸载/重装选择页 + 重跑 WebView2 安装 + 卸载器侧清理闸失效）。
        assert!(text.contains("/UPDATE"), "NSIS setup 必须带 /UPDATE");
        // 同时钉死**不得**再出现 electron-builder 的 `--updated`：这条门此前锁的正是那个错 flag，
        // 于是「传了个 Tauri 根本不认的参数」看起来像验过了。反向断言让退回旧写法直接转红。
        assert!(
            !text.contains("--updated"),
            "`--updated` 是 electron-builder 的约定，Tauri 不认（会静默降级成全新安装）"
        );
    }

    #[test]
    fn mac_script_must_clear_quarantine_and_resign_adhoc() {
        // **变异验证（用户点名）**：删掉 quarantine 清除步骤 → 本测试必须转红。
        // ad-hoc 签名下不清 quarantine = 用户点了更新、装完打不开（最差体验）。
        let spec = build_install_script(&plan_of(InstallPlatform::Macos), &InstallTexts::default());
        let s = String::from_utf8(spec.bytes).unwrap();
        assert!(
            s.contains("xattr -dr com.apple.quarantine \"$DEST\""),
            "ad-hoc 签名下必须清 quarantine，否则 Gatekeeper 拦「身份不明的开发者」"
        );
        assert!(
            s.contains("codesign --force --deep --sign - \"$DEST\""),
            "签名校验不过时必须 ad-hoc 重签（`-s -` 无需任何证书）"
        );
        // 清 quarantine 必须在 `open` **之前**（顺序颠倒 = 先被拦一次）。
        let q = s.find("xattr -dr com.apple.quarantine").unwrap();
        let o = s.find("open \"$DEST\"").unwrap();
        assert!(q < o, "清 quarantine 必须早于启动新版");
        // 兜底指引：若仍起不来，把 .app 亮给用户（可右键→打开放行）。
        assert!(
            s.contains("open -R \"$DEST\""),
            "启动失败须给出用户可执行的下一步"
        );
        // 原子性：mv-swap，绝不先 rm 目标。
        assert!(
            !s.contains("rm -rf \"$DEST\""),
            "绝不先毁目标再建（brick 风险）"
        );
        assert!(s.contains("hdiutil attach"), "须挂载 DMG");
        assert!(s.contains("hdiutil detach"), "须卸载 DMG（否则残留挂载点）");
    }

    /// 成功判据必须落在 `$STAGE`（被移走 = 真替换过），不得落在 `$BAK` 不在场上。
    ///
    /// `[ ! -d "$BAK" ]` 是**三种状态共有**的：真成功、第二步 mv 失败后已回滚、第一步 mv 就失败
    /// （含提权密码框被取消，$BAK 从未产生）。后两种落进成功分支 = 删掉新版与 DMG、把旧版拉起来，
    /// 而调用方已经向前端回了 success。
    #[test]
    fn mac_script_success_branch_keys_on_stage_not_bak() {
        let spec = build_install_script(&plan_of(InstallPlatform::Macos), &InstallTexts::default());
        let s = String::from_utf8(spec.bytes).unwrap();

        assert!(
            s.contains(r#"if [ -d "$DEST" ] && [ ! -d "$STAGE" ]; then"#),
            "成功判据没落在 $STAGE 上：\n{s}"
        );
        assert!(
            !s.contains(r#"[ ! -d "$BAK" ]"#),
            "还在用 $BAK 不在场当成功判据 —— 失败已回滚与提权被取消都满足它"
        );

        // 破坏性收尾必须落在**成功分支体内**。
        // 不做全局位置比较：脚本更早还有一条合法的早退腿（找不到 `.app` 时 `rm -f "$DMG"; exit 0`），
        // 全局 `find` 会撞上它 —— 实测本门第一版就是这么误红的。
        let cond = s.find(r#"[ ! -d "$STAGE" ]"#).expect("上一条已断言存在");
        let branch_end = s[cond..].find("\nelse\n").map_or(s.len(), |i| cond + i);
        let branch = &s[cond..branch_end];
        for destructive in [r#"rm -f "$DMG""#, r#"open "$DEST""#, r#"rm -rf "$STAGE""#] {
            assert!(
                branch.contains(destructive),
                "`{destructive}` 不在成功分支体内（跑到判据之外 = 失败时也会执行）：\n{branch}"
            );
        }

        // 失败侧的退路必须还在：重新打开 DMG 让用户手动拖拽。
        // **判据必须落在 else 分支体内**：脚本更早还有两处 `open "$DMG"`（挂载失败早退、
        // 定位不到 bundle 的回退），裸 `contains` 会被它们喂饱 —— 实测本门第一版就是这样，
        // 把 else 里那行删掉照样绿。
        let else_at = s[cond..]
            .find("\nelse\n")
            .map(|i| cond + i)
            .expect("成功分支没有 else —— 失败时无退路");
        let else_end = s[else_at..].find("\nfi").map_or(s.len(), |i| else_at + i);
        let else_body = &s[else_at..else_end];
        assert!(
            else_body.contains(r#"open "$DMG""#),
            "else 分支的手动拖拽退路没了 —— 失败时用户无路可走：\n{else_body}"
        );

        // 自检：$STAGE 确实是被 mv 走的那个（否则上面的判据在语义上不成立）。
        assert!(
            s.contains(r#"mv "$STAGE" "$DEST""#),
            "$STAGE 不再是被移走的对象，本门的整个前提失效"
        );
    }

    #[test]
    fn mac_script_falls_back_to_open_dmg_without_bundle() {
        let plan = InstallPlan {
            app_bundle_path: None,
            ..plan_of(InstallPlatform::Macos)
        };
        let s =
            String::from_utf8(build_install_script(&plan, &InstallTexts::default()).bytes).unwrap();
        assert!(s.contains("open '/tmp/x'"));
        // 定位不到 bundle 时**绝不**瞎猜路径去 mv。
        assert!(!s.contains("mv "), "定位不到 .app 时不得做任何替换");
    }

    #[test]
    fn linux_scripts_match_form() {
        let s = String::from_utf8(
            build_install_script(
                &plan_of(InstallPlatform::LinuxAppImage),
                &InstallTexts::default(),
            )
            .bytes,
        )
        .unwrap();
        assert!(s.contains("chmod +x \"$DEST\""), "覆盖后必须补执行位");
        assert!(!s.contains("pkexec"), "AppImage 路径绝不提权");

        let s = String::from_utf8(
            build_install_script(
                &plan_of(InstallPlatform::LinuxDeb),
                &InstallTexts::default(),
            )
            .bytes,
        )
        .unwrap();
        assert!(
            s.contains("pkexec apt-get install"),
            "deb 须走 apt 原位升级"
        );
        assert!(
            s.contains("xdg-open"),
            "提权被取消须回退到打开下载目录，不静默"
        );
    }

    #[test]
    fn sh_quote_neutralizes_injection() {
        // 路径里的单引号是命令注入面（脚本以 root 跑 deb 分支）。
        assert_eq!(sh_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(
            sh_quote("/tmp/x';rm -rf /;'"),
            r"'/tmp/x'\'';rm -rf /;'\'''"
        );
        let plan = InstallPlan {
            installer_path: p("/tmp/x';touch /tmp/pwned;'"),
            ..plan_of(InstallPlatform::LinuxDeb)
        };
        let s =
            String::from_utf8(build_install_script(&plan, &InstallTexts::default()).bytes).unwrap();
        assert!(
            !s.contains("DEB='/tmp/x';touch"),
            "单引号必须被转义，不得逃出字面量"
        );
    }

    #[test]
    fn utf16le_with_bom_roundtrips_non_ascii() {
        let bytes = utf16le_with_bom("中文A");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "中文A");
    }

    #[test]
    fn script_generation_is_deterministic() {
        // 快照断言的前提：同一 plan 恒得同一字节（脚本里不得掺时间戳/随机数；$$ 是 shell 运行期取的）。
        let plan = plan_of(InstallPlatform::Macos);
        let a = build_install_script(&plan, &InstallTexts::default());
        let b = build_install_script(&plan, &InstallTexts::default());
        assert_eq!(a, b);
    }
}
