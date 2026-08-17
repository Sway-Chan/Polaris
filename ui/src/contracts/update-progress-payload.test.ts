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

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

const UPDATER_RS = read('../../../src-tauri/src/commands/updater.rs');
const API_CLIENT_TS = read('../ipc/api-client.ts');
const SETTINGS_LOGIC_TS = read('../components/screens/settings/settings-logic.ts');
const SETTINGS_UPDATE_TSX = read('../components/screens/settings/SettingsUpdate.tsx');

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

/**
 * 进度帧**剥掉**的清单字段 —— 单一真值在 Rust 的 `PROGRESS_MANIFEST_OMITTED`。形变即抛。
 */
function rustOmittedManifestFields(): Set<string> {
  const m = /const PROGRESS_MANIFEST_OMITTED: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(UPDATER_RS);
  if (!m) {
    throw new Error('updater.rs 里找不到 `PROGRESS_MANIFEST_OMITTED` 的字面量 —— 本门已失去判据');
  }
  const fields = [...m[1].matchAll(/"(\w+)"/g)].map((x) => x[1]);
  if (fields.length === 0) throw new Error('`PROGRESS_MANIFEST_OMITTED` 里一个字段都没解析到');
  return new Set(fields);
}

/** TS `UpdateProgressManifest` 的 `Omit<UpdateInfo, …>` 列表。形变即抛。 */
function tsOmittedManifestFields(): Set<string> {
  const m = /export interface UpdateProgressManifest extends Omit<UpdateInfo,([^>]*)>/.exec(
    API_CLIENT_TS,
  );
  if (!m) {
    throw new Error('api-client.ts 里找不到 `UpdateProgressManifest` 的 Omit 列表 —— 本门已失去判据');
  }
  const fields = [...m[1].matchAll(/'(\w+)'/g)].map((x) => x[1]);
  if (fields.length === 0) throw new Error('`UpdateProgressManifest` 的 Omit 里一个字段都没解析到');
  return new Set(fields);
}

describe('进度帧的清单投影 —— 剥掉的那两个字段', () => {
  it('剥除表两侧一致（Rust 剥的 == TS 声明成可选的）', () => {
    // 两侧分叉的后果各不相同、都静默：Rust 多剥一个 ⇒ TS 说它是必有的 `string`，
    // 下一个人 `.length` 就在运行期炸；TS 多列一个 ⇒ 类型上可选、运行期恒在，白白让消费方加判空。
    expect([...tsOmittedManifestFields()].sort(), 'PROGRESS_MANIFEST_OMITTED 与 UpdateProgressManifest 已分叉').toEqual(
      [...rustOmittedManifestFields()].sort(),
    );
  });

  it('`available` 不可能由进度帧进入 —— 剥除的整条论证就架在这一句上', () => {
    // 剥掉 releaseNotes 之所以「准确性零损失」，唯一理由是它只在 available 那一屏渲染，
    // 而 available 进不去进度帧。哪天有人把某个 status 映射到 available，这条先红。
    const at = SETTINGS_LOGIC_TS.indexOf('const PROGRESS_CARD_RULE');
    if (at < 0) throw new Error('找不到 `PROGRESS_CARD_RULE` —— 本门已失去判据');
    const body = SETTINGS_LOGIC_TS.slice(at, at + SETTINGS_LOGIC_TS.slice(at).indexOf('\n};'));
    const targets = [...body.matchAll(/us:\s*'([\w-]+)'/g)].map((m) => m[1]);
    expect(targets.length, '一条 us 取值都没解析到 —— 取材器失效').toBeGreaterThan(0);
    expect(targets, '有 status 会把卡片推进 available —— 剥掉 releaseNotes 的前提当场不成立').not.toContain(
      'available',
    );
  });

  /**
   * **遗漏自曝**：progress 可达面一旦读了被剥掉的字段，这条必须红。
   *
   * 剥除表是枚举型判据，而枚举型判据在本仓一路在栽 —— 差别在于它的**失效方向**是「多带」
   * （帧胖了，性能退化，正确性零损失），不是白名单那种「漏带」（消费方要的字段没了 ⇒ 「重试」
   * 重新变哑键）。即便如此，「剥掉的东西没人读」这句话仍须有门守着，否则它只是一句注释。
   *
   * 扫描面 = **整个组件减去 `available` 那一段**（上一条门刚证明了进度帧进不去 available），
   * 且**由剥除表驱动**：剥除表加第三个字段时，本门的扫描面自动跟着长，不用谁记得来改。
   *
   * **变异探针**：在 `downloaded` 那格加一行 `{updateInfo?.releaseNotes}` ⇒ 转红。
   */
  it('progress 可达面没有任何一处读被剥掉的字段（扫描面由剥除表驱动）', () => {
    const omitted = rustOmittedManifestFields();
    // 剥注释：注释里提到字段名会顶红判据（本仓踩过的老坑）。
    const src = stripLineComments(stripBlockComments(SETTINGS_UPDATE_TSX));
    // 切到**下一个** `{us === `，不是切到写死的 `{us === 'downloading'`。
    // 前身那种写法今天恰好重合，但在两者之间插一屏新态并让它读被剥字段 ⇒ 那一屏被整块吞进
    // `availableBlock`、排除在扫描面外，全量全绿。判据面必须由「下一个态屏」定，不由夹具定
    // （姊妹实现见 `settings-logic.test.ts` 的 `stateBlock()`）。
    const availAt = src.indexOf("{us === 'available'");
    expect(availAt, "找不到 available 态分支 —— 扫描面切不出来").toBeGreaterThan(-1);
    const after = src.slice(availAt + 1);
    const nextUs = after.indexOf('{us === ');
    expect(nextUs, 'available 之后没有下一个态屏 —— 扫描面切不出来').toBeGreaterThan(-1);
    const availEnd = availAt + 1 + nextUs;
    const availableBlock = src.slice(availAt, availEnd);
    const reachable = src.slice(0, availAt) + src.slice(availEnd);

    // 收集「在清单对象上取字段」的全部读法（`updateInfo.x` / `updateInfo?.x` / `patch.info.x` …）。
    const readsIn = (region: string) =>
      new Set([...region.matchAll(/\b(?:updateInfo|info|manifest)\s*\??\.\s*(\w+)/g)].map((m) => m[1]));
    const reachableReads = readsIn(reachable);
    // 取材自检：扫不到东西时下面的循环 0 次断言而「恰好」全绿。
    expect(reachableReads.has('version'), '扫描器没抓到 progress 可达面的清单读法 —— 取材器失效').toBe(true);
    expect(reachableReads.has('fileSize'), '同上').toBe(true);
    for (const field of omitted) {
      expect(
        reachableReads.has(field),
        `progress 可达面读了 \`${field}\`，而进度帧根本不带它 —— 要么别读，要么把它从剥除表里拿掉`,
      ).toBe(false);
    }
    // 正向对照：被剥的字段确实**还有**消费方（只是在 available 那一屏）。
    // 全都没人读了 ⇒ 上面那条循环恒真、无信息量，届时该问的是「这个字段还留着干嘛」。
    const availableReads = readsIn(availableBlock);
    expect(
      [...omitted].some((f) => availableReads.has(f)),
      '被剥掉的字段在 available 那一屏也没人读了 —— 上面那条断言已无信息量，请复核剥除表',
    ).toBe(true);
  });
});

/**
 * `update:progress` 的**消费点普查** —— 扫描面的文件集不能由夹具定。
 *
 * 上面那道「progress 可达面不读被剥字段」的门只扫 `SettingsUpdate.tsx`。这在今天成立，因为
 * 全仓只有一个消费者；但那是**事实**不是**判据** —— 第二个消费者出现时它会静默出界：新组件
 * 读 `patch.info.releaseNotes` 拿到 `undefined`，两道门都不响。
 *
 * 故本条把「消费点恰好一处、且就是被扫的那个文件」钉住：普查方式是**递归遍历** `ui/src`
 * （排除测试自身），不是点名几个文件。第二个消费者一出现，这条先红，作者必须回到上面那道门
 * 把扫描面补齐。
 *
 * **变异探针**：在任意组件里再加一句 `updateApi.onProgress(() => {})` ⇒ 转红。
 */
describe('update:progress 的消费点', () => {
  const walk = (dir: string): string[] =>
    readdirSync(dir).flatMap((name) => {
      const full = join(dir, name);
      if (statSync(full).isDirectory()) return walk(full);
      return /\.tsx?$/.test(full) && !/\.test\.tsx?$/.test(full) ? [full] : [];
    });

  it('全仓恰好一处消费，且就是被剥除表扫描的那个文件', () => {
    const files = walk(fileURLToPath(new URL('..', import.meta.url)));
    // 取材自检：遍历不到文件时下面的断言会在空集合上「恰好」失败/通过，两个方向都无意义。
    expect(files.length, '递归遍历 ui/src 一个源文件都没拿到 —— 取材器失效').toBeGreaterThan(50);
    const consumers = files.filter((f) => readFileSync(f, 'utf8').includes('updateApi.onProgress('));
    expect(
      consumers.map((f) => f.replace(/^.*\/ui\/src\//, '')),
      '`update:progress` 的消费点不再是唯一那一处 —— 剥除表扫描面只覆盖 SettingsUpdate.tsx，' +
        '新消费者会静默出界（读到的被剥字段恒 undefined，两道门都不响）',
    ).toEqual(['components/screens/settings/SettingsUpdate.tsx']);
  });
});
