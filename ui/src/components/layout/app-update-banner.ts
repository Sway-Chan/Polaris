/**
 * AppUpdateBanner 纯逻辑 —— 常驻应用更新横幅的可见性判定 + 「本次会话已跳过」会话态。
 *
 * 抽出理由同 `status-bar-display.ts` / `settings-logic.ts`：本仓 vitest 跑在 `environment:'node'`
 * （无 jsdom / testing-library），组件渲染不了；而「跳过的版本不得再提示」这条是**契约要求**
 * （polaris-上游-capability-contract.md:126「常驻 banner + 独立 mini 更新窗」+ `update:skip` 语义），
 * 只能靠纯函数断言锁死。组件**直接消费**本文件导出，不并行复刻判定（复刻 = 测试假绿）。
 *
 * # 为什么前端还要自己记一份 skipped
 *
 * 后端 `update_check`（commands/updater.rs:248）已经把 `update-state.json` 的 `skippedVersion`
 * 喂给 `check_app_update` 过滤 —— **跨会话**那半边后端已经守住了。前端这份只补**本会话**：
 * 用户点「跳过此版本」后，本会话不会再有第二次 check（横幅每会话只查一次），若不本地记一笔，
 * 横幅会在点完跳过后原样杵着，直到下次启动才消失。
 *
 * 它是**会话态**、不是第二真值源：进程退出即丢，下次启动仍以后端持久态为准。
 */

/* ────────────────────────────────────────────────────────────────────────────
 * 会话级「已跳过版本」——横幅与设置页更新卡共享
 * ──────────────────────────────────────────────────────────────────────────── */

const sessionSkipped = new Set<string>();
const listeners = new Set<() => void>();

/**
 * 记一次「用户跳过了该版本」。
 *
 * 两个入口都要调：横幅自己的「跳过此版本」按钮，以及设置页更新卡的同名按钮
 * （`SettingsUpdate.tsx::skipVersion`）—— 否则会出现「在设置页跳过了，横幅还在顶部提示同一个版本」
 * 的自相矛盾，而两处调的其实是同一条后端命令。
 *
 * 空串忽略（后端 `update_skip` 对空 version 直接报错，本地也不该记一个不存在的版本）。
 */
export function markAppVersionSkipped(version: string): void {
  if (!version || sessionSkipped.has(version)) return;
  sessionSkipped.add(version);
  for (const l of listeners) l();
}

/** 本会话已跳过的版本集合（只读视图，供纯判定函数消费）。 */
export function skippedAppVersions(): ReadonlySet<string> {
  return sessionSkipped;
}

/** 订阅「已跳过集合」变化；返回退订闭包（消费方 useEffect cleanup 必须调）。 */
export function subscribeAppVersionSkipped(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/* ────────────────────────────────────────────────────────────────────────────
 * 会话级「已忽略版本」—— 与「已跳过」并列，但语义弱一档
 * ──────────────────────────────────────────────────────────────────────────── */

const sessionDismissed = new Set<string>();

/**
 * 记一次「用户关掉了该版本的横幅」。
 *
 * 此前「忽略」是组件内 `useState` ⇒ **切屏重挂即再现**（`ScreenRouter` 是裸 switch，切屏重挂；
 * 横幅虽在 `AppShell` 层，但同一份 state 一旦随组件卸载就丢）。相邻的「跳过此版本」是持久的，
 * 两颗按钮语义强度差一个数量级而 UI 上毫无提示，用户会以为「忽略」坏了。
 *
 * 与 `markAppVersionSkipped` 的差别**是有意的**：跳过写后端持久态（跨会话不再提示），
 * 忽略只在**本会话**闭嘴（下次启动仍提示）。故落在同一层会话态、不写后端。
 */
export function markAppVersionDismissed(version: string): void {
  if (!version || sessionDismissed.has(version)) return;
  sessionDismissed.add(version);
  for (const l of listeners) l();
}

/** 本会话已忽略的版本集合（只读视图）。 */
export function dismissedAppVersions(): ReadonlySet<string> {
  return sessionDismissed;
}

/** 仅供单测复位模块级会话态（生产无调用点：会话态本就该随进程走完）。 */
export function __resetSkippedForTest(): void {
  sessionSkipped.clear();
  sessionDismissed.clear();
  listeners.clear();
}

/* ────────────────────────────────────────────────────────────────────────────
 * 可见性判定
 * ──────────────────────────────────────────────────────────────────────────── */

/** `update_check` 返回体里横幅关心的子集。 */
export interface AppUpdateSnapshot {
  hasUpdate: boolean;
  version: string | null;
}

export interface AppUpdateBannerState {
  /** 横幅是否渲染。 */
  visible: boolean;
  /** 横幅正文的版本号；不可见时为 null。 */
  version: string | null;
}

/**
 * 横幅可见性。四道闸，任一不过即不渲染：
 *  1. 有快照且 `hasUpdate` 为真（**没查到 / 查失败一律不渲染** —— 失败不是「有更新」，
 *     后台检查失败抢用户注意力毫无价值，与后端 `spawn_auto_check_update` 的「失败只记日志」同取向）；
 *  2. 版本号非空（`hasUpdate:true` 却缺 version = 后端契约破损，宁可不弹也不弹个空版本号，
 *     与 `startup_tasks.rs:207-213` 的同款判定一致）；
 *  3. 该版本本会话未被跳过；
 *  4. 该版本本会话未被用户手动关掉。
 *
 * 第 4 条**按版本**判（不是一个全局布尔）：忽略的语义是「这个版本别再烦我」，
 * 若期间查到了更新的版本，横幅仍该出来。
 */
export function appUpdateBannerState(input: {
  snapshot: AppUpdateSnapshot | null;
  skipped: ReadonlySet<string>;
  dismissed: ReadonlySet<string>;
}): AppUpdateBannerState {
  const { snapshot, skipped, dismissed } = input;
  const version = snapshot?.hasUpdate ? (snapshot.version ?? null) : null;
  if (!version || skipped.has(version) || dismissed.has(version)) {
    return { visible: false, version: null };
  }
  return { visible: true, version };
}

/**
 * 是否允许横幅自查一次更新。
 *
 * 与后端 `startup_tasks.rs::should_auto_check_update` **严格同口径**（`autoCheckUpdate !== false`，
 * 缺省为开）：用户在设置里关掉「启动时检查更新」，就不该再被这条横幅偷偷发一次 GitHub 请求。
 * config 尚未载入（null）→ **不查**，等载入后再判；这样「关掉了却仍发一次请求」结构性不可能发生。
 */
export function shouldBannerCheckUpdate(config: { autoCheckUpdate?: boolean } | null): boolean {
  if (!config) return false;
  return config.autoCheckUpdate !== false;
}
