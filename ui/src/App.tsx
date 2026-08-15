/**
 * Polaris App 根组件。
 *
 * 渲染 AppShell（980×740 窗口 + 侧栏 + 状态栏 + screen 路由），UI 真值 = 原型。
 *
 * # 全局事件订阅层（主窗唯一一处）
 *
 * 后端事件 → app-store 的收敛点。**必须挂在这里而不是各屏内部**：各屏是条件渲染（`ScreenRouter`
 * 按 `mainScreen` switch），组件卸载后订阅即失效，而连接态/配置变更在用户不在该屏时同样会发生
 * （典型来源：托盘浮层——它是独立窗口，不共享本 store，只能靠后端事件把两个窗口收敛到同一真值）。
 *
 * 只订阅**后端确实会 emit** 的事件（`commands/proxy.rs` / `commands/config.rs` 的 emit 点、
 * `runtime/proxy.rs::set_error`）：订阅一个从不 emit 的通道 = 假接线，比不订更糟（看着像做了）。
 * 加订阅前先确认发射点存在——本仓 33 个事件常量里曾有 16 个是「定义了没人发也没人听」的死通道。
 */

import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { invoke } from '@tauri-apps/api/core';
import { IPC_CHANNELS } from './domain/ipc-channels';
import { useAppStore, useEffectiveConfig } from './store/app-store';
import type { UnlockDisplayState } from './store/app-store';
import { subscribeLatencyEvents, useLatencyStore } from './store/use-latency-store';
import { subscribeSubscriptionProgressEvents } from './store/use-subscription-progress-store';
import { subscribeSpeedTestProgressToast } from './lib/speedtest-progress-toast';
import { useSystemProxyLivePolling } from './store/use-system-proxy-live';
import { api } from './ipc';
import { unlockApi } from './ipc/api-client';
import { toast } from './lib/error-handler';
import { useIdlePrivacyLock } from './lib/use-idle-privacy-lock';
import { initTooltips } from './lib/tooltip-engine';
import { notifyDesktop, setDesktopNotificationsEnabled } from './lib/desktop-notify';
import { syncLanguageChoice } from './i18n';
import { isProxyStartClaimed } from './lib/proxy-start-claim';
import { proxyErrorText } from './domain/proxy-error-text';
import { isDefinitiveTsLoginFrame } from './domain/tailscale-conn-state';
import AppShell from './components/layout/AppShell';
import { createOnceGate } from './components/screens/settings/settings-logic';

/**
 * #17 内核基线警告的「每会话一次」闸门。**必须是模块级单例**——挂在组件内（useRef/state）会随
 * App 重挂（轻量模式返回 / window 重建）复位，等于没去重。同 上游的模块级
 * `let coreBaselineWarnedThisSession = false`。
 */
const coreBaselineWarnGate = createOnceGate();

/**
 * R2 待应用差集 pull 兜底：拉当下差集写 store（覆盖 PUSH 盖不住的清差集/冷启/订阅刷新——对齐 上游的
 * started/stopped/挂载/订阅自动更新触发点，见 vault 的节点变更重启设计 §5.3）。
 *
 * 后端返回体已收口为 `{added, modified, removed}`（pull 与 push 同一个结构，无适配层）。
 * `?? []` 保留：核未运行/IPC 降级时可能拿到畸形对象，缺字段按空集降级，绝不抛。
 * 失败静默（差集是锦上添花，不该因拉取失败弹错）。
 */
function pullPendingChanges(): void {
  void api.proxy
    .getPendingChanges()
    .then((p) =>
      useAppStore.getState().setPendingChanges({
        added: p.added ?? [],
        modified: p.modified ?? [],
        removed: p.removed ?? [],
        // `?? false` 与三个 `?? []` 同理：旧版/降级载荷缺键时按「没有欠账」降级，绝不让条恒亮。
        restartDeferred: p.restartDeferred ?? false,
      })
    )
    .catch(() => {});
}

/**
 * `event:proxyError` 分腿路由——从 onError 订阅体抽出为具名函数，供 `.test.ts` 直接断言副作用调用
 * （toast/notifyDesktop/refreshProxyStatus 是否被真调），不必整棵渲染 App。
 *
 * # 文案一律经 `proxyErrorText`，**不得**直接用 `data.message`
 *
 * 载荷里的 `message` 是 Rust 侧写死的中文串。此前六处都写成 `data.message || t(key)`，而
 * `emit_proxy_error(message, error_code)` 两参皆非可选 ⇒ `||` 右边永远短路不到，那些 key 是死键、
 * 俄语/波斯语用户在最高频的错误路径上看到中文。改为按 `errorCode` 优先取键（三段式回落规则与
 * 「为什么不翻译 Rust message」见 `domain/proxy-error-text.ts` 头注）。**本函数每条腿的分类判据
 * 仍是 `errorCode`，文案不再参与分腿** —— 两者混在一句 `||` 里正是上面那个缺陷的形状。
 *
 * - **崩溃腿**（PROCESS_EXITED / AUTO_RESTART_FAILED）：核不再运行 → 刷连接态 + 「已断开」toast +
 *   桌面通知（C8，对齐 上游 index.ts:1900 notifyUser(proxyError*)：崩溃常发生在窗口已收进托盘/隐藏时，
 *   应用内 toast 看不到，系统通知是唯一送达路径；desktopNotifications 关/无权限则 notifyDesktop 内部静默不发）。
 * - **出口误导腿**（SYSTEM_PROXY_FAILED / EXIT_MISMATCH）：核仍在运行，只是流量未按预期经代理 ——
 *   **不**刷连接态、**不**用「已断开」文案（都会把活核错误地标成已停）。报 `home.proxyMisdirected`
 *   警告 toast；同样发桌面通知——窗口常被收进托盘，用户可能正以为在用代理、实则明文直连，
 *   应用内 toast 送不到。
 * - **能力降级腿**（RULE_RESOURCES_MISSING）：核仍在运行，只是引用缺失 `.srs` 的分流规则整段被跳过
 *   → 智能分流失效。后端剪枝是 fail-closed：剪枝把 `final` 压成 `direct` 时会回退成用户出口
 *   （`builder/route.rs` T2 fail-safe），即**未命中规则的流量兜底走代理，不会因剪枝静默转直连**。
 *   但这**不等于「全量经代理」**，两个例外仍在：① 不依赖 rule_set 的显式 direct 规则不受剪枝影响
 *   （bypass-LAN 的 ip_cidr、ICMP 兜底、DoH 泄漏拦截、系统进程放行等）；② 组网出口回退时
 *   （`mesh_selected_exit_falls_back_to_direct` 为真）用户出口本身就是 `direct`，fail-safe 回退后
 *   `final` 仍是 `direct`。与出口误导腿同形（**不**刷连接态、warning toast + 桌面通知——同样常发生在
 *   窗口已收进托盘时），但文案指向「规则资源」页下载而非直连风险：两者的用户下一步动作完全不同，
 *   共用一条文案等于把可操作指引冲掉。
 * - **root 孤儿阻断腿**（ROOT_ORPHAN_BLOCKED）：残留的 root 孤儿核用户态杀不动、独占 `cache.db`，
 *   任何模式都起不来 → 核**未起**（终态），故必须 `refreshProxyStatus`。与 HELPER 两码同为后端
 *   **双出口**，同样纳入认领闸门去重（此前**完全不路由**，托盘/自动连接等无人 await 的入口静默丢弃，
 *   有人 await 时又落到 HomeScreen 的「检查服务器配置」通用兜底——对残留进程这码是错的指引）。
 *   文案**不经** `proxyErrorText` 第 1 段：本码在门的豁免表里判断过后决定不配 i18n 键
 *   （message 带着这次残留进程的 pid，用户唯一能据以行动的信息，键只能给同义反复），故恒走第 2 段
 *   （后端原文）。toast.error + 桌面通知（窗口常已收进托盘）。
 * - 其余码（如 STARTUP_FAILED）：忽略。它必然伴随某次 proxy.start 的 reject，发起方（Home 连接按钮）
 *   自己会 toast，此处重报 = 同一次失败弹两遍。
 */
export function handleProxyErrorEvent(
  data: { errorCode?: string; message?: string },
  deps: { t: (key: string, fallback?: string) => string; refreshProxyStatus: () => Promise<void> }
): void {
  const { t, refreshProxyStatus } = deps;
  if (data.errorCode === 'PROCESS_EXITED' || data.errorCode === 'AUTO_RESTART_FAILED') {
    void refreshProxyStatus();
    toast.error(proxyErrorText(data, t));
    void notifyDesktop(t('notify.proxyError.title'), t('notify.proxyError.body'));
    return;
  }
  if (data.errorCode === 'SYSTEM_PROXY_FAILED' || data.errorCode === 'EXIT_MISMATCH') {
    toast.warning(proxyErrorText(data, t));
    void notifyDesktop(t('notify.proxyMisdirected.title'), t('notify.proxyMisdirected.body'));
    return;
  }
  if (data.errorCode === 'RULE_RESOURCES_MISSING') {
    toast.warning(proxyErrorText(data, t));
    void notifyDesktop(
      t('notify.ruleResourcesMissing.title'),
      t('notify.ruleResourcesMissing.body')
    );
    return;
  }
  // TUN 出口未夺到（其他 VPN 占默认路由）：核已被硬闸 `kill_core`、**未起**（终态），故必须
  // `refreshProxyStatus` —— 否则 UI 停在假「已连接」。同 HELPER 两码是后端**双出口**（既 emit 事件、
  // 又让 start reject），故认领期内让位给 await 腿（Home 连接按钮），无人认领（托盘/自动连接）时照常报。
  if (data.errorCode === 'TUN_ROUTE_NOT_CAPTURED') {
    void refreshProxyStatus();
    if (isProxyStartClaimed()) return;
    toast.error(proxyErrorText(data, t));
    void notifyDesktop(
      t('notify.tunRouteNotCaptured.title'),
      t('notify.tunRouteNotCaptured.body')
    );
    return;
  }
  // TUN 网卡从未建出来（#327，Windows）：核已被逐腿硬闸 `kill_core`、**未起**（终态），处置形态与上一支
  // 完全相同（刷连接态 + 认领去重 + 报）。**文案与通知刻意分开**：上一支叫用户「断开其他 VPN」，本支
  // 叫用户「查 wintun 驱动是否被安全软件拦截」——共用一条会把两边的下一步动作都指错。
  if (data.errorCode === 'TUN_ADAPTER_MISSING') {
    void refreshProxyStatus();
    if (isProxyStartClaimed()) return;
    toast.error(proxyErrorText(data, t));
    void notifyDesktop(t('notify.tunAdapterMissing.title'), t('notify.tunAdapterMissing.body'));
    return;
  }
  // TUN 提权门两码：核**未起**（终态），故必须 `refreshProxyStatus` —— 与崩溃腿同理，
  // 不刷则 UI 停在假「已连接」。这两条腿的发起方常常**没人在 await**：托盘切档位 / 启动自动连接 /
  // switchMode 去抖重启都不经 Home 的连接按钮，此前后端已发码、前端整条 else 落空 = 静默丢弃
  // （真机反馈「点了没反应」的直接成因）。
  if (data.errorCode === 'HELPER_GATE_ABORTED' || data.errorCode === 'HELPER_NOT_INSTALLED') {
    // 刷连接态**在认领判定之外**：核未起是终态，与「谁负责提示」无关，两条路径都得刷。
    // 挪进下面的 return 之后 = 认领期内 UI 停在假「已连接」（await 腿并不刷）。
    void refreshProxyStatus();

    // 这两码是后端**双出口**（既 emit 事件、又让 `api.proxy.start` reject），故须与 await 腿去重 ——
    // 同本文件 §STARTUP_FAILED 写明的既有约定「两处都报 = 同一次失败弹两遍」。但不能照搬那条的
    // 「整条忽略」：忽略会把上面说的「没人 await 的入口」重新变成静默。故由发起方显式认领
    // （Home 连接按钮把 startProxy 包进 `withProxyStartClaim`）：认领期内让位给 await 腿，
    // 无人认领（托盘/自动连接）时照常报。认领期为何带宽限尾巴见 proxy-start-claim.ts 头注。
    if (isProxyStartClaimed()) return;

    // **两码分开报，不合并**（同 §RULE_RESOURCES_MISSING 的分家理由）：
    // - GATE_ABORTED = 用户刚亲口拒绝安装 → 中性 info，**不发桌面通知**（自己点的取消，再推一条是噪音）；
    // - NOT_INSTALLED = 装不上/被抑制 → error + 桌面通知（窗口可能已收进托盘，用户正等着它连上）。
    if (data.errorCode === 'HELPER_GATE_ABORTED') {
      toast.info(proxyErrorText(data, t));
      return;
    }
    toast.error(proxyErrorText(data, t));
    void notifyDesktop(t('notify.helperNotInstalled.title'), t('notify.helperNotInstalled.body'));
    return;
  }

  // root 孤儿阻断起核：核**未起**（终态，与 HELPER 两码同理），必须 refreshProxyStatus。
  // 与 HELPER 两码同为后端**双出口**（emit 事件 + 让 `api.proxy.start` reject），同样纳入认领闸门
  // 去重（见 proxy-start-claim.ts 头注）：认领期内让位给 Home 连接按钮的 await 腿，无人认领
  // （托盘/自动连接/switchMode 去抖重启）时照常报——此前这里**没有这一支**，两类入口分别是
  // 「静默丢弃」与「落到 STARTUP_FAILED 兜底的错误指引」，见本函数头注对应小节。
  if (data.errorCode === 'ROOT_ORPHAN_BLOCKED') {
    void refreshProxyStatus();
    if (isProxyStartClaimed()) return;
    // 不经 proxyErrorText 第 1 段——本码在门的豁免表里判断过后决定不配 i18n 键：message 带着
    // 这次残留进程的 pid（sudo kill 掉哪个、或修 helper），任何键只能给同义反复，会把这句唯一
    // 可诊断的话换掉。恒走第 2 段（后端原文）。
    toast.error(proxyErrorText(data, t));
    void notifyDesktop(t('notify.rootOrphanBlocked.title'), t('notify.rootOrphanBlocked.body'));
  }
}

export default function App() {
  const { t, i18n } = useTranslation();
  const loadConfig = useAppStore((s) => s.loadConfig);
  const refreshProxyStatus = useAppStore((s) => s.refreshProxyStatus);
  const servers = useAppStore((s) => s.servers);

  // 延迟是会话缓存，不该替已删除节点永久占位。配置节点集每次收敛后同步裁剪；保留节点的测量值不动。
  useEffect(() => {
    useLatencyStore.getState().retainServerIds(servers.map((server) => server.id));
  }, [servers]);

  // 系统代理**活态**轮询（`system_proxy_get_status`）——**全窗口唯一一处驱动**。
  // 消费方是 StatusBar 与 HomeScreen，两者同屏共存；各起一份轮询会双倍 exec
  // `networksetup`/`gsettings`/`reg`，且两条链不同相时会出现「首页说未生效、状态栏还亮绿灯」。
  // 真值存 `use-system-proxy-live` store，消费方各自读（同 latency / ipInfo 的既有形态）。
  useSystemProxyLivePolling();

  // 统一 tooltip 引擎（`data-tip` 属性驱动，取代原生 `title=`；见 lib/tooltip-engine.ts 头注）。
  // **必须挂在这里**：引擎是全局事件委托（document 级 mouseover/focusin），挂进任何一屏都会随该屏
  // 卸载失效，而 `data-tip` 遍布所有屏。返回值即拆卸函数，StrictMode 的双调用自洽。
  useEffect(() => initTooltips(), []);

  // 自动隐私锁闲置计时（autoPrivacyMode + 未锁时武装；见 hook 头注）。
  // **只有隐私锁的计时留在 renderer**：它锁的就是这个界面本身，界面不在时锁它没有意义，而窗口
  // 可见时 renderer 恒不被节流 —— 判断者与被判断者同生共死正是它成立的条件。
  // 自动轻量模式（autoLightweightMode）恰恰相反：它要销毁的就是这个 renderer，计时器不能放在
  // 里面（隐藏窗的 visibilityState 依平台、定时器又被 WKWebView 节流），已整条挪到主进程的
  // `src-tauri/src/idle_lightweight.rs`。前端在此**不留任何轻量模式计时**，避免两套计时打架。
  useIdlePrivacyLock();

  // C8 桌面通知总开关同步（desktopNotifications，缺省视为开）：对齐 上游 notify-user 的
  // setDesktopNotificationsEnabled——config 变即同步给通知出口，`notifyDesktop` 据此静默门控。
  const desktopNotifications = useEffectiveConfig((c) => c?.desktopNotifications);
  useEffect(() => {
    setDesktopNotificationsEnabled(desktopNotifications);
  }, [desktopNotifications]);

  // 界面语言水合：`config.language` 是语言选择的真值源，但 i18n 首屏是同步执行的（不能 await invoke，
  // 否则闪现回退语言），只能先按 localStorage + navigator 兜底解析。这条 effect 就是 `i18n/index.ts`
  // 头注承诺的「App 挂载后从 config.language 校正」那一腿 —— 它此前**根本不存在**，导致切界面语言写进了
  // config 却主窗一个字不变、重启也不变（首屏读的 localStorage 键从来没人写过），只有 Rust 侧原生托盘
  // （`tray.rs:90` 直读 config）跟着变。
  //
  // **`undefined` 必须早退**：config 未水合时 `config?.language` 是 undefined，若照常走一遍会把兜底的
  // `'auto'` 写进 localStorage，把用户上次的具体选择冲掉 —— 下次冷启动首屏就退回跟随系统。
  const languageChoice = useEffectiveConfig((c) => c?.language);
  useEffect(() => {
    if (languageChoice === undefined) return;
    syncLanguageChoice(languageChoice);
  }, [languageChoice]);

  // ── mount 健康门：**必须是最先注册的 effect** ──
  // 回发 renderer:ready = 「renderer 进程活着且 DOM 真的挂上了」。effect 跑到这里意味着 React 已完成
  // 首次 commit —— C 类白屏（进程活着但 DOM 空）恰恰是走不到这一步，主进程超时未收到即判 mount 失败。
  // 直接用 @tauri-apps/api 的 invoke 而非 ./ipc 封装：这条信号是逃生门，依赖面越小越好，不该跟着
  // 业务 IPC 层的演进一起坏。上报失败一律吞掉 —— 宁可门误判超时 reload，也不能让它把 App 搭进去。
  useEffect(() => {
    void invoke(IPC_CHANNELS.RENDERER_READY).catch(() => {});
  }, []);

  // 首帧水合：config + 连接态各拉一次（事件只推增量，初值得自己取）。
  useEffect(() => {
    void loadConfig();
    void refreshProxyStatus();
    // R2 挂载首拉待应用差集（上游 4 触发点之「挂载」）：window 重建后 store 为空时水合操作条。
    pullPendingChanges();
  }, [loadConfig, refreshProxyStatus]);

  // 隐私态首帧水合：`config_get_privacy_mode` 读的是 Rust 进程内 PRIVACY_MODE atomic（跨 renderer
  // reload 存活）。**必须自取初值**——若锁定期间 renderer 白屏 reload / 从托盘锁定后首次开主窗，
  // 光靠 enter/exit 事件（只推增量）会漏掉「已在隐私态」这一初值，遮罩不出 = 安全缺口。幂等且便宜。
  useEffect(() => {
    void api.config
      .getPrivacyMode()
      .then((on) => useAppStore.getState().setPrivacyMode(on))
      .catch(() => {});
  }, []);

  // 隐私模式进/出：后端 config_set_privacy_mode 状态跃迁时 emit（config.rs:355-362）。三来源
  // （托盘「立即锁定」/ idle 计时 / 本窗解锁后 setPrivacyMode(false)）统一经事件收敛到 store。
  useEffect(() => {
    const setPrivacyMode = useAppStore.getState().setPrivacyMode;
    const offEnter = api.config.onEnterPrivacyMode(() => setPrivacyMode(true));
    const offExit = api.config.onExitPrivacyMode(() => setPrivacyMode(false));
    return () => {
      offEnter();
      offExit();
    };
  }, []);

  // Tailscale 登录态挂载兜底：不起核、只查 state 目录「登录过没」，喂 tailscaleLoginStates
  // （组网卡角标的三条 feed 之一，见 domain/tailscale-conn-state.ts）。
  useEffect(() => {
    const ids = servers.filter((s) => s.protocol === 'tailscale').map((s) => s.id);
    if (ids.length === 0) return;
    void api.server
      .tailscaleStateExists(ids)
      .then((states) => useAppStore.getState().applyTailscaleStateExists(states))
      .catch((err) => console.error('[App] tailscaleStateExists failed:', err));
  }, [servers]);

  // Tailscale STATUS 实时校正（tailscaleLoginStates 的第三条 feed）：**A3 relay 已让后端真 emit**
  // `event:tailscaleStatus`——核就绪后订阅 sing-box 管理 API `SubscribeTailscaleStatus` 流，逐端点推
  // 真实 backendState/loggedIn（`proxy.rs::spawn_tailscale_status_relay`）。故此处订阅是真接线（非死通道）：
  // 每条事件把该节点的 loggedIn 真值写回 store，组网卡角标（deriveTsCardState）随之从「未登录」转「已登录」。
  //
  // **不订阅** `event:tailscaleStateCleared`：语义是「属主归一删 state → 登录态真失效」（本仓无该机制，
  // 且核停 ≠ 登录失效，登录经 state 目录持久），后端仍零 emit；订阅死通道 = 假接线（且 stateCleared 若在
  // 停核时误发会把 loggedIn 打回 false = 真 bug）。见复审队列。
  //
  // **必须过 `isDefinitiveTsLoginFrame`（W1 判决门）**：后端的 `loggedIn` 是折叠值
  // （`backendState ∈ {Running,Starting} 且未过期`），核启动早期的 `NoState`/`Stopped` 帧会折叠成
  // false —— 那是「后端还没启完」而非「凭据无效」。无条件写下去不只让角标闪一下「未登录」，
  // 更会经 `setTailscaleLoginState` 的**双写**把这个假 false 写穿进 localStorage 缓存，
  // 而缓存正是代理关着时唯一的登录态来源 ⇒ 下次冷启动进来就显示「需登录」。判据全文见该函数。
  //
  // **判决门只管折叠登录态那一路**：原始帧另存一份（`setTailscaleStatus`）。此前本订阅只取
  // `data.loggedIn` 一个布尔，把 `peers` / `backendState` / `expired` 全丢了 —— 于是
  // `deriveTsExitWarning` 的 `peers` 恒为 undefined，`exit-device-offline` /
  // `exit-device-not-advertised` 两条**在产品里永不可达**（谓词与单测都在，只是没人喂数据），
  // 「选中的出口设备在线但没广告出口」这种流量出不去的形态零提示。原始帧不过判决门：
  // 那道门是为了不把启动过渡帧写穿进登录缓存，而告警要的恰是「控制面这一刻怎么说」。
  useEffect(() => {
    const off = api.proxy.onTailscaleStatus((data) => {
      useAppStore.getState().setTailscaleStatus(data);
      if (!isDefinitiveTsLoginFrame(data)) return;
      useAppStore.getState().setTailscaleLoginState(data.serverId, data.loggedIn);
    });
    return off;
  }, []);

  // Tailscale 交互登录 URL 全局兜底（对齐 上游 ProxyManager 抓到 authURL 即
  // `shell.openExternal` 自动开浏览器 + 系统通知，见 ProxyManager.ts:6634/6854）。**必须挂在 App 全局
  // 而非仅 TsLoginDialog 内**：登录弹窗一旦最小化/收进托盘/卸载，其局部订阅即失效，登录 URL 到达时
  // 无人接 → 用户完全错过登录（审计 diag-update：Polaris 此前仅弹窗内手动点「在浏览器打开」才
  // openExternal，托盘/最小化态必漏）。收到即三件事：① 落 store（authUrl 真值，弹窗/角标读同一份，
  // 永不丢）② 自动开浏览器（openExternal）③ 系统桌面通知（notifyDesktop 尊重 desktopNotifications
  // 总开关+权限，正文不含节点身份）。按 URL 去重（seen = 上游 tailscaleAuthSeen）：同一 URL 重复到达
  // 不重复开浏览器/弹通知（弹窗内的手动按钮仍可再开）。
  const tsAuthSeen = useRef<Set<string>>(new Set());
  useEffect(() => {
    const setTailscaleAuthUrl = useAppStore.getState().setTailscaleAuthUrl;
    return api.proxy.onTailscaleAuth((data) => {
      if (!data.url) return;
      if (data.serverId) setTailscaleAuthUrl(data.serverId, data.url);
      if (tsAuthSeen.current.has(data.url)) return;
      tsAuthSeen.current.add(data.url);
      void api.system.openExternal(data.url);
      void notifyDesktop(
        t('notify.tsLogin.title'),
        t('notify.tsLogin.body')
      );
    });
  }, [t]);

  // 登录期出口让位（A4）：**后端 `reconcile_login_fallback` 真 emit**（选中账号制 TS 全隧道出口未就绪时，
  // 默认路由零重启热切 direct → engaged=true；就绪/关开关后切回 → engaged=false）。故此订阅是真接线（非死通道，
  // 见复审队列 R26——原「零 emit」判断随 A4 落地作废）。engage → 提示已临时直连；同出口就绪切回（带 serverName）
  // → 提示已恢复；切走出口的静默复位（engaged=false 无 serverName）不弹（避免误报「已切回 XXX」）。
  useEffect(() => {
    const off = api.proxy.onMeshLoginFallback((data) => {
      if (data.engaged) {
        toast.info(t('nodes.meshLoginFallbackTitle'));
      } else if (data.serverName) {
        toast.success(t('nodes.meshLoginFallbackRestored', { name: data.serverName }));
      }
    });
    return off;
  }, [t]);

  // 30s 低频兜底轮询（系统设计 §B.3.7）：**事件推送为主、轮询兜底**，不是二选一。
  // 上游 是 2s 轮询；Polaris 降到 30s 并以事件为主，但**不能降到零**——事件面盖不住的边缘态仍存在：
  // 主进程崩溃恢复、轻量模式返回、以及任何「后端状态变了但那条腿没 emit」的缺口（本仓刚发现崩溃腿
  // 就不发 proxyStopped）。轮询是这类未知缺口的最后一道网，30s 的代价是每半分钟一次本地 IPC。
  useEffect(() => {
    const id = setInterval(() => void refreshProxyStatus(), 30_000);
    return () => clearInterval(id);
  }, [refreshProxyStatus]);

  // 连接态：proxyStarted / proxyStopped → 重拉真值。
  // 不在 store 的 startProxy/stopProxy 里直接写 proxyStatus——那样只有「本窗口发起的启停」会刷新，
  // 托盘浮层发起的不会。走事件则三个来源（Home 按钮 / 托盘 / 后端自愈重启）统一收敛。
  useEffect(() => {
    const offStarted = api.proxy.onStarted(() => {
      void refreshProxyStatus();
      // R2 pull 兜底：重启落地 → 起核快照刷新 → 待应用差集清空/更新（PUSH 的重启腿留下的 added 由此清）。
      pullPendingChanges();
    });
    const offStopped = api.proxy.onStopped(() => {
      void refreshProxyStatus();
      // R2 pull 兜底：停核 → 无起核快照 → 差集恒空（操作条自然卸载）。
      pullPendingChanges();
    });
    return () => {
      offStarted();
      offStopped();
    };
  }, [refreshProxyStatus]);

  // runtime 生命周期结局（`event:proxyLifecycle`）：后端在**真状态跃迁点**发 —— 覆盖上面那对
  // proxyStarted/Stopped 盖不住的全部**后端自驱**路径（去抖重启 /「立即应用」/ drain 排空 / 崩溃自愈）。
  //
  // 这里只做一件事：重拉连接态。载荷刻意不带 pid/startTime（权威源是 `proxy:getStatus`，事件只作
  // 变更信号——同 `onStarted` 头注那条既定结论），故收到即 `refreshProxyStatus()`。**不再 pull 差集**：
  // 后端在同一跃迁点已经 PUSH 了差集（`push_pending_changes` 与 `push_lifecycle` 严格配对），
  // 这里再拉一次纯属重复的往返。
  //
  // 修的是：此前内部重启后 `proxyStatus` 无人刷新 ⇒ 首页 pid / 已运行时长最多陈旧 30s（到下一次
  // 兜底轮询才对上）。「应用中…」的收场归 PendingChangesBar 自己那条订阅（它要的是结局，不是连接态）。
  useEffect(() => {
    const off = api.proxy.onLifecycle(() => void refreshProxyStatus());
    return off;
  }, [refreshProxyStatus]);

  // 待应用差集 PUSH（R2）：后端 `switch_mode` 末尾 emit `event:proxyPendingChanges`，
  // 载荷与 pull 腿**同构**（`{added, modified, removed}`，后端同一个 `PendingChangesSummary`）。
  // 收到即写 store → 待应用操作条渲染/更新。清差集（重启落地/停核/冷启）由上方 started/stopped +
  // 挂载/订阅 pull 兜底（PUSH 盖不住的边缘态）。
  useEffect(() => {
    const off = api.proxy.onPendingChanges((data) =>
      useAppStore.getState().setPendingChanges({
        added: data.added ?? [],
        modified: data.modified ?? [],
        removed: data.removed ?? [],
        restartDeferred: data.restartDeferred ?? false,
      })
    );
    return off;
  }, []);

  // 配置变更：任何写盘路径（config_save / config_set_value，见 commands/config.rs 的
  // broadcast_config_changed）都会广播 → 重拉 config。这是「写后端 → UI 刷新」的兜底总线：
  // 弹窗提交后各自 loadConfig(true) 是快路径，此处保证漏掉的、以及别的窗口写的，也能收敛。
  //
  // **必须 force**：非 force 会被在飞的 loadConfig 单飞合并——写 1 的回声启动 get；写 2 在其在飞期间
  // 落盘，其回声被合并进写 1 那次 get；若该 get 携带的是写 2 之前的快照，store 就停在旧值，而写 2 的
  // 回声已被消费掉、不会再有刷新 → 与磁盘持久分叉（表现：开关点了弹回、拖拽回跳一拍）。
  // force 自带代际失效（invalidateLoadConfig），旧在飞载的回填会被丢弃。代价只是每次广播一次 get。
  useEffect(() => {
    const off = api.config.onChanged(() => void loadConfig(true));
    return off;
  }, [loadConfig]);

  // 代理错误：分两条不同语义的腿，其余码（如 STARTUP_FAILED）忽略——那类必然伴随某次 proxy.start
  // 的 reject，发起方（Home 连接按钮）自己会 toast；两处都报 = 同一次失败弹两遍。后端把事件收口在
  // set_error/set_nonfatal_error（运行时进入错误/警告态的状态跃迁点），故这里能捞到所有「没人等着的」异常。
  //
  // **崩溃腿**（PROCESS_EXITED / AUTO_RESTART_FAILED）：核不再运行。**必须同时刷连接态**——崩溃腿只走
  // set_error → 只 emit proxyError，**不 emit proxyStopped**（全仓无任何崩溃路径发它）。不刷则 UI 一直假
  // 「已连接」：圆钮停在 on、状态栏显已连接、uptime 还拿陈旧 startTime 继续走字，直到用户手动启停。
  //
  // **出口误导腿**（SYSTEM_PROXY_FAILED / EXIT_MISMATCH）：核**仍在运行**，只是流量未按预期经代理
  // （系统代理设置失败 / 实际生效出口 ≠ 选中节点，见 proxy.rs::set_nonfatal_error），**不能**刷连接态或
  // 报「已断开」——那会把一个「跑着但走错路」的活核错误地标成「已停」，报本码自己的警告文案即可。
  // 文案一律经 `proxyErrorText`（按 errorCode 取 i18n 键），**不再**用后端 message —— 那是中文串。
  useEffect(() => {
    // `t` 的真实类型（i18next TFunction）重载形状与本函数需要的窄接口不完全结构兼容（可测性收窄），
    // 两处实际调用形（0/1 个字符串兜底参数）均是 TFunction 支持的合法调用，此处仅收窄类型标注。
    const simpleT = t as (key: string, fallback?: string) => string;
    const off = api.proxy.onError((data) =>
      handleProxyErrorEvent(data, { t: simpleT, refreshProxyStatus })
    );
    return off;
  }, [t, refreshProxyStatus]);

  // 无效节点：起核时 detour 级联剔除的死引用节点（config-engine 判、runtime emit）→ store 标灰。
  // **空数组不短路**：空 = 清陈旧标灰（上次无效的这次有效了）。后端每次起核都发（含空），是全量快照。
  useEffect(() => {
    const off = api.proxy.onInvalidNodes((nodes) =>
      useAppStore.getState().setInvalidNodes(nodes)
    );
    return off;
  }, []);

  // 出口 IP 推送（B3）：后端每次探测出口 IP（本地直连 / 代理出口，含中间态与终值）后 emit
  // `event:ipInfoUpdated`（misc.rs:805 `broadcast`）。**必须在此订阅**——托盘浮层是独立窗口、不共享本 store，
  // 主窗漏订则多 webview 的状态栏 IP 各自停在自取的旧值、互不同步（这正是本仓 IP 不同步的根因）。
  // 收到即整帧写 store（同 上游 handleIpInfoUpdated），StatusBar 消费 store.ipInfo 随之收敛到同一真值。
  //
  // 订阅之外还要 **peek 一次水合**（零探测读后端快照）：窗口重建 / 主窗后开时，上一帧 `ipInfoUpdated`
  // 早已发过，只订阅的话 store 会空到下一次探测（可能是几分钟后的下一次切节点）。peek 是纯读缓存、
  // 不打网，正是为这个场景存在的。**只在 store 仍为空时写**——peek 是异步的，若期间真探测的结果先到，
  // 用旧快照覆盖新结果就是倒退（后端缓存里那份可能正是被覆盖的上一帧）。
  useEffect(() => {
    const off = api.ipInfo.onUpdated((snap) => useAppStore.getState().setIpInfo(snap));
    api.ipInfo
      .peek()
      .then((snap) => {
        if (!useAppStore.getState().ipInfo) useAppStore.getState().setIpInfo(snap);
      })
      .catch(() => {
        /* 非 Tauri（浏览器预览）忽略 */
      });
    return off;
  }, []);

  // 测速结果数据面 —— 与下面的解锁数据面**同一个理由**：必须挂 app 级，不能挂业务屏。
  //
  // 原先 HomeScreen / NodesScreen / StatusBar 各自在组件内订阅、各自存一份私有 useState，而
  // `ScreenRouter` 是裸 switch（无 keep-alive）⇒ 切屏即卸载即丢；测速期间切走还会漏掉在飞的流式结果。
  // 对齐 上游 `use-native-events.ts:371-374`（该注释原文记着 上游 修过同一个 bug：
  // 「原 per-node 监听写在 handler 作用域内…提到顶层持久 hook 后跨页不丢」）。
  // 三屏改为读 `useLatencyStore`（单一真值），本订阅是它在主窗的唯一写入口。
  useEffect(() => subscribeLatencyEvents(), []);

  // 订阅更新逐阶段进度 → `use-subscription-progress-store` → 节点页的订阅信息栏。
  // 同上一条的理由（切屏不丢），外加一条本条独有的：**后台 scheduler 也会更新订阅**，
  // 那条腿没有任何前端调用点可以挂 state，只能靠事件流。
  useEffect(() => subscribeSubscriptionProgressEvents(), []);

  // 测速**进度** → 全局 sticky toast。与上面的结果数据面同一个理由（切屏不丢），只是出口不同：
  // 结果落 store 供三屏渲染，进度没有归属屏 —— 它是「这台机器正在忙什么」的全局事实，
  // 原先只在节点页画一行字，切走就没了。收口/终止语义与「为什么不从调用点取终止信号」
  // 见 `lib/speedtest-progress-toast.ts` 文件头。
  //
  // 外部面在这里装配（协调器本身零运行时 import）：它一旦静态 import `./i18n`，就会在模块加载期
  // 碰 `document`，本仓无 jsdom 的 vitest 里整个模块加载失败 ⇒ 它那批门全部消失。同理走 `i18n.t`
  // 而不是 `useTranslation()` 的 `t`：这条订阅活一辈子，闭包捕获的 `t` 在用户切语言后就是陈旧的。
  //
  // 终态（完成/中断）走 `onSpeedTestDone` 这条**广播**通道，而不是 `speedTest()` 的返回值：托盘浮层是
  // 独立 JS 堆，它发起的那轮测速，返回值到不了主窗的 toast（这正是「断开后进度条还要等十几秒才转中断」
  // 的根因）。静默超时随之降级为纯兜底。
  //
  // 「继续」按下时才发续测请求（**不自动续**，判据见协调器文件头）。两个注入面都必须是 getter：
  // 本订阅活一辈子，闭包捕获的节点数组在用户改订阅后就是陈旧的。续测失败静默 —— 它是增益路径，
  // 而失败原因（核又没了 / in-flight）用户下一步操作自然会再撞上，此处再弹一条只是噪音。
  useEffect(
    () =>
      subscribeSpeedTestProgressToast({
        subscribe: (l) => api.server.onSpeedTestProgress(l),
        subscribeDone: (l) => api.server.onSpeedTestDone(l),
        toast,
        t: (key, vars) => i18n.t(key, vars ?? {}),
        currentServerIds: () => useAppStore.getState().servers.map((s) => s.id),
        run: (ids) => {
          void api.server.speedTest(ids).catch(() => {
            /* 续测失败静默（见上） */
          });
        },
      }),
    []
  );

  // 解锁检测数据面（progress / updated / invalidated）——**必须挂在 app 级，不能挂 HomeScreen**。
  //
  // 对齐 上游 `use-native-events.ts:481-483`（顶层持久 hook）。原先这三条订阅在 HomeScreen 内、且 deps 含
  // `proxyStatus?.running` ⇒ 两个后果：①切离首页即退订，后台自跑的终态无人接收，切回来看到的是陈旧态；
  // ②起停代理时 deps 变化触发重订，正好错过 invalidate 前后那几帧。检测由后端驱动、跨页存活，订阅也必须跨页存活。
  //
  // 三条 handler 都走 `getState()` 而非闭包捕获，故依赖恒空、订阅只建一次。
  useEffect(() => {
    const offProgress = unlockApi.onProgress((p) =>
      useAppStore.getState().setUnlockProgress(p.serviceId, p.result)
    );
    const offUpdated = unlockApi.onUpdated((snap) =>
      useAppStore.getState().applyUnlockSnapshot(snap)
    );
    const offInvalidated = unlockApi.onInvalidated((payload) => {
      // 载荷带**后端核真态**（`{running, exitBlocked}`）——invalidate 常先于 EVENT_PROXY_STARTED 抵达，
      // 前端此刻的 proxyStatus 是陈旧的。这正是后端带上这两个字段的理由，故据载荷判、不读本地视图。
      // running && !exitBlocked → 后端 1500ms 去抖后会自跑，此处显「检测中」等它；否则复位 idle。
      const willRerun = payload.running && !payload.exitBlocked;
      if (willRerun) {
        useAppStore.getState().beginUnlockCheck();
      } else {
        useAppStore.getState().resetUnlock();
      }
    });
    return () => {
      offProgress();
      offUpdated();
      offInvalidated();
    };
  }, []);

  // 解锁检测冷水合（仅 store 无检测态时）：窗口重建 / 首次进入且后端自跑尚未落 → 拉一次上次快照；
  // 无有效快照则**主动发起一轮**（对齐 上游 `use-unlock-detection.ts:106-131` 的 kickAutoRun 冷启动腿）。
  // 后端 gating 自守（核没跑 → 短路返 blocked 快照），故此处不再判 running，避免启动竞态下永不发起。
  useEffect(() => {
    let cancelled = false;
    const hasState = (u: UnlockDisplayState) =>
      u.running || u.checkedAt != null || Object.keys(u.results).length > 0;
    if (hasState(useAppStore.getState().unlock)) return;
    void unlockApi
      .get()
      .then((snap) => {
        if (cancelled || hasState(useAppStore.getState().unlock)) return; // 期间已被事件填充 → 不覆盖
        if (snap && (snap.checkedAt || snap.notReady || snap.blockedReason)) {
          useAppStore.getState().applyUnlockSnapshot(snap); // 有终态快照 → 零网络水合
        } else {
          void unlockApi
            .run(false)
            .then((s) => useAppStore.getState().applyUnlockSnapshot(s))
            .catch(() => useAppStore.getState().resetUnlock());
        }
      })
      .catch(() => {
        /* 非 Tauri 环境忽略 */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // 自动换节点（C3）：节点故障自动切换成功后后端 emit `event:autoNodeSwitched`（payload 含 newServerName/latency）。
  // **emit 发射点由并行 proxy 批接线**（events.rs 已定义常量，发射随该批落地）——故此订阅非死通道，与 A3/A4 同型
  // （契约先行、后端随批合璧，wave gate 验前后端合璧）。收到后三件事：①刷连接态（切换后运行核已指向新节点，
  // getStatus 取真值）②loadConfig(true) 重拉 config（selectedServerId 已被后端改到新节点 → 下拉/卡片选中态同步）
  // ③toast 告知已切到哪个节点。Polaris toast 为单串（无 description）→ 只用 home.autoSwitched（含 name/latency），
  // 舍 上游的补充 autoSwitchedDesc。
  useEffect(() => {
    const off = api.proxy.onAutoNodeSwitched((data) => {
      void refreshProxyStatus();
      void loadConfig(true);
      toast.success(t('home.autoSwitched', { name: data.newServerName, latency: data.latency }));
    });
    return off;
  }, [t, refreshProxyStatus, loadConfig]);

  // 系统代理残留：仅 TUN 模式、每会话一次——检测到**别人设的**系统代理仍开着（无 Polaris marker）
  // → 一次性提示（非阻塞）。文案用现成 i18n（`proxy.*` 命名空间，{{proxy}}=host:port；此前误取 `settings.*`
  // 空键 → 显原始 key）。marker 门控保证绝不 stomp 用户自配。一键关闭动作入口在「网络 > 系统代理」区
  // （B2；Polaris toast 无动作按钮，故动作落设置页而非 toast，对齐 上游 语义）。
  useEffect(() => {
    const off = api.proxy.onSystemProxyResidual(({ proxy }) =>
      toast.warning(t('proxy.systemProxyResidualDesc', { proxy }))
    );
    return off;
  }, [t]);

  // #17 非官方核 ≤ 随包基线 → 兼容风险警告（对齐 上游 use-native-events.ts::handleCoreBaselineWarning）。
  // **每会话一次**：启动期发射，若不去重则轻量模式返回/window 重建重挂 App 时会重复唠叨（同上方
  // systemProxyResidual 的每会话一次模式）。去重闸门用模块级单例 `coreBaselineWarnGate`（见文件顶部），
  // 等价于 上游的 `let coreBaselineWarnedThisSession`，但工厂形态可被单测锁死「第 2 次返 false」。
  // 发射端由并行后端批补上（启动期检测非官方核 ≤ 随包基线时发，payload {current, bundled, kind}）。
  // Polaris 的 toast.warning 是单串签名（无 sonner 的 options 对象）→ 标题与详情合成一条，
  // 直接用带 {{current}}/{{bundled}} 插值的 coreBaselineWarnDesc（同 systemProxyResidualDesc 的用法）。
  useEffect(() => {
    const off = api.proxy.onCoreBaselineWarning(({ current, bundled }) => {
      if (!coreBaselineWarnGate()) return;
      toast.warning(t('settings.advanced.coreBaselineWarnDesc', { current, bundled }));
    });
    return off;
  }, [t]);

  // 订阅后台自动更新失败：scheduler 拉取失败此前**完全静默**（无日志/无事件/无 toast），用户不知订阅已
  // 停更。补一条**仅失败态** toast——对齐 上游 后台更新的静默度（上游 后台失败只入日志、不弹成功、不发系统
  // 通知；OS 通知在 上游 仅留给代理崩溃等），故此处不弹成功、不走 notifyDesktop。退避已限频（5min→6h），不刷屏。
  // 手动刷新的三态 toast 走 NodesScreen/SubDialog（updateServers 命令），不经本通道。
  useEffect(() => {
    const off = api.subscription.onAutoUpdate((data) => {
      // R2 pull 兜底：订阅自动刷新的新节点绕过 switch_mode 落盘（不经 PUSH）→ 其差集经此 pull 纳入（设计 §5.3）。
      pullPendingChanges();
      if (!data.success) {
        toast.error(t('nodes.subAutoUpdateFail'), data.error);
      }
    });
    return off;
  }, [t]);

  return <AppShell />;
}
