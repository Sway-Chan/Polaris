/**
 * 导入回执接线门（`ImportDialog.handleImport`）。
 *
 * **为什么是接线门而不是纯函数单测。** 这里要防的缺陷是**遗漏**：`handleImport` 成功后直接
 * `close()`，唯一会说话的那条腿是「撞上 WARP / Tailscale 单例槽」，而它今天恒不命中
 * （clash/singbox/xray 三个解析器都跳过 wireguard）⇒ 导入在用户眼里完全静默。
 * 把三选一的判断抽成纯函数再单测，测到的是**方法体**；而缺陷长在**调用点**上 ——
 * 纯函数留着不调用，单测照样全绿。故本门直接读 `ImportDialog.tsx` 的 AST 断言接线。
 *
 * 第二条不变量是**诚实**：`editRoute('servers') === 'staged'` 时一个字节都没落盘，
 * 回执说「已导入」会让用户跳过待应用条上的「保存」。故三元的两臂与 staged 的对应关系
 * 必须锁死方向 —— 只断言「两个键都出现过」是不够的，把两臂对调它照样绿。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC_PATH = join(HERE, 'ImportDialog.tsx');
const SRC = readFileSync(SRC_PATH, 'utf8');

const SF = ts.createSourceFile(SRC_PATH, SRC, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

/** `handleImport` 的函数体（箭头函数常量声明）。找不到即门失效，直接抛而不是静默放行。 */
const HANDLE_IMPORT = (() => {
  let found: ts.Node | null = null;
  const visit = (n: ts.Node): void => {
    if (
      ts.isVariableDeclaration(n) &&
      ts.isIdentifier(n.name) &&
      n.name.text === 'handleImport' &&
      n.initializer
    ) {
      found = n.initializer;
      return;
    }
    ts.forEachChild(n, visit);
  };
  visit(SF);
  if (!found) throw new Error('ImportDialog.tsx 里找不到 handleImport —— 本门已失效，先修门');
  return found as ts.Node;
})();

function textOf(n: ts.Node): string {
  return SRC.slice(n.getStart(SF), n.getEnd());
}

function collect<T extends ts.Node>(root: ts.Node, pred: (n: ts.Node) => n is T): T[] {
  const out: T[] = [];
  const visit = (n: ts.Node): void => {
    if (pred(n)) out.push(n);
    ts.forEachChild(n, visit);
  };
  visit(root);
  return out;
}

/** 形如 `a.b(...)` 且点分名恰为 `name` 的调用。 */
function callsTo(root: ts.Node, name: string): ts.CallExpression[] {
  return collect(root, (n): n is ts.CallExpression => ts.isCallExpression(n)).filter(
    (c) => textOf(c.expression) === name
  );
}

describe('导入回执接线', () => {
  it('成功腿必须发一条 toast.success，且是 staged 三元', () => {
    const successCalls = callsTo(HANDLE_IMPORT, 'toast.success');
    expect(successCalls.length, 'handleImport 成功后必须给回执（此前直接 close，用户零反馈）').toBe(
      1
    );

    const arg = successCalls[0].arguments[0];
    expect(
      arg && ts.isConditionalExpression(arg),
      '回执必须按 staged 分叉：直落盘与暂存是两件事'
    ).toBe(true);

    const cond = arg as ts.ConditionalExpression;
    expect(textOf(cond.condition), '分叉判据必须是 staged 本身').toBe('staged');
    // 方向锁死：两臂对调 → 本断言转红。
    expect(textOf(cond.whenTrue), 'staged 腿必须用 importStagedOk（不能说「已导入」）').toContain(
      'nodes.importStagedOk'
    );
    expect(textOf(cond.whenFalse), '直落盘腿必须用 importOk').toContain('nodes.importOk');
    expect(textOf(cond.whenTrue), 'staged 腿不得混入 importOk').not.toContain("'nodes.importOk'");
  });

  it('staged 判据只算一次，两处消费同一个值', () => {
    // `editRoute(...)` 在本函数里只应出现一次并存进 `staged`：算两次的话，
    // 「走了哪条腿」与「回执怎么说」就可能分叉（中途 store 变更即两值不一致）。
    expect(callsTo(HANDLE_IMPORT, 'editRoute').length).toBe(1);
  });

  it('直落盘腿写完必须刷 store（快路径，与其余四个写节点弹窗同形）', () => {
    const bulk = callsTo(HANDLE_IMPORT, 'api.server.addBulk');
    expect(bulk.length).toBe(1);

    // `addBulk` 与 `loadConfig(true)` 必须同属一个 Block —— 只断言「文件里出现过
    // loadConfig」的话，把它挪到 staged 腿里门照样绿。
    let owner: ts.Block | null = null;
    for (let p: ts.Node | undefined = bulk[0]; p; p = p.parent) {
      if (ts.isBlock(p)) {
        owner = p;
        break;
      }
    }
    expect(owner, 'addBulk 应在 else 块内').not.toBeNull();
    const refresh = callsTo(owner as ts.Block, 'loadConfig');
    expect(refresh.length, 'addBulk 之后必须 loadConfig(true) 刷 store').toBe(1);
    expect(textOf(refresh[0].arguments[0])).toBe('true');
  });

  it('两个回执键五语齐备（G6 已零容忍，这里只钉「本门用到的这两条」）', async () => {
    const locales = ['en-US', 'zh-CN', 'zh-TW', 'ru', 'fa'];
    for (const L of locales) {
      const json = JSON.parse(
        readFileSync(join(HERE, '..', '..', 'i18n', 'locales', `${L}.json`), 'utf8')
      ) as { nodes?: Record<string, unknown> };
      for (const k of ['importOk', 'importStagedOk']) {
        expect(typeof json.nodes?.[k], `${L} 缺 nodes.${k}`).toBe('string');
        expect(String(json.nodes?.[k]), `${L} 的 nodes.${k} 必须带 {{count}}`).toContain(
          '{{count}}'
        );
      }
    }
  });
});
