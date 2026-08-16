/**
 * TsSettingsDialog 纯逻辑 —— `TailscaleSettings` ⇄ 表单草稿的成对接线，外加出口下拉的候选构造
 * （tailnet peers → 选项行；见文件末尾 `exitNodeOptions`）。无 DOM/网络/react，可 vitest 直测。
 *
 * 与 `wg-logic.ts` / `ts-login-server.ts` 同型：把「回填」与「提交」两端成对定义在一处，
 * review 时一眼可核；配套 `ts-settings-logic.test.ts` 给下面这条纪律真牙。
 *
 * ⚠️ **本文件不算「编辑器」** —— `contracts/protocol-settings-coverage.test.ts` 那道覆盖门刻意
 * 不把它列进编辑器清单：它要的是**控件**（FieldSpec 表里那一项），不是接线。只在这里加读写、
 * 不在 `TsSettingsDialog` 的 FieldSpec 里加一项，字段仍然是「用户改不了」，门会转红。
 *
 * # 缺省即默认（本文件存在的主要理由）
 *
 * 每个键的**缺席**都有确定语义，回填必须显示那个真实缺省，提交必须**不把默认值写成显式值**：
 * 写 `false` 与「键不存在」对后端等价，但会把当下的默认值复制一份到磁盘上——日后改默认，
 * 存量配置不跟随，磁盘就成了第二个默认值真值源。`ts-login-server.ts:47` 那条注释防的就是这件事
 * （它连 `tailscaleSettings: {}` 都不肯显式写 `allowInternet: true`）。
 *
 * 缺省口径逐条对齐**消费侧谓词**，不在这里另立一套：
 *  - `alwaysRouteSubnets` 缺省 **true**：`meshAlwaysRoutesSubnets`（domain/endpoint-routes.ts:162）与
 *    Rust `mesh_always_routes_subnets` 都是 `unwrap_or(true)`；
 *  - `reverseMesh` 缺省 **false**：`meshUsesSystemInterface` / `mesh_uses_system_interface` 均 `unwrap_or(false)`；
 *  - `resolveByName` 缺省 **false**，且后端取的是 `== Some(true)`（builder/dns.rs:1214）；
 *  - `relayServerPort` 缺席 = 未设 → `undefined`（R2：number 空绝不塞 0）。
 *
 * # `allowInternet` 为什么没有控件
 *
 * Tailscale 的「是否允许作外网出口」**两侧谓词都由 `exitNode` 派生**：`domain/endpoint-routes.ts:153`
 * 与 `crates/config-engine/src/builder/endpoint_routes.rs:76` 都是 `!!exitNode`，前者注释写明
 * 「存量 `tailscaleSettings.allowInternet` 字段谓词层忽略（向后兼容、不迁移）」。给它做开关会得到一个
 * **拨了不生效的控件** + 第二个真值源。要改这个语义只能改 `exitNode`。
 */

import type { ServerConfig, TailscaleSettings } from '@/contracts/types';
import type { TailscaleStatusPeer } from '@/contracts/tailscale-status';
import type { FormValues, SelectOption } from './field-spec';
import { splitCsv } from './wg-logic';
import { detourDraftValue } from './detour-options';
import { isValidIpCidr } from '@/domain/rules';
import { controlUrlReject, type ControlUrlReject } from '@/domain/control-url';

/** 「自定义…」哨兵（与 `TsSettingsDialog` 的 `when` 谓词、`buildTsSettings` 的分支同一常量）。 */
export const EXIT_CUSTOM = '__custom__';

function str(v: unknown): string {
  return typeof v === 'string' ? v : '';
}

/** 存量设置 → 草稿（缺席回显 = 该字段真实缺省，见文件头）。 */
export function initTsDraft(node?: ServerConfig): FormValues {
  const ts = node?.tailscaleSettings;
  return {
    hostname: ts?.hostname ?? '',
    exitNode: ts?.exitNode ?? '',
    exitNodeCustom: '',
    reverseMesh: ts?.reverseMesh === true,
    alwaysRouteSubnets: ts?.alwaysRouteSubnets !== false,
    acceptRoutes: !!ts?.acceptRoutes,
    routes: (ts?.routes ?? []).join(', '),
    exitNodeAllowLanAccess: !!ts?.exitNodeAllowLanAccess,
    advertiseRoutes: (ts?.advertiseRoutes ?? []).join(', '),
    acceptDefaultResolvers: !!ts?.acceptDefaultResolvers,
    controlUrl: ts?.controlUrl ?? '',
    advertiseTags: (ts?.advertiseTags ?? []).join(', '),
    ephemeral: !!ts?.ephemeral,
    listenPort: ts?.listenPort,
    relayServerPort: ts?.relayServerPort,
    sshServer: !!ts?.sshServer,
    resolveByName: ts?.resolveByName === true,
    // detour 在 `ServerConfig` **顶层**，不在 `tailscaleSettings` 里 —— 故取 `node` 而非 `ts`，
    // 提交侧也另走 `applyDetour`（`buildTsSettings` 只管 settings 那一层）。
    detour: detourDraftValue(node),
  };
}

/**
 * 提交前的 CIDR 校验：返回**非法项**（全合法 → 空数组）。
 *
 * 口径与后端 `crates/store/src/sanitize.rs:303` 的 `sanitize_cidr_list` 对齐（那里同样只作用于
 * `routes` / `advertiseRoutes`），判据同源用 `domain/rules.ts` 的 `isValidIpCidr` —— 它就是 Rust
 * `rule_validate.rs:107` `is_valid_ip_cidr` 的前端孪生。
 *
 * 为什么必须前端拦：后端对非法项是**静默丢弃**（保留合法的、丢掉坏的，不报错）。前端不拦，用户就会
 * 遇到「界面收下了、盘上没有」——既没有报错也没有回显，无从得知自己填错了。
 */
export function invalidTsCidrs(draft: FormValues): string[] {
  return [...splitCsv(draft.routes), ...splitCsv(draft.advertiseRoutes)].filter(
    (c) => !isValidIpCidr(c)
  );
}

/**
 * 草稿态的 control_url 非法判定 —— 合法返 `null`，否则返 reason token
 * （token 经 `INVALID_NODE_REASON_KEY` 换成人话，语义见 `domain/control-url.ts` 头注）。
 *
 * **谓词放这里而不是让弹窗直接调 `controlUrlReject`**：与 [`invalidTsCidrs`] 同形 —— 弹窗只认
 * 「这份草稿哪里不合法」，不该关心字段怎么从 `FormValues` 里取出来。`str` 是本模块私有的取值
 * 归一化（`unknown → string`），外泄它等于把草稿的内部表示漏给调用方。
 */
export function invalidControlUrl(draft: FormValues): ControlUrlReject | null {
  return controlUrlReject(str(draft.controlUrl));
}

/**
 * 草稿 → `TailscaleSettings`（提交路径）。
 *
 * `base` = 该节点现有的 `tailscaleSettings`，用于**保全未建模字段**（如 `authKey`——它由
 * `TsLoginDialog` 写入，本弹窗只能原样带过，绝不能覆写掉）。
 */
export function buildTsSettings(
  base: TailscaleSettings | undefined,
  draft: FormValues
): TailscaleSettings {
  const next: TailscaleSettings = { ...(base ?? {}) };
  next.hostname = str(draft.hostname).trim() || undefined;
  const exit =
    draft.exitNode === EXIT_CUSTOM ? str(draft.exitNodeCustom).trim() : str(draft.exitNode);
  next.exitNode = exit || undefined;
  next.alwaysRouteSubnets = draft.alwaysRouteSubnets !== false;
  const routes = splitCsv(draft.routes);
  // 上游 `tailscale-form.tsx:127` 同款派生：填了 routes 却不 acceptRoutes，tsnet 根本不接收这些
  // advertised 子网 ⇒ 路由白配。开关的 hint 已写明「填写下方网段时自动开启」，不是静默改写。
  next.acceptRoutes = !!draft.acceptRoutes || routes.length > 0;
  next.exitNodeAllowLanAccess = !!draft.exitNodeAllowLanAccess;
  const adv = splitCsv(draft.advertiseRoutes);
  next.advertiseRoutes = adv.length ? adv : undefined;
  next.controlUrl = str(draft.controlUrl).trim() || undefined;
  const tags = splitCsv(draft.advertiseTags);
  next.advertiseTags = tags.length ? tags : undefined;
  next.ephemeral = !!draft.ephemeral;
  next.sshServer = !!draft.sshServer;

  // ── 以下四项：缺省即默认，用户没开就**删键**而不是写 false/0/[]（见文件头）。
  // 上面那批既有字段沿用原写法（无条件写显式值），本轮不动：改它们会改到既有节点的落盘形状，
  // 属独立一批，且值与默认等价、无行为差。
  if (routes.length) next.routes = routes;
  else delete next.routes;
  if (draft.reverseMesh === true) next.reverseMesh = true;
  else delete next.reverseMesh;
  if (draft.resolveByName === true) next.resolveByName = true;
  else delete next.resolveByName;
  // acceptDefaultResolvers 只在 resolveByName 为真的分支里被后端读到（builder/dns.rs:1069）。
  // 关掉按名解析后不留残值，免得磁盘上躺着一个永不生效的 true。
  if (draft.resolveByName === true && draft.acceptDefaultResolvers === true)
    next.acceptDefaultResolvers = true;
  else delete next.acceptDefaultResolvers;
  // 两个端口字段共用 `portOrUndefined` 的判据，落盘写法同上一段：合法才写，否则删键。
  const listen = portOrUndefined(draft.listenPort);
  if (listen !== undefined) next.listenPort = listen;
  else delete next.listenPort;
  const relay = portOrUndefined(draft.relayServerPort);
  if (relay !== undefined) next.relayServerPort = relay;
  else delete next.relayServerPort;
  return next;
}

/**
 * u16 端口的提交口径：合法（整数 1..=65535）返回该值，否则 `undefined`（= 调用方删键）。
 *
 * 抽成一处而不是每个字段各写一遍：越界的代价与「这个字段没生效」不是一个量级 —— `Option<u16>`
 * 装不下的值会让**整份 UserConfig 反序列化失败**（同 `server_config.rs:208` 记的那类整机不可用）。
 * 两个端口字段各写一份判据，迟早漂掉其中一份，而漂掉的那份没有任何 UI 反馈。
 *
 * 后端两处都是 `if p > 0` 才下发（`builder/endpoints.rs`）⇒ `0` 与空在两侧同为「未设」。
 */
function portOrUndefined(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isInteger(v) && v >= 1 && v <= 65535 ? v : undefined;
}

// ════════════════════════════════════════════════════════════════════════════
// 出口节点下拉的候选构造
// ════════════════════════════════════════════════════════════════════════════

/**
 * 行文案（i18n 已译入，**由组件传入**）—— 本模块因此保持纯：无 react / 无 i18next 运行时依赖，
 * 也不会往这个文件里落裸中文（`i18n-coverage` 的 G1 对未登记文件要求零裸 CJK）。
 */
export interface ExitNodeLabels {
  none: string;
  custom: string;
  inUse: string;
  offline: string;
  notAdvertised: string;
}

/** 配置值命中该 peer 吗 —— ip 或 hostName 双匹配：sing-box `exit_node` 两种写法都合法。 */
function peerMatches(peer: TailscaleStatusPeer, saved: string): boolean {
  if (!saved) return false; // 空值不许命中「ip 也为空」的 peer（否则那台会被白白豁免掉禁用）
  return peer.ip === saved || peer.hostName === saved;
}

/**
 * 是否禁用：**非广告出口 且 非当前已配置项**。当前配置项恒豁免 —— 保证已配置行始终可见可选，
 * 不因对端一时未广告出口（离线 / 刚重启 / 临时关了 `--advertise-exit-node`）就无法回显与重选。
 * 判据用**已保存值**而非草稿值：草稿值会随用户点选变动，用它的话「选走了就再也选不回来」。
 */
function peerDisabled(peer: TailscaleStatusPeer, savedExit: string): boolean {
  return !peer.exitNodeOption && !peerMatches(peer, savedExit);
}

/**
 * 行文案：`hostName · ip · [使用中] · [离线] · [未广告出口]`。
 *
 * 三个状态**独立叠加**不是互斥分支 —— 一台机器可以同时「离线 · 未广告出口」。
 * `Csel` 的 `CselOption` 没有 `note` 字段，而本仓本来就把 `hostName · ip` 拼进 label，
 * 状态顺着同一条 label 走即可，不必为此给原语加字段。
 */
function peerLabel(peer: TailscaleStatusPeer, labels: ExitNodeLabels): string {
  const parts = [peer.hostName, peer.ip].filter(Boolean);
  if (peer.exitNode) parts.push(labels.inUse);
  if (!peer.online) parts.push(labels.offline);
  if (!peer.exitNodeOption) parts.push(labels.notAdvertised);
  return parts.join(' · ');
}

/** 排序：可作出口优先 → 在线优先 → 名称。不排的话可用出口会被埋在一堆禁用行中间。 */
function compareExitPeers(a: TailscaleStatusPeer, b: TailscaleStatusPeer): number {
  if (a.exitNodeOption !== b.exitNodeOption) return a.exitNodeOption ? -1 : 1;
  if (a.online !== b.online) return a.online ? -1 : 1;
  return (a.hostName || a.ip).localeCompare(b.hostName || b.ip);
}

/**
 * tailnet peers → 出口下拉候选 `[无, ...设备, (已保存值), 自定义…]`。
 *
 * # 列全部 peer，不再过滤
 *
 * 此前只留 `exitNodeOption` 的 peer，于是「对端就在 tailnet 里、只是没开 `--advertise-exit-node`」
 * 与「对端根本不存在」在界面上**长得一模一样**（都是不在列表里），用户无从判断该去哪台机器上开开关。
 * 现在全部列出，不可用的**显示但禁用**并注明原因 —— 与 上游 `exit-node-field-logic.ts` 同口径。
 *
 * # 去重按「选项值」，不按 hostName
 *
 * `Csel` 的选中态、键盘索引、回填✓ 全靠 `value` 唯一（`Csel.tsx:129` 的 `findIndex`）——
 * 重复值会让第二行永远选不中且两行同时打勾。故去重的判据必须**就是选项值本身**：
 * 先排序再逐个取值，撞了就丢后来的 ⇒ 同名多台时留下的是「可作出口 / 在线」那一台（排序已保证），
 * 而不是 `flatMap` 顺序里碰巧排前面的那台。
 * （选项值取 `hostName || ip`：写盘的是它，沿用既有口径不改磁盘形状；hostName 缺失才退到 ip，
 * 两者皆空的 peer 无法寻址 ⇒ 只能丢，否则它会造出一个 `value===''` 的行与「无」撞车。）
 *
 * # 已保存值恒可寻址
 *
 * 已保存值命中某个 peer（ip 或 hostName）⇒ 该行的选项值直接取已保存值，回显✓ 落在设备行上、
 * 不另生一行重复项。一个都命不中（核没跑 ⇒ peers 全空 / 该机已退出 tailnet / 手填了个 IP）
 * ⇒ 末尾补一行原样值 —— 禁用豁免只能救「在列表里的行」，救不了**空列表**，两者覆盖的是不同情形。
 */
export function exitNodeOptions(
  peers: readonly TailscaleStatusPeer[],
  savedExit: string,
  labels: ExitNodeLabels
): SelectOption[] {
  const saved = savedExit === EXIT_CUSTOM ? '' : savedExit;
  const seen = new Set<string>();
  const rows: SelectOption[] = [];
  for (const p of [...peers].sort(compareExitPeers)) {
    const value = peerMatches(p, saved) ? saved : p.hostName || p.ip;
    if (!value || seen.has(value)) continue;
    seen.add(value);
    rows.push([value, peerLabel(p, labels), peerDisabled(p, saved)]);
  }
  return [
    ['', labels.none],
    ...rows,
    ...(saved && !seen.has(saved) ? ([[saved, saved]] as SelectOption[]) : []),
    [EXIT_CUSTOM, labels.custom],
  ];
}
