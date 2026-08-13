/**
 * D1 纯逻辑单测（vitest，node 环境）：Csel 定位/键盘协议 + URL 弹窗推断/校验。
 * 真机行为（showModal/焦点/scrim/裁切）不在此层，走 Linux 本机冒烟（见交付说明）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  buildCselRows,
  cselGroupIdAt,
  CSEL_EDGE,
  CSEL_GAP,
  cselKeyReduce,
  computeCselPosition,
  flattenCselOptions,
  isCselGrouped,
  type CselGroup,
  type CselOptionLike,
  type Rect,
} from './csel-logic';
import {
  inferResource,
  validateResName,
  validateResUrl,
} from './res-url-logic';
import { missingRuleSetRefs, ruleSetPickMatches, ruleSetPickState } from './rule-set-pick';
import { geoPoolOptions, matchRuleValueOptions } from './rule-cond';
import type { RuleResourceListItem } from '@/contracts/types';

const VP = { width: 925, height: 740 };

function trig(partial: Partial<Rect>): Rect {
  return { left: 100, top: 300, bottom: 328, width: 220, ...partial };
}

describe('computeCselPosition', () => {
  it('下方空间充足 → 贴触发器下沿，不翻转', () => {
    const p = computeCselPosition(trig({ top: 200, bottom: 228 }), 180, VP);
    expect(p.flipped).toBe(false);
    expect(p.top).toBe(228 + CSEL_GAP);
    expect(p.left).toBe(100);
    expect(p.width).toBe(220);
  });

  it('下方空间不足 → 向上翻转到触发器上方', () => {
    // 触发器贴近视口底部，菜单 300 放不下 → flip up
    const p = computeCselPosition(trig({ top: 600, bottom: 628 }), 300, VP);
    expect(p.flipped).toBe(true);
    expect(p.top).toBe(600 - 300 - CSEL_GAP);
  });

  it('翻转后仍越顶 → 贴顶边（不越出视口）', () => {
    const p = computeCselPosition(trig({ top: 120, bottom: 720 }), 300, VP);
    expect(p.flipped).toBe(true);
    expect(p.top).toBe(CSEL_EDGE); // max(EDGE, 120-300-5) = EDGE
  });

  it('右溢出 → 贴右缘内收', () => {
    const p = computeCselPosition(trig({ left: 800, width: 200 }), 100, VP);
    // 800+200=1000 > 925-8 → left = max(8, 925-200-8) = 717
    expect(p.left).toBe(VP.width - 200 - CSEL_EDGE);
  });

  it('左越界 → 兜底 EDGE', () => {
    const p = computeCselPosition(trig({ left: -20, width: 100 }), 100, VP);
    expect(p.left).toBe(CSEL_EDGE);
  });
});

describe('cselKeyReduce', () => {
  it('ArrowDown 环绕递增', () => {
    expect(cselKeyReduce(0, 5, 'ArrowDown')).toEqual({ active: 1, action: 'move' });
    expect(cselKeyReduce(4, 5, 'ArrowDown')).toEqual({ active: 0, action: 'move' });
  });
  it('ArrowUp 环绕递减', () => {
    expect(cselKeyReduce(0, 5, 'ArrowUp')).toEqual({ active: 4, action: 'move' });
    expect(cselKeyReduce(2, 5, 'ArrowUp')).toEqual({ active: 1, action: 'move' });
  });
  it('Enter / Space → choose', () => {
    expect(cselKeyReduce(3, 5, 'Enter').action).toBe('choose');
    expect(cselKeyReduce(3, 5, ' ').action).toBe('choose');
  });
  it('Escape → close', () => {
    expect(cselKeyReduce(3, 5, 'Escape').action).toBe('close');
  });
  it('Tab → close-blur', () => {
    expect(cselKeyReduce(3, 5, 'Tab').action).toBe('close-blur');
  });
  it('其它键 → none，active 不变', () => {
    expect(cselKeyReduce(3, 5, 'a')).toEqual({ active: 3, action: 'none' });
  });
  it('count<=0 → none（防除零/空表）', () => {
    expect(cselKeyReduce(0, 0, 'ArrowDown')).toEqual({ active: 0, action: 'none' });
  });
});

describe('cselKeyReduce ↑/↓ 跳过 disabled 选项（LOW-8）', () => {
  // [A, B(disabled), C]
  const midDisabled = [false, true, false];
  const isMidDisabled = (i: number) => midDisabled[i];

  it('ArrowDown 跳过中间的 disabled 项，落在下一个可用项', () => {
    expect(cselKeyReduce(0, 3, 'ArrowDown', isMidDisabled)).toEqual({ active: 2, action: 'move' });
  });
  it('ArrowUp 跳过中间的 disabled 项，落在上一个可用项', () => {
    expect(cselKeyReduce(2, 3, 'ArrowUp', isMidDisabled)).toEqual({ active: 0, action: 'move' });
  });

  // [A(disabled), B, C] —— 环绕时须跨过组头前的 disabled 首项
  const headDisabled = [true, false, false];
  const isHeadDisabled = (i: number) => headDisabled[i];

  it('ArrowDown 环绕后仍跳过 disabled 项（末项 → 环绕 → 首项禁用 → 落第二项）', () => {
    expect(cselKeyReduce(2, 3, 'ArrowDown', isHeadDisabled)).toEqual({ active: 1, action: 'move' });
  });

  it('全部禁用（异常输入）→ 原地不动，不死循环', () => {
    const allDisabled = () => true;
    expect(cselKeyReduce(1, 3, 'ArrowDown', allDisabled)).toEqual({ active: 1, action: 'move' });
  });

  it('未传 isDisabled → 行为与原始环绕算术一致（向后兼容）', () => {
    expect(cselKeyReduce(0, 5, 'ArrowDown')).toEqual({ active: 1, action: 'move' });
    expect(cselKeyReduce(0, 5, 'ArrowUp')).toEqual({ active: 4, action: 'move' });
  });
});

describe('csel grouped-index math (D4 optgroup)', () => {
  const flat: CselOptionLike[] = [
    { value: 'a', label: 'A' },
    { value: 'b', label: 'B' },
    { value: 'c', label: 'C' },
  ];
  const grouped: CselGroup[] = [
    { label: 'G1', options: [{ value: 'a', label: 'A' }, { value: 'b', label: 'B' }] },
    { label: 'G2', options: [{ value: 'c', label: 'C' }] },
    { label: 'G3', options: [{ value: 'd', label: 'D' }, { value: 'e', label: 'E' }] },
  ];

  it('isCselGrouped 判别：分组带 options / 扁平不带 / 空按扁平', () => {
    expect(isCselGrouped(grouped)).toBe(true);
    expect(isCselGrouped(flat)).toBe(false);
    expect(isCselGrouped([])).toBe(false);
  });

  it('flattenCselOptions：分组按声明顺序摊平；扁平原样', () => {
    expect(flattenCselOptions(grouped).map((o) => o.value)).toEqual(['a', 'b', 'c', 'd', 'e']);
    expect(flattenCselOptions(flat).map((o) => o.value)).toEqual(['a', 'b', 'c']);
  });

  it('flattenCselOptions/buildCselRows 逐字保留可选字段（danger/icon 掉了 = 渲染端取不到）', () => {
    // 为什么值得单钉：这两个函数今天是 `slice()` / 传引用，字段天然全在；但若哪天改成
    // 「显式投影出 value/label/disabled」重建对象，TypeScript **不会报错**（可选字段缺失仍满足
    // CselOptionLike），而症状是「规则弹窗的阻断项不再变红」「节点行的国旗没了」这种只有真机
    // 才看得见的静默丢失。danger 通道见 csel-logic.ts 的字段头注。
    const src: CselGroup[] = [
      { label: 'G', options: [
        { value: 'proxy', label: '代理' },
        { value: 'block', label: '阻断', danger: true, disabled: true },
      ] },
    ];
    expect(flattenCselOptions(src).map((o) => o.danger)).toEqual([undefined, true]);
    expect(flattenCselOptions(src).map((o) => o.disabled)).toEqual([undefined, true]);
    const opts = buildCselRows(src).flatMap((r) => (r.kind === 'option' ? [r.opt] : []));
    expect(opts.map((o) => o.danger)).toEqual([undefined, true]);
  });

  it('buildCselRows(扁平)：全 option 行，flatIndex = 下标，无组头', () => {
    const rows = buildCselRows(flat);
    expect(rows.every((r) => r.kind === 'option')).toBe(true);
    expect(rows.map((r) => (r.kind === 'option' ? r.flatIndex : -1))).toEqual([0, 1, 2]);
  });

  it('buildCselRows(分组)：每组一个组头 + 选项，flatIndex 跨组连续', () => {
    const rows = buildCselRows(grouped);
    // 3 组头 + 5 选项 = 8 行
    expect(rows).toHaveLength(8);
    expect(rows.filter((r) => r.kind === 'header').map((r) => (r.kind === 'header' ? r.label : ''))).toEqual([
      'G1',
      'G2',
      'G3',
    ]);
    // 选项 flatIndex 与 flatten 顺序一致，组头不占索引
    const opts = rows.filter((r) => r.kind === 'option');
    expect(opts.map((r) => (r.kind === 'option' ? [r.opt.value, r.flatIndex] : []))).toEqual([
      ['a', 0],
      ['b', 1],
      ['c', 2],
      ['d', 3],
      ['e', 4],
    ]);
  });

  it('↑/↓ 跨组连续、跳过组头（flatCount = flatten 长度上环绕）', () => {
    const count = flattenCselOptions(grouped).length; // 5
    // G1 末选项(b=1) → ArrowDown → G2 首选项(c=2)，中间的 G2 组头不被停留
    expect(cselKeyReduce(1, count, 'ArrowDown')).toEqual({ active: 2, action: 'move' });
    // G3 末选项(e=4) → ArrowDown → 环绕回 G1 首选项(a=0)
    expect(cselKeyReduce(4, count, 'ArrowDown')).toEqual({ active: 0, action: 'move' });
    // 首选项(a=0) → ArrowUp → 环绕到末选项(e=4)
    expect(cselKeyReduce(0, count, 'ArrowUp')).toEqual({ active: 4, action: 'move' });
  });
});

/**
 * 可折叠分组（`CselGroup.id`）—— 规则弹窗「目标出站」按订阅分组 + 默认折叠所需。
 *
 * 核心不变量：**折叠只藏渲染行，不改索引空间**。折叠项若同时退出扁平索引，组头又不在焦点链里
 * （菜单一 Tab 就关），折叠组里的选项对键盘用户就成了永远够不到的死区 —— 折叠是给鼠标省滚动的，
 * 不该顺手把一批节点藏死。下面 flatIndex 与 cselGroupIdAt 两条钉的就是这个。
 */
describe('csel 可折叠分组（带 id 的组）', () => {
  const mixed: CselGroup[] = [
    // 无 id：不可折叠、恒展开（规则类型那 15×5 分组即此形态）。
    { label: 'P', options: [{ value: 'proxy', label: 'Proxy' }] },
    { id: 'g1', label: 'G1', options: [{ value: 'a', label: 'A' }] },
    { id: 'g2', label: 'G2', options: [{ value: 'b', label: 'B' }, { value: 'c', label: 'C' }] },
  ];
  /** 当前**渲染出来**的选项（折叠组的选项行不在其中）。 */
  const shown = (openIds?: ReadonlySet<string>) =>
    buildCselRows(mixed, openIds).flatMap((r) => (r.kind === 'option' ? [r.opt.value] : []));

  it('省略 openIds ⇒ 全展开（既有分组消费方逐字不受影响）', () => {
    expect(shown()).toEqual(['proxy', 'a', 'b', 'c']);
    expect(buildCselRows(mixed)).toHaveLength(3 + 4);
  });

  it('不在 openIds 里的带 id 组被折叠：选项行不渲染', () => {
    expect(shown(new Set(['g2']))).toEqual(['proxy', 'b', 'c']);
    expect(shown(new Set())).toEqual(['proxy']);
  });

  it('无 id 的组不受 openIds 影响（快速策略那一组不许被折进去）', () => {
    // 变异靶：把 buildCselRows 的折叠判据从「有 id 才可折叠」改成「一律可折叠」→ 本条转红。
    expect(shown(new Set())).toContain('proxy');
  });

  it('折叠**不改**索引空间：flatIndex 恒等于该项在 flattenCselOptions 里的下标', () => {
    // 变异靶：把 buildCselRows 折叠分支的 `flat += g.options.length` 删掉 → 本条转红
    //（症状是键盘 ↑/↓ 选中的项与高亮行错位，且折叠组里的项彻底选不到）。
    const full = flattenCselOptions(mixed).map((o) => o.value);
    expect(full).toEqual(['proxy', 'a', 'b', 'c']);
    const opts = buildCselRows(mixed, new Set(['g2'])).flatMap((r) =>
      r.kind === 'option' ? [[r.opt.value, r.flatIndex]] : [],
    );
    expect(opts).toEqual([
      ['proxy', 0],
      // 'a' 在折叠的 g1 里 —— 不渲染，但它占着的 1 号位没让给别人
      ['b', 2],
      ['c', 3],
    ]);
    // 键盘协议的环绕基数恒是全量（cselKeyReduce 的 count），折叠不缩小它。
    expect(flattenCselOptions(mixed)).toHaveLength(4);
  });

  it('cselGroupIdAt：按扁平索引反查所属可折叠组（↑/↓ 走进折叠组时靠它展开）', () => {
    expect(cselGroupIdAt(mixed, 0)).toBeUndefined(); // 无 id 的组
    expect(cselGroupIdAt(mixed, 1)).toBe('g1');
    expect(cselGroupIdAt(mixed, 2)).toBe('g2');
    expect(cselGroupIdAt(mixed, 3)).toBe('g2');
    expect(cselGroupIdAt(mixed, 9)).toBeUndefined(); // 越界不炸
  });

  it('组头行恒在（折叠态下它是那一组唯一的展开入口），并带 groupId/collapsed/count', () => {
    const heads = buildCselRows(mixed, new Set(['g1'])).flatMap((r) =>
      r.kind === 'header' ? [[r.label, r.groupId, r.collapsed, r.count]] : [],
    );
    expect(heads).toEqual([
      ['P', undefined, false, 1],
      ['G1', 'g1', false, 1],
      ['G2', 'g2', true, 2],
    ]);
  });

  it('扁平 options 没有组的概念：openIds 对它不适用，cselGroupIdAt 恒 undefined', () => {
    const flatOpts: CselOptionLike[] = [{ value: 'x', label: 'X' }];
    expect(buildCselRows(flatOpts, new Set())).toHaveLength(1);
    expect(cselGroupIdAt(flatOpts, 0)).toBeUndefined();
  });
});

describe('inferResource', () => {
  it('末段文件名去扩展名作 name', () => {
    expect(inferResource('https://x.io/geo/geosite-cn.srs').name).toBe('geosite-cn');
    expect(inferResource('https://x.io/a/b/list.txt?ref=1').name).toBe('list');
  });
  it('分类推断：含 geoip → geoip', () => {
    expect(inferResource('https://x/geoip-cn.srs').category).toBe('geoip');
  });
  it('分类推断：含 geosite → geosite', () => {
    expect(inferResource('https://x/geosite-google.srs').category).toBe('geosite');
  });
  it('分类推断：无关键词 → custom', () => {
    expect(inferResource('https://x/my-list.txt').category).toBe('custom');
  });
  it('空/无扩展名健壮', () => {
    expect(inferResource('')).toEqual({ name: '', category: 'custom' });
    expect(inferResource('https://x/plainname').name).toBe('plainname');
  });
});

describe('validateResUrl', () => {
  it('空 → urlEmpty', () => {
    expect(validateResUrl('')).toBe('urlEmpty');
    expect(validateResUrl('   ')).toBe('urlEmpty');
  });
  it('非法 → urlInvalid', () => {
    expect(validateResUrl('not a url')).toBe('urlInvalid');
    expect(validateResUrl('ftp://x/a.srs')).toBe('urlInvalid');
    expect(validateResUrl('file:///etc/passwd')).toBe('urlInvalid');
  });
  it('合法 http/https → null', () => {
    expect(validateResUrl('https://x.io/a.srs')).toBeNull();
    expect(validateResUrl('http://x.io/a.srs')).toBeNull();
    expect(validateResUrl('  https://x.io/a.srs  ')).toBeNull();
  });
});

describe('validateResName', () => {
  it('空 → nameEmpty', () => {
    expect(validateResName('')).toBe('nameEmpty');
    expect(validateResName('  ')).toBe('nameEmpty');
  });
  it('非空 → null', () => {
    expect(validateResName('geosite-cn')).toBeNull();
  });
});

/* ───────────────────────────────────────────────────────────────────────────
 * 规则集选择器（`rule-set-pick.ts`）—— 内置/外置分区 + 检索 + 缺失引用。
 *
 * 判据抽出来的唯一动机就是让它可断言（本仓 vitest 是 node 环境无 jsdom，留在 .tsx 里等于没有门）。
 * 每条都标了变异靶：改坏对应那行即转红（已逐条实跑，见交付说明）。
 * ─────────────────────────────────────────────────────────────────────────── */
/** 列表项夹具：`builtin:true` = 随包（Rust `is_bundled_geo_tag` 的投影），裸 id = 已下载。 */
function resItem(over: Partial<RuleResourceListItem> & { id: string }): RuleResourceListItem {
  return {
    id: over.id,
    name: over.name ?? over.id,
    category: 'geosite',
    sourceUrl: '',
    fileName: `${over.id}.srs`,
    format: 'binary',
    size: 1,
    downloadedAt: '',
    fileExists: over.fileExists ?? true,
    referencedBy: 0,
    builtin: over.builtin,
  };
}

const RS_ITEMS: RuleResourceListItem[] = [
  resItem({ id: 'builtin:geosite-cn', name: 'geosite-cn', builtin: true }),
  resItem({ id: 'builtin:geoip-cn', name: 'geoip-cn', builtin: true }),
  resItem({ id: 'geosite-youtube', name: 'youtube' }),
  resItem({ id: 'geoip-jp', name: 'jp' }),
];

describe('geoPoolOptions —— 三个类型共用一个池，差别只在寻址', () => {
  it('res-id（规则集）：全量、值恒 `res:<id>`、分区判据是 item.builtin', () => {
    // 变异靶：把 `it.builtin === true` 改成 `it.id.startsWith('builtin:')` —— 下一条把两者分开。
    const opts = geoPoolOptions('ruleSet', RS_ITEMS);
    expect(opts.map((o) => o.value)).toEqual([
      'res:builtin:geosite-cn',
      'res:builtin:geoip-cn',
      'res:geosite-youtube',
      'res:geoip-jp',
    ]);
    expect(opts.map((o) => o.group)).toEqual(['builtin', 'builtin', 'external', 'external']);
  });

  it('分区只看 builtin 字段：裸 id + builtin:true 仍归内置', () => {
    // 变异靶：把判据改成按 id 前缀猜 → 本条转红。
    expect(geoPoolOptions('ruleSet', [resItem({ id: 'geosite-ads', builtin: true })])[0].group).toBe(
      'builtin'
    );
  });

  it('bare（geosite/geoip）：按**描述符 id** 派生前缀过滤 + 去前缀取值', () => {
    // 变异靶：把前缀写死成 'geosite-' → geoip 那条转红；不去前缀 → 值变 `geosite-cn`，
    // 而生成端会把它回拼成 `geosite-geosite-cn`（静默不生效）。
    expect(geoPoolOptions('geosite', RS_ITEMS).map((o) => o.value)).toEqual(['cn', 'youtube']);
    expect(geoPoolOptions('geoip', RS_ITEMS).map((o) => o.value)).toEqual(['cn', 'jp']);
  });

  it('free 类型没有候选面（给零候选源的类型造清单 = 造假清单）', () => {
    expect(geoPoolOptions('domain', RS_ITEMS)).toEqual([]);
    expect(geoPoolOptions('processName', RS_ITEMS)).toEqual([]);
  });

  it('检索语料按描述符的 searchFields 取：res-id 走 name+id，bare 只走裸 tag', () => {
    // 变异靶：把 res-id 的 searchFields 改成只剩 ['name'] → 第二条转红（`geoip-jp` 的 name 是裸 'jp'）。
    const vals = (q: string) =>
      matchRuleValueOptions(geoPoolOptions('ruleSet', RS_ITEMS), q).map((o) => o.value);
    expect(vals('  YouTube ')).toEqual(['res:geosite-youtube']); // 命中 name、大小写/空白无关
    expect(vals('geoip-jp')).toEqual(['res:geoip-jp']); // 只经 id 命中
    // bare 池的语料是去前缀后的裸 tag ⇒ 搜 `geosite` 一条都不该命中（前缀已不在语料里）。
    expect(matchRuleValueOptions(geoPoolOptions('geosite', RS_ITEMS), 'geosite')).toEqual([]);
  });
});

describe('missingRuleSetRefs —— 引用了本地不可用的规则集', () => {
  it('fileExists=false 的资源被引用 → 报缺失', () => {
    // 变异靶：把 availableResourceTagSet 换成「无条件全可用」→ 转红。
    const items = [resItem({ id: 'geosite-youtube', fileExists: false })];
    expect(missingRuleSetRefs(['res:geosite-youtube'], items)).toEqual(['res:geosite-youtube']);
  });

  it('清单里压根没有的 id → 报缺失（资源被删后规则里留的孤儿引用）', () => {
    expect(missingRuleSetRefs(['res:geosite-gone'], RS_ITEMS)).toEqual(['res:geosite-gone']);
  });

  it('可用的不报；`builtin:` 前缀按 geoTagOf 归一后比对（与列表角标同一判据）', () => {
    // 变异靶：把 geoTagOf 换成原样返回 → `res:geosite-cn`（裸 tag 引用随包项）会被误报缺失 → 转红。
    expect(missingRuleSetRefs(['res:builtin:geosite-cn'], RS_ITEMS)).toEqual([]);
    expect(missingRuleSetRefs(['res:geosite-cn'], RS_ITEMS)).toEqual([]);
  });

  it('非 `res:` 前缀的值一概不看（手填的裸 tag 不是本函数的射程）', () => {
    // 生成端 custom_rules.rs 只认 res: 前缀，裸值本就不会被解析成资源引用。
    expect(missingRuleSetRefs(['category-ads-all', ' '], RS_ITEMS)).toEqual([]);
  });

  it('去重保序 + 容忍首尾空白', () => {
    expect(missingRuleSetRefs([' res:x ', 'res:x', 'res:y'], RS_ITEMS)).toEqual(['res:x', 'res:y']);
  });
});

describe('ruleSetPickState —— 「挑不出来」的三种原因必须可辨', () => {
  it('null = 还在加载（不是「没有」）', () => {
    // 变异靶：把 `items === null` 那条删掉 → 落进 failed → 本条转红。
    // 谎报后果：惰性拉取的空窗期里弹窗会说「清单加载失败」，而它根本还没拉完。
    expect(ruleSetPickState(null, '')).toBe('loading');
    expect(ruleSetPickState(null, 'anything')).toBe('loading');
  });

  it('[] = 拉过且失败（成功的 rule_resources_list 恒非空 —— 无条件投影随包表）', () => {
    // 变异靶：把 `items.length === 0` 那条删掉 → 落进 noMatch → 本条转红。
    // 谎报后果：把**加载失败**说成**结果为空**，用户会去改搜索词，而清单压根没拉到。
    expect(ruleSetPickState([], '')).toBe('failed');
    expect(ruleSetPickState([], 'youtube')).toBe('failed');
  });

  it('拿到清单但检索滤空 = noMatch（只有这一态该说「无匹配」）', () => {
    expect(ruleSetPickState(RS_ITEMS, 'zzz-no-such')).toBe('noMatch');
  });

  it('挑得出至少一条 = ok', () => {
    expect(ruleSetPickState(RS_ITEMS, '')).toBe('ok');
    expect(ruleSetPickState(RS_ITEMS, 'youtube')).toBe('ok');
    // 只剩一个真实分组（外置被检索滤掉）仍是 ok —— 挑得出来。
    expect(ruleSetPickState(RS_ITEMS, 'geoip-cn')).toBe('ok');
  });

  it('全部选项都已被勾上时仍是 ok（挑得出来，只是都挑过了）', () => {
    // 已勾 ≠ 没有：此时用户不需要「去下载」，需要的是知道自己已经全加了。
    // 故 state 只看命中数、不看已选值（勾选态是 `selectedValueSet` 的事）。
    expect(ruleSetPickState(RS_ITEMS, '')).toBe('ok');
  });

  it('三态与**勾选区**同步：state≠ok ⟺ 勾选区一条候选都排不出来', () => {
    // 钉的是「勾选区里那句说明」与「勾选区外那条『前往规则资源』提示」不会各说一套 ——
    // 二者一个由 ruleSetPickState 派生、一个由 geoPoolOptions+matchRuleValueOptions 派生，
    // 两条过滤口径必须重合（都是 name+id）。
    // 变异靶：把 ruleSet 描述符的 searchFields 改成只剩 ['name'] → 最后一条 case 转红。
    const cases: { items: RuleResourceListItem[] | null; q: string }[] = [
      { items: null, q: '' },
      { items: [], q: '' },
      { items: RS_ITEMS, q: 'zzz-no-such' },
      { items: RS_ITEMS, q: '' },
      { items: RS_ITEMS, q: 'youtube' },
      // **只经 id 命中**的查询：`geoip-jp` 的 name 是裸 'jp'，只有 id 含它，且没有任何条目的 name
      // 含这个串 ⇒ 若两边过滤条件分叉（如勾选区只匹配 name），勾选区会空而 state 仍报 ok。
      // 这条是本不变量真正的牙：少了它，改成只匹配 name 也不会让本条转红。
      { items: RS_ITEMS, q: 'geoip-jp' },
    ];
    for (const { items, q } of cases) {
      const picked = matchRuleValueOptions(geoPoolOptions('ruleSet', items ?? []), q);
      expect(
        picked.length === 0,
        `state/勾选区不同步：items=${JSON.stringify(items)?.slice(0, 20)} q=${q}`
      ).toBe(ruleSetPickState(items, q) !== 'ok');
    }
  });
});

describe('ruleSetPickMatches —— 分组渲染与状态判定共用的唯一过滤器', () => {
  it('空查询 → 原样返回（副本，不是同一引用）', () => {
    const out = ruleSetPickMatches(RS_ITEMS, '   ');
    expect(out).toEqual([...RS_ITEMS]);
    expect(out).not.toBe(RS_ITEMS);
  });

  it('匹配 name 与 id 两者、大小写与首尾空白无关', () => {
    expect(ruleSetPickMatches(RS_ITEMS, '  YouTube ').map((r) => r.id)).toEqual(['geosite-youtube']);
    expect(ruleSetPickMatches(RS_ITEMS, 'geosite').map((r) => r.id)).toEqual([
      'builtin:geosite-cn',
      'geosite-youtube',
    ]);
  });
});

/**
 * 接线守卫：规则集「前往规则资源」的**两条触发腿都还在**。
 *
 * 为什么要源码结构断言：纯逻辑单测钉住了 `isRuleSetPickEmpty` / `missingRuleSetRefs` 各自算得对，
 * 但「弹窗到底消费了哪几条」它管不到 —— 删掉 `|| rsEmpty` 那条腿，两个函数照样全绿，而检索无命中
 * 时的出路就静默消失了。本仓既有守卫（config-write-wiring / tray-live-wiring / style-invariants）
 * 同款手法。
 *
 * 变异靶：删 `|| rsEmpty` → 第二条红；删 `rsMissing.length > 0` → 第一条红；
 * 把按钮换成新造的 i18n 键 → 第三条红。
 */
describe('接线：规则集缺失提示的两条腿 + 复用既有按钮键', () => {
  /** 去注释：本文件与 RuleDialog 的注释都逐字引用了这些标识符，不去掉就是拿注释当证据。 */
  const code = (src: string): string =>
    src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
  const src = code(
    readFileSync(fileURLToPath(new URL('./RuleDialog.tsx', import.meta.url)), 'utf8')
  );

  it('自检：读到了 RuleDialog 源码且去注释后仍是可断言的代码', () => {
    expect(src.length, 'RuleDialog.tsx 读空了 —— 被改名/移走了？').toBeGreaterThan(5000);
    expect(src, '去注释把源码吃光了').toContain('import');
  });

  it('腿①：缺失引用告警（有真实后果的那个：生成端 fail-closed 剪枝 ⇒ 规则静默不工作）', () => {
    expect(src, 'RuleDialog 不再算缺失引用').toContain('ruleSetMissing(');
    expect(src, '提示行不再消费缺失引用这条腿').toMatch(/rsMissing\.length\s*>\s*0/);
  });

  it('腿②：一条都挑不出来时也给出路（检索无命中 / 清单拉取失败）', () => {
    expect(src, 'RuleDialog 不再判「挑不出来」').toContain('ruleSetPickState(');
    expect(src, '提示行不再消费「挑不出来」这条腿').toMatch(/\|\|\s*rsEmpty/);
  });

  it('按钮复用应用分流那个既有键（零新 locale 键）+ 去处是规则资源屏', () => {
    expect(src, '按钮换成了新键 —— 同词同义不该有两套措辞，且会动 locale-parity 债务基线').toContain(
      'appAdd.gotoResources'
    );
    expect(src, '出路不再指向规则资源屏').toContain("navigate('resources')");
  });
});

/**
 * 接线守卫：普通 WG 节点的 `Reserved` 入口 —— **覆盖门守不住这一条**。
 *
 * `contracts/protocol-settings-coverage.test.ts` 的锁 2 对 `WireGuardSettings` 取两个编辑器的
 * **并集**：只要 `WarpDialog` 提到 `reserved` 就算覆盖。于是「普通 WG 节点没有 Reserved 入口」
 * 这个真实缺口能一路绿着存在（该门文件头的射程自曝里逐字记了这一条）。`wg-logic.test.ts` 同样
 * 管不到 —— 它测的是读写逻辑对不对，不是「用户有没有控件」。故这条不变式只能落在源码结构断言上，
 * 与上面那组 RuleDialog 守卫同款手法。
 *
 * 变异靶：删掉 `wgSpec` 里 `k: 'reserved'` 那一行 → 第 2 条红（覆盖门与逻辑单测都不会红）；
 * 删掉提交前的校验腿 → 第 3 条红（后端对不合法 reserved 是静默忽略，没有别的地方会说话）。
 */
describe('接线：WG 弹窗的 Reserved 控件与提交校验', () => {
  /** 去注释：本文件与 WgDialog 的注释都逐字引用了这些标识符，不去掉就是拿注释当证据。 */
  const code = (src: string): string =>
    src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');
  const src = code(readFileSync(fileURLToPath(new URL('./WgDialog.tsx', import.meta.url)), 'utf8'));

  it('自检：读到了 WgDialog 源码且去注释后仍是可断言的代码', () => {
    expect(src.length, 'WgDialog.tsx 读空了 —— 被改名/移走了？').toBeGreaterThan(5000);
    expect(src, '去注释把源码吃光了').toContain('import');
  });

  it('FieldSpec 表里真有 reserved 这一项（= 用户改得了），且走 i18n 键', () => {
    expect(src, 'wgSpec 里没有 reserved 字段项 —— 普通 WG 节点又改不了 Reserved 了').toMatch(
      /k:\s*'reserved'/
    );
    expect(src, 'Reserved 标签没走 i18n 键').toContain("'wg.reserved'");
  });

  it('提交前拦下「填了但不合法」（否则后端静默忽略，用户以为存上了）', () => {
    expect(src, 'WgDialog 不再校验 reserved').toContain('reservedInputInvalid(');
    expect(src, '校验失败没有可见反馈').toContain("'wg.errReserved'");
  });
});
