/**
 * U-7「需重启 App 才生效」键集合 + 变更判定的单测。
 *
 * 每条都带**变异对照**（改坏哪一行会让哪条转红）—— 无变异对照的测试视为无信息量。
 */
import { describe, it, expect } from 'vitest';
import {
  APP_RESTART_REQUIRED_KEYS,
  appRestartRequiredChanges,
  appRestartRequiredDiff,
  restartKeysStillPending,
} from './app-restart-keys';
import type { UserConfig } from '@/contracts/types';

const cfg = (o: Partial<UserConfig>): Partial<UserConfig> => o;

describe('APP_RESTART_REQUIRED_KEYS 成员', () => {
  /**
   * 集合被**逐字钉死**。这不是复述实现，而是「改集合必须同时改测试 + 回去核实 Rust 消费点」的闸门：
   * 判据（键的消费点在 webview/进程建立之前，且描述本次运行）不可由 TS 侧自证，只能人工回核，
   * 故这里锁字面量强制那次回核发生。
   *
   * 变异对照：往集合里塞一个运行期就能生效的键（如 uiTheme）→ 本条转红。
   */
  it('恰好是三个进程启动期才读的键', () => {
    expect([...APP_RESTART_REQUIRED_KEYS]).toEqual([
      'hardwareAcceleration',
      'windowEffects',
      'rememberWindowSize',
    ]);
  });

  /**
   * 反向边界：这几个键同样在启动期被读过，但**有运行期消费者**或**语义本就是「下次启动」**，
   * 进集合就是纯噪音（`silentStart` 更糟：为它重启会让应用把自己藏起来）。
   *
   * 变异对照：把 silentStart / uiTheme / logLevel / language / minimizeToTray 任一加进集合 → 转红。
   *
   * ## `language` 的来历（它曾经不在本条里，别再把它拿出去）
   *
   * 早先复审查实渲染端**根本没有** `config.language` 的消费者：`persistLanguageChoice` 零调用者、
   * 全 `ui/src` 无 `i18n.changeLanguage`，`SettingsDisplay` 写完 `config.language` 就没有下文
   * ⇒ 切界面语言主窗一个字不变、**重启也不变**（只有 Rust 托盘跟着变，`tray.rs:90` 直读 config）。
   * 当时把 `language` 从本条**移除**并只留指针，理由是：水合腿缺失期间，断言「它有运行期消费者所以
   * 不该进集合」是用测试固化一个错误结论。
   *
   * **2026-07-29 水合腿已补**（`i18n/syncLanguageChoice` + `App.tsx` 订阅 `config.language` 的 effect，
   * 行为门见 `i18n/language-hydration.test.ts`）⇒ 前提成立，指针兑现成断言：`language` 确有运行期
   * 消费者，改它当场生效，**不需要**重启 App，故不得进本集合。
   */
  it('不含「只描述下次启动」或有运行期消费者的键', () => {
    for (const k of [
      'silentStart',
      'autoStart',
      'autoConnect',
      'uiTheme',
      'logLevel',
      'minimizeToTray',
      'language',
    ]) {
      expect(APP_RESTART_REQUIRED_KEYS as readonly string[]).not.toContain(k);
    }
  });
});

describe('appRestartRequiredChanges —— 只在值真的变了时命中', () => {
  /** 变异对照：把实现里的 `key in patch` 去掉 → 每次保存都返回全部三键 → 本条转红。 */
  it('patch 没碰这些键 → 空', () => {
    expect(
      appRestartRequiredChanges(cfg({ hardwareAcceleration: true }), cfg({ uiTheme: 'dark' })),
    ).toEqual([]);
  });

  /** 变异对照：把 `!==` 判等去掉（只要 patch 里有该键就命中）→ 本条转红（受控组件回声会天天弹窗）。 */
  it('同值重复保存 → 空', () => {
    expect(
      appRestartRequiredChanges(
        cfg({ hardwareAcceleration: false, windowEffects: true }),
        cfg({ hardwareAcceleration: false, windowEffects: true }),
      ),
    ).toEqual([]);
  });

  /** 变异对照：把归一函数 `v !== false` 换成 `v === true` / `Boolean(v)` → 本条转红。 */
  it('undefined 与 true 是同一个后端判定值（缺省为开）→ 互换不命中', () => {
    expect(appRestartRequiredChanges(cfg({}), cfg({ hardwareAcceleration: true }))).toEqual([]);
    expect(appRestartRequiredChanges(cfg({ windowEffects: true }), cfg({ windowEffects: undefined }))).toEqual([]);
    expect(appRestartRequiredChanges(cfg({}), cfg({ rememberWindowSize: true }))).toEqual([]);
  });

  /** 变异对照：归一成 `v === false` 之类的反向语义 → 本条转红。 */
  it('缺省(开) → 显式 false 命中；显式 false → 缺省(开) 也命中', () => {
    expect(appRestartRequiredChanges(cfg({}), cfg({ windowEffects: false }))).toEqual([
      'windowEffects',
    ]);
    expect(
      appRestartRequiredChanges(cfg({ rememberWindowSize: false }), cfg({ rememberWindowSize: undefined })),
    ).toEqual(['rememberWindowSize']);
  });

  /** 变异对照：把 filter 改成 find/some（只返首个）→ 本条转红。 */
  it('一次 patch 改多个 → 全部命中，顺序恒为集合声明序', () => {
    expect(
      appRestartRequiredChanges(
        cfg({ hardwareAcceleration: true, windowEffects: true, rememberWindowSize: true }),
        // 故意用与声明相反的书写顺序，断言输出顺序不随 patch 的键序漂移。
        cfg({ rememberWindowSize: false, windowEffects: false, hardwareAcceleration: false }),
      ),
    ).toEqual(['hardwareAcceleration', 'windowEffects', 'rememberWindowSize']);
  });

  /** 变异对照：去掉 `if (!prev) return []` → config 尚未水合时首次写会误弹窗 → 本条转红。 */
  it('prev 未水合（null/undefined）→ 空，不误弹', () => {
    expect(appRestartRequiredChanges(null, cfg({ windowEffects: false }))).toEqual([]);
    expect(appRestartRequiredChanges(undefined, cfg({ windowEffects: false }))).toEqual([]);
  });

  /** 空 patch 是 no-op 写（部分调用点会传空对象），不得命中。 */
  it('空 patch → 空', () => {
    expect(appRestartRequiredChanges(cfg({ windowEffects: false }), cfg({}))).toEqual([]);
  });
});

/**
 * 整份 config 比对（备份导入 / 托盘 / 后端自愈这类**不经设置页**的变更）。
 *
 * 与 `appRestartRequiredChanges` 的唯一差别是没有 `key in` 守卫，而那正是本组存在的理由：
 * 整份 config 里键缺席 = 取默认值，不是「本次没碰」。
 */
describe('appRestartRequiredDiff：整份 config 比对', () => {
  /**
   * **本组的核心用例**，也是修 U-7 备份导入绕过时的回归钉子。
   * 旧备份根本没有这三个键 ⇒ `generalSettings` 整类替换后键消失 ⇒ 值实际从「关」变回「开」。
   *
   * 变异对照：把 `appRestartRequiredDiff` 的实现换成 `appRestartRequiredChanges`（即加回
   * `key in next` 守卫）→ 本条转红（导入抹掉键的场景重新变成静默）。
   */
  it('显式 false 被抹回缺省（键消失）→ 命中', () => {
    expect(
      appRestartRequiredDiff(
        cfg({ hardwareAcceleration: false, windowEffects: false }),
        cfg({}),
      ),
    ).toEqual(['hardwareAcceleration', 'windowEffects']);
  });

  /** 反向：缺省被导入成显式 false，同样是真变更。 */
  it('缺省被导成显式 false → 命中', () => {
    expect(appRestartRequiredDiff(cfg({}), cfg({ rememberWindowSize: false }))).toEqual([
      'rememberWindowSize',
    ]);
  });

  /**
   * `undefined` 与 `true` 归一后是同一个判定结果（Rust 侧「仅显式 false 才关」），
   * 二者互换什么也没改变，不得弹窗 —— 否则导入任何一份备份都会平白弹一次。
   *
   * 变异对照：把 `effectiveValue` 改成 `v === true` → 本条转红。
   */
  it('缺省 ↔ 显式 true 互换 → 不命中（归一后同值）', () => {
    expect(appRestartRequiredDiff(cfg({}), cfg({ hardwareAcceleration: true }))).toEqual([]);
    expect(appRestartRequiredDiff(cfg({ windowEffects: true }), cfg({}))).toEqual([]);
  });

  /** 导入的备份没碰这三个键 → 静默，不打扰用户。 */
  it('两份完全同值 → 空', () => {
    const same = cfg({ hardwareAcceleration: false, windowEffects: true });
    expect(appRestartRequiredDiff(same, { ...same })).toEqual([]);
  });

  /** 首次回填（prev 为空）不判：否则一进设置页就按默认值比一次，必弹。 */
  it('prev 为 null/undefined → 空', () => {
    expect(appRestartRequiredDiff(null, cfg({ hardwareAcceleration: false }))).toEqual([]);
    expect(appRestartRequiredDiff(undefined, cfg({ windowEffects: false }))).toEqual([]);
  });

  /** 非成员键的变化不得混进来（导入备份必然改动一大票无关键）。 */
  it('只报集合成员，无关键的变化不命中', () => {
    expect(
      appRestartRequiredDiff(cfg({ uiTheme: 'dark' } as Partial<UserConfig>), cfg({
        uiTheme: 'light',
      } as Partial<UserConfig>)),
    ).toEqual([]);
  });
});

/**
 * 判据基线 = **本次进程启动值**，不是上一次保存值（复审 M1）。
 *
 * 这一组守的是「重启到底会不会改变什么」这个问句本身。上面两组只回答「值变了没有」，
 * 单独用它们会在「改走又改回」时误报一次重启 —— 而重启会断代理。
 */
describe('restartKeysStillPending：只报「重启真的会改变什么」的键', () => {
  /**
   * **M1 的回归钉子**。进程以 hardwareAcceleration=true 启动 → 用户关掉（第一次弹窗，点「稍后」）
   * → 又打开：值确实变了（false→true），但磁盘值已回到启动值，重启什么都不会发生。
   *
   * 变异对照：把 `restartKeysStillPending` 的 filter 去掉（直接返回 changed）→ 本条转红。
   */
  it('改走又改回启动值 → 不再提示', () => {
    const startup = cfg({ hardwareAcceleration: true });
    // 第一步：关掉 —— 与启动值不同，应提示。
    expect(
      restartKeysStillPending(['hardwareAcceleration'], startup, cfg({ hardwareAcceleration: false })),
    ).toEqual(['hardwareAcceleration']);
    // 第二步：改回来 —— 值又变了一次，但已等于启动值 ⇒ 不提示。
    expect(
      restartKeysStillPending(['hardwareAcceleration'], startup, cfg({ hardwareAcceleration: true })),
    ).toEqual([]);
  });

  /**
   * 两层必须是**交集**：光看「≠ 启动值」会在每次无关保存的回声里反复弹
   * （差异一直存在，直到用户真的重启）。没碰过的键不得因为「仍与启动值不同」被带出来。
   *
   * 变异对照：把实现改成 `APP_RESTART_REQUIRED_KEYS.filter(...)`（无视 changed）→ 本条转红。
   */
  it('本次没碰的键即便仍与启动值不同也不报', () => {
    expect(
      restartKeysStillPending(
        [], // 本次改的是别的字段
        cfg({ hardwareAcceleration: true }),
        cfg({ hardwareAcceleration: false }), // 上一轮关掉后一直没重启，差异仍在
      ),
    ).toEqual([]);
  });

  /** 归一同口径：缺省 ↔ 显式 true 与启动值比也不算差异。 */
  it('缺省与显式 true 视为与启动值相同', () => {
    expect(restartKeysStillPending(['windowEffects'], cfg({ windowEffects: true }), cfg({}))).toEqual(
      [],
    );
  });

  /**
   * 拿不到启动值（IPC 失败 / 旧后端）→ 退回只看「值变了」的旧行为。
   * 宁可多提示一次，也不静默——静默正是 U-7 要修的病。
   *
   * 变异对照：把 `if (!startup) return [...changed]` 改成 `return []` → 本条转红（回归静默）。
   */
  it('启动值缺失 → 原样返回，不静默', () => {
    expect(
      restartKeysStillPending(['windowEffects'], null, cfg({ windowEffects: false })),
    ).toEqual(['windowEffects']);
    expect(
      restartKeysStillPending(['windowEffects'], undefined, cfg({ windowEffects: false })),
    ).toEqual(['windowEffects']);
  });

  /** 多键场景：只留下真的与启动值不同的那些，顺序不乱。 */
  it('多键混合 → 只留仍与启动值不同的', () => {
    const startup = cfg({ hardwareAcceleration: true, windowEffects: true, rememberWindowSize: true });
    expect(
      restartKeysStillPending(
        ['hardwareAcceleration', 'windowEffects', 'rememberWindowSize'],
        startup,
        cfg({ hardwareAcceleration: false, windowEffects: true, rememberWindowSize: false }),
      ),
    ).toEqual(['hardwareAcceleration', 'rememberWindowSize']);
  });
});
