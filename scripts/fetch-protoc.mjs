#!/usr/bin/env node
/**
 * fetch-protoc.mjs — 下载钉扎版本的 protoc 到临时目录，供 CI 装配构建工具链。
 *
 * 用法：node scripts/fetch-protoc.mjs
 * CI 侧：脚本在 `GITHUB_PATH` / `GITHUB_ENV` 存在时自动完成 PATH 注册与 `PROTOC_EXPECT` 导出；
 *        本地跑只下载校验解压，并打印 bin 目录（不碰任何 CI 环境变量）。
 *
 * # 为什么有这个脚本（2026-08-18，CI-4）
 *
 * 此前 protoc 的「固定版本 + 四平台 URL + 四个 sha256」常量在 `ci.yml` 与 `package.yml`
 * **各内联一份**，靠 `workflow-toolchain-pin-parity.test.ts` 对拍防漂。收敛到脚本后常量只有
 * 一处，对拍对象消失，该测试的 protoc 部分退休（NASM 部分仍在——它还是两份）。
 * 与 `fetch-core` / `fetch-cronet` / `fetch-dashboard` 四处同形：**不是引入新概念，
 * 是回到仓里已有的概念**。
 *
 * 相比内联 bash 步还消掉两个平台分支：
 *   · sha256 校验走 node 的 `createHash` —— `sha256sum`（Linux）/ `shasum -a 256`（macOS）
 *     的分支没了，Git Bash 是否自带 `sha256sum` 这个「未经证实」也随之作废；
 *   · 解压走 `lib/extract-zip.mjs` —— 已在三平台 CI 腿绿过的那条路径，且与
 *     「调用它的 workflow step 是 bash 还是 pwsh」彻底解耦（node 用系统 PATH）。
 *
 * # 版本依据（要改版本先读完这段；原注释从 ci.yml 内联步迁来，此处即唯一真值）
 *
 *   · 旧口径 `23.x` 是被替掉的 `arduino/setup-protoc@v3` 的**默认值**，实际解析到 protoc
 *     23.4（2023-07-06 发布）—— 构建工具链版本此前一直是浮动的，不可复现；
 *   · 本仓 protoc 的唯一消费者是 `crates/singbox-grpc/build.rs`（tonic-prost-build 0.14 →
 *     prost-build 0.14.4），它对 protoc 的全部用法就是
 *     `--include_imports --include_source_info -o <fds> -I <dir> <proto>`，都是上古参数；
 *   · vendored `proto/started_service.proto` 是 proto3，无 import、无 proto3 `optional`
 *     （那条才需要 protoc ≥ 3.15）、只用到 oneof ⇒ 版本下限实际上是 3.x 任意一版；
 *   · 本机实测：protoc 3.21.12 / 23.4 / 35.1 对这份 proto 产出的 FileDescriptorSet
 *     **逐字节相同**（sha256 50ce0c7e4757…）。prost-build 的唯一输入就是这份 FDS ⇒
 *     生成的 Rust 代码与旧口径完全一致，升版本零行为差。
 *   故钉 35.1（2026-06-11 发布；36.0 当时还是 rc）。Linux 那份是静态可执行，不依赖
 *   glibc 版本；官方 zip 自带 `bin/` + `include/`，well-known types 的相对 include 保持原样。
 *
 * sha256 来源：本机 curl 下载 GitHub Release 资产后 `sha256sum` 实测（防事后替换与传输
 * 截断，不构成对 GitHub 的独立第三方校验 —— protobuf 官方没有发布校验和文件）。
 */
import { execFileSync } from 'child_process';
import { createHash } from 'crypto';

import { extractZip } from './lib/extract-zip.mjs';
import {
  appendFileSync,
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

const PROTOC_VERSION = '35.1';

/**
 * 平台 → 官方 release 资产名 + zip 的 sha256。键是 `process.platform-process.arch`。
 * 四条之外的平台组合**当场红**，不猜最近邻 —— 换 runner 架构时是加一行判据的事，
 * 不是静默下错架构的事。
 */
const ASSETS = {
  'linux-x64': { asset: 'linux-x86_64', sha256: '6930ebf62bd4ea607b98fff052596c6ee564b9835b4ce172c75a3f53ae9d91b7' },
  'win32-x64': { asset: 'win64', sha256: '5d3ff218d7d91eea95f7569bcb5a98f3030f8996d44151279d9772edcff76082' },
  'darwin-arm64': { asset: 'osx-aarch_64', sha256: '193289af0470c6a1aada357d4fba0bbf8d78bfaac8b5e42ca30af2ef75583de2' },
  'darwin-x64': { asset: 'osx-x86_64', sha256: '537d73604a344ded6fc94e98e07e529d4fe3e4a0b09e59905353950fafc2a1f7' },
};

const key = `${process.platform}-${process.arch}`;
const pinned = ASSETS[key];
if (!pinned) {
  console.error(
    `FAILED: 没有 ${key} 的 protoc 钉扎（ASSETS 表只有 ${Object.keys(ASSETS).join(' / ')}）—— ` +
      '新平台组合请下载对应资产、自算 sha256 后加一行，不要猜最近邻'
  );
  process.exit(1);
}

const url =
  `https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}` +
  `/protoc-${PROTOC_VERSION}-${pinned.asset}.zip`;

// CI 上用 RUNNER_TEMP（随 job 回收），本地用系统 tmp。
const base = process.env.RUNNER_TEMP || tmpdir();

// 落点绝不落进仓库——protoc 是构建工具不是随包产物（与 resources/ 下那批不同），库里多一份
// 只会漂。目录名织入**完整** sha（对齐 fetch-cronet 的指纹思路）：换版本或**同版本重钉 sha**
// 都换目录 ⇒ 必然重新下载重校验。⚠️ 不能只织前 12 位——confirm 轮 M13 实证：改 sha 尾位
// 的 hex 不动前缀 ⇒ 同目录 ⇒ 旧产物 skip 掉新 sha 的验证，重钉保护整个失效。
const dest = join(
  base,
  'polaris-protoc',
  `protoc-${PROTOC_VERSION}-${pinned.asset}-${pinned.sha256}`
);

const sha256 = (file) => createHash('sha256').update(readFileSync(file)).digest('hex');

// 已装配且非 --force 则直接复用（幂等重跑；sha 表换版后 dest 路径天然不同）。
const FORCE = process.argv.includes('--force');
if (!FORCE && existsSync(join(dest, 'bin'))) {
  console.log(`skip (up to date): ${dest}`);
} else {
  const work = mkdtempSync(join(tmpdir(), 'polaris-protoc-dl-'));
  let failed = false;
  try {
    const zip = join(work, 'protoc.zip');
    console.log(`downloading ${url} ...`);
    // 骨架形制照 fetch-cronet，强度参数（--retry 5 / --retry-all-errors / --connect-timeout 20）
    // 承自被替掉的内联步 —— 与两个先例都不逐字同款，差异有意：不带 --retry-delay 5（curl 默认
    // 指数退避，总等待反而更长）；不带 -sS（stdio inherit 下进度条进日志，噪音不是功能差）。
    execFileSync('curl', ['-fL', '--retry', '5', '--retry-all-errors', '--connect-timeout', '20', '-o', zip, url], {
      stdio: 'inherit',
    });
    const got = sha256(zip);
    if (got !== pinned.sha256) {
      throw new Error(`sha256 不符：期望 ${pinned.sha256}，实得 ${got}（版本漂移 / 投毒 / 截断）`);
    }
    console.log(`sha256 ok (${got.slice(0, 12)}…)`);
    rmSync(dest, { recursive: true, force: true });
    mkdirSync(dest, { recursive: true });
    extractZip(zip, dest);
    console.log(`extracted → ${dest}`);
  } catch (e) {
    console.error(`FAILED: ${e.message}`);
    failed = true;
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
  // exit 放在 finally 之后：catch 里直接 process.exit 会跳过 finally、泄漏整个 mkdtemp 工作区
  //（fetch-cronet 的 catch 只记数、循环外统一 exit，就是为这个——照抄骨架时别把这处变形抄丢）。
  if (failed) process.exit(1);
}

const binDir = join(dest, 'bin');
const protocBin =
  process.platform === 'win32' ? join(binDir, 'protoc.exe') : join(binDir, 'protoc');
if (!existsSync(protocBin)) {
  console.error(`FAILED: ${binDir} 里没有 protoc 可执行 —— zip 布局变了？`);
  process.exit(1);
}
if (process.platform !== 'win32') {
  // 对齐被替掉的内联步的 `chmod +x`：zip 理论上带执行位，但「理论上有」不是判据。
  chmodSync(protocBin, 0o755);
}

// CI 装配：GITHUB_PATH/GITHUB_ENV 由 runner 提供；本地跑时两者不存在，走纯打印。
if (process.env.GITHUB_PATH) {
  // node 在 win32 上给出的已是 `C:\…` 形态，正是 GITHUB_PATH 要的；其余平台是 POSIX 路径。
  appendFileSync(process.env.GITHUB_PATH, `${binDir}\n`);
  console.log(`GITHUB_PATH += ${binDir}`);
} else {
  console.log(`bin 目录：${binDir}`);
}
if (process.env.GITHUB_ENV) {
  appendFileSync(process.env.GITHUB_ENV, `PROTOC_EXPECT=libprotoc ${PROTOC_VERSION}\n`);
  console.log(`PROTOC_EXPECT=libprotoc ${PROTOC_VERSION}`);
}
