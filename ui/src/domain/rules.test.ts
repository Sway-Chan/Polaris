/**
 * 规则类型**描述符表**的门 —— 钉死「加第 16 个类型只改描述符表，不动弹窗 JSX」。
 *
 * # 为什么这条要有门，而不是写在注释里
 *
 * 「反补丁」是个**结构性**主张：它只有在「真去加一个类型时不必碰 JSX」这件事上成立才算数。
 * 注释挡不住下一个人在 `RuleDialog.tsx` 里写一句 `if (c.t === 'ruleSet')` —— 类型对、
 * 构建过、所有既有测试绿，而那张描述符表从那一刻起就不再是唯一物料源了（下一个人只会照抄）。
 *
 * # 三条断言的分工
 *
 *  ① 覆盖：`RULE_TYPES` 与 `RULE_TYPE_IDS` 严格同集 —— 表漏一份 = 弹窗读到 undefined 直接崩。
 *  ② 无字面量：`RuleDialog.tsx` 去注释后不得出现任何 `RuleType` id（引号字面量 / 对象字面量键）。
 *  ③ 描述符生成的动态 i18n 键在五种语言中均存在且非空。
 *
 * # 射程（如实记账）
 *
 * ② 抓**引号字面量**与**对象字面量键**两种形态。抓不到：拼接构造（`'rule' + 'Set'`）、
 * 从别处 import 一个逐类型常量表、把逐类型分支挪进本目录下另一个 `.ts`（那正是本次做的事 ——
 * `rule-cond.ts` 只按描述符字段分派，但门管不住它将来退化）。这三种要靠 review。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  RULE_TYPE_IDS,
  RULE_TYPES,
  RULE_CATEGORY_ORDER,
  ruleCategoryLabelKey,
  ruleTypeHintKey,
  ruleTypeNameKey,
  ruleTypePlaceholderKey,
  validateRuleValue,
} from './rules';
import type { RuleCategory } from './rules';

const DIALOG = fileURLToPath(new URL('../components/dialogs/RuleDialog.tsx', import.meta.url));

/** 去注释：本仓的注释逐字引用了这些标识符，不去掉就是拿注释当证据。 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');

describe('规则类型描述符表：15 份全覆盖', () => {
  it('① RULE_TYPES 与 RULE_TYPE_IDS 严格同集（漏一份 = 弹窗读 undefined 直接崩）', () => {
    expect(RULE_TYPE_IDS.length, 'RULE_TYPE_IDS 空了 —— 下面的断言会空跑').toBe(15);
    expect(Object.keys(RULE_TYPES).sort()).toEqual([...RULE_TYPE_IDS].sort());
    for (const id of RULE_TYPE_IDS) {
      expect(RULE_TYPES[id].id, `${id} 的描述符 id 与表键不一致`).toBe(id);
    }
  });

  it('② 分类顺序覆盖全部出现过的分类', () => {
    const used = new Set<RuleCategory>(RULE_TYPE_IDS.map((id) => RULE_TYPES[id].category));
    expect([...used].sort(), '有类型的分类不在 RULE_CATEGORY_ORDER 里 ⇒ 它的选项在下拉里整组消失').toEqual(
      [...RULE_CATEGORY_ORDER].filter((c) => used.has(c)).sort()
    );
  });

  it('③ 候选源自洽：free 无池字段；pool 的 addressing 与 pool 相配', () => {
    for (const id of RULE_TYPE_IDS) {
      const s = RULE_TYPES[id].source;
      if (s.kind === 'free') continue;
      expect(s.searchFields.length, `${id} 的池没有可检索字段 ⇒ 搜索框恒无结果`).toBeGreaterThan(0);
      // 寻址词汇按池划分：geoTag 池只有 bare / res-id，process 池只有 proc-name / proc-path。
      // 混用会让池提供者按错误的字段取值（表现成「勾了却填进去一个空串」）。
      const allowed = s.pool === 'geoTag' ? ['bare', 'res-id'] : ['proc-name', 'proc-path'];
      expect(allowed, `${id}: pool=${s.pool} 与 addressing=${s.addressing} 不相配`).toContain(
        s.addressing
      );
    }
  });
});

describe('RuleDialog.tsx 不得出现任何 RuleType 字面量（= 加第 16 个类型不必动 JSX）', () => {
  const src = code(readFileSync(DIALOG, 'utf8'));

  it('自检：读到了源码且去注释后仍是可断言的代码', () => {
    expect(src.length, 'RuleDialog.tsx 读空了 —— 被改名/移走了？').toBeGreaterThan(5000);
    expect(src, '去注释把源码吃光了').toContain('import');
  });

  it('自检：本条正则确实抓得到 RuleType 字面量（拿 domain/rules.ts 自己当正对照）', () => {
    // 描述符表本身就是一大堆 RuleType 字面量键 —— 扫它必须命中，否则下面那条是恒绿假门。
    const self = code(readFileSync(fileURLToPath(new URL('./rules.ts', import.meta.url)), 'utf8'));
    expect(hits(self).length, '正则在描述符表上一个都没抓到 ⇒ 判据失效').toBeGreaterThan(10);
  });

  it('弹窗源码零命中（写 `c.t === \'ruleSet\'` 或 `{ ruleSet: … }` 都会让这条转红）', () => {
    expect(
      hits(src),
      '弹窗里出现了 RuleType 字面量 —— 逐类型差异请落回 domain/rules.ts 的描述符字段' +
        '（source.kind / source.pool / source.addressing / test / category），不要在 JSX 里开分支。'
    ).toEqual([]);
  });

  /** 命中：`'id'` / `"id"` 引号字面量，或 `{ id: …` / `, id: …` 对象字面量键。 */
  function hits(text: string): string[] {
    const out: string[] = [];
    for (const id of RULE_TYPE_IDS) {
      const re = new RegExp(`(['"\`])${id}\\1|(?:^|[{,])\\s*${id}\\s*:`, 'gm');
      for (const m of text.matchAll(re)) out.push(`${id} @ ${JSON.stringify(m[0].trim())}`);
    }
    return out.sort();
  }
});

/* ───────────────────────────────────────────────────────────────────────────
 * i18n 接线 —— 描述符声明的键，五个语种都要真的解析得出非空文案
 *
 * 为什么必须单独立这道门：`locale-parity.test.ts` 的可寻址性扫描只抓**字面量** `t('a.b.c')`，
 * 而这三组键是**动态**生成的（`ruleTypeNameKey(id)` 等）—— 它一个都扫不到。历史上正是这个
 * 盲区让 `t('rules.type.${id}')`（单数 type）与 `t('rules.cat.<x>')` 两个 namespace 在五个
 * locale 里**全都不存在**，15 个类型名 + 5 个分类名一律落 defaultValue 中文，而 CI 恒绿。
 *
 * 「非空」也要断言：`rules.types.ruleSet.placeholder` 与 `rules.typeHints.ruleSet` 此前是空串。
 * i18next 把空串当**有效译文**返回（不回落 defaultValue）⇒ 那两处在所有语种下都是空的。
 * ─────────────────────────────────────────────────────────────────────────── */
const LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;
const localeData = new Map(
  LOCALES.map((n) => [
    n,
    JSON.parse(
      readFileSync(fileURLToPath(new URL(`../i18n/locales/${n}.json`, import.meta.url)), 'utf8')
    ) as Record<string, unknown>,
  ])
);
/** 按点分路径取值（本组键在 locale 里都是严格嵌套，无扁平键形态）。 */
const lookup = (data: unknown, key: string): unknown =>
  key.split('.').reduce<unknown>((cur, seg) => {
    if (cur === null || typeof cur !== 'object') return undefined;
    return (cur as Record<string, unknown>)[seg];
  }, data);

describe('描述符的 i18n 键：五语种齐全且非空', () => {
  const keys = [
    ...RULE_TYPE_IDS.flatMap((id) => [
      ruleTypeNameKey(id),
      ruleTypeHintKey(id),
      ruleTypePlaceholderKey(id),
    ]),
    ...RULE_CATEGORY_ORDER.map((c) => ruleCategoryLabelKey(c)),
  ];

  it('自检：键数与语种数都在预期量级（防空跑恒绿）', () => {
    expect(keys.length, '15×3 + 5 = 50').toBe(50);
    expect(localeData.size).toBe(5);
  });

  for (const loc of LOCALES) {
    it(`${loc}: 50 个键全部解析出非空字符串`, () => {
      const bad = keys
        .map((k) => [k, lookup(localeData.get(loc), k)] as const)
        .filter(([, v]) => typeof v !== 'string' || v.trim() === '')
        .map(([k, v]) => `${k} = ${JSON.stringify(v)}`);
      expect(
        bad.sort(),
        `${loc} 里这些键缺失或为空 —— 动态键不在 locale-parity 的扫描射程内，只有本门会说话`
      ).toEqual([]);
    });
  }
});

/**
 * `domainKeyword` 的冒号闸门 —— 与 Rust 权威 `rule_validate.rs` 的 `domain_keyword_*` 同源对拍。
 *
 * 背景：`domain_keyword` 在 sing-box 里是对**域名**做子串匹配。DNS 名里不可能出现 `:`，故含冒号的
 * 关键词恒不命中；而内核不会因此报错 —— 用户填一个 IPv6 字面量进去，得到的是一条静默失效的死规则。
 * 此前判据只是「非空」，`ruleAppendTargets` 因此把 IPv6 主机名当成合法追加目标（见
 * `dialogs/rule-append.test.ts` 的 `valueUnfit` 用例）。
 */
describe('domainKeyword 拒含冒号值（IPv6 字面量不得落成永不命中的 domain_keyword）', () => {
  it('IPv6 字面量一律拒：裸写 / 带方括号 / 压缩 / v4-mapped', () => {
    for (const v of [
      '2001:db8::1',
      '[2001:db8::1]', // URL 写法，方括号形式同样含冒号
      '::1',
      'fe80::1%eth0',
      '::ffff:192.168.1.1',
      '2606:4700::1',
    ]) {
      expect(validateRuleValue('domainKeyword', v), `${v} 应被拒：含冒号的关键词永不命中`).toBe(false);
    }
  });

  it('任何含冒号的值都拒（判据是「含冒号」而非「像 IPv6」—— foo:bar 同样永不命中）', () => {
    expect(validateRuleValue('domainKeyword', 'foo:bar')).toBe(false);
    expect(validateRuleValue('domainKeyword', 'example.com:443')).toBe(false);
  });

  /** 反向对照：别把正常关键词误判成 IP。这条挂了说明闸门收得过宽，砍了合法能力。 */
  it('正常域名关键词照收（含 IPv4 字面量 —— 它能命中 nip.io / in-addr.arpa 这类真实域名）', () => {
    for (const v of [
      'ads',
      'googlevideo',
      'example.com',
      'cdn-',
      '1.2.3.4', // `1.2.3.4.nip.io`、`4.3.2.1.in-addr.arpa` 都是真实可命中的域名
      '10.0.0.1',
      'v6', // 名字里带 v6 不等于 IPv6
    ]) {
      expect(validateRuleValue('domainKeyword', v), `${v} 应被接受：它是合法关键词`).toBe(true);
    }
  });

  it('空 / 纯空白仍然拒（原有语义不变）', () => {
    expect(validateRuleValue('domainKeyword', '')).toBe(false);
    expect(validateRuleValue('domainKeyword', '   ')).toBe(false);
  });

  /** 同族一致性：域名族三个字面量类型对 IPv6 现在口径统一（此前只有 keyword 漏）。 */
  it('domain / domainSuffix 同样拒 IPv6（DOMAIN_RE 无冒号字符类）', () => {
    for (const t of ['domain', 'domainSuffix'] as const) {
      expect(validateRuleValue(t, '2001:db8::1')).toBe(false);
      expect(validateRuleValue(t, '[2001:db8::1]')).toBe(false);
    }
  });
});
