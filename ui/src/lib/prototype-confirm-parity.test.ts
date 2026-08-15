/**
 * G6 —— 原型 ↔ 实现「确认站点」**双向锁**。
 *
 * # 为什么要这一半
 *
 * 同目录的 `destructive-confirm-wiring.test.ts` 守的是**实现内部自洽**：只有一份原语、武装态真渲染、
 * 弹窗清册逐文件对账。它有一个结构性盲区 —— **它只看实现**。原型那一侧改了（新增一个 `confirmTwice`
 * 调用点、或删掉一个），实现纹丝不动，它照样全绿。2026-07-29 真机手验抓到的「卸载弹窗不是原型的操作
 * 逻辑」正是这一类：原型 14 处 `confirmTwice` vs 实现 3 处弹窗，**纯静态可判**，却是靠人眼发现的。
 *
 * 本文件补的就是那一半：直接读原型 HTML 原文，抽出全部 `confirmTwice(` 调用站点，与实现侧的确认 key
 * 集合逐条对账。**任一侧新增/删除都转红**。
 *
 * # 判据面
 *
 *  - **原型侧** = `~/docs/polaris/design/prototype/polaris-prototype.html` 原文（可用环境变量
 *    `POLARIS_PROTOTYPE_HTML` 覆盖路径）。抽 `confirmTwice(` 的每个调用点，回溯到最近的
 *    `case '<act>':` 或 `function <name>(`，得到「站点标签」。
 *  - **实现侧** = `ui/src/**`（非测试）里 `confirmTwice(<key>, …)` 的第一个实参，解析成静态 key
 *    （常量标识符 → 同文件 const 值；模板串 → 取 `${}` 之前的静态前缀）。
 *
 * # 为什么读原型原文而不是钉一份快照
 *
 * 钉快照只能锁住实现侧；原型改了没人知道 —— 而「原型悄悄变了、实现按旧原型写」正是要防的方向之一。
 * 代价：本仓 UI 测试是**本地门**（`.github/workflows/ci.yml` 只跑 Rust 链，`paths-ignore` 含 `ui/**`），
 * 故依赖 vault 里的原型文件可接受；换机器/CI 要跑本文件时须提供该文件或设 `POLARIS_PROTOTYPE_HTML`。
 * 文件缺席 ⇒ **模块加载期 throw**（不是 skip）—— 「没检查」与「检查通过」的输出必须可区分。
 *
 * # 守形态不守措辞
 *
 * 断言的都是「站点集合」与「key 集合」这类结构事实。原型改文案（`T('确认删除？',…)`）、实现改注释都
 * 不会误伤；任一侧增删一个确认站点则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';

const SRC = fileURLToPath(new URL('..', import.meta.url));

// ── 原型侧取材 ──────────────────────────────────────────────────────────────

const PROTO_PATH =
  process.env.POLARIS_PROTOTYPE_HTML ??
  join(homedir(), 'docs/polaris/design/prototype/polaris-prototype.html');

if (!existsSync(PROTO_PATH)) {
  throw new Error(
    `[prototype-confirm-parity] 原型 HTML 不存在：${PROTO_PATH}\n` +
      '本守卫的判据面就是原型原文，读不到 = 这道门不存在。' +
      '请置备该文件，或用环境变量 POLARIS_PROTOTYPE_HTML 指向它。',
  );
}
const PROTO = readFileSync(PROTO_PATH, 'utf8');

if (PROTO.length < 100_000) {
  throw new Error(
    `[prototype-confirm-parity] 原型文件只有 ${PROTO.length} 字节 —— 疑似被截断/换成了占位文件`,
  );
}

/**
 * 抽原型里每个 `confirmTwice(` 调用点的「站点标签」。
 *
 * 回溯规则：从调用点往前找最近的 `case '<x>':` 与 `function <name>(`，取**更近**的那个。
 * 原型的 13 个调用点在 `dispatch` 的 `case` 分支里，第 14 个（`uninstall-app`）在
 * `function uninstallApp(t){ … }` 里 —— 单看 `case` 会回溯到几十行外的无关分支。
 *
 * 定义行 `function confirmTwice(btn, msg, action){` 本身不是调用点，显式排除。
 */
function protoConfirmSites(html: string): string[] {
  const out: string[] = [];
  const re = /confirmTwice\(/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(html))) {
    const at = m.index;
    if (html.slice(Math.max(0, at - 9), at) === 'function ') continue; // 定义行
    const head = html.slice(0, at);
    const caseAt = head.lastIndexOf("case '");
    const fnAt = head.lastIndexOf('function ');
    if (fnAt > caseAt) {
      const name = /^function\s+([A-Za-z_$][\w$]*)/.exec(head.slice(fnAt))?.[1];
      out.push(`fn:${name ?? '?'}`);
    } else {
      const act = /^case\s+'([^']+)'/.exec(head.slice(caseAt))?.[1];
      out.push(`case:${act ?? '?'}`);
    }
  }
  return out;
}

const PROTO_SITES = protoConfirmSites(PROTO);

if (PROTO_SITES.length === 0) {
  throw new Error(
    '[prototype-confirm-parity] 原型里一个 confirmTwice 调用点都没抽到 —— 抽取器塌了或原型换了写法',
  );
}
if (PROTO_SITES.some((s) => s.endsWith(':?'))) {
  throw new Error(
    `[prototype-confirm-parity] 有调用点回溯不到 case/function：${PROTO_SITES.filter((s) => s.endsWith(':?')).join(', ')}`,
  );
}

/** 原型 `confirmTwice` 定义里的自动复位时长（L3217 的 `2600`）。 */
function protoResetMs(html: string): number {
  const defAt = html.indexOf('function confirmTwice(');
  if (defAt < 0) throw new Error('[prototype-confirm-parity] 原型里找不到 confirmTwice 定义');
  const body = html.slice(defAt, defAt + 1200);
  const ms = /\}\s*,\s*(\d+)\s*\)\s*;/.exec(body)?.[1];
  if (!ms) throw new Error('[prototype-confirm-parity] 抽不出 confirmTwice 的自动复位时长');
  return Number(ms);
}

// ── 实现侧取材 ──────────────────────────────────────────────────────────────

/** 递归收集前端生产源码（排测试文件——测试里的样本是字符串字面量，扫它等于自己判自己）。 */
function collectSources(dir: string, acc: string[] = []): string[] {
  for (const e of readdirSync(dir)) {
    if (e === 'node_modules' || e === 'dist') continue;
    const full = join(dir, e);
    if (statSync(full).isDirectory()) collectSources(full, acc);
    else if (/\.tsx?$/.test(e) && !/\.(test|spec)\.tsx?$/.test(e)) acc.push(full);
  }
  return acc;
}

const FILES = collectSources(SRC).map((f) => ({
  rel: relative(SRC, f).split(sep).join('/'),
  src: readFileSync(f, 'utf8'),
}));

if (FILES.length < 100) {
  throw new Error(`[prototype-confirm-parity] 只扫到 ${FILES.length} 个源文件 —— 扫描面已塌`);
}

/** 去注释（本仓注释逐字引用原型调用点，直接扫原文会把注释里的 `confirmTwice(t, …)` 当成调用点）。 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

/** 同文件里 `const NAME = '值'` 的解析（key 常量与前缀常量都走这条）。 */
function constValue(src: string, name: string): string | null {
  const re = new RegExp(`const\\s+${name}\\s*=\\s*'([^']*)'`);
  return re.exec(src)?.[1] ?? null;
}

/**
 * 解析 `confirmTwice(<arg1>, …)` 的静态 key。三种写法：
 *  1. 裸串 `'geo-reset'`；
 *  2. 常量 `GEO_RESET_KEY` → 同文件 const 值；
 *  3. 模板串 `` `node-del:${id}` `` / `` `${RES_DEL_PREFIX}${id}` `` → 取 `${}` 之前的静态前缀
 *     （前缀为空则取首个 `${IDENT}` 的常量值）。
 * 末尾 `:` 剥掉 —— 那是「一站点多实例」的实例分隔符，不是站点名的一部分。
 */
function implConfirmKeys(rel: string, src: string): { key: string; file: string }[] {
  const out: { key: string; file: string }[] = [];
  const re = /(^|[^A-Za-z0-9_$.])confirmTwice\(\s*([^,]+),/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src))) {
    const raw = m[2].trim();
    let key: string | null = null;
    if (/^'[^']*'$/.test(raw)) key = raw.slice(1, -1);
    else if (/^[A-Za-z_$][\w$]*$/.test(raw)) key = constValue(src, raw);
    else if (raw.startsWith('`')) {
      const lit = /^`([^`$]*)\$\{/.exec(raw)?.[1];
      if (lit) key = lit;
      else {
        const ident = /^`\$\{([A-Za-z_$][\w$]*)\}/.exec(raw)?.[1];
        if (ident) key = constValue(src, ident);
      }
    }
    if (key === null) {
      throw new Error(
        `[prototype-confirm-parity] ${rel} 里的 confirmTwice(${raw}, …) 解析不出静态 key —— ` +
          '新写法必须让本守卫看得懂，否则这个确认站点就从对账里消失了',
      );
    }
    out.push({ key: key.replace(/:$/, ''), file: rel });
  }
  return out;
}

const IMPL_SITES = FILES.filter((f) => f.rel !== 'lib/confirm-twice.ts').flatMap((f) =>
  implConfirmKeys(f.rel, code(f.src)),
);

if (IMPL_SITES.length === 0) {
  throw new Error('[prototype-confirm-parity] 实现侧一个 confirmTwice 调用点都没抽到 —— 抽取器塌了');
}

// ── 登记表（双向锁的那张表）───────────────────────────────────────────────────

/**
 * 原型 14 个 `confirmTwice` 站点 ↔ 实现落点。
 *
 * `impl: null` = 实现侧**没有**对应确认站点。表里逐条写清是「待修」还是「有署名依据的有意缺席」——
 * 「本仓先例 / 既有惯例」不算理由（自我循环，2026-07-29 判据已切换成原型 ↔ 实现双向对拍）。
 *
 * `key` 与 `act` 不同名时必须在 `note` 里说清 —— key 是本仓单槽状态机的内部标识（`useConfirmTwice`
 * 的槽名），不是原型的 `data-act`，两者本就不必同名；但**不同名必须显式登记**，否则改名即失联。
 */
interface ParityRow {
  /** 原型站点标签（`case:<data-act>` 或 `fn:<函数名>`）。 */
  site: string;
  /** 实现侧 `confirmTwice` 的静态 key 与所在文件；`null` = 实现侧无此确认站点。 */
  impl: { key: string; file: string } | null;
  note: string;
}

/**
 * **实现侧独有**的确认站点 —— 原型没有对应 `confirmTwice`，故进不了上面那张双向表
 * （`site` 那一列被 `PROTO_SITES` 逐条锁死，塞一个原型没有的标签会让原型侧那条断言转红）。
 *
 * 新增一条的门槛与 `PARITY` 同高：**必须写清为什么这个动作值得一道确认**。
 * 本仓 `useConfirmTwice` 的射程是不可逆/破坏性操作；给可逆操作套确认会稀释「点两次 = 有危险」
 * 这个信号，那才是这张表存在的意义 —— 不是登记「谁用了」，是登记「凭什么用」。
 */
interface ImplOnlyRow {
  key: string;
  file: string;
  why: string;
}

const IMPL_ONLY: readonly ImplOnlyRow[] = [
  {
    key: 'conn-clear-closed',
    file: 'components/screens/connections/ConnectionsScreen.tsx',
    why:
      '清空已结束连接历史会永久丢失当前会话内最多 1000 条诊断记录，且清空水位会阻止上游重放恢复，' +
      '属于不可撤销的数据删除，因此保留二次确认。',
  },
  {
    key: 'node-use',
    file: 'components/screens/nodes/NodesScreen.tsx',
    why:
      '节点卡「设为出口」。**切节点本身不确认**（`server_switch` 只写 selectedServerId + 广播，不重启内核，' +
      '且在暂存层 BYPASS_TABLE 里被显式豁免为同步即时操作）——本站点只在选中「待入池/待生效」差集里的' +
      '节点时武装：那一次会让节点由未引用变被引用 ⇒ 恒立即整核重启、断掉现有连接，确认才有信息量。',
  },
];

const PARITY: readonly ParityRow[] = [
  {
    site: 'case:reset-pending',
    impl: { key: 'reset-pending', file: 'components/layout/PendingChangesBar.tsx' },
    note: '放弃全部待应用改动',
  },
  {
    site: 'case:core-rollback',
    impl: { key: 'core-rollback', file: 'components/screens/settings/SettingsUpdate.tsx' },
    note: '内核回滚',
  },
  {
    site: 'case:rule-del-dlg',
    impl: { key: 'rule-del-dlg', file: 'components/dialogs/RuleDialog.tsx' },
    note: '规则弹窗内删除',
  },
  {
    site: 'case:rule-del',
    impl: { key: 'rule-del', file: 'components/screens/rules/RulesScreen.tsx' },
    note:
      '规则列表**行内**删除（key 带 `:<id>` 实例后缀）。2026-07-30 补齐 —— 此前删一条规则必须先点' +
      '「编辑」开窗、再点 footer 那颗；与 `rule-del-dlg` 共用 `useRuleDelete` 一条执行腿。',
  },
  {
    site: 'case:conn-close-all',
    impl: { key: 'conn-close-all', file: 'components/screens/connections/ConnectionsScreen.tsx' },
    note: '关闭全部连接',
  },
  {
    site: 'case:conn-close-filtered',
    impl: {
      key: 'conn-close-filtered',
      file: 'components/screens/connections/ConnectionsScreen.tsx',
    },
    note: '关闭筛选命中的连接',
  },
  {
    site: 'case:log-clear',
    impl: { key: 'logs-clear', file: 'components/screens/logs/LogsScreen.tsx' },
    note:
      '清空日志。**key 与 act 不同名**（`logs-clear` vs `log-clear`）：key 是单槽状态机的内部槽名、' +
      '不渲染也不跨进程传递，与原型 `data-act` 无契约关系；此处显式登记该映射，改任一侧即转红。',
  },
  {
    site: 'case:batch-del',
    impl: { key: 'batch-del', file: 'components/screens/nodes/NodesScreen.tsx' },
    note: '批量删除选中节点',
  },
  {
    site: 'case:node-del',
    impl: { key: 'node-del', file: 'components/screens/nodes/NodesScreen.tsx' },
    note: '删除单节点（图标钮，key 带 `:<id>` 实例后缀）',
  },
  {
    site: 'case:app-remove',
    impl: { key: 'app-remove', file: 'components/screens/apppolicy/AppPolicyScreen.tsx' },
    note: '移除自定义应用（key 带 `:<id>` 实例后缀）',
  },
  {
    site: 'case:geo-reset',
    impl: { key: 'geo-reset', file: 'components/screens/resources/ResourcesScreen.tsx' },
    note: '重置内置资源',
  },
  {
    site: 'case:res-del',
    impl: { key: 'res-del', file: 'components/screens/resources/ResourcesScreen.tsx' },
    note: '删除规则资源（key 带 `:<id>` 实例后缀）',
  },
  {
    site: 'case:helper-uninstall',
    impl: { key: 'helper-uninstall', file: 'components/screens/settings/SettingsHelper.tsx' },
    note: '卸载提权助手',
  },
  {
    site: 'fn:uninstallApp',
    impl: { key: 'uninstall-app', file: 'components/screens/settings/SettingsAbout.tsx' },
    note: '完全卸载 Polaris（原型该站点不在 dispatch 的 case 里，走独立函数）',
  },
] as const;

// ── 断言 ────────────────────────────────────────────────────────────────────

describe('守卫自检：两侧判据面都真的存在（防空转恒绿）', () => {
  it('原型抽取器在合成样本上确实会命中（否则真原型抽出空集也无从察觉）', () => {
    const sample = `
      case 'demo-act': confirmTwice(t, '？', ()=>{}); break;
      function demoFn(t){ confirmTwice(t, '', ()=>{}); }
      function confirmTwice(btn, msg, action){ }
    `;
    // 定义行被排除；两个真调用点被抽出。
    expect(protoConfirmSites(sample)).toEqual(['case:demo-act', 'fn:demoFn']);
  });

  it('实现侧 key 解析器三种写法都解得出（裸串 / 常量 / 模板前缀）', () => {
    const sample = [
      "const A_KEY = 'alpha';",
      "const P = 'beta:';",
      "confirmTwice('gamma', () => {});",
      'confirmTwice(A_KEY, () => {});',
      'confirmTwice(`delta:${id}`, () => {});',
      'confirmTwice(`${P}${id}`, () => {});',
    ].join('\n');
    expect(implConfirmKeys('sample.ts', sample).map((s) => s.key)).toEqual([
      'gamma',
      'alpha',
      'delta',
      'beta',
    ]);
  });

  it('`useConfirmTwice()` 不会被误当成调用点（大小写敏感的负向对照）', () => {
    expect(implConfirmKeys('sample.ts', 'const { confirmTwice } = useConfirmTwice();')).toEqual([]);
  });

  it('去注释真的生效（否则注释里逐字抄的原型调用点会污染实现侧集合）', () => {
    const withComment = "// confirmTwice(t, '注释里的', …)\nconfirmTwice('real', () => {});";
    expect(implConfirmKeys('sample.ts', code(withComment)).map((s) => s.key)).toEqual(['real']);
  });
});

describe('G6：原型 ↔ 实现确认站点双向锁', () => {
  it('原型侧站点集合与登记表逐条相等（原型新增/删除 confirmTwice ⇒ 转红）', () => {
    // 变异对照：往原型里加一个 `case 'x': confirmTwice(...)` → 本条转红并点名 `case:x`。
    expect([...PROTO_SITES].sort()).toEqual([...PARITY.map((r) => r.site)].sort());
  });

  it('原型的自动复位时长与 `CONFIRM_TWICE_MS` 逐字相等（原型改 2600 ⇒ 转红）', async () => {
    const { CONFIRM_TWICE_MS } = await import('./confirm-twice');
    expect(protoResetMs(PROTO)).toBe(CONFIRM_TWICE_MS);
  });

  it('实现侧 key 集合与登记表逐条相等（实现新增/删除确认站点 ⇒ 转红）', () => {
    const expected = [
      ...PARITY.flatMap((r) => (r.impl ? [`${r.impl.file} :: ${r.impl.key}`] : [])),
      ...IMPL_ONLY.map((r) => `${r.file} :: ${r.key}`),
    ];
    const actual = IMPL_SITES.map((s) => `${s.file} :: ${s.key}`);
    // 去重：`node-del` / `res-del` / `app-remove` 各只有一个调用点，但同 key 若出现两次说明
    // 同一站点被复制了一份实现 —— 那要显式登记，不该悄悄通过。
    expect([...new Set(actual)].sort()).toEqual([...new Set(expected)].sort());
    expect(actual.length, '同一 key 出现多次 = 确认站点被复制，需显式登记').toBe(
      new Set(actual).size,
    );
  });

  it('登记为 null 的站点，实现侧确实找不到对应 key（防「已修了但表还写着缺失」）', () => {
    const implKeys = new Set(IMPL_SITES.map((s) => s.key));
    const stale = PARITY.filter((r) => r.impl === null).filter((r) => {
      const act = r.site.replace(/^case:/, '').replace(/^fn:/, '');
      return implKeys.has(act);
    });
    expect(
      stale.map((r) => r.site),
      '登记表写着「实现侧缺失」但磁盘上已经有了 —— 把它从缺失改成正式登记',
    ).toEqual([]);
  });

  it('缺失项必须逐条写明理由，且理由不得诉诸「本仓先例 / 既有惯例」', () => {
    for (const r of PARITY.filter((x) => x.impl === null)) {
      expect(r.note.length, `${r.site} 的缺失理由太短，说不清`).toBeGreaterThan(20);
      expect(r.note, `${r.site} 的理由是自我循环`).not.toMatch(/本仓先例|既有惯例|历来如此/);
    }
  });
});
