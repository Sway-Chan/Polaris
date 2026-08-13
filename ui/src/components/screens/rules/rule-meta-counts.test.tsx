/**
 * 规则行条件摘要（`RuleItem.tsx` 的 `RuleMetaCounts`）的门 —— 守三条，全部离线、零网络。
 *
 *  G1 **恒是「类型 ×n」，绝不平铺值**。原实现把每个条件的全部值逗号拼进 `.rmeta`，值一多就把规则行
 *     撑成好几行（真机 2026-08-03「券商」一条 21 个值）。陈先生定的是**一律计数、不做少量显值的分档**：
 *     分档会让行高随内容跳动，且值的条数只会随时间膨胀。
 *  G2 **值仍然拿得到，只是移进了 hover**。计数换掉平铺后若值本身消失，这就不是折叠而是删信息。
 *  G3 **连接符区分 AND / OR**。原实现用 `∧` / `,` 表达 combineMode，语义不能在改版里丢。
 *
 * 渲染断言用 `react-dom/server` 真渲染真组件（同 `hover-cards/mesh-info.test.tsx`）：本仓 vitest 是
 * `environment:'node'`，刻意不装 jsdom / testing-library，别为这道门破例。`t()` 桩返回 key 本身 ⇒
 * 断言落在**键**上而非中文措辞，改译文不误伤、换错键必然转红。
 *
 * **射程之外**（如实记）：hover 的开合/延迟/定位归 `HoverCard.tsx`；且 `HoverCardPanel` 在 `open=false`
 * 时 `return null`，`renderToStaticMarkup` 下面板恒未展开 ⇒ **G2 只能守源码接线，无法断言渲染产物**。
 * 单行截断的视觉效果（`.rmeta` 的 `text-overflow`）要真 CSSOM，node 下无从断言。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server';
import type { Rule } from '@/contracts/types';

vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: 'zh-CN' } }),
}));

const { RuleMetaCounts } = await import('./RuleItem');

/** 真机样本「券商」的形状：3 个条件、21 个值 —— 正是压垮原实现的那一条。 */
const BROKER = {
  id: 'r1',
  type: 'domainSuffix',
  action: 'proxy',
  enabled: true,
  remarks: '券商',
  combineMode: 'or',
  values: ['lbctrl.com'],
  conditions: [
    {
      type: 'domainSuffix',
      values: [
        'lbctrl.com', 'lbkrs.com', 'wbrks.com', 'futufin.com', 'futuhk.com',
        'futuhkapp.com', 'futuhn.com', 'futuholdings.com', 'moomoo.com',
        'moomooequity.com', 'moomootrustee.com', 'futuesop.com',
        'futuniuniu.com', 'futunn.com', 'futustatic.com',
      ],
    },
    { type: 'domainKeyword', values: ['longbridge', 'itiger'] },
    {
      type: 'ruleSet',
      values: ['res:geosite-futu', 'res:geosite-ibkr', 'res:geosite-itiger', 'res:geosite-longbridge'],
    },
  ],
} as unknown as Rule;

/** 单值规则：一律计数的另一端 —— 它也必须是 `×1`，不许回落成显示值本身。 */
const SINGLE = {
  id: 'r2',
  type: 'ruleSet',
  action: 'proxy',
  enabled: true,
  remarks: 'Adblock',
  values: ['res:geosite-adblock'],
} as unknown as Rule;

describe('规则行条件摘要', () => {
  /// G1：每个条件恰好一枚计数标签，且计数是该类型的值条数。
  ///
  /// 变异锁：把 `cond.values.length` 写成条件数 / 写死 1 → 三个数其中之一转红；
  /// 把 map 退回成逐值渲染 → `×15` 消失、转红。
  it('每类型只出一枚「×n」，n = 该类型的值条数', () => {
    const html = renderToStaticMarkup(<RuleMetaCounts rule={BROKER} />);
    expect(html).toContain('×15');
    expect(html).toContain('×2');
    expect(html).toContain('×4');
    expect(html.match(/rmeta-cnt/g)?.length).toBe(3);
  });

  /// G1 另一端：单值规则也走计数，不许分档回落成显示值。
  ///
  /// 变异锁：加一条「≤N 个值就直接显示值」的分支 → 转红（那正是陈先生否掉的方案）。
  it('单值规则同样是 ×1，不因值少而回落显示值本身', () => {
    const html = renderToStaticMarkup(<RuleMetaCounts rule={SINGLE} />);
    expect(html).toContain('×1');
    expect(html.match(/rmeta-cnt/g)?.length).toBe(1);
  });

  /// G2：折叠不等于删信息 —— 值必须仍被交给悬停面板。
  ///
  /// **为什么是源码守卫而不是渲染断言**：`HoverCardPanel` 在 `open=false` 时 `return null`
  /// （HoverCard.tsx:132），而 `renderToStaticMarkup` 下 hover 恒未展开 ⇒ 产物里必然没有值，
  /// 对它断言只会永远转红。故这一条只能守「接线在」：面板体里真的渲染了 `cond.values`。
  ///
  /// 变异锁：把面板里的 `cond.values.join(', ')` 删掉或换成计数 → 转红。守的是「这是折叠，不是丢数据」。
  it('值被交给了悬停面板（接线守卫）', () => {
    const src = readFileSync(
      fileURLToPath(new URL('./RuleItem.tsx', import.meta.url)),
      'utf8'
    );
    const at = src.indexOf('function CondCount');
    expect(at).toBeGreaterThan(-1);
    const body = src.slice(at, src.indexOf('function RuleMetaCounts'));
    expect(body).toContain('<HoverCardPanel');
    expect(body).toMatch(/cond\.values\.join\(/);
    expect(body).toContain('rmeta-vals');
  });

  /// G3：AND / OR 的连接符语义不能在改版里丢。
  ///
  /// 变异锁：分隔符写死成一种 → 两条断言其中之一转红。
  it('连接符区分 AND / OR', () => {
    const or = renderToStaticMarkup(<RuleMetaCounts rule={BROKER} />);
    expect(or).toContain('·');
    const and = renderToStaticMarkup(
      <RuleMetaCounts rule={{ ...BROKER, combineMode: 'and' } as Rule} />
    );
    expect(and).toContain('∧');
  });
});
