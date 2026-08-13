# Polaris Windows 安装器配置（B10 发布工程）
#
# ⚠️ 本文件是**纯说明文档**，不含任何 NSIS 代码，也不会被 makensis 读取（扩展名 `.nsi` 属历史遗留，
# 易被误认为是脚本）。真正被注入构建的是 `nsis-hooks.nsh`（经 tauri.conf.json 的
# `bundle.windows.nsis.installerHooks`）。
#
# 本文件说明 Windows 侧 NSIS 双产物策略（§I-Q2 + §E.2）。Tauri 2 不需要手写完整 .nsi 脚本
# （官方 NSIS 模板已覆盖通用 webview 应用流程），而是通过 tauri.conf.json 的 bundle.windows.nsis
# 字段配置；需要深度定制时再经 installerHooks / template 注入。
#
# === installerHooks（2026-08-05 起启用）===
# `nsis-hooks.nsh` 实现 `NSIS_HOOK_POSTUNINSTALL`：真卸载（非 `/UPDATE`）时提权清理运行期外置的
# `PolarisHelper` 服务与 `C:\ProgramData\Polaris`。那两样不在 NSIS 安装清单里，默认卸载器管不到，
# 不补则控制面板卸载后残留孤儿 LocalSystem 服务。用户数据不在该钩子范围内 —— Tauri 模板自带的
# 「删除应用数据」复选框已覆盖 `%APPDATA%\com.polaris.app` 与 `%LOCALAPPDATA%\com.polaris.app`。
#
# === 双产物（§E.2 默认 = 双产物）===
# Tauri 2 的 webviewInstallMode 仅能二选一（一次构建一种），故双产物 = 跑两趟 build，CI 各产一个 setup：
#
#   1. polaris-{version}-{arch}-setup.exe        主产物：DownloadBootstrapper（+0MB，需联网装 WebView2）
#      → tauri.conf.json 默认值。普通用户用（绝大多数 Win10/11 已预装 WebView2，bootstrapper 即 no-op）。
#
#   2. polaris-{version}-{arch}-setup-offline.exe 离线产物：OfflineInstaller（+~127MB，内嵌 WebView2 runtime）
#      → 用 tauri.offline.conf.json 覆盖（tauri build --config tauri.offline.conf.json）。
#        LTSC / 精简系统 / 内网 / 离线用户用。README 显式指引「装不上？用 offline 版」。
#
# 体积权衡（§E.2 表）：embedBootstrapper +1.8MB 不离线 / fixedVersion +180MB 过大 → 均不默认，
# 仅留 bootstrapper（主）+ offlineInstaller（离线）两档。
#
# === 不签名（§I-Q1 用户定调）===
# Windows 无代码签名（沿 上游 现状）。后果保留、不可删：
#   - SmartScreen 「Windows 已保护你的电脑」提示首次运行 → 用户点「更多信息 → 仍要运行」。
#   - UAC 提权流（helper 安装 / TUN）照常触发，未经 Authenticode 签名只多一次确认。
#   - updater 自定义安装脚本（§B.5 updater crate）不依赖签名清单信任模型（故不用 tauri-plugin-updater）。
# signingIdentity=null / certificateThumbprint=null / digestAlgorithm=sha256（仅 hashing，不签名）已配。
#
# === installMode: currentUser ===
# per-user 安装到 %LOCALAPPDATA%\Programs\Polaris（不需管理员装、不污染 Program Files）。
# helper 与 TUN 提权仍走运行期 UAC 弹窗（独立于安装动作），与 上游 一致。
#
# === portable 形态（§I-Q4）===
# 上游 支持 portable exe + 专属更新逻辑（§C #40/#33）。Tauri 2 的 NSIS 无原生 portable 产物，
# 由 CI 单独产一个 zip（解压即用的目录形态，绕过安装器，便携盘/U盘场景）。portable 启动检测
# WebView2 缺失时用原生弹窗指引 offline installer（§E.2 附加护栏，对应 上游 早期崩溃路径）。
#
# 语言：English / 简体中文 / 繁体中文（displayLanguageSelector=true 首装让用户选）。
