/**
 * 界面语言的**三向覆盖门** —— 前端语言集 ↔ Rust `AppleLanguages` 码表 ↔ Info.plist `CFBundleLocalizations`。
 *
 * # 这道门守的是什么
 *
 * macOS 用「用户 `AppleLanguages` ∩ **应用声明的本地化集合**」算应用的有效本地化，
 * 原生对话框（NSOpenPanel/NSAlert）、WKWebView 的 `navigator.languages` 全取自它。
 * Polaris 在这条链上有**三份必须一致的清单**，分属三种文件、三种语言：
 *
 *  1. `ui/src/domain/language.ts` 的 `SUPPORTED_LANGUAGES` —— 出货语种（i18next 资源键）；
 *  2. `src-tauri/src/app_language.rs` 的 `APPLE_LANGUAGES` —— 语种 → macOS 本地化码；
 *  3. `src-tauri/Info.plist` 的 `CFBundleLocalizations` —— 向 macOS 声明「本应用会哪些语言」。
 *
 * 三者任意两份不一致，**症状全是静默的**：
 *  - ①有②无（加语种时忘了补码表）⇒ 该语种用户把界面切过去，原生对话框仍按系统语言，无报错；
 *  - ②有③无（码表写了 macOS 不认的码）⇒ 交集为空 ⇒ AppKit 回落**英文**，无报错；
 *  - ③有①无（Info.plist 留着已下线的语种）⇒ 向系统谎称会某语言，`auto` 用户可能被解析到一门
 *    应用里其实没有翻译的语言，界面回落英文而系统认为一切正常。
 *
 * 三种都不会让任何既有测试转红 —— 类型检查过、build 过、5 语 locale parity 过。
 * `src-tauri/Info.plist` 的文件头注释已经把「加语种时这里要同步加」写成了告诫，
 * 但**告诫不是门**：本文件把它变成会转红的判据。
 *
 * # 为什么读源码而不是再抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格（改了源不改镜像 = 门在守一个假真值）。
 * 范式照抄本仓既有的 `protocol-settings-coverage.test.ts` / `user-config-fields.test.ts`：
 * 直接把 Rust 源码与 plist 当真值读进来解析。
 *
 * # 自曝纪律
 *
 * 三处解析任何一处解析不出内容一律 **throw**，不走「读不到就跳过」——
 * 那样文件一改名 / 常量一改形，门就静默消失，「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 */

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

import { SUPPORTED_LANGUAGES } from '../domain/language';

function read(rel: string): string {
  return readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
}

const RUST_SRC = read('../../../src-tauri/src/app_language.rs');
const PLIST_SRC = read('../../../src-tauri/Info.plist');

/**
 * 解析 Rust 的 `APPLE_LANGUAGES` 表 → `[i18n 键, macOS 码][]`。
 *
 * 只吃 `pub const APPLE_LANGUAGES: &[(&str, &str)] = &[ ... ];` 这一形，
 * 形变（改名 / 改类型 / 拆成函数）即抛 —— 见文件头「自曝纪律」。
 */
function parseRustMap(): [string, string][] {
  const block = /APPLE_LANGUAGES:\s*&\[\(&str,\s*&str\)\]\s*=\s*&\[([\s\S]*?)\];/.exec(RUST_SRC);
  if (!block) {
    throw new Error(
      'app_language.rs 里找不到 `APPLE_LANGUAGES: &[(&str, &str)] = &[...]` —— 码表被改名或改形了，本门已失去判据'
    );
  }
  const pairs = [...block[1].matchAll(/\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)/g)].map(
    (m) => [m[1], m[2]] as [string, string]
  );
  if (pairs.length === 0) {
    throw new Error('APPLE_LANGUAGES 解析出 0 条 —— 表项写法变了，本门已失去判据');
  }
  return pairs;
}

/** 解析 Info.plist 的 `CFBundleLocalizations` 数组。同样：解析不出即抛。 */
function parsePlistLocalizations(): string[] {
  const block = /<key>CFBundleLocalizations<\/key>\s*<array>([\s\S]*?)<\/array>/.exec(PLIST_SRC);
  if (!block) {
    throw new Error(
      'Info.plist 里找不到 CFBundleLocalizations 数组 —— 它一旦缺席，macOS 会判定本应用「只会英文」，' +
        '原生对话框 / sing-box 面板 / 跟随系统三处同时回落英文（见该文件头注）'
    );
  }
  const codes = [...block[1].matchAll(/<string>([^<]+)<\/string>/g)].map((m) => m[1].trim());
  if (codes.length === 0) {
    throw new Error('CFBundleLocalizations 解析出 0 项 —— 等同于没声明，见上');
  }
  return codes;
}

describe('界面语种三向覆盖门（前端 ↔ Rust 码表 ↔ Info.plist）', () => {
  it('Rust 码表的左列 = 前端 SUPPORTED_LANGUAGES（逐项双向相等）', () => {
    const rustKeys = parseRustMap().map(([key]) => key);
    // 双向：少一项 = 该语种用户拿不到原生对话框本地化；多一项 = 码表在给一个已下线的语种做映射。
    expect([...rustKeys].sort()).toEqual([...SUPPORTED_LANGUAGES].sort());
  });

  it('Rust 码表的右列全部在 Info.plist 的 CFBundleLocalizations 里', () => {
    const declared = new Set(parsePlistLocalizations());
    for (const [key, code] of parseRustMap()) {
      // 不在声明集合里 ⇒ macOS 侧交集为空 ⇒ 静默回落英文，应用内翻译却好好的，极难归因。
      expect(
        declared.has(code),
        `${key} 映射到的 macOS 码 "${code}" 不在 Info.plist 的 CFBundleLocalizations 里 —— ` +
          `写进 AppleLanguages 后 macOS 算不出交集，会静默回落英文`
      ).toBe(true);
    }
  });

  it('Info.plist 声明的每一项都有语种在用（不向系统谎称会某语言）', () => {
    const used = new Set(parseRustMap().map(([, code]) => code));
    for (const code of parsePlistLocalizations()) {
      // 反向锁：下线语种时只删了 locale/码表、忘了删 plist ⇒ 声明面比实际翻译面大 ⇒
      // `auto` 用户可能被 macOS 解析到一门应用里根本没有翻译的语言。
      expect(
        used.has(code),
        `Info.plist 声明了 "${code}"，但 app_language.rs 的码表里没有任何语种映射到它 —— ` +
          `要么码表漏了，要么 plist 留了已下线语种的残留`
      ).toBe(true);
    }
  });

  it('macOS 码用脚本消歧而非地区（zh 两项必须是 Hans/Hant）', () => {
    // 这是最容易踩的一格：i18next 资源键用 zh-CN/zh-TW，直接照抄进 AppleLanguages 在 macOS 侧
    // 不匹配任何声明项。单独钉一条，因为它错了以后上面三条**全是绿的**（zh-CN 若同时写进 plist，
    // 三向一致但 macOS 仍不认这个写法）。
    const map = new Map(parseRustMap());
    expect(map.get('zh-CN')).toBe('zh-Hans');
    expect(map.get('zh-TW')).toBe('zh-Hant');
  });
});
