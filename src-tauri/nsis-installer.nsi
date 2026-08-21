# Polaris Windows 安装器配置（B10 发布工程）
#
# ⚠️ 本文件是**纯说明文档**，不含任何 NSIS 代码，也不会被 makensis 读取（扩展名 `.nsi` 属历史遗留，
# 易被误认为是脚本）。真正被注入构建的是 `nsis-hooks.nsh`（经 tauri.conf.json 的
# `bundle.windows.nsis.installerHooks`）。
#
# 本文件说明 Windows 侧 NSIS 与 WebView2 策略（§I-Q2 + §E.2）。Tauri 2 不需要手写完整 .nsi 脚本
# （官方 NSIS 模板已覆盖通用 webview 应用流程），而是通过 tauri.conf.json 的 bundle.windows.nsis
# 字段配置；需要深度定制时再经 installerHooks / template 注入。
#
# === installerHooks（2026-08-05 起启用）===
# `nsis-hooks.nsh` 实现 `NSIS_HOOK_POSTUNINSTALL`：真卸载（非 `/UPDATE`）时提权清理运行期外置的
# `PolarisHelper` 服务与 `C:\ProgramData\Polaris`。那两样不在 NSIS 安装清单里，默认卸载器管不到，
# 不补则控制面板卸载后残留孤儿 LocalSystem 服务。用户数据不在该钩子范围内 —— Tauri 模板自带的
# 「删除应用数据」复选框已覆盖 `%APPDATA%\com.polaris.app` 与 `%LOCALAPPDATA%\com.polaris.app`。
#
# === WebView2（§E.2）===
# CI 只产一个 polaris-{version}-{arch}-setup.exe，使用 tauri.conf.json 的 DownloadBootstrapper。
# Polaris 不内嵌或分发 WebView2 Runtime。普通 Win10/11 通常已预装；精简版 / LTSC 缺失时，
# 安装器需要联网获取微软 Runtime。便携用户需先安装微软官方 Runtime，README 给出官方下载入口。
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
# WebView2 缺失时需安装微软官方 Runtime；Polaris 不提供离线 Runtime 或第二安装器。
#
# 语言：English / 简体中文 / 繁体中文（displayLanguageSelector=true 首装让用户选）。
