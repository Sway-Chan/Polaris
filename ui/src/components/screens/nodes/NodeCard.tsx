/**
 * NodeCard —— 节点卡片（1:1 提取自原型 polaris-prototype.html L1845 等 .nd-card
 * + enhanceNodeCards() L4407-4424 的运行期增强：状态点/水印/复选框）。
 *
 * 原型 DOM 结构（.nd-card，样式见 src/styles/screens.css L「NODES」段）：
 *   .nd-flag（国旗水印，afterbegin 插入，仅代理协议节点、按名称猜中国家才有；见 nd-flag.tsx）
 *   .nd-top（.nd-dot + .nd-name + .nd-lat）
 *   .nd-addr（原型恒 hidden——DOM 保留供搜索匹配，不可见）
 *   .nd-pills（pill proto + 可选 nd-cur/nd-cap + nd-xfer）
 *   .nd-acts（nd-a 按钮：测速/复制/克隆/编辑/删除）
 *   .nd-check（批选复选框，原型 appendChild 置于末尾——position:absolute 不受 DOM 序影响）
 *
 * 数据源：ServerConfig（store.servers）。延迟态来自测速流（props.latencyMs）——
 * 节点本身不存延迟，每次测速经 onSpeedTestResult 推。延迟分级用 shared/format.latLevel。
 */

import { useTranslation } from 'react-i18next';
import type { ServerConfig } from '@/contracts/types';
import { latLevel } from '@/components/screens/shared/format';
import { isMeshNode } from '@/domain/endpoint-routes';
import { invalidNodeReasonText } from '@/domain/invalid-node-reason';
import { useHoverCard, HoverCardPanel } from '@/components/hover-cards/HoverCard';
import { MeshInfoHoverCardContent } from '@/components/hover-cards/MeshInfoHoverCard';
import { cn } from '@/lib/utils';
import { NdFlag, flagCodeForName } from './nd-flag';
import type { NodeUseVia } from './nodes-logic';

/** 协议显示名（小写协议枚举 → 用户可读）。 */
function protocolLabel(proto: string): string {
  const map: Record<string, string> = {
    vless: 'VLESS',
    vmess: 'VMess',
    trojan: 'Trojan',
    hysteria2: 'Hysteria2',
    shadowsocks: 'Shadowsocks',
    wireguard: 'WireGuard',
    tailscale: 'Tailscale',
    anytls: 'AnyTLS',
    tuic: 'TUIC',
    naive: 'Naive',
    snell: 'Snell',
    socks: 'SOCKS',
    http: 'HTTP',
    ssh: 'SSH',
    hysteria: 'Hysteria',
    tor: 'Tor',
    openconnect: 'OpenConnect',
    'openvpn-client': 'OpenVPN',
    custom: 'Custom',
  };
  return map[proto.toLowerCase()] ?? proto;
}

/** 传输/安全摘要（原型 .nd-xfer：reality · tcp / ws · tls / quic 等）。 */
function transferSummary(server: ServerConfig): string {
  const proto = server.protocol.toLowerCase();
  if (proto === 'wireguard') return 'udp · wg';
  if (proto === 'tailscale') return 'mesh · wg';
  if (proto === 'openconnect') return 'enterprise vpn';
  if (proto === 'openvpn-client') return server.openvpnClientSettings?.network || 'udp · vpn';
  const parts: string[] = [];
  if (server.network) {
    const netMap: Record<string, string> = {
      tcp: 'tcp',
      ws: 'ws',
      grpc: 'grpc',
      http: 'http',
      httpupgrade: 'httpupgrade',
    };
    parts.push(netMap[server.network] ?? server.network);
  }
  if (server.security) parts.push(server.security);
  return parts.length > 0 ? parts.join(' · ') : '';
}

export interface NodeCardProps {
  server: ServerConfig;
  /** 测速延迟（ms）。null=超时，undefined=未测。 */
  latencyMs?: number | null;
  /** 当前出口（选中节点）。 */
  isCurrent?: boolean;
  /** 组网出口节点（显示「出口」标记）。 */
  isExit?: boolean;
  /** 仅局域网组网节点（`meshAllowsInternet=false`：WG/WARP 关了「允许访问外网」、TS 没设出口节点）。 */
  lanOnly?: boolean;
  /**
   * 该节点本轮**结构上**能否测速（`isSpeedTestable`，由 NodesScreen 以全屏同一口径算好传入）。
   * `undefined` 视为可测（默认放行）。
   *
   * 为什么是"传进来的布尔"而不是卡内自己判：⚡、页头「全部测速」、工具栏「测速」（可见集）、批选「测速」
   * 四个入口必须同一条过滤线，卡内另起判据就会重现「谓词在、调用点却用弱判定」的老形态。
   */
  speedTestable?: boolean;
  /** 不可测时的原因说明（已本地化）：置灰不给理由等于没说话，用户只会反复点。 */
  speedTestBlockedHint?: string;
  /**
   * 本节点中**被更早组网节点抢占**、因而不会实际生效的网段（`meshShadowedCidrs`，已把抢占者 id 解析成显示名）。
   * 空/未传 = 无冲突。一条 ip_cidr 只能指向一个 outbound，后来者静默失效——不显式标出来，
   * 用户会以为两个节点都在路由这段。
   */
  shadowedCidrs?: { cidr: string; by: string }[];
  /** 选中态（多选批选）。 */
  selected?: boolean;
  /** 多选模式（显隐复选框）。 */
  batchMode?: boolean;
  /**
   * 启动 gate 剔除原因（`EVENT_PROXY_INVALID_NODES` → store.invalidNodes → 本卡）。
   * 有值 = 该节点这一轮**没有进内核配置**（坏配置会让 sing-box 整体 FATAL，故启动前 check 剔掉）。
   * `undefined` = 正常。空串 = 被剔但后端没给原因（仍要置灰，tooltip 走兜底文案）。
   *
   * 对齐 上游 `server-card.tsx:103-104`：**只把名称降透明度 + 挂 tooltip，不禁用点击**——
   * 剔除是会话内语义（每次启动重判、换核可能复活），锁死交互反而让用户没法改配置来修它。
   */
  invalidReason?: string;
  /**
   * 该节点只存在于**暂存**里、磁盘上还没有（判据 `lib/staged-config.stagedOnlyIds` = effective − disk）。
   *
   * 为什么必须标出来：列表读的是 effective（否则用户看不见自己刚做的编辑），而出口选单读磁盘 ——
   * 同一个节点在一张屏上可见、在另一处却选不到。不给标记，用户只会认为「出口选单坏了」。
   * 用词与 pending-bar 的「N 项待保存」同源，不另起一套词汇。
   */
  stagedOnly?: boolean;
  onSpeedTest?: (server: ServerConfig) => void;
  onCopy?: (server: ServerConfig) => void;
  onClone?: (server: ServerConfig) => void;
  onEdit?: (server: ServerConfig) => void;
  /**
   * 设为出口节点。**此前整个节点屏没有这条腿** —— 卡片能显示谁是当前出口（`isCurrent` → `.cur`），
   * 却没有任何入口去改它；切节点只能走首页出口选单或托盘菜单（2026-07-29 真机反馈）。
   *
   * 两个触发面共用同一个回调（禁止分叉）：整卡点击（鼠标顺手）+ `.nd-acts` 里的显式按钮
   * （键盘/读屏可达 —— 整卡不设 `tabIndex`，见下方 speed 按钮注释里「不给列表每张卡新增 Tab 停靠点」
   * 的既有决定，那条至今未变）。
   *
   * **`via` 只报来源，不做决策** —— 确认与否的判据全在 `nodes-logic.nodeUseAction`，卡片不参与。
   * 两个面的误触概率差一个量级，故那边按 `via` 分档（按钮直切、整卡确认）。
   */
  onUse?: (server: ServerConfig, via: NodeUseVia) => void;
  /**
   * 本卡的「设为出口」处于「再点一次确认」待定态。
   *
   * 整卡一律确认、显式按钮直切（陈先生 2026-07-30 裁定；会重启内核那档两个面都确认）。
   * 判据在 `nodes-logic.nodeUseAction`。
   */
  useConfirming?: boolean;
  /** 待定态属于「会重启内核」那一档 —— 文案要先说清现有连接会断，不是只说「再点一次」。 */
  useWillRestart?: boolean;
  onDelete?: (server: ServerConfig) => void;
  /**
   * 是否渲染删除入口。订阅节点传 `false`：删了下次订阅刷新的 reconcile 会照原样拉回来，
   * 操作没有净效果、只剩误删风险（陈先生 2026-07-29 裁定）。
   * 与 ResourcesScreen 对内置资源的处置同一口径 —— **入口不渲染，而非渲染后禁用**：
   * 一颗点了必然无效的按钮比没有更坏。
   */
  deletable?: boolean;
  /**
   * 本卡的删除按钮处于「再点一次即删」待定态（原型 :4140 `node-del` 的 confirmTwice 第一段）。
   * 状态由 NodesScreen 的 `useConfirmTwice` 单点持有 —— 卡片不自持，否则每张卡一份定时器，
   * 又变回「各写各的」。
   */
  deleteConfirming?: boolean;
  onToggleSelect?: (server: ServerConfig) => void;
}

export function NodeCard({
  server,
  latencyMs,
  isCurrent,
  isExit,
  lanOnly,
  speedTestable,
  speedTestBlockedHint,
  shadowedCidrs,
  selected,
  batchMode,
  invalidReason,
  stagedOnly,
  onSpeedTest,
  onCopy,
  onClone,
  onEdit,
  onUse,
  useConfirming,
  useWillRestart,
  onDelete,
  deletable = true,
  deleteConfirming,
  onToggleSelect,
}: NodeCardProps) {
  const { t } = useTranslation();
  /* 组网信息 ⓘ（Tailscale / WireGuard / WARP）：内网地址 / 路由 / 生效出口 / 对端在线。
     判据 `isMeshNode` —— 代理协议节点没有「内网地址/路由」这套概念，整颗不渲染；
     openconnect / openvpn-client 只在用户声明了内网段时才有。
     内容与「配置值 vs 运行期值怎么分」见 MeshInfoHoverCard.tsx 的文件头。 */
  const meshInfo = useHoverCard<HTMLButtonElement>();
  const hasMeshInfo = isMeshNode(server);
  // 延迟位与 ⚡ 都跟着**同一个**可测性判定走（此前跟的是 lanOnly 这条弱判定：reverseMesh 的 WG
  // 与无出口的 TS 照样显 ⚡ + 显数值，而它们要么测出直连假好值、要么必假超时）。
  const testable = speedTestable !== false;
  const level = testable ? latLevel(latencyMs) : 'none';
  const latText = !testable
    ? t('nodes.speedTestNotApplicable')
    : latencyMs === null
      ? t('nodes.timeout')
      : latencyMs === undefined
        ? ''
        : `${latencyMs} ms`;
  const latencyTip = !testable
    ? speedTestBlockedHint
    : latencyMs === null
      ? t('nodes.timeoutHint')
      : undefined;
  /* 「设为出口」那颗的文案：当前出口 / 已武装（两档） / 常态。三处（tip、aria、无障碍）同一份，
     不各写一遍——此前 data-tip 与 aria-label 逐字重复了整段三元式，改一处漏一处只是时间问题。 */
  const useTip = isCurrent
    ? t('nodes.useCurrent')
    : useConfirming
      ? useWillRestart
        ? t('nodes.useConfirmRestart')
        : t('nodes.useConfirmAgain')
      : t('nodes.use');
  const xfer = transferSummary(server);
  // 「被覆盖」角标：cidr 全列（用户要据此去重/调序），抢占者去重后列出（同一节点可能抢多段）。
  const shadowText = shadowedCidrs?.map((s) => s.cidr).join(', ') ?? '';
  const shadowBy = [...new Set(shadowedCidrs?.map((s) => s.by) ?? [])].join(', ');
  // 国旗水印：仅代理协议节点（非组网/端点协议）按名称猜中国家才显示，原型注释「mesh/globe get none」。
  const flagCode = isExit ? null : flagCodeForName(server.name);

  return (
    <div
      className={cn('nd-card', isCurrent && 'cur', useConfirming && 'confirming')}
      /* 多选态整卡 = 勾选（原有语义，优先）；否则整卡 = 设为出口。
         已是当前出口就什么都不做 —— 重选同一个节点在后端是空操作，弹一条「已切换」是噪声。 */
      onClick={() => {
        if (batchMode) onToggleSelect?.(server);
        else if (!isCurrent) onUse?.(server, 'card');
      }}
      style={!batchMode && !isCurrent && onUse ? { cursor: 'pointer' } : undefined}
      role={batchMode ? 'button' : undefined}
      aria-pressed={batchMode ? !!selected : undefined}
    >
      {flagCode && <NdFlag code={flagCode} />}

      <div className="nd-top">
        <span className={cn('nd-dot', level === 'dead' && 'dead')} />
        {/* 被启动 gate 剔除的节点：名称半透明 + tooltip 说明原因（上游 server-card.tsx:103）。
            reason 是**机器 token**（`detour-cascade` / `control-url-ip` …），必须经
            `invalidNodeReasonText` 换成对应语种的人话 —— 此前是把 token 原样拼进 tooltip，
            用户看到的是「…已在启动时跳过: detour-cascade」，后半截五语种一律看不懂，
            而它恰恰是唯一说明「为什么」的那半句。空串 / 未登记 token → 只渲染通用句。 */}
        <div
          className={cn('nd-name', invalidReason !== undefined && 'nd-invalid')}
          data-tip={
            invalidNodeReasonText(invalidReason, (k, f) => (f === undefined ? t(k) : t(k, f))) ??
            undefined
          }
        >
          {server.name}
        </div>
        {/* 不可测时把原因也挂在延迟位上（而非只挂在按钮上）：延迟位不是 `disabled`，鼠标与键盘
            都碰得到，是这条解释最可靠的落点。按钮那处见下方注释。 */}
        {latText && (
          <span className={cn('nd-lat', level)} data-tip={latencyTip}>
            {latText}
          </span>
        )}
      </div>

      <div className="nd-addr" hidden>
        {server.address ? `${server.address}:${server.port}` : ''}
      </div>

      <div className="nd-pills">
        <span className="pill proto">{protocolLabel(server.protocol)}</span>
        {stagedOnly && (
          <span
            className="nd-cap"
            data-tip={t('home.stagedOnlyHint')}
          >
            {t('home.stagedOnlyBadge')}
          </span>
        )}
        {isCurrent && <span className="nd-cur">{t('nodes.currentExit')}</span>}
        {isExit && !isCurrent && (
          <span className="nd-cap exit">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.9}>
              <path d="M9 21H5a2 2 0 01-2-2V5a2 2 0 012-2h4M16 17l5-5-5-5M21 12H9" />
            </svg>
            {t('nodes.exitCapableBadge')}
          </span>
        )}
        {/* 角标带上解释：「仅局域网」= 该组网节点没有公网出口（WG/WARP 关了「允许访问外网」/
            TS 没设 exitNode），只承载内网网段。原先这句解释挂在延迟位上且只提 WireGuard，
            现在延迟位归"为什么不能测速"用，这条归"这个角标什么意思"用，两者不再互相顶。 */}
        {lanOnly && (
          <span
            className="nd-cap lan"
            data-tip={t('nodes.lanOnlyHint')}
          >
            {t('nodes.lanOnly')}
          </span>
        )}
        {shadowedCidrs && shadowedCidrs.length > 0 && (
          <span
            className="nd-cap shadow"
            data-tip={t('nodes.shadowedHint', {
              cidrs: shadowText,
              by: shadowBy,
            })}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.9}>
              <path d="M3 3l18 18M10.6 5.1A9.9 9.9 0 0112 5c5 0 9 4.5 10 7-.5 1.3-1.6 2.9-3.2 4.2M6.2 6.3C4.3 7.7 2.9 9.6 2 12c1 2.5 5 7 10 7 1.6 0 3.1-.5 4.4-1.2" />
            </svg>
            {t('nodes.shadowed')}
          </span>
        )}
        {xfer && <span className="nd-xfer">{xfer}</span>}
        {/* 组网信息 ⓘ。放在角标行末（`margin-left:auto` 靠右）而**不是** `.nd-acts`：那一排是「对这个节点
            做点什么」，本颗只是「看看它现在什么样」，混进去会让删除/测速旁多一颗语义不同的按钮。
            也**不是**塞进 TS/WG/WARP 弹窗：那是配置面，把运行期值混进去正是本卡要避免的「填的=生效的」误读。

            `stopPropagation` 必须有：整卡 onClick 是「设为出口」/ 批选勾选，点 ⓘ 不该触发它们。
            `onFocus`/`onBlur` 是键盘腿 —— `useHoverCard` 只给鼠标 handler（另三处消费方的触发器是
            不可聚焦的 `<span>`，给不给都一样），本颗是真按钮、Tab 停得进来，故就地补上，不去改共用 hook
            （那会让 AppPolicy 那个含按钮的触发 span 因 focus 冒泡而意外弹卡）。 */}
        {hasMeshInfo && (
          <>
            <button
              type="button"
              className="nd-info-btn"
              ref={meshInfo.triggerRef}
              onMouseEnter={meshInfo.triggerHandlers.onMouseEnter}
              onMouseLeave={meshInfo.triggerHandlers.onMouseLeave}
              onFocus={meshInfo.triggerHandlers.onMouseEnter}
              onBlur={meshInfo.triggerHandlers.onMouseLeave}
              onClick={(e) => e.stopPropagation()}
              aria-label={t('nodes.meshInfoAria')}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.9}>
                <circle cx="12" cy="12" r="9" />
                <path d="M12 11v5M12 7.5h.01" />
              </svg>
            </button>
            <HoverCardPanel
              cardRef={meshInfo.cardRef}
              open={meshInfo.open}
              pos={meshInfo.pos}
              onMouseEnter={meshInfo.cardHandlers.onMouseEnter}
              onMouseLeave={meshInfo.cardHandlers.onMouseLeave}
            >
              <MeshInfoHoverCardContent server={server} />
            </HoverCardPanel>
          </>
        )}
      </div>

      {!batchMode && (
        <div className="nd-acts">
          {/* 不可测 → **置灰保留**而非整个藏起来（原状是 `!lanOnly &&` 直接不渲染）：按钮凭空少一个，
              用户只会以为这张卡坏了/布局歪了；灰着 + 说明原因才答得上「为什么这个不能测」。
              `.nd-acts` 下没有 `:disabled` 皮肤（`.rule-acts .nd-a:disabled` 是别的作用域），
              故就地给最小内联降透明度，不去动全局样式表。 */}
          <button
            type="button"
            className="nd-a speed"
            disabled={!testable}
            style={!testable ? { opacity: 0.35, cursor: 'default' } : undefined}
            onClick={(e) => {
              e.stopPropagation();
              onSpeedTest?.(server);
            }}
            data-tip={testable ? t('nodes.speedTest') : speedTestBlockedHint}
            /* 不可测时把**原因并进 aria-label**。这条**不是**原生 title 的遗留绕行，迁到引擎后依然必要：
               `disabled` 的按钮不可聚焦，引擎的键盘腿（focusin + :focus-visible）到不了它，
               `aria-describedby` 也就挂不上 ⇒ 「为什么这个不能测」对键盘/读屏用户仍然不可达
               （2026-07-28 复审 LOW #8）。视觉侧则已由引擎覆盖：`.nd-acts` 作用域没有
               `pointer-events:none`（`.rule-acts .nd-a:disabled` 是别的作用域），hover 收得到。

               真正的根治是把 `disabled` 换成 `aria-disabled` + 保持可聚焦，让引擎接管这条解释。
               **本批不做**：那会给节点列表里每张卡新增一个 Tab 停靠点，是可聚焦性/键盘遍历的
               语义变更，射程超出「tooltip 形态迁移」，需单独定夺。在那之前 aria-label 这条腿不能拆——
               拆了就是无障碍净倒退。 */
            aria-label={
              testable || !speedTestBlockedHint
                ? t('nodes.speedTest')
                : `${t('nodes.speedTest')}${t('common.colon')}${speedTestBlockedHint}`
            }
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M13 2L4 14h6l-1 8 9-12h-6z" />
            </svg>
          </button>
          <button
            type="button"
            className="nd-a"
            onClick={(e) => {
              e.stopPropagation();
              onCopy?.(server);
            }}
            data-tip={t('nodes.copyLink')}
            aria-label={t('nodes.copyLink')}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
            </svg>
          </button>
          <button
            type="button"
            className="nd-a"
            onClick={(e) => {
              e.stopPropagation();
              onClone?.(server);
            }}
            data-tip={t('nodes.clone')}
            aria-label={t('nodes.clone')}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <rect x="9" y="9" width="11" height="11" rx="2" />
              <path d="M5 15V5a2 2 0 012-2h10" />
            </svg>
          </button>
          <button
            type="button"
            className="nd-a"
            onClick={(e) => {
              e.stopPropagation();
              onEdit?.(server);
            }}
            data-tip={t('common.edit')}
            aria-label={t('common.edit')}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
            </svg>
          </button>
          {/* 设为出口。整卡点击是同一条腿的鼠标面，这颗是它的**键盘/读屏面** —— 整卡不设 tabIndex
              （见上方 speed 按钮注释：不给列表每张卡新增 Tab 停靠点），没有这颗按钮的话
              「切节点」对键盘用户在本屏就完全不可达。
              当前出口置灰而非隐藏：按钮凭空少一个只会让人以为这张卡与别的不一样。
              `useConfirming` 见 props 注释（只有会触发整核重启那一次才武装）。 */}
          {onUse && (
            <button
              type="button"
              className={cn('nd-a', useConfirming && 'confirming')}
              disabled={isCurrent}
              style={isCurrent ? { opacity: 0.35, cursor: 'default' } : undefined}
              onClick={(e) => {
                e.stopPropagation();
                onUse(server, 'button');
              }}
              data-tip={useTip}
              aria-label={useTip}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <circle cx="12" cy="12" r="9" />
                <path d="M8 12l3 3 5-6" />
              </svg>
            </button>
          )}
          {/* 删除 = 原地二次点击（原型 :4140 `node-del`）。原型这颗是图标按钮、msg 传空串 ⇒ 无文案可换，
              确认态**只**靠 `.confirming` 翻红（prototype.css `.nd-a.confirming`）。
              title/aria-label 同步换成「再点一次确认」：纯颜色状态对键盘/读屏用户不可达。
              `deletable=false`（订阅节点）时整颗不渲染，理由见 props。 */}
          {deletable && (
            <button
              type="button"
              className={cn('nd-a err', deleteConfirming && 'confirming')}
              onClick={(e) => {
                e.stopPropagation();
                onDelete?.(server);
              }}
              data-tip={deleteConfirming ? t('common.confirmAgain') : t('common.delete')}
              aria-label={
                deleteConfirming ? t('common.confirmAgain') : t('common.delete')
              }
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
              </svg>
            </button>
          )}
        </div>
      )}

      {/* 复选框本身必须可独立点击、键盘可达（原型 `:4419` 注入的是 `<button data-act="node-check">`，
          `:4144` 自带 stopPropagation）。此前是不可聚焦的 `<span role="checkbox">` —— 语义宣称是复选框、
          Tab 却停不进来，纯 a11y 回归；`components.css:961` 那条 `.nd-check:focus-visible` 描边也因此
          恒不命中（又一处死 CSS）。stopPropagation 防与整卡的 onClick 叠成「点一下切两次」。 */}
      {batchMode && (
        <button
          type="button"
          className={cn('nd-check', selected && 'on')}
          role="checkbox"
          aria-checked={!!selected}
          aria-label={server.name}
          onClick={(e) => {
            e.stopPropagation();
            onToggleSelect?.(server);
          }}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
            <path d="M5 12l5 5 9-11" />
          </svg>
        </button>
      )}
    </div>
  );
}

export default NodeCard;
