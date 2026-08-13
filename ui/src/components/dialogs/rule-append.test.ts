/**
 * 「加入已有规则」的五道门。每一道都对着一次**具体的变异**，变异后必须转红（已逐个实跑）：
 *
 * ① 候选面完整 —— 变异「退回只列已有域名条件的规则」⇒ 红（这是 2026-07-30 真机反馈的根因：
 *    用户的规则多是 `ruleSet` / `geosite` / `processName`，旧判据下一条候选都没有）。
 * ② 置灰分类正确 —— 三类原因各有用例，变异「把某一类误判成可追加」⇒ 红。
 * ③ 新开条件只在 `combineMode !== 'and'` 下允许 —— 变异「and 也允许新开」⇒ 红（**核心线**：
 *    `or` 下新开条件与追加值生成结果等价，`and` 下是求交，语义完全不同）。
 * ④ 镜像不变式 —— 变异「追加/新开时不同步 `conditions[0]` 的镜像」⇒ 红。
 * ⑤ 优先级提示只是提示 —— 变异「据此禁用『新建』」⇒ 红（钉住它不许升级成门）。
 * 暂存分流由 `lib/config-write-wiring.test.ts` T3 守（`host-rule-menu.tsx` 登记为 `staged`
 * ⇒ 必须真的有 `editRoute('customRules', stagingEnabled) === 'staged'`），本文件不重复造第二道。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import type { Rule, RuleType } from '@/contracts/types';
import { RULE_TYPES, RULE_TYPE_IDS, ruleConditions } from '@/domain/rules';
import {
  analyzeDomainCoverage,
  appendValueToRule,
  APPENDABLE_HOST_TYPES,
  isShadowedTarget,
  matchAppendTargets,
  NEW_COND_TYPE,
  ruleAppendTargets,
  sortAppendTargets,
} from './rule-append';

const rule = (over: Partial<Rule> & Pick<Rule, 'id'>): Rule => ({
  type: 'domainSuffix',
  values: ['example.com'],
  action: 'proxy',
  enabled: true,
  ...over,
});

/** 多条件规则：镜像恒 = conditions[0]（构造器自己维护，免得样例本身就违反不变式）。 */
const multi = (id: string, conds: Array<{ type: RuleType; values: string[] }>, over: Partial<Rule> = {}): Rule => ({
  id,
  type: conds[0].type,
  values: conds[0].values,
  conditions: conds,
  action: 'proxy',
  enabled: true,
  ...over,
});

describe('① 候选面：**全部规则**都出现，一条都不许被静默吃掉', () => {
  it('自检：域名族恰四类，可追加的恰三类（`domainRegex` 被排除），新开腿用其中之一', () => {
    const domainFamily = RULE_TYPE_IDS.filter((id) => RULE_TYPES[id].category === 'domain');
    expect(domainFamily).toHaveLength(4);
    expect([...APPENDABLE_HOST_TYPES].sort()).toEqual(['domain', 'domainKeyword', 'domainSuffix']);
    expect(APPENDABLE_HOST_TYPES, '新开条件用了一个不能追加的类型').toContain(NEW_COND_TYPE);
  });

  it('**每条规则至少一项**：非域名族规则不再消失，而是以可追加或置灰的形式出现（本轮根因门）', () => {
    const rules = [
      rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] }),
      rule({ id: 'geo', type: 'geosite', values: ['youtube'] }),
      rule({ id: 'proc', type: 'processName', values: ['Telegram'] }),
      rule({ id: 'port', type: 'port', values: ['443'] }),
      rule({ id: 'rx', type: 'domainRegex', values: ['^stun\\..+'] }),
      rule({ id: 'sfx', type: 'domainSuffix', values: ['a.com'] }),
    ];
    const targets = ruleAppendTargets(rules, 'www.youtube.com');
    expect(
      [...new Set(targets.map((t) => t.ruleId))],
      '有规则从清单里消失了 —— 用户会以为规则不存在'
    ).toEqual(['ip', 'geo', 'proc', 'port', 'rx', 'sfx']);
    // 默认 or ⇒ 这一批全都可追加（前五条新开条件，最后一条追加进既有条件）。
    expect(targets.every((t) => t.block === null)).toBe(true);
  });

  it('无域名条件 + 默认 or ⇒ **新开条件腿**（condIndex = -1，类型 = NEW_COND_TYPE）', () => {
    const rules = [
      rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] }),
      rule({ id: 'or', type: 'geosite', values: ['cn'], combineMode: 'or' }),
    ];
    for (const t of ruleAppendTargets(rules, 'x.example.com')) {
      expect(t.block, '默认/显式 or 的规则被挡住了 —— 生成侧那两条路在 or 下都是 OR').toBeNull();
      expect(t.condIndex, '新开腿必须用 -1 寻址，不能假装指向某个已存在的条件').toBe(-1);
      expect(t.type).toBe(NEW_COND_TYPE);
      expect(t.values).toEqual([]);
    }
  });

  it('只有 `domainRegex` 条件 ⇒ 新开一个**字面量**条件，绝不把主机名塞进正则表', () => {
    const rules = [rule({ id: 'rx', type: 'domainRegex', values: ['^stun\\..+'] })];
    const [t] = ruleAppendTargets(rules, 'stun.l.google.com');
    expect(t.block).toBeNull();
    expect(t.condIndex).toBe(-1);
    expect(t.type, '值被塞进 domainRegex —— `.` 会从字面点变成通配符').not.toBe('domainRegex');
    const next = appendValueToRule(rules[0], t, 'stun.l.google.com')!;
    expect(next.conditions!.map((c) => c.type)).toEqual(['domainRegex', NEW_COND_TYPE]);
    expect(next.conditions![0].values, '正则条件被动过').toEqual(['^stun\\..+']);
  });

  it('多个域名族条件 ⇒ 各出一项；非域名条件不单独出项（同一条规则只出可追加的那几个）', () => {
    const rules = [
      multi('a', [
        { type: 'ipCidr', values: ['10.0.0.0/8'] },
        { type: 'domainSuffix', values: ['a.com'] },
        { type: 'domainKeyword', values: ['ads'] },
      ]),
    ];
    const targets = ruleAppendTargets(rules, 'x.example.com');
    expect(targets.map((t) => t.condIndex)).toEqual([1, 2]);
    for (const t of targets) {
      const conds = ruleConditions(rules[0]);
      expect(conds[t.condIndex].type, '候选的类型与该位置上的条件不一致').toBe(t.type);
      expect(APPENDABLE_HOST_TYPES).toContain(t.type);
    }
  });

  it('每一行都带得出规则身份（`ruleType`/`ruleValues` = 首条件镜像）—— 无备注的行才认得出是哪条', () => {
    const rules = [
      rule({ id: 'g1', type: 'geosite', values: ['netflix'] }),
      rule({ id: 'g2', type: 'geosite', values: ['disney'] }),
      multi('m', [
        { type: 'ipCidr', values: ['10.0.0.0/8'] },
        { type: 'domainSuffix', values: ['a.com'] },
      ]),
    ];
    expect(
      ruleAppendTargets(rules, 'x.example.com').map((t) => [t.ruleType, t.ruleValues]),
      '两条无备注的 geosite 规则在 UI 上会长得一模一样'
    ).toEqual([
      ['geosite', ['netflix']],
      ['geosite', ['disney']],
      ['ipCidr', ['10.0.0.0/8']],
    ]);
  });

  it('结构不变式：`block !== null` 的行恒不指向任何条件（condIndex = -1，点了也写不进去）', () => {
    const rules = [
      rule({ id: 'and', type: 'ipCidr', values: ['10.0.0.0/8'], combineMode: 'and' }),
      multi('and2', [
        { type: 'ipCidr', values: ['10.0.0.0/8'] },
        { type: 'port', values: ['443'] },
      ], { combineMode: 'and' }),
      rule({ id: 'has', type: 'domainSuffix', values: ['x.example.com'] }),
    ];
    for (const t of ruleAppendTargets(rules, 'x.example.com').filter((x) => x.block !== null)) {
      if (t.block === 'contains') continue; // 「已包含」指向的是真实条件
      expect(t.condIndex, '置灰项指向了一个真实条件 —— 会被误当成可写目标').toBe(-1);
    }
  });
});

describe('② 置灰分类：三类原因各自成立，不用笼统的「不可追加」', () => {
  it('`andMode`：`combineMode:"and"` 且没有能收值的域名条件 ⇒ 置灰（核心线，变异靶）', () => {
    const rules = [
      rule({ id: 'and1', type: 'ipCidr', values: ['10.0.0.0/8'], combineMode: 'and' }),
      multi('and2', [
        { type: 'ipCidr', values: ['10.0.0.0/8'] },
        { type: 'processName', values: ['Telegram'] },
      ], { combineMode: 'and' }),
    ];
    const targets = ruleAppendTargets(rules, 'x.example.com');
    expect(targets.map((t) => t.block), '给 and 规则新开条件 = 把扩宽写成求交').toEqual([
      'andMode',
      'andMode',
    ]);
  });

  it('`and` 规则**已有**域名条件时仍可追加值（单条件内多值恒 OR，不许一刀切）', () => {
    const base = multi('and', [
      { type: 'domainSuffix', values: ['a.com'] },
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
    ], { combineMode: 'and' });
    const targets = ruleAppendTargets([base], 'b.com');
    expect(targets).toHaveLength(1);
    expect(targets[0].block, '追加值只是把那个条件的取值集合扩宽，与 and/or 无关').toBeNull();
    expect(targets[0].condIndex).toBe(0);
  });

  it('`valueUnfit`：值本身进不了域名字面量条件（IPv6 主机名 vs 域名形状）', () => {
    const rules = [
      rule({ id: 'sfx', type: 'domainSuffix', values: ['a.com'] }),
      rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] }),
      rule({ id: 'kw', type: 'domainKeyword', values: ['ads'] }),
    ];
    // `isRuleableHost` 只判「含 . 或 :」⇒ IPv6 会走到这里，三类规则都必须收不下它。
    const targets = ruleAppendTargets(rules, '2606:4700::1');
    expect(targets.map((t) => [t.ruleId, t.block])).toEqual([
      ['sfx', 'valueUnfit'], // 有 domainSuffix 条件但值不合形状，新开同类型条件同样收不下
      ['ip', 'valueUnfit'], // 无域名条件，新开腿的 NEW_COND_TYPE 也收不下
      // 关键词条件曾经**收下**它（判据只是「非空」）⇒ 落成 `domain_keyword: ["2606:4700::1"]`，
      // 而 DNS 名不含冒号 ⇒ 永不命中，内核不报错、用户以为配好了。现按含冒号硬拒。
      ['kw', 'valueUnfit'],
    ]);
  });

  it('`valueUnfit` 压过 `andMode`：值本身不合形状时，改成 or 也没用，原因必须指向值', () => {
    const rules = [rule({ id: 'and', type: 'ipCidr', values: ['10.0.0.0/8'], combineMode: 'and' })];
    expect(ruleAppendTargets(rules, '2606:4700::1')[0].block).toBe('valueUnfit');
  });

  it('`contains`：已包含该值（大小写不敏感，口径同 rule-cond 的 selectedValueSet）', () => {
    const rules = [rule({ id: 'r', type: 'domainSuffix', values: [' Example.COM '] })];
    expect(ruleAppendTargets(rules, 'example.com')[0].block).toBe('contains');
    expect(ruleAppendTargets(rules, 'other.com')[0].block).toBeNull();
  });

  it('禁用规则**也是**合法目标（写入目标 ≠ 冲突告警），但标出禁用态', () => {
    const rules = [rule({ id: 'off', type: 'domainSuffix', values: ['a.com'], enabled: false })];
    const targets = ruleAppendTargets(rules, 'b.com');
    expect(targets).toHaveLength(1);
    expect(targets[0].block, '禁用只是标注，不是拦截').toBeNull();
    expect(targets[0].enabled).toBe(false);
  });
});

describe('② 排序与检索：可追加在前、置灰沉底，同档内规则顺序（= 优先级）不变', () => {
  const rules = [
    rule({ id: 'and', type: 'ipCidr', values: ['10.0.0.0/8'], combineMode: 'and' }),
    rule({ id: 'has', type: 'domainSuffix', values: ['b.com'] }),
    rule({ id: 'dup', type: 'domain', values: ['x.example.com'] }),
    rule({ id: 'new', type: 'geosite', values: ['cn'] }),
  ];

  it('三档：可追加 → 已包含 → 其余置灰', () => {
    const sorted = sortAppendTargets(ruleAppendTargets(rules, 'x.example.com'));
    expect(sorted.map((t) => t.ruleId)).toEqual(['has', 'new', 'dup', 'and']);
    expect(sorted.map((t) => t.block)).toEqual([null, null, 'contains', 'andMode']);
  });

  it('同档内不打乱规则顺序（先匹配先生效，顺序即优先级）', () => {
    const many = [
      rule({ id: 'a', type: 'domainSuffix', values: ['a.com'] }),
      rule({ id: 'b', type: 'domainSuffix', values: ['b.com'] }),
      rule({ id: 'c', type: 'domainSuffix', values: ['c.com'] }),
    ];
    const sorted = sortAppendTargets(ruleAppendTargets(many, 'x.example.com'));
    expect(sorted.map((t) => t.ruleIndex)).toEqual([0, 1, 2]);
  });

  it('检索按规则名 / 类型 / 已有值三路命中，**置灰项同样参与**', () => {
    const src = [
      rule({ id: 'r1', type: 'domainSuffix', values: ['netflix.com'], remarks: '流媒体解锁' }),
      rule({ id: 'r2', type: 'domainKeyword', values: ['analytics'], remarks: '广告拦截' }),
      rule({ id: 'r3', type: 'ipCidr', values: ['10.0.0.0/8'], remarks: '内网直连', combineMode: 'and' }),
    ];
    const all = ruleAppendTargets(src, 'x.com');
    expect(all.find((t) => t.ruleId === 'r3')!.block).toBe('andMode');
    expect(matchAppendTargets(all, '')).toHaveLength(3);
    expect(matchAppendTargets(all, '解锁').map((t) => t.ruleId)).toEqual(['r1']);
    expect(matchAppendTargets(all, 'netflix').map((t) => t.ruleId)).toEqual(['r1']);
    expect(matchAppendTargets(all, 'keyword').map((t) => t.ruleId)).toEqual(['r2']);
    expect(
      matchAppendTargets(all, '内网').map((t) => t.ruleId),
      '置灰项搜不到 —— 用户会以为这条规则不存在'
    ).toEqual(['r3']);
    expect(matchAppendTargets(all, '10.0.0.0').map((t) => t.ruleId)).toEqual(['r3']);
  });

  it('多条件规则：任一条件的值都能把它搜出来（检索语料按整条规则取）', () => {
    const src = [
      multi('m', [
        { type: 'ipCidr', values: ['192.168.7.0/24'] },
        { type: 'domainSuffix', values: ['a.com'] },
      ]),
    ];
    const all = ruleAppendTargets(src, 'x.com');
    expect(matchAppendTargets(all, '192.168.7').map((t) => t.ruleId)).toEqual(['m']);
  });
});

describe('④ 写入侧：镜像不变式 + 新开条件腿', () => {
  const mirrorHolds = (r: Rule) => {
    const first = ruleConditions(r)[0];
    expect(r.type, '镜像 type 与 conditions[0] 漂开').toBe(first.type);
    expect(r.values, '镜像 values 与 conditions[0] 漂开').toEqual(first.values);
  };

  it('单条件规则：追加后镜像同步', () => {
    const base = rule({ id: 'r', type: 'domainSuffix', values: ['a.com'] });
    const t = ruleAppendTargets([base], 'b.com')[0];
    const next = appendValueToRule(base, t, 'b.com')!;
    expect(next.values).toEqual(['a.com', 'b.com']);
    expect(next.conditions).toBeUndefined(); // 单条件形态不变
    expect(next.combineMode).toBeUndefined();
    mirrorHolds(next);
  });

  it('多条件规则、追加进**首条件**：镜像必须跟着走（这一条是变异靶）', () => {
    const base = multi('r', [
      { type: 'domainSuffix', values: ['a.com'] },
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
    ], { combineMode: 'and' });
    const t = ruleAppendTargets([base], 'b.com').find((x) => x.condIndex === 0)!;
    const next = appendValueToRule(base, t, 'b.com')!;
    expect(next.conditions![0].values).toEqual(['a.com', 'b.com']);
    expect(next.values, '写入侧没同步镜像 —— 别指望读盘侧 sanitize 兜底').toEqual(['a.com', 'b.com']);
    mirrorHolds(next);
    expect(next.combineMode, 'combineMode 被改写了 = 规则的逻辑形状变了').toBe('and');
  });

  it('多条件规则、追加进**非首条件**：首条件与镜像都不许动', () => {
    const base = multi('r', [
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
      { type: 'domainKeyword', values: ['ads'] },
    ]);
    const t = ruleAppendTargets([base], 'tracker.example.com')[0];
    expect(t.condIndex).toBe(1);
    const next = appendValueToRule(base, t, 'tracker.example.com')!;
    expect(next.conditions![1].values).toEqual(['ads', 'tracker.example.com']);
    expect(next.conditions![0].values).toEqual(['10.0.0.0/8']);
    mirrorHolds(next);
  });

  it('新开条件：挂在**末尾**、只含这一个值，镜像与 combineMode 都不动', () => {
    const base = rule({ id: 'r', type: 'ipCidr', values: ['10.0.0.0/8'] });
    const t = ruleAppendTargets([base], 'x.example.com')[0];
    const next = appendValueToRule(base, t, 'x.example.com')!;
    expect(next.conditions).toEqual([
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
      { type: NEW_COND_TYPE, values: ['x.example.com'] },
    ]);
    expect(next.combineMode, '新开条件不许顺手写 combineMode —— 缺省即 or，写了就是改规则').toBeUndefined();
    mirrorHolds(next);
  });

  it('新开条件（多条件规则）：既有条件一个不动，镜像仍 = conditions[0]', () => {
    const base = multi('r', [
      { type: 'geosite', values: ['cn'] },
      { type: 'port', values: ['443'] },
    ], { combineMode: 'or' });
    const t = ruleAppendTargets([base], 'x.example.com')[0];
    const next = appendValueToRule(base, t, 'x.example.com')!;
    expect(next.conditions!.map((c) => c.type)).toEqual(['geosite', 'port', NEW_COND_TYPE]);
    expect(next.conditions!.slice(0, 2)).toEqual(base.conditions);
    expect(next.combineMode).toBe('or');
    mirrorHolds(next);
  });

  it('新开条件在写入侧同样拦 `and`：判据不是只画在 UI 上（③ 的第二道，变异靶）', () => {
    const or = rule({ id: 'r', type: 'ipCidr', values: ['10.0.0.0/8'] });
    const t = ruleAppendTargets([or], 'x.example.com')[0];
    expect(appendValueToRule(or, t, 'x.example.com'), '前提校验：or 下这一步是成功的').not.toBeNull();
    // 从「打开选择器」到「点下去」之间规则被改成了 and ⇒ 必须放弃，不能把扩宽写成求交。
    const drifted = { ...or, combineMode: 'and' as const };
    expect(appendValueToRule(drifted, t, 'x.example.com'), 'and 规则被新开了条件').toBeNull();
  });

  it('新开条件的漂移防御：规则已经有能收下这个值的域名条件 ⇒ 放弃（不挂多余条件）', () => {
    const base = rule({ id: 'r', type: 'ipCidr', values: ['10.0.0.0/8'] });
    const t = ruleAppendTargets([base], 'x.example.com')[0];
    const drifted = multi('r', [
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
      { type: 'domainSuffix', values: ['a.com'] },
    ]);
    expect(appendValueToRule(drifted, t, 'x.example.com')).toBeNull();
  });

  it('置灰的目标恒写不进去（`block !== null` ⇒ null，UI 与写入侧口径一致）', () => {
    const and = rule({ id: 'r', type: 'ipCidr', values: ['10.0.0.0/8'], combineMode: 'and' });
    const t = ruleAppendTargets([and], 'x.example.com')[0];
    expect(t.block).toBe('andMode');
    expect(appendValueToRule(and, t, 'x.example.com')).toBeNull();
  });

  it('`{...base}` 起底：`tlsSpoof` 等非模型字段一个都不丢', () => {
    const base = rule({
      id: 'r',
      type: 'domainSuffix',
      values: ['a.com'],
      remarks: '解锁',
      targetServerId: 'srv-1',
      bypassFakeIP: true,
      tlsSpoof: 'www.apple.com',
      tlsSpoofMethod: 'wrong-md5',
    });
    const next = appendValueToRule(base, ruleAppendTargets([base], 'b.com')[0], 'b.com')!;
    expect(next.tlsSpoof).toBe('www.apple.com');
    expect(next.tlsSpoofMethod).toBe('wrong-md5');
    expect(next.targetServerId).toBe('srv-1');
    expect(next.bypassFakeIP).toBe(true);
    expect(next.remarks).toBe('解锁');
    expect(next.id).toBe('r');
  });

  it('无事可做 / 目标漂移 ⇒ null（不写）', () => {
    const base = rule({ id: 'r', type: 'domainSuffix', values: ['a.com'] });
    const t = ruleAppendTargets([base], 'b.com')[0];
    expect(appendValueToRule(base, t, ' A.COM '), '已包含该值').toBeNull();
    expect(appendValueToRule(base, t, '  '), '空值').toBeNull();
    expect(appendValueToRule({ ...base, id: 'other' }, t, 'b.com'), 'id 对不上').toBeNull();
    // 漂移：选中之后那个位置换成了别的类型
    const drifted = rule({ id: 'r', type: 'domainKeyword', values: ['a.com'] });
    expect(appendValueToRule(drifted, t, 'b.com'), '条件类型已变').toBeNull();
    // 漂移：条件被删光，位置越界
    const shrunk = multi('r', [{ type: 'ipCidr', values: ['10.0.0.0/8'] }]);
    const t2 = ruleAppendTargets([multi('r', [
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
      { type: 'domainSuffix', values: ['a.com'] },
    ])], 'b.com')[0];
    expect(appendValueToRule(shrunk, t2, 'b.com'), 'condIndex 越界').toBeNull();
  });
});

describe('⑤ 优先级提示：算得出遮蔽，但只能是提示', () => {
  it('先匹配先生效：命中集 + 第一条命中的下标', () => {
    const rules = [
      rule({ id: 'off', type: 'domainSuffix', values: ['example.com'], enabled: false }),
      rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] }),
      rule({ id: 'hit1', type: 'domainSuffix', values: ['example.com'] }),
      rule({ id: 'hit2', type: 'domainKeyword', values: ['exam'] }),
    ];
    const cov = analyzeDomainCoverage(rules, 'www.example.com');
    expect([...cov.coveredIds].sort()).toEqual(['hit1', 'hit2']);
    expect(cov.firstId, '禁用规则不下发，遮蔽不了任何东西').toBe('hit1');
    expect(cov.firstIndex).toBe(2);
  });

  it('只有**更靠前**的命中才算遮蔽（追加进第一条命中的那一条不是被遮蔽）', () => {
    const rules = [
      rule({ id: 'hit1', type: 'domainSuffix', values: ['example.com'] }),
      rule({ id: 'later', type: 'domainSuffix', values: ['other.com'] }),
    ];
    const cov = analyzeDomainCoverage(rules, 'www.example.com');
    const targets = ruleAppendTargets(rules, 'www.example.com');
    expect(isShadowedTarget(cov, targets.find((t) => t.ruleId === 'hit1')!)).toBe(false);
    expect(isShadowedTarget(cov, targets.find((t) => t.ruleId === 'later')!)).toBe(true);
  });

  it('零命中 ⇒ 无遮蔽（firstIndex = -1，不得退化成「everything shadowed」）', () => {
    const rules = [rule({ id: 'r', type: 'domainSuffix', values: ['other.com'] })];
    const cov = analyzeDomainCoverage(rules, 'www.example.com');
    expect(cov.firstIndex).toBe(-1);
    expect(cov.firstId).toBeNull();
    expect(isShadowedTarget(cov, ruleAppendTargets(rules, 'www.example.com')[0])).toBe(false);
  });

  it('`domainRegex` 里的逗号不会被拼接口径拆坏（值用换行拼，不用逗号）', () => {
    const rules = [rule({ id: 'r', type: 'domainRegex', values: ['^a{1,3}\\.example\\.com$'] })];
    expect(analyzeDomainCoverage(rules, 'aa.example.com').firstId).toBe('r');
  });
});

describe('⑤ 提示不是门：菜单腿不得出现任何禁用态', () => {
  const strip = (s: string) =>
    s
      .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
      .replace(/(^|[^:])\/\/.*$/gm, (m, p1: string) => p1 + ' '.repeat(m.length - p1.length));
  const MENU = fileURLToPath(new URL('../host-rule-menu.tsx', import.meta.url));
  const src = strip(readFileSync(MENU, 'utf8'));

  it('自检：读到的是真源码，且它**真的**用了覆盖判据（否则下面那条恒绿）', () => {
    expect(src.length).toBeGreaterThan(1000);
    expect(src, '菜单没在用 analyzeDomainCoverage —— 本组断言失去对象').toContain('analyzeDomainCoverage');
  });

  it('去注释后源码里一个 `disabled` 都没有（据启发式禁用「新建」= 把提示升格成门）', () => {
    expect(
      [...src.matchAll(/\bdisabled\b/g)].map((m) => m[0]),
      'host-rule-menu.tsx 出现了禁用态。`analyzeDomainCoverage` 是**客户端启发式**（geoip 恒判命中、' +
        'geosite 退化成子串、IP 只比前两段），权威匹配器在 sing-box 内核 —— 据它阻断「新建规则」' +
        '会把一个会误报的判据变成用户绕不过去的闸门。它只许改排序与一行说明。'
    ).toEqual([]);
  });

  it('选择器里的禁用态只由 `block` 判据决定，不由遮蔽/覆盖判据决定', () => {
    const dlg = strip(readFileSync(fileURLToPath(new URL('./RulePickDialog.tsx', import.meta.url)), 'utf8'));
    const exprs = [...dlg.matchAll(/disabled=\{([^}]*)\}/g)].map((m) => m[1].trim());
    expect(exprs, 'RulePickDialog 里一个 disabled 都没有 ⇒ 下面那条恒绿').not.toEqual([]);
    for (const e of exprs) {
      expect(e, `禁用表达式 \`${e}\` 引用了遮蔽/覆盖判据 —— 那是提示，不是门`).not.toMatch(
        /shadow|Shadow|coverage|Coverage|covered/
      );
      expect(e, `禁用表达式 \`${e}\` 没走 \`block\` —— 置灰理由必须来自 rule-append 的可测判据`).toContain(
        'block'
      );
    }
  });

  it('选择器**列全部规则**：不得在渲染层再过滤掉置灰项（本轮根因的第二道门）', () => {
    const dlg = strip(readFileSync(fileURLToPath(new URL('./RulePickDialog.tsx', import.meta.url)), 'utf8'));
    // 注意 `[^;]{0,120}?` 而不是 `[^)]*`：箭头函数的形参括号 `(t) =>` 里就有一个 `)`，
    // 用 `[^)]*` 会在那里停住，`.filter((t) => t.block === null)` 这种最典型的写法反而漏掉
    // （这条变异实跑过，第一版正则确实没抓住）。
    expect(dlg, 'ruleAppendTargets 的产物被渲染层 filter 掉了一部分 —— 清单又会缺规则').not.toMatch(
      /\.filter\([^;]{0,120}?\bblock\b/
    );
    expect(dlg, '没在用 sortAppendTargets ⇒ 置灰项不会沉底').toContain('sortAppendTargets');
  });
});
