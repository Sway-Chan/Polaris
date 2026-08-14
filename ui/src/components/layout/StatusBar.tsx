/**
 * StatusBar —— 主内容区底部状态栏（原型 .statusbar L544-558，markup L2430-2438）。
 *
 * 位置：在 .main 内部（flex 子项，与 sidebar 无关）。原型结构：
 *   [状态点+文案] · [节点图标+名] · [国旗+IP] · [延迟] —— 右贴 —— [↓速率 ↑速率 · 连接数]
 * 高 32px，flex 一行，overflow:hidden + nowrap。窄宽时按 sb-fold-* 折叠（≤500/620/520px）。
 *
 * 数据来源：
 *  - 运行态 / 当前节点名：app-store（proxyStatus + servers/selectedServerId，同 HomeScreen currentServer 取法）。
 *  - 出口 IP + 出口地区：app-store `ipInfo`（单一真值，水合与订阅统一挂 App.tsx 顶层；本组件只读）。
 *    地区与 IP **同源同一次探测**——回答「我现在从哪出去」，唯一诚实的依据就是那次出口 IP 探测
 *    带回的 countryCode / country，绝不用节点名/入口域名派生（理由见 domain/exit-flag.ts）。
 *    旗优先，无旗（境外直连出口 ipip 无 ISO 码）回落 country 地名文本（`resolveExitRegion` 三态）；
 *    两者皆无 → 不画。
 *  - 当前节点延迟：全局 `use-latency-store` 按 selectedServerId 取（与 NodesScreen/HomeScreen 同一份 map，
 *    订阅挂 App.tsx 顶层）——IpInfoSnapshot 本身不带 latency 字段，节点延迟的单一真值是测速结果流。
 *  - 上下行速率 + 连接数：statsApi 'stats' topic（EVENT_STATS_UPDATED → TrafficStats）。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { createTopicSubscription } from '@/lib/topic-subscription';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { useLatencyStore, isLatencyStale } from '@/store/use-latency-store';
import { api } from '@/ipc';
import type { TrafficStats } from '@/contracts/types';
import { fmtRate } from '@/components/screens/shared/format';
import { SbNodeIcon, SbConnsIcon } from '@/components/icons';
import { FlagImg } from '@/components/flag-img';
import { isBlockSelection, isDirectSelection } from '@/domain/direct-selection';
import { resolveExitRegion, localizeRegion } from '@/domain/exit-flag';
import {
  resolveStatusBarExitIp,
  resolveStatusBarLatencyText,
  resolveStatusBarNodeName,
} from './status-bar-display';
import { deriveTakeoverConnState } from '@/components/screens/home/connection-state';
import { useSystemProxyLive } from '@/store/use-system-proxy-live';

/**
 * 延迟分级着色（原型 `latLevel()`，L3037/setExitLatLevel L3041）：断开恒 'none'；
 * 已连接按阈值 <80 fast / <150 mid / <300 slow / 否则 dead。CSS 靠 `#sb-lat.<level>`
 * 上色（prototype.css L393-397），故此值必须同时写成 id + class。
 */
type LatLevel = 'fast' | 'mid' | 'slow' | 'dead' | 'none';
function latLevel(connected: boolean, ms: number | null | undefined): LatLevel {
  if (!connected || ms === null || ms === undefined) return 'none';
  if (ms < 80) return 'fast';
  if (ms < 150) return 'mid';
  if (ms < 300) return 'slow';
  return 'dead';
}

export default function StatusBar() {
  const { t, i18n } = useTranslation();
  const proxyStatus = useAppStore((s) => s.proxyStatus);
  const servers = useAppStore((s) => s.servers);
  const selectedServerId = useAppStore((s) => s.selectedServerId);
  const proxyModeType = useEffectiveConfig((c) => c?.proxyModeType);

  /** 连接态**按接管方式分叉**（契约 L17）：systemProxy 下「核在跑」≠「流量经核」。
   *  systemProxy 分支的判据是**活态**（后端实读 OS 代理设置并与本进程 mixed 入站比对），
   *  `errorCode` 退为活态未知时的回落 —— 二者的主次与三态方向见 `home/connection-state.ts`。 */
  const running = proxyStatus?.running ?? false;
  /** 活态读**共享 store**（`use-system-proxy-live`），轮询驱动唯一挂在 App.tsx 顶层。
   *  勿改回组件内私有轮询：本组件与 HomeScreen 同屏共存，各起一份就是双倍 exec
   *  `networksetup`/`gsettings`/`reg`，且两条链不同相时会出现「首页说未生效、状态栏还亮绿灯」。
   *  适用范围门（核在跑 + systemProxy）与兜底口径一并收在那个 store 里，此处只读结论。 */
  const systemProxyLive = useSystemProxyLive();
  const connState = deriveTakeoverConnState({
    running,
    proxyModeType,
    errorCode: proxyStatus?.errorCode,
    systemProxyLive,
  });
  /** 展示口径的「已连接」——degraded 一律按未连上呈现（状态点/文案/延迟/出口 IP/速率全部据此）。
   *  systemProxy 没设上时流量走的是本机直连出口，此时若仍按「已连接」显示代理出口 IP + 代理延迟，
   *  等于把直连流量描述成走了代理，与绿灯同源的误导。 */
  const connected = connState === 'connected';

  const currentServer = servers.find((s) => s.id === selectedServerId) ?? null;
  const directSelected = isDirectSelection(selectedServerId);
  const blockSelected = isBlockSelection(selectedServerId);

  /** 出口 IP 快照：读 app-store 单一真值（水合 peek + `EVENT_IP_INFO_UPDATED` 订阅统一挂 App.tsx 顶层）。
   *
   *  勿改回组件内私有 useState + peek：本组件唯一挂载点是 `AppShell.tsx` 的 `<main>`（托盘浮层是另一个
   *  入口、**不渲染 StatusBar**，别拿它当理由），而该布局会随窗口重建 / 路由重挂而卸载 —— 订阅与快照
   *  存在组件里就随之丢掉，要等下一次探测（可能是几分钟后的切节点）才恢复。提到 App.tsx 顶层订阅 +
   *  store 单一真值后，重挂只是重新读同一份 store。**多窗口口径一致**是另一件事，靠后端 `broadcast`
   *  向全部 webview 发同一帧保证（见 App.tsx 该订阅处注释），不靠本组件。 */
  const ipInfo = useAppStore((s) => s.ipInfo);
  const [stats, setStats] = useState<TrafficStats | null>(null);

  /** 当前出口延迟：从**全局** latencyMap 按 selectedServerId 取（订阅挂 App.tsx 顶层）。
   *  切节点自然得到新 id 的值、未测过则 `undefined`（显「—」）——旧实现的「切节点手动清空」由
   *  「按 id 取」天然覆盖，无需额外清理。勿改回组件内的测速结果私有订阅：那样本条会随
   *  状态栏所在布局重挂而丢，且与三屏各存一份是同一个病（见 store/use-latency-store.ts）。 */
  const latencyMs = useLatencyStore((s) =>
    selectedServerId ? s.latencyMap[selectedServerId] : undefined
  );
  /** 该延迟的落库时刻（契约「陈旧>30min 半透明」的数据基础，见 use-latency-store）。 */
  const latencyTestedAt = useLatencyStore((s) =>
    selectedServerId ? s.testedAt[selectedServerId] : undefined
  );

  /* ── 上下行速率 + 连接数：订阅 stats topic ──
   * 走与首页拓扑 / 连接页明细同一份 `createTopicSubscription`。本腿原先两条缺陷俱在：
   *  - 监听挂在 `subscribe()` 的 `.then()` 里 → `run_stats_poller` 不 sleep 的首拍打在无监听的窗口上
   *    被丢，状态栏速率要等下一拍（1s）才有第一个数；
   *  - cleanup 里的 `off()` 在 `.then()` 未 resolve 时还是空壳 → 真监听在 cleanup 之后注册且再没人摘
   *    （StrictMode 的双挂载每次冷启动都稳定走这条路），漏一个恒活的 onStatsUpdated 监听。 */
  useEffect(() => {
    const sub = createTopicSubscription<TrafficStats>(
      {
        onFrame: (cb) => api.stats.onStatsUpdated(cb),
        subscribe: () => api.stats.subscribe('stats'),
        unsubscribe: () => api.stats.unsubscribe('stats'),
      },
      setStats
    );
    sub.setWanted(true);
    return () => sub.dispose();
  }, []);

  // 三条口径见 status-bar-display.ts 顶部行号注释（原型 setConnected :3433-3455 / pickDirectExit :3513 /
  // 契约「状态栏」条目）：节点名不随连接态切换 + 直连显「直连」；延迟断开恒 '—'；出口 IP 按连接态分叉不跨态回落。
  const nodeName = resolveStatusBarNodeName(
    directSelected,
    currentServer?.name,
    t('home.routingDirect'),
    t('home.plsConfigServer'),
    blockSelected,
    t('home.routingBlock')
  );
  const exitIp = resolveStatusBarExitIp(connected, ipInfo?.proxy?.ip, ipInfo?.direct?.ip);
  // 出口地区：与 exitIp 同一条口径、同一次探测（按连接态分叉、不跨态回落）。旗优先，无旗回落地名文本
  // （境外直连出口 ipip 无 ISO 码 → 画不出旗，退到 country 地名而非只剩裸 IP）。三态：flag / text / none。
  const exitRegion = resolveExitRegion(connected, ipInfo?.proxy, ipInfo?.direct);
  // text 分支的 region 未本地化，此处折成当前语言地区名（2 位 ISO→地区名；ipip 地名文本→原样）。
  const exitRegionText =
    exitRegion.kind === 'text' ? localizeRegion(exitRegion.region, i18n.language) : undefined;
  const latency = resolveStatusBarLatencyText(connected, latencyMs);
  const rateDn = fmtRate(stats?.downloadSpeed);
  const rateUp = fmtRate(stats?.uploadSpeed);
  const conns = stats?.activeConnections ?? 0;

  /** 延迟陈旧（>30min）→ 半透明。**只降不透明度、不改数值也不清空**：旧值仍是「上次测出来的事实」，
   *  抹掉它反而丢信息；半透明表达的是「这个数该重测了」。断开态文本恒 '—'，无所谓陈旧。
   *  刷新时机：本组件随 stats 事件（连接态下约 1s 一帧）重渲染，跨过 30min 门槛后自然转半透明；
   *  不为此单挂定时器（断开态本就不显数值，连接态有事件流兜住）。 */
  const latencyStale = connected && isLatencyStale(latencyTestedAt);

  // 状态点/文案三态（契约 L17）：degraded = 核在跑但流量没经核 → 琥珀 + 明说未生效，
  // 既不冒充「已连接」（原状的误导），也不谎报「已断开」（核确实在跑，圆钮该显示断开动作）。
  const statusText =
    connState === 'connected'
      ? t('home.statusConnected')
      : connState === 'proxy-degraded'
        ? t('home.statusProxyDegraded')
        : t('home.statusDisconnected');
  const dotClass =
    connState === 'connected' ? 'ok' : connState === 'proxy-degraded' ? 'warn' : 'idle';

  return (
    <div className="statusbar" role="status" aria-live="polite">
      <span className="sb-i" data-tip={connState === 'proxy-degraded' ? t('home.statusProxyDegradedHint') : undefined}>
        <span className={cn('dot', dotClass)} />
        <b>{statusText}</b>
      </span>

      <span className="sb-sep sb-fold-node" />
      <span className="sb-i sb-fold-node">
        <SbNodeIcon className="w-[13px] h-[13px]" />
        {/* 动作标签轴：出口=阻断时这行文字恒红（与首页 `#cur-node`、规则/应用分流那几个 pill 同色）。
            无色是最坏的一种谎报 —— 状态栏是「现在流量怎么走」的常驻答案，「阻断」在这里读起来
            与一个叫「阻断」的节点名毫无分别。`.act-block-txt` 是纯文字形态，不带 pill 底色。 */}
        <span className={blockSelected ? 'act-block-txt' : undefined}>{nodeName}</span>
      </span>

      <span className="sb-sep sb-fold-ip" />
      <span className="sb-i sb-fold-ip">
        {/* 能画旗显旗；无旗但探到地名 → 回落地名文本（境外直连出口不再只剩裸 IP）；两者皆无 → 不占位。 */}
        {exitRegion.kind === 'flag' ? (
          <FlagImg code={exitRegion.code} className="sb" />
        ) : exitRegionText ? (
          <span className="max-w-[140px] truncate text-[hsl(var(--fg-faint))]">
            {exitRegionText}
          </span>
        ) : null}
        <span className="mono">{exitIp}</span>
      </span>

      <span className="sb-sep sb-fold-lat" />
      <span
        id="sb-lat"
        className={cn('sb-i mono sb-fold-lat', latLevel(connected, latencyMs))}
        style={latencyStale ? { opacity: 0.45 } : undefined}
        data-tip={latencyStale ? t('home.latencyStaleHint') : undefined}
      >
        {latency}
      </span>

      {/* 速率/连接数**仅连接态**显示（契约「状态栏」：`实时速率/连接数(仅连接态)`）。断开时这两个数
          只会是上一帧的残留或恒 0——把「没有数据面」画成「0 B/s · 0 连接」，读起来像「代理在跑只是没流量」，
          与「根本没在跑」是两回事。整块不渲染，右侧留白即诚实。 */}
      {connected && (
        <span className="sb-count sb-i">
          <span className="rate-dn">↓ {rateDn}</span>
          &nbsp;
          <span className="rate-up">↑ {rateUp}</span>
          <span className="sb-sep" style={{ margin: '0 4px' }} />
          <span className="sb-conns">
            <SbConnsIcon className="w-[13px] h-[13px]" />
            <b>{conns}</b>
          </span>
        </span>
      )}
    </div>
  );
}
