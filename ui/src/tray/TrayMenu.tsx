/**
 * 托盘自绘浮层 UI（原型 `.tray-menu` L2905-2963 移植）——**真实接线，无假 UI**。
 *
 * 独立窗口（与主窗不共享 JS 堆 → 无法共享 Zustand store），故自持最小本地状态：挂载即 invoke 读
 * config/status/ipInfo，并订阅 `event:proxyStarted`/`event:proxyStopped`/`event:configChanged` +
 * 窗口 focus 保持新鲜。业务动作全走既有 `@/ipc` 的 `api`（连接/断开/切节点/切模式），窗口生命周期
 * （显示主窗/退出/收起/自适应高）走 tray 模块的自定义 command。
 *
 * 两级视图（对齐原型）：main（状态卡 + 连接 + 节点·最近 MRU + 节点入口 + 模式 + 接管方式 + 测速 +
 * 打开主窗 + 打开设置 + 检查更新 + 锁定 + 轻量 + 退出）/ nodes（全部节点切换）。
 * 「打开设置」与「检查更新」（对齐 上游 `TrayManager.ts:421/425`）已补齐 —— 后者旧注释给的阻塞理由
 * （「workspace 无 HTTP/TLS 栈，结构性缺失」）本就已不成立：`runtime/http.rs` 的 `HttpRuntime` 与
 * `update_check` 命令均已接线且在用（更新页与常驻横幅都走这条链）。
 * 测速/锁定/节点·最近（MRU，数据源 `config.recentServerIds`，由 `server_switch` 写入，见
 * `commands/server.rs push_recent_server_id`）均已真实接线。
 */

import { Fragment, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@/ipc/ipc-client';
import { IPC_CHANNELS } from '@/domain/ipc-channels';
import { api } from '@/ipc';
import PolarisStarSprite from '@/components/brand-icons/PolarisStarSprite';
import type {
  UserConfig,
  ServerConfig,
  ProxyErrorCode,
  ProxyMode,
  ProxyModeType,
  SubscriptionConfig,
} from '@/contracts/types';
import {
  groupServersBySubscription,
  defaultOpenGroupIds,
  type ServerGroup,
} from '@/domain/server-grouping';
import {
  BLOCK_SERVER_ID,
  DIRECT_SERVER_ID,
  isBlockSelection,
  isDirectSelection,
} from '@/domain/direct-selection';
import { sortServersByLatency } from '@/domain/server-latency-sort';
// 全量测速的目标集口径：与首页圆钮 / 节点页「全部测速」**同一条过滤线**（domain 单一真值）。
import { speedTestableIds } from '@/domain/endpoint-routes';
// 桌面通知出口（浮层无 toast 层，且托盘操作常在主窗关闭时发生 → 系统通知是唯一送达路径）。
import { notifyDesktop, setDesktopNotificationsEnabled } from '@/lib/desktop-notify';
import { useNodeSortStore } from '@/store/use-node-sort-store';
import { useLatencyStore, subscribeLatencyEvents } from '@/store/use-latency-store';
import { latLevel } from '@/components/screens/shared/format';
// 节点行国旗：与首页出口选单**同一个渲染器 + 同一个数据源**（名称派生 `flagCodeForName`）——
// 那处的语义是「这个节点自称在哪」，托盘节点行是同一件事的另一个视图，故共用而不新写。
// 跨目录只读引用，同上面 `shared/format` 的先例。
import { flagCodeForName } from '@/components/screens/nodes/nd-flag';
import { FlagImg } from '@/components/flag-img';
// FakeIP-TUN 待纠正快照消费（纯函数）：与 HomeScreen.applyIntercept 同源，切到 TUN 时把迁移冻结的
// enableFakeIp:false 一次性回 true（见 fakeip-tun-entry.ts 头注）。跨目录只读引用，同 shared/format 先例。
import { applyFakeIpTunEntry } from '@/components/screens/home/fakeip-tun-entry';
// 降级态（核在跑但流量没经核）与主窗**同一个判定**：跨窗各写一套必然分叉，而分叉的形态就是
// 「主窗琥珀说未生效、托盘绿点说已连接」。取数走 store 的一次性取数出口（浮层不常驻轮询）。
import {
  deriveTakeoverConnState,
  type SystemProxyLive,
} from '@/components/screens/home/connection-state';
import { fetchSystemProxyLive, isSystemProxyLiveApplicable } from '@/store/use-system-proxy-live';
// `deriveTrayConnectButton` 内部收口到主窗同一个 `deriveConnectButtonState`：托盘是独立窗口（不共享
// Zustand store），一旦各写一套判定就必然分叉——原来的 `running ? stop : start` 正是分叉出来的缺陷
// （起核期 running 恒 false ⇒ 点击在已有起核腿之上叠第二个核）。
import {
  deriveTrayConnectButton,
  isTrayServerConfigured,
  normalizeLatency,
  protoShort,
  resolveTrayExitIp,
} from './tray-node-select';
import { applyTrayTheme } from './tray-theme';
import { t, trayLang, refreshTrayLang, type TrayKey } from './labels';
import { trayStatusTone, TRAY_TONE_DOT_CLASS } from './tray-status-tone';
import { revealSiblingGroup, useRevealAfterCommit } from '@/components/reveal';

const MODES: ReadonlyArray<{ v: ProxyMode; k: TrayKey }> = [
  { v: 'smart', k: 'tray.modeSmart' },
  { v: 'global', k: 'tray.modeGlobal' },
  { v: 'direct', k: 'tray.modeDirect' },
];

// 接管方式（systemProxy/TUN/manual）——对齐 上游 TrayManager 的 proxyModeTypeSubmenu（traySystemProxy/
// trayTun/trayLocalOnly）。原型托盘未绘此段，但审查判 HIGH「概念在 Settings 存活仅托盘域未接线」→ 补齐。
// 文案沿用 Polaris 既有措辞（en-US settings.tunMode="TUN Mode" / manualMode="Local Only"）。
const TAKEOVERS: ReadonlyArray<{ v: ProxyModeType; k: TrayKey }> = [
  { v: 'systemProxy', k: 'tray.takeoverSystemProxy' },
  { v: 'tun', k: 'tray.takeoverTun' },
  { v: 'manual', k: 'tray.takeoverManual' },
];

export default function TrayMenu() {
  const [config, setConfig] = useState<UserConfig | null>(null);
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [subscriptions, setSubscriptions] = useState<SubscriptionConfig[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  // 后端 `ProxyStatus.starting`（读时投影）：`running:false` 期间也可能正在起核（重试预算内可达数十秒）。
  // 托盘每次弹出获焦即 hydrate → 打开浮层时拿到的是当次的真值。
  const [starting, setStarting] = useState(false);
  // 后端 `ProxyStatus.errorCode`（与 `event:proxyError` 同源同点落值）：A2 的「错误态可辨」靠它。
  // **读快照而非监听 proxyError 事件**：浮层隐藏期间也会有崩溃发生，事件那会儿没人听；而每次弹出
  // 都会 hydrate ⇒ 读快照恒能拿到当下真值（且清除路径已由 runtime 层保证，托盘不必自己想何时清标记）。
  // 存**码本体**而非布尔：降级判定要区分 `SYSTEM_PROXY_FAILED` 与其它码（见 deriveTakeoverConnState），
  // 折成布尔就把这个信息扔了。
  const [errorCode, setErrorCode] = useState<ProxyErrorCode | undefined>(undefined);
  // 系统代理**活态**（一次性取数，随 hydrate 走 ⇒ 浮层每次弹出拿到的是当次真值）。
  // 不做常驻轮询：浮层是「弹出才可见、几秒就收起」的窗，常驻查询纯烧 exec（理由见 use-system-proxy-live 头注）。
  const [systemProxyLive, setSystemProxyLive] = useState<SystemProxyLive>('unknown');
  const [ip, setIp] = useState('');
  const statusDetailRef = useRef<HTMLDivElement>(null);
  const [statusDetailOverflowing, setStatusDetailOverflowing] = useState(false);
  const [view, setView] = useState<'main' | 'nodes'>('main');
  // 「全部节点」二级视图里当前展开的分组（多组可同时展开，非手风琴互斥）。默认全折叠 ——
  // 浮层窗高按内容自适应，几十上百个节点平铺时整窗会拉成一条贴不下屏的长条；折叠后一屏就是分组目录。
  const [openGroups, setOpenGroups] = useState<ReadonlySet<string>>(new Set());
  // 本窗发起的在飞启停方向。**不是布尔 busy**：布尔只能表达"忙"，而忙的时候恰恰需要区分"正在启动
  // （可取消）"与"正在停止（不可操作）"——旧的 `busy` 一律置灰，等于把取消入口一起关掉了。
  const [pending, setPending] = useState<'start' | 'stop' | null>(null);
  // 测速互斥闸（前端半）：在测则「测速」项置灰 + 文案转「测速中」，杜绝连点触发多条并发（审查 MED
  // 「前后端均无 busy/single-flight」）。后端另有进程级单飞闸（commands/speedtest.rs）兜跨窗口并发。
  const [testing, setTesting] = useState(false);
  // 浮层内的一次性提示行（浮层无 toast 层、无 dialog 层 —— 这是它唯一的可回显表面）。
  // 用于：测速集为空、检查更新的"已是最新/失败"、切 TUN 自动开 FakeIP 的告知。
  const [notice, setNotice] = useState('');
  // 「检查更新」在飞（互斥闸：连点会并发发多次 GitHub 请求）。
  const [checking, setChecking] = useState(false);
  // 浮层语言（A3）：`labels.ts` 的模块状态**可变**，但改了不会自己触发 React 重渲染 →
  // 用一个 state 作渲染依赖，`refreshTrayLang()` 之后 setState 一次，`t()` 在那次渲染里就取到新语言。
  const [lang, setLang] = useState(trayLang);
  // 测速结果（ALL-NODES 视图延迟徽标）：读全局 store。托盘窗与主窗**不共享 JS 堆** ⇒ 这是本窗
  // 独立的 store 实例，靠各自订阅同一后端事件流收敛到同一真值（事件向全部窗口广播）。
  const latencies = useLatencyStore((s) => s.latencyMap);

  // 托盘是独立 JS 堆；即使它通常会在隐藏超时后被整体回收，显示期间也与主窗遵循同一缓存边界。
  useEffect(() => {
    useLatencyStore.getState().retainServerIds(servers.map((server) => server.id));
  }, [servers]);
  // 按延迟排序偏好：持久化 + 同源 localStorage 共享（use-node-sort-store.ts 顶部注释），
  // 托盘直接读本 store —— 无需后端排序 command（纯渲染端视图偏好，同源 webview 天然共享）。
  const sortByLatency = useNodeSortStore((s) => s.sortByLatency);
  const menuRef = useRef<HTMLDivElement>(null);
  // 最近一次已知的 config.uiTheme（hydrate 写）。onFocus / matchMedia 监听是**挂载一次**的闭包，
  // 直接读 config state 会捕获到过期值 → 用 ref 拿最新 uiTheme，主题折算才跟得上配置变更。
  const uiThemeRef = useRef<UserConfig['uiTheme']>(undefined);

  // 弹出/切视图时把焦点落到菜单**容器本身**（`role=menu` + `tabindex=-1`），而非首个按钮（defect#2）。
  // 根因：鼠标点托盘弹出走 `win.set_focus()`（+ 旧代码 querySelector('button').focus()）会给首个可点按钮
  // （设备上是「连接代理」，harness 里因 hydrate 前 Connect 置灰而落到「智能分流」）打上 :focus-visible
  // 焦点环——原生菜单鼠标打开时不高亮任何项。改为聚焦非交互容器：任何按钮都不获焦 → 无焦点环；键盘
  // 方向键仍可用（document keydown 处理器按 activeElement 计算，容器不在 items 中→ArrowDown 落首项，
  // 且那次 .focus() 发生在键盘事件内→WebKit 判定 :focus-visible→焦点环正常显示，a11y 焦点环不丢）。
  const focusMenu = useCallback(() => {
    // preventScroll 必须：容器 `.tray-menu` 顶部留有 margin-top（贴菜单栏的 native 间隙），默认 focus
    // 会把它 scrollIntoView → 把该间隙滚出窗口上沿。
    menuRef.current?.focus({ preventScroll: true });
  }, []);

  const hydrate = useCallback(async () => {
    // 出口 IP 那腿要按连接态分叉取值（见下），而 status 的作用域收在上面那个 try 里 ⇒ 提一个本地变量。
    // 读取失败时保持 false（= 未连接），与该 catch 的「保持空态」一致，绝不乐观假定已连接。
    let connected = false;
    try {
      const [cfg, status] = await Promise.all([
        api.config.get(),
        api.proxy.getStatus(),
      ]);
      connected = status.running;
      setConfig(cfg);
      setServers(cfg.servers ?? []);
      setSubscriptions(cfg.subscriptions ?? []);
      setSelectedId(cfg.selectedServerId);
      setRunning(status.running);
      setStarting(status.starting ?? false);
      setErrorCode(status.errorCode);
      // 活态取一发 —— 只在**适用范围内**（核在跑 + systemProxy）才查，闸门与主窗共用同一个谓词
      // （`isSystemProxyLiveApplicable`），退出适用范围立刻把结论丢回 `unknown`，别让陈旧的
      // `not-effective` 悬着。查询失败折 `unknown` 而非 `not-effective`（读不到 ≠ 没生效）。
      if (isSystemProxyLiveApplicable(status.running, cfg.proxyModeType)) {
        setSystemProxyLive(await fetchSystemProxyLive());
      } else {
        setSystemProxyLive('unknown');
      }
      // A6：把桌面通知总开关同步进**本窗**的 JS 堆。托盘窗与主窗不共享模块实例 ⇒ `App.tsx` 里那次
      // `setDesktopNotificationsEnabled` 只作用于主窗；缺这行，浮层发的通知会无视用户的关闭设置。
      setDesktopNotificationsEnabled(cfg.desktopNotifications);
      // A3：语言可能在主窗被改过（浮层常驻、模块不重载 → 必须显式重解析）。
      setLang(refreshTrayLang());
      // 主题按 config.uiTheme 精确校正（显式浅/深直接定；'system' 跟系统偏好）。记 ref 供
      // 系统主题变化监听 / focus 同步校正复用同一 uiTheme（见下方两处 effect）。
      uiThemeRef.current = cfg.uiTheme;
      applyTrayTheme(cfg.uiTheme);
    } catch {
      /* 读取失败：保持空态（未连接 / 无节点），不崩 */
    }
    try {
      const snap = await api.ipInfo.peek();
      // 出口 IP **不跨连接态回落**（判定收在 `resolveTrayExitIp`，与状态栏同口径 + vitest 直测）。
      setIp(resolveTrayExitIp(connected, snap.proxy?.ip, snap.direct?.ip));
    } catch {
      /* ipInfo 不可用：不显 IP */
    }
  }, []);

  // 挂载：hydrate + 订阅代理生命周期/配置变更 + 窗口 focus（每次浮层弹出即刷新）。
  useEffect(() => {
    void hydrate();
    const offStarted = api.proxy.onStarted(() => {
      setRunning(true);
      void hydrate();
    });
    const offStopped = api.proxy.onStopped(() => {
      setRunning(false);
      setStarting(false); // 停/取消完成 → 起核腿必已退场
      // 顺带回读一次快照：`errored` 的真值在 `ProxyStatus.errorCode` 里，本地推不出来
      // （主动停会清掉它，崩溃则根本不发这个事件）—— 少了这次回读，状态点会停在旧的错误态。
      void hydrate();
    });
    const offConfig = api.config.onChanged(() => void hydrate());
    // 浮层每次弹出（获焦）先**同步**按已知 uiTheme 校正主题：浮层窗常驻，隐藏期间系统可能切了明暗，
    // show 首帧 DOM 还挂着旧 data-theme 会「闪一下旧主题」；同步先校正、再异步 hydrate 拉真值。
    // 顺带把焦点落到第一个可操作项——像原生菜单一样「打开即可方向键导航」。
    const onFocus = () => {
      applyTrayTheme(uiThemeRef.current);
      // A3 兜底腿：`configChanged` 万一没送达（浮层隐藏期间的投递、或语言只改了 localStorage 那一侧），
      // 每次弹出获焦时同步重解析一次 —— 与主题校正同款「同步先校正、再异步 hydrate 拉真值」。
      setLang(refreshTrayLang());
      setNotice(''); // 上次弹出留下的提示不该跨次显示
      // 复位到主视图（原型 `openTray:4009` 的 `trayView(false)`）：托盘窗是**常驻不重建**的，
      // 上次停在「全部节点」二级视图，下次点托盘图标仍停在那里 —— 用户点图标要的是主视图。
      setView('main');
      focusMenu();
      void hydrate();
    };
    window.addEventListener('focus', onFocus);
    return () => {
      offStarted();
      offStopped();
      offConfig();
      window.removeEventListener('focus', onFocus);
    };
  }, [hydrate]);

  // ── 键盘操作（像内置菜单）──
  // ↑↓/Home/End 在可操作项间导航（`button:not([disabled])` 天然跳过分隔线/静态标签/置灰项；「全部节点」
  // 的分组头本身是展开钮 ⇒ **在导航环里**，这是对的：折叠态下它是那一组唯一够得着的落点）；Esc 关闭浮层；
  // Enter/Space 由原生 <button> 直接激活，无需在此处理。挂 document 级监听（浮层是独立窗口/独立 JS 堆，
  // 不与主窗冲突）→ 即便焦点落在非按钮区（如状态卡）也能响应。切视图（main↔nodes）后由下方 effect 重聚焦。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        void invoke(IPC_CHANNELS.TRAY_HIDE).catch(() => {});
        return;
      }
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp' && e.key !== 'Home' && e.key !== 'End') return;
      const menu = menuRef.current;
      if (!menu) return;
      e.preventDefault();
      const items = Array.from(menu.querySelectorAll<HTMLButtonElement>('button:not([disabled])'));
      if (items.length === 0) return;
      const cur = items.findIndex((el) => el === document.activeElement);
      let next: number;
      if (e.key === 'Home') next = 0;
      else if (e.key === 'End') next = items.length - 1;
      else {
        const dir = e.key === 'ArrowDown' ? 1 : -1;
        next = cur < 0 ? (dir > 0 ? 0 : items.length - 1) : (cur + dir + items.length) % items.length;
      }
      items[next]?.focus();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);

  // 切视图（main↔nodes）后焦点落回菜单容器（非按钮）：切视图由**点击**触发，聚焦按钮同样会打上
  // :focus-visible 焦点环（同 defect#2）。容器获焦下方向键仍从首项起步导航（见 focusMenu 注释）。
  useEffect(() => {
    focusMenu();
  }, [view]);

  // 系统主题实时跟随：浮层窗常驻（隐藏也活着），系统明暗切换时 matchMedia 'change' 触发——即便浮层
  // 此刻隐藏也更新 data-theme，故下次弹出时 DOM 已是新主题、从根上消除「点托盘闪旧主题」（onFocus
  // 的同步校正是兜底：万一隐藏态收不到 change 事件，弹出获焦时仍会校正）。仅 uiTheme 为 'system'/未设
  // 时真正跟随（折算在 applyTrayTheme 内部判定）；显式 light/dark 时该回调等价重设同值 = no-op。
  useEffect(() => {
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTrayTheme(uiThemeRef.current);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);

  // ALL-NODES 视图延迟徽标：逐节点流式回填，不等整批。写入口与主窗共用 `use-latency-store`
  // （本窗是**另一个 store 实例**，见该文件「托盘窗是独立 JS 堆」一节——两窗各自收敛到同一后端事件流）。
  // TrayMenu 是托盘窗的根组件、生命周期即窗口生命周期 ⇒ 这里就是本窗的「顶层持久位置」。
  useEffect(() => subscribeLatencyEvents(), []);

  // 自适应高：内容尺寸变化 → 回报浮层窗高（tray 模块设窗高 + 重定位，宽固定）。
  useLayoutEffect(() => {
    const report = () => {
      // WebKit 实证：`document.body.scrollHeight` 漏掉 `.tray-menu` 的顶外边距 + 底外边距，上报比
      // 真实内容矮 → tray_resize 把窗设矮 → 末项「退出 Polaris」被裁。改按卡片下沿（rect.bottom 天然
      // 涵盖顶部外边距）+ 底外边距量高 → 含顶部间隙 + 全部项 + 底部留白。
      const menu = menuRef.current;
      let h = document.body.scrollHeight;
      if (menu) {
        const mb = parseFloat(getComputedStyle(menu).marginBottom || '0');
        h = menu.getBoundingClientRect().bottom + mb;
      }
      void invoke(IPC_CHANNELS.TRAY_RESIZE, { height: Math.ceil(h) }).catch(() => {});
    };
    report();
    const ro = new ResizeObserver(report);
    if (menuRef.current) ro.observe(menuRef.current);
    ro.observe(document.body);
    return () => ro.disconnect();
  }, []);

  const hide = () => void invoke(IPC_CHANNELS.TRAY_HIDE).catch(() => {});

  /* ── 连接钮派生态（与主窗 HomeScreen 共用 `deriveConnectButtonState`）──
   * 已配置含「直连」哨兵（isTrayServerConfigured），否则 direct-only 配置无法从托盘启动
   * （此前只判 servers.length>0，与 Home 的 serverConfigured 口径分裂，见 tray-node-select.ts）。
   *
   * `starting` 有两个来源，缺一不可：
   *  - `pending === 'start'`：**本窗**发起的那一轮（点了连接、start 还没返回）。用户往往就在这个浮层里
   *    等着，取消入口首先要在这里可用。
   *  - 后端 `ProxyStatus.starting`：主窗/自动连接/崩溃自愈发起的那一轮。托盘没有 store 可共享，只能从
   *    状态快照得知；缺了它，从主窗点连接后再打开托盘，看到的仍是"连接代理" ⇒ 点下去叠第二次起核。 */
  const connBtn = deriveTrayConnectButton({
    running,
    backendStarting: starting,
    pending,
    serverConfigured: !!config && isTrayServerConfigured(selectedId, servers.length),
  });

  const toggleConnect = async () => {
    const { action } = connBtn;
    if (action === 'none' || connBtn.disabled) return;
    // **取消腿**：启动中点击 = 打断在飞的那一轮（走 stop 通道），**绝不**再发一次 start。
    // 旧实现按 `running` 分发，而起核期 running 恒 false ⇒ 点击必走 start 分支 ⇒ 在已有起核腿之上
    // 再叠一个核。这不是文案问题，是真会多起一个进程。
    setPending(action === 'start' ? 'start' : 'stop');
    try {
      if (action === 'start') {
        // `config` 只用于「配置加载完了没」这道守门；起核载荷由后端读盘（见 Rust `proxy_start` 头注）
        // —— 浮层这份快照只在它打开时取一次，正是最容易陈旧的那一份。
        if (!config) return;
        await api.proxy.start();
      } else {
        await api.proxy.stop();
      }
      // **不关浮层**（原型 `:4236` `tray-connect` 无 `hidden=true`）：连完通常还要看一眼状态点/延迟，
      // 或接着切个节点。原型只有「选节点」那一条关（`:4237`）。
    } catch {
      /* 失败：保持浮层打开；错误经 proxy:error 事件在主窗呈现 */
    } finally {
      setPending(null);
    }
  };

  // 切模式同样**不关浮层**（原型 `:4240` `tray-mode` 无 `hidden=true`）—— 切完模式常接着切节点，
  // 每点一次就收起会逼用户反复唤出托盘。射程与原型逐条对齐：只有「选节点」关。
  const setMode = async (mode: ProxyMode) => {
    if (mode === config?.proxyMode) return;
    try {
      await api.config.updateMode(mode);
      setConfig((c) => (c ? { ...c, proxyMode: mode } : c));
    } catch {
      /* 忽略：切模式失败不阻断 */
    }
  };

  // 切接管方式（systemProxy/TUN/manual）：与 HomeScreen.applyIntercept 同源——先过 applyFakeIpTunEntry
  // 消费「FakeIP-TUN 待纠正」快照，再 api.config.save 全量落盘。走既有 @/ipc（同 setMode/pickDirect），
  // 不另造后端 tray command：config_save 已在后端保私钥哈希 + 经 broadcast→switch_mode 触发重启。
  // 直接落盘不弹「已连接会重启」确认（浮层无 dialog 层，对齐 上游 托盘 onChangeProxyModeType 的直切语义；
  // 已连接切换会瞬断当前连接，属托盘直切既定取舍）。
  const setTakeover = async (modeType: ProxyModeType) => {
    if (!config || modeType === config.proxyModeType) {
      hide();
      return;
    }
    try {
      // corrected=true（迁移冻结的 enableFakeIp:false 首次进 TUN 被回 true）→ **必须告知**：这是一次
      // 用户没要求过的配置变更，且有实际副作用。A6：桌面通知（对齐 上游 托盘 onChangeProxyModeType
      // 与主窗 `HomeScreen.applyIntercept` 的 notifyDesktop 那条腿）+ 浮层内 notice 行双管——
      // 托盘切换常在主窗关闭时发生，主窗那条 toast 此刻看不到；而浮层这行只在浮层还开着时有用。
      // 开关门控由 hydrate 里的 `setDesktopNotificationsEnabled` 负责（用户关了就静默不发）。
      const { config: next, corrected } = applyFakeIpTunEntry({ ...config, proxyModeType: modeType });
      await api.config.save(next);
      setConfig(next);
      if (corrected) {
        const title = t('tray.takeoverTun');
        const body = t('tray.fakeIpAutoEnabled');
        setNotice(body);
        void notifyDesktop(title, body);
        return; // 保持浮层打开，让 notice 有机会被看到
      }
    } catch {
      /* 忽略：切接管方式失败不阻断；错误经 proxy:error 事件在主窗呈现 */
    }
    hide();
  };

  const switchNode = async (id: string) => {
    try {
      await api.server.switch(id);
      setSelectedId(id);
    } catch {
      /* 忽略：切节点失败不阻断 */
    }
    setView('main');
    hide();
  };

  // 直连哨兵走 saveConfig（全量配置写）而非 server:switch —— 后者的 Rust 实现要求 id 命中真实
  // servers 列表，哨兵不在其中会被拒绝；config-engine 已对该哨兵放行校验（见 HomeScreen.tsx
  // onPickDirectExit 同款注释 + crates/store/validate.rs）。
  const pickDirect = async () => {
    if (config) {
      try {
        const next = { ...config, selectedServerId: DIRECT_SERVER_ID };
        // **单键写**而非整份覆盖：本动作只改 `selectedServerId`，而整份写会把浮层这份快照里
        // **其它所有键**一并按快照回写 —— 浮层的 config 只在它打开时取一次，期间主窗改的任何设置
        // 都会被这次「切直连」静默回滚。`config_set_value` 在后端读盘打补丁，结构上不可能误伤别的键。
        // 入核行为不变：该命令同样走 `broadcast_config_changed` → `switch_mode`，也同样做
        // `invalidate_unlock_on_exit_change`（见 `commands/config.rs::config_set_value`）。
        await api.config.setValue('selectedServerId', DIRECT_SERVER_ID);
        setConfig(next);
        setSelectedId(DIRECT_SERVER_ID);
      } catch {
        /* 忽略：切直连失败不阻断 */
      }
    }
    setView('main');
    hide();
  };

  // 阻断哨兵同 pickDirect：走单键写 `selectedServerId`（server:switch 只收真实节点 id；
  // 不用整份 `config.save`，理由见 pickDirect 处那段——浮层快照会静默回滚主窗的改动）。
  // 直连模式下该项已 disabled，这里二次守门 —— 走到这里说明浮层渲染态与配置态脱节
  // （如浮层关着的期间主窗改了 proxyMode），静默返回胜过写入一个不会生效的出口。
  const pickBlock = async () => {
    if (config && !blockDisabledReason) {
      try {
        const next = { ...config, selectedServerId: BLOCK_SERVER_ID };
        await api.config.setValue('selectedServerId', BLOCK_SERVER_ID);
        setConfig(next);
        setSelectedId(BLOCK_SERVER_ID);
      } catch {
        /* 忽略：切阻断失败不阻断 */
      }
    }
    setView('main');
    hide();
  };

  /* ── 测速：**全量**（A5）──
   *
   * 此前只测「当前选中的那一个节点」，与 上游 托盘 `tray-actions.ts:232` 的全量测速分叉，也与本仓
   * 首页圆钮 / 节点页「全部测速」分叉 —— 同一句「测速」在三处测出三个集合。
   *
   * 集合口径收口到 `speedTestableIds(servers, { mainCorePool: running })`：
   *  - 与 `HomeScreen.onSpeedTest` / `nodes-logic` 逐字节同一条过滤线（domain 单一真值），
   *    排除结构性测不出真值的节点（reverseMesh 走 OS default 会测出直连假好值 / custom endpoint 无
   *    gate 真值 / TS-mesh-only 是公网黑洞）—— 测了只会产生假数值；
   *  - `mainCorePool: running` 是 path-aware 位：TS-exit 仅在主核池可用（=代理在跑）时可测。
   *
   * 空集**不发请求**（上游 `use-speed-test.ts:51-54` 同款）：空跑一轮既浪费也让用户读作"测过了都超时"。
   * 浮层没有 toast，故提示落在 notice 行上。
   *
   * 不关浮层——对齐原型 tray-speedtest（notify 后不隐藏），用户可切到全部节点视图看流式回填。
   * 互斥闸：在测则忽略连点；置 testing 灰态，await invoke（server_speed_test 是 async、resolve=整批测完）
   * 后 finally 复位。 */
  const onSpeedTest = async () => {
    if (testing) return;
    const ids = speedTestableIds(servers, { mainCorePool: running });
    if (ids.length === 0) {
      setNotice(t('tray.noTestableNodes'));
      return;
    }
    setNotice('');
    setTesting(true);
    try {
      await api.server.speedTest(ids);
    } catch {
      /* 忽略：测速失败（含后端单飞闸拒绝）不阻断，仅复位灰态 */
    } finally {
      setTesting(false);
    }
  };

  /* ── 打开设置（A1，对齐 上游 `TrayManager.ts:421` 的 trayOpenSettings）──
   *
   * 设置屏只存在于主窗（浮层里没有也不该有 268px 的设置页）⇒ 这是唯一需要「跨窗令主窗跳屏」的动作。
   * 走**既有** `tray_show_main` 加的受限目标屏参数，不新增 command、不复活已删净的 `EVENT_NAVIGATE`
   * 通用路由（选型理由见 `src-tauri/src/tray.rs::normalize_tray_screen` 上方注释）。 */
  const openSettings = () => void invoke(IPC_CHANNELS.TRAY_SHOW_MAIN, { screen: 'settings' }).catch(() => {});

  /* ── 检查更新（A1，对齐 上游 `TrayManager.ts:425` + `tray-actions.ts:195-220`）──
   *
   * 旧头注称"workspace 无 HTTP/TLS 栈"是结构性缺失 —— 那已不成立：`update_check` 已注册、
   * `crates/updater` 完整、`api.update.check` 早在更新页与常驻横幅上跑着。
   *
   * 整条链（check → hasUpdate → 弹 mini 提醒窗）收在后端 `tray_check_update` 里，**与原生兜底菜单
   * 共用同一个实现**（见 `src-tauri/src/tray.rs`）——两个入口共用一条链，不会出现「菜单查到的和浮层
   * 查到的不一样」；且弹窗要的 `currentVersion` 真值在主进程手里（`app.package_info()`），放前端拼
   * 只会多一条可能与启动期自动检查读出不同值的路径。返回 true = 已弹提醒窗。
   *
   * 无更新 / 失败 → notice 行如实说（上游 在这里弹 dialog；浮层没有 dialog 层，notice 是等价表面）。
   * **绝不把失败显示成"已是最新"**（后端也不会：`update_check` 的失败语义是 `success:false`）。 */
  const checkUpdate = async () => {
    if (checking) return;
    setChecking(true);
    setNotice(t('tray.checkingUpdate'));
    try {
      const hasUpdate = await invoke<boolean>(IPC_CHANNELS.TRAY_CHECK_UPDATE);
      if (hasUpdate) {
        hide(); // 提醒窗已弹出，浮层让位
        return;
      }
      setNotice(t('tray.upToDate'));
    } catch {
      setNotice(t('tray.updateCheckFailed'));
    } finally {
      setChecking(false);
    }
  };

  // 立即锁定：关浮层 + 进隐私态（原型 tray-lock: $('#tray').hidden=true; openLock()）。setPrivacyMode(true)
  // → 后端 emit enterPrivacyMode → 主窗 App 订阅收敛 → LockOverlay 遮罩（主窗即便当前隐藏，renderer 仍活、
  // 订阅在，下次显示即锁）。不强行 tray_show_main：锁定是为隐藏内容，不该反把主窗弹出来。
  const lockNow = async () => {
    hide();
    try {
      await api.config.setPrivacyMode(true);
    } catch {
      /* 忽略：进隐私态失败不阻断 */
    }
  };

  // A2：浮层状态（与原生图标同一条优先级，见 tray-status-tone.ts）+ 降级位。
  // 降级判定**复用主窗那个纯函数**（`deriveTakeoverConnState`），不在浮层重写一套：跨窗各写一套
  // 必然分叉，而分叉的形态正是「主窗琥珀说未生效、托盘绿点说已连接」（2026-07-28 复审抓出）。
  const tone = trayStatusTone({
    running,
    starting,
    errored: !!errorCode,
    degraded:
      deriveTakeoverConnState({
        running,
        proxyModeType: config?.proxyModeType,
        errorCode,
        systemProxyLive,
      }) === 'proxy-degraded',
  });
  const statusLabel =
    tone === 'connected'
      ? t('tray.statusConnected')
      : tone === 'degraded'
        ? t('tray.statusProxyInactive')
        : tone === 'connecting'
          ? t('tray.statusConnecting')
          : tone === 'error'
            ? t('tray.statusError')
            : t('tray.statusDisconnected');
  // `lang` 只作渲染依赖存在（`t()` 读的是 labels 模块状态）。显式引用一次，免得被当成未使用而删掉
  // ——删了它语言切换就不再触发重渲染，回归成"必须重启才跟得上"的老 bug，且 tsc 不会报任何错。
  void lang;

  const directSelected = isDirectSelection(selectedId);
  const blockSelected = isBlockSelection(selectedId);
  const selected = servers.find((s) => s.id === selectedId);
  // 阻断必须在这里如实成名：漏了它状态卡会显「未选择节点」，而实际是用户主动选的有效出口，
  // 那就是谎报（且与首页的「阻断」文案分叉，两处本是同一状态的两个视图）。
  const nodeName = directSelected
    ? t('tray.modeDirect')
    : blockSelected
      ? t('tray.blocked')
      : selected?.name ?? t('tray.noNode');
  const statusDetail = ip ? `${nodeName} · ${ip}` : nodeName;

  // 状态副标题保留默认 ellipsis；仅「确实显示 IP 且确实溢出」时开放悬停跑马灯。
  // 距离从真实 scrollWidth 量，不按 IPv4/IPv6 字符数猜：节点名、字体与 locale 都会改变最终宽度。
  useLayoutEffect(() => {
    const el = statusDetailRef.current;
    if (!el) return;

    const measure = () => {
      const distance = Math.max(0, el.scrollWidth - el.clientWidth);
      const overflowing = !!ip && distance > 1;
      const direction = getComputedStyle(el).direction === 'rtl' ? 1 : -1;
      el.style.setProperty('--tray-status-scroll-distance', `${direction * distance}px`);
      el.style.setProperty(
        '--tray-status-scroll-duration',
        `${Math.max(5, 2 + distance / 28).toFixed(2)}s`,
      );
      setStatusDetailOverflowing((current) =>
        current === overflowing ? current : overflowing,
      );
    };

    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [ip, statusDetail]);
  const currentMode = config?.proxyMode ?? 'smart';
  /** 与首页 blockDisabledReason 同判据：直连模式下阻断不生效 ⇒ 托盘该项也禁用。
   *  文案比首页短：浮层宽 268px，行内提示与「阻断」标签同行，长句会挤掉标签。 */
  const blockDisabledReason =
    currentMode === 'direct' ? t('tray.noEffectInDirect') : null;
  const currentTakeover: ProxyModeType = config?.proxyModeType ?? 'systemProxy';
  // 原 `canConnect` 已并入 `connBtn`（`deriveConnectButtonState` 的 disabled）：可点性与「点了干什么」
  // 必须出自同一次派生，分成两处算就会再次出现「按钮可点但走错分支」这类不一致。

  // 节点分组（自建/组网/各订阅）：groupServersBySubscription 单一真值，与 Home 的 NodeMenu 同一
  // 消费口径（includeEmptyMesh=false，托盘不显空组）。
  const groups = useMemo<ServerGroup[]>(
    () => groupServersBySubscription(servers, subscriptions, false),
    [servers, subscriptions]
  );
  const groupLabel = (g: ServerGroup) =>
    g.isManual ? t('tray.groupManual') : g.isMesh ? t('tray.groupMesh') : g.name;

  /**
   * 进入「全部节点」：**每次进入都重算展开集**（不是挂载时算一次）。托盘窗常驻不重建，
   * 选中节点会在浮层关着的期间被主窗/规则/自动切换改掉 —— 沿用上次的展开集就会展开错组。
   * 判据委托 domain 单一真值（与应用分流策略菜单、规则弹窗目标出站同一条线）。
   */
  const openNodesView = () => {
    setOpenGroups(defaultOpenGroupIds(groups, selectedId));
    setView('nodes');
  };

  const scheduleReveal = useRevealAfterCommit();
  const toggleGroup = (id: string) => {
    const next = new Set(openGroups);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setOpenGroups(next);
  };

  // 最近连接节点（MRU，原型 tray-mru L2917 移植）：当前节点置顶 + 历史（`config.recentServerIds`，
  // server_switch 写入）去重后至多 2 个，共 3 项——对齐原型 `[st.node.name, ...st.mru.filter(x=>x!==
  // st.node.name)].slice(0,3)`。直连哨兵不入选（原型 pickDirectExit 不碰 mru）；历史里指向已删除节点
  // 的 id 解析不出 ServerConfig，随之跳过（同原型 `nodeBy(nm); if(!n) return ''`），故显示条数可能 <3。
  const recentItems = useMemo<ServerConfig[]>(() => {
    // 阻断同直连：不是节点、不入 MRU（否则 `selectedId` 会以一个查不到 ServerConfig 的 id 占位）。
    if (directSelected || blockSelected) return [];
    const recentIds = config?.recentServerIds ?? [];
    const ids = [selectedId, ...recentIds.filter((id) => id !== selectedId)].slice(0, 3);
    return ids
      .filter((id): id is string => !!id)
      .map((id) => servers.find((s) => s.id === id))
      .filter((s): s is ServerConfig => !!s);
  }, [config?.recentServerIds, directSelected, blockSelected, selectedId, servers]);

  const latText = (v: number | null | undefined): string => {
    if (v === null) return t('tray.timeout');
    if (v === undefined) return '';
    return `${v} ms`;
  };

  return (
    <>
      <PolarisStarSprite />
      <div className="tray-menu" ref={menuRef} role="menu" tabIndex={-1}>
        {/* 状态卡：连接态 + 当前节点 + 出口 IP。
            A2：四态（连接 / 连接中 / 异常 / 未连接）与**原生托盘图标同一条优先级**折算
            （`trayStatusTone` ↔ Rust `resolve_tray_state`）。此前只有 ok/idle 二态，起核中与崩溃
            都显示成"未连接"——图标与浮层各说各话。 */}
        <div className={`tray-status${running ? '' : ' stopped'}`}>
          <span className="ts-mk">
            <svg viewBox="-46 -46 92 92">
              <use href="#polarisStar" />
            </svg>
          </span>
          <div className="ts-tx">
            <b>{statusLabel}</b>
            <div
              ref={statusDetailRef}
              className={`tray-status-detail${statusDetailOverflowing ? ' is-overflowing' : ''}`}
              aria-label={statusDetail}
              tabIndex={statusDetailOverflowing ? 0 : undefined}
            >
              <span className="tray-status-detail-track">{statusDetail}</span>
            </div>
          </div>
          <span className={`dot ${TRAY_TONE_DOT_CLASS[tone]}`} />
        </div>

        {/* 一次性提示行：浮层无 toast / 无 dialog 层，这是它唯一的可回显表面（测速空集 /
            检查更新结果 / FakeIP 自动启用告知）。空串即不渲染，不占高度。 */}
        {notice && <div className="tray-note">{notice}</div>}

        {view === 'main' ? (
          <div className="tray-view">
            <button
              className="tray-i accent"
              onClick={() => void toggleConnect()}
              disabled={connBtn.disabled}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M12 3v9" strokeLinecap="round" />
                <path d="M6.5 6.8a7 7 0 108.9 0" strokeLinecap="round" />
              </svg>
              <span>
                {/* 文案跟 action 走（不是跟 running 走）：启动中必须显「取消启动」，否则用户读作
                    "还没连上，再点一下"——正是叠第二次 start 的诱因。 */}
                {connBtn.action === 'cancel'
                  ? t('tray.cancelStartup')
                  : connBtn.action === 'stop'
                    ? t('tray.disconnect')
                    : t('tray.connect')}
              </span>
            </button>

            {/* **这一段不再被 `servers.length > 0` 门住**：出口选择里有两个「非节点出口」（直连 / 阻断），
                零节点时它们仍然是合法且唯一可用的出口。此前整段被门掉 ⇒ 零节点用户从托盘**够不到**
                「全部节点」入口 ⇒ 够不到直连行，而 `isTrayServerConfigured(DIRECT_SERVER_ID, 0)` 早已
                判 true（连接按钮可点）—— 于是「能连但选不了出口」。阻断会继承同一缺陷，故一并解除。
                recentItems 在零节点时自然为空数组，组头文案随之退化成「节点」，无需额外分支。 */}
            <div className="tray-sep" />
            <div className="tray-group-h">
              {recentItems.length > 0 ? t('tray.nodesRecent') : t('tray.nodes')}
            </div>
            {recentItems.map((s) => {
              const on = selectedId === s.id;
              const lat = normalizeLatency(latencies[s.id]);
              return (
                <button
                  key={`recent-${s.id}`}
                  className={`tray-i${on ? ' on' : ''}`}
                  onClick={() => void switchNode(s.id)}
                >
                  {/* 延迟色点（原型 `:3785`/`:3789` 的 `.tray-node-dot`）：一眼扫色比读数字快，
                      且行右侧被勾选标记占用时（当前节点）数字根本不渲染，色点是那一行唯一的延迟信号。 */}
                  <span className={`tray-node-dot ${latLevel(lat)}`} aria-hidden />
                  <FlagImg code={flagCodeForName(s.name)} />
                  <span className="tray-node-name">{s.name}</span>
                  <span className="tray-proto">{protoShort(s.protocol)}</span>
                  {on ? (
                    <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                      <path d="M5 12l5 5 9-11" />
                    </svg>
                  ) : (
                    <span className={`tray-lat ${latLevel(lat)}`}>{latText(lat)}</span>
                  )}
                </button>
              );
            })}
            <button className="tray-i" onClick={openNodesView}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="3" y="4" width="18" height="7" rx="1.5" />
                <rect x="3" y="13" width="18" height="7" rx="1.5" />
              </svg>
              <span>{t('tray.allNodes')}</span>
              <svg className="tray-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M9 6l6 6-6 6" />
              </svg>
            </button>

            <div className="tray-sep" />
            <div className="tray-group-h">{t('tray.groupMode')}</div>
            {MODES.map((m) => (
              <button
                key={m.v}
                className="tray-i"
                onClick={() => void setMode(m.v)}
              >
                {m.v === 'smart' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <circle cx="12" cy="12" r="9" />
                    <path d="M3 12h18" />
                  </svg>
                )}
                {m.v === 'global' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <path d="M4 4h16v16H4z" />
                  </svg>
                )}
                {m.v === 'direct' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <path d="M4 12h16" />
                  </svg>
                )}
                <span>{t(m.k)}</span>
                {currentMode === m.v && (
                  <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                    <path d="M5 12l5 5 9-11" />
                  </svg>
                )}
              </button>
            ))}

            {/* 接管方式（systemProxy/TUN/manual）——托盘内直切（审查 HIGH，对齐 上游 proxyModeTypeSubmenu）。
                切换走 setTakeover（applyFakeIpTunEntry + config.save，与 HomeScreen.applyIntercept 同源）。 */}
            <div className="tray-sep" />
            <div className="tray-group-h">{t('tray.groupTakeover')}</div>
            {TAKEOVERS.map((tk) => (
              <button key={tk.v} className="tray-i" onClick={() => void setTakeover(tk.v)}>
                {tk.v === 'systemProxy' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <rect x="3" y="4" width="18" height="12" rx="2" />
                    <path d="M8 20h8M12 16v4" strokeLinecap="round" />
                  </svg>
                )}
                {tk.v === 'tun' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <path d="M12 3l7 3v6c0 4-3 7-7 8-4-1-7-4-7-8V6z" strokeLinejoin="round" />
                  </svg>
                )}
                {tk.v === 'manual' && (
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                    <path d="M5 6h14M5 12h9M5 18h14" strokeLinecap="round" />
                    <circle cx="17" cy="12" r="2" />
                  </svg>
                )}
                <span>{t(tk.k)}</span>
                {currentTakeover === tk.v && (
                  <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                    <path d="M5 12l5 5 9-11" />
                  </svg>
                )}
              </button>
            ))}

            <div className="tray-sep" />
            {/* 测速（原型 tray-speedtest L2925，闪电图标）：测选中节点，结果流式回填 nodes 视图延迟徽标。
                在测则置灰 + 文案转「测速中」（互斥闸前端半）。 */}
            <button className="tray-i" onClick={() => void onSpeedTest()} disabled={testing}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M13 2L4 14h6l-1 8 9-12h-6z" />
              </svg>
              <span>{testing ? t('tray.testing') : t('tray.speedtest')}</span>
            </button>
            <button className="tray-i" onClick={() => void invoke(IPC_CHANNELS.TRAY_SHOW_MAIN).catch(() => {})}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="3" y="4" width="18" height="16" rx="2" />
                <path d="M3 9h18" />
              </svg>
              <span>{t('tray.openMain')}</span>
            </button>
            {/* 打开设置（A1，上游 TrayManager.ts:421 的 trayOpenSettings）：显示主窗 + 跳设置屏。 */}
            <button className="tray-i" onClick={openSettings}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <circle cx="12" cy="12" r="3.2" />
                <path d="M19.4 15a1.6 1.6 0 00.3 1.8l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.6 1.6 0 00-1.8-.3 1.6 1.6 0 00-1 1.5V21a2 2 0 11-4 0v-.1A1.6 1.6 0 008 19.4a1.6 1.6 0 00-1.8.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.6 1.6 0 00.3-1.8 1.6 1.6 0 00-1.5-1H3a2 2 0 110-4h.1A1.6 1.6 0 004.6 8a1.6 1.6 0 00-.3-1.8l-.1-.1a2 2 0 112.8-2.8l.1.1a1.6 1.6 0 001.8.3H9a1.6 1.6 0 001-1.5V3a2 2 0 114 0v.1a1.6 1.6 0 001 1.5 1.6 1.6 0 001.8-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.6 1.6 0 00-.3 1.8V9a1.6 1.6 0 001.5 1H21a2 2 0 110 4h-.1a1.6 1.6 0 00-1.5 1z" />
              </svg>
              <span>{t('tray.openSettings')}</span>
            </button>
            {/* 检查更新（A1，上游 TrayManager.ts:425 的 trayCheckUpdates）：有更新弹 mini 提醒窗，
                无更新/失败落 notice 行。在飞则置灰（连点会并发发多次 GitHub 请求）。 */}
            <button className="tray-i" onClick={() => void checkUpdate()} disabled={checking}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M12 20a8 8 0 10-7.6-5.5" strokeLinecap="round" />
                <path d="M4 9V4.5M4 9h4.5" strokeLinecap="round" strokeLinejoin="round" />
                <path d="M12 8v4.5l3 1.8" strokeLinecap="round" />
              </svg>
              <span>
                {checking ? t('tray.checking') : t('tray.checkUpdate')}
              </span>
            </button>
            {/* 立即锁定（原型 tray-lock L2927，锁图标）：进隐私态，主窗遮罩靠 enterPrivacyMode 事件收敛。 */}
            <button className="tray-i" onClick={() => void lockNow()}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="5" y="11" width="14" height="9" rx="2" />
                <path d="M8 11V8a4 4 0 018 0v3" />
              </svg>
              <span>{t('tray.lockNow')}</span>
            </button>
            {/* C16 进入轻量模式（原型 tray L2942）：**销毁主窗 webview 释放内存，保托盘+核活**。
                当前与主窗关闭按钮共用同一后端腿；本项保留为“主窗仍开着时立即释放”的显式入口。
                tray_enter_lightweight 在销毁前置 LightweightState 保核 + clear_window 释放 stats 订阅账。
                唤出：macOS/Windows 托盘左键、Linux 原生菜单或 dock → show_main_window 重建主窗。 */}
            <button
              className="tray-i"
              onClick={() => void invoke(IPC_CHANNELS.TRAY_ENTER_LIGHTWEIGHT).catch(() => {})}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7" />
              </svg>
              <span>{t('tray.lightweight')}</span>
            </button>
            <div className="tray-sep" />
            <button className="tray-i danger" onClick={() => void invoke(IPC_CHANNELS.TRAY_QUIT).catch(() => {})}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M9 21H5a2 2 0 01-2-2V5a2 2 0 012-2h4M16 17l5-5-5-5M21 12H9" />
              </svg>
              <span>{t('tray.quit')}</span>
            </button>
          </div>
        ) : (
          <div className="tray-view">
            <button className="tray-back" onClick={() => setView('main')}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M15 6l-6 6 6 6" />
              </svg>
              <span>{t('tray.allNodes')}</span>
            </button>

            {/* 直连置顶（DIRECT_SERVER_ID 哨兵，与 Home NodeMenu 的 onPickDirect 同款「快捷直连」——
                此前托盘不识别该哨兵：直连选中时显「未选择节点」，且 direct-only 配置无法从托盘启动，
                见 tray-node-select.ts isTrayServerConfigured 的注释）。 */}
            {/* `on` 类：选中态视觉与节点行统一（flow-weak 填充 + flow-hi 文字）。此前托盘**压根没有
                这个类** —— 选中的那一行与旁边任何一行长得一模一样，唯一区别是行尾延迟数字被换成勾
                （而未测速的行本就没有数字可换 ⇒ 选中态可能完全不可见）。取值见 styles/index.css。 */}
            <button
              className={`tray-i${directSelected ? ' on' : ''}`}
              onClick={() => void pickDirect()}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 12h16" />
              </svg>
              <span>{t('tray.modeDirect')}</span>
              {directSelected && (
                <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                  <path d="M5 12l5 5 9-11" />
                </svg>
              )}
            </button>
            {/* 阻断紧随直连，与首页 NodeMenu 逐项对齐（同一状态的两个视图，选项集合必须一致）。
                图标沿用应用分流「阻断」的斜杠语汇。

                禁用原因走**行内可见文本**而不是 `data-tip`：`initTooltips` 只在 App.tsx（主窗）挂载，
                托盘是独立 webview、压根没有 tooltip 引擎 ⇒ `data-tip` 在这里是死属性（而原生 `title=`
                被 G10 门禁止，见 lib/tooltip-wiring.test.ts）。行内文本反而恒可见，不依赖悬浮。 */}
            <button
              // `act-block-txt` = 动作标签轴常驻红。**不能**改钩 `.tray-i.danger`：上面那颗「退出」
              // 也是 `tray-i danger`，那样会把退出一并涂成常驻红（不在裁定范围内）。
              className={`tray-i danger act-block-txt${blockSelected ? ' on' : ''}${blockDisabledReason ? ' disabled' : ''}`}
              onClick={blockDisabledReason ? undefined : () => void pickBlock()}
              disabled={!!blockDisabledReason}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M5 5l14 14" />
              </svg>
              <span>{t('tray.blocked')}</span>
              {blockDisabledReason && <span className="tray-hint">{blockDisabledReason}</span>}
              {blockSelected && (
                <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                  <path d="M5 12l5 5 9-11" />
                </svg>
              )}
            </button>
            <div className="tray-sep" />

            {/* 按分组列出（自建/组网/各订阅），组内可选按延迟排序（use-node-sort-store 持久偏好，
                托盘/首页出口下拉/Nodes 工具栏三处共读，原型 :4475 "persisted + tray-synced"）。
                组头是**展开钮**：默认全折叠，只有含当前出口节点的那组进来时是展开的（openNodesView）。 */}
            {groups.map((g) => {
              const items = sortByLatency
                ? sortServersByLatency(g.servers, (id) => latencies[id])
                : g.servers;
              const groupOpen = openGroups.has(g.id);
              return (
                <Fragment key={g.id}>
                  <button
                    className={`tray-group-h tray-grp-t${groupOpen ? ' open' : ''}`}
                    aria-expanded={groupOpen}
                    onClick={(e) => {
                      const header = e.currentTarget;
                      toggleGroup(g.id);
                      scheduleReveal(groupOpen ? null : () => revealSiblingGroup(header));
                    }}
                  >
                    <svg
                      className="tray-grp-chev"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                    >
                      <path d="M9 6l6 6-6 6" />
                    </svg>
                    <span>{groupLabel(g)}</span>
                    <span className="tray-grp-c">{items.length}</span>
                  </button>
                  {groupOpen &&
                    items.map((s) => {
                      const on = !directSelected && selectedId === s.id;
                      const lat = normalizeLatency(latencies[s.id]);
                      return (
                        <button
                          key={s.id}
                          className={`tray-i${on ? ' on' : ''}`}
                          onClick={() => void switchNode(s.id)}
                        >
                          <span className={`tray-node-dot ${latLevel(lat)}`} aria-hidden />
                          {/* 国旗：与首页出口选单同一渲染器 + 同一数据源（名称派生），三处节点行
                              前置图标从此同一套（见 styles/index.css「三处节点选择器的视觉统一」轴 4）。 */}
                          <FlagImg code={flagCodeForName(s.name)} />
                          <span className="tray-node-name">{s.name}</span>
                          {/* 协议短标签（WG/TS/SS/Hy2，审查 LOW，对齐 上游 PROTOCOL_SHORT）：夹在名与延迟间。 */}
                          <span className="tray-proto">{protoShort(s.protocol)}</span>
                          {on ? (
                            <svg className="tk" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4">
                              <path d="M5 12l5 5 9-11" />
                            </svg>
                          ) : (
                            <span className={`tray-lat ${latLevel(lat)}`}>{latText(lat)}</span>
                          )}
                        </button>
                      );
                    })}
                </Fragment>
              );
            })}
            <div className="tray-sep" />
            <button className="tray-i" onClick={() => void invoke(IPC_CHANNELS.TRAY_SHOW_MAIN).catch(() => {})}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 6h16M4 12h16M4 18h10" />
              </svg>
              <span>{t('tray.manageInMain')}</span>
            </button>
          </div>
        )}
      </div>
    </>
  );
}
