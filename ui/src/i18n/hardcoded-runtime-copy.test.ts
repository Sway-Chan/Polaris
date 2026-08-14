/**
 * 运行时文案不得再把中文默认值塞进 `t(key, fallback)`。
 *
 * 五份 locale 已由 locale-parity.test.ts 保证键集合一致；继续保留默认值会让代码里出现第六份文案真值，
 * 翻译改动后很容易只更新 JSON、漏掉组件中的旧句子。缺键应由 parity 门直接报错，而不是在运行时静默
 * 回落到中文。
 */
import { describe, expect, it } from 'vitest';
import ts from 'typescript';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const sourceRoot = fileURLToPath(new URL('../', import.meta.url));

function runtimeFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return runtimeFiles(full);
    if (!/\.(?:ts|tsx)$/.test(entry.name)) return [];
    if (/(?:\.test|\.spec)\.(?:ts|tsx)$/.test(entry.name)) return [];
    return [full];
  });
}

describe('i18n 文案只有 locale 一个真值源', () => {
  it('运行时代码不含 t(key, fallback) 或 defaultValue', () => {
    const violations: string[] = [];
    for (const file of runtimeFiles(sourceRoot)) {
      const source = readFileSync(file, 'utf8');
      const parsed = ts.createSourceFile(
        file,
        source,
        ts.ScriptTarget.Latest,
        true,
        file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
      );
      const visit = (node: ts.Node) => {
        const hasPositionalFallback =
          ts.isCallExpression(node) &&
          ts.isIdentifier(node.expression) &&
          node.expression.text === 't' &&
          node.arguments.length >= 2 &&
          (ts.isStringLiteral(node.arguments[1]) || ts.isNoSubstitutionTemplateLiteral(node.arguments[1]));
        const hasDefaultValue =
          ts.isCallExpression(node) &&
          ts.isIdentifier(node.expression) &&
          node.expression.text === 't' &&
          node.arguments.slice(1).some(
            (argument) =>
              ts.isObjectLiteralExpression(argument) &&
              argument.properties.some(
                (property) =>
                  ts.isPropertyAssignment(property) &&
                  ((ts.isIdentifier(property.name) && property.name.text === 'defaultValue') ||
                    (ts.isStringLiteral(property.name) && property.name.text === 'defaultValue')),
              ),
          );
        if (hasPositionalFallback || hasDefaultValue) {
          const line = parsed.getLineAndCharacterOfPosition(node.getStart()).line + 1;
          violations.push(`${path.relative(sourceRoot, file)}:${line}`);
        }
        ts.forEachChild(node, visit);
      };
      visit(parsed);
    }
    expect(violations, '请把文案放入五份 locale，并只调用 t(key)').toEqual([]);
  });

  it('直接可见文本与无障碍名称只允许跨语言同形的技术名', () => {
    const technicalText = new Set([
      'Geosite', 'GeoIP', 'WARP+', 'Polaris', 'English', 'DoH', 'DNSPod DoH',
      'Mixed', 'gVisor', 'System', 'Auto', 'macOS', 'Windows', 'Linux', 'MB',
      'polaris-backup.json', '.conf · .yaml · .json · .txt', 'com.polaris.helper',
      'https://223.5.5.5/dns-query', 'https://1.12.12.12/dns-query',
      '简体中文', '繁體中文',
    ]);
    const technicalNames = new Set([
      'Cloudflare WARP', 'Tailscale', 'OpenConnect', 'OpenVPN', 'WireGuard',
      'AI', 'DNS', 'FakeIP', 'TUN', 'MTU', 'CIDR', 'MAC', 'DoH URL',
    ]);
    const technicalExamples = new Set([
      'https://example.com/sub?token=…', 'tskey-auth-xxxxCNTRL-xxxxxxxxxxxxxxxxxxxx',
      'https://example/geosite-cn.srs', 'geosite-cn', 'xxxxxxxx-xxxxxxxx-xxxxxxxx',
      'example.com', 'https://…/icon.png', 'chrome.exe, slack', '.lan',
      'localhost · 10.0.0.0/8 · *.example.cn', 'https://223.5.5.5/dns-query',
      'dns.example.com', 'https://cdn.example/ · cdn.example',
      '••••••••', '172.16.0.0/12', '100.64.0.0/10', '00:11:22:33:44:55',
    ]);
    const violations: string[] = [];
    for (const file of runtimeFiles(sourceRoot)) {
      const rel = path.relative(sourceRoot, file);
      if (rel === 'components/ErrorBoundary.tsx') continue;
      const source = readFileSync(file, 'utf8');
      const parsed = ts.createSourceFile(
        file,
        source,
        ts.ScriptTarget.Latest,
        true,
        file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
      );
      const report = (node: ts.Node, value: string) => {
        const line = parsed.getLineAndCharacterOfPosition(node.getStart()).line + 1;
        violations.push(`${rel}:${line} ${JSON.stringify(value)}`);
      };
      const visit = (node: ts.Node) => {
        if (ts.isJsxText(node)) {
          const value = node.text.trim().replace(/\s+/g, ' ');
          const visible = value.replace(/&[A-Za-z]+;/g, '').trim();
          if (visible && /[A-Za-z\u3400-\u9fff]{2}/.test(visible) && !technicalText.has(visible)) {
            report(node, visible);
          }
        }
        if (
          ts.isJsxAttribute(node) &&
          ts.isIdentifier(node.name) &&
          ['aria-label', 'ariaLabel', 'title', 'alt', 'label', 'desc', 'placeholder', 'data-tip'].includes(node.name.text) &&
          node.initializer &&
          ts.isStringLiteral(node.initializer) &&
          node.initializer.text !== '' &&
          !technicalNames.has(node.initializer.text) &&
          !technicalExamples.has(node.initializer.text)
        ) {
          report(node, node.initializer.text);
        }
        if (
          ts.isNewExpression(node) &&
          ts.isIdentifier(node.expression) &&
          node.expression.text === 'Error' &&
          node.arguments?.[0] &&
          ts.isStringLiteral(node.arguments[0]) &&
          /[A-Za-z]+\s+[A-Za-z]+/.test(node.arguments[0].text)
        ) {
          report(node, node.arguments[0].text);
        }
        ts.forEachChild(node, visit);
      };
      visit(parsed);
    }
    expect(
      violations,
      '自然语言须来自 locale；白名单仅包含协议、平台、品牌、单位和技术示例。',
    ).toEqual([]);
  });
});
