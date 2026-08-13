/**
 * 设置页第二份 config 副本的门 —— 「`onChanged` 静默重拉不得抹掉暂存值」（本轮硬性不变式 4）。
 *
 * # 这里在防什么
 *
 * `useConfig` 自持一份 `UserConfig`（不经 app-store），`configApi.onChanged` 到达时**整份重拉覆盖**。
 * 后台写盘者很多（订阅调度器写 `subscriptions[].etag`、规则资源调度器写 `updatedAt`、托盘切模式、
 * 后端自愈），任何一次都会广播。若暂存值写在那份 state 里，一次回声就把用户刚拨的开关弹回原位，
 * 而暂存条上还记着一条 —— 与「节点列表不回显」完全同型的静默回退。
 *
 * 修法：state 只装**磁盘副本**，对外交出 `effectiveConfigOf(磁盘副本, 条目)`。判断收口在 `useConfig`
 * 一处，**不下放到 9 个设置子页**。
 *
 * # 两半各自证明什么（分开说，不混为一谈）
 *
 *  - **行为半**（下面第一个 describe）：把「重拉覆盖」这个序列在纯函数上跑一遍 —— 换一份新的磁盘
 *    副本之后，暂存键还在、磁盘侧的新值也进来了。这是真跑出来的，不是结构断言。
 *  - **接线半**（第二个 describe）：`useConfig` 是 hook，vitest 是 `environment: 'node'`（无 jsdom，
 *    有意为之）⇒ **渲染不了，本文件测不到真实的 `setConfig` 时序**。故接线用源码断言钉住三件
 *    结构事实：磁盘副本保持纯净、对外那份经派生、落盘入参不含暂存键。
 *    源码断言证明不了行为 —— 「按下开关后界面上到底显示什么」仍需真机/DOM 验证，已在交接里挂账。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import type { UserConfig } from '@/contracts/types';
import type { StagedEntry } from '@/lib/staged-config';
import { effectiveConfigOf } from '@/store/app-store';

/** 用户打开设置页那一刻磁盘上的那份。 */
const DISK_1 = {
  mixedPort: 7890,
  desktopNotifications: true,
  subscriptions: [{ id: 's1', name: '主订阅', etag: 'aaa' }],
} as unknown as UserConfig;

/** 后台订阅调度器刷了一次 etag 之后的磁盘那份（`onChanged` 回声带回来的就是它）。 */
const DISK_2 = {
  mixedPort: 7890,
  desktopNotifications: true,
  subscriptions: [{ id: 's1', name: '主订阅', etag: 'bbb' }],
} as unknown as UserConfig;

/** 用户在设置页改了 `mixedPort`（Class B ⇒ 进暂存，`useConfig` 里 `entityPath: [key]` 的形态）。 */
const stagedPort: StagedEntry = {
  id: 'setting:mixedPort',
  kind: 'setting',
  label: '修改设置 · mixedPort',
  entityPath: ['mixedPort'],
  nextValue: 1080,
};

describe('行为：onChanged 整份重拉之后，暂存值仍在界面上', () => {
  it('重拉前：设置页显示的是暂存值', () => {
    expect(effectiveConfigOf(DISK_1, [stagedPort])!.mixedPort).toBe(1080);
  });

  it('重拉后：暂存值还在，且磁盘侧的新值也进来了', () => {
    const shown = effectiveConfigOf(DISK_2, [stagedPort])!;
    // 变异对照（已实跑）：把 useConfig 改回「对外直接交出 state 那份」，等价于这里读 DISK_2 本身，
    // 本断言红（7890）—— 那正是「开关弹回原位、暂存条上还记着一条」的现场。
    expect(shown.mixedPort).toBe(1080);
    // 正向对照：不是靠「不更新」蒙对的 —— 磁盘侧真变了的字段必须跟着变。
    expect(shown.subscriptions?.[0].etag).toBe('bbb');
  });

  it('无暂存时逐字节不变：交出的**就是**重拉回来的那份本体', () => {
    // 总开关关着 ⇒ 条目恒空 ⇒ 这条就是设置页今天的全部行为。`toBe` 不可换成 `toEqual`：
    // 等值副本照样过，而副本会让 9 个子页每次回声全量重渲染。
    expect(effectiveConfigOf(DISK_2, [])).toBe(DISK_2);
  });

  it('撤销那条暂存后，设置页退回磁盘值（条目订阅而非快照读的理由）', () => {
    expect(effectiveConfigOf(DISK_2, [])!.mixedPort).toBe(7890);
  });
});

describe('接线：三件结构事实（源码断言 —— 证明的是接线，不是行为）', () => {
  const SRC = readFileSync(fileURLToPath(new URL('./useConfig.ts', import.meta.url)), 'utf8')
    // 去注释保留行号：本文件的注释逐字引用了被禁的旧写法，扫原文会被说明文字误伤。
    .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
    .replace(/(^|[^:])\/\/.*$/gm, (m, p1: string) => p1 + ' '.repeat(m.length - p1.length));

  it('对外那份经 `effectiveConfigOf` 派生（判断只在这一处，不下放子页）', () => {
    expect(SRC).toMatch(/return\s*\{\s*config:\s*effectiveConfigOf\(/);
  });

  it('磁盘副本保持纯净：`load` 的回填不经任何重放', () => {
    // `load` 里若也 replay 一次，暂存值就进了磁盘副本 ⇒ 下一次 `configApi.save` 会把它写进 config.json
    //（FR-1「零磁盘写」当场破）。故这条腿必须只写 `cfg` 本身。
    const load = SRC.slice(SRC.indexOf('const load = useCallback'), SRC.indexOf('const reload ='));
    expect(load.length).toBeGreaterThan(200); // 切片没切空（否则下面两条恒真）
    expect(load).toMatch(/setConfig\(cfg\)/);
    expect(load).not.toMatch(/effectiveConfigOf|\breplay\b/);
  });

  it('落盘入参与乐观写都取 `persisted`（= 磁盘副本 + direct，不含暂存键）', () => {
    expect(SRC).toMatch(/const persisted = \{ \.\.\.prev, \.\.\.direct \}/);
    expect(SRC).toMatch(/configApi\.save\(persisted\)/);
    expect(SRC).toMatch(/setConfig\(persisted\)/);
    // 旧写法 `const next = { ...prev, ...patch }` + `setConfig(next)`：把暂存键并进了磁盘副本，
    // 第二次改一个直落盘键时会把它们一起写盘。该变量已删，复活即红。
    expect(SRC).not.toMatch(/\.\.\.prev, \.\.\.patch/);
  });
});
