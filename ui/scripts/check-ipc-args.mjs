#!/usr/bin/env node
/**
 * IPC 参数袋防回归门（BUG-2「missing required key」族的静态断言）。
 *
 * 背景：`invoke(cmd, args)` 的 `args` 是 **Tauri 参数袋**（`Record<string, unknown>`），Tauri 按
 * Rust `#[tauri::command]` 的**具名参数**去袋里取值。前端若把一个领域对象/裸标量**整个当参数袋**传
 * （`invoke(PROXY_START, config)` 而非 `invoke(PROXY_START, { config })`），Tauri 找不到 required key
 * → 运行期 `missing required key config` → 命令炸。这类错 **tsc 抓不到**（`invoke` 签名是 `args?: unknown`）。
 *
 * 本门直接对拍 **前端调用点**（ui/src/ipc/api-client.ts）与 **Rust 命令签名**
 * （src-tauri/src/commands/*.rs 的 `#[tauri::command]` 具名参数）——两侧任一漂移都转红，
 * 不依赖手维护的映射表（自身即 ground truth 对拍）。
 *
 * 判定（只锁 crash 族，不苛求 extra key —— 大量未接线 stub 命令会忽略前端多传的键）：
 *   - 参数键集必须**覆盖** Rust 所有 **required**（非 `Option<_>`、非注入 State/AppHandle/Window）参数。
 *   - 参数键 = 前端调用点的对象字面量键（Tauri 默认 camelCase，对齐 Rust 参数名的 lowerCamelCase）；
 *     `invokeScalar(cmd, x)` 恒包成 `{ value }`；无参调用键集为空。
 *   - **裸标识符**参数（`invoke(cmd, foo)`，无法静态取键）在命令有 required 参数时直接判红：
 *     无法证明其包了参数袋，且这正是 BUG-2 的形态 —— 逼调用点写对象字面量。
 *
 * 无新增依赖（纯 node:fs + 正则）。CI：`node scripts/check-ipc-args.mjs`（已挂进 ui `build`）。
 *
 * 关键实现依据：tauri-macros 2.6.3 `src/command/wrapper.rs:484` —— 参数键 = 形参 ident 经
 * `to_lower_camel_case`（`_value` → `value`，`server_id` → `serverId`，前导下划线作分隔被吃掉）。
 */

import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const ROOT = join(SCRIPT_DIR, '..', '..'); // <repo>/ui/scripts → <repo>
const API_CLIENT = join(ROOT, 'ui', 'src', 'ipc', 'api-client.ts');
const CHANNELS = join(ROOT, 'ui', 'src', 'domain', 'ipc-channels.ts');
const COMMANDS_DIR = join(ROOT, 'src-tauri', 'src', 'commands');

/** snake/underscore → lowerCamelCase（对齐 tauri-macros `to_lower_camel_case`）。 */
function camel(name) {
  const parts = name.split('_').filter(Boolean);
  if (parts.length === 0) return '';
  return (
    parts[0].toLowerCase() +
    parts
      .slice(1)
      .map((p) => p.charAt(0).toUpperCase() + p.slice(1).toLowerCase())
      .join('')
  );
}

/** 深度感知的顶层逗号切分（尊重 <> () [] {}）。 */
function splitTop(s) {
  const out = [];
  let depth = 0;
  let cur = '';
  for (const ch of s) {
    if ('<([{'.includes(ch)) depth++;
    else if ('>)]}'.includes(ch)) depth--;
    if (ch === ',' && depth === 0) {
      out.push(cur);
      cur = '';
    } else cur += ch;
  }
  if (cur.trim()) out.push(cur);
  return out.map((x) => x.trim()).filter(Boolean);
}

// ── 1. Rust 命令签名 → required / optional 参数键集 ──────────────────────────────
function parseRustCommands() {
  const map = new Map(); // cmd → { required:Set, optional:Set }
  for (const f of readdirSync(COMMANDS_DIR).filter((n) => n.endsWith('.rs'))) {
    const src = readFileSync(join(COMMANDS_DIR, f), 'utf8');
    const re =
      /#\[tauri::command\]\s*(?:#\[[^\]]*\]\s*)*pub\s+(?:async\s+)?fn\s+(\w+)\s*\(([\s\S]*?)\)\s*(?:->|\{)/g;
    let m;
    while ((m = re.exec(src))) {
      const cmd = m[1];
      const required = new Set();
      const optional = new Set();
      for (const p of splitTop(m[2])) {
        const colon = p.indexOf(':');
        if (colon < 0) continue;
        const rawName = p.slice(0, colon).trim();
        const type = p.slice(colon + 1).trim();
        if (rawName === '_' || rawName === 'self') continue;
        // 注入参数（Tauri 自动填，不是 JS 参数袋键）。
        if (/\bAppHandle\b|\bState\s*<|\bWebviewWindow\b|\bWindow\b|\bAppRuntime\b/.test(type)) continue;
        const key = camel(rawName);
        if (!key) continue;
        if (/^Option\s*</.test(type)) optional.add(key);
        else required.add(key);
      }
      map.set(cmd, { required, optional });
    }
  }
  return map;
}

// ── 2. IPC_CHANNELS 名 → 命令串 ───────────────────────────────────────────────
function parseChannels() {
  const src = readFileSync(CHANNELS, 'utf8');
  const map = new Map();
  const re = /\b([A-Z0-9_]+):\s*'([^']+)'/g;
  let m;
  while ((m = re.exec(src))) map.set(m[1], m[2]);
  return map;
}

// ── 3. api-client.ts 的 invoke / invokeScalar 调用点 ─────────────────────────────
function parseCalls() {
  const src = readFileSync(API_CLIENT, 'utf8');
  const lines = src.split('\n');
  const lineOf = (idx) => src.slice(0, idx).split('\n').length;
  const calls = [];
  const re = /\b(invokeScalar|invoke)\s*(?:<[^>]*>)?\s*\(/g;
  let m;
  while ((m = re.exec(src))) {
    const fn = m[1];
    const open = re.lastIndex - 1; // '(' 位置
    let depth = 0;
    let j = open;
    for (; j < src.length; j++) {
      if (src[j] === '(') depth++;
      else if (src[j] === ')') {
        depth--;
        if (depth === 0) break;
      }
    }
    const inner = src.slice(open + 1, j);
    const parts = splitTop(inner);
    if (parts.length === 0) continue;
    const chMatch = /IPC_CHANNELS\.(\w+)|STATS_TOPIC_EVENT/.exec(parts[0]);
    if (!chMatch || !chMatch[1]) continue; // 非 IPC_CHANNELS.* 首参（如事件），跳过
    calls.push({
      fn,
      channel: chMatch[1],
      arg: parts.length > 1 ? parts.slice(1).join(',').trim() : undefined,
      line: lineOf(open),
    });
  }
  return { calls, lines };
}

/** 对象字面量 `{ a, b: x }` → 顶层键；非对象字面量返回 null（未知）。 */
function objectKeys(expr) {
  const s = expr.trim();
  if (!s.startsWith('{')) return null;
  let depth = 0;
  let start = -1;
  let end = -1;
  for (let k = 0; k < s.length; k++) {
    if (s[k] === '{') {
      if (depth === 0) start = k;
      depth++;
    } else if (s[k] === '}') {
      depth--;
      if (depth === 0) {
        end = k;
        break;
      }
    }
  }
  if (start < 0 || end < 0) return null;
  const keys = [];
  for (let p of splitTop(s.slice(start + 1, end))) {
    p = p.trim();
    if (!p || p.startsWith('...')) continue;
    const key = p.split(':')[0].trim();
    if (key) keys.push(key);
  }
  return keys;
}

function main() {
  const rust = parseRustCommands();
  const channels = parseChannels();
  const { calls } = parseCalls();

  const errors = [];
  let checked = 0;

  for (const c of calls) {
    const cmd = channels.get(c.channel);
    if (!cmd) {
      errors.push(`api-client.ts:${c.line}  IPC_CHANNELS.${c.channel} 未在 ipc-channels.ts 定义`);
      continue;
    }
    if (cmd.includes(':')) continue; // event 名（不是 command），非 invoke 目标
    const sig = rust.get(cmd);
    if (!sig) {
      errors.push(
        `api-client.ts:${c.line}  invoke("${cmd}") 目标命令在 src-tauri/src/commands/*.rs 无 #[tauri::command] 定义`
      );
      continue;
    }
    checked++;
    const required = [...sig.required];

    // 计算前端传入的参数键集。
    let keys; // string[] | null(未知/裸标识符)
    if (c.fn === 'invokeScalar') keys = ['value'];
    else if (c.arg === undefined) keys = [];
    else keys = objectKeys(c.arg);

    if (keys === null) {
      // 裸标识符 / 非对象字面量：无法静态证明它是正确参数袋。
      if (required.length > 0) {
        errors.push(
          `api-client.ts:${c.line}  invoke("${cmd}") 传入裸参数 \`${c.arg}\`（非对象字面量），` +
            `无法核对是否覆盖 required 参数 [${required.join(', ')}] —— 请写成 { ${required.join(', ')} }`
        );
      }
      continue;
    }
    const missing = required.filter((r) => !keys.includes(r));
    if (missing.length > 0) {
      errors.push(
        `api-client.ts:${c.line}  invoke("${cmd}") 参数袋缺 required 键 [${missing.join(', ')}]` +
          `（实传 [${keys.join(', ') || '∅'}]，Rust required [${required.join(', ')}]）`
      );
    }
  }

  if (errors.length > 0) {
    console.error(`\n✗ IPC 参数袋门失败（${errors.length} 处）：\n`);
    for (const e of errors) console.error('  ' + e);
    console.error(
      `\n根因：Tauri 按 Rust 具名参数从参数袋取值；漏包/错包 required 键 → 运行期 missing key 崩。\n`
    );
    process.exit(1);
  }
  console.log(
    `✓ IPC 参数袋门通过：核对 ${checked} 处 invoke 调用 vs ${rust.size} 个 Rust 命令，required 键全覆盖。`
  );
}

main();
