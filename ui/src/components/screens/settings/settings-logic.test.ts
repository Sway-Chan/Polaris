/**
 * `settings-logic` 单测 —— 锁死「UI 显示态 ↔ 后端消费口径」的对齐点。
 *
 * 这些函数是设置屏组件的**生产接线点**（组件直接 import 消费，非并行复刻），故断言即真实行为。
 * 重点覆盖「缺省为开」（`!== false`）语义：写成 `!!` 会让存量配置（无该键）显示成「关」而后端按
 * 「开」跑 —— UI 与后台分叉是本批要根治的最恶劣缺陷。
 */
import { describe, it, expect } from 'vitest';
import ts from 'typescript';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { USER_CONFIG_FIELDS } from '@/contracts/user-config-fields';
import {
  defaultOn,
  bypassLanState,
  autoCheckUpdateChecked,
  ruleResourceAutoUpdateChecked,
  closeBehaviorOf,
  minimizeToTrayFor,
  backgroundIntervalSelectValue,
  isManualInterval,
  ruleResourceAutoStatus,
  subscriptionAutoUpdateStatus,
  coreBannerState,
  createOnceGate,
  isPortableZipUpdate,
  showsHardwareAccelRow,
  languageDescKey,
  windowEffectsDescKey,
  controlApiPort,
  normalizePortInput,
  MANUAL_INTERVAL_HOURS,
  DEFAULT_MIXED_PORT,
  DEFAULT_CONTROL_PORT,
  MIN_LISTEN_PORT,
  STAGED_SETTING_SECTION_LABELS,
  MAX_LISTEN_PORT,
  releaseShipsDigest,
  appDownloadIntegrity,
  progressResetsIntegrity,
} from './settings-logic';
import type { UpdateProgress } from '@/ipc/api-client';

describe('defaultOn —— 缺省为开的三态布尔', () => {
  it('仅显式 false 判关', () => {
    expect(defaultOn(false)).toBe(false);
  });

  it('true 判开', () => {
    expect(defaultOn(true)).toBe(true);
  });

  it('undefined（字段缺失 / 存量配置）判开——与后端 `!= Some(false)` 同口径', () => {
    expect(defaultOn(undefined)).toBe(true);
  });

  it('null（JSON null）判开', () => {
    expect(defaultOn(null)).toBe(true);
  });
});

describe('#12 bypassLanState —— 绕过局域网总开关三态', () => {
  it('缺省（未设该键）→ 开关开 + 清单渲染（后端 effective_bypass_lan 此时返回默认清单）', () => {
    expect(bypassLanState({})).toEqual({ checked: true, showList: true });
  });

  it('显式 true → 开关开 + 清单渲染', () => {
    expect(bypassLanState({ bypassLAN: true })).toEqual({ checked: true, showList: true });
  });

  it('显式 false → 开关关 + 清单隐藏（后端返空清单，继续展示可编辑清单是误导）', () => {
    expect(bypassLanState({ bypassLAN: false })).toEqual({ checked: false, showList: false });
  });
});

describe('#9 autoCheckUpdateChecked —— 正向、缺省为 true', () => {
  it('缺字段 → 开（此前 UI 写 config.autoCheckUpdate 会显示成关，与后端 !== false 分叉）', () => {
    expect(autoCheckUpdateChecked({})).toBe(true);
  });

  it('显式 false → 关', () => {
    expect(autoCheckUpdateChecked({ autoCheckUpdate: false })).toBe(false);
  });

  it('显式 true → 开', () => {
    expect(autoCheckUpdateChecked({ autoCheckUpdate: true })).toBe(true);
  });
});

describe('#15 ruleResourceAutoUpdateChecked —— 正向、缺省为 true', () => {
  it('缺字段 → 开（此前 UI 写 !!config.x 会显示「关」而后台调度器在跑，最恶劣不一致）', () => {
    expect(ruleResourceAutoUpdateChecked({})).toBe(true);
  });

  it('显式 false → 关（调度器同样按 === false 才停）', () => {
    expect(ruleResourceAutoUpdateChecked({ ruleResourceAutoUpdate: false })).toBe(false);
  });

  it('显式 true → 开', () => {
    expect(ruleResourceAutoUpdateChecked({ ruleResourceAutoUpdate: true })).toBe(true);
  });
});

describe('#10 closeBehavior ↔ minimizeToTray 双向派生', () => {
  it('minimizeToTray:true → to-tray', () => {
    expect(closeBehaviorOf({ minimizeToTray: true })).toBe('to-tray');
  });

  it('minimizeToTray:false → quit', () => {
    expect(closeBehaviorOf({ minimizeToTray: false })).toBe('quit');
  });

  // 缺省口径锁：store.rs:208 seed `minimizeToTray: true`，main.rs::resolve_close_action 读不到
  // 配置时亦兜底 true。UI 缺省若渲染成 quit，就会「显示退出应用、实际收进托盘」。
  it('缺字段 → to-tray（与 store seed + 后端兜底同口径）', () => {
    expect(closeBehaviorOf({})).toBe('to-tray');
  });

  it('仅显式 false 才判 quit（正向语义，不被 undefined 坍塌）', () => {
    expect(closeBehaviorOf({ minimizeToTray: undefined })).toBe('to-tray');
    expect(closeBehaviorOf({ minimizeToTray: false })).toBe('quit');
  });

  it('反向：to-tray → true / quit → false', () => {
    expect(minimizeToTrayFor('to-tray')).toBe(true);
    expect(minimizeToTrayFor('quit')).toBe(false);
  });

  it('双向无损：两个方向往返回到原值', () => {
    for (const v of [true, false]) {
      expect(minimizeToTrayFor(closeBehaviorOf({ minimizeToTray: v }))).toBe(v);
    }
    for (const b of ['to-tray', 'quit'] as const) {
      expect(closeBehaviorOf({ minimizeToTray: minimizeToTrayFor(b) })).toBe(b);
    }
  });
});

describe('#18 后台检查间隔', () => {
  it('缺省 → 12（下拉显示每 12 小时）', () => {
    expect(backgroundIntervalSelectValue({})).toBe('12');
  });

  it('0 → 字符串 "0"，对应「仅手动」选项', () => {
    expect(backgroundIntervalSelectValue({ subscriptionUpdateIntervalHours: 0 })).toBe('0');
  });

  it('0 = 仅手动（后端 select_due 把 0 处理成「周期不跑」）', () => {
    expect(isManualInterval(MANUAL_INTERVAL_HOURS)).toBe(true);
    expect(isManualInterval(0)).toBe(true);
  });

  it('非 0 周期不是「仅手动」', () => {
    expect(isManualInterval(6)).toBe(false);
    expect(isManualInterval(168)).toBe(false);
  });

  it('缺省不是「仅手动」——缺省走 12h 周期，不能误判成不跑', () => {
    expect(isManualInterval(undefined)).toBe(false);
    expect(isManualInterval(null)).toBe(false);
  });
});

describe('ruleResourceAutoStatus —— 开关开 ≠ 真会刷新', () => {
  it('开关显式关 → off（无论间隔）', () => {
    expect(ruleResourceAutoStatus({ ruleResourceAutoUpdate: false })).toBe('off');
    expect(
      ruleResourceAutoStatus({ ruleResourceAutoUpdate: false, subscriptionUpdateIntervalHours: 12 })
    ).toBe('off');
  });

  it('开关开 + 正常周期 → active（可以给绿点）', () => {
    expect(
      ruleResourceAutoStatus({ ruleResourceAutoUpdate: true, subscriptionUpdateIntervalHours: 24 })
    ).toBe('active');
  });

  // 这条是本函数存在的理由：开关开着但间隔=0，后端周期腿整轮不跑，绝不能显示绿点。
  it('开关开 + 仅手动(0) → manual，绝不判 active（防假绿）', () => {
    expect(
      ruleResourceAutoStatus({ ruleResourceAutoUpdate: true, subscriptionUpdateIntervalHours: 0 })
    ).toBe('manual');
  });

  it('开关缺省（视为开）+ 仅手动(0) → manual', () => {
    expect(ruleResourceAutoStatus({ subscriptionUpdateIntervalHours: 0 })).toBe('manual');
  });

  it('开关缺省 + 间隔缺省 → active（双缺省走 12h 周期，确实会刷新）', () => {
    expect(ruleResourceAutoStatus({})).toBe('active');
  });
});

describe('#16 coreBannerState —— 横幅状态机', () => {
  const NOTICE = { previousVersion: '1.10.0', currentVersion: '1.11.3' };

  it('无 pendingChangeNotice → 不可见、不 ack（当前后端真实状态：换核链路是桩，无生产者）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: false, pendingChangeNotice: null },
      dismissed: false,
    });
    expect(s.visible).toBe(false);
    expect(s.shouldAck).toBe(false);
    expect(s.notice).toBeNull();
  });

  it('versionInfo 为 null（拉取失败）→ 不可见、不 ack', () => {
    expect(coreBannerState({ versionInfo: null, dismissed: false })).toMatchObject({
      visible: false,
      shouldAck: false,
    });
  });

  it('有 pendingChangeNotice → 可见 + shouldAck（show→ack，弹一次非每启）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: false, pendingChangeNotice: NOTICE },
      dismissed: false,
    });
    expect(s.visible).toBe(true);
    expect(s.shouldAck).toBe(true);
    expect(s.notice).toEqual({ ...NOTICE, hasBackup: false });
  });

  it('hasBackup=false（后端硬编码值）→ 不显示回滚按钮 + 走 noBackupDesc 文案', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: false, pendingChangeNotice: NOTICE },
      dismissed: false,
    });
    expect(s.showRollback).toBe(false);
    expect(s.descKey).toBe('noBackupDesc');
  });

  it('hasBackup=true → 显示回滚按钮 + 走 changedDesc 文案（后端现读真实 .bak 状态）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: true, pendingChangeNotice: NOTICE },
      dismissed: false,
    });
    expect(s.showRollback).toBe(true);
    expect(s.descKey).toBe('changedDesc');
  });

  it('手动换核可用——core_replace_manual 已接线（零提权，落位于用户可写核目录）', () => {
    expect(
      coreBannerState({
        versionInfo: { hasBackup: true, pendingChangeNotice: NOTICE },
        dismissed: false,
      }).manualReplaceDisabled,
    ).toBe(false);
  });

  it('dismissed → 不可见（且不再显示回滚），但 shouldAck 不受影响（ack 的是后端持久态）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: true, pendingChangeNotice: NOTICE },
      dismissed: true,
    });
    expect(s.visible).toBe(false);
    expect(s.showRollback).toBe(false);
    expect(s.notice).toBeNull();
    expect(s.shouldAck).toBe(true);
  });

  it('事件到达 → 重新可见（组件收事件时复位 dismissed，此处以 dismissed:false 表达）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: false, pendingChangeNotice: null },
      eventPayload: { ...NOTICE, hasBackup: false },
      dismissed: false,
    });
    expect(s.visible).toBe(true);
    expect(s.shouldAck).toBe(true);
    expect(s.notice).toEqual({ ...NOTICE, hasBackup: false });
  });

  it('事件载荷优先于挂载快照（事件是刚发生的即时推送）', () => {
    const s = coreBannerState({
      versionInfo: { hasBackup: false, pendingChangeNotice: NOTICE },
      eventPayload: { previousVersion: '2.0.0', currentVersion: '2.1.0', hasBackup: true },
      dismissed: false,
    });
    expect(s.notice).toEqual({ previousVersion: '2.0.0', currentVersion: '2.1.0', hasBackup: true });
    expect(s.showRollback).toBe(true);
  });
});

describe('#17 createOnceGate —— 每会话一次去重', () => {
  it('首次调用放行', () => {
    expect(createOnceGate()()).toBe(true);
  });

  it('第 2 次及以后调用返回 false（这条正是重构中最易丢掉的行为）', () => {
    const gate = createOnceGate();
    expect(gate()).toBe(true);
    expect(gate()).toBe(false);
    expect(gate()).toBe(false);
  });

  it('两个闸门相互独立（工厂而非模块级 let，用例间不污染）', () => {
    const a = createOnceGate();
    const b = createOnceGate();
    expect(a()).toBe(true);
    expect(a()).toBe(false);
    expect(b()).toBe(true);
  });
});

/* ────────────────────────────────────────────────────────────────────────────
 * 消费面守卫
 *
 * 确认已全部改走原地二次点击（`lib/confirm-twice.ts`），编排层 `runConfirmed` 与它唯一的实现腿
 * `dialogConfirm` 已一并删除。但**逻辑单测证明不了组件没在裸用 `window.confirm`** —— 本仓 vitest
 * 是 node 环境（无 jsdom/testing-library），组件渲染不了，若哪天有人在某个屏里写回
 * `if (window.confirm(...))`，别处的用例会全绿而缺陷复活（那条腿在 Tauri 下返 Promise ⇒ 恒 truthy
 * ⇒ 闸门恒开，见 `main.rs::production_code_never_calls_global_confirm`）。故留这条扫源码的守卫，
 * 把「settings/ 下**零** window.confirm」钉死 —— 它是本文件里唯一与 runConfirmed 无关、也不随其消亡的断言。
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 去注释后再扫 —— 守卫针对的是**代码**，注释里讲解这个缺陷（本文件到处都在讲）不该算违规。
 *
 * **为什么是字符扫描而不是两条正则**：`/\/\*[\s\S]*?\*\//g` 不认字符串边界，会把**字符串字面量里的**
 * `/*` 当成注释起点，非贪婪吃到下一个 `*​/` —— 中间夹着的真代码被一并删掉 ⇒ 违规从此看不见（假阴性，
 * 守卫恒绿）。本扫描器带一个「是否在字符串里」的状态位，只摘代码位置上的注释。
 *
 * **失败方向刻意选「响」而非「哑」**：JSX 文本里的英文撇号（`don't`）会被当成字符串起点、吞到下一个
 * 引号为止，极端情况可能让一段本无违规的代码被当作字符串保留 → 守卫**误红**。误红有人查，
 * 漏报（旧正则那种）没人知道 —— 故宁可错杀不可放过。
 */
function stripComments(src: string): string {
  let out = '';
  let quote: string | null = null; // 当前所处字符串的引号（' " `）；null = 在代码里
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const next = src[i + 1];
    if (quote !== null) {
      if (c === '\\') {
        out += c + (next ?? ''); // 转义对整体保留，避免 \" 被误判为字符串结束
        i += 2;
        continue;
      }
      if (c === quote) quote = null;
      out += c;
      i++;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c;
      out += c;
      i++;
      continue;
    }
    if (c === '/' && next === '*') {
      const end = src.indexOf('*/', i + 2);
      i = end === -1 ? src.length : end + 2;
      out += ' '; // 留一个空白，避免把注释两侧的 token 粘成一个
      continue;
    }
    if (c === '/' && next === '/') {
      const end = src.indexOf('\n', i + 2);
      i = end === -1 ? src.length : end;
      out += ' ';
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

/**
 * 曾经的豁免项：`settings-logic.ts` 是 `nativeConfirm` 的合法归宿。
 *
 * **2026-07-29 取消豁免**：二次确认改走原地二次点击（`lib/confirm-twice.ts` 的 `useConfirmTwice`），
 * 生产代码已无 `window.confirm` 调用 ⇒ 再留一个「谁可以用」的白名单，等于给它留了条回来的路，
 * 而且那条路上的文件恰好**不在扫描面内**（豁免 = 不扫）。现在**零豁免、全扫**。
 */

describe('消费面守卫 —— 确认框不得在组件里裸用', () => {
  /**
   * 递归收集设置页源码（相对 `dir` 的路径），`.tsx` **与 `.ts` 同收**。
   *
   * 早先只扫 `readdirSync(dir).filter(endsWith('.tsx'))` 单层 —— 组件被挪进子目录、或改写成 `.ts`
   * 就整片扫不到，`offenders` 恒空、守卫恒绿（检测器有牙 ≠ 扫描面有牙）。
   *
   * 排除两类：① `settings-logic.ts` —— 唯一获授权的 `window.confirm` 归宿；② `*.test.ts(x)` /
   * `*.spec.ts(x)` —— 测试里的违规样本是**字符串字面量**（stripComments 摘不掉），扫它等于自己判自己
   * 违规。**两种后缀都排**：Rust 侧 `main.rs:1535` 的同类扫描 `.test.` / `.spec.` 双排，此处只排前者
   * ⇒ 谁第一个建 `foo.spec.ts` 谁踩（当前仓里恰好没有 `.spec.*`，所以是颗哑雷而非现行故障）。
   */
  function collect(dir: string, deps: typeof import('node:fs'), path: typeof import('node:path'), base = dir): string[] {
    return deps.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) return collect(full, deps, path, base);
      if (!/\.tsx?$/.test(e.name) || /\.(test|spec)\.tsx?$/.test(e.name)) return [];
      return [path.relative(base, full)];
    });
  }

  it('settings/**/*.ts(x) 内不出现 window.confirm / window.alert（一律走 useConfirmTwice）', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const { fileURLToPath } = await import('node:url');

    const dir = path.dirname(fileURLToPath(import.meta.url));
    const scanned = collect(dir, fs, path);

    // ── 扫描面自检（对齐 Rust 侧 main.rs 的 `assert!(!files.is_empty(), ...)`）──
    // 没有这几条，`dir` 漂走 / 后缀过滤失配都会让 offenders 恒为 []、`toEqual([])` 恒绿。
    //
    // **必扫锚点优先于数量下限**：`toBeGreaterThan(0)` 只挡「全塌」，挡不住「缩水」—— 递归分支被改坏
    // （只剩顶层）或后缀过滤失配时，scanned 从 15 掉到 1 依然 `> 0`，守卫悄悄只守着一个文件还是绿的。
    // 锚点取两个**卸载屏**：本页破坏性最强的两条腿（卸载 Polaris / 卸载提权助手）都在这里，
    // 一旦有人写回 `window.confirm`，最可能就发生在这两处；它俩不在扫描面内 = 守卫已经失去意义。
    //
    // `useConfig.ts` 是第三个锚点，且**必须是 `.ts`**：前两个锚点都是 `.tsx`，只钉它俩的话，
    // 「后缀过滤从 `/\.tsx?$/` 退化成 `/\.tsx$/`」这一变异会让扫描面悄悄丢掉全部 `.ts`（实测：
    // 15 → 14，数量下限与两个 .tsx 锚点全都照过 ⇒ 逃逸）。钉住它 = 钉住 `.ts` 那条分支。
    expect(scanned).toEqual(
      expect.arrayContaining(['SettingsAbout.tsx', 'SettingsHelper.tsx', 'useConfig.ts']),
    );
    // 数量下限兜底。留些许余量给正常增删，但把「扫描面整体塌掉」挡在门外。
    // 豁免取消后 settings-logic.ts 也在扫描面内，故下限比从前高一个。
    expect(scanned.length).toBeGreaterThanOrEqual(13);
    // 豁免取消后必须真的扫到它 —— 否则「零豁免」只是句注释。
    expect(scanned).toContain('settings-logic.ts');

    const offenders = scanned.filter((rel) =>
      /window\.(confirm|alert)\s*\(/.test(stripComments(fs.readFileSync(path.join(dir, rel), 'utf8'))),
    );

    expect(offenders).toEqual([]);
  });

  it('守卫本身有牙：给一段裸用 window.confirm 的代码必须判为违规', () => {
    // 反向自检 —— 防止 stripComments 写过头（比如把整份源码吃空）导致守卫恒绿。
    const bad = stripComments('/* 注释里的 window.confirm() 不算 */\nif (window.confirm("x")) drop();');
    expect(/window\.(confirm|alert)\s*\(/.test(bad)).toBe(true);
    const good = stripComments('// window.confirm("x")\nconfirmTwice(KEY, drop);');
    expect(/window\.(confirm|alert)\s*\(/.test(good)).toBe(false);

    // **字符串里的 `/*` 不是注释起点**：旧的两条正则版会从这里一路吃到下一个 `*​/`，把夹在中间的
    // `window.confirm(` 一并删掉 ⇒ 违规看不见（假阴性）。退回旧实现 → 本条转红。
    const strTrap = stripComments('const a = "/*"; if (window.confirm("x")) drop(); const b = "*/";');
    expect(/window\.(confirm|alert)\s*\(/.test(strTrap)).toBe(true);
  });
});

/* ────────────────────────────────────────────────────────────────────────────
 * 便携版更新：「已下载，需手动替换」不得被渲染成「更新失败」
 * ──────────────────────────────────────────────────────────────────────────── */

describe('isPortableZipUpdate —— 便携 zip ⇔ 真形态错配的分流判据', () => {
  it('产出侧口径的便携包判真（前缀 polaris-portable- + .zip）', () => {
    expect(isPortableZipUpdate('C:\\Users\\me\\AppData\\Local\\polaris\\updates\\polaris-portable-1.2.3.zip')).toBe(true);
    // 纯 POSIX 分隔符也要切（开发机/测试注入的路径）。
    expect(isPortableZipUpdate('/home/me/.cache/polaris/updates/polaris-portable-1.2.3.zip')).toBe(true);
    // 裸文件名（无目录段）——`split().pop()` 分支。
    expect(isPortableZipUpdate('polaris-portable-1.2.3.zip')).toBe(true);
  });

  it('其余四种安装件一律判假（它们走 classify_installer，根本到不了本分流）', () => {
    // 这四个后缀就是 `runtime/update_install.rs::classify_installer` 认得的全集。
    expect(isPortableZipUpdate('/c/updates/polaris-1.2.3-win-setup.exe')).toBe(false);
    expect(isPortableZipUpdate('/c/updates/polaris-1.2.3-mac-arm64.dmg')).toBe(false);
    expect(isPortableZipUpdate('/c/updates/polaris-1.2.3.AppImage')).toBe(false);
    expect(isPortableZipUpdate('/c/updates/polaris_1.2.3_amd64.deb')).toBe(false);
  });

  it('别的 zip 判假 —— 判据是前缀+后缀，不是「凡 zip 皆便携」', () => {
    // 只看 `.zip` 会把这些也说成便携版，然后对用户描述一个不成立的场景。
    expect(isPortableZipUpdate('/c/updates/polaris-portable.zip')).toBe(false); // 缺尾部连字符 → 不是产出侧命名
    expect(isPortableZipUpdate('/c/updates/sing-box-1.9.0-windows-amd64.zip')).toBe(false);
    expect(isPortableZipUpdate('/c/updates/geosite.zip')).toBe(false);
    // 前缀对但后缀不对（将来若出别的便携产物形态，也不该套用「解压覆盖」这套说明）。
    expect(isPortableZipUpdate('/c/updates/polaris-portable-1.2.3.7z')).toBe(false);
    // 前缀必须在**文件名**上而不是路径中段。
    expect(isPortableZipUpdate('/c/polaris-portable-cache/geosite.zip')).toBe(false);
  });

  it('空值/空串判假（尚未下载时不得误判成便携交接）', () => {
    expect(isPortableZipUpdate(null)).toBe(false);
    expect(isPortableZipUpdate(undefined)).toBe(false);
    expect(isPortableZipUpdate('')).toBe(false);
  });
});

describe('便携交接文案：消费面 + 内容守卫', () => {
  async function paths() {
    const path = await import('node:path');
    const { fileURLToPath } = await import('node:url');
    const settingsDir = path.dirname(fileURLToPath(import.meta.url));
    // settings → screens → components → src → ui → <repo root>
    const repoRoot = path.resolve(settingsDir, '../../../../..');
    return {
      path,
      updateTsx: path.join(settingsDir, 'SettingsUpdate.tsx'),
      zhCN: path.join(repoRoot, 'ui/src/i18n/locales/zh-CN.json'),
    };
  }

  it('取材自检：两处源文件都真读到了非空内容', async () => {
    // 没有这条，路径漂走会让下面所有断言在空串上「恰好」通过 = 假绿。
    const fs = await import('node:fs');
    const p = await paths();
    for (const f of [p.updateTsx, p.zhCN]) {
      expect(fs.existsSync(f), `取材文件不存在：${f}`).toBe(true);
      expect(fs.readFileSync(f, 'utf8').length).toBeGreaterThan(500);
    }
  });

  it('组件直接消费 isPortableZipUpdate，且便携分支落在 manual 态而非 error 态', async () => {
    // 纯函数测对了也证明不了组件在用它（node 环境渲染不了组件）——这条钉的是接线本身。
    const fs = await import('node:fs');
    const p = await paths();
    const tsx = fs.readFileSync(p.updateTsx, 'utf8');
    expect(tsx.includes('isPortableZipUpdate'), '组件必须消费该判据，不得并行复刻').toBe(true);
    expect(tsx.includes("setUs('manual')"), '便携交接必须落 manual 态').toBe(true);
    expect(
      tsx.includes('settings.update.portableManualReplace'),
      '便携交接必须走 portableManualReplace 文案',
    ).toBe(true);
    // 回退方向：真形态错配仍走原文案 + error 态，两条腿都在。
    expect(tsx.includes('settings.update.formMismatch')).toBe(true);
  });

  it('文案必须说清三件事：下载到哪 / 手动解压覆盖 / 别双击安装', async () => {
    // 后端返 `ok:false`（准确：没执行安装），UI 若只说「失败」，用户读到的是坏消息而不是**下一步动作**。
    // 缺任何一条，用户都会卡住：不知道包在哪 / 不知道要自己解压 / 去找不存在的安装程序。
    const fs = await import('node:fs');
    const p = await paths();
    const zh = JSON.parse(fs.readFileSync(p.zhCN, 'utf8')) as {
      settings: { update: { portableManualReplace?: string } };
    };
    const msg = zh.settings.update.portableManualReplace ?? '';
    expect(msg, 'zh-CN 缺 portableManualReplace').toBeTruthy();
    expect(msg, '必须带 {{path}} 插值，否则用户不知道包下到哪了').toContain('{{path}}');
    expect(msg, '必须说明要手动解压覆盖').toMatch(/解压/);
    expect(msg, '必须说明要覆盖到当前程序目录').toMatch(/覆盖/);
    expect(msg, '必须明说别双击安装（便携版没有安装程序）').toMatch(/请勿双击安装|不要双击安装/);
    // 反向：不得再把它描述成一次失败（这正是本次要修的误读）。
    expect(msg, '便携交接不是失败，文案里不得出现「失败」').not.toMatch(/失败/);
  });
});

describe('controlApiPort —— 逐条对齐 crates/config-engine/.../proxy_ports.rs:36-42', () => {
  it('controlPort > 0 用之', () => {
    expect(controlApiPort({ controlPort: 9091 })).toBe(9091);
  });

  it('未设 → 默认 9090', () => {
    expect(controlApiPort({})).toBe(DEFAULT_CONTROL_PORT);
    expect(DEFAULT_CONTROL_PORT).toBe(9090);
  });

  it('0 → 默认（后端是 `Some(p) if p > 0`；UI 若写 `?? 9090` 会在此显示 0）', () => {
    expect(controlApiPort({ controlPort: 0 })).toBe(DEFAULT_CONTROL_PORT);
  });

  it('null（JSON null）→ 默认', () => {
    expect(controlApiPort({ controlPort: null })).toBe(DEFAULT_CONTROL_PORT);
  });
});

describe('normalizePortInput —— 端口输入的落盘判定（null = 标红不落盘）', () => {
  it('空串 / 纯空白 → 回默认（「清空即回默认」，同 DNS 两栏）', () => {
    expect(normalizePortInput('', DEFAULT_MIXED_PORT)).toBe(7890);
    expect(normalizePortInput('   ', DEFAULT_CONTROL_PORT)).toBe(9090);
  });

  it('区间内的纯数字放行（含两端边界）', () => {
    expect(normalizePortInput('7890', DEFAULT_MIXED_PORT)).toBe(7890);
    expect(normalizePortInput(String(MIN_LISTEN_PORT), DEFAULT_MIXED_PORT)).toBe(1024);
    expect(normalizePortInput(String(MAX_LISTEN_PORT), DEFAULT_MIXED_PORT)).toBe(65535);
  });

  it('特权端口（<1024）判非法 —— 上游 network-settings.tsx:219 同口径', () => {
    expect(normalizePortInput('80', DEFAULT_MIXED_PORT)).toBeNull();
    expect(normalizePortInput('1023', DEFAULT_MIXED_PORT)).toBeNull();
  });

  it('逐键输入的中间态全程判非法 —— 这正是「每键落盘」会写进 config 的那些值', () => {
    // 用户想输 7891：中间态 7 / 78 / 789 逐个都不得落盘（789 甚至是特权端口）。
    for (const mid of ['7', '78', '789']) {
      expect(normalizePortInput(mid, DEFAULT_MIXED_PORT), `中间态 ${mid} 不该落盘`).toBeNull();
    }
    expect(normalizePortInput('7891', DEFAULT_MIXED_PORT)).toBe(7891);
  });

  it('超上界判非法（后端 validate_port 亦然）', () => {
    expect(normalizePortInput('65536', DEFAULT_MIXED_PORT)).toBeNull();
    expect(normalizePortInput('99999', DEFAULT_MIXED_PORT)).toBeNull();
  });

  it('非纯数字一律判非法 —— 不做 Number() 宽松转换（否则「看到的 ≠ 落盘的」）', () => {
    // Number(' 80 ')=80、Number('7e3')=7000、Number('-1')=-1、Number('7890.5')=7890.5：
    // 全部是「用户看到的字符串与落盘值不一致」，故在正则那一关就拒掉。
    for (const bad of ['abc', '78 90', '7e3', '-1', '7890.5', '0x1f5a', '+7890']) {
      expect(normalizePortInput(bad, DEFAULT_MIXED_PORT), `${bad} 不该被放行`).toBeNull();
    }
  });

  it('比后端严：差集 1..1023 由 UI 拦下（方向安全——UI 放行的后端必收）', () => {
    // 后端 crates/store/src/validate.rs:264-279 的 validate_port 是 1..=65535。
    expect(MIN_LISTEN_PORT).toBe(1024);
    expect(MAX_LISTEN_PORT).toBe(65535);
    expect(normalizePortInput('1', DEFAULT_MIXED_PORT)).toBeNull();
  });

  it('fallback 由调用方给 —— 两个端口的默认值不同，不得写死成一个', () => {
    expect(normalizePortInput('', 9090)).toBe(9090);
    expect(normalizePortInput('', 7890)).toBe(7890);
  });
});

describe('showsHardwareAccelRow —— 硬件加速行按平台显隐', () => {
  it('mac 不渲染（WKWebView 无关 GPU 途径 → no-op 死开关，用户要求去掉）', () => {
    expect(showsHardwareAccelRow('mac')).toBe(false);
  });

  it('win 渲染（WEBVIEW2 --disable-gpu 有效，是排障逃生门）', () => {
    expect(showsHardwareAccelRow('win')).toBe(true);
  });

  it('lin 渲染（WEBKIT_DISABLE_DMABUF_RENDERER 有效）', () => {
    expect(showsHardwareAccelRow('lin')).toBe(true);
  });

  it('undefined（非 Tauri 预览 / data-os 未落定）渲染——宁可多显示排障开关也不误藏', () => {
    expect(showsHardwareAccelRow(undefined)).toBe(true);
  });
});

describe('windowEffectsDescKey —— 窗口特效说明按平台选键', () => {
  it('mac → mac 版键（只讲毛玻璃）', () => {
    expect(windowEffectsDescKey('mac')).toBe('settings.general.windowEffectsDescMac');
  });

  it('win → win 版键（只讲 Mica）', () => {
    expect(windowEffectsDescKey('win')).toBe('settings.general.windowEffectsDescWin');
  });

  it('lin → mac 版键兜底（该行在 Linux 由 CSS 隐藏，实际不展示）', () => {
    expect(windowEffectsDescKey('lin')).toBe('settings.general.windowEffectsDescMac');
  });

  it('undefined（非 Tauri 预览）→ mac 版键兜底', () => {
    expect(windowEffectsDescKey(undefined)).toBe('settings.general.windowEffectsDescMac');
  });

  it('两版 i18n 键在 zh-CN 存在且各自单平台（拆分未回退成混写）', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const url = await import('node:url');
    const here = path.dirname(url.fileURLToPath(import.meta.url));
    const zhPath = path.resolve(here, '../../../i18n/locales/zh-CN.json');
    const zh = JSON.parse(fs.readFileSync(zhPath, 'utf8')) as {
      settings: { general: Record<string, string> };
    };
    const mac = zh.settings.general.windowEffectsDescMac ?? '';
    const win = zh.settings.general.windowEffectsDescWin ?? '';
    expect(mac, 'zh-CN 缺 windowEffectsDescMac').toBeTruthy();
    expect(win, 'zh-CN 缺 windowEffectsDescWin').toBeTruthy();
    // mac 版只提 macOS 毛玻璃、不得再夹带 Windows Mica；win 版反之。
    expect(mac).toContain('毛玻璃');
    expect(mac).not.toContain('Mica');
    expect(win).toContain('Mica');
    expect(win).not.toContain('毛玻璃');
    // 旧混写键必须已删除（否则组件读新键、旧键成孤儿）。
    expect(
      (zh.settings.general as Record<string, unknown>).windowEffectsDesc,
      '旧 windowEffectsDesc 混写键应已删除',
    ).toBeUndefined();
  });
});

/**
 * 语言说明按平台选键 —— mac 版要多说一句「原生对话框重启后跟随」。
 *
 * 为什么值一条测试：错向的代价不对称。mac 上误用通用版 ⇒ 承诺「即时生效」，
 * 而用户改完语言去点导出备份，看到的仍是旧语言的系统对话框 —— 文案在撒谎；
 * 反向（Linux/Win 误用 mac 版）⇒ 给两个根本没有这层机制的平台挂一条永远用不上的重启提示。
 */
describe('languageDescKey —— 语言说明按平台选键', () => {
  it('mac → mac 版键（多一句原生对话框需重启）', () => {
    expect(languageDescKey('mac')).toBe('settings.display.languageDescMac');
  });

  it('win / lin → 通用版键（这两个平台不经 AppleLanguages 协商）', () => {
    expect(languageDescKey('win')).toBe('settings.display.languageDesc');
    expect(languageDescKey('lin')).toBe('settings.display.languageDesc');
  });

  it('undefined（非 Tauri 预览）→ 通用版键，不承诺不存在的重启行为', () => {
    expect(languageDescKey(undefined)).toBe('settings.display.languageDesc');
  });

  it('两版键在全部 5 个 locale 都存在，且 mac 版确实比通用版多说了重启', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const url = await import('node:url');
    const here = path.dirname(url.fileURLToPath(import.meta.url));
    for (const loc of ['en-US', 'zh-CN', 'zh-TW', 'ru', 'fa']) {
      const p = path.resolve(here, `../../../i18n/locales/${loc}.json`);
      const json = JSON.parse(fs.readFileSync(p, 'utf8')) as {
        settings: { display: Record<string, string> };
      };
      const base = json.settings.display.languageDesc ?? '';
      const mac = json.settings.display.languageDescMac ?? '';
      expect(base, `${loc} 缺 languageDesc`).toBeTruthy();
      expect(mac, `${loc} 缺 languageDescMac —— 该语种 mac 用户会看到一个空说明`).toBeTruthy();
      // 判据不是「文案不同」而是「mac 版更长」：mac 版是在通用版基础上追加限制说明，
      // 若某语种把它翻译成与通用版等价的一句，那条 mac 专属限制就没传达到。
      expect(
        mac.length,
        `${loc} 的 languageDescMac 不比 languageDesc 长 —— 多出来的那句「原生对话框需重启」没写进去`,
      ).toBeGreaterThan(base.length);
      // 五语文案都必须点名 Polaris（要重启的是哪个东西），否则「重启」指代不明（重启系统？）。
      expect(mac, `${loc} 的 languageDescMac 没点名 Polaris`).toContain('Polaris');
    }
  });
});

/**
 * 订阅自动更新三态：判据 1:1 对应后端 `subscription_scheduler.rs::select_due` 的门链。
 * 逐格穷举（总开关 × 间隔），第三格就是真机上会误导用户的那一格。
 */
describe('subscriptionAutoUpdateStatus', () => {
  it('总开关关 → off（两条腿都不跑，无论间隔）', () => {
    expect(subscriptionAutoUpdateStatus({ autoUpdateSubscriptionOnStart: false })).toBe('off');
    expect(
      subscriptionAutoUpdateStatus({
        autoUpdateSubscriptionOnStart: false,
        subscriptionUpdateIntervalHours: 12,
      })
    ).toBe('off');
    // 字段缺省（存量配置无该键）也按未开处理——后端判的是 `!= Some(true)`。
    expect(subscriptionAutoUpdateStatus({})).toBe('off');
  });

  it('总开关开 + 间隔「仅手动」(0) → startup-only（启动腿仍跑，周期腿整轮返空）', () => {
    // 变异：把 0 当成「没填」回落默认 12h（后端 #18 修过的老写法）→ 本条转红。
    expect(
      subscriptionAutoUpdateStatus({
        autoUpdateSubscriptionOnStart: true,
        subscriptionUpdateIntervalHours: 0,
      })
    ).toBe('startup-only');
  });

  it('总开关开 + 间隔 N 小时 → active', () => {
    expect(
      subscriptionAutoUpdateStatus({
        autoUpdateSubscriptionOnStart: true,
        subscriptionUpdateIntervalHours: 6,
      })
    ).toBe('active');
    // 间隔字段缺省 → 后端回落 DEFAULT_INTERVAL_HOURS（周期腿照跑）→ active，不是 startup-only。
    expect(subscriptionAutoUpdateStatus({ autoUpdateSubscriptionOnStart: true })).toBe('active');
  });
});

/**
 * 段级译名表 —— 守两条：键名不能拼错（拼错 = 静默回落裸键名，没有任何门会红），
 * 且译名必须在**五个**语种里都有（缺一个语种 = 那个语种的用户看到 i18n key 本身）。
 */
describe('STAGED_SETTING_SECTION_LABELS', () => {
  it('每个配置键都是真实的 Class B 键（拼错就静默失效）', () => {
    // 变异对照：把 `dnsConfig` 写成 `dnsConfigs` → 本条转红。
    for (const key of Object.keys(STAGED_SETTING_SECTION_LABELS)) {
      expect(USER_CONFIG_FIELDS as readonly string[], `${key} 不是 UserConfig 字段`).toContain(key);
    }
  });

  it('每条译名在五个语种里都可寻址', () => {
    const locales = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'];
    for (const loc of locales) {
      const json = JSON.parse(
        readFileSync(fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)), 'utf8'),
      ) as Record<string, unknown>;
      for (const path of Object.values(STAGED_SETTING_SECTION_LABELS)) {
        const value = path
          .split('.')
          .reduce<unknown>((node, seg) => (node as Record<string, unknown> | undefined)?.[seg], json);
        expect(value, `${loc} 缺 ${path}`).toBeTypeOf('string');
      }
    }
  });
});

/* ════════════════════════════════════════════════════════════════════════════
 * 无摘要明示（U4 轻方案）—— 两个字段、两个时机，不许合并
 * ════════════════════════════════════════════════════════════════════════════ */

describe('releaseShipsDigest —— 逐条对齐 commands/updater.rs::resolve_expected_digest', () => {
  it('有非空 sha256 ⇒ 有摘要（下载腿会做强校验）', () => {
    expect(releaseShipsDigest({ sha256: 'a'.repeat(64) })).toBe(true);
  });

  it('字段缺失 ⇒ 无摘要（后端 `let Some(raw) = .. else { continue }`）', () => {
    expect(releaseShipsDigest({})).toBe(false);
  });

  it('空串 / 纯空白 ⇒ 无摘要（后端 `hex.trim()` 后 `is_empty()` 即 continue）', () => {
    // 写成 `!!raw` 会把 "   " 判成有摘要 ⇒ 后端不校验、UI 却不提示 = 本批要消掉的静默腿。
    expect(releaseShipsDigest({ sha256: '' })).toBe(false);
    expect(releaseShipsDigest({ sha256: '   ' })).toBe(false);
    expect(releaseShipsDigest({ sha256: '\t\n ' })).toBe(false);
  });

  it('null / undefined 的 updateInfo ⇒ 无摘要（尚未查到版本时不得误报「有」）', () => {
    // 不测 `{ sha256: null }`：后端对「字段在、非字符串」是**拒装**，本函数判 false 与它
    // 故意不对齐（成因见 releaseShipsDigest 文档）。把它写成通过用例等于把一格已知失真
    // 登记成「支持的行为」，而签名也不再接纳 null。
    expect(releaseShipsDigest(null)).toBe(false);
    expect(releaseShipsDigest(undefined)).toBe(false);
  });

  it('**不校验 hex 形态**：坏 hex 仍算「有摘要」', () => {
    // 后端对坏 hex 的处理是照常进 `verify_hex_digest` 然后 `InvalidExpectedHash` **拒装**，
    // 不是当成「本来就没摘要」放行。这里若判 false，就会在一次注定失败的下载前先讲一段
    // 不成立的「该版本没有摘要」。
    expect(releaseShipsDigest({ sha256: 'not-a-hex' })).toBe(true);
    expect(releaseShipsDigest({ sha256: 'ABC' })).toBe(true);
  });
});

describe('appDownloadIntegrity —— unknown 与 unverified 必须分得开', () => {
  it('verified:true ⇒ verified', () => {
    expect(appDownloadIntegrity({ verified: true })).toBe('verified');
  });

  it('verified:false ⇒ unverified（唯一该出提示的那一格）', () => {
    expect(appDownloadIntegrity({ verified: false })).toBe('unverified');
  });

  it('回包里没有 verified ⇒ unknown，**不是** unverified', () => {
    // 自动下载腿（startup_tasks::spawn_auto_download）只推 `downloaded` 事件、没有回包，
    // 折叠成 unverified 会凭空造一条警告，折叠成 verified 则是假绿。
    expect(appDownloadIntegrity({})).toBe('unknown');
    expect(appDownloadIntegrity(null)).toBe('unknown');
    expect(appDownloadIntegrity(undefined)).toBe('unknown');
    expect(appDownloadIntegrity({ verified: null })).toBe('unknown');
  });

  it('穷尽性：任何输入都落在三态里，且只有布尔 false 落 unverified', () => {
    // 真穷尽 —— 输入面里带上会绕过 `=== true` / `=== false` 的**类真/类假**值：
    // 判据若被抄成 `verified ? .. : 'unverified'`，`0` / `''` / `'false'` 就会错落。
    const inputs: unknown[] = [
      { verified: true },
      { verified: false },
      {},
      null,
      undefined,
      { verified: undefined },
      { verified: 0 },
      { verified: 1 },
      { verified: '' },
      { verified: 'false' },
    ];
    const verdicts = inputs.map((v) => appDownloadIntegrity(v as { verified?: boolean }));
    for (const [i, verdict] of verdicts.entries()) {
      expect(['verified', 'unverified', 'unknown'], `第 ${i} 个输入落到三态之外`).toContain(verdict);
    }
    // 只有布尔 false 那一个输入配得上 unverified（= 唯一会出警告的那一格）。
    expect(verdicts.filter((v) => v === 'unverified')).toHaveLength(1);
    expect(verdicts[1]).toBe('unverified');
    expect(verdicts.filter((v) => v === 'verified')).toHaveLength(1);
    expect(verdicts[0]).toBe('verified');
  });
});

describe('progressResetsIntegrity —— 对整个 status 联合闭合的真值表', () => {
  /**
   * 期望表也写成 `Record<UpdateProgress['status'], boolean>`：**两侧都靠类型强制全键**。
   * `UpdateProgress['status']` 将来加一个成员 ⇒ 实现那张表与这张期望表**同时 tsc 红**，
   * 而不是变成「监听器里第三个没人补的分支」这种运行期静默漏项。
   */
  const EXPECTED: Record<UpdateProgress['status'], boolean> = {
    idle: false,
    checking: false,
    'no-update': false,
    'update-available': false,
    // 唯一两条由真实下载腿发出的进度；事件是 app 级广播，可能来自别的窗口发起的下载。
    downloading: true,
    // 复用本地已有包那条腿**只发这一条**，不发 downloading ⇒ 少了它就漏掉整条复用路径。
    downloaded: true,
    // 失败不落位（tmp 由 RAII 清掉，dest 未动）⇒ 盘上旧包与它的结论都还成立。
    error: false,
  };

  it('逐个 status 断言该不该复位（键由类型穷尽，不是手写数组）', () => {
    const statuses = Object.keys(EXPECTED) as UpdateProgress['status'][];
    // 取材自检：键集空/塌缩会让下面的循环 0 次断言而「恰好」全绿。
    expect(statuses.length, '期望表键数不对（联合是 7 个成员）').toBe(7);
    for (const status of statuses) {
      expect(progressResetsIntegrity(status), `${status} 的复位判定与真值表不符`).toBe(
        EXPECTED[status],
      );
    }
  });

  it('恰好两条 status 触发复位，且正是下载腿真会发的那两条', () => {
    // 「全 false」和「全 true」两个方向都要说话：恒 false ⇒ 跨包污染回来；
    // 恒 true ⇒ 每次检查/失败都把结论抹掉，明示在该出现时静默缺席。
    const resetting = (Object.keys(EXPECTED) as UpdateProgress['status'][]).filter((s) =>
      progressResetsIntegrity(s),
    );
    expect(resetting.sort()).toEqual(['downloaded', 'downloading']);
  });
});

/**
 * 剥注释内核：用 TS 自己的 parser 逐 token 取注释区间并抹成空格（保留换行与偏移，行号不漂）。
 *
 * # 为什么本文件所有源码级判据都必须先剥注释
 *
 * 本仓已被这一格坑过一次（跨批复审 Low：「TS 取材器不剥块注释 ⇒ 注释伪造订阅 + 真订阅被删仍
 * 全绿」）。两个方向都被污染过：正向断言可以被一句注释**假装满足**；负向断言（「不得直接读
 * `.sha256`」）会被一句解释该字段的注释**误判成违规**。
 *
 * 2026-08-17 由 [`readTsx`] 内提出来：「全仓 `updateApi.check(` 普查」那道门要对**任意**
 * `ui/src` 下的源码剥注释，而 `readTsx` 的自检是给单个组件文件量身做的（要求含块注释、要求
 * `export default function <Component>`）。两份剥法早晚会漂，且漂的时候两边都还是绿的。
 */
function stripTsComments(file: string, raw: string): string {
  const sf = ts.createSourceFile(file, raw, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const out = [...raw];
  const blank = (pos: number, end: number) => {
    for (let i = pos; i < end; i++) if (out[i] !== '\n') out[i] = ' ';
  };
  // `forEachChild` 跳过 token 节点，而 `{/* … */}` 这类 JSX 注释正挂在 token 的前导上，
  // 故必须走 `getChildren`（含 token）。
  const walk = (n: ts.Node) => {
    for (const r of ts.getLeadingCommentRanges(raw, n.getFullStart()) ?? []) blank(r.pos, r.end);
    for (const r of ts.getTrailingCommentRanges(raw, n.getEnd()) ?? []) blank(r.pos, r.end);
    for (const c of n.getChildren(sf)) walk(c);
  };
  walk(sf);
  return out.join('');
}

/**
 * 单个组件文件的取材器（缺省 `SettingsUpdate.tsx`）：读盘 → [`stripTsComments`] → 三条自检。
 *
 * 自检存在的理由与判据本身同等重要：路径漂走 / 剥过头都会让下游断言在**空串**上「恰好」通过 =
 * 假绿。故断言「文件够长」「剥完与原文不同」「注释标记没了」「代码骨架还在」四件事，其中骨架那条
 * 按 `rel` 推出组件名走，参数化之后才不会恒真。
 *
 * **2026-08-17 由「无摘要明示」describe 内提到模块作用域**（原地不动地搬，行为零变化）：预发布
 * 档次那道门要断言的也是这张更新卡的分状态结构。两份取材器 + 两份「什么算一个 `us` 态分支」的
 * 定义早晚会对不上，而它们对不上时**两边都还是绿的** —— 那正是本文件反复在防的形态。
 */
async function readTsx(rel = 'SettingsUpdate.tsx') {
  const fs = await import('node:fs');
  const path = await import('node:path');
  const { fileURLToPath: toPath } = await import('node:url');
  const dir = path.dirname(toPath(import.meta.url));
  const file = path.join(dir, rel);
  const raw = fs.readFileSync(file, 'utf8');
  // 取材自检：路径漂走会让下面全部断言在空串上「恰好」通过 = 假绿。
  expect(raw.length, `取材文件太短或不对：${file}`).toBeGreaterThan(2000);

  const src = stripTsComments(file, raw);
  // 剥注释自检（正负对照）：本文件必有块注释 ⇒ 剥完必须**真的不一样**，且注释标记没了、
  // 代码骨架还在（不能把整份剥成空白还一路绿）。
  // 不写 `src.length === raw.length`：`blank()` 是原地单字符替换，那条恒真、零信息量。
  expect(src, '剥注释后与原文逐字相同 ⇒ 什么都没剥掉').not.toBe(raw);
  expect(raw.includes('/**'), '取材文件本应含块注释（否则本自检无信息量）').toBe(true);
  expect(src.includes('/**'), '注释未被剥掉').toBe(false);
  // 骨架自检按**取材的那个文件**走（`rel` 换了组件名也得跟着换），否则参数化之后这条会恒假/恒真。
  const component = rel.replace(/^.*\//, '').replace(/\.tsx$/, '');
  expect(
    src.includes(`export default function ${component}`),
    `剥过头，${component} 的代码骨架没了`,
  ).toBe(true);
  return src;
}

/** 抽出 `{us === 'X' && (` 起、到下一个 `{us === ` 为止的那一段 JSX。 */
function stateBlock(src: string, state: string): string {
  const start = src.indexOf(`{us === '${state}'`);
  expect(start, `SettingsUpdate 里找不到 ${state} 态分支`).toBeGreaterThan(-1);
  const rest = src.slice(start + 1);
  const nextIdx = rest.indexOf('{us === ');
  const block = nextIdx === -1 ? rest : rest.slice(0, nextIdx);
  expect(block.length, `${state} 态分支取材为空`).toBeGreaterThan(200);
  return block;
}

describe('无摘要明示：接线面 + 五语文案', () => {
  it('组件消费两个判据本身，不并行复刻字段读法', async () => {
    const src = await readTsx();
    expect(src.includes('releaseShipsDigest'), '必须消费 releaseShipsDigest').toBe(true);
    expect(src.includes('appDownloadIntegrity'), '必须消费 appDownloadIntegrity').toBe(true);
    // 单点判据：直接读 `.sha256` / `.verified` 就是在组件里另写一份口径，
    // 后端 `resolve_expected_digest` 的 trim/空串语义会在那份复刻里丢掉。
    expect(src.includes('.sha256'), '不得在组件里直接读 sha256').toBe(false);
    // `.verified` 这条是**前瞻守卫**：本文件今天连注释里都没有它，取材器就算完全不剥注释
    // 它也是绿的 ⇒ 它证明不了取材器有效，不算进本批的变异收据。留着是为挡将来那次复刻。
    expect(src.includes('.verified'), '不得在组件里直接读 verified').toBe(false);
  });

  it('两处明示各由**各自那个字段**驱动，不得对调或合并', async () => {
    const src = await readTsx();
    // 检查阶段那一格只能来自 updateInfo.sha256（经 releaseShipsDigest）。
    expect(src).toMatch(/const releaseDigestMissing = !releaseShipsDigest\(updateInfo\)/);
    // 下载之后那一格只能来自回包 verified（经 appDownloadIntegrity），且只认 unverified。
    expect(src).toMatch(/const downloadUnverified = downloadIntegrity === 'unverified'/);
    expect(src).toMatch(/setDownloadIntegrity\(appDownloadIntegrity\(r\)\)/);

    const available = stateBlock(src, 'available');
    expect(available.includes('digestMissingBefore'), 'available 态缺下载前明示').toBe(true);
    expect(available.includes('releaseDigestMissing'), 'available 态必须由 sha256 判据驱动').toBe(true);
    expect(available.includes('downloadUnverified'), 'available 态不得用下载后的 verified').toBe(false);
    expect(available.includes('digestMissingAfter'), 'available 态不得用下载后的文案').toBe(false);

    // `downloaded` 与 `manual` 是**互斥**的两个「包已在盘、下一步就是装/解压」态：
    // 便携腿转 manual 后 downloaded 整块不渲染，只挂一条腿等于在便携用户那里静默撤掉明示。
    for (const state of ['downloaded', 'manual'] as const) {
      const block = stateBlock(src, state);
      expect(block.includes('digestMissingAfter'), `${state} 态缺下载后明示`).toBe(true);
      expect(block.includes('downloadUnverified'), `${state} 态必须由 verified 判据驱动`).toBe(true);
      expect(block.includes('releaseDigestMissing'), `${state} 态不得用检查期的 sha256`).toBe(false);
      expect(block.includes('digestMissingBefore'), `${state} 态不得用下载前的文案`).toBe(false);
    }
  });

  it('**三个**入口都必须把上次的校验结论清回 unknown，且第三个走真值表而非内联枚举', async () => {
    const src = await readTsx();
    // 不清 ⇒ 换了个包还举着上一次的「未校验」（或反过来，把旧的「已校验」当新包的背书）。
    // 第三个入口最容易漏：`onProgress` 收到的事件可能来自**别的窗口**发起的下载
    // （`update_popup_action` 的「更新/重试」、`spawn_auto_download`），本页拿不到那次回包。
    const between = (from: string, to: string) => {
      const a = src.indexOf(from);
      const b = src.indexOf(to);
      expect(a, `取材锚点不在了：${from}`).toBeGreaterThan(-1);
      expect(b, `取材锚点不在了：${to}`).toBeGreaterThan(a);
      return src.slice(a, b);
    };

    const checkFn = between('async function checkUpdate(', 'async function skipVersion(');
    const dlFn = between('async function downloadUpdate(', 'async function installUpdate(');
    expect(checkFn.includes("setDownloadIntegrity('unknown')"), 'checkUpdate 未复位').toBe(true);
    expect(dlFn.includes("setDownloadIntegrity('unknown')"), 'downloadUpdate 未复位').toBe(true);

    // 监听器：判据必须**经谓词**下达，且只有一个调用点。
    // 谓词提到纯逻辑层之后，接线这半失守的形态有二，两条都要挡：
    //  ① 谓词根本没被调（有人把它抄成内联 `p.status === 'downloading' || ...`）；
    //  ② 谓词调了、但下面的 status 分支里**又**补了一次 —— 枚举又长回来了。
    const listener = between('updateApi.onProgress(', 'async function checkUpdate(');
    expect(
      listener.includes('progressResetsIntegrity(p.status)'),
      '监听器必须经 progressResetsIntegrity 判定，不得内联复刻 status 枚举',
    ).toBe(true);
    // 那唯一一次必须挂在谓词上，而不是躺在某个 status 分支里。
    expect(listener).toMatch(
      /if \(progressResetsIntegrity\(p\.status\)\) setDownloadIntegrity\('unknown'\);/,
    );
    // 反向：status 分支体内不得再出现复位（谓词是唯一判据）。
    // **这条排在计数之前**：分支里多补一次同样会让计数不符，若计数先报，本条就永远不是首个
    // 失败者 = 一条报不出话的装饰断言。排在前面它才能指出「多的那次在哪个分支」。
    for (const arm of ['downloading', 'downloaded', 'error'] as const) {
      const armStart = listener.indexOf(`p.status === '${arm}'`);
      expect(armStart, `监听器缺 ${arm} 分支`).toBeGreaterThan(-1);
      const armBody = listener.slice(armStart, listener.indexOf('}', armStart + 20));
      expect(armBody.length, `${arm} 分支取材为空`).toBeGreaterThan(20);
      expect(
        armBody.includes("setDownloadIntegrity('unknown')"),
        `${arm} 分支里不得再内联复位（判据只有真值表一处）`,
      ).toBe(false);
    }
    const listenerResets = listener.match(/setDownloadIntegrity\('unknown'\)/g) ?? [];
    expect(listenerResets.length, '监听器只许有一个复位调用点（多于一个 = 枚举又长回来了）').toBe(1);

    // 全局计数收尾：三个入口各一次，多一次少一次都说话。
    const resets = src.match(/setDownloadIntegrity\('unknown'\)/g) ?? [];
    expect(resets.length, 'checkUpdate / downloadUpdate / 监听器 = 3 处复位').toBe(3);
  });

  it('三个键在五个语种里都非空，且都点名 sha256（把缺口限定在「摘要」这一级）', async () => {
    const fs = await import('node:fs');
    const keys = ['digestMissingTag', 'digestMissingBefore', 'digestMissingAfter'] as const;
    for (const loc of ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa']) {
      const json = JSON.parse(
        fs.readFileSync(fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)), 'utf8'),
      ) as { settings: { update: Record<string, string> } };
      for (const k of keys) {
        const v = json.settings.update[k];
        expect(v, `${loc} 缺 settings.update.${k}`).toBeTypeOf('string');
        expect(v.trim().length, `${loc} 的 ${k} 是空串`).toBeGreaterThan(0);
      }
      // 两条说明必须写出「缺的是 sha256 摘要」——只写「未校验」会被读成「什么都没查」，
      // 而实际还有清单体积 / Content-Length 两级弱校验（虽都有条件，不写进文案）。
      expect(json.settings.update.digestMissingBefore, `${loc} 下载前文案未点名 sha256`).toContain('sha256');
      expect(json.settings.update.digestMissingAfter, `${loc} 下载后文案未点名 sha256`).toContain('sha256');
    }
  });

  it('文案不得暗示「已校验」，也不得把它写成一次失败', async () => {
    const fs = await import('node:fs');
    const read = (loc: string) =>
      (
        JSON.parse(
          fs.readFileSync(fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)), 'utf8'),
        ) as { settings: { update: Record<string, string> } }
      ).settings.update;
    const zh = read('zh-CN');
    const en = read('en-US');
    for (const k of ['digestMissingBefore', 'digestMissingAfter'] as const) {
      expect(zh[k], `zh-CN ${k} 不得声称已校验`).not.toMatch(/已校验|校验通过/);
      // 无摘要不是错误、不阻断安装：写成「失败」会把一次正常更新描述成故障。
      expect(zh[k], `zh-CN ${k} 不得写成失败`).not.toMatch(/失败|错误/);
      expect(en[k], `en-US ${k} 不得声称 verified`).not.toMatch(/\bverified\b/i);
      expect(en[k], `en-US ${k} 不得写成 failed`).not.toMatch(/\bfail(ed|ure)?\b/i);
    }
    // 下载前那条必须明说不阻断（用户此刻要决定的正是「还下不下」）。
    expect(zh.digestMissingBefore, 'zh-CN 未说明不阻断更新').toMatch(/不会因此被阻断|不阻断/);
    expect(en.digestMissingBefore, 'en-US 未说明不阻断更新').toMatch(/not blocked/i);
  });
});

/**
 * 预发布档次明示：接线面 + 五语文案。
 *
 * # 为什么必须有这道门
 *
 * 本页的手动「检查更新」是**全仓唯一**会返回预发布的 App 更新入口（`updateApi.check(true)`）——
 * 启动自动检查、托盘检查更新（连同弹窗点「更新」时的复查）共用后端
 * `PUSH_UPDATE_INCLUDE_PRERELEASE` 恒只看正式版，顶部常驻横幅同口径。于是「用户手里这份是不是
 * 预发布」这件事，只有在这张卡上说得出来。
 *
 * 不说的话，用户只能从 tag 文本里猜档次 —— 而 GitHub 的 `prerelease` 是一个与 tag 命名**无关**的
 * 独立布尔，一个打成 `v1.3.0` 的 release 完全可以是预发布。`isPrerelease` 一路从 `AppUpdateInfo`
 * 传到前端却零消费，正是这个缺口的形态。
 *
 * # 与「无摘要明示」正交
 *
 * 那道门管**校验状态**（这份字节能不能验真），本门管**版本档次**（这个版本成不成熟）。一个版本
 * 完全可能既是预发布又没带摘要，两条明示会同框出现 —— 故本门只断言自己那一半，不碰对方的判据。
 */
describe('预发布档次明示：接线面 + 五语文案', () => {
  it('前提自检：本页确实还在拉预发布（否则本门无的放矢）', async () => {
    const src = await readTsx();
    // 前提没了要连同标注一并复核，而不是让本门恒绿空转。
    expect(src.includes('updateApi.check(true)'), '本页不再以 check(true) 拉预发布').toBe(true);
    // 反向：真值源必须是后端那个布尔，不是从版本号字符串里猜档次。
    expect(src.includes('isPrerelease'), '拿得到 isPrerelease 却不消费').toBe(true);
    expect(
      /includes\(['"`]beta|match\(\/.*(alpha|beta|rc)/.test(src),
      '不得从 tag 文本反推档次 —— GitHub 的 prerelease 与 tag 命名无关',
    ).toBe(false);
  });

  /**
   * 三条腿必须都挂徽标：`available` 是「要不要下」的决策点，`downloaded` 是「要不要重启装上去」
   * （不可逆，离真的执行这些字节最近），`manual` 与 `downloaded` **互斥** —— 便携腿转 manual 后
   * `downloaded` 整块不渲染，只挂两条等于在便携用户那里静默撤掉标注。判据与「无摘要」那条同源。
   *
   * **变异探针**：任删一条腿的徽标 ⇒ 该腿转红。
   */
  it('徽标挂在三条腿上（available / downloaded / manual），不只挂决策那一屏', async () => {
    const src = await readTsx();
    for (const state of ['available', 'downloaded', 'manual'] as const) {
      const block = stateBlock(src, state);
      // 两个字符串各自出现还不够 —— 徽标必须**由档次判据本身**驱动。只查「都出现过」时，
      // 写成 `{true && <Pill>prereleaseTag</Pill>}` 再在别处提一句 isPrerelease 也能过。
      // 允许前置一个资格判据（安装屏那两条腿有 `downloadedPath &&`，见下一条门），但
      // `isPrerelease` 必须仍在**同一个表达式**里 —— 否则 `{true && <Pill>}` 加一句无关的
      // `isPrerelease` 也能过。
      expect(
        /\{(?:downloadedPath && )?updateInfo\??\.isPrerelease\s*&&\s*\([\s\S]{0,240}?prereleaseTag/.test(
          block,
        ),
        `${state} 态的预发布徽标没有挂在 updateInfo.isPrerelease 这个条件上`,
      ).toBe(true);
    }
  });

  /**
   * 两枚徽标同为 `Pill variant="warn"`、会同框出现（一个版本完全可能既是预发布又没带摘要），
   * 靠**位置**分工：预发布贴在版本号后（限定版本），无摘要留在行尾（限定制品）。都堆到行尾就是
   * 一坨警告色，读者无从判断谁在说谁。
   *
   * 这条论证此前一个字的判据都没有 —— 把预发布 Pill 挪到行尾 digest Pill 旁边，整段论证被推翻
   * 而门全绿。源码顺序即渲染顺序，故「预发布出现在无摘要之前」同时蕴含了「两者不在同一个槽位」。
   *
   * **变异探针**：把预发布 Pill 挪到 `</div>` 之后（行尾槽位，digest Pill 旁）⇒ 转红。
   */
  it('两枚徽标按「版本先、制品后」排布，不挤在同一个槽位', async () => {
    const src = await readTsx();
    // 三条腿都是两枚 Pill 同框（`downloaded`/`manual` 挂的是 digestMissingAfter 那一档），
    // 位置约定对三处同样成立 —— 只钉 available 等于给另两处发了免死金牌。
    for (const state of ['available', 'downloaded', 'manual'] as const) {
      const block = stateBlock(src, state);
      const pre = block.indexOf('prereleaseTag');
      const digest = block.indexOf('digestMissingTag');
      expect(pre, `${state} 态缺预发布徽标`).toBeGreaterThan(-1);
      expect(digest, `${state} 态缺无摘要徽标（本门的对照方没了）`).toBeGreaterThan(-1);
      expect(pre, `${state} 态：预发布徽标必须排在无摘要徽标之前（版本先、制品后）`).toBeLessThan(
        digest,
      );
    }
  });

  /**
   * 说明文案只挂在 `available`：那是用户决定「要不要拿一份预发布」的那一屏。
   *
   * **刻意不跟着重复三遍**（与「无摘要」腿的处置不同，这里如实记下差异）：那边 before/after 说的
   * 是两件不同的事（「将要取回未署摘要的包」vs「即将执行未经校验的字节」），而档次的说明三处一字
   * 不差 —— 抄三遍只是噪声。档次这个**事实**由徽标在三条腿上持续持有，**解释**留在做决定的那屏。
   */
  /**
   * 徽标的**资格判据**：安装屏那两条腿只在「盘上这份是本页下的」时才敢说档次。
   *
   * `updateInfo` 描述的是本页**上一次检查**的结果，而 `us` 会被**别的窗口**的下载广播推到
   * `downloaded`（弹窗「更新/重试」、`spawn_auto_download`）—— 那时两者毫无因果关系：用户查到
   * `v1.3.0-beta.1` 没下、外部腿下了 `v1.2.0` 正式包 ⇒ 卡片举着预发布徽标去描述一份正式包。
   *
   * 判据取 `downloadedPath`：它的**唯一**写点是 `downloadUpdate` 的成功分支（外部广播那条腿从不
   * 设它）⇒ 恰好等价于「这次下载是本页完成的」。漏报（外部腿下的预发布不显示徽标）方向安全，
   * 正解归 W5。
   *
   * **为什么不是「清空 `updateInfo`」**：那条会让 `us==='error'` 的「重试」（该分支唯一按钮，
   * 直通 `downloadUpdate` 首行 `if (!updateInfo) return`）变成哑键 —— 用真缺陷换假话，不划算。
   * 本门连带把这条也钉住：`downloadUpdate` 的入口守卫仍在，且监听器**不得**清空 `updateInfo`。
   *
   * **变异探针**：任一腿去掉 `downloadedPath &&` ⇒ 转红；监听器里加回 `setUpdateInfo(null)` ⇒ 转红。
   */
  it('安装屏的徽标只描述本页下的那份包（不清数据，只收窄断言）', async () => {
    const src = await readTsx();
    for (const state of ['downloaded', 'manual'] as const) {
      const block = stateBlock(src, state);
      expect(
        /\{downloadedPath && updateInfo\?\.isPrerelease &&/.test(block),
        `${state} 态的预发布徽标没有由 downloadedPath 把关 —— 会贴到别的窗口下的正式包上`,
      ).toBe(true);
    }
    // `available` 是本页自己刚查出来的结果，数据源就是对的，不该也被这条判据挡住
    // （那一屏 `downloadedPath` 恒为 null ⇒ 加了资格判据 = 徽标在**决策那一屏**彻底消失，
    // 而那正是整批的立项理由）。
    //
    // ⚠️ 正则必须一路咬到 `prereleaseTag`：该 block 里 `updateInfo.isPrerelease` 出现**两次**
    // （徽标 + 下面的 `prereleaseNote` 说明），只判「有没有这个开头」会被**说明那条**喂饱 ——
    // 给徽标也加上 `downloadedPath &&` 时本条照样绿。这与 Rust 侧「别的臂替本臂作证」同形，
    // 换到了 JSX 上。
    expect(
      /\{updateInfo\.isPrerelease && \([\s\S]{0,240}?prereleaseTag/.test(stateBlock(src, 'available')),
      'available 态的徽标被多加了资格判据 —— 那一屏的 updateInfo 本来就是本页查的，加了等于徽标消失',
    ).toBe(true);
    // 反向：绝不能回到「清空 updateInfo」那条路（会让 error 态的「重试」变哑键）。
    expect(
      src.includes('setUpdateInfo(null)'),
      '监听器又开始清空 updateInfo —— error 态的「重试」会变成哑键',
    ).toBe(false);
    expect(
      src.includes('if (!updateInfo) return'),
      'downloadUpdate 的入口守卫没了 —— 上面那条反向断言就失去了意义',
    ).toBe(true);
  });

  it('说明文案挂在决策那一屏', async () => {
    const src = await readTsx();
    expect(stateBlock(src, 'available').includes('prereleaseNote'), 'available 态缺档次说明').toBe(
      true,
    );
  });

  /**
   * 前端「推」面的**普查**，不是点名。
   *
   * 上一版只 `readTsx('../../layout/AppUpdateBanner.tsx')` 点名横幅一个文件 —— 覆盖面由夹具定，
   * 不由判据定：**新增第三个推面写 `check(true)`，前端全绿，Rust 那道门也管不着（它只扫 `.rs`）**。
   * 这与 Rust 侧刚修掉的「按函数名点三条腿」是同一条教训的另一条腿，故同形修：递归 `ui/src`
   * 收全部 `updateApi.check(`，断言**恰好一处**传 `true` 且位于设置页，其余一律「看得出是正式版」。
   *
   * 缺省值 `check(includePrerelease = false)` 只挡得住裸调用，挡不住显式 `true`，所以判据看实参。
   * 折行写法（源码里是 `updateApi\n  .check(false)`）由 `\s*` 接住。
   *
   * # ⚠️ 与后端同名门的方向**相反**，别改宽
   *
   * Rust 侧 `every_update_check_call_site_uses_the_shared_prerelease_scope` **要求**写共享常量、
   * 拒收字面量；本门恰好反过来 —— 因为两边的处境不同：后端三条腿共用一个口径，常量是**单点真值**；
   * 前端两处口径**天生不同**（横幅是推、设置页是拉），没有可共用的常量，能静态读出的只有字面量。
   *
   * 但「只认字面量」会挡住一次正当重构：前端真长出第三条推腿、按后端那条纪律提一个共享常量出来时，
   * `updateApi.check(PUSH_INCLUDE_PRERELEASE)` 会被判 murky 而转红。故留一个**具名白名单**
   * [`ALLOWED_SCOPE_IDENTS`]：要新增一个名字，就得来这里加一行——判据面显式扩张，而不是把门改宽。
   *
   * # 射程自曝
   *
   * needle 是 `updateApi.check(`，**绕得过去**：直接 `invoke(IPC_CHANNELS.UPDATE_CHECK, {...})`
   * 不经这层封装（今天全仓没有这种写法，`api-client.ts` 是唯一拆包点）。同理，运行期拼出来的实参
   * （`check(cond ? a : b)`）会被判 murky 转红而不是放行 —— 方向安全，但那是拒收不是识别。
   *
   * **变异探针**：横幅改 `check(true)` / 任意新文件里写一处 `check(true)` ⇒ 转红。
   */
  it('全仓前端只有一处含预发布的 App 检查，且在设置页（其余推面一律正式版）', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const { fileURLToPath: toPath } = await import('node:url');
    const uiSrc = path.resolve(path.dirname(toPath(import.meta.url)), '../../..');

    const files: { rel: string; src: string }[] = [];
    const walk = (dir: string) => {
      for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) walk(p);
        else if (/\.tsx?$/.test(e.name) && !/\.test\.tsx?$/.test(e.name)) {
          files.push({
            rel: path.relative(uiSrc, p).replace(/\\/g, '/'),
            src: stripTsComments(p, fs.readFileSync(p, 'utf8')),
          });
        }
      }
    };
    walk(uiSrc);
    // 取材自检：扫到 0 个文件会让下面所有断言在空集上「恰好」通过。
    expect(files.length, `${uiSrc} 下扫到的源文件太少`).toBeGreaterThan(100);

    const sites: { rel: string; arg: string }[] = [];
    for (const { rel, src } of files) {
      for (const m of src.matchAll(/\bupdateApi\s*\.\s*check\s*\(([^)]*)\)/g)) {
        sites.push({ rel, arg: m[1].replace(/\s+/g, '') });
      }
    }
    expect(sites.length, '一处 updateApi.check( 都没扫到 —— 判据面塌了').toBeGreaterThanOrEqual(2);

    const withTrue = sites.filter((s) => s.arg === 'true').map((s) => s.rel);
    expect(withTrue, '含预发布的 App 检查必须**恰好**只有设置页那一处「拉」').toEqual([
      'components/screens/settings/SettingsUpdate.tsx',
    ]);
    // 其余一律「看得出是正式版」：显式 `false`、空参（缺省即 false），或白名单里的具名常量。
    // 白名单今天是空的 —— 前端还没有可共用的口径常量（横幅是推、设置页是拉，两处天生不同）。
    // 真要提一个出来，在这里加一行即可；**判据面显式扩张，不许把 murky 那条改宽**。
    const ALLOWED_SCOPE_IDENTS: readonly string[] = [];
    const murky = sites.filter(
      (s) => !['true', 'false', ''].includes(s.arg) && !ALLOWED_SCOPE_IDENTS.includes(s.arg),
    );
    expect(murky, '有调用点的预发布口径既不是字面量、也不在具名白名单里，无法静态判定').toEqual(
      [],
    );
    // ⚠️ **前瞻守卫，本增量零执行覆盖**（口径同本文件 `.verified` 那条）：`ALLOWED_SCOPE_IDENTS`
    // 今天是空数组 ⇒ 下面这条 `filter` 恒得空集、断言恒真，**不算进本批的变异收据**。它挡的是
    // 将来第一次往白名单加名字的那一刻。
    // 白名单只认**名字**是不够的：某条推腿写 `check(SCOPE)` 而 `SCOPE = true` 会全绿放行，
    // 横幅零标注地举着一条 beta —— 正是这道门存在的理由。故名字进白名单还不算完，它的**值**
    // 必须能在 `ui/src` 里静态解析到 `= false`。加名字仍是显式扩张，口径依旧被读出来。
    const unresolved = ALLOWED_SCOPE_IDENTS.filter(
      (ident) =>
        !files.some(({ src }) =>
          new RegExp(`\\b(?:const|let|var)\\s+${ident}\\s*(?::[^=]+)?=\\s*false\\b`).test(src),
        ),
    );
    expect(
      unresolved,
      '白名单里的常量在 ui/src 里解析不到 `= false` 的初始化式 —— 名字进了白名单，口径却没人读',
    ).toEqual([]);
  });

  it('两个键在五个语种里都非空，且说明都点名 alpha/beta/rc（档次不可从 tag 反推）', async () => {
    const fs = await import('node:fs');
    for (const loc of ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa']) {
      const json = JSON.parse(
        fs.readFileSync(
          fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)),
          'utf8',
        ),
      ) as { settings: { update: Record<string, string> } };
      for (const k of ['prereleaseTag', 'prereleaseNote'] as const) {
        const v = json.settings.update[k];
        expect(v, `${loc} 缺 settings.update.${k}`).toBeTypeOf('string');
        expect(v.trim().length, `${loc} 的 ${k} 是空串`).toBeGreaterThan(0);
      }
      // 只写「预发布版」等于把解释权推回给 tag 文本；必须说出这是 alpha / beta / rc 那一档。
      expect(
        json.settings.update.prereleaseNote,
        `${loc} 的说明未点名 alpha / beta / rc`,
      ).toMatch(/alpha/i);
    }
  });
});
