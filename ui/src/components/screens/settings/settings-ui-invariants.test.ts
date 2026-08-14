/**
 * Settings UI 治理门。
 *
 * 这里守的是页面级规则，而不是某个页面当下的像素：
 *  1. 下拉只能经共享 Select → Csel，禁止重新引入系统原生弹层；
 *  2. 设置项与从属内容只能用语义分组组件组织，页面不得靠内联边框补缝；
 *  3. disabled 必须传到真实触发器，不能只有变灰但仍可点击的假禁用态。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const stripComments = (source: string) =>
  source.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

const settingScreens = readdirSync(here)
  .filter((name) => /^Settings.+\.tsx$/.test(name))
  .map((name) => ({ name, source: stripComments(read(`./${name}`)) }));

describe('Settings UI 使用统一组件与语义分组', () => {
  it('所有设置页都不直接渲染原生 select', () => {
    for (const { name, source } of settingScreens) {
      expect(source, `${name} 重新引入了系统原生下拉`).not.toMatch(/<\/?select\b/);
    }
  });

  it('页面不以内联上下边框拼装设置分组', () => {
    for (const { name, source } of settingScreens) {
      expect(source, `${name} 使用了内联 borderTop/borderBottom`).not.toMatch(
        /\bborder(?:Top|Bottom)\s*:/,
      );
      expect(source, `${name} 使用了 border: 0 局部消线`).not.toMatch(/\bborder\s*:\s*0\b/);
    }
  });

  it('可见标签与无障碍名称不写死自然语言', () => {
    const technicalNames = new Set(['MTU', 'CIDR', 'MAC', 'FakeIP', 'DoH URL']);
    const violations: string[] = [];
    for (const { name, source } of settingScreens) {
      for (const match of source.matchAll(/\b(?:label|aria-label|ariaLabel)="([^"]+)"/g)) {
        if (!technicalNames.has(match[1])) violations.push(`${name}: ${match[1]}`);
      }
    }
    expect(violations, '自然语言应来自 locale；这里只允许跨语言同形的技术名').toEqual([]);
  });

  it('共享 Select 由 Csel 实现并把 disabled 传到真实控件', () => {
    const primitives = stripComments(read('./primitives.tsx'));
    const selectStart = primitives.indexOf('export function Select');
    const selectEnd = primitives.indexOf('export function TextInput', selectStart);
    const selectBody = selectStart >= 0 && selectEnd > selectStart
      ? primitives.slice(selectStart, selectEnd)
      : undefined;
    expect(selectBody, '找不到共享 Select 实现').toBeDefined();
    expect(selectBody).toContain('<Csel');
    expect(selectBody).toContain('disabled={disabled}');
    expect(selectBody).not.toMatch(/<\/?select\b/);
  });

  it('完整设置项分组由共享结构与覆盖层统一承担分隔线', () => {
    const primitives = stripComments(read('./primitives.tsx'));
    const css = stripComments(read('../../../styles/index.css'));
    expect(primitives).toMatch(/export function SetRowGroup\b/);
    expect(primitives).toMatch(/export function SetRowSection\b/);
    expect(css).toMatch(/\.set-row-group\s*\{[^}]*border-bottom/);
    expect(css).toMatch(/\.set-row-group\s*>\s*\.set-row\s*\{[^}]*border-bottom\s*:\s*0/);
    expect(css).toMatch(/\.set-row-section\s*\{[^}]*border-top/);
  });
});
