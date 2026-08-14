/**
 * MeshInfoHoverCard —— 组网节点（Tailscale / WireGuard / WARP）的**信息面**：内网地址 / 路由 / 出口 / 对端。
 *
 * # 为什么需要它
 *
 * 组网节点在本仓此前**只有配置态**（弹窗里填的那些），运行期真值一个都看不见：tailnet 下发的内网 IP、
 * 真正生效的出口对端、对端在不在线。用户要知道「我到底拿到了哪个地址」只能去 Tailscale 控制台看。
 * 上游 早有此面（`src/renderer/components/settings/mesh-info-popover.tsx`，卡片角标行末尾的 ⓘ），
 * 本卡是它在 Polaris 的对应物。
 *
 * # 配置值与运行期值必须**视觉可分**（本卡的核心约束）
 *
 * 用户填的 `exitNode` 与 tailnet 上真正生效的出口可以不同；填的 `routes` 与实际拿到的地址也不是一回事。
 * 混在一张表里列出来，读者会默认「填的就是生效的」——那比不显示更坏。故本卡分两段：
 *
 *   ┌ 运行期（绿点 = 流是活的 / 琥珀点 + 「上次已知」= 核已停、这是缓存末帧）
 *   │   值取自 sing-box 管理 API `SubscribeTailscaleStatus` 的末帧快照
 *   ├─ 分隔线（`.tc-sep`，与规则 hover 卡同一条）
 *   └ 配置（用户填的值，恒可得、恒不陈旧）
 *
 * 两段各有段头、值的字重/色阶不同（`.mi-v.live` / `.mi-v.stale` / `.mi-v.cfg`，见 styles/index.css）。
 *
 * # 拿不到的字段一行都不画
 *
 * WireGuard / WARP **没有任何运行期数据源**：sing-box 1.14 管理 API 的全部 RPC 见
 * `crates/singbox-grpc/proto/started_service.proto` —— 只有 Tailscale 三条（Subscribe/Logout/SetExitNode）、
 * 状态流、连接流与出站选择，**没有 WireGuard 的 peer / 握手 / 最近握手时间**。于是 WG/WARP 卡
 * 只有「配置」一段，不摆一个恒为「—」的「上次握手」位。同理 `peers` 为空的那一帧不画对端行。
 *
 * # 数据怎么来
 *
 * 卡片挂载时（= 用户已 hover 满 500ms）拉一次 `TAILSCALE_GET_STATUS` 缓存末帧 + 新鲜度（`connected`）。
 * **刻意不订阅事件流、也不进 store**：本卡的生命周期以秒计，一次本地 invoke（读后端一个已有的缓存）
 * 比常驻订阅 + store 字段便宜得多，也免去与并行改动 `tailscale_status.rs` / TS 认证态那一批的接触面。
 * 拉不到（非 Tauri / 后端未就绪）→ 无运行期段，配置段照常渲染。
 *
 * 挂载/定位/延迟由调用方（`NodeCard.tsx`）用 `useHoverCard()` + `<HoverCardPanel>` 组装，同本目录另三卡。
 */
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ServerConfig } from '@/contracts/types';
import type { TailscaleStatusEvent, TailscaleStatusSnapshot } from '@/contracts/tailscale-status';
import { dedupeTrim } from '@/domain/collections';
import { isAccountBasedProtocol, isMeshNode, stripCatchAll } from '@/domain/endpoint-routes';
import { api } from '@/ipc/api-client';
import { cn } from '@/lib/utils';

/** 行标识。命名不带段前缀 —— 同一个 id（`routes`）在两段里语义不同由**所在段**表达，不靠键名重复说一遍。 */
export type MeshInfoRowId =
  // 运行期段（仅 Tailscale）
  | 'intranetIp'
  | 'activeExit'
  | 'peers'
  // 配置段
  | 'localAddress'
  | 'routes'
  | 'exitNode'
  | 'acceptRoutes';

/** 一行信息。`values` 为空 = **有数据源但此刻无值**（渲染成该行的占位文案），不是「没有数据源」。 */
export interface MeshInfoRow {
  id: MeshInfoRowId;
  values: string[];
}

/**
 * 运行期行 —— **只有 Tailscale 有源**（见文件头「拿不到的字段一行都不画」）。
 *
 * `status` 缺席（没拉到 / 该节点不在快照里）→ 返回空数组，调用方整段不画。
 */
export function meshRuntimeRows(
  server: Pick<ServerConfig, 'protocol'>,
  status: TailscaleStatusEvent | undefined,
): MeshInfoRow[] {
  if (!isAccountBasedProtocol(server.protocol) || !status) return [];
  const rows: MeshInfoRow[] = [
    // 内网 IP：tailnet 下发（100.x / fd7a:…），**没有任何配置项与之对应** —— 这正是「配置里看不到」的那一格。
    { id: 'intranetIp', values: status.tailscaleIPs },
    // 生效出口：peers 里 `exitNode=true` 的那台。与配置段的「出口节点」并排看，一眼分辨填的有没有生效。
    { id: 'activeExit', values: pickActiveExit(status) },
  ];
  // 对端在线数。`peers` 空 = 这一帧根本没带对端信息（未登录 / 刚起核），画「0/0」会被读成「一台都没在线」，
  // 那是两回事 —— 故整行不画。
  if (status.peers.length > 0) {
    const online = status.peers.filter((p) => p.online).length;
    rows.push({ id: 'peers', values: [`${online}/${status.peers.length}`] });
  }
  return rows;
}

/** 当前生效的出口对端（`exitNode=true`）→ `主机名 · IP`；没有则空数组（渲染成「无」）。 */
function pickActiveExit(status: TailscaleStatusEvent): string[] {
  const active = status.peers.find((p) => p.exitNode);
  if (!active) return [];
  return [active.ip ? `${active.hostName} · ${active.ip}` : active.hostName];
}

/** 配置行 —— 三协议都有（用户填的值，恒可得）。非组网节点返回空数组。 */
export function meshConfigRows(server: ServerConfig): MeshInfoRow[] {
  if (!isMeshNode(server)) return [];
  if (isAccountBasedProtocol(server.protocol)) {
    const ts = server.tailscaleSettings;
    const rows: MeshInfoRow[] = [
      // routes（把这些段送进本节点）∪ advertiseRoutes（本机对外广告的段）。纯展示，与 force-route 计算解耦
      // （口径同 上游 mesh-info-popover.tsx:33-40）。
      { id: 'routes', values: dedupeTrim([...(ts?.routes ?? []), ...(ts?.advertiseRoutes ?? [])]) },
      { id: 'exitNode', values: dedupeTrim([ts?.exitNode ?? '']) },
    ];
    // 布尔项只在**开着**时入列：关着是缺省态，摆一行「未开启」是噪声。它的值文案见 ROW_EMPTY_TEXT。
    if (ts?.acceptRoutes) rows.push({ id: 'acceptRoutes', values: [] });
    return rows;
  }
  const wg = server.wireguardSettings;
  return [
    { id: 'localAddress', values: dedupeTrim(wg?.localAddress ?? []) },
    // 只列**具体**段：`0.0.0.0/0` / `::/0` 由 `allowInternet` 接管，卡面已有「仅局域网」角标表达它，
    // 在这里再画一次 catch-all 会被读成一条独立配置。
    { id: 'routes', values: stripCatchAll(wg?.allowedIPs) },
  ];
}

/**
 * 行标签的 i18n 键。穷举 Record —— 新增行 id 而漏配标签会被类型检查挡下。
 *
 * **刻意不带中文兜底**（本仓另一惯例 `t('k', '默认')` 在此不适用）：这是一张查表，兜底文案只能写成
 * 表里的裸字面量，那既会被 `i18n/i18n-coverage.test.ts` 的 G1 判为裸 CJK（`REF_KIND_DEFAULT` 那类
 * 存量正挂在债务表上），又会与 `locales/zh-CN.json` 里的真值各自漂移。五份 locale 已同批补齐，
 * 缺键会被 `locale-parity.test.ts` 抓住 —— 兜底在这里只是第二份会过期的真值。
 */
const ROW_LABEL: Record<MeshInfoRowId, string> = {
  intranetIp: 'nodes.meshInfoIntranetIp',
  activeExit: 'nodes.meshInfoActiveExit',
  peers: 'nodes.meshInfoPeers',
  localAddress: 'nodes.meshInfoLocalAddress',
  routes: 'nodes.meshInfoRoutes',
  exitNode: 'nodes.meshInfoExitNode',
  acceptRoutes: 'nodes.meshInfoAcceptRoutes',
};

/**
 * `values` 为空时那一格显示什么。同样穷举，理由同上。
 *
 * 两处不是「占位」而是**正文**：
 *  - `acceptRoutes` 恒空 values（它只在开着时入列）⇒ `meshInfoOn`（「已开启」）就是它唯一的值；
 *  - `peers` 由构造保证非空（0 对端时整行不画）⇒ 该条永不被取用，留着只为让 Record 穷举生效。
 */
const ROW_EMPTY_TEXT: Record<MeshInfoRowId, string> = {
  intranetIp: 'nodes.meshInfoNotAssigned',
  activeExit: 'nodes.meshInfoNone',
  peers: 'nodes.meshInfoNone',
  localAddress: 'nodes.meshInfoNone',
  routes: 'nodes.meshInfoNone',
  exitNode: 'nodes.meshInfoNone',
  acceptRoutes: 'nodes.meshInfoOn',
};

/** 值的色阶 = 这一行属于哪一段：活的运行期值 / 陈旧运行期值 / 配置值。三档在深浅两档主题下都用 token。 */
type RowTone = 'live' | 'stale' | 'cfg';

function InfoRow({ row, tone }: { row: MeshInfoRow; tone: RowTone }) {
  const { t } = useTranslation();
  return (
    <div className="mi-row">
      <span className="mi-k">{t(ROW_LABEL[row.id])}</span>
      <span className={cn('mi-v mono', tone)}>
        {row.values.length > 0 ? row.values.join(', ') : t(ROW_EMPTY_TEXT[row.id])}
      </span>
    </div>
  );
}

/**
 * 纯呈现层（无 IPC，可离线渲染断言）。
 *
 * `live`：`true`=状态流是活的；`false`=核已停、这是缓存末帧（标「上次已知」）；`undefined`=快照还没到
 * （首帧或非 Tauri）——此时 `status` 必然也是 undefined，运行期段整段不画，不会出现「有值但不知新鲜度」。
 */
export function MeshInfoRows({
  server,
  status,
  live,
}: {
  server: ServerConfig;
  status?: TailscaleStatusEvent;
  live?: boolean;
}) {
  const { t } = useTranslation();
  const runtime = meshRuntimeRows(server, status);
  const config = meshConfigRows(server);
  const stale = live === false;
  return (
    <>
      {runtime.length > 0 && (
        <>
          <div className="mi-hd">
            <span className={cn('mi-dot', stale ? 'stale' : 'live')} />
            {t('nodes.meshInfoRuntime')}
            {stale && <span className="mi-stale">{t('nodes.meshInfoStale')}</span>}
          </div>
          {runtime.map((row) => (
            <InfoRow key={row.id} row={row} tone={stale ? 'stale' : 'live'} />
          ))}
          <div className="tc-sep" />
        </>
      )}
      <div className="mi-hd">{t('nodes.meshInfoConfig')}</div>
      {config.map((row) => (
        <InfoRow key={row.id} row={row} tone="cfg" />
      ))}
    </>
  );
}

/** 带取数的卡片内容 —— `<HoverCardPanel>` 的 children。 */
export function MeshInfoHoverCardContent({ server }: { server: ServerConfig }) {
  const [snap, setSnap] = useState<TailscaleStatusSnapshot | null>(null);
  // 只有 Tailscale 有运行期源；WG/WARP 连这次 invoke 都不发。
  const wantsRuntime = isAccountBasedProtocol(server.protocol);
  useEffect(() => {
    if (!wantsRuntime) return;
    let cancelled = false;
    api.server
      .tailscaleGetStatus()
      .then((s) => {
        if (!cancelled) setSnap(s);
      })
      .catch(() => {
        /* 非 Tauri / 后端未就绪 → 无运行期段，配置段照常。不弹错：这是信息面，不是操作。 */
      });
    return () => {
      cancelled = true;
    };
  }, [wantsRuntime]);
  return (
    <MeshInfoRows
      server={server}
      status={snap?.statuses.find((s) => s.serverId === server.id)}
      live={snap?.connected}
    />
  );
}
