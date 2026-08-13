/**
 * `settings-logic` 单测 —— 锁死「UI 显示态 ↔ 后端消费口径」的对齐点。
 *
 * 这些函数是设置屏组件的**生产接线点**（组件直接 import 消费，非并行复刻），故断言即真实行为。
 * 重点覆盖「缺省为开」（`!== false`）语义：写成 `!!` 会让存量配置（无该键）显示成「关」而后端按
 * 「开」跑 —— UI 与后台分叉是本批要根治的最恶劣缺陷。
 */
import { describe, it, expect } from 'vitest';
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
} from './settings-logic';

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
