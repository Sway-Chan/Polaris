/**
 * 节点屏测速接线不变量守卫 —— 钉死「三个测速入口 + 卡上 ⚡ 同一条过滤线」的**接线形态**。
 *
 * 为什么必须是源码结构守卫（而不是又一组逻辑单测）：被守的缺陷根本不在算法里。
 * `isSpeedTestable` / `speedTestableIds` 一直是对的、也一直有单测（`domain/speed-testable-ids.test.ts`），
 * 但节点屏**零消费**它们，自己另起了一条弱判定 `lanOnly = allowInternet === false`：
 *  - reverseMesh（System 内核接口）的 WireGuard 照样显 ⚡ → dial 走 OS default → 测出**直连的假好值**，
 *    挂在组网节点名下；
 *  - 无 exitNode 的 Tailscale 照样显 ⚡ → 公网黑洞 → 必假超时（`-1` 在 UI 上读作「真实超时」而非「未测」）；
 *  - 页头「全部测速」`api.server.speedTest()` 连 id 集都不传 → 同一句「全部测速」在首页与本屏**不同义**。
 * 三条全是「谓词在、调用点没用」——逻辑单测全绿、缺陷照旧。故沿用本仓既有的源码不变量守卫模式
 * （`store/latency-wiring-invariants.test.ts`、`domain/speed-testable-ids.test.ts` 的消费面守卫段、
 * `i18n/locale-parity.test.ts`）。
 *
 * 守的是**形态**不是措辞：断言的都是「哪条腿调了哪个函数 / 状态存在哪」这类结构事实；
 * 改注释、改文案、改变量名不会误伤，把调用点换回弱判定则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const read = (rel: string): string =>
  readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

/**
 * 去掉注释后的源码 —— **所有断言都跑在它上面**，两个方向都必要：
 *  - 负向：本仓注释习惯逐字引用「被替换掉的旧形态」（`allowInternet === false`、`speedTest()`），
 *    直接扫原文会被自己的说明文字误伤；
 *  - 正向：只在注释里提一句 `speedTestableIds` 就能让 `toContain` 变绿 —— 那是假绿，
 *    守卫要守的是**代码真的这么写**，不是文档里这么说。
 * `[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const NODES_RAW = read('./NodesScreen.tsx');
const CARD_RAW = read('./NodeCard.tsx');
const NODES = code(NODES_RAW);
const CARD = code(CARD_RAW);

/** 取顶层 `const <name> = useCallback(` 到其收尾 `);`（列 2 缩进）为止的函数体（同 speed-testable-ids.test.ts）。 */
function callbackBody(src: string, name: string): string {
  const anchor = `const ${name} = useCallback(`;
  const start = src.indexOf(anchor);
  expect(start, `锚点消失，守卫已失去判据: ${anchor}`).toBeGreaterThan(-1);
  const rest = src.slice(start);
  const end = rest.indexOf('\n  );');
  expect(end, `找不到 ${name} 的 useCallback 收尾`).toBeGreaterThan(-1);
  return rest.slice(0, end);
}

describe('守卫自检：扫到的确实是源码（防读空文件恒绿）', () => {
  it('两个源文件非空且是本屏的文件', () => {
    expect(NODES_RAW.length).toBeGreaterThan(1000);
    expect(CARD_RAW.length).toBeGreaterThan(1000);
    expect(NODES).toContain('export function NodesScreen');
    // NodeCard 的出口是 `memo(NodeCardView)`（渲染预算门要求，见 nodes-render-budget.test.tsx），
    // 故这里认实现函数名 + memo 出口两段，不再认已不存在的 `export function NodeCard`。
    expect(CARD).toContain('function NodeCardView');
    expect(CARD).toContain('export const NodeCard = memo(NodeCardView)');
  });

  it('去注释后仍是可断言的代码（防 code() 把源码整段吃掉 → 负向断言恒绿）', () => {
    expect(NODES.length).toBeGreaterThan(NODES_RAW.length / 3);
    expect(CARD.length).toBeGreaterThan(CARD_RAW.length / 3);
    // 注释确实被剥掉了（否则负向断言会被说明文字误伤，本守卫等于没接线）。
    expect(NODES).not.toContain('1:1 提取自原型');
  });
});

describe('T1：三个测速入口共用 isSpeedTestable 口径（候选集不同、过滤线只有一条）', () => {
  it('页头「全部测速」= 全量 servers 过 speedTestableIds（不得退回无参 speedTest()）', () => {
    const body = callbackBody(NODES, 'testAll');
    expect(body, '页头腿必须自己给出 id 集，且与首页圆钮同口径').toContain('speedTestableIds(servers');
    // 这正是被指控的那行原文形态：不传 id 集 = 由后端决定测谁 = 与首页不同义。
    expect(NODES, '不得退回 api.server.speedTest()（无参全量）').not.toMatch(
      /api\.server\.speedTest\(\s*\)/
    );
  });

  it('工具栏「测速」（可见集）= 当前可见集（搜索/协议筛选后）过 speedTestableIds，不是整组', () => {
    const body = callbackBody(NODES, 'testVisible');
    expect(body, '必须以 visibleServers 为候选集，否则又变成无视筛选的整组测速').toContain(
      'speedTestableIds(visibleServers'
    );
    // 原状形态：`api.server.speedTest(activeGroup.servers.map(...))`。
    expect(NODES, '不得退回按 activeGroup 整组测速').not.toMatch(/speedTest\(\s*activeGroup/);
  });

  it('批选「测速」= 可见 ∩ 已选 ∩ 可测（走 speedTestIdsForSelection，它内部过 speedTestableIds）', () => {
    const body = callbackBody(NODES, 'testSelected');
    expect(body).toContain('speedTestIdsForSelection(');
    // 断言 caps 在**实参位**上，不是「函数体里出现过 speedTestCaps 就算」——它同时躺在 deps 数组里，
    // 只要 toContain 就会被 deps 喂绿（变异实证：删掉实参、留着 deps，弱断言照样全绿）。
    expect(body, '批选腿也必须传 caps，否则 TS-exit 的 path-aware 位在这条腿上丢失').toMatch(
      /speedTestIdsForSelection\([^)]*speedTestCaps/
    );
  });

  it('口径是 path-aware 的（带 mainCorePool 能力位，否则 TS-exit 会被误纳）', () => {
    expect(NODES).toMatch(/mainCorePool/);
    // 能力位必须来自代理运行态，而不是随手写死 true。
    expect(NODES).toMatch(/mainCorePool:\s*!!proxyStatus\?\.running/);
  });

  it('三条腿都不空跑：空集提示而非发空请求', () => {
    const body = callbackBody(NODES, 'runSpeedTest');
    expect(body).toContain('nodes.noTestableNodes');
  });

  it('卡片单节点测速也消费 invoke 返回值兜底，不能只赌事件一定送达', () => {
    const body = callbackBody(NODES, 'testOne');
    expect(body).toContain('absorbRunResult(await api.server.speedTest([server.id]))');
  });
});

describe('T2：节点卡不得再自造弱判定（谓词在、调用点却绕开它，是本轮的根因形态）', () => {
  it('NodesScreen **不得**出现 `allowInternet === false` 这条自造判定', () => {
    // 被替换掉的原文形态：`const lanOnly = isMesh && server.wireguardSettings?.allowInternet === false;`
    // 它漏掉 Tailscale 一族（TS 的外网出口由 exitNode 派生），也管不了 reverseMesh。
    expect(NODES).not.toMatch(/allowInternet\s*===\s*false/);
  });

  it('「仅局域网」角标走 domain 谓词 meshAllowsInternet', () => {
    expect(NODES).toMatch(/meshAllowsInternet\(/);
  });

  it('每张卡的可测性由 speedTestBlockReason 单点给出，并传给 NodeCard', () => {
    // 第三个入参是 staged-only 位（`ENTITY_ACTION_TABLE` 的 `block` 腿）：盘上还没有这个节点时
    // 卡上的 ⚡ 必须置灰。**断言收紧而非放宽** —— 少传它，卡会对一个测不出真值的节点亮着 ⚡。
    expect(NODES).toMatch(
      /speedTestBlockReason\(server,\s*speedTestCaps,\s*stagedOnly\.has\(server\.id\)\)/
    );
    expect(NODES).toContain('speedTestable={');
    expect(NODES).toContain('speedTestBlockedHint={');
  });

  it('三个批量入口与卡上 ⚡ 同一条过滤线（staged-only 必须从候选 id 集里也排掉）', () => {
    // 只置灰不过滤 = 「全部测速」照样把 staged-only 的 id 发给后端，后端按 id 查不到 → 静默缺席。
    // 变异对照：删掉任一处的 `stagedOnly` 实参，本条转红。
    expect(NODES).toMatch(/speedTestableIds\(servers,\s*speedTestCaps,\s*stagedOnly\)/);
    expect(NODES).toMatch(/speedTestableIds\(visibleServers,\s*speedTestCaps,\s*stagedOnly\)/);
    expect(NODES).toMatch(
      /speedTestIdsForSelection\(\s*visibleServers,\s*selectedIds,\s*speedTestCaps,\s*stagedOnly\s*\)/
    );
  });
});

describe('T3：不可测的 ⚡ 是「置灰 + 说明原因」，不是整个藏起来', () => {
  it('NodeCard 的 ⚡ 恒渲染、按可测性 disabled（**不得**退回 `!lanOnly &&` 条件渲染）', () => {
    expect(CARD).toContain('disabled={!testable}');
    // 原状形态：`{!lanOnly && (<button className="nd-a speed" ...`——按钮凭空消失，用户无从得知为什么。
    expect(CARD).not.toMatch(/\{\s*!lanOnly\s*&&\s*\(/);
  });

  it('置灰必须带得出理由（tooltip 挂按钮 + 延迟位两处）', () => {
    expect(CARD).toContain('speedTestBlockedHint');
    // 延迟位也挂一份：部分 WebView 上 disabled 按钮的 title 不弹。
    expect(CARD).toMatch(/const latencyTip[\s\S]{0,120}speedTestBlockedHint/);
    expect(CARD).toMatch(/nd-lat[\s\S]{0,120}latencyTip/);
  });

  it('理由必须进 aria-label —— 只挂 title 时键盘/读屏用户拿不到（2026-07-28 复审 LOW #8）', () => {
    // `title` 是 hover-only，而 `disabled` 按钮不可聚焦 ⇒ 读屏用户读到的只有通用的「测速」，
    // 「为什么这个不能测」对他们完全不可达。故不可测时把原因拼进无障碍名。
    // 定位到 ⚡ 那个按钮（`className="nd-a speed"`）之后的第一个 aria-label，取其花括号块。
    const at = CARD.indexOf('nd-a speed');
    expect(at, '找不到 ⚡ 按钮（className="nd-a speed"）').toBeGreaterThan(-1);
    const start = CARD.indexOf('aria-label={', at);
    expect(start, '⚡ 按钮没有 aria-label').toBeGreaterThan(-1);
    let depth = 0;
    let end = -1;
    for (let i = start + 'aria-label='.length; i < CARD.length; i++) {
      if (CARD[i] === '{') depth++;
      else if (CARD[i] === '}' && --depth === 0) {
        end = i + 1;
        break;
      }
    }
    expect(end, 'aria-label 的花括号没配平').toBeGreaterThan(-1);
    const ariaLabel = CARD.slice(start, end);
    expect(ariaLabel, 'aria-label 仍是通用文案，没带原因').toContain('speedTestBlockedHint');
    // 可测时必须保持**通用**文案（把原因无条件拼上去会让正常节点的无障碍名变成一句废话）。
    expect(ariaLabel).toMatch(/testable\s*\|\|\s*!speedTestBlockedHint/);
  });

  it('延迟数值与 ⚡ 跟同一个可测性判定（不得一个看 lanOnly、一个看 speedTestable）', () => {
    expect(CARD).toMatch(/const\s+level\s*=\s*testable\s*\?/);
    expect(CARD).toMatch(/const\s+latText\s*=\s*!testable\s*\?/);
  });
});

describe('T4：视图档（卡片/列表）是持久偏好，不是组件私有 state', () => {
  it('NodesScreen 读持久 store', () => {
    expect(NODES).toContain('useNodeViewStore');
  });

  it('**不得**退回 `useState(\'cards\')`（切屏即卸载重挂 → 用户选的列表视图被悄悄改回）', () => {
    expect(NODES).not.toMatch(/useState<ViewMode>/);
    expect(NODES).not.toMatch(/useState\(\s*['"]cards['"]\s*\)/);
  });
});

describe('T5：空订阅组的空态引导「刷新订阅」，不是「点右上添加」', () => {
  it('订阅组走 nodes.emptySub 分支（自建/组网组仍用 nodes.empty）', () => {
    expect(NODES).toContain('nodes.emptySub');
    expect(NODES).toMatch(/activeSub\s*\n?\s*\?\s*t\('nodes\.emptySub'/);
  });
});
