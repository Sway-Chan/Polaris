#!/bin/sh
# ensure-dashboard.sh — 拉取 + 断言 resources/dashboard/index.html 存在且非空。
#
# ⚠️ **本脚本已不再是构建 hook**（2026-08-05）。打包期那道**断言**已搬进
# `src-tauri/build.rs::assert_bundled_dashboard`，`tauri.conf.json` 的 `beforeBundleCommand`
# 随之删除。搬迁理由（Windows 上 hook 走 `cmd /C`、sh 语法必挂；以及 hook cwd 不确定）
# 见那个函数的文档注释。
#
# 现在它是**手动便利脚本**：一条命令搞定「拉 + 验」，本机开发想更新面板时跑它。
#   sh scripts/ensure-dashboard.sh
# CI 不调它（有独立的 `Fetch sing-box dashboard` 步 + build.rs 的断言两道）。
#
# 保留 `cd` 自定位：作为手动脚本，从任何目录调用都应成立。
#
# ⚠️ 历史教训留档（别再赌 hook 的 cwd）：2026-07-20 实测结论「beforeBuildCommand → src-tauri/ ；
#   beforeBundleCommand → 仓库根」在 CLI 2.11.4 上已不成立，2026-08-05 首次真跑 linux 打包腿即挂：
#     Running beforeBundleCommand `sh scripts/ensure-dashboard.sh`
#     sh: 0: cannot open scripts/ensure-dashboard.sh: No such file
#   而它挂在整条 Rust 编译之后，每验证一轮先烧 15~20 分钟。搬进 build script 后该不确定性消失
#   （`CARGO_MANIFEST_DIR` 是编译期常量）。
#
# ⚠️ **订正一条流传过的错误前提**（2026-08-05）：本文件此前写着「打包机 mac 5.238 没装 Node」。
# 那是错的 —— 见 polaris-mac-deploy-recipe.md「5.238 工具链状态（2026-07-20 订正）」：node / cmake /
# brew **全都装在 `/opt/homebrew/bin`**，只是该目录不在 ssh 会话（含 `bash -lc` 登录 shell）的 PATH 里。
# 把这个现象误读成「没装」曾造成两轮部署失败 + 一次白编译。判断远端工具在不在，必须用绝对路径实证
# （`ls -la /opt/homebrew/bin/<tool>`），非交互 ssh 下的 `command -v` 会假阴性。
#
# 所以下面这套「有 node 就拉、没 node 只断言」的分支**留着仍然有价值，但理由变了**：不是「那台机器
# 没有 node」，而是「**能不能在 PATH 上拿到 node** 取决于调用环境」—— 远端非交互 ssh 会话拿不到，
# 本机开发 shell 拿得到。按能力探测（`command -v`）而不是按机器身份写死，两种环境都成立。
#
# 设计：**fetch 可选，断言必做**。
#   - PATH 上有 node（开发机 / CI）→ 顺手拉一次，省得人工记。
#   - PATH 上没有                  → 跳过拉取，但仍用纯 shell 断言资源已就位（由 Linux 侧预构建 + rsync 提供）。
# 不可放弃的保证是**断言**：空的 dashboard 目录绝不能混进安装包——Tauri bundler 照样会把空目录打进去，
# 核只能回落联网下载（离线不可用；CWD 只读时还会刷 mkdir 报错）。fetch 在哪台机器做无所谓，
# 断言必须在**真正出包的那台**跑 —— 这一条现在由 `src-tauri/build.rs::assert_bundled_dashboard`
# 保证（release-only、零 shell 依赖、cwd 由 cargo 固定），本脚本的断言退化为「手动跑时顺带告诉你结果」。
set -e

# 自定位到仓库根（`$0` = 本脚本路径，其父目录的父目录即仓库根），使脚本对调用方 cwd 免疫。
cd "$(dirname "$0")/.." || {
  echo "FAILED: 无法定位仓库根（\$0=$0）" >&2
  exit 1
}

DASH_INDEX="resources/dashboard/index.html"

if command -v node >/dev/null 2>&1; then
  node scripts/fetch-dashboard.mjs
else
  echo "ensure-dashboard: 未检测到 node，跳过自动拉取，仅做资源断言。"
fi

if [ ! -s "$DASH_INDEX" ]; then
  echo "FAILED: $DASH_INDEX 缺失或为 0 字节 —— 面板资源未就位，拒绝打包。" >&2
  echo "  有 Node 的机器：node scripts/fetch-dashboard.mjs" >&2
  echo "  无 Node 的打包机：先在有 Node 的机器跑上面那条，再把 resources/dashboard/ rsync 过来。" >&2
  exit 1
fi

echo "ok: $DASH_INDEX 就位（$(wc -c < "$DASH_INDEX") 字节）。"
