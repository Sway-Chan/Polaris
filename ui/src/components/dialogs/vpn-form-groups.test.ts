import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { allFields, ND_SPEC } from './node-spec';
import { WG_FORM_GROUP_KEYS, vpnDraftError } from './vpn-form-layout';

const readDialog = (name: string) =>
  readFileSync(fileURLToPath(new URL(`./${name}`, import.meta.url)), 'utf8');

const localeValue = (dict: unknown, key: string): unknown =>
  key.split('.').reduce<unknown>(
    (value, part) =>
      value && typeof value === 'object'
        ? (value as Record<string, unknown>)[part]
        : undefined,
    dict,
  );

describe('VPN form information architecture', () => {
  it.each(['openconnect', 'openvpn-client'] as const)('%s uses three task-oriented groups without duplicating fields', (protocol) => {
    const groups = ND_SPEC[protocol].groups;
    expect(groups?.map((group) => group.id)).toEqual(['basic', 'routing', 'advanced']);
    const keys = allFields(protocol).map((field) => field.k);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it('WireGuard splits connection, routing and low-frequency fields', () => {
    expect(WG_FORM_GROUP_KEYS.basic).toEqual([
      'address', 'port', 'privateKey', 'localAddress', 'peerPublicKey', 'preSharedKey',
    ]);
    expect(WG_FORM_GROUP_KEYS.routing).toEqual([
      'allowedIPs', 'reverseMesh', 'allowInternet', 'alwaysRouteSubnets',
    ]);
    expect(WG_FORM_GROUP_KEYS.advanced).toEqual([
      'persistentKeepalive', 'mtu', 'reserved', 'detour',
    ]);
  });

  it('required and JSON errors point to the tab that can fix them', () => {
    expect(vpnDraftError('openconnect', {},)).toEqual({ tab: 'basic', key: 'required' });
    expect(vpnDraftError('openvpn-client', { user: 'u', pwd: 'p', ovpnCa: 'CA', extraJson: '[]' }))
      .toEqual({ tab: 'advanced', key: 'json' });
    expect(vpnDraftError('openvpn-client', { user: 'u', pwd: 'p', ovpnCa: 'CA', extraJson: '{}' }))
      .toBeNull();
  });

  it('统一添加菜单下的实际录入表单共用 540px，接入方式选择器单独使用 700px', () => {
    for (const name of [
      'NodeDialog.tsx',
      'WgDialog.tsx',
      'WarpDialog.tsx',
      'TsLoginDialog.tsx',
      'TsSettingsDialog.tsx',
      'SubDialog.tsx',
      'ImportDialog.tsx',
    ]) {
      expect(readDialog(name), `${name} 没有使用统一录入表单宽度`).toContain('entry-form-dlg');
    }
    expect(readDialog('MeshJoinDialog.tsx')).toContain('access-picker-dlg');

    const css = readFileSync(
      fileURLToPath(new URL('../../styles/index.css', import.meta.url)),
      'utf8',
    );
    expect(css).toMatch(/\.dlg\.entry-form-dlg\s*\{[^}]*width:min\(540px,\s*calc\(100vw - 40px\)\)/s);
    expect(css).toMatch(/\.dlg\.access-picker-dlg\s*\{[^}]*width:min\(700px,\s*calc\(100vw - 40px\)\)/s);
  });

  it('三段协议页签在紧凑表单中使用短标签', () => {
    const expected = {
      'zh-CN': ['基础', '路由', '高级'],
      'zh-TW': ['基礎', '路由', '進階'],
      'en-US': ['Basic', 'Routing', 'Advanced'],
      ru: ['Основное', 'Маршруты', 'Дополнительно'],
      fa: ['پایه', 'مسیریابی', 'پیشرفته'],
    } as const;

    for (const [locale, labels] of Object.entries(expected)) {
      const dict = JSON.parse(
        readFileSync(
          fileURLToPath(new URL(`../../i18n/locales/${locale}.json`, import.meta.url)),
          'utf8',
        ),
      ) as unknown;
      expect(['basic', 'routing', 'advanced'].map((key) =>
        localeValue(dict, `node.formGroup.${key}`)
      )).toEqual(labels);
    }
  });

  it('节点页只保留一个全局添加菜单，不在组网列表区重复渲染入口或摘要', () => {
    const src = readFileSync(
      fileURLToPath(new URL('../screens/nodes/NodesScreen.tsx', import.meta.url)),
      'utf8',
    );
    for (const key of [
      'nodes.add',
      'nodes.manualAdd',
      'nodes.meshAddAccess',
      'nodes.manualImport',
      'nodes.addSubscription',
      'nodes.meshEmpty',
    ]) {
      expect(src, `${key} 应只引用 locale，不应内联第二份文案`).toContain(`t('${key}')`);
    }
    expect(src).toContain('openMeshJoin();');
    expect(src).not.toContain('!activeGroup?.isMesh');
    expect(src).not.toContain('mesh-list-head');
    expect(src).not.toContain('meshListSummary');
    expect(src).toContain("openDialog({ kind: 'sub', onAdded: setActiveTab })");

    const meshJoin = readFileSync(
      fileURLToPath(new URL('./MeshJoinDialog.tsx', import.meta.url)),
      'utf8',
    );
    expect(meshJoin, '组网接入选择器的文案应以 locale 为唯一真值').not.toMatch(
      /\bt\(\s*['"][A-Za-z0-9_.]+['"]\s*,/,
    );

    const sub = readFileSync(fileURLToPath(new URL('./SubDialog.tsx', import.meta.url)), 'utf8');
    expect(sub.indexOf('await loadConfig(true);')).toBeLessThan(sub.indexOf('onAdded?.(newSub.id);'));
  });

  it('节点与组网接入表单只以五语言 locale 为文案真值，不保留内联默认值', () => {
    const formNames = [
      'MeshJoinDialog.tsx',
      'NodeDialog.tsx',
      'SubDialog.tsx',
      'TsSettingsDialog.tsx',
      'WarpDialog.tsx',
      'WgDialog.tsx',
    ];
    const nodeScreen = readFileSync(
      fileURLToPath(new URL('../screens/nodes/NodesScreen.tsx', import.meta.url)),
      'utf8',
    );
    for (const [name, source] of [
      ...formNames.map((name) => [name, readDialog(name)] as const),
      ['NodesScreen.tsx', nodeScreen] as const,
    ]) {
      expect(source, `${name} 仍有 t(key, 内联默认值)`).not.toMatch(
        /\bt\(\s*['"][A-Za-z0-9_.]+['"]\s*,\s*['"]/
      );
    }

    const fieldSources = ['WgDialog.tsx', 'WarpDialog.tsx', 'TsSettingsDialog.tsx']
      .map(readDialog)
      .join('\n');
    expect(fieldSources, 'FieldSpec 不得保留中文 fallback 属性').not.toMatch(
      /\b(?:zh|hintZh|disabledHintZh)\s*:/,
    );

    const dynamicKeys = new Set(
      [...fieldSources.matchAll(/\b(?:label|hint|disabledHint):\s*'([A-Za-z0-9_.]+)'/g)]
        .map((match) => match[1]),
    );
    expect(dynamicKeys.size, '动态 FieldSpec 键扫描异常，防止测试空转').toBeGreaterThan(30);

    for (const locale of ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa']) {
      const dict = JSON.parse(
        readFileSync(
          fileURLToPath(new URL(`../../i18n/locales/${locale}.json`, import.meta.url)),
          'utf8',
        ),
      ) as unknown;
      for (const key of dynamicKeys) {
        expect(localeValue(dict, key), `${locale} 缺少动态表单键 ${key}`).not.toBeUndefined();
        expect(localeValue(dict, key), `${locale} 的动态表单键 ${key} 为空`).not.toBe('');
      }
    }
  });
});
