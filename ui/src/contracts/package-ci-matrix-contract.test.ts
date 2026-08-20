/**
 * package(all) → reusable CI 的矩阵入口契约。
 *
 * GitHub workflow_call 会继承外层 workflow_dispatch 的 event_name/payload。package.yml 的输入叫
 * `platform`，没有 ci.yml 自身手动入口的 `os`；因此 package(all) 调 CI 时
 * `github.event.inputs.os === ''`。空串若只经过 `!= 'all'` 判定，会被误当成具体平台并生成 `[""]`，
 * 最终是 `runs-on: ""` / labels=[] 的永久 pending job（run 32357370395 的真实失败形态）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

function workflow(name: string): string {
  return readFileSync(join(REPO_ROOT, '.github/workflows', name), 'utf8');
}

describe('package 全平台前置 CI 的矩阵输入', () => {
  it('空 os 必须回到完整矩阵，只有显式非 all 的 os 才能走单平台', () => {
    const ci = workflow('ci.yml');
    expect(ci).toMatch(
      /github\.event_name == 'workflow_dispatch'\s*&& github\.event\.inputs\.os != ''\s*&& github\.event\.inputs\.os != 'all'/s,
    );
    expect(ci).toContain(`fromJSON(format('["{0}"]', github.event.inputs.os))`);
    expect(ci).toContain(`fromJSON('["ubuntu-22.04","windows-2022","macos-14"]')`);
  });

  it('package 的 dispatch 输入确实只有 platform，并复用 ci.yml 作全平台门', () => {
    const pkg = workflow('package.yml');
    const dispatch = pkg.slice(pkg.indexOf('workflow_dispatch:'), pkg.indexOf('# 最小权限'));
    expect(dispatch).toContain('platform:');
    expect(dispatch).not.toMatch(/^\s+os:/m);
    expect(pkg).toContain('uses: ./.github/workflows/ci.yml');
    expect(pkg).toContain("if: needs.setup.outputs.full == 'true'");
  });

  it('独立门与 Package 复用门必须按调用方隔离并发组，不能互相取消制造假红', () => {
    expect(workflow('ci.yml')).toContain(
      'group: ci-${{ github.workflow }}-${{ github.ref }}',
    );
    expect(workflow('ui.yml')).toContain(
      'group: ui-${{ github.workflow }}-${{ github.ref }}',
    );
  });
});
