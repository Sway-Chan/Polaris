/**
 * gen-third-party-licenses.mjs — 生成随产物分发的第三方许可汇总。
 *
 * # 为什么需要
 *
 * `NOTICE` 覆盖的是**以子进程 / 二进制资源形式集成**的组件（sing-box、libcronet、面板、规则数据），
 * 那类属 mere aggregation。但 Tauri / React / 以及几百个 Rust crate 是**链进产物**的源码级依赖，
 * MIT / Apache-2.0 / BSD 这类许可要求随二进制附带许可与版权声明 —— 那部分此前无人登记。
 *
 * # 为什么自己写而不是装 cargo-about
 *
 * `cargo metadata` 与 node 标准库已经够用，装工具会给**每个构建者**多一个前置依赖。
 * 本脚本零第三方依赖。
 *
 * # 按许可文本去重
 *
 * 逐包铺开会得到几 MB 冗余，故按文本哈希分组，每份只出现一次。**去重比例不高是正常的**：
 * MIT 文本内嵌各自的版权行，而那行正是必须保留的归属 —— 565 个包对应 200+ 份互不相同的文本。
 *
 * # 取哪些包
 *
 * Rust：`cargo metadata` 的 resolve 图，从工作区各包出发**只走 normal 依赖**（跳过 dev 与 build）。
 * dev 依赖不进产物，build 依赖只在构建期跑、其产物不被链接。
 * 前端：`pnpm list --prod --depth Infinity` 的生产闭包（devDependencies 不进 Vite 产物）。
 * 不自己走 `node_modules`：pnpm 布局下传递依赖在 `.pnpm/` 里，顶层只有直接依赖的软链。
 *
 * # 用法
 *
 *   node scripts/gen-third-party-licenses.mjs           # 写 THIRD-PARTY-LICENSES.md
 *   node scripts/gen-third-party-licenses.mjs --check    # 只校验已生成的是否最新（CI 用）
 */
import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync, readdirSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(ROOT, 'THIRD-PARTY-LICENSES.md');
const CHECK = process.argv.includes('--check');

/** 目录里像许可文件的那些（大小写与后缀都不统一，故按前缀匹配）。 */
function licenseTexts(dir) {
  if (!dir || !existsSync(dir)) return [];
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((n) => /^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)/i.test(n))
    .filter((n) => {
      try {
        return readFileSync(join(dir, n), 'utf8').length > 0;
      } catch {
        return false;
      }
    })
    .sort()
    // 行尾归一化：部分许可文本是 CRLF，而 git 会在 checkout 时归一成 LF ⇒ 生成结果与仓里
    // 的文件不一致，`--check` 在 CI 上必红（本文件落地时就踩了这一下）。产出必须与平台无关。
    .map((n) => ({
      name: n,
      text: readFileSync(join(dir, n), 'utf8').replace(/\r\n/g, '\n').trim(),
    }));
}

function rustDeps() {
  const meta = JSON.parse(
    execFileSync('cargo', ['metadata', '--format-version', '1', '--all-features'], {
      cwd: ROOT,
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
    }),
  );
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const workspace = new Set(meta.workspace_members);

  // 从工作区各包出发，只走 normal 依赖。dev 不进产物；build 依赖只在构建期跑。
  const seen = new Set();
  const queue = [...workspace];
  while (queue.length) {
    const id = queue.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    const node = nodes.get(id);
    if (!node) continue;
    for (const d of node.deps ?? []) {
      const kinds = (d.dep_kinds ?? []).map((k) => k.kind);
      // kind 为 null = normal。dev/build 一律跳过。
      if (!kinds.some((k) => k === null || k === undefined)) continue;
      queue.push(d.pkg);
    }
  }

  return [...seen]
    .filter((id) => !workspace.has(id))
    .map((id) => byId.get(id))
    .filter(Boolean)
    .map((p) => ({
      ecosystem: 'Rust',
      name: p.name,
      version: p.version,
      license: p.license ?? (p.license_file ? `见 ${p.license_file}` : '未声明'),
      repository: p.repository ?? '',
      texts: licenseTexts(p.manifest_path ? dirname(p.manifest_path) : null),
    }));
}

function jsDeps() {
  // pnpm 布局：传递依赖住在 `node_modules/.pnpm/<name>@<ver>/node_modules/<name>`，顶层
  // `node_modules/<name>` 只有直接依赖的软链。按 `node_modules/<name>` 逐层找会**只捞到直接依赖**
  // （落地时踩过：555 个包里 npm 侧只出了 8 个）。让 pnpm 自己解析生产闭包，路径也由它给。
  let raw;
  try {
    raw = execFileSync('pnpm', ['list', '--prod', '--depth', 'Infinity', '--json'], {
      cwd: join(ROOT, 'ui'),
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    });
  } catch (e) {
    console.error('pnpm list 失败 —— 前端依赖无法枚举，拒绝生成一份缺半边的清单');
    throw e;
  }
  const tree = JSON.parse(raw);
  const found = new Map();
  const walk = (node) => {
    for (const [name, v] of Object.entries(node.dependencies ?? {})) {
      if (found.has(name)) continue;
      found.set(name, v);
      walk(v);
    }
  };
  for (const proj of Array.isArray(tree) ? tree : [tree]) walk(proj);

  return [...found.entries()].map(([name, v]) => {
    const dir = v.path ?? '';
    let p = {};
    const manifest = join(dir, 'package.json');
    if (existsSync(manifest)) {
      try {
        p = JSON.parse(readFileSync(manifest, 'utf8'));
      } catch {
        p = {};
      }
    }
    return {
      ecosystem: 'npm',
      name,
      version: v.version ?? p.version ?? '',
      license: typeof p.license === 'string' ? p.license : (p.license?.type ?? '未声明'),
      repository: typeof p.repository === 'string' ? p.repository : (p.repository?.url ?? ''),
      texts: licenseTexts(dir),
    };
  });
}

function render(pkgs) {
  const sorted = pkgs.sort((a, b) =>
    a.ecosystem === b.ecosystem ? a.name.localeCompare(b.name) : a.ecosystem.localeCompare(b.ecosystem),
  );

  // 按「许可文本」分组：绝大多数包共用逐字相同的 MIT / Apache 文本。
  const groups = new Map();
  const noText = [];
  for (const p of sorted) {
    if (!p.texts.length) {
      noText.push(p);
      continue;
    }
    const key = createHash('sha256')
      .update(p.texts.map((t) => t.text).join('\n---\n'))
      .digest('hex');
    if (!groups.has(key)) groups.set(key, { texts: p.texts, pkgs: [] });
    groups.get(key).pkgs.push(p);
  }

  const lines = [
    '# 第三方许可',
    '',
    '本文件登记**链进 Polaris 产物**的源码级依赖及其许可，随二进制一同分发。',
    '',
    '以子进程 / 二进制资源形式集成的组件（sing-box、libcronet、面板 UI、规则数据）属 mere aggregation，',
    '登记在 `NOTICE`，不在此列。',
    '',
    `本文件由 \`scripts/gen-third-party-licenses.mjs\` 生成，请勿手改。共 ${sorted.length} 个包，`,
    `${groups.size} 份互不相同的许可文本（多数包共用逐字相同的文本，故按文本分组，每份只出现一次）。`,
    '',
    '## 清单',
    '',
    '| 包 | 版本 | 生态 | 许可 |',
    '|---|---|---|---|',
    ...sorted.map((p) => `| ${p.name} | ${p.version} | ${p.ecosystem} | ${p.license} |`),
    '',
  ];

  if (noText.length) {
    lines.push(
      '### 未随包附带许可文本',
      '',
      '以下包的源码目录里没有 LICENSE / COPYING 文件，许可以其清单声明（上表「许可」列）为准：',
      '',
      ...noText.map((p) => `- ${p.name} ${p.version} — ${p.license}`),
      '',
    );
  }

  lines.push('## 许可文本', '');
  let i = 0;
  for (const g of groups.values()) {
    i += 1;
    lines.push(
      `### ${i}. 适用于 ${g.pkgs.length} 个包`,
      '',
      '<details><summary>包清单</summary>',
      '',
      ...g.pkgs.map((p) => `- ${p.name} ${p.version}${p.repository ? ` — ${p.repository}` : ''}`),
      '',
      '</details>',
      '',
    );
    for (const t of g.texts) {
      lines.push('```text', t.text, '```', '');
    }
  }
  return lines.join('\n');
}

const content = render([...rustDeps(), ...jsDeps()]);

if (CHECK) {
  if (!existsSync(OUT)) {
    console.error(`缺 ${OUT} —— 跑 \`node scripts/gen-third-party-licenses.mjs\` 生成`);
    process.exit(1);
  }
  if (readFileSync(OUT, 'utf8') !== content) {
    console.error('THIRD-PARTY-LICENSES.md 与当前依赖不一致 —— 依赖变了却没重新生成');
    process.exit(1);
  }
  console.log('ok: 第三方许可清单与当前依赖一致');
} else {
  writeFileSync(OUT, content);
  const kb = (Buffer.byteLength(content) / 1024).toFixed(0);
  console.log(`ok: ${OUT} 已生成（${kb}KB）`);
}
