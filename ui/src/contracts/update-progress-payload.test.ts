/**
 * `update:progress` 载荷的**跨语言对拍门**：Rust 产出的字段集 ↔ TS `UpdateProgress` 声明的字段集。
 *
 * # 这道门守的是什么
 *
 * 本事件走 `events::broadcast` fan-out 给**所有**窗口 ⇒ 把设置页推进 downloading / downloaded /
 * error 的路径大多**不是设置页发起的**（启动自动下载腿 `startup_tasks::spawn_auto_download`、
 * 弹窗「更新·重试」腿 `update_popup_action`），那些路径上设置页拿不到任何 invoke 回包。于是这条
 * 事件是那几条腿**唯一**的事实通道：状态所依赖的数据（这份包的清单、落位路径、已收字节、校验
 * 结论）少一样，前端就少一样，而且是**静默**地少 —— 少掉的那个字段在 TS 里长得和「后端没发」
 * 一模一样，`tsc` 与 `cargo build` 都不会说话。已经付过的代价有三条：「重启并安装」按钮点了没
 * 反应（拿不到 `filePath`）、「重试」按钮点了没反应（拿不到 `updateInfo`）、卡片上的版本号与
 * 体积写的是上一次检查的另一个版本。
 *
 * 故必须有一道**两边源码都读**的门。Rust 一侧对称的那半在
 * `src-tauri/src/commands/updater.rs` 的 `progress_frame_carries_the_facts_its_state_depends_on`
 * （对 `ProgressStage` 穷尽的行为门：每个变体的帧里必须有哪几个键）——那条守「值对不对」，
 * 本条守「两侧字段集对不对得上」，两边合起来才是完整射程。
 *
 * # 判据是**集合相等**，不是「点名几个字段」
 *
 * 点名清单的门是由夹具定覆盖面：新加一个字段两边都不会红。集合相等则两个方向都说话 ——
 * Rust 多发一个键 ⇒ 前端在静默丢字段；TS 多声明一个字段 ⇒ 前端在读一个恒 `undefined` 的东西。
 *
 * # 自曝纪律
 *
 * 任何一处解析不出内容一律 **throw**，不走「读不到就跳过」—— 那样函数一改名门就静默消失，
 * 「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 */

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

const UPDATER_RS = read('../../../src-tauri/src/commands/updater.rs');
const API_CLIENT_TS = read('../ipc/api-client.ts');
const SETTINGS_LOGIC_TS = read('../components/screens/settings/settings-logic.ts');

/** 整行注释换空行（保留行序）。两侧的判据都对注释文本敏感：注释里提字段名会喂饱集合。 */
function stripLineComments(src: string): string {
  return src
    .split('\n')
    .map((l) => (l.trimStart().startsWith('//') || l.trimStart().startsWith('*') ? '' : l))
    .join('\n');
}

/** 剥块注释（`/** … *\/`），整段换空行。 */
function stripBlockComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '));
}

/**
 * 取 Rust 顶层 `fn <name>` 的函数体（到列 0 的 `\n}` 为止）。形变即抛。
 *
 * 锚点按**行首的定义形态**匹配（可带 `pub` / `const`、可带泛型参数列表），不是裸 `indexOf` ——
 * 后者既认不出 `const fn f<'a>(`，又会被文档注释里提到的同名函数抢先命中。
 */
function rustFnBody(name: string): string {
  const at = UPDATER_RS.search(
    new RegExp(String.raw`^(?:pub(?:\(crate\))? )?(?:const )?fn ${name}[<(]`, 'm'),
  );
  if (at < 0) throw new Error(`updater.rs 里找不到 \`fn ${name}\` 的定义 —— 本门已失去判据`);
  const rest = UPDATER_RS.slice(at);
  const end = rest.indexOf('\n}\n');
  if (end < 0) throw new Error(`\`fn ${name}\` 的右花括号锚点消失 —— 本门已失去判据`);
  return stripLineComments(rest.slice(0, end));
}

/** Rust `progress_payload` 真正写进载荷的键（`json!` 里的 `"k":` + `payload["k"] =`）。 */
function rustPayloadKeys(): Set<string> {
  const body = rustFnBody('progress_payload');
  const keys = new Set<string>([
    ...[...body.matchAll(/"([A-Za-z]\w*)":/g)].map((m) => m[1]),
    ...[...body.matchAll(/payload\["([A-Za-z]\w*)"\]\s*=/g)].map((m) => m[1]),
  ]);
  if (keys.size < 5) {
    throw new Error(`只从 progress_payload 解析到 ${keys.size} 个键 —— 写法变了？`);
  }
  return keys;
}

/** Rust `stage_facts` 的 match 产出的 status 字面量（= 后端真会发的那几种帧）。 */
function rustEmittedStatuses(): Set<string> {
  const body = rustFnBody('stage_facts');
  const found = [...body.matchAll(/=>\s*\("([\w-]+)"/g)].map((m) => m[1]);
  if (found.length === 0) throw new Error('`stage_facts` 的 match 一条分支都没解析到');
  return new Set(found);
}

/** TS `interface <name>` 的一级字段名。形变即抛。 */
function tsInterfaceFields(src: string, name: string): Set<string> {
  const at = src.indexOf(`export interface ${name} {`);
  if (at < 0) throw new Error(`找不到 \`export interface ${name}\` —— 本门已失去判据`);
  const rest = src.slice(at);
  const end = rest.indexOf('\n}');
  if (end < 0) throw new Error(`\`interface ${name}\` 的收尾锚点消失 —— 本门已失去判据`);
  const body = stripLineComments(stripBlockComments(rest.slice(0, end)));
  const fields = new Set(
    [...body.matchAll(/^\s{2}(\w+)\??:/gm)].map((m) => m[1]),
  );
  if (fields.size < 3) throw new Error(`只从 ${name} 解析到 ${fields.size} 个字段 —— 写法变了？`);
  return fields;
}

/** `PROGRESS_CARD_RULE` 里**产出 patch** 的那些 status（值不是 `null` 的行）。 */
function tsStatusesThatDriveTheCard(): Set<string> {
  const at = SETTINGS_LOGIC_TS.indexOf('const PROGRESS_CARD_RULE');
  if (at < 0) throw new Error('settings-logic.ts 里找不到 `PROGRESS_CARD_RULE` —— 本门已失去判据');
  const rest = SETTINGS_LOGIC_TS.slice(at);
  const end = rest.indexOf('\n};');
  if (end < 0) throw new Error('`PROGRESS_CARD_RULE` 的收尾锚点消失 —— 本门已失去判据');
  const body = stripLineComments(rest.slice(0, end));
  const rows = [...body.matchAll(/^\s+'?([\w-]+)'?:\s*(null|\{)/gm)];
  if (rows.length !== 7) {
    throw new Error(`PROGRESS_CARD_RULE 解析到 ${rows.length} 行，联合是 7 个成员 —— 写法变了？`);
  }
  return new Set(rows.filter((m) => m[2] !== 'null').map((m) => m[1]));
}

describe('update:progress 载荷 —— Rust ↔ TS 双向对拍', () => {
  it('字段集**逐字相等**：任一侧多一个 / 少一个都说话', () => {
    const rust = [...rustPayloadKeys()].sort();
    const ts = [...tsInterfaceFields(API_CLIENT_TS, 'UpdateProgress')].sort();
    // 单向包含挡不住另一半：Rust 多发 ⇒ 前端静默丢字段；TS 多声明 ⇒ 读一个恒 undefined 的字段。
    expect(ts, 'UpdateProgress 的字段集与 Rust progress_payload 写出的键集不一致').toEqual(rust);
    // 取材自检：两侧都解析到东西了（空集合相等是恒真的假绿）。
    expect(rust.length, '解析到的键太少 —— 取材器已失效').toBeGreaterThanOrEqual(5);
  });

  it('三样随行事实确实在契约里（哑键与假版本号各自的受益方）', () => {
    // 这条不是覆盖面判据（那由上一条的集合相等负责），是**动机存档**：三个字段各自对应一条
    // 已核实的缺陷，谁要删其中之一，先在这里读到它删掉的是什么。
    const rust = rustPayloadKeys();
    expect(rust.has('updateInfo'), '没有清单 ⇒ 版本号/体积说的是上一次检查的版本，且「重试」是哑键').toBe(true);
    expect(rust.has('filePath'), '没有落位路径 ⇒ 「重启并安装」首行恒早退（哑键）').toBe(true);
    expect(rust.has('receivedBytes'), '没有已收字节 ⇒ 进度只能从百分比反推，每帧都是错的').toBe(true);
  });

  it('后端真会发的 status ↔ 前端表里产出 patch 的 status，集合相等', () => {
    // Rust 多发一种帧而前端表里那格仍是 `null` ⇒ 那种帧被静默丢弃；
    // 前端表里多一格非 null 而后端从不发 ⇒ 那格是一条永远不执行的死策略。
    expect([...tsStatusesThatDriveTheCard()].sort(), 'stage_facts 与 PROGRESS_CARD_RULE 已经分叉').toEqual(
      [...rustEmittedStatuses()].sort(),
    );
  });
});
