import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import type { Rule, RuleType } from '@/contracts/types';
import type { RuleSubject } from '@/domain/rules';
import { ruleConditions } from '@/domain/rules';
import {
  analyzeRuleCoverage,
  appendableRuleTypes,
  appendSubjectToRule,
  isShadowedTarget,
  matchAppendTargets,
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

const multi = (
  id: string,
  conds: Array<{ type: RuleType; values: string[] }>,
  over: Partial<Rule> = {},
): Rule => ({
  id,
  type: conds[0].type,
  values: conds[0].values,
  conditions: conds,
  action: 'proxy',
  enabled: true,
  ...over,
});

const domain = (value: string): RuleSubject => ({ kind: 'domain', type: 'domain', value });
const ip = (value: string): RuleSubject => ({ kind: 'ip', type: 'ipCidr', value });
const process = (value: string): RuleSubject => ({
  kind: 'process',
  type: 'processName',
  value,
});

describe('规则对象 → 条件类型', () => {
  it('域名可追加进字面量域名条件，新开条件固定为精确 domain', () => {
    expect([...appendableRuleTypes(domain('a.example.com'))].sort()).toEqual([
      'domain',
      'domainKeyword',
      'domainSuffix',
    ]);
    const target = ruleAppendTargets(
      [rule({ id: 'geo', type: 'geosite', values: ['youtube'] })],
      domain('www.youtube.com'),
    )[0];
    expect(target).toMatchObject({ condIndex: -1, type: 'domain', block: null });
  });

  it('IP 只进入 ipCidr，进程只进入 processName', () => {
    expect(appendableRuleTypes(ip('1.1.1.1'))).toEqual(['ipCidr']);
    expect(appendableRuleTypes(process('curl'))).toEqual(['processName']);

    const base = multi('mixed', [
      { type: 'domain', values: ['example.com'] },
      { type: 'ipCidr', values: ['10.0.0.0/8'] },
      { type: 'processName', values: ['wget'] },
    ]);
    expect(ruleAppendTargets([base], ip('1.1.1.1')).map((target) => target.condIndex)).toEqual([1]);
    expect(ruleAppendTargets([base], process('curl')).map((target) => target.condIndex)).toEqual([2]);
  });

  it('domainRegex 不接字面值，而是在 OR 规则末尾新开精确 domain', () => {
    const base = rule({ id: 'rx', type: 'domainRegex', values: ['^stun\\..+'] });
    const subject = domain('stun.l.google.com');
    const target = ruleAppendTargets([base], subject)[0];
    expect(target).toMatchObject({ condIndex: -1, type: 'domain', block: null });
    const next = appendSubjectToRule(base, target, subject)!;
    expect(next.conditions).toEqual([
      { type: 'domainRegex', values: ['^stun\\..+'] },
      { type: 'domain', values: ['stun.l.google.com'] },
    ]);
  });

  it('每条规则至少出现一项，不能兼容的规则通过新开条件或置灰说明', () => {
    const rules = [
      rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] }),
      rule({ id: 'geo', type: 'geosite', values: ['youtube'] }),
      rule({ id: 'proc', type: 'processName', values: ['Telegram'] }),
      rule({ id: 'port', type: 'port', values: ['443'], combineMode: 'and' }),
      rule({ id: 'sfx', type: 'domainSuffix', values: ['a.com'] }),
    ];
    const targets = ruleAppendTargets(rules, domain('www.youtube.com'));
    expect([...new Set(targets.map((target) => target.ruleId))]).toEqual([
      'ip',
      'geo',
      'proc',
      'port',
      'sfx',
    ]);
    expect(targets.find((target) => target.ruleId === 'port')?.block).toBe('andMode');
  });
});

describe('候选分类、排序与检索', () => {
  it('AND 规则没有兼容条件时禁止新开；已有兼容条件仍可追加', () => {
    const blocked = rule({
      id: 'blocked',
      type: 'ipCidr',
      values: ['10.0.0.0/8'],
      combineMode: 'and',
    });
    const compatible = multi(
      'compatible',
      [
        { type: 'domainSuffix', values: ['a.com'] },
        { type: 'ipCidr', values: ['10.0.0.0/8'] },
      ],
      { combineMode: 'and' },
    );
    expect(ruleAppendTargets([blocked], domain('b.com'))[0].block).toBe('andMode');
    expect(ruleAppendTargets([compatible], domain('b.com'))[0]).toMatchObject({
      condIndex: 0,
      block: null,
    });
  });

  it('非法对象值置灰，已包含大小写不敏感', () => {
    expect(
      ruleAppendTargets(
        [rule({ id: 'ip', type: 'ipCidr', values: ['10.0.0.0/8'] })],
        domain('2606:4700::1'),
      )[0].block,
    ).toBe('valueUnfit');
    expect(
      ruleAppendTargets(
        [rule({ id: 'proc', type: 'processName', values: [' CURL '] })],
        process('curl'),
      )[0].block,
    ).toBe('contains');
  });

  it('可追加 → 已包含 → 其余置灰，同档内保持规则优先级', () => {
    const rules = [
      rule({ id: 'and', type: 'port', values: ['443'], combineMode: 'and' }),
      rule({ id: 'add', type: 'domainSuffix', values: ['b.com'] }),
      rule({ id: 'has', type: 'domain', values: ['x.example.com'] }),
      rule({ id: 'new', type: 'geosite', values: ['cn'] }),
    ];
    const sorted = sortAppendTargets(ruleAppendTargets(rules, domain('x.example.com')));
    expect(sorted.map((target) => target.ruleId)).toEqual(['add', 'new', 'has', 'and']);
    expect(sorted.map((target) => target.block)).toEqual([null, null, 'contains', 'andMode']);
  });

  it('规则名、类型、任一条件值都可检索，置灰项也不消失', () => {
    const targets = ruleAppendTargets(
      [
        rule({ id: 'media', remarks: '流媒体', values: ['netflix.com'] }),
        rule({
          id: 'lan',
          remarks: '内网直连',
          type: 'ipCidr',
          values: ['10.0.0.0/8'],
          combineMode: 'and',
        }),
      ],
      domain('x.com'),
    );
    expect(matchAppendTargets(targets, 'netflix').map((target) => target.ruleId)).toEqual(['media']);
    expect(matchAppendTargets(targets, '内网').map((target) => target.ruleId)).toEqual(['lan']);
  });
});

describe('写入侧不变式与漂移防御', () => {
  const mirrorHolds = (value: Rule) => {
    const first = ruleConditions(value)[0];
    expect(value.type).toBe(first.type);
    expect(value.values).toEqual(first.values);
  };

  it('单条件追加同步镜像并保全规则其它字段', () => {
    const base = rule({
      id: 'r',
      values: ['a.com'],
      remarks: '解锁',
      targetServerId: 'srv-1',
      bypassFakeIP: true,
      tlsSpoof: 'www.apple.com',
      tlsSpoofMethod: 'wrong-md5',
    });
    const subject = domain('b.com');
    const next = appendSubjectToRule(base, ruleAppendTargets([base], subject)[0], subject)!;
    expect(next.values).toEqual(['a.com', 'b.com']);
    expect(next.conditions).toBeUndefined();
    expect(next).toMatchObject({
      remarks: '解锁',
      targetServerId: 'srv-1',
      bypassFakeIP: true,
      tlsSpoof: 'www.apple.com',
      tlsSpoofMethod: 'wrong-md5',
    });
    mirrorHolds(next);
  });

  it('新开条件追加在末尾，保持既有条件、镜像与 combineMode', () => {
    const base = multi(
      'r',
      [
        { type: 'geosite', values: ['cn'] },
        { type: 'port', values: ['443'] },
      ],
      { combineMode: 'or' },
    );
    const subject = ip('1.1.1.1');
    const next = appendSubjectToRule(base, ruleAppendTargets([base], subject)[0], subject)!;
    expect(next.conditions).toEqual([
      ...base.conditions!,
      { type: 'ipCidr', values: ['1.1.1.1'] },
    ]);
    expect(next.combineMode).toBe('or');
    mirrorHolds(next);
  });

  it('选择后规则改为 AND、条件类型漂移或对象类型漂移时拒绝写入', () => {
    const base = rule({ id: 'r', type: 'port', values: ['443'] });
    const subject = process('curl');
    const target = ruleAppendTargets([base], subject)[0];
    expect(appendSubjectToRule(base, target, subject)).not.toBeNull();
    expect(appendSubjectToRule({ ...base, combineMode: 'and' }, target, subject)).toBeNull();
    expect(appendSubjectToRule(base, target, domain('curl'))).toBeNull();

    const existing = rule({ id: 'e', type: 'processName', values: ['wget'] });
    const existingTarget = ruleAppendTargets([existing], subject)[0];
    expect(
      appendSubjectToRule(
        rule({ id: 'e', type: 'domain', values: ['example.com'] }),
        existingTarget,
        subject,
      ),
    ).toBeNull();
  });
});

describe('覆盖提示', () => {
  it('域名/IP/进程分别只按同轴条件判断，禁用规则不参与', () => {
    const rules = [
      rule({ id: 'off', type: 'domainSuffix', values: ['example.com'], enabled: false }),
      rule({ id: 'domain', type: 'domainSuffix', values: ['example.com'] }),
      rule({ id: 'ip', type: 'ipCidr', values: ['1.1.0.0/16'] }),
      rule({ id: 'proc', type: 'processName', values: ['curl'] }),
    ];
    expect(analyzeRuleCoverage(rules, domain('www.example.com')).firstId).toBe('domain');
    expect(analyzeRuleCoverage(rules, ip('1.1.1.1')).firstId).toBe('ip');
    expect(analyzeRuleCoverage(rules, process('CURL')).firstId).toBe('proc');
  });

  it('跨维度 AND 信息不足时不谎报覆盖；更靠前的已知命中才算遮蔽', () => {
    const and = multi(
      'and',
      [
        { type: 'domain', values: ['example.com'] },
        { type: 'processName', values: ['curl'] },
      ],
      { combineMode: 'and' },
    );
    expect(analyzeRuleCoverage([and], domain('example.com')).firstId).toBeNull();

    const rules = [
      rule({ id: 'first', type: 'domainSuffix', values: ['example.com'] }),
      rule({ id: 'later', type: 'domainSuffix', values: ['other.com'] }),
    ];
    const subject = domain('www.example.com');
    const coverage = analyzeRuleCoverage(rules, subject);
    const targets = ruleAppendTargets(rules, subject);
    expect(isShadowedTarget(coverage, targets.find((target) => target.ruleId === 'first')!)).toBe(false);
    expect(isShadowedTarget(coverage, targets.find((target) => target.ruleId === 'later')!)).toBe(true);
  });
});

describe('启发式覆盖只做提示', () => {
  const strip = (source: string) =>
    source
      .replace(/\/\*[\s\S]*?\*\//g, (match) => match.replace(/[^\n]/g, ' '))
      .replace(/(^|[^:])\/\/.*$/gm, (match, prefix: string) =>
        prefix + ' '.repeat(match.length - prefix.length),
      );
  const menuPath = fileURLToPath(new URL('../rule-subject-menu.tsx', import.meta.url));
  const menuSource = strip(readFileSync(menuPath, 'utf8'));

  it('共用菜单使用覆盖判据，但不据此禁用动作', () => {
    expect(menuSource).toContain('analyzeRuleCoverage');
    expect(menuSource).not.toMatch(/\bdisabled\b/);
  });

  it('选择器禁用态只由可测的 block 判据决定', () => {
    const source = strip(
      readFileSync(fileURLToPath(new URL('./RulePickDialog.tsx', import.meta.url)), 'utf8'),
    );
    const expressions = [...source.matchAll(/disabled=\{([^}]*)\}/g)].map((match) => match[1].trim());
    expect(expressions).not.toEqual([]);
    for (const expression of expressions) {
      expect(expression).toContain('block');
      expect(expression).not.toMatch(/shadow|coverage|covered/i);
    }
  });
});
