/**
 * AppShell —— 980×740 窗口容器 + flex(shell) 布局（原型 .win / .shell L104-236）。
 *
 * 原型结构：
 *   .win（925×740，圆角，overflow:hidden，border，shadow）
 *     .winctl（Win/Linux 浮动控制，绝对定位右上；mac 用原生交通灯）
 *     .shell（flex:1，flex 行）
 *       .side（侧栏 148px / 折叠 56-80px）
 *       .main（flex:1 列：.main-scroll 滚动区 + .statusbar 状态栏）
 *
 * 视觉族全部走 components.css 语义类（.win/.shell/.main/.main-scroll），per-os 差异
 * （圆角/透明/vibrancy 分区）由 `:root[data-os]` 选择器驱动，不再堆 Tailwind 工具类。
 *
 * OS 检测：写 <html data-os>（mac/win/lin），驱动交通灯槽 / 窗口控制 / 侧栏宽 / 圆角 / vibrancy。
 *   权威值来自 tauri-plugin-os 的 platform()（WKWebView UA 不可靠，真机会识别错）；异步取，
 *   取到前用 navigator UA 嗅探作默认（真机 mac 的 UA 含 "Macintosh" → 默认即正确，不闪烁），
 *   plugin 值返回后覆盖修正边缘情况。
 * 主题检测：写 <html data-theme>（dark/light）—— 真值 config.uiTheme；config 未水合时回落主进程
 *   首帧种子 `window.__POLARIS_INITIAL_THEME__`（折算见 ./theme-state.ts，那里解释了为什么不能回落 'system'）。
 * 内容区按 nav-store mainScreen 渲染对应屏幕（ScreenRouter）。
 */

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useNavStore } from '@/store/nav-store';
import { useEffectiveConfig } from '@/store/app-store';
import Sidebar from './Sidebar';
import SettingsSidebar from '../screens/settings/SettingsSidebar';
import StatusBar from './StatusBar';
import { WinCtl } from './TitleBar';
import { ScreenRouter } from '../screens/ScreenRouter';
import { DialogHost } from '../dialogs/DialogHost';
import { Toaster } from './Toaster';
import LockOverlay from './LockOverlay';
import AppUpdateBanner from './AppUpdateBanner';
import PendingChangesBar from './PendingChangesBar';
import PolarisStarSprite from '../brand-icons/PolarisStarSprite';
import { resolveWindowEffectsState, type WindowEffectsState } from './window-effects';
import { resolveTheme, readInitialThemeSeed } from './theme-state';

/** 平台标识（mac/win/lin），决定窗口 chrome 样式。 */
export type Platform = 'mac' | 'win' | 'lin';

/** navigator UA 嗅探默认（同步，取权威 plugin 值前的兜底；真机 mac UA 含 "Macintosh" → 正确）。 */
function detectPlatform(): Platform {
  if (typeof navigator === 'undefined') return 'lin';
  const ua = navigator.userAgent.toLowerCase();
  if (ua.includes('mac')) return 'mac';
  if (ua.includes('win')) return 'win';
  return 'lin';
}

/** tauri-plugin-os platform() 值（"macos"/"windows"/"linux"/…）→ 内部 Platform。 */
function mapOsPlatform(p: string): Platform {
  if (p === 'macos') return 'mac';
  if (p === 'windows') return 'win';
  return 'lin';
}

export interface AppShellProps {
  /** 可选：覆盖内容区（默认走 ScreenRouter）。测试或阶段接入用。 */
  children?: ReactNode;
}

export default function AppShell({ children }: AppShellProps) {
  const [os, setOs] = useState<Platform>(() => detectPlatform());
  const scope = useNavStore((s) => s.scope);
  const mainScreen = useNavStore((s) => s.mainScreen);
  const settingsScreen = useNavStore((s) => s.settingsScreen);
  const mainScrollRef = useRef<HTMLDivElement>(null);
  /** 首帧不抢焦点（原型 `route()` 里的 `st.booted` 门）：启动时把焦点摁到 h1 上会盖掉别的初始焦点。 */
  const routedOnce = useRef(false);

  // 权威平台标识：tauri-plugin-os 的 platform() 命令（Rust 端已注册 + capability os:default 已授权）。
  // 经 @tauri-apps/api/core invoke 直连插件命令，无需额外 JS 包装包；非 Tauri（浏览器）下 invoke
  // 失败即保留 UA 嗅探默认。
  useEffect(() => {
    invoke<string>('plugin:os|platform')
      .then((p) => setOs(mapOsPlatform(p)))
      .catch(() => {
        /* 非 Tauri：保留 UA 嗅探默认 */
      });
  }, []);

  // 同步 <html data-os>（驱动交通灯槽 / 窗口控制 / 侧栏宽 / 圆角 / vibrancy 分区）。原型同一属性。
  useEffect(() => {
    document.documentElement.setAttribute('data-os', os);
  }, [os]);

  // 同步 <html data-window-effects>（'on'/'off'/缺失）——驱动 index.css 里 mac 的「让位给原生 vibrancy」
  // 规则。判定口径与理由见 ./window-effects.ts；两个否决位取 store 里既有的 config 字段，无新增 IPC。
  // 分三个标量选择器取值：返回对象字面量会每次新引用 → Object.is 判不等 → 无限重渲染。
  // ⚠️ 判定必须是 `!== null`：store 把 config 声明为 `UserConfig | null` 并初始化为 `null`
  // （app-store.ts:84 / :169），**从不是 `undefined`** ⇒ 写成 `!== undefined` 会恒为 true，
  // 这个「已载入」闸门就是空的。叠加下方的一次性锁存后果是致命的：首帧拿到两个 `undefined` 字段
  // → resolve 回落「默认开」→ 'on' 被永久钉死；而 `windowEffects:false` 的机器上 Rust 建的是
  // **不透明 #0B0F14** 窗，CSS 却按 'on' 把上面每层都剥透明 ⇒ 浅色主题深底配浅色前景，冷启动 100% 复现。
  // （2026-07-21 独立复审抓出；此前写的是 `!== undefined`，守卫测试只 grep 锁存语句的存在性，验不出判定为空。）
  const configLoaded = useEffectiveConfig((c) => c !== null);
  const windowEffects = useEffectiveConfig((c) => c?.windowEffects);
  const hardwareAcceleration = useEffectiveConfig((c) => c?.hardwareAcceleration);
  // **一次性快照，之后不再跟随 config 变化** —— 本属性描述的是「**这扇窗当初被建成什么样**」，
  // 不是「配置现在写着什么」。`transparent` 是 `WebviewWindowBuilder` 参数、**运行期不可改**
  // （src-tauri/src/main.rs:440-458），故建窗那一刻的取值在本窗生命周期内恒定；而 config 会被设置页
  // 实时改写，两者一旦分叉，CSS 就会按错误的窗口形态渲染。
  //
  // 具体故障（跟随式实现的真 bug，非假设）：启动时特效关 ⇒ 窗口建成**不透明 + 实色 #0B0F14**；
  // 用户在设置里打开特效 ⇒ 若属性跟着翻成 'on' ⇒ CSS 让位 ⇒ 前端透明层露出那块**深色**实底 ⇒
  // **浅色主题下深底配深字，直接不可读**，且要到重启才恢复。反向（启动开→设置关）同样错：
  // 窗口仍是透明的，前端却停止让位……那一支恰好安全（自绘不透明底盖住透明窗），但靠的是巧合不是设计。
  //
  // 快照语义也与用户可见行为一致：改这两个开关本就需要重启才生效，UI 侧的重启提示即此约束的外化。
  const builtWindowEffects = useRef<WindowEffectsState | null>(null);
  useEffect(() => {
    if (builtWindowEffects.current !== null) return; // 已快照 ⇒ 本窗生命周期内不再变
    // config 未载入时**传 undefined**（而不是先 return）——让 `resolveWindowEffectsState` 的
    // `'unknown'` 腿在生产里真的走到。此前是「未载入即 return」，那条腿只有单测走得到 = 死代码，
    // 而 index.css 的「属性缺失即不让位」兜底正依赖它（2026-07-21 独立复审抓出）。
    const state = resolveWindowEffectsState(
      configLoaded ? { windowEffects, hardwareAcceleration } : undefined,
    );
    // 'unknown' ⇒ 既不锁存也不写属性：CSS 侧「属性缺失即不让位」兜底成现有不透明外观，
    // 等 config 到位后本 effect 再跑一次才定值。**不可在此锁存**，否则又把首帧的猜测钉死。
    if (state === 'unknown') return;
    builtWindowEffects.current = state;
    document.documentElement.setAttribute('data-window-effects', state);
  }, [configLoaded, windowEffects, hardwareAcceleration]);

  // 主题：接 config.uiTheme（'light'/'dark'/'system'）。'system' 跟随 prefers-color-scheme；显式
  // light/dark 直接落 data-theme（Settings「显示」里选浅色即刻生效——此前只按系统嗅探，选浅色无效）。
  //
  // ⚠️ **config 未水合时必须回落主进程种子**（`window.__POLARIS_INITIAL_THEME__`），不是 `'system'`。
  // 折算全部收在 `resolveTheme` 里（纯函数 + 单测）；写成 `config?.uiTheme ?? 'system'` 的那一版
  // 会在 config 到达前把 `theme_boot_script` 播下的正确值覆写成「跟随系统」⇒ uiTheme=dark + OS 浅色
  // 的用户冷启动看到「首帧深 → 挂载闪浅 → config 到达转回深」，FOUC 只是被挪了位置。
  // （2026-07-28 独立复审抓出；详见 ./theme-state.ts 模块头。）
  const uiTheme = useEffectiveConfig((c) => c?.uiTheme);
  useEffect(() => {
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => {
      document.documentElement.setAttribute(
        'data-theme',
        resolveTheme({
          // 已水合但字段缺省 → 'system'（config schema 的默认档）；**未水合才是 undefined**。
          uiTheme: configLoaded ? uiTheme ?? 'system' : undefined,
          seed: readInitialThemeSeed(),
          systemDark: mq.matches,
        }),
      );
    };
    apply();
    const handler = () => apply();
    mq.addEventListener('change', handler);
    return () => mq.removeEventListener('change', handler);
  }, [configLoaded, uiTheme]);

  /**
   * 切屏的两条固有副作用（原型 `route()`，`proto:3072-3073`）—— `nav-store.navigate` 是纯 state、
   * 零副作用，这两条在本仓一直缺席：
   *
   *  1. **主滚动区置顶**：长列表（连接/日志/节点）切走再切回会停在旧滚动位，看起来像「跳到了半截」；
   *  2. **焦点转移到 h1**：屏幕阅读器不会宣告换屏，键盘焦点还留在侧栏刚点的那一项上。
   *     `tabindex="-1"` 是让 h1 可编程聚焦而不进 Tab 序；`preventScroll` 防止聚焦本身又把刚置顶的
   *     滚动位拉走（两条副作用会互相打架，原型也是这么写的）。
   */
  useEffect(() => {
    mainScrollRef.current?.scrollTo({ top: 0 });
    if (!routedOnce.current) {
      routedOnce.current = true;
      return;
    }
    const h1 = mainScrollRef.current?.querySelector<HTMLElement>('h1');
    if (!h1) return;
    h1.setAttribute('tabindex', '-1');
    h1.focus({ preventScroll: true });
    // WKWebView 会给这次**程序化**聚焦判 `:focus-visible`（Chromium 不会）⇒ 首次换屏时主标题上
    // 冒出一圈蓝框（陈先生 2026-07-29 真机报；「后续不复现」是因为焦点已停在 h1，再聚焦同一元素
    // 不重新触发）。焦点转移本身要保留 —— 它是读屏宣告换屏的唯一通路，去掉是 a11y 净倒退；
    // 故只摘视觉环，见 `styles/index.css` 的 `.scr h1[tabindex='-1']:focus-visible`。
  }, [scope, mainScreen, settingsScreen]);

  return (
    <div className="stage">
      <PolarisStarSprite />
      {/* .win：原型 925×740 卡片；真实窗口锁 980×740，覆盖层让卡片填满窗口、无 stage 露边。 */}
      <div className="win">
        {os !== 'mac' && <WinCtl />}

        <div className="shell">
          {scope === 'settings' ? <SettingsSidebar /> : <Sidebar />}

          <main className="main">
            {/* chrome 头：窗口拖动带（镜像 .side-chrome）。拖动靠 `data-tauri-drag-region` HTML 属性——
                Tauri v2 注入 drag.js 监听 mousedown，命中该属性即 invoke('plugin:window|start_dragging')
                （tauri-2.11.5/src/window/scripts/drag.js:11,78-105）。`-webkit-app-region` 是 Electron 约定，
                Tauri 全栈（tauri/wry/tao）零处理、mac WKWebView 也不实现 → 曾经那版是死 CSS，故拖不动。
                本元素保持空：裸属性语义是「只有直接点中本元素才拖」（drag.js:66 el===composedPath[0]）。 */}
            <div className="main-chrome" data-tauri-drag-region />
            {/* 常驻应用更新横幅（契约 §About「常驻 banner + 独立 mini 更新窗」）。刻意放在
                `.main-scroll` **之外**：放里面会随内容滚走，"常驻"就名不副实。无更新 / 已跳过 /
                已关闭时组件 return null，零 DOM、零高度，对既有布局无影响。 */}
            <AppUpdateBanner />
            <div ref={mainScrollRef} className="main-scroll">{children ?? <ScreenRouter />}</div>
            {/* 待应用差集条：docked 于状态栏正上方（原型 L2447「a MAIN child ⇒ structurally cannot span
                the sidebar」）。与 `AppUpdateBanner` 分居 `main` flex 列两端 —— 横幅在顶、本条在底，
                中间 `.main-scroll` 吸收剩余高度，两者互不挤压。差集为空时组件 return null，零 DOM 零高度。
                位置是语义的一部分：条一旦回到 `.main-scroll` **内**就会随内容滚走，"常驻"名不副实。 */}
            <PendingChangesBar />
            <StatusBar />
          </main>
        </div>

        <DialogHost />
        {/* toast 栈默认挂 .win 内（原型 notify() 把 #toast-stack append 进 winEl()）。
            **有弹窗打开时它会 portal 到最顶层 `<dialog>` 的子树里** —— 那是 top-layer，不进去就被
            `::backdrop` 压住且 z-index 无效（根因与双引擎实测见 `../dialogs/dialog-top-layer.ts`）。
            故本处只是「无弹窗时的宿主」，不是唯一挂载点；定位一律 fixed 相对视口，两种宿主下落点一致。 */}
        <Toaster />
        {/* 隐私锁遮罩（#lock-overlay 铺满 .win，privacyMode===true 时渲染；否则 return null 零成本）。
            挂在最后 → DOM 序在 chrome 之上；z-index:100 盖住 shell/侧栏/状态栏/toast。 */}
        <LockOverlay />
      </div>
    </div>
  );
}
