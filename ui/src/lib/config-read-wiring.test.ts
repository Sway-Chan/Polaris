/**
 * UserConfig **读**点接线守卫 —— 钉死「每一个从 app-store 读 `config` 的点都被显式分过类」。
 *
 * # 为什么读侧也要一盏红灯
 *
 * `config-write-wiring.test.ts` 管的是「写下去的字节落哪」。读侧现在有一条完全对称的静默风险：
 * `store/app-store.ts` 的 `config` 是**磁盘真值**，暂存层一开，用户改完节点、对话框关掉，
 * 任何还读裸 `config` 的展示点都会继续显示旧值 —— 那不是「延迟生效」，是 UI 撒谎
 * （与 spec §2.5 Q3 否决「切节点进暂存」的理由完全同型：列表高亮 A、状态栏显示 A，流量走 B）。
 *
 * 而这条风险**没有任何门会抓**：新增一个 `useAppStore((s) => s.config?.xxx)` 的展示点、
 * 忘了改成 `useEffectiveConfig`，类型对、测试绿、构建过。本文件就是那盏红灯。
 *
 * # 判据面与登记粒度
 *
 * 判据面 = 前端能读到 app-store 那份 `config` 的**全部**形态（下方 `hitsIn`）：
 * `useAppStore(<selector 读 s.config>)`、`useAppStore.getState().config`、
 * 以及 store 内部的 `const { config } = get()`。每一个扫到的 `(文件, 形态)` 必须在 `SITES` 里有一行
 * 并带一个说得出因由的去向；表里有、树上没有 ⇒ 也红（陈旧登记会让人以为某个读点还在被管辖）。
 *
 * **保留裸 `config` 的唯一理由是「这个读点要回答的是关于磁盘的问题」**：暂存层自身的基准、
 * 落盘/起核路径的入参、与后端对账。其余展示可编辑实体的读点一律经 `useEffectiveConfig`。
 *
 * # 守的是形态不是措辞
 *
 * 断言落在「哪个文件用哪种形态读了 `config`」这类结构事实上；改注释、改文案不会误伤，
 * 新增/挪走一个读点则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { isBypassedConfigKey } from './staged-config';

const SRC = fileURLToPath(new URL('..', import.meta.url));

// ─────────────────────────────── 扫描器 ───────────────────────────────

/**
 * 去掉注释、**但保留行号**（把注释体换成等量空白）。与写侧守卫同一份实现理由：本仓注释习惯逐字
 * 引用被禁的旧形态（本文件头就写着 `useAppStore((s) => s.config?.xxx)`），扫原文会被说明文字误伤；
 * 反过来只在注释里提一句 `useEffectiveConfig` 就能让正向断言变绿，那是假绿。
 */
function code(src: string): string {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
    .replace(/(^|[^:])\/\/.*$/gm, (m, p1: string) => p1 + ' '.repeat(m.length - p1.length));
}

function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) sourceFiles(p, out);
    else if (/\.tsx?$/.test(e.name) && !/\.(test|spec)\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

/**
 * 从 `useAppStore` 这个 token 起，取出**整个调用表达式**（括号配平）+ 其后的成员链。
 *
 * 为什么不用正则：`useAppStore((s) => s.config?.uiTheme)` 这类实参里嵌套括号，`[^)]*` 会在
 * 第一个 `)` 截断 —— 截断处恰好可能把 `.config` 切掉，得到一条**永远扫不到东西的判据面**
 * （空集合 + 恒真断言，正是本守卫要防的那种假绿）。配平扫描没有这个失效模式。
 *
 * 返回 `null` = 这不是一次调用（`import { useAppStore }` / `typeof useAppStore`）：
 * token 与第一个 `(` 之间只允许成员访问，否则会一路吃到文件下方某个无关的括号。
 */
function callExprAt(src: string, start: number): string | null {
  let i = start;
  while (i < src.length && /[\s.\w$]/.test(src[i])) {
    if (src[i] === '(') break;
    i += 1;
  }
  if (src[i] !== '(') return null;
  let depth = 0;
  let j = i;
  for (; j < src.length; j += 1) {
    if (src[j] === '(') depth += 1;
    else if (src[j] === ')') {
      depth -= 1;
      if (depth === 0) {
        j += 1;
        break;
      }
    }
  }
  if (depth !== 0) return null;
  // 尾随成员链（`useAppStore.getState()` 后面的 `.config`）
  const tail = /^(?:\s*\??\.\s*\w+)*/.exec(src.slice(j));
  return src.slice(start, j + (tail?.[0].length ?? 0));
}

interface Hit {
  readonly file: string;
  /** 形态标识（去空白后截断），`(文件, 形态)` 是本守卫的登记粒度。 */
  readonly shape: string;
  readonly line: number;
}

const TOKEN = /\b(?:useAppStore|useEffectiveServers|useEffectiveRules)\b/g;
/** store 内部读法：`const { config } = get()` / `= useAppStore.getState()`。 */
const DESTRUCTURE = /\{[^{}]*\bconfig\b[^{}]*\}\s*=\s*(?:\w+\s*\.\s*)*get(?:State)?\s*\(\s*\)/g;
/** 判据：表达式里出现 `.config` 本身（`\b` 天然排除 `.configLoading` / `.configError`）。 */
const READS_CONFIG = /\.\s*config\b/;
/**
 * 判据：表达式里出现 `.servers` / `.rules` —— app-store 的两个**扁平磁盘镜像**字段
 * （`loadConfig`/`saveConfig` 从 `config.servers` / `config.customRules` 抄出来的那两份）。
 *
 * 它们必须与 `.config` 分成两张表：节点页 / 规则页 / 首页出口选单渲染的是镜像，不是 `config`，
 * 故只把 `.config` 读点接上 `effectiveConfig` 并不会让列表回显 staged 编辑。
 */
const READS_MIRROR = /\.\s*(?:servers|rules)\b/;
/** 派生 hook 的**定义**行不是读点（`export function useEffectiveServers()`），别把它算进判据面。 */
const IS_DECLARATION = /\bfunction\s+$/;

/** 形态归一：去空白 + 截断到可读长度。行号漂移不进 key（否则守卫变成每次改动都要重刷的噪音表）。 */
function shapeOf(expr: string): string {
  const flat = expr.replace(/\s+/g, '');
  return flat.length > 60 ? `${flat.slice(0, 60)}…` : flat;
}

function lineOf(src: string, idx: number): number {
  return src.slice(0, idx).split('\n').length;
}

interface Scan {
  readonly hits: Hit[];
  /** 镜像字段（`servers` / `rules`）的读点，含派生 hook `useEffectiveServers/Rules()` 的调用点。 */
  readonly mirrorHits: Hit[];
  /** 全部 `useAppStore(...)` 调用表达式数（含不读 config 的）—— 扫描器自检的分母。 */
  readonly exprs: number;
  /** 整份 state 别名（`const s = useAppStore.getState();`）：`.config` 能从这里隐形溜走。 */
  readonly aliases: { file: string; name: string; line: number }[];
}

function scan(files: string[]): Scan {
  const hits: Hit[] = [];
  const mirrorHits: Hit[] = [];
  const aliases: Scan['aliases'] = [];
  let exprs = 0;
  for (const p of files) {
    const rel = p.slice(SRC.length).split(/[\\/]/).join('/');
    const src = code(readFileSync(p, 'utf8'));
    for (const m of src.matchAll(TOKEN)) {
      const expr = callExprAt(src, m.index!);
      if (expr === null) continue;
      if (IS_DECLARATION.test(src.slice(0, m.index!))) continue;
      exprs += 1;
      const hit = { file: rel, shape: shapeOf(expr), line: lineOf(src, m.index!) };
      if (/^useEffective/.test(expr)) {
        mirrorHits.push(hit);
        continue;
      }
      if (READS_MIRROR.test(expr)) mirrorHits.push(hit);
      if (READS_CONFIG.test(expr)) {
        hits.push(hit);
      }
    }
    for (const m of src.matchAll(DESTRUCTURE)) {
      hits.push({ file: rel, shape: shapeOf(m[0]), line: lineOf(src, m.index!) });
    }
    for (const m of src.matchAll(/\b(?:const|let)\s+(\w+)\s*=\s*useAppStore\s*\.\s*getState\s*\(\s*\)\s*;/g)) {
      aliases.push({ file: rel, name: m[1], line: lineOf(src, m.index!) });
    }
  }
  return { hits, mirrorHits, exprs, aliases };
}

const FILES = sourceFiles(SRC);
const { hits: HITS, mirrorHits: MIRROR_HITS, exprs: EXPRS, aliases: ALIASES } = scan(FILES);
const SOURCE = new Map(
  FILES.map((p) => [p.slice(SRC.length).split(/[\\/]/).join('/'), code(readFileSync(p, 'utf8'))])
);

/**
 * **自曝纪律**（模块加载期就抛，不留给断言）：扫空了 / 配平扫描被改坏 ⇒ 下面每一条断言都会
 * 空跑恒绿，而那正是本守卫要防的那种「没有任何门会转红」。抛出来比绿着更诚实。
 */
if (FILES.length < 100) throw new Error(`读侧守卫扫不到源码（只收到 ${FILES.length} 个文件）`);
if (EXPRS < 100) throw new Error(`读侧守卫只扫到 ${EXPRS} 个 useAppStore 调用，判据面已失配`);
if (HITS.length < 10) throw new Error(`读侧守卫只扫到 ${HITS.length} 个 config 读点，判据面已失配`);
if (MIRROR_HITS.length < 15)
  throw new Error(`读侧守卫只扫到 ${MIRROR_HITS.length} 个镜像读点，判据面已失配`);

// ─────────────────────────────── 去向登记表 ───────────────────────────────

/**
 * - `disk` —— **必须看磁盘**。唯一合法理由：这个读点回答的是关于磁盘的问题（暂存基准 /
 *   落盘·起核入参 / 与后端对账）。理由必须点名是哪一类。
 * - `derive` —— `effectiveConfig` 派生层自身的读点（它就是那层，不能读自己）。
 *
 * 上一轮的 `pending-handoff`（欠账）已**清零**：那 9 处读点所在的三个文件不再被别的改动独占，
 * 本轮逐条判完归位。连同钉住「把欠账伪装成 disk 即红」的 `HANDOFF_OWNED` 与 `T4` 一并删除
 * （两者都自陈「清零后删掉本段」）。欠账这条腿若日后重现，照原样加回来即可。
 */
type Route = 'disk' | 'derive';

interface Site {
  readonly file: string;
  readonly shape: string;
  readonly route: Route;
  /** 为什么是这个去向。`disk` 必须点名「基准 / 入参 / 对账」之一。 */
  readonly why: string;
}

const SITES: readonly Site[] = [
  // ── 派生层自身 ──
  {
    file: 'store/app-store.ts',
    shape: 'useAppStore((s)=>select(effectiveConfigOf(s.config,entries))\u2026',
    route: 'derive',
    why: 'useEffectiveConfig 本体：它就是把磁盘 config 与 staged 条目合成的那一层',
  },
  {
    file: 'store/app-store.ts',
    shape: 'useAppStore.getState().config',
    route: 'derive',
    why: 'getEffectiveConfig 本体（命令式读点），与 hook 共用同一记忆化槽',
  },

  // ── app-store 内部：落盘 / 起核入参 ──
  {
    file: 'store/app-store.ts',
    shape: '{config}=get()',
    route: 'disk',
    why: '入参类：updateProxyMode / startProxy / switchServer 三条腿——起核发给后端的是整份磁盘 config，切模式切节点写的也是磁盘那份；喂合成值会把用户还没保存的编辑一起送进核',
  },

  // ── 与后端对账 / 重放基准 ──
  {
    file: 'components/layout/PendingChangesBar.tsx',
    shape: 'useAppStore((s)=>s.config)',
    route: 'disk',
    why: '基准类：FR-9 逐条标注走 classifyStaged(replay(config,[e]))，问的是「只有这一条时要不要重启」，重放基准必须是盘',
  },
  {
    file: 'components/screens/logs/LogsScreen.tsx',
    shape: 'useAppStore((s)=>s.config)',
    route: 'disk',
    why: '入参类：saveConfig（改日志级别）的基准必须是盘，拿暂存合成值当基准会把未应用的暂存值一并落盘',
  },
  {
    file: 'components/screens/home/HomeScreen.tsx',
    shape: 'useAppStore((s)=>s.config)',
    route: 'disk',
    why: '入参类：onPickDirectExit / applyIntercept 两条 W-1·W-2 直落盘腿的基准（展示口径已改读 useEffectiveConfig）',
  },
  {
    file: 'components/screens/settings/SettingsDisplay.tsx',
    shape: 'useAppStore.setState((s)=>(s.config?{config:{...s.config,uiT\u2026',
    route: 'disk',
    why: '基准类：把 uiTheme 同步进 app-store 那份**磁盘镜像**，读的必须是被改的那个对象本身',
  },

  // ── 上一轮的欠账，本轮判完归位（原 pending-handoff 9 处）──
  {
    file: 'components/dialogs/AppAddDialog.tsx',
    shape: 'useAppStore.setState((s)=>(s.config?{config:{...s.config,cus\u2026',
    route: 'disk',
    why: '基准类：直落盘腿写成功后把 customAppPresets 同步进 app-store 那份**磁盘镜像**，读的必须是被改的那个对象本身（展示口径已改读 useEffectiveConfig）',
  },
  {
    file: 'components/screens/apppolicy/AppPolicyScreen.tsx',
    shape: 'useAppStore.setState((s)=>(s.config?{config:{...s.config,app\u2026',
    route: 'disk',
    why: '基准类：直落盘腿写成功后把 appRules / appRoutingEnabled 同步进磁盘镜像（同形态归一行），读的必须是被改的那个对象本身',
  },
  {
    file: 'components/screens/apppolicy/AppPolicyScreen.tsx',
    shape: 'useAppStore.setState((s)=>(s.config?{config:{...s.config,cus\u2026',
    route: 'disk',
    why: '基准类：同上，customAppPresets 那条腿',
  },
  {
    file: 'components/screens/rules/RulesScreen.tsx',
    shape: 'useAppStore.getState().config',
    route: 'disk',
    why: '入参类：handleRegionChange 的 saveConfig 整份覆盖落盘，喂 effective 会把用户还没保存的编辑一起写进去',
  },
];

// ─────────────── 镜像字段（servers / rules）的展示面·操作面登记表 ───────────────

/**
 * # 为什么镜像要单独一张表，而不是并进 `SITES`
 *
 * `SITES` 管的是「这个读点该看磁盘还是看合成值」，答案由**读点自己**决定。镜像上是**同一个集合的
 * 两个不同问题**，答案由**这个集合被拿去干什么**决定：
 *
 * | 读点在问什么 | 读谁 | 反过来会怎样 |
 * |---|---|---|
 * | `display`：用户**现在编辑出来**的实体集合长什么样（列表、编辑基准、下拉枚举） | `useEffectiveServers/Rules()` | 用户看不见自己刚做的编辑 ⇒ UI 撒谎 |
 * | `operation`：**后端此刻能按 id 找到 / 正在使用**的实体集合长什么样 | 裸镜像 `s.servers` / `s.rules` | staged-only 实体不在盘上，后端按 id 查不到 ⇒ 当场被拒 |
 *
 * ⇒ staged-only 实体**在列表里可见且带「待保存」标记**（判据 `stagedOnlyIds` = effective − disk），
 * **不进出口选单**。这与 pending-bar 的「N 项待保存」是同一语义面，不是新概念。
 *
 * # 为什么带 `count`
 *
 * `SITES` 的粒度是 `(文件, 形态)`：同形态两处读点会归成一行。对 `.config` 那张表无所谓（同形态同去向），
 * 对镜像**不成立** —— 同一个文件里「渲染列表」与「喂给后端的 id 集」可以写成一模一样的形态却分属两面
 * （`WgDialog` 就有两处同形态读点）。计数让「悄悄多加一处同形态读点」也转红。
 */
type Surface = 'display' | 'operation' | 'derive';

interface MirrorSite {
  readonly file: string;
  readonly shape: string;
  /** 该 (文件, 形态) 在树上的读点条数。 */
  readonly count: number;
  readonly surface: Surface;
  /**
   * 为什么归这一面。
   * `operation` 必须点名它喂的那条立即腿（按 id / 切节点 / 起核 / 对账 / 镜像自身）；
   * `display` 必须说清它渲染或编辑的是什么。
   */
  readonly why: string;
}

const MIRROR_SITES: readonly MirrorSite[] = [
  // ── 派生层自身 ──
  {
    file: 'store/app-store.ts',
    shape: "useAppStore((s)=>effectiveCollection('servers',s.servers,ent…",
    count: 1,
    surface: 'derive',
    why: 'useEffectiveServers 本体：它就是把磁盘镜像与 staged 条目合成的那一层',
  },
  {
    file: 'store/app-store.ts',
    shape: "useAppStore((s)=>effectiveCollection('customRules',s.rules,e…",
    count: 1,
    surface: 'derive',
    why: 'useEffectiveRules 本体，与上一行同层',
  },

  // ── 操作面：后端按 id 查盘 / 运行核当前在用 ──
  {
    file: 'App.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '按 id 下发：喂 api.server.tailscaleStateExists(ids)，后端按 id 去磁盘 state 目录查「登录过没」，盘上没有的 id 无从查起',
  },
  {
    file: 'components/dialogs/TsLoginDialog.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '按 id 下发：planTsLoginSubmit 判完就直接 api.server.add/update 落盘并起登录核（该腿恒绕过暂存），基准必须是盘上那份',
  },
  {
    file: 'components/layout/PendingChangesBar.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：pendingDiff 的节点 id 来自后端（startup_snapshot ↔ 磁盘），名字必须在同一侧解析，否则条上会出现盘上不存在的名字',
  },
  {
    file: 'components/layout/StatusBar.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '运行核当前在用：解析 selectedServerId 指向的出口节点，与同屏的出口 IP / 延迟 / 状态点同一语义面（选节点恒走 W-1 直落盘）',
  },
  {
    file: 'components/screens/home/HomeScreen.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '切节点（W-1 立即腿）：出口选单点下去就是 api.server.switch(id)，后端按 id 在磁盘 servers 里查不到即拒；同一集合还喂全量测速的候选 id',
  },
  {
    file: 'components/screens/shared/use-switch-node.ts',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '切节点（W-1 立即腿）本体：首页出口选单与节点页卡片共用这一条，`api.server.switch(id)` 的后端按 id 在磁盘 servers 里查不到即拒 —— 拿合成值取名会让 toast 报出一个后端根本不认的节点',
  },
  {
    file: 'components/screens/home/TsExitWarning.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '运行核当前在用：判的是当前出口那个 TS 节点有没有配出口设备，并与活态 peers 对照（W-2），喂合成值会让警示与活态永久打架',
  },
  {
    file: 'components/screens/nodes/NodesScreen.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：**只**用来算 staged-only 差集的磁盘侧（stagedOnlyIds 的第二个入参），不参与渲染集合本身',
  },
  {
    file: 'components/screens/rules/RulesScreen.tsx',
    shape: 'useAppStore((s)=>s.rules)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：同上，规则那一侧的 staged-only 差集磁盘入参',
  },
  {
    file: 'components/screens/rules/RulesScreen.tsx',
    shape: 'useAppStore.getState().rules',
    count: 2,
    surface: 'operation',
    why: '镜像自身：拖拽重排 / 开关切换那两次乐观 setState 的回滚基准 —— 改的是镜像这个对象，读的就得是它',
  },
  {
    file: 'App.tsx',
    shape: 'useAppStore.getState().servers.map',
    count: 1,
    surface: 'operation',
    why: '按 id 下发（测速中断后「继续」的续测集过滤）：喂 api.server.speedTest(ids)，后端按 id 在磁盘 servers 里查不到即判「请求了但配置里查无此节点」；且测速本就只能测**运行核/磁盘上**的节点，暂存未落盘的节点连入池都谈不上，用合成值过滤等于放一批必然缺席的 id 下去',
  },

  // ── 展示面：用户现在编辑出来的那份 ──
  {
    file: 'components/dialogs/ImportDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '导入的单例槽判据（admitMeshSingletons）：漏掉暂存节点就能再导一个 WARP/TS，重放后配置非法',
  },
  {
    file: 'components/dialogs/NodeDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '编辑基准 + 单例槽判据：读盘的话暂存过的节点再打开显示的是改前旧值',
  },
  {
    file: 'components/dialogs/RuleDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '规则目标下拉的节点枚举：选中只写进本条规则，不触发任何按 id 查盘的后端调用',
  },
  {
    file: 'components/dialogs/RuleDialog.tsx',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '外层取编辑基准（暂存过的规则再打开必须显示暂存后的值）；差集那处已随删除腿抽进 lib/use-rule-delete.ts',
  },
  // 2026-07-30：删除腿抽成 `useRuleDelete`（列表行内垃圾桶 + 规则弹窗 footer 共用），
  // staged-only 差集的两个入参跟着搬过来 —— 它们是那条腿自己的判据，不是调用方的关切。
  {
    file: 'lib/use-rule-delete.ts',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '删除腿 staged-only 差集的 effective 侧判据：问的是「用户现在这套规则里，哪几条盘上还没有」',
  },
  {
    file: 'components/dialogs/RulePickDialog.tsx',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '「加入已有规则」的候选枚举：列的是用户现在这套规则（暂存中新建的也必须能当追加目标），选中后写的仍是整条规则、不按 id 查盘',
  },
  {
    file: 'components/host-rule-menu.tsx',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '菜单排序判据（谁会先命中该域名）+ 追加腿的编辑基准；基准不同源会让追加从盘上旧值起算，把暂存中的编辑吞掉',
  },
  {
    file: 'lib/use-rule-delete.ts',
    shape: 'useAppStore((s)=>s.rules)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：删除腿的 staged-only 差集磁盘入参（盘上没有这条 ⇒ 删除改走撤销条目）',
  },
  {
    file: 'components/dialogs/TsSettingsDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 2,
    surface: 'display',
    why: '两处：外层取编辑基准（提交腿走暂存，基准不同源会让第二次编辑从盘上旧值起算）+ 表单里算 staged-only 差集的 effective 侧',
  },
  {
    file: 'components/dialogs/TsSettingsDialog.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：登出腿（block 策略）的 staged-only 差集磁盘入参',
  },
  {
    file: 'components/dialogs/WarpDialog.tsx',
    shape: 'useAppStore((s)=>s.servers)',
    count: 1,
    surface: 'operation',
    why: '与后端对账：applyWarpLicense / update 两条 block 腿的 staged-only 差集磁盘入参',
  },
  {
    file: 'components/dialogs/WarpDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '编辑基准 + WARP 单例槽判据：含暂存节点更保守（暂存里已有 WARP 就不再发远端注册，W-3 不可逆）',
  },
  {
    file: 'components/dialogs/WgDialog.tsx',
    shape: 'useEffectiveServers()',
    count: 2,
    surface: 'display',
    why: '两处：表单里的组网单例槽判据 + 外层的编辑基准，同为「用户现在看到的节点集」',
  },
  {
    file: 'components/screens/apppolicy/AppPolicyScreen.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '应用策略 pill 上的目标节点名映射（纯渲染，不喂任何按 id 查盘的调用）',
  },
  {
    file: 'components/screens/nodes/NodesScreen.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '节点列表本体 —— 「列表不回显 staged 编辑」这条缺口在本屏的落点',
  },
  {
    file: 'components/screens/resources/ResourcesScreen.tsx',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '资源引用徽章：哪些规则引用了该资源，问的是用户现在这套规则',
  },
  {
    file: 'components/screens/rules/RulesScreen.tsx',
    shape: 'useEffectiveRules()',
    count: 1,
    surface: 'display',
    why: '规则列表本体 —— 「列表不回显 staged 编辑」这条缺口在本屏的落点',
  },
  {
    file: 'components/screens/rules/RulesScreen.tsx',
    shape: 'useEffectiveServers()',
    count: 1,
    surface: 'display',
    why: '规则目标节点名映射（纯渲染，不喂任何按 id 查盘的调用）',
  },
];

// ─────────────────────────────── 断言 ───────────────────────────────

const key = (s: { file: string; shape: string }) => `${s.file} | ${s.shape}`;

describe('守卫自检：扫到的确实是源码（防扫空 / 配平扫描被改坏 → 断言恒真）', () => {
  it('测试文件被排除在扫描面外', () => {
    expect(FILES.some((p) => /\.(test|spec)\.tsx?$/.test(p))).toBe(false);
  });

  it('配平扫描没被括号截断：已知含嵌套括号的读点必须被完整取到', () => {
    // `useAppStore((s) => s.config?.…)` 里 `.config` 在第一个 `)` **之后**；用 `[^)]*` 正则
    // 会把它切掉 ⇒ 命中数骤降而断言仍绿。这条锚点专抓那种失配。
    const found = new Set(HITS.map(key));
    expect(found.has('components/layout/PendingChangesBar.tsx | useAppStore((s)=>s.config)')).toBe(
      true
    );
    expect(found.has('store/app-store.ts | {config}=get()')).toBe(true);
  });

  it('没有人把 useAppStore 改名导入（改名即绕过整条判据面）', () => {
    for (const [f, src] of SOURCE) {
      expect(src, `${f} 给 useAppStore 起了别名`).not.toMatch(
        /import\s*\{[^}]*\buseAppStore\s+as\s+/
      );
    }
  });
});

describe('T1：读点全登记（新增读 config 的路径 ⇒ 必须显式选一个去向）', () => {
  it('树上扫到的每一个 (文件, 形态) 都在登记表里', () => {
    const registered = new Set(SITES.map(key));
    const unregistered = [...new Set(HITS.map(key))].filter((k) => !registered.has(k)).sort();
    expect(
      unregistered,
      `以下读点未登记 —— 展示可编辑实体的一律改读 useEffectiveConfig，确需看磁盘的补进 SITES：\n${unregistered.join('\n')}`
    ).toEqual([]);
  });

  it('登记表里没有陈旧行（登记了却在树上找不到）', () => {
    const found = new Set(HITS.map(key));
    const stale = SITES.map(key).filter((k) => !found.has(k)).sort();
    expect(stale, `以下登记已陈旧（读点被删/已改读 effectiveConfig），删掉：\n${stale.join('\n')}`).toEqual(
      []
    );
  });

  it('登记表自身不重复（同一 (文件, 形态) 不得有两个去向）', () => {
    const seen = new Set<string>();
    const dup = SITES.map(key).filter((k) => (seen.has(k) ? true : (seen.add(k), false)));
    expect(dup).toEqual([]);
  });
});

describe('T2：白名单是数据不是借口（每条 disk 都得点名它在回答哪个磁盘问题）', () => {
  it('disk 的理由必须点名「基准 / 入参 / 对账」之一', () => {
    for (const s of SITES.filter((x) => x.route === 'disk')) {
      expect(s.why, `${key(s)} 的理由没点名任何磁盘语义`).toMatch(/基准|入参|对账/);
    }
  });

  it('每条登记都有非空理由', () => {
    for (const s of SITES) expect(s.why.length, key(s)).toBeGreaterThan(10);
  });
});

describe('T3：转换面真的在源码里（防登记表被整体降级成 disk 后本守卫空跑）', () => {
  const consumers = [...SOURCE].filter(([, src]) => /\buseEffectiveConfig\s*\(/.test(src));

  it('有一批展示点真的改读了 effectiveConfig', () => {
    expect(
      consumers.length,
      'useEffectiveConfig 的消费者掉到 10 个以下 —— 要么被批量退回读裸 config，要么派生层被绕过'
    ).toBeGreaterThan(10);
  });

  it('每个消费者都从 @/store/app-store 取它（不得各自复刻一份 replay）', () => {
    for (const [f, src] of consumers) {
      expect(src, `${f} 没从 app-store import useEffectiveConfig`).toMatch(
        /import\s*\{[^}]*\buseEffectiveConfig\b[^}]*\}\s*from\s*'(?:@\/store|\.\/|\.\.\/store)[^']*app-store'/
      );
    }
  });

  it('派生层单点：全仓只有 app-store 定义 effectiveConfigOf', () => {
    const definers = [...SOURCE].filter(([, src]) => /function\s+effectiveConfigOf\b/.test(src));
    expect(definers.map(([f]) => f)).toEqual(['store/app-store.ts']);
  });
});

describe('M1：镜像读点全登记（含条数 —— 同形态多加一处也要说话）', () => {
  const actual = new Map<string, number>();
  for (const h of MIRROR_HITS) actual.set(key(h), (actual.get(key(h)) ?? 0) + 1);

  it('树上扫到的每一个 (文件, 形态) 都在登记表里', () => {
    const registered = new Set(MIRROR_SITES.map(key));
    const unregistered = [...actual.keys()].filter((k) => !registered.has(k)).sort();
    expect(
      unregistered,
      `以下镜像读点未登记 —— 渲染/编辑基准改读 useEffectiveServers()/useEffectiveRules()，\n` +
        `喂给「后端按 id 查盘」的集合保留裸镜像并补进 MIRROR_SITES：\n${unregistered.join('\n')}`
    ).toEqual([]);
  });

  it('登记表里没有陈旧行', () => {
    const stale = MIRROR_SITES.map(key)
      .filter((k) => !actual.has(k))
      .sort();
    expect(stale, `以下登记已陈旧（读点被删/改形态），删掉：\n${stale.join('\n')}`).toEqual([]);
  });

  it('登记的条数与树上一致', () => {
    const drift = MIRROR_SITES.filter((s) => actual.get(key(s)) !== s.count).map(
      (s) => `${key(s)}：登记 ${s.count}，实际 ${actual.get(key(s)) ?? 0}`
    );
    expect(
      drift,
      '同形态的读点数变了 —— 新增的那处属于哪一面必须显式判一次，不能搭旧行的便车'
    ).toEqual([]);
  });

  it('登记表自身不重复', () => {
    const seen = new Set<string>();
    const dup = MIRROR_SITES.map(key).filter((k) => (seen.has(k) ? true : (seen.add(k), false)));
    expect(dup).toEqual([]);
  });
});

describe('M2：两面各自说得出因由，且形态与面结构一致', () => {
  it('operation 的理由必须点名它喂的那条立即腿', () => {
    for (const s of MIRROR_SITES.filter((x) => x.surface === 'operation')) {
      expect(s.why, `${key(s)} 的理由没点名任何立即腿`).toMatch(
        /按 id|切节点|起核|对账|镜像自身|运行核当前在用/
      );
    }
  });

  it('display 的理由必须说清渲染或编辑的是什么', () => {
    for (const s of MIRROR_SITES.filter((x) => x.surface === 'display')) {
      expect(s.why, `${key(s)} 没说清它展示的是什么`).toMatch(/列表|枚举|基准|映射|判据|徽章/);
    }
  });

  it('形态即面：useEffective* 不得记成 operation，裸镜像不得记成 display', () => {
    const wrong = MIRROR_SITES.filter((s) =>
      /^useEffective/.test(s.shape) ? s.surface === 'operation' : s.surface === 'display'
    ).map(key);
    expect(
      wrong,
      '读的是哪一份是结构事实，不能靠登记表的一句话改写：展示面必须真的调 useEffectiveServers/Rules()'
    ).toEqual([]);
  });

  it('每条登记都有非空理由', () => {
    for (const s of MIRROR_SITES) expect(s.why.length, key(s)).toBeGreaterThan(10);
  });
});

describe('M3：两个锚点（改错了这两处，上面的表格式断言仍会绿）', () => {
  it('出口选单所在的 HomeScreen 读的是磁盘镜像', () => {
    const row = MIRROR_SITES.find((s) => s.file === 'components/screens/home/HomeScreen.tsx');
    expect(row?.surface, '出口选单一旦读 effective，staged-only 节点就会进选单 → 切过去被后端拒').toBe(
      'operation'
    );
  });

  it('节点列表所在的 NodesScreen 有一处展示面读点', () => {
    const rows = MIRROR_SITES.filter(
      (s) => s.file === 'components/screens/nodes/NodesScreen.tsx' && s.surface === 'display'
    );
    expect(rows.length, '节点列表退回读磁盘镜像 = 用户看不见自己刚做的编辑').toBe(1);
  });

  it('`selectedServerId` 不在本判据面内，是因为它恒不入暂存（这条事实本身要有门）', () => {
    // 镜像有三个字段，本守卫只扫 servers / rules。第三个 `selectedServerId` 走 W-1 绕过腿
    // （BYPASS_TABLE 的 switchServer），永远不会有 staged 条目改它 ⇒ 镜像 ≡ 磁盘 ≡ effective，
    // 三者恒等就没有「该读哪一份」可判。这条前提一旦被摘掉，本守卫会漏掉一整个字段。
    expect(
      isBypassedConfigKey('selectedServerId'),
      'selectedServerId 不再绕过暂存 ⇒ 它的读点也要按展示面/操作面分类，本守卫的判据面必须扩到它'
    ).toBe(true);
  });
});

describe('T5：整份 state 别名不得成为隐形读点', () => {
  it('别名扫描器仍有命中（锚点：TsLoginDialog 的收尾 effect）', () => {
    expect(ALIASES.length, '别名扫描器归零 = 正则失配或该形态已消失，两者都要人看一眼').toBeGreaterThan(0);
  });

  it('拿到整份 state 的别名都没有从它读 config（读了就绕过了本守卫）', () => {
    for (const a of ALIASES) {
      const src = SOURCE.get(a.file)!;
      expect(
        src,
        `${a.file}:${a.line} 的 \`${a.name}\` 是整份 state 别名，从它读 config 会绕过判据面；改用 getEffectiveConfig() 或直接 useAppStore.getState().config`
      ).not.toMatch(new RegExp(`\\b${a.name}\\s*\\??\\.\\s*config\\b`));
    }
  });
});
