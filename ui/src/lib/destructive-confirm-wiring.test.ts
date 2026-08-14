/**
 * 破坏性操作的**确认形态**对拍守卫 —— 钉死「原地二次点击，不弹窗」这条接线事实。
 *
 * 为什么必须是源码结构守卫而不是又一组逻辑单测：`confirm-twice.test.ts` 只能证明那台状态机自己对，
 * 它证明不了**各屏真的在用它**。本仓 vitest 是 `environment:'node'`（无 jsdom / testing-library，
 * 有意为之）⇒ 组件渲染不了，谁哪天在某个屏里写回 `openDialog({ kind: 'confirm' })`，
 * 状态机的单测会全绿而对拍偏差复活。这正是 2026-07-29 真机手验抓到的形态：
 * 同一交互类在本仓有三套实现（timeout / onBlur / 自绘弹窗），用户在不同屏得到不同的肌肉记忆。
 *
 * 判据取自原型 `~/docs/polaris/design/prototype/polaris-prototype.html`：
 *  - `confirmTwice` 定义 L3211-3218（2600ms、复原文案、二次点击前清 timeout）；
 *  - 调用点 :4070 / :4075 / :4095 / :4097 / :4113 / :4114 / :4130 / :4137 / :4140 / :4173 / :4198 /
 *    :4217 / :4234 / :5185 共 14 处破坏性操作。
 *
 * 守的是**形态**不是措辞：断言的都是「哪个文件 import 了哪个模块 / 哪段回调里有没有开弹窗」这类
 * 结构事实，改注释与文案不会误伤，把某处换回弹窗则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SRC = fileURLToPath(new URL('..', import.meta.url));

/** 递归收集前端生产源码（排测试文件——测试里的违规样本是字符串字面量，扫它等于自己判自己违规）。 */
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

/**
 * ── 自曝：扫不到判据面就在**模块加载期**炸，不留「空集合 toEqual([]) 恒绿」的余地 ──
 *
 * 每个锚点都是本守卫真正要守的那个文件。只写「文件数 > 0」挡得住全塌、挡不住缩水
 * （递归分支坏掉只剩顶层时，数量仍 > 0，守卫悄悄只守着一个文件还是绿的）。
 */
const ANCHORS = [
  'lib/confirm-twice.ts',
  'components/screens/logs/LogsScreen.tsx',
  'components/screens/connections/ConnectionsScreen.tsx',
  'components/screens/settings/SettingsAbout.tsx',
  'components/screens/settings/SettingsHelper.tsx',
  'components/screens/nodes/NodesScreen.tsx',
  'components/screens/nodes/NodeCard.tsx',
  'components/dialogs/dialog-store.ts',
  'components/layout/PendingChangesBar.tsx',
  'components/screens/resources/ResourcesScreen.tsx',
  'components/screens/apppolicy/AppPolicyScreen.tsx',
  'components/dialogs/RuleDialog.tsx',
  'components/screens/settings/SettingsUpdate.tsx',
] as const;

if (FILES.length < 100) {
  throw new Error(
    `[destructive-confirm-wiring] 只扫到 ${FILES.length} 个源文件 —— 扫描面已塌，本守卫失去判据`,
  );
}
for (const a of ANCHORS) {
  if (!FILES.some((f) => f.rel === a)) {
    throw new Error(`[destructive-confirm-wiring] 锚点文件缺失：${a} —— 被改名/移走了？先修判据面再谈绿`);
  }
}

function get(rel: string): string {
  const hit = FILES.find((f) => f.rel === rel);
  if (!hit) throw new Error(`取材失败：${rel}`);
  return hit.src;
}

/** 去注释（本仓注释习惯逐字引用被替换掉的旧形态，直接扫原文会被自己的说明文字误伤）。 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

/** 取 `const <name> = useCallback(` 到列 2 缩进的收尾 `);` 为止的函数体（同 nodes-speedtest-wiring.test.ts）。 */
function callbackBody(src: string, name: string): string {
  const anchor = `const ${name} = useCallback(`;
  const start = src.indexOf(anchor);
  expect(start, `锚点消失，守卫已失去判据: ${anchor}`).toBeGreaterThan(-1);
  const rest = src.slice(start);
  const end = rest.indexOf('\n  );');
  expect(end, `找不到 ${name} 的 useCallback 收尾`).toBeGreaterThan(-1);
  return rest.slice(0, end);
}

describe('守卫自检：判据面真的存在（防空转恒绿）', () => {
  it('去注释后仍剩下可断言的代码（防 code() 把源码整段吃掉 → 负向断言恒绿）', () => {
    for (const a of ANCHORS) {
      const raw = get(a);
      expect(code(raw).length, `${a} 去注释后几乎为空`).toBeGreaterThan(raw.length / 4);
    }
    // 注释确实被剥掉了（否则下面的负向断言会被说明文字误伤，等于没接线）。
    expect(code(get('lib/confirm-twice.ts'))).not.toContain('原型');
  });
});

describe('T1：全仓只有一份原地二次确认实现（三套写法收敛）', () => {
  const OWNER = 'lib/confirm-twice.ts';

  it('2600ms 这个字面量只许出现在 confirm-twice.ts', () => {
    // 变异对照：把 CLEAR_CONFIRM_MS = 2600 写回 LogsScreen（原状形态）→ 本条转红。
    const offenders = FILES.filter((f) => f.rel !== OWNER && /\b2600\b/.test(code(f.src))).map(
      (f) => f.rel,
    );
    expect(offenders, '又出现了自己写的 2.6s 超时 —— 请消费 CONFIRM_TWICE_MS').toEqual([]);
  });

  it('确认态的 setTimeout 只许由 confirm-twice.ts 起（别处不得自管定时器）', () => {
    // 命中形态：同一表达式里既有 setTimeout 又在改 confirm/confirming 状态。
    const re = /setTimeout\([^)]*[Cc]onfirm/;
    const offenders = FILES.filter((f) => f.rel !== OWNER && re.test(code(f.src))).map((f) => f.rel);
    expect(offenders, '确认复位定时器散落在别处（第三套写法的原状形态）').toEqual([]);
  });

  it('`.confirming` 类的消费点全部来自 useConfirmTwice 的 armed（不得再有本地 useState 确认位）', () => {
    // 原状形态：`const [confirmingAll, setConfirmingAll] = useState(false)`（ConnectionsScreen）、
    // `const [confirmClear, setConfirmClear] = useState(false)`（LogsScreen）。
    // 大小写都收（`confirmX` / `confirmingAll`），`set` 那半单独锚 —— 只锚前半会把
    // `const [confirmed, setSomethingElse]` 这种无关解构也算进来。
    const re = /const\s*\[\s*confirm\w*\s*,\s*set[Cc]onfirm\w*\s*\]\s*=\s*useState/i;
    const offenders = FILES.filter((f) => re.test(code(f.src))).map((f) => f.rel);
    expect(offenders, '又出现了组件私有的确认位 state —— 会再长出一套复位语义').toEqual([]);
  });

  it('confirm-twice.ts 自己扛住三条原型语义（超时常量 / 二次点击先清 timeout / 卸载清理）', () => {
    const c = code(get(OWNER));
    expect(c).toContain('CONFIRM_TWICE_MS = 2600');
    expect(c, '第二次点击必须先清 timeout 再执行 action').toMatch(
      /if \(armed === key\) \{\s*clear\(\);/,
    );
    expect(c, 'React 下不清定时器 = 在已卸载组件上 setState').toMatch(
      /useEffect\(\(\) => \(\) => coreRef\.current\?\.dispose\(\), \[\]\)/,
    );
  });
});

describe('T2：原型走 confirmTwice 的破坏性操作，本仓一律原地二次点击、不弹窗', () => {
  /** 屏 → 该屏必须消费共用实现，且**整份文件**不得再开 confirm 弹窗。 */
  const WHOLE_FILE_CLEAN = [
    ['components/screens/logs/LogsScreen.tsx', '清空日志（原型 :4130 log-clear）'],
    [
      'components/screens/connections/ConnectionsScreen.tsx',
      '关闭全部 / 关闭筛选命中（原型 :4113 / :4114）',
    ],
    ['components/screens/settings/SettingsAbout.tsx', '完全卸载（原型 :5185 uninstallApp）'],
    ['components/screens/settings/SettingsHelper.tsx', '卸载提权助手（原型 :4234 helper-uninstall）'],
    ['components/layout/PendingChangesBar.tsx', '重置全部待应用改动（原型 :4070 reset-pending）'],
    [
      'components/screens/resources/ResourcesScreen.tsx',
      '删除资源 / 重置内置资源（原型 :4217 res-del / :4198 geo-reset）',
    ],
    ['components/screens/apppolicy/AppPolicyScreen.tsx', '移除自定义应用（原型 :4173 app-remove）'],
    ['components/screens/rules/RulesScreen.tsx', '规则列表行内删除（原型 :4097 rule-del）'],
  ] as const;

  for (const [rel, what] of WHOLE_FILE_CLEAN) {
    it(`${rel} 消费 useConfirmTwice 且零 confirm 弹窗 —— ${what}`, () => {
      const c = code(get(rel));
      expect(c, '没有消费共用实现').toContain("from '@/lib/confirm-twice'");
      expect(c).toMatch(/useConfirmTwice\(\)/);
      // 变异对照：把任一处改回 openDialog({ kind: 'confirm', ... }) → 本条转红。
      expect(c, '破坏性操作又走回弹窗了').not.toContain("kind: 'confirm'");
    });
  }

  it('NodesScreen 的删节点 / 批删走原地二次点击（原型 :4140 node-del / :4137 batch-del）', () => {
    const c = code(get('components/screens/nodes/NodesScreen.tsx'));
    expect(c).toContain("from '@/lib/confirm-twice'");
    for (const name of ['requestDelete', 'deleteBatch']) {
      const body = callbackBody(c, name);
      expect(body, `${name} 必须走 confirmTwice`).toMatch(/confirmTwice\(/);
      // 变异对照：把 confirmTwice(...) 换回 openDialog({ kind:'confirm' }) → 本条转红。
      expect(body, `${name} 又开回了确认弹窗`).not.toContain("kind: 'confirm'");
    }
    // 卡上那颗图标按钮的确认态必须真接到 armed 上（只加 state 不传下去 = 按钮永远不翻红）。
    expect(c).toMatch(/deleteConfirming=\{confirmArmed === `node-del:\$\{server\.id\}`\}/);
    const card = code(get('components/screens/nodes/NodeCard.tsx'));
    expect(card, '删除按钮没有按 deleteConfirming 挂 .confirming').toMatch(
      /cn\('nd-a err', deleteConfirming && 'confirming'\)/,
    );
  });

  /**
   * 这两份文件**不能**进 WHOLE_FILE_CLEAN：它们各自还留着与破坏性操作无关的 `kind:'confirm'`
   * （RuleDialog 的「放弃更改？」脏态确认 —— 原型无此形态、实现单方面新增且方向更好；
   * SettingsUpdate 的「恢复出厂内核」等三处 —— 需要成段解释的副作用）。
   * 逐文件计数由 T3 精确锁死（回退成弹窗 ⇒ 计数涨 ⇒ 转红），此处只钉「确实消费了共用实现」。
   */
  const HAS_CONFIRM_TWICE = [
    ['components/dialogs/RuleDialog.tsx', '删除此规则（原型 :4095 rule-del-dlg）'],
    ['components/screens/settings/SettingsUpdate.tsx', '内核回滚（原型 :4075 core-rollback）'],
  ] as const;

  for (const [rel, what] of HAS_CONFIRM_TWICE) {
    it(`${rel} 消费 useConfirmTwice —— ${what}`, () => {
      const c = code(get(rel));
      expect(c, '没有消费共用实现').toContain("from '@/lib/confirm-twice'");
      expect(c).toMatch(/useConfirmTwice\(\)/);
      expect(c, '武装态必须真的接到 armed 上（只调 confirmTwice 不渲染 = 隐形闸门）').toMatch(
        /armed === [A-Z_]+_KEY/,
      );
    });
  }

  /**
   * 武装态必须**真的渲染出来** —— 只调 `confirmTwice` 而按钮不挂 `.confirming`，用户点第一下
   * 得到的是「毫无反应」，2.6s 内再点一下东西就没了：那不是闸门，是延时地雷。
   *
   * 上面几条只断言「消费了共用实现」，挡不住这一类：实测把 `AppPolicyScreen` 的
   * `cn('app-remove', confirming && 'confirming')` 改回裸 `"app-remove"`，tsc 与全部既有断言**照常全绿**。
   * 故按文件逐个钉住那句 class 绑定的源码形态。
   */
  const ARMED_RENDER: readonly (readonly [string, RegExp])[] = [
    ['components/layout/PendingChangesBar.tsx', /arming && 'confirming'/],
    ['components/screens/resources/ResourcesScreen.tsx', /armed === GEO_RESET_KEY && 'confirming'/],
    ['components/screens/resources/ResourcesScreen.tsx', /deleteConfirming && 'confirming'/],
    ['components/screens/apppolicy/AppPolicyScreen.tsx', /confirming && 'confirming'/],
    ['components/dialogs/RuleDialog.tsx', /armed === RULE_DEL_KEY && 'confirming'/],
    // 规则行内删除：状态在 RulesScreen（单槽），翻红在 RuleItem —— 两段都要钉，只钉一头就留下
    // 「state 有了但没传下去」或「传下去了但没挂 class」两种隐形闸门（同 NodesScreen / NodeCard 那对）。
    ['components/screens/rules/RuleItem.tsx', /cn\('nd-a err', deleteConfirming && 'confirming'\)/],
    [
      'components/screens/settings/SettingsUpdate.tsx',
      /armed === CORE_ROLLBACK_KEY && 'confirming'/,
    ],
  ] as const;

  it('本批 7 处武装态都挂上了 `.confirming`（不留隐形闸门）', () => {
    const offenders = ARMED_RENDER.filter(([rel, re]) => !re.test(code(get(rel)))).map(
      ([rel, re]) => `${rel} :: ${re.source}`,
    );
    expect(offenders, '按钮没按 armed 挂 .confirming —— 第一下点击零反馈').toEqual([]);
  });

  /**
   * 规则行内删除的两段接线（原型 :4097 `rule-del`）。上面那条只钉住 `RuleItem` 里的 class 绑定，
   * 挡不住「状态有了但根本没传下去」——那种情况按钮永远不翻红，用户点两下就把规则删了。
   *
   * 变异对照：摘掉 `deleteConfirming={...}` 或 `onDelete={requestDelete}` 任一 → 本条转红。
   */
  it('RulesScreen 把 armed / 删除回调都真的传给了 RuleItem（原型 :4097 rule-del）', () => {
    const c = code(get('components/screens/rules/RulesScreen.tsx'));
    expect(c, '行内删除没走 confirmTwice').toMatch(/confirmTwice\(`\$\{RULE_DEL_PREFIX\}\$\{rule\.id\}`/);
    expect(c, '删除回调没接到行上（按钮整个不渲染）').toContain('onDelete={requestDelete}');
    expect(c, '武装态没传下去 —— 按钮永远不翻红').toContain(
      'deleteConfirming={armed === `${RULE_DEL_PREFIX}${rule.id}`}',
    );
  });
});

describe('T3：确认弹窗的存量清册（新增一处必须显式登记，不许悄悄长）', () => {
  /**
   * `kind: 'confirm'` 的**全仓**分布快照，`文件 → 出现次数`。
   *
   * `ConfirmDialog` 本身**不退役**：它服务的是「需重启才生效」「放弃未保存的更改」「换代理模式前置
   * 忠告」这类**需要成段解释**、原型也没有对应 confirmTwice 调用点的确认。退役的只是把它包成
   * `Promise<boolean>` 的破坏性操作闸门 `dialogConfirm`（已删）。
   *
   * ✅ **上一轮登记的 4 条债务已于 2026-07-29 第二批清零**（`res-del` / `geo-reset` / `app-remove` /
   * `rule-del-dlg` 全部改为原地二次点击）⇒ ResourcesScreen 与 AppPolicyScreen 整份出表，
   * RuleDialog 由 2 降到 1（剩下的那处是「放弃更改？」脏态确认，非破坏性操作）。
   * 同批把 3 处**确认整个缺席**的补上（`reset-pending` / `geo-reset` / `core-rollback`），
   * 其中 `core-rollback` 落在 SettingsUpdate，它的 3 处弹窗都与回滚无关，故计数不变。
   *
   * NodesScreen 余下的 2 处（删订阅 `requestSubDelete` / 注销 WARP `removeWarpNode`）**不是**债务：
   * 原型里没有对应的 confirmTwice 调用点，属本仓自加的确认，维持弹窗。
   *
   * 数字只许降不许升：新增一处弹窗式确认 ⇒ 本表对不上 ⇒ 转红，必须显式登记并说明为什么不能原地确认。
   */
  const CENSUS: Record<string, number> = {
    'components/dialogs/AppAddDialog.tsx': 1,
    'components/dialogs/BackupImportDialog.tsx': 1,
    'components/dialogs/ImportDialog.tsx': 1,
    'components/dialogs/NodeDialog.tsx': 1,
    'components/dialogs/ResUrlDialog.tsx': 1,
    'components/dialogs/RuleDialog.tsx': 1,
    'components/dialogs/SubDialog.tsx': 1,
    'components/dialogs/TsLoginDialog.tsx': 1,
    'components/dialogs/TsSettingsDialog.tsx': 1,
    'components/dialogs/WarpDialog.tsx': 1,
    'components/dialogs/WgDialog.tsx': 1,
    'components/dialogs/dialog-store.ts': 1, // union 的类型声明，非调用点
    'components/screens/home/HomeScreen.tsx': 2,
    'components/screens/nodes/NodesScreen.tsx': 2,
    'components/screens/settings/SettingsDns.tsx': 1,
    // 清理系统代理会改系统级网络状态，且用户明确要求二次确认；需要成段说明风险，不适合原地双击。
    'components/screens/settings/SettingsNetwork.tsx': 1,
    'components/screens/settings/SettingsUpdate.tsx': 3,
    'components/screens/settings/useConfig.ts': 1,
  };

  it('清册与磁盘现状逐文件相等（多一处 / 少一处都说话）', () => {
    const actual: Record<string, number> = {};
    for (const f of FILES) {
      const n = code(f.src).split("kind: 'confirm'").length - 1;
      if (n > 0) actual[f.rel] = n;
    }
    // 相等而非 `<=`：涨了 = 有人新加了弹窗式确认；跌了 = 又消掉一处，把清册一并调低。
    expect(actual, '确认弹窗分布与清册不符 —— 见本 describe 头注的登记规则').toEqual(CENSUS);
  });

  it('已退役的 dialogConfirm 不许回来（它是破坏性操作那条弹窗腿的入口）', () => {
    const offenders = FILES.filter((f) => /dialogConfirm|confirm-gate/.test(code(f.src))).map(
      (f) => f.rel,
    );
    expect(offenders, 'dialogConfirm / confirm-gate 已删除，不得复活').toEqual([]);
  });
});
