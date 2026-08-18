/**
 * U1 错误码契约的**三面对拍门**：Rust 码表 ↔ TS 联合 ↔ 两张 i18n 表 × 五语种。
 *
 * `UpdateErrCode::wire()` 的返回串是跨语言协议（`update:progress` 的 `errorCode`、弹窗
 * `errorCode`、失败信封 `code`、以及两侧 locale 键的后缀）。此前更新失败正文由后端硬编码
 * 中文直出（i18n 模块文档登记的出口 #1/#2），本批拆成「后端发码、渲染端取键」——于是任何
 * 一侧单方面加/改码，用户看到的都会是**裸码串或回落文案**（静默劣化，不红）。本门把四个
 * 真值源钉成一张表：
 *
 *  - Rust：`crates/updater/src/popup.rs` 里 `wire()` 的 match 臂（剥不出 11 条 ⇒ 红）；
 *  - TS：`contracts/types/update.ts` 的 `UpdateErrWire` 联合（与 Rust 逐字相等）;
 *  - 主表：五语种 `settings.update.err.*` 键集 = Rust 集；
 *  - 辅表：五语种 `updatePopup.err.*` 键集 = Rust 集（多一条少一条都红——多 = 死键，
 *    少 = 该语种用户看到回落文案）。
 *
 * # 这门抓不到什么
 * - **译文质量**：键在、译文错，本门全绿（fa/ru 为非母语产出，已按 W3-i18n 先例登记
 *   「需母语者复核」）。
 * - **正文里怎么拼 detail**：括注格式（ASCII 圆括号）由渲染端两处各自实现，G1 裸 CJK 门
 *   顺带钉住「不得在代码里拼全角括号」。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

const LOCALES = ['en-US', 'zh-CN', 'zh-TW', 'ru', 'fa'] as const;

function read(rel: string): string {
  return readFileSync(join(REPO_ROOT, rel), 'utf8');
}

/** Rust `wire()` 臂 → 码集（`Self::X => "code",` 形态；取不到 11 条 ⇒ 后面的断言红）。 */
function rustWireCodes(): Set<string> {
  const src = read('crates/updater/src/popup.rs');
  const at = src.indexOf('pub const fn wire(self)');
  expect(at, 'popup.rs 里找不到 wire() —— 码表被改形，本门失去判据').toBeGreaterThan(0);
  const body = src.slice(at, src.indexOf('\n    }', at));
  return new Set([...body.matchAll(/=> "([a-zA-Z]+)",/g)].map((m) => m[1]));
}

/** TS `UpdateErrWire` 联合 → 码集。 */
function tsWireCodes(): Set<string> {
  const src = read('ui/src/contracts/types/update.ts');
  const at = src.indexOf('export type UpdateErrWire');
  expect(at, 'types/update.ts 里找不到 UpdateErrWire —— 联合被改形').toBeGreaterThan(0);
  const body = src.slice(at, src.indexOf(';', at));
  return new Set([...body.matchAll(/'([a-zA-Z]+)'/g)].map((m) => m[1]));
}

function localeKeys(rel: string, section: string): Set<string> {
  const [head, mid, tail] = section.split('.');
  const obj = JSON.parse(read(rel)) as unknown as Record<
    string,
    Record<string, Record<string, Record<string, string>>>
  >;
  return new Set(Object.keys(obj[head][mid][tail]));
}

describe('U1 错误码契约：Rust 码表 ↔ TS 联合 ↔ 两张 i18n 表 × 五语种', () => {
  it('Rust wire() 与 TS UpdateErrWire 逐字相等，且量级守恒（≥11）', () => {
    const rust = rustWireCodes();
    const ts = tsWireCodes();
    expect(rust.size, 'wire() 臂解析塌了（剥不出码表）').toBeGreaterThanOrEqual(11);
    expect([...ts].sort(), 'TS 联合与 Rust 码表漂移 —— 协议两侧单方面改了').toEqual([
      ...rust,
    ].sort());
  });

  it.each(LOCALES)('主表 settings.update.err（%s）键集 = Rust 码表', (loc) => {
    const rust = rustWireCodes();
    const keys = localeKeys(`ui/src/i18n/locales/${loc}.json`, 'settings.update.err');
    expect(
      [...keys].sort(),
      `${loc} 主表键集与码表漂移（多 = 死键；少 = 该语种用户看到回落文案）`,
    ).toEqual([...rust].sort());
  });

  it.each(LOCALES)('辅表 updatePopup.errXxx（%s）键集 = Rust 码表（扁平命名空间）', (loc) => {
    const rust = rustWireCodes();
    // 辅表是**扁平**自定义命名空间（createAuxI18n 不走嵌套），码键形如 errMissingDownloadUrl。
    const obj = JSON.parse(read(`ui/src/i18n/locales/auxiliary/${loc}.json`)) as unknown as Record<
      string,
      Record<string, string>
    >;
    const keys = new Set(
      Object.keys(obj.updatePopup)
        .filter((k) => k.startsWith('err'))
        .map((k) => k[3].toLowerCase() + k.slice(4)),
    );
    expect([...keys].sort(), `${loc} 辅表键集与码表漂移`).toEqual([...rust].sort());
  });

  it('五语种主表/辅表逐键都有非空译文（空串 = 该语种用户看到空正文）', () => {
    for (const loc of LOCALES) {
      for (const rel of [`ui/src/i18n/locales/${loc}.json`, `ui/src/i18n/locales/auxiliary/${loc}.json`]) {
        if (rel.includes('auxiliary')) {
          const obj = JSON.parse(read(rel)) as unknown as Record<string, Record<string, string>>;
          for (const [k, v] of Object.entries(obj.updatePopup).filter(([k]) => k.startsWith('err'))) {
            expect(v.trim().length, `${rel} 的 ${k} 是空串`).toBeGreaterThan(0);
          }
        } else {
          const [head, mid, tail] = ['settings', 'update', 'err'];
          const obj = JSON.parse(read(rel)) as unknown as Record<
            string,
            Record<string, Record<string, Record<string, string>>>
          >;
          for (const [k, v] of Object.entries(obj[head][mid][tail])) {
            expect(v.trim().length, `${rel} 的 err.${k} 是空串`).toBeGreaterThan(0);
          }
        }
      }
    }
  });
});
