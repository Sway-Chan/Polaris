/**
 * G11 —— 通道常量绕过守卫 + 跨语言通道名单一真值源。
 *
 * # 守什么
 *
 * `domain/ipc-channels.ts` 存在的唯一理由是「通道名只有一处可改」。它挡不住的是**绕过**：
 * 谁在别处写 `invoke('update_popup_action', …)`，常量表里那条键就变成零引用的死条目，
 * 而两边的字符串从此各改各的、静默失联。2026-07-29 交互层对拍在更新弹窗上再次撞到这一形态
 * （常量 `UPDATE_POPUP_ACTION` 定义了但零引用；实际调用是裸字面量）——**该处已于当日修回常量表**。
 * 本门建成时同类还剩 9 处（`renderer_ready` 绕过 + `renderer_log`/7 条 `tray_*` 未收编），**已于同日全部清掉**：
 * 8 条补进 `IPC_CHANNELS`、13 个调用点改用常量。`BARE_REGISTRY` 因此只剩 Tauri 官方插件那一档真豁免 ——
 * 表外再出现任何裸通道名即转红，账不许再长回来。
 *
 * 这是跨语言字符串派发面：Rust `#[tauri::command]` 经 `invoke(<字符串>)` 调用，**静态调用图上没有这条边**
 * ⇒ `callers == 0` 是假阴性，只能按内容反查。本文件即那份反查，做成常驻门。
 *
 * # 与既有门的分工（不重叠）
 *
 *  - `scripts/check-ipc-args.mjs`（挂在 `npm run build`）：守 `api-client.ts` 调用点的**参数袋键**
 *    覆盖 Rust required 参数。射程是参数，不是通道名，也不看 `api-client.ts` 之外。
 *  - 本文件：守**通道名**本身 —— 谁绕过常量表、哪些常量成了死条目、跨语言两侧的字符串是否仍相等。
 *
 * # 判据面
 *
 *  1. `ui/src/**`（非测试）里 `invoke(<字面量>)` / `listen(<字面量>)` 的第一个实参 —— 裸通道名全集；
 *  2. `domain/ipc-channels.ts` 的 `IPC_CHANNELS` 键值表；
 *  3. `src-tauri/src` 下全部 `.rs` 原文 —— 命令函数名与事件名字符串（跨语言那一侧的地面真值）。
 *
 * # 守形态不守措辞
 *
 * 断言的是「集合相等」「字符串两侧相等」这类结构事实。改注释、改文案不会误伤；
 * 新写一个裸 `invoke('x')`、或把常量表里某条改名而 Rust 侧没跟着改，都必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SRC = fileURLToPath(new URL('..', import.meta.url));
const REPO = fileURLToPath(new URL('../../..', import.meta.url));
const TAURI_SRC = join(REPO, 'src-tauri', 'src');

/** 递归收集前端生产源码（排测试文件——测试里的违规样本是字符串字面量，扫它等于自己判自己违规）。 */
function collectSources(dir: string, ext: RegExp, acc: string[] = []): string[] {
  for (const e of readdirSync(dir)) {
    if (e === 'node_modules' || e === 'dist') continue;
    const full = join(dir, e);
    if (statSync(full).isDirectory()) collectSources(full, ext, acc);
    else if (ext.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) acc.push(full);
  }
  return acc;
}

/** 去注释（本仓注释里逐字写着 `invoke('plugin:window|start_dragging')` 这类举例，扫原文会误判）。 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const FILES = collectSources(SRC, /\.tsx?$/).map((f) => ({
  rel: relative(SRC, f).split(sep).join('/'),
  src: code(readFileSync(f, 'utf8')),
}));

const RUST = collectSources(TAURI_SRC, /\.rs$/)
  .map((f) => readFileSync(f, 'utf8'))
  .join('\n');

// ── 自曝：三块判据面缺任何一块，本守卫就没有判据 ──

if (FILES.length < 100) {
  throw new Error(`[ipc-channel-bypass] 只扫到 ${FILES.length} 个前端源文件 —— 扫描面已塌`);
}
if (RUST.length < 100_000) {
  throw new Error(
    `[ipc-channel-bypass] Rust 侧只读到 ${RUST.length} 字节（${TAURI_SRC}）—— 跨语言对拍面已塌`,
  );
}

const CHANNELS_FILE = FILES.find((f) => f.rel === 'domain/ipc-channels.ts');
if (!CHANNELS_FILE) throw new Error('[ipc-channel-bypass] 锚点缺失：domain/ipc-channels.ts');

/** 解析 `IPC_CHANNELS` 的 `键 → 值`。 */
function parseChannels(src: string): Record<string, string> {
  const body = src.slice(src.indexOf('export const IPC_CHANNELS'));
  const out: Record<string, string> = {};
  const re = /^\s{2}([A-Z][A-Z0-9_]*)\s*:\s*'([^']+)'/gm;
  let m: RegExpExecArray | null;
  while ((m = re.exec(body))) out[m[1]] = m[2];
  return out;
}

const CHANNELS = parseChannels(CHANNELS_FILE.src);
if (Object.keys(CHANNELS).length < 100) {
  throw new Error(
    `[ipc-channel-bypass] IPC_CHANNELS 只解析出 ${Object.keys(CHANNELS).length} 条 —— 解析器塌了`,
  );
}

/** 抽 `invoke('x')` / `listen('x')` 的裸字面量通道名（含泛型实参写法 `invoke<T>('x')`）。 */
function bareChannelLiterals(src: string): { fn: 'invoke' | 'listen'; name: string }[] {
  const out: { fn: 'invoke' | 'listen'; name: string }[] = [];
  const re = /(^|[^A-Za-z0-9_$.])(invoke|listen)\s*(?:<[^<>()]*>)?\(\s*'([^']+)'/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src))) out.push({ fn: m[2] as 'invoke' | 'listen', name: m[3] });
  return out;
}

const BARE = FILES.flatMap((f) =>
  bareChannelLiterals(f.src).map((b) => ({ ...b, file: f.rel })),
);

if (BARE.length === 0) {
  throw new Error('[ipc-channel-bypass] 一个裸通道字面量都没抽到 —— 抽取器塌了（登记表会恒绿）');
}

// ── 登记表 1：裸字面量白名单 ──────────────────────────────────────────────────

/**
 * 每个绕过常量表的裸通道名逐条登记。**表外命中 → 转红**。
 *
 * `status` 三档：
 *  - `builtin-plugin` —— Tauri 官方插件命令（`plugin:<插件>|<命令>`）。它们**不是**本仓的
 *    `#[tauri::command]`，Rust 侧没有同名函数可查，也不该进 `IPC_CHANNELS`（那张表的头注明写
 *    「值必须逐字等于 Rust 命令函数名」）。这是形态上的真豁免。
 *  - `no-constant` —— 常量表里根本没有这条通道，属**待补**（不是豁免）。
 *  - `bypass` —— 常量表里**有**同值的键却没用它，属**待修**（不是豁免）。
 *
 * 后两档一律带 `todo`，交回里已逐条点名等裁定；本表只保证「账目可见且不许再长」。
 */
interface BareRow {
  name: string;
  status: 'builtin-plugin' | 'no-constant' | 'bypass';
  reason: string;
}

const BARE_REGISTRY: readonly BareRow[] = [
  {
    name: 'plugin:os|platform',
    status: 'builtin-plugin',
    reason: 'tauri-plugin-os 的内置命令，非本仓 #[tauri::command]，按定义不进 IPC_CHANNELS',
  },
  {
    name: 'plugin:notification|is_permission_granted',
    status: 'builtin-plugin',
    reason: 'tauri-plugin-notification 内置命令，同上',
  },
  {
    name: 'plugin:notification|request_permission',
    status: 'builtin-plugin',
    reason: 'tauri-plugin-notification 内置命令，同上',
  },
  {
    name: 'plugin:notification|notify',
    status: 'builtin-plugin',
    reason: 'tauri-plugin-notification 内置命令，同上',
  },
  // `renderer_ready` / `renderer_log` / 7 条 `tray_*` 曾登记在此（`bypass` + `no-constant` 两档，共 9 条）。
  // 2026-07-29 已全部清掉：8 条补进 IPC_CHANNELS（RENDERER_LOG + TRAY_*），13 个调用点改用常量。
  // 表里现在只剩官方插件那一档真豁免 —— 本仓自己的通道名再出现裸字面量即转红。
] as const;

// ── 登记表 2：常量表里的零引用键 ─────────────────────────────────────────────

/**
 * `IPC_CHANNELS` 里定义了却在前端全仓零引用的键。零引用 = 「改它不会有任何后果」= 漂移温床。
 * 表外出现新的零引用键 → 转红（有人加了常量却没接线，或接线被删了没清常量）。
 */
const DEAD_CHANNEL_REGISTRY: Readonly<Record<string, string>> = {
  FATAL_RETRY:
    '真豁免：唯一调用方是 Rust 注入终局页的 JS（src-tauri/src/window_health.rs 的 ' +
    '`__TAURI_INTERNALS__.invoke(\'fatal_retry\')`），不经前端 bundle ⇒ TS 侧零引用是必然。' +
    '本文件另有一条断言锁死「该值必须在 window_health.rs 里逐字出现」，避免它变成谁也不管的孤儿。',
  UPDATE_POPUP_STATE:
    '待裁定：值 `update_popup_state` 与两侧实际使用的名字都对不上 —— 主→弹窗的事件名是 ' +
    '`event:updatePopupState`（Rust runtime/update_popup.rs 与 ui/src/update-popup/main.ts 各硬编码一份），' +
    '另有 src-tauri/src/events.rs 的 `UPDATE_POPUP_STATE = "update:popupState"` 全仓零引用。' +
    '三个同名不同值的常量、零个被用上。',
};

// ── 登记表 3：常量表之外硬编码的跨语言 event 名 ──────────────────────────────

/**
 * `'event:*'` 字面量出现在 `domain/ipc-channels.ts` 之外 = 前端这一侧多了一份手抄。
 * 逐条登记，并对每条钉死「它必须与 Rust 侧的字符串逐字相等」。
 */
const HARDCODED_EVENT_REGISTRY: readonly {
  file: string;
  value: string;
  rustAnchor: string;
  reason: string;
}[] = [
  {
    file: 'update-popup/main.ts',
    value: 'event:updatePopupState',
    rustAnchor: 'pub const EVENT_POPUP_STATE: &str = "event:updatePopupState";',
    reason:
      '待修：主→弹窗的状态事件名两侧各硬编码一份（对拍 §12.15），改一边另一边静默失联。' +
      '本条在门修好之前至少锁住「两侧字符串相等」。',
  },
] as const;

// ── 断言 ────────────────────────────────────────────────────────────────────

describe('守卫自检：判据面真的存在（防空转恒绿）', () => {
  it('裸字面量抽取器在合成样本上确实会命中', () => {
    const sample = [
      "invoke('foo_bar');",
      "invoke<boolean>('baz_qux');",
      "listen('event:someThing', cb);",
      'invoke(IPC_CHANNELS.PROXY_START);', // 走常量 → 不该命中
      "someObj.invoke('not_a_channel');", // 成员调用 → 不该命中
    ].join('\n');
    expect(bareChannelLiterals(sample)).toEqual([
      { fn: 'invoke', name: 'foo_bar' },
      { fn: 'invoke', name: 'baz_qux' },
      { fn: 'listen', name: 'event:someThing' },
    ]);
  });

  it('常量表解析器解得出已知条目（否则「零引用键」会算成全表）', () => {
    expect(CHANNELS.PROXY_START).toBe('proxy_start');
    expect(CHANNELS.EVENT_PROXY_STARTED).toBe('event:proxyStarted');
  });

  it('去注释真的生效（注释里举例的 `invoke(\'plugin:window|start_dragging\')` 不该算违规）', () => {
    const withComment = "// 命中该属性即 invoke('plugin:window|start_dragging')\ninvoke('real_cmd');";
    expect(bareChannelLiterals(code(withComment)).map((b) => b.name)).toEqual(['real_cmd']);
  });
});

describe('G11-1：绕过常量表的裸通道名必须逐条登记', () => {
  it('裸字面量集合与登记表逐条相等（新写一个裸 invoke ⇒ 转红）', () => {
    // 变异对照：在任一屏加一行 `void invoke('brand_new_cmd')` → 本条转红并点名 brand_new_cmd。
    const actual = [...new Set(BARE.map((b) => b.name))].sort();
    const registered = [...BARE_REGISTRY.map((r) => r.name)].sort();
    expect(actual, '裸通道名与登记表不符 —— 见 BARE_REGISTRY 头注的登记规则').toEqual(registered);
  });

  it('登记的 status 与磁盘现状一致（防「已改用常量了但表还写着 bypass」）', () => {
    const valueToKey = new Map(Object.entries(CHANNELS).map(([k, v]) => [v, k]));
    const wrong = BARE_REGISTRY.filter((r) => {
      const hasConst = valueToKey.has(r.name);
      if (r.status === 'builtin-plugin') return !r.name.startsWith('plugin:');
      if (r.status === 'bypass') return !hasConst;
      return hasConst; // no-constant
    }).map((r) => `${r.name}(${r.status})`);
    expect(wrong, 'status 与常量表现状对不上 —— 修好了就把登记一起改掉').toEqual([]);
  });

  it('每条登记都有理由，且「待修/待补」不许伪装成豁免', () => {
    for (const r of BARE_REGISTRY) {
      expect(r.reason.length, `${r.name} 的理由太短`).toBeGreaterThan(15);
      expect(r.reason, `${r.name} 的理由是自我循环`).not.toMatch(/本仓先例|既有惯例|历来如此/);
      if (r.status !== 'builtin-plugin') {
        expect(r.reason, `${r.name} 不是真豁免，理由须以「待修」/「待补」起头`).toMatch(/^待[修补]/);
      }
    }
  });

  it('非插件的裸通道名在 Rust 侧真有同名命令（跨语言存在性，callers==0 在这里是假阴性）', () => {
    // 变异对照：把 `invoke('tray_hide')` 改成 `invoke('tray_hidee')` → 本条转红。
    const missing = BARE.filter((b) => b.fn === 'invoke' && !b.name.startsWith('plugin:'))
      .filter((b) => !new RegExp(`fn\\s+${b.name}\\s*\\(`).test(RUST))
      .map((b) => `${b.file} :: ${b.name}`);
    expect(missing, 'Rust 侧找不到同名 #[tauri::command] —— 运行期必然 command not found').toEqual(
      [],
    );
  });
});

describe('G11-2：常量表里的零引用键必须逐条登记', () => {
  it('零引用键集合与登记表逐条相等（新增死条目 / 死条目复活都说话）', () => {
    // 变异对照：把 api-client.ts 里某处 IPC_CHANNELS.CONFIG_GET 换成裸字面量 → CONFIG_GET 变零引用 → 转红。
    const referenced = new Set(
      FILES.flatMap((f) => f.src.match(/IPC_CHANNELS\.([A-Z0-9_]+)/g) ?? []).map((s) =>
        s.replace('IPC_CHANNELS.', ''),
      ),
    );
    const dead = Object.keys(CHANNELS)
      .filter((k) => !referenced.has(k))
      .sort();
    expect(dead, '零引用常量与登记表不符 —— 见 DEAD_CHANNEL_REGISTRY 头注').toEqual(
      Object.keys(DEAD_CHANNEL_REGISTRY).sort(),
    );
  });

  it('FATAL_RETRY 的唯一调用方（Rust 注入的终局页 JS）仍在，且字符串两侧逐字相等', () => {
    // 这条把 DEAD_CHANNEL_REGISTRY 里唯一的「真豁免」变成可证伪的：
    // Rust 侧那行没了 / 改名了 ⇒ 转红，不许它悄悄变成谁也不管的孤儿。
    // 必须去注释后再断言：该文件的文档注释里也逐字写着这句调用，只扫原文的话「把真代码改坏、
    // 只留注释」会照样绿（实测过：单改 :325 那行、留 :286 的注释，本条不转红 ⇒ 无牙）。
    const wh = code(readFileSync(join(TAURI_SRC, 'window_health.rs'), 'utf8'));
    expect(wh, "window_health.rs 里 invoke('fatal_retry') 那行消失了").toContain(
      `invoke('${CHANNELS.FATAL_RETRY}'`,
    );
  });
});

describe('G11-3：跨语言通道名单一真值源（两侧字符串必须相等）', () => {
  it('全部 EVENT_* 值都能在 src-tauri 里逐字找到（前端改名而 Rust 没跟 ⇒ 订阅静默收不到）', () => {
    // 变异对照：把 EVENT_PROXY_STARTED 改成 'event:proxyStarted2' → 本条转红。
    const evs = Object.entries(CHANNELS).filter(([k]) => k.startsWith('EVENT_'));
    expect(evs.length, 'EVENT_* 一条都没解析出来 —— 本条会恒绿').toBeGreaterThan(20);
    const missing = evs.filter(([, v]) => !RUST.includes(`"${v}"`)).map(([k, v]) => `${k}=${v}`);
    expect(missing, 'Rust 侧找不到同名事件字符串 —— 跨语言派发已失联').toEqual([]);
  });

  it('全部 command 值都能在 src-tauri 找到同名函数（值 = Rust 函数名，是 IPC_CHANNELS 头注的硬约束）', () => {
    const cmds = Object.entries(CHANNELS).filter(([k]) => !k.startsWith('EVENT_'));
    expect(cmds.length, 'command 一条都没解析出来 —— 本条会恒绿').toBeGreaterThan(80);
    const missing = cmds
      .filter(([, v]) => !new RegExp(`fn\\s+${v}\\s*\\(`).test(RUST))
      .map(([k, v]) => `${k}=${v}`);
    expect(missing, 'Rust 侧找不到同名命令函数 —— 运行期 command not found，且 tsc 查不出').toEqual(
      [],
    );
  });

  it('常量表之外硬编码的 `event:*` 必须逐条登记，且与 Rust 侧字符串逐字相等', () => {
    const hard = FILES.filter((f) => f.rel !== 'domain/ipc-channels.ts').flatMap((f) =>
      (f.src.match(/'event:[^']+'/g) ?? []).map((s) => `${f.rel} :: ${s.slice(1, -1)}`),
    );
    expect(
      [...new Set(hard)].sort(),
      '常量表之外多了一份手抄的 event 名 —— 见 HARDCODED_EVENT_REGISTRY',
    ).toEqual(HARDCODED_EVENT_REGISTRY.map((r) => `${r.file} :: ${r.value}`).sort());

    for (const r of HARDCODED_EVENT_REGISTRY) {
      // 变异对照：把 main.ts 那行或 Rust 那行任一改字 → 本条转红。
      expect(RUST, `Rust 侧锚点变了：${r.rustAnchor}`).toContain(r.rustAnchor);
      expect(r.reason).toMatch(/^待修/);
    }
  });
});
