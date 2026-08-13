#!/usr/bin/env node
/**
 * verify-dashboard-resources.mjs — 打包前硬断言：resources/dashboard/index.html 存在且非 0 字节。
 *
 * 为什么需要：曾经出现过 `resources/dashboard/` 目录存在（.gitkeep）但 fetch-dashboard.mjs 未被
 * 实际执行（本地手跑打包漏了这步 / CI 步骤被跳过）的情况——目录非空但内容是空，Tauri bundler
 * 照样把空目录打进安装包，核只能回落联网下载（离线不可用 + CWD 只读时 mkdir 报错）。仅凭
 * fetch-dashboard.mjs 自身的「解压产物含 index.html 才 rename」校验不够：它只保证脚本**跑了**
 * 就不会产出空目录，但不保证脚本**被跑过**。此脚本在打包链的另一端（bundle 前）独立复核最终
 * 落盘状态，两端夹逼防止空目录混进安装包。
 *
 * 用法：node scripts/verify-dashboard-resources.mjs
 * 挂载点：src-tauri/tauri.conf.json 的 `build.beforeBundleCommand`（fetch-dashboard.mjs 之后紧跟执行，
 * `cargo tauri build` / `tauri-action` 都会自动触发，不再依赖人工记得先手跑 fetch 脚本）。
 *
 * ⚠️ 路径陷阱（2026-07-20 实测踩过）：同一份 tauri.conf.json 里两个 hook 的 **cwd 不同**——
 * `beforeBuildCommand` 的 cwd 是 `src-tauri/`（所以那行写 `../ui` 是对的），而
 * `beforeBundleCommand` 的 cwd 是 **Cargo workspace 根**（本仓即仓库根，根 Cargo.toml 有
 * `[workspace] members=["src-tauri","crates/*"]`）。故本 hook 必须写 `scripts/...` 而**不是**
 * `../scripts/...`——按前者惯例写 `../` 会解析到仓库外，`MODULE_NOT_FOUND` 让整个 bundle 阶段
 * exit 1（响亮失败，不会静默产出空目录，但会白跑一次编译）。改这行前先确认 cwd 假设。
 * 本脚本与 fetch-dashboard.mjs 自身用 `import.meta.url` 定位仓库根，**不受 cwd 影响**，只有
 * conf 里的调用路径受影响。
 */
import { existsSync, statSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const INDEX_HTML = join(ROOT, 'resources', 'dashboard', 'index.html');

if (!existsSync(INDEX_HTML)) {
  console.error(
    `FAILED: resources/dashboard/index.html 不存在。打包前先跑 \`node scripts/fetch-dashboard.mjs\`。`
  );
  process.exit(1);
}

const size = statSync(INDEX_HTML).size;
if (size === 0) {
  console.error(
    `FAILED: resources/dashboard/index.html 存在但为 0 字节（空文件不能当面板用）。` +
      `删掉 resources/dashboard/ 后跑 \`node scripts/fetch-dashboard.mjs --force\` 重新拉取。`
  );
  process.exit(1);
}

console.log(`ok: resources/dashboard/index.html 存在且非空（${size} 字节）。`);
