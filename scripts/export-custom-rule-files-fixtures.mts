/**
 * export-custom-rule-files-fixtures.mts — 导出 planCustomRule + condMatcherFields 金样对拍 fixture。
 *
 * import 上游 `custom-rule-files.ts`（纯函数，不拖 electron）。遍历覆盖矩阵输出 JSON。
 * Rust 侧 tests/golden_custom_rule_files.rs 读此 JSON 逐条对拍。
 *
 * 覆盖：planCustomRule（inline/ext/ext-skip/logical/mergeable/fail-closed/AND-drop）+
 *       condMatcherFields（各 EXT type + 端口解析 + 域名剥通配符）。
 *
 * 用法：REF_REPO=<上游仓根> npx tsx <本仓>/scripts/export-custom-rule-files-fixtures.mts
 *       > /home/sway/Code/polaris/crates/config-engine/fixtures/custom-rule-files.json
 */
// 上游仓路径由环境变量 `REF_REPO` 注入：静态 import 的路径必须是字面量，写死等于把某台机器的
// 绝对路径钉进仓库。
const REF_REPO = process.env.REF_REPO;
if (!REF_REPO) {
  console.error('缺 REF_REPO 环境变量：需指向上游参考实现的仓库根目录');
  process.exit(2);
}
const { planCustomRule, condMatcherFields, EXT_TYPES } = (await import(
  `${REF_REPO}/src/main/services/custom-rule-files.ts`
)) as { planCustomRule: (...a: never[]) => unknown; condMatcherFields: (...a: never[]) => unknown; EXT_TYPES: readonly string[] };
type Rule = Record<string, unknown>;
type RuleCondition = Record<string, unknown>;

interface PlanCase {
  name: string;
  input: Rule;
  output: unknown;
}

const cond = (type: any, values: string[]): RuleCondition => ({ type, values } as RuleCondition);
const rule = (over: Partial<Rule> & { id: string; type: any; values: string[]; action: any }): Rule =>
  ({ enabled: true, ...over } as Rule);

function plan(name: string, r: Rule): PlanCase {
  return { name, input: r, output: planCustomRule(r) }
}

const pc: PlanCase[] = [];
pc.push(plan('plan_ext_single_domain', rule({ id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy' })));
pc.push(plan('plan_ext_single_ipCidr', rule({ id: 'r1', type: 'ipCidr', values: ['10.0.0.0/8'], action: 'direct' })));
pc.push(plan('plan_ext_single_port', rule({ id: 'r1', type: 'port', values: ['443', '1000-2000'], action: 'proxy' })));
pc.push(plan('plan_ext_or_mergeable', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy',
  conditions: [cond('domain', ['a.com']), cond('ipCidr', ['1.2.3.0/24'])],
})));
pc.push(plan('plan_ext_cross_dim_logical', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy',
  conditions: [cond('domain', ['a.com']), cond('port', ['443'])],
})));
pc.push(plan('plan_ext_and', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy', combineMode: 'and',
  conditions: [cond('domain', ['a.com']), cond('ipCidr', ['1.2.3.0/24'])],
})));
pc.push(plan('plan_ext_and_logical', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy', combineMode: 'and',
  conditions: [cond('domain', ['a.com']), cond('port', ['443'])],
})));
pc.push(plan('plan_ext_skip_empty', rule({ id: 'r1', type: 'domain', values: ['  '], action: 'proxy' })));
pc.push(plan('plan_ext_skip_and_drop', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy', combineMode: 'and',
  conditions: [cond('domain', ['a.com']), cond('domain', ['  '])],
})));
pc.push(plan('plan_ext_bypass_fakeip', rule({
  id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy', bypassFakeIP: true,
})));
pc.push(plan('plan_ext_bypass_fakeip_suffix', rule({
  id: 'r1', type: 'domainSuffix', values: ['example.com'], action: 'direct', bypassFakeIP: true,
})));
pc.push(plan('plan_inline_ruleset', rule({ id: 'r1', type: 'domain', values: ['a.com'], action: 'proxy', conditions: [cond('domain', ['a.com']), cond('ruleSet', ['res:r1'])] })));

// condMatcherFields：各 EXT type。
interface CondCase { name: string; input: RuleCondition; output: unknown; }
const cc: CondCase[] = [];
cc.push({ name: 'cond_domain', input: cond('domain', ['a.com', 'b.com']), output: condMatcherFields(cond('domain', ['a.com', 'b.com'])) });
cc.push({ name: 'cond_domainSuffix_wildcard', input: cond('domainSuffix', ['*.example.com', 'test.com']), output: condMatcherFields(cond('domainSuffix', ['*.example.com', 'test.com'])) });
cc.push({ name: 'cond_domainKeyword', input: cond('domainKeyword', ['ads']), output: condMatcherFields(cond('domainKeyword', ['ads'])) });
cc.push({ name: 'cond_domainRegex', input: cond('domainRegex', ['^foo.*']), output: condMatcherFields(cond('domainRegex', ['^foo.*'])) });
cc.push({ name: 'cond_ipCidr', input: cond('ipCidr', ['1.2.3.0/24']), output: condMatcherFields(cond('ipCidr', ['1.2.3.0/24'])) });
cc.push({ name: 'cond_sourceIpCidr', input: cond('sourceIpCidr', ['10.0.0.0/8']), output: condMatcherFields(cond('sourceIpCidr', ['10.0.0.0/8'])) });
cc.push({ name: 'cond_port_mixed', input: cond('port', ['443', '1000-2000', 'abc']), output: condMatcherFields(cond('port', ['443', '1000-2000', 'abc'])) });
cc.push({ name: 'cond_port_all_invalid', input: cond('port', ['abc', '0']), output: condMatcherFields(cond('port', ['abc', '0'])) });
cc.push({ name: 'cond_sourcePort', input: cond('sourcePort', ['1234', '5000-6000']), output: condMatcherFields(cond('sourcePort', ['1234', '5000-6000'])) });
cc.push({ name: 'cond_processName', input: cond('processName', ['chrome']), output: condMatcherFields(cond('processName', ['chrome'])) });
cc.push({ name: 'cond_processPath', input: cond('processPath', ['/usr/bin/curl']), output: condMatcherFields(cond('processPath', ['/usr/bin/curl'])) });
cc.push({ name: 'cond_non_ext_geosite', input: cond('geosite', ['cn']), output: condMatcherFields(cond('geosite', ['cn'])) });
cc.push({ name: 'cond_empty_values', input: cond('domain', ['  ']), output: condMatcherFields(cond('domain', ['  '])) });

process.stdout.write(JSON.stringify({ planCases: pc, condCases: cc, extTypes: [...EXT_TYPES] }, null, 2) + '\n');
