/**
 * `UpdatePopupState` 载荷的**跨语言对拍门**：Rust 结构体字段集 ↔ TS `UpdatePopupState` ↔ 渲染端读点。
 *
 * # 这道门守的是什么
 *
 * 更新弹窗是独立 webview，主进程与它之间只有这一个载荷。字段在两侧各写一遍，于是有两类**只能
 * 静态查出来**的死角，两个编译器都不会说话（少掉的字段在 TS 里长得和「后端这一帧没发」一模一样）：
 *
 *  - **写了没人读**：Rust 发了字段，渲染端不读 ⇒ 事实到了窗口边上又被丢掉。
 *  - **读了没人写**：TS 声明了、渲染端也读了，Rust 侧却从来不发 ⇒ 那个读点是**恒 undefined**
 *    的死代码，用户永远看不到那条信息。
 *
 * 第二类不是假想：`bytesText` 就是活样本 —— Rust 有字段、有 serde 单测，`main.ts` 有
 * `state.bytesText ?? \`${pct}%\`` 的读点，**全仓零生产写点**，于是整整一个移植周期里用户只见过
 * 百分比、没见过一次字节数。它在 2026-08-18 这批被换成 `receivedBytes` / `totalBytes` 两个数字字段
 * 并接上写点；「字段有没有生产写点」那一半由 Rust 侧
 * `crates/updater/src/popup.rs::every_declared_field_has_a_production_write_point` 守（它看得见
 * `impl` 块），本门守的是**跨语言的字段集**与**渲染端有没有读**，两边合起来才是完整射程。
 *
 * phase 那一维（枚举 ↔ 白名单 ↔ render 分支 ↔ TS 联合）由
 * `ui/src/lib/update-popup-action-parity.test.ts` 对拍，与本门正交。
 *
 * # 判据是**集合相等**，不是点名几个字段
 *
 * 点名清单由夹具定覆盖面：新加一个字段两边都不会红。集合相等则两个方向都说话。
 *
 * # 自曝纪律
 *
 * 任何一处解析不出内容一律 **throw**，不走「读不到就跳过」—— 那样函数/结构体一改名门就静默消失，
 * 「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 */

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

const POPUP_RS = read('../../../crates/updater/src/popup.rs');
const TYPES_TS = read('./types/update.ts');
const MAIN_TS = read('../update-popup/main.ts');

/** 整行注释换空行（保留行序）。两侧判据都对注释文本敏感：注释里提字段名会喂饱集合。 */
function stripLineComments(src: string): string {
  return src
    .split('\n')
    .map((l) => {
      const t = l.trimStart();
      return t.startsWith('//') || t.startsWith('*') || t.startsWith('/*') ? '' : l;
    })
    .join('\n');
}

/** `snake_case` → `camelCase`（= Rust `#[serde(rename_all = "camelCase")]` 的产物）。 */
function camel(name: string): string {
  return name.replace(/_(\w)/g, (_, c: string) => c.toUpperCase());
}

/** Rust `pub struct UpdatePopupState { … }` 的字段名（serde camelCase 后）。形变即抛。 */
function rustFields(): string[] {
  const at = POPUP_RS.indexOf('pub struct UpdatePopupState {');
  if (at < 0) throw new Error('popup.rs 里找不到 `pub struct UpdatePopupState` —— 本门已失去判据');
  const rest = POPUP_RS.slice(at);
  const end = rest.indexOf('\n}\n');
  if (end < 0) throw new Error('`UpdatePopupState` 的列 0 右花括号锚点消失 —— 本门已失去判据');
  const body = stripLineComments(rest.slice(0, end));
  // `pub <ident>: <ty>,` 才算字段；要求有冒号，否则结构体自己的头一行会被算成一个字段。
  const fields = [...body.matchAll(/^\s{4}pub (\w+):/gm)].map((m) => camel(m[1]));
  if (fields.length < 5) {
    throw new Error(`只从 Rust 结构体解析到 ${fields.length} 个字段 —— 写法变了？`);
  }
  return fields;
}

/** TS `export interface UpdatePopupState` 的一级字段名。形变即抛。 */
function tsFields(): string[] {
  const at = TYPES_TS.indexOf('export interface UpdatePopupState {');
  if (at < 0) throw new Error('找不到 `export interface UpdatePopupState` —— 本门已失去判据');
  const rest = TYPES_TS.slice(at);
  const end = rest.indexOf('\n}');
  if (end < 0) throw new Error('`interface UpdatePopupState` 的收尾锚点消失 —— 本门已失去判据');
  const body = stripLineComments(rest.slice(0, end));
  const fields = [...body.matchAll(/^\s{2}(\w+)\??:/gm)].map((m) => m[1]);
  if (fields.length < 5) throw new Error(`只从 TS interface 解析到 ${fields.length} 个字段 —— 写法变了？`);
  return fields;
}

const RUST = rustFields();
const TS = tsFields();

/**
 * 渲染端**读得到**该字段：`main.ts` 里出现 `state.<field>`。
 *
 * # 取材面刻意收到「这一个形状」
 *
 * 初版还有第二条腿：「字段名同时出现在 `main.ts` 与 `state-facts.ts`」，用来兜住「读了但拼串在
 * 别处」。那条腿**比意图面宽** —— 它把「两份文件里都出现过这个词」当成读点，形参名、局部变量名、
 * 甚至一句提到字段的表达式都能喂饱它。本会话已经因为同一形态栽过三次（说明文案喂饱正则 /
 * 邻居臂替被守的臂作证 / 形参声明 `_file_path:` 替字段写点作证），共同点都是取材面宽于意图面。
 *
 * 意图面其实很窄：**渲染端得把这个字段从 `state` 上取下来**。取下来之后拿去哪儿拼串是另一回事
 * （`doneSubject(state.version, state.filePath)` 照样命中）。故只认 `state.<field>` 这一个形状。
 *
 * 失效方向是**误红**（有人把 `render()` 改成 `const { filePath } = state` 解构）—— 安全方向：
 * 红了会有人来看，而放过一个死字段没有任何人会知道（那正是 `bytesText` 躺了一整个周期的原因）。
 */
function isReadByRenderer(field: string): boolean {
  return new RegExp(String.raw`\bstate\.${field}\b`).test(stripLineComments(MAIN_TS));
}

describe('守卫自检：解析器不会空转', () => {
  it('两侧字段表都解析得动，且量级合理', () => {
    expect(RUST.length).toBeGreaterThanOrEqual(8);
    expect(TS.length).toBeGreaterThanOrEqual(8);
    // 判据面里必须真有本批新接的那几个字段，否则下面的集合相等是拿旧形状在自证。
    for (const f of ['receivedBytes', 'totalBytes', 'filePath']) {
      expect(RUST, `Rust 侧没有 ${f} —— 本门在守一个已经不存在的形状`).toContain(f);
    }
  });

  it('camel 转换真的在做事（`file_path` → `filePath`）', () => {
    expect(camel('file_path')).toBe('filePath');
    expect(camel('received_bytes')).toBe('receivedBytes');
    expect(camel('phase')).toBe('phase');
  });

  it('读点探针正反两向都判得动', () => {
    expect(isReadByRenderer('phase')).toBe(true);
    expect(isReadByRenderer('thisFieldDoesNotExistAnywhere')).toBe(false);
  });
});

describe('UpdatePopupState：Rust 结构体 ↔ TS interface ↔ 渲染端读点', () => {
  it('两侧字段集逐字相等（改一边不改另一边 ⇒ 转红）', () => {
    // 变异对照：Rust 结构体加一个字段而 TS 不加 → 本条转红并点名它。
    expect([...RUST].sort(), 'Rust 与 TS 的字段集不符 —— 少的那个在前端长得跟「后端没发」一样').toEqual(
      [...TS].sort(),
    );
  });

  it('每个字段都有渲染端读点（发了没人读 = 事实到了窗边又被丢掉）', () => {
    // 变异对照：把 `main.ts` 里的 `state.filePath` 删掉 → 本条转红并点名 filePath。
    const unread = RUST.filter((f) => !isReadByRenderer(f));
    expect(unread, '这些字段后端发了、渲染端一个都没读').toEqual([]);
  });
});
