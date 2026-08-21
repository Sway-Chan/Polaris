# 构建与打包

从 README 迁出（2026-08-13）：这些是工程内幕与不变量，读者是维护者，不是使用者。
README 只留「怎么装、怎么用」。

## 构建

实现按系统设计 §H 分批落地（B0 脚手架 → B10 发布工程）。

### 工具链要求

| 工具 | 版本 | 用途 |
|---|---|---|
| Rust | stable（edition 2021） | 后端 + 17 crate |
| Node.js | 24+（CI 钉 26） | 前端构建 + fetch 脚本 |
| pnpm | 9+ | 前端包管理（`ui/`） |
| [Tauri CLI 2](https://v2.tauri.app/) | 2.x | `cargo tauri build` 打包（随 `ui/` devDep 装） |

### 系统依赖

**Linux**（Tauri 2 WebKitGTK 4.1 栈，Debian/Ubuntu）：

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  librsvg2-dev libglib2.0-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
  libdbus-1-dev pkg-config
```

**macOS**：13.0+（Ventura，BTM「允许后台」三级探测需要），Xcode Command Line Tools。
Windows：MSVC build tools（Visual Studio Build Tools 2022 + Windows 10/11 SDK）。

### Rust workspace（开发门禁）

```bash
cargo build --workspace        # 编译
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace         # 单测
cargo fmt --all -- --check     # 格式
```

### 资源拉取（sing-box 核 / cronet / 面板）

`scripts/fetch-{core,cronet,dashboard}.mjs` 从官方 release 按 `src-tauri/core-manifest.json`
的 SHA256 钉扎下载（现拉现打、不入库）。**打包前必须跑**：Tauri bundle `resources` 字段引用
这些产物。缺 pin 即 fail（绝不无校验拉可执行核）。

```bash
node scripts/fetch-core.mjs       # sing-box 四平台核（版本 = core-manifest.json 的 bundledCoreVersion，勿在此重复钉）
node scripts/fetch-cronet.mjs     # libcronet（仅 linux/windows；mac 静态编入核心）
node scripts/fetch-dashboard.mjs  # sing-box 面板（gh-pages 产物）——已挂 beforeBundleCommand 自动跑，手动跑仅为提前预热
```

面板资源另有 `tauri.conf.json` 的 `build.beforeBundleCommand` 兜底：`cargo tauri build` 打包时
自动跑 `fetch-dashboard.mjs`（已存在则跳过，幂等）+ `verify-dashboard-resources.mjs`（断言
`resources/dashboard/index.html` 存在且非 0 字节），忘记手动预热也不会打出空面板的包。
core / cronet 暂无此兜底，仍需按上面手动跑。

升级核心只需改 `src-tauri/core-manifest.json` 的版本号 + 对应 sha256，再加 `--force` 重拉。

### 打包出安装包

```bash
# 1) 拉资产（见上）
# 2) 前端构建 + Rust 编译 + 安装包（Tauri CLI 自动编排 beforeBuildCommand）
#    从**仓库根**跑，并显式传本平台的 config（见下方「按平台筛内核」）
cargo tauri build --config src-tauri/tauri.linux.conf.json          # Linux
cargo tauri build --config src-tauri/tauri.windows.conf.json        # Windows
cargo tauri build --config src-tauri/tauri.macos-arm64.conf.json    # macOS Apple Silicon
cargo tauri build --config src-tauri/tauri.macos-x64.conf.json --target x86_64-apple-darwin  # macOS Intel
```

产物落在**仓库根**的 `target/release/bundle/`（本仓是 cargo workspace，workspace 根在仓库根，
故**不是** `src-tauri/target/`）；传了 `--target <triple>` 时再下沉一层：`target/<triple>/release/bundle/`。

| 平台 | 产物 | 形态 |
|---|---|---|
| Linux | `*.deb` / `*.AppImage` | deb 包 + AppImage（单文件免装） |
| macOS | `*-mac-arm64.dmg` / `*-mac-x64.dmg` | **分架构单出**（不再出 universal，未签名）。⚠️ 这是 **release 资产名**，不是本地产物名 —— 见下 |
| Windows | `*-win-setup.exe` | NSIS 安装器（WebView2 downloadBootstrapper，不内嵌 Runtime） |
| Windows | `polaris-portable-*.zip` | 免安装绿色版（解压即用，自带 `resources/` + `portable.marker` 形态标记） |

portable 由 `package.yml` 在 Windows 腿从 `target/release/polaris.exe` + `resources/` 额外打 zip，
本地单跑上面那条 `cargo tauri build` 不会有。

⚠️ **dmg 那行同理，但成因不同**：`-mac-arm64` / `-mac-x64` 这个 arch tag 不是 Tauri 产出的，
是 `package.yml` 的 `Tag macOS dmg with arch` 步把 `<名>.dmg` 重命名成 `<名>-<tag>.dmg` 加上的
（该步只在 CI 跑）。**本地跑 `cargo tauri build` 拿到的是 Tauri 默认名的 dmg，不带 tag。**
这个 tag 是更新器选包契约的硬要求：`github.rs::find_suitable_update_asset` 按资产名里的
`mac-arm64` / `mac-x64` 选包，匹配不到直接返回 `None`（已取消「任意 .dmg」回落）。

一个 release 里 deb / AppImage / mac-arm64 dmg / mac-x64 dmg / win setup / portable zip
**各恰好一个**（共 6 个平台交付物，另含 `SHA256SUMS`），由
`verify-packaging.mjs assets --label release` 机器守住。
两个 Linux 形态同样是「恰好一个」而非「至少一个」：updater 的 Linux 分支取首个命中
（`github.rs` 的 `app_image.first()` / `deb.first()`），多一个就和 dmg / setup 一样选谁看资产顺序。

#### 按平台筛内核（`--config` 不可省）

`src-tauri/tauri.conf.json` 的 `bundle.resources` **不含任何平台内核目录**，四个平台内核分别由
`tauri.{linux,windows,macos-arm64,macos-x64}.conf.json` 指定。理由与纪律：

- 四平台内核全塞 ⇒ 每个包白背 ~210MB 死重（运行期只按 `env::consts::OS/ARCH` 取一份）。
- 合并按 RFC 7396，**数组整体替换不合并** ⇒ 往 base 加公共资源必须同步到四份，否则四个包全部静默丢失。
- **不吃 Tauri 的「按平台名隐式合并」**：隐式只认固定文件名，文件一改名就静默停止合并，
  包里没有内核、bundler 照常出包，只在用户机器上 `resolve_core_binary → Err` 才暴露。显式
  `--config` 下同样的改名会得到 `failed to read configuration file` 硬失败。
  （macOS 那份原名 `tauri.macos.conf.json` 会被隐式合并，实为 arm64 专用 ⇒ 在 Intel Mac 上裸
  `cargo tauri build` 会打进 arm64 内核；已改名为 `tauri.macos-arm64.conf.json` 消除该隐式默认。）

这些不变量由 `node scripts/verify-packaging.mjs confs` 机器守住（CI 每次打包前跑，本机可直接复现）；
构建后再由 `payload` / `assets` 两个模式断言产物里恰好一份本平台内核、且产物名满足更新器选包契约。

## 持续集成

两个 workflow 分工（`.github/workflows/`）：

- **`ci.yml`** — 快速门禁：三平台 `cargo fmt + clippy + build + test`，每个 PR / push 到 main 触发。
  职责 = 「改动是否正确」。建议把三平台 `cargo-test` 设为 required checks。
- **`package.yml`** — 发布工程：三平台 matrix 跑 fetch + `tauri build` + 产物上传。
  触发 = tag（`v*`）/ 手动 / main 改动打包相关路径。职责 = 「能否产出可分发安装包」。

## Windows 安装器与 WebView2

Tauri 2 依赖 WebView2 Runtime。Windows 只发布一个 **`*-win-setup.exe`**，使用
`tauri.conf.json` 的 `downloadBootstrapper`：普通 Win10/11 通常已预装，缺失时安装器联网获取微软
Runtime。Polaris 不内嵌、不镜像 WebView2 Runtime，也不维护第二套 Windows 安装包。

精简版 / LTSC 或便携版用户若缺少 Runtime，需要先从微软官方下载并安装
[Microsoft Edge WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。设备若完全
离线，也无法下载 Polaris 或获取订阅，因此发行链不再为该场景维护第二套安装包与校验工作流。

命名不是随意的：更新器（`crates/updater/src/github.rs`）Windows 侧**按运行形态分成两条互不相交的
选包规则**：

| 运行形态 | 选包判据 | 命中 |
|---|---|---|
| 安装态（NSIS 装的） | `.exe` 且名含 `win` | `*-win-setup.exe` |
| 便携（解压 zip 跑的） | `polaris-portable-` 前缀 + `.zip` | `polaris-portable-*.zip` |

故安装器显式带 `win`，便携版是 zip，与 `.exe` 分属不相交的命名空间，两条规则各自无歧义。
这些「恰好一个」由 `verify-packaging.mjs assets` 在 CI 里机器守住。

**便携形态怎么被认出来**：便携 zip 里与 `polaris.exe` 同级有一个 `portable.marker` 文件，
应用启动检查更新时读它（`commands/updater.rs::is_portable_layout`）。不用 `PORTABLE_EXECUTABLE_DIR`
那类环境变量——那是 electron-builder 便携目标（自解压 stub）特有的注入，本仓的便携版是
`Compress-Archive` 打的纯 zip、没有 stub，该变量恒不存在。`package.yml` 打完 zip 会开包核验
标记确实在里面，缺了就让整条腿硬失败。

⚠️ **便携版的更新是「下载 + 交系统」，不是全自动**：便携用户拿到的是 zip，`update_install`
不认识 zip 形态（只认 `.exe/.dmg/.AppImage/.deb`）⇒ 回退用系统打开该 zip，由用户自行解压覆盖。
这是有意的诚实降级：形态正确、且绝不会有 NSIS 在背后装出第二份程序。自动解压替换需引解压依赖，未做。
⚠️ 用户若删掉 `portable.marker`，应用会重新把自己当成安装态、改推安装器——**没有自动手段能守住这点**，
故该文件内写明了「勿删」。
