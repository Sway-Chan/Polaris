/**
 * 界面语言水合腿（`syncLanguageChoice` + App.tsx 调用点）的行为门 + 接线守卫。
 *
 * # 修的是什么缺陷
 *
 * `config.language` 是语言选择的真值源，写侧一直是通的（`SettingsDisplay` → `update({language})` →
 * 后端 config），**读侧整条腿不存在**：全 `ui/src` 没有一处 `i18n.changeLanguage`，
 * `persistLanguageChoice` 零调用者 ⇒ `localStorage['polaris.language']` 永远是空 ⇒ i18n 首屏恒解析成
 * `'auto'` 跟随系统。症状是「切界面语言主窗一个字不变，重启也不变」，只有 Rust 侧原生托盘
 * （`tray.rs:90` 直读 config）跟着变。同一条缺腿还打断托盘浮层（`tray/labels.ts` 读同一个键）。
 *
 * # 为什么行为门之外还必须有接线守卫
 *
 * 这个缺陷的本体就在**调用点**：`resolveEffectiveLanguage` / `persistLanguageChoice` 一直存在且正确，
 * 没人调而已。只测 `syncLanguageChoice` 的方法体，把 App.tsx 那个 `useEffect` 整个删掉照样全绿 ——
 * 那正是修复前的状态。故沿用本仓既有守卫模式（`tray/tray-live-wiring.test.ts`、
 * `store/latency-wiring-invariants.test.ts`）。
 *
 * # node 环境的桩
 *
 * 本仓 vitest 是 node 环境无 jsdom（见 `vite.config.ts` test 段）。`i18n/index.ts` 在**模块加载期**就
 * 读 `document`（写 `<html dir/lang>`）与 `localStorage`/`navigator`，故先立桩再动态 import ——
 * 与 `settings/terminal-env-and-fold.test.tsx` 同一先例。
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/** 只保留 i18next 挂载所需的最小面，避免把 React 渲染扯进来（同 terminal-env-and-fold 先例）。 */
vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
}));

/** `<html dir/lang>` 的落点。 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '' },
};

/** 可控的 localStorage 桩 —— 断言「选择真的落盘了」（下次首屏 + 托盘浮层都读它）。 */
const store = new Map<string, string>();
(globalThis as unknown as { localStorage: unknown }).localStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
};

/** 可控的 OS 偏好语言 —— `'auto'` 分支的唯一输入。 */
const sysLangs: { value: string[] } = { value: ['ru-RU'] };
Object.defineProperty(globalThis, 'navigator', {
  value: { get languages() { return sysLangs.value; } },
  configurable: true,
});

const { syncLanguageChoice, default: i18n } = await import('./index');

const LANGUAGE_STORAGE_KEY = 'polaris.language';

/** i18next 的 `changeLanguage` 是异步的（资源已内联，只需让出一次事件循环）。 */
const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  store.clear();
  sysLangs.value = ['ru-RU'];
});

describe('syncLanguageChoice —— 具体码', () => {
  /** 变异对照：把 `i18n.changeLanguage(eff)` 删掉 → 本条转红（= 修复前的状态）。 */
  it('切到具体码时真的改 i18n 运行期语言', async () => {
    await i18n.changeLanguage('en-US');
    expect(syncLanguageChoice('zh-TW')).toBe('zh-TW');
    await flush();
    expect(i18n.language).toBe('zh-TW');
  });

  /** 变异对照：把 `persistLanguageChoice(c)` 删掉 → 本条转红（重启后退回跟随系统）。 */
  it('把选择写进 localStorage（下次首屏 + 托盘浮层的解析对象）', () => {
    syncLanguageChoice('fa');
    expect(store.get(LANGUAGE_STORAGE_KEY)).toBe('fa');
  });

  /** 变异对照：去掉 `migrateLanguageCode` → 落盘成 `fa-IR` 且解析回落系统 → 本条转红。 */
  it('旧码 fa-IR 先迁移成 fa 再落盘', () => {
    expect(syncLanguageChoice('fa-IR')).toBe('fa');
    expect(store.get(LANGUAGE_STORAGE_KEY)).toBe('fa');
  });
});

describe("syncLanguageChoice —— 'auto' 与空值", () => {
  /** 变异对照：把 `|| AUTO_LANGUAGE` 换成 `?? AUTO_LANGUAGE` → 空串不再归一 → 本条转红。 */
  it('null / undefined / 空串 一律归一成 auto 并按系统偏好解析', () => {
    for (const empty of [null, undefined, '']) {
      store.clear();
      expect(syncLanguageChoice(empty)).toBe('ru');
      expect(store.get(LANGUAGE_STORAGE_KEY)).toBe('auto');
    }
  });

  /** 变异对照：把 `getSystemLanguages()` 换成写死数组 → 本条转红。 */
  it('auto 跟随系统偏好（系统换语言 → 解析结果跟着换）', () => {
    sysLangs.value = ['zh-Hant-TW', 'en-US'];
    expect(syncLanguageChoice('auto')).toBe('zh-TW');
    sysLangs.value = ['de-DE'];
    expect(syncLanguageChoice('auto')).toBe('en-US'); // 全不匹配 → DEFAULT_LANGUAGE
  });

  /**
   * 「只改选择不改结果」的切换必须照样落盘 —— 这是 `persistLanguageChoice` 放在幂等判断**之前**
   * 的理由：中文系统上 auto 与 zh-CN 解析出的有效语言相同，若跟着 `changeLanguage` 一起被跳过，
   * 用户「从 auto 显式改成 zh-CN」下次冷启动会退回 auto。
   *
   * 变异对照：把 `persistLanguageChoice(c)` 挪进 `if (i18n.language !== eff)` 分支 → 本条转红。
   */
  it('有效语言没变但选择变了，选择仍然落盘', async () => {
    sysLangs.value = ['zh-CN'];
    syncLanguageChoice('auto');
    await flush();
    expect(i18n.language).toBe('zh-CN');
    expect(store.get(LANGUAGE_STORAGE_KEY)).toBe('auto');

    expect(syncLanguageChoice('zh-CN')).toBe('zh-CN');
    expect(store.get(LANGUAGE_STORAGE_KEY)).toBe('zh-CN');
  });

  /** 变异对照：把非法码直接当有效码用（去掉 SUPPORTED_LANGUAGES 成员判定）→ 本条转红。 */
  it('不受支持的码回落系统解析，不会把 i18n 顶成未知语言', async () => {
    sysLangs.value = ['en-GB'];
    expect(syncLanguageChoice('klingon')).toBe('en-US');
    await flush();
    expect(i18n.language).toBe('en-US');
  });
});

// ─────────────────────────── 接线守卫（源码结构） ───────────────────────────

const read = (rel: string): string =>
  readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

/**
 * 去注释后的源码。两个方向都必要：本文件与被扫源码的注释都逐字引用了「修复前的坏形态」
 * （如 `persistLanguageChoice 零调用者`），扫原文会被说明文字误伤；反过来，只在注释里提一句
 * `syncLanguageChoice` 就能让 `toContain` 变绿 —— 那是假绿。
 * `[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const APP_RAW = read('../App.tsx');
const APP = code(APP_RAW);
const I18N_RAW = read('./index.ts');
const I18N = code(I18N_RAW);

describe('守卫自检：扫到的确实是源码（防读空文件 / 去注释吃光 → 恒绿）', () => {
  it('两个源文件非空且是目标文件', () => {
    expect(APP_RAW.length).toBeGreaterThan(1000);
    expect(APP).toContain('export default function App');
    expect(I18N).toContain('export function syncLanguageChoice');
  });

  it('去注释后仍有可断言的代码', () => {
    expect(APP.length).toBeGreaterThan(APP_RAW.length / 3);
    expect(I18N.length).toBeGreaterThan(I18N_RAW.length / 3);
    // 这句只在注释里出现，去注释后必须消失（否则 code() 没生效，负向断言全是假绿）
    expect(I18N).not.toContain('切界面语言主窗一个字不变');
  });
});

describe('接线守卫：App.tsx 真的挂了语言水合腿', () => {
  /**
   * 缺陷本体就是这个 effect 整个不存在。
   * 变异对照：删掉 App.tsx 里的 `syncLanguageChoice(languageChoice)` → 本条转红。
   */
  it('订阅 config.language 并调 syncLanguageChoice', () => {
    // 读点本轮从裸 `s.config` 迁到 `useEffectiveConfig`（暂存回显层，见 `store/app-store.ts`）。
    // 门的牙不变：仍钉「App.tsx 真的订阅了 config 的 `language` 这一个字段」，只是换了读它的那层；
    // 退回读裸 `s.config` 会被 `lib/config-read-wiring.test.ts` 的读侧守卫另行抓住（未登记即红）。
    expect(APP).toContain('useEffectiveConfig((c) => c?.language)');
    expect(APP).toContain('syncLanguageChoice(languageChoice)');
    expect(APP).toMatch(/import\s*\{\s*syncLanguageChoice\s*\}\s*from\s*'\.\/i18n'/);
  });

  /**
   * `undefined` 早退是必要的：config 未水合时若照常走一遍，会把兜底的 `'auto'` 写进 localStorage，
   * 冲掉用户上次的具体选择 ⇒ 下次冷启动首屏退回跟随系统（把这个 bug 换了个形态而已）。
   *
   * 变异对照：删掉这行早退 → 本条转红。
   */
  it('config 未水合时早退，不拿兜底值覆盖已持久化的选择', () => {
    expect(APP).toContain('if (languageChoice === undefined) return;');
  });

  /**
   * 依赖数组必须只挂 `languageChoice`：挂 `config` 整体会让任何一次配置写入都重跑一遍
   * （每次都 setItem + 可能的 changeLanguage）。
   * 变异对照：把依赖改成 `[config]` → 本条转红。
   */
  it('effect 依赖只有 languageChoice', () => {
    expect(APP).toMatch(/syncLanguageChoice\(languageChoice\);\s*\}\s*,\s*\[languageChoice\]\)/);
  });
});
