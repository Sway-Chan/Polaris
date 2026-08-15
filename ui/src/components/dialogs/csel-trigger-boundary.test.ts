/**
 * Csel 点击边界：只有看得见的下拉框可以展开菜单，字段标题只负责说明。
 *
 * 原因不是 CSS 命中区，而是 HTML `<label for>` 的默认激活行为：它会替关联的 button
 * 再派发一次 click。文本框需要这份「点标题即聚焦」，自定义下拉已有完整触发框，继续关联只会
 * 让标题和 InfoIcon 变成一块看不见的第二触发器。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join, relative } from 'node:path';
import ts from 'typescript';

const SRC = fileURLToPath(new URL('../../', import.meta.url));

function tsxFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return tsxFiles(path);
    return entry.isFile() && entry.name.endsWith('.tsx') && !entry.name.endsWith('.test.tsx')
      ? [path]
      : [];
  });
}

function literalAttr(node: ts.JsxAttributes, name: string): string | null {
  const attr = node.properties.find(
    (item): item is ts.JsxAttribute =>
      ts.isJsxAttribute(item) && ts.isIdentifier(item.name) && item.name.text === name,
  );
  return attr?.initializer && ts.isStringLiteral(attr.initializer) ? attr.initializer.text : null;
}

function directLabelLinks(path: string): string[] {
  const source = readFileSync(path, 'utf8');
  const ast = ts.createSourceFile(path, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const controls = new Set<string>();
  const labels = new Set<string>();

  const visit = (node: ts.Node) => {
    if (ts.isJsxSelfClosingElement(node) || ts.isJsxOpeningElement(node)) {
      const tag = node.tagName.getText(ast);
      if (tag === 'Csel' || tag === 'Select') {
        const id = literalAttr(node.attributes, 'id');
        if (id) controls.add(id);
      } else if (tag === 'label') {
        const target = literalAttr(node.attributes, 'htmlFor');
        if (target) labels.add(target);
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(ast);

  return [...controls].filter((id) => labels.has(id));
}

describe('Csel 只由可见触发框展开', () => {
  it('所有手写 Csel/Select 都不再用 label[for] 扩张点击区', () => {
    const leaks = tsxFiles(SRC).flatMap((path) =>
      directLabelLinks(path).map((id) => `${relative(SRC, path)}: ${id}`),
    );
    expect(leaks).toEqual([]);
  });

  it('FieldRenderer 的动态 select 分支也使用非交互标题', () => {
    const source = readFileSync(new URL('./field-spec.tsx', import.meta.url), 'utf8');
    const start = source.indexOf("if (spec.t === 'select')");
    const end = source.indexOf("if (spec.t === 'number')", start);
    expect(start).toBeGreaterThan(-1);
    expect(end).toBeGreaterThan(start);
    const branch = source.slice(start, end);
    expect(branch).toContain('{selectLabelEl}');
    expect(branch).not.toContain('{labelEl}');
    expect(branch).not.toMatch(/<label\b/);
  });

  it('OpenConnect 字段使用面向用户的“厂商”，不暴露实现术语“方言”', () => {
    const expected: Record<string, string> = {
      'zh-CN': '厂商',
      'zh-TW': '廠商',
      'en-US': 'Vendor',
      ru: 'Производитель',
      fa: 'سازنده',
    };
    for (const [locale, label] of Object.entries(expected)) {
      const dict = JSON.parse(
        readFileSync(new URL(`../../i18n/locales/${locale}.json`, import.meta.url), 'utf8'),
      ) as { node: { field: { flavor: string } } };
      expect(dict.node.field.flavor, locale).toBe(label);
    }
  });
});
