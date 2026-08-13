# 排障

## 不签名说明（macOS / Windows）

Polaris **不进行代码签名**（§I-Q1 用户定调，沿 Polaris 现状）：

- **macOS**：首次打开「无法验证开发者」。Gatekeeper 放行 = 右键 App → 「打开」→ 确认；或
  `xattr -dr com.apple.quarantine /Applications/Polaris.app`。更新脚本内置 xattr 清理。
  DMG 内附一份同内容的中英文首次打开引导（`0 首次打开必读 READ ME FIRST.txt`）——
  用户被 Gatekeeper 拦下时进不了应用、也未必看过本文件，那份引导是他当时唯一能看到的说明。
- **Windows**：SmartScreen「Windows 已保护你的电脑」→ 「更多信息」→ 「仍要运行」。UAC 提权流
  （helper 安装 / TUN）照常触发，仅多一次确认。

签名会引入证书成本 + 签名清单信任模型，与现有自定义更新管线（VBS/osascript/pkexec 编排）冲突，
故显式不做（见 §I-Q1 / `updater` crate 说明）。

## 排障：界面白屏 / 花屏 / GPU 进程反复崩溃

多见于 NVIDIA 专有驱动、虚拟机（QEMU 虚拟显卡）、远程桌面（xrdp）等无正常硬件加速的环境 ——
webview 的合成层出不了帧，但**代理内核本身不受影响**（照常运行/转发）。

**界面还能操作**（偶发白屏 / 花屏，重开窗后能进 UI）→ 设置 → 显示 → 「图形兼容」→ 关掉「硬件加速」→
重启 Polaris。（该块在 macOS 不显示：WKWebView 没有受支持的禁 GPU 开关。）

**界面完全打不开**（从头到尾一片空白，点不到设置）→ 下面两条路都**不需要界面配合**：

### ① 直接编辑 config.json

关掉 Polaris，在配置文件顶层加 `"hardwareAcceleration": false`，再启动：

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/com.polaris.app/polaris/config.json` |
| Windows | `%APPDATA%\com.polaris.app\polaris\config.json` |
| macOS | `~/Library/Application Support/com.polaris.app/polaris/config.json` |

```json
{
  "hardwareAcceleration": false
}
```

配置读取刻意做成容错第一：文件损坏 / 字段类型不对一律回落「默认开」，不会因为这个键写坏而启动失败
（判定见 `src-tauri/src/graphics_compat.rs`，建窗前直读原文本，不依赖 store 装配成功）。

### ② 平台环境变量（一次性试跑，不改配置）

这些变量由 WebKitGTK / WebView2 **原生读取，不需要应用配合**；Polaris 也刻意不覆盖你已设的同名变量，
排障时的临时实验不会被应用打断。

```bash
# Linux（WebKitGTK）—— DMABUF 是 NVIDIA 白屏的主修复，COMPOSITING 兜 resize 崩溃
WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 polaris
```

```powershell
# Windows（WebView2）
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--disable-gpu"; .\Polaris.exe
```

macOS 无对应开关（WKWebView 未提供公开 API，WebKit #26651 长期未实现），故 `hardwareAcceleration`
在 mac 上是 no-op（应用会如实记一条 warn 日志，不谎称已生效）。mac 上遇到白屏请提 issue 并附日志：
`~/Library/Application Support/com.polaris.app/polaris/logs/`（Linux 为 `~/.config/...`、Windows 为
`%APPDATA%\...` 下的同名 `logs/` 目录）。

> 两处开关是同一个键：`hardwareAcceleration`（正向语义，默认 `true`=开）。改动均需**整个应用重启**才生效
> —— 环境变量只在 webview 创建那一刻被读取。
