/**
 * 节点表单数据表 ND_SPEC —— 1:1 移植原型 `polaris-prototype.html` :3658-3696 起步的 8 协议，
 * 后补全其余代理协议与 OpenConnect/OpenVPN 组网隧道（后端
 * `crates/config-engine/builder/outbound.rs` + `crates/store/validate.rs#ALLOWED_PROTOCOLS` 为能力真值源）。
 * wireguard/tailscale 不在此清单——二者已有专属弹窗（`WgDialog`/`TsSettingsDialog`，`dialog-store`
 * kind `wg`/`ts-settings`），故意不重复建模（见 NodeDialog 顶部注释的已知缺口）。
 *
 * 17 协议的字段描述符仍以 cred/adv 维持 codec 真值；展示层由 `nodeFormGroups` 统一分成任务分组。
 * 这是**节点特定**数据，与 `field-spec.tsx` 的通用渲染器解耦（渲染器不认识协议）。
 *
 * 条件显隐（原型 `ndSyncCond` :3707）：安全层 sec 驱动 tls/reality 字段——
 *  - `whenTls`：sec ∈ {tls, reality} 时显（tls 字段在 reality 下也需要，如 SNI）；
 *  - `whenReality`：sec === reality 时显（Reality 公钥 / Short ID）。
 *  - `whenSnellV4`/`whenSnellV6`/`whenSnellObfsHttp`：Snell version 驱动 obfs（v4）/ mode（v6）互斥分支。
 *
 * i18n：本表**只写键、不写 zh 缺省**（2026-08-07）。99 个 `node.field.*` 键已五语齐备（`0b0c186` 铺 95 条，本批拆标签再添 4 条 hint），
 * 再留一份 `zh:` 就是第二个真值源 —— 而它与 locale 谁在生效**在界面上看不出来**：`t(key, zh)` 只在
 * 键缺失时才回落 zh，于是「locale 里译错了一条」与「locale 里漏了一条」渲染出的东西都是「看起来正常」，
 * 后者还会把 5 个语种一起压回中文（这正是这批键补进 locale 之前的实况）。删掉之后漏键会渲染成裸键名
 * （实测 i18next 23.16.8：`t('node.field.sni')` → `"node.field.sni"`），且由
 * `i18n/i18n-coverage.test.ts` 的 **G5a/G5b** 在 CI 先拦下 —— 那道门是与本次删除**同批**立的，
 * 顺序刻意是「先立门（当时全绿）→ 再删缺省」，这样漏了哪条会立刻红而不是等到真机。
 * select 的**选项文案**不在此列：多为专有名词（TCP/xtls-rprx-vision/…）直接字面量，不入 i18n。
 */

import type { FieldSpec, FormValues } from './field-spec';

/** 节点表单支持的 17 协议（不含 wireguard/tailscale，见上方文件头注释）。 */
export type NodeProto =
  | 'vless'
  | 'vmess'
  | 'trojan'
  | 'shadowsocks'
  | 'hysteria2'
  | 'tuic'
  | 'socks'
  | 'http'
  | 'anytls'
  | 'naive'
  | 'snell'
  | 'ssh'
  | 'hysteria'
  | 'tor'
  | 'openconnect'
  | 'openvpn-client'
  | 'custom';

/** 协议下拉选项（value = NodeProto，label = 展示名）。 */
export const PROTO_OPTIONS: readonly (readonly [NodeProto, string])[] = [
  ['vless', 'VLESS'],
  ['vmess', 'VMess'],
  ['trojan', 'Trojan'],
  ['shadowsocks', 'Shadowsocks'],
  ['hysteria2', 'Hysteria2'],
  ['tuic', 'TUIC'],
  ['socks', 'SOCKS'],
  ['http', 'HTTP'],
  ['anytls', 'AnyTLS'],
  ['naive', 'NaiveProxy'],
  ['snell', 'Snell'],
  ['ssh', 'SSH'],
  ['hysteria', 'Hysteria v1'],
  ['tor', 'Tor'],
  ['openconnect', 'OpenConnect'],
  ['openvpn-client', 'OpenVPN'],
  ['custom', 'Custom'],
];

/**
 * 普通代理入口的分组。组网接入不再伪装成普通协议组：OpenConnect / OpenVPN 只由
 * `MESH_TUNNEL_NODE_PROTOCOLS` 提供给组网入口，避免入口重复和已经失真的旧分类判据。
 *
 * 所有组内都按用户看到的展示名排序；“常用”只表达成员归属，不再暗含一套难以维护的主观次序。
 */
export const PROTO_GROUP_ORDER = ['common', 'proxy', 'custom'] as const;
export type ProtoGroupId = (typeof PROTO_GROUP_ORDER)[number];

/** 常用代理；NaiveProxy 与其它主流订阅协议同组。 */
const COMMON: readonly NodeProto[] = [
  'vless',
  'vmess',
  'trojan',
  'shadowsocks',
  'hysteria2',
  'tuic',
  'anytls',
  'naive',
];

/** 普通入口的其它代理。显式列举，避免把新的组网 endpoint 误吸进普通节点下拉。 */
const PROXY: readonly NodeProto[] = ['socks', 'http', 'snell', 'ssh', 'hysteria', 'tor'];

/** 组网弹窗中由 NodeDialog 承载的隧道接入。WireGuard 有自己的专用弹窗。 */
export const MESH_TUNNEL_NODE_PROTOCOLS = ['openconnect', 'openvpn-client'] as const satisfies readonly NodeProto[];

/** 仅用于选择正确的表单入口，不代表后端协议类型。 */
export function isMeshTunnelNodeProtocol(proto: NodeProto): boolean {
  return MESH_TUNNEL_NODE_PROTOCOLS.some((candidate) => candidate === proto);
}

const PROTOCOLS_BY_GROUP: Readonly<Record<ProtoGroupId, readonly NodeProto[]>> = {
  common: COMMON,
  proxy: PROXY,
  custom: ['custom'],
};

const PROTO_LABEL = new Map<NodeProto, string>(PROTO_OPTIONS);

function sortByDisplayName(protocols: readonly NodeProto[]): NodeProto[] {
  return [...protocols].sort((a, b) =>
    (PROTO_LABEL.get(a) ?? a).localeCompare(PROTO_LABEL.get(b) ?? b, 'en', { sensitivity: 'base' })
  );
}

/** 普通入口某组的协议，统一按展示名排序。 */
export function protosInGroup(g: ProtoGroupId): NodeProto[] {
  return sortByDisplayName(PROTOCOLS_BY_GROUP[g]);
}

/** 组网隧道选项也按展示名排序。 */
export function meshTunnelNodeProtocols(): NodeProto[] {
  return sortByDisplayName(MESH_TUNNEL_NODE_PROTOCOLS);
}

/** 端口占位符（原型 ndRenderFields :3716：ss=8388 / socks=1080 / http=8080 / ssh=22 / 其余=443）。 */
export function defaultPortPlaceholder(proto: NodeProto): string {
  return { shadowsocks: '8388', socks: '1080', http: '8080', ssh: '22' }[proto as string] ?? '443';
}

// 传输方式（原型 O_NET :3655）；uTLS 指纹（O_FP :3656）。
const O_NET: readonly (readonly [string, string])[] = [
  ['tcp', 'TCP'],
  ['ws', 'WebSocket'],
  ['grpc', 'gRPC'],
  ['httpupgrade', 'HTTPUpgrade'],
  ['http', 'HTTP/2'],
];
const O_FP: readonly (readonly [string, string])[] = [
  ['chrome', 'Chrome'],
  ['firefox', 'Firefox'],
  ['safari', 'Safari'],
  ['edge', 'Edge'],
  ['random', 'random'],
];
/**
 * uTLS 指纹选项 **带「不启用」首项** —— vmess / trojan 专用，与 vless/anytls 的 `O_FP` 分开是因为
 * **后端缺省值按协议不同**（`builder/outbound.rs` 的 `final_fp`）：缺 `fingerprint` 时 vless/anytls
 * 回落 `chrome`，其余协议回落 `none`，而 `final_fp === 'none'` 时整个 `tls.utls` 块不下发。
 * 若这两个协议直接复用 `O_FP`，`draftFromSpecs` 会把首项 `chrome` 当默认值 seed 进草稿 ⇒ 新建的
 * vmess/trojan 节点凭空多出 `utls: {enabled:true, fingerprint:"chrome"}`，与今天的生成结果不同。
 * 空串首项 = 「不下发该键」= 后端缺省 `none`（同 vless 的 flow / hy2 的 bbr 的既有写法）。
 */
const O_FP_OPT: readonly (readonly [string, string])[] = [['', 'none'], ...O_FP];

/**
 * TLS 栈引擎档位（`tls.engine`）—— **首项必须是空串**。
 *
 * 后端 `builder/outbound_helpers.rs::should_emit_tls_engine` 只在
 * `("windows", platform=="win32")` / `("apple", platform=="darwin")` 两种组合下才回 true，
 * 其余（含 `"go"`、`None`）一律**不下发该键** ⇒ 空串与显式 `go` 逐字节同结果，故只给空串一档，
 * 不给一个写进磁盘却永远不生效的 `go`（上游 `http-form.tsx:97` 也是 `!== 'go'` 才写）。
 *
 * **与 上游的一处刻意分歧**：上游 `tls-fields.tsx:TlsEngineField` 按 `window.electron.platform`
 * 隐藏非本平台的两档；Polaris 的 `ND_SPEC` 是**模块级常量**，求值发生在任何 platform 探测之前
 * （`AppShell` 的 `plugin:os|platform` 是 effect 里的异步 invoke），拿不到同步平台值 ⇒ 三档全给。
 * 安全性不依赖这道 UI 门：跨平台选错档时后端**不下发**，不会造出会 FATAL 的配置。
 * 取值集与随包核 beta.7 schema 的 `OutboundTLSOptions.engine` enum（go/apple/windows）一致。
 */
const O_TLS_ENGINE: readonly (readonly [string, string])[] = [
  ['', 'go'],
  ['windows', 'Windows (Schannel)'],
  ['apple', 'Apple (Network.framework)'],
];

/**
 * TLS spoof 方法档位 —— **首项空串 = 不启用**。
 *
 * 取值集**必须与后端 `user_config/tls_spoof.rs::TLS_SPOOF_METHODS` 完全一致**（三档），
 * 不是照抄内核 schema：随包 beta.7 的 `spoof_method` enum 其实有 5 档
 * （多 `wrong-sequence`/`wrong-checksum`），但 `validate_tls_spoof_default` 只放行这三个
 * ⇒ 多给的两档在本客户端里是**选了必然不下发**的假档位。窄的一侧是权威。
 */
const O_SPOOF_METHOD: readonly (readonly [string, string])[] = [
  ['', 'none'],
  ['wrong-ack', 'wrong-ack'],
  ['wrong-md5', 'wrong-md5'],
  ['wrong-timestamp', 'wrong-timestamp'],
];

/**
 * Multiplex 协议档位。**首项 `h2mux` 是安全的默认 seed**（不同于 fp 那次的 chrome 陷阱）：
 * 整组 multiplex 字段都在 `mux` 开关（`switch`，默认 false）之下，`toConfig` 只在开关打开时
 * 才装配 `multiplexSettings` ⇒ 被 seed 的 `muxProto` 在开关关闭时根本不会写进 config。
 * 而开关打开时，后端 `mux.protocol.unwrap_or("h2mux")` 的缺省恰是 `h2mux`，显式写与不写同结果。
 * 取值集 = 随包核 beta.7 schema 的 `OutboundMultiplexOptions.protocol` enum。
 */
const O_MUX_PROTO: readonly (readonly [string, string])[] = [
  ['h2mux', 'h2mux'],
  ['smux', 'smux'],
  ['yamux', 'yamux'],
];

/**
 * Shadowsocks 加密方式 —— **内核认的全集 18 档**（2026-08-06 裁定：口径与「遵循订阅下发」一致，
 * 内核认什么就给什么，不替用户收窄）。
 *
 * 取值集**以随包核 beta.7 的 JSON Schema 为准**（`sing-box schema` →
 * `$defs/Outbound/oneOf[*]` 里 type=shadowsocks 那支的 `method` enum），不是照抄 上游
 * （它 `ss-form.tsx:COMMON_METHODS` 只有 15 档：缺 `aes-192-gcm`/`xchacha20`/`none`，
 * 且含一个内核 enum 里没有的写法差异）。
 *
 * **首项必须仍是 `2022-blake3-aes-128-gcm`**：它是今天 `draftFromSpecs` 的默认 seed 与
 * `fromConfig` 的回落值，换掉会让新建 ss 节点的默认加密方式静默改变。其余按「2022 系 → AEAD →
 * 流式/遗留」排，`none`（无加密）压在最后。
 *
 * `da97add` 的「保留表外当前值」（`toCselOptions`）仍在，管的是**内核 enum 之外**的存量值
 * （如机场下发的 `salsa20`）；本表管的是让用户能**主动选**内核认的每一档。
 */
const O_SS_METHOD: readonly (readonly [string, string])[] = [
  ['2022-blake3-aes-128-gcm', '2022-blake3-aes-128-gcm'],
  ['2022-blake3-aes-256-gcm', '2022-blake3-aes-256-gcm'],
  ['2022-blake3-chacha20-poly1305', '2022-blake3-chacha20-poly1305'],
  ['aes-256-gcm', 'aes-256-gcm'],
  ['aes-192-gcm', 'aes-192-gcm'],
  ['aes-128-gcm', 'aes-128-gcm'],
  ['chacha20-ietf-poly1305', 'chacha20-ietf-poly1305'],
  ['xchacha20-ietf-poly1305', 'xchacha20-ietf-poly1305'],
  ['aes-256-ctr', 'aes-256-ctr'],
  ['aes-192-ctr', 'aes-192-ctr'],
  ['aes-128-ctr', 'aes-128-ctr'],
  ['aes-256-cfb', 'aes-256-cfb'],
  ['aes-192-cfb', 'aes-192-cfb'],
  ['aes-128-cfb', 'aes-128-cfb'],
  ['chacha20-ietf', 'chacha20-ietf'],
  ['xchacha20', 'xchacha20'],
  ['rc4-md5', 'rc4-md5'],
  ['none', 'none'],
];

// sec 条件谓词（原型 ndSyncCond :3707-3710 的等价）。
// export：proto-codec.toConfig 复用同一谓词做「隐藏字段不下发」的**单一真值**（HIGH-1）——
// 表单显隐（NodeDialog `visible` 过滤）与提交装配（toConfig 的 TLS/Reality 组门）共用，不再各写一份。
export const whenTls = (v: FormValues): boolean => v.sec === 'tls' || v.sec === 'reality';
export const whenReality = (v: FormValues): boolean => v.sec === 'reality';

/**
 * http 的 TLS 门 —— **不是 `whenTls`**。http 表单没有 `sec` 选择器，安全层由自己的 `tls` 开关承载
 * （codec 里也是 `security = draft.tls ? 'tls' : 'none'`）⇒ 草稿里根本没有 `sec` 键，`whenTls` 恒 false，
 * 用它等于控件永不渲染（hy2/tuic 那次的同款陷阱，见本文件 hysteria2 段注释）。
 * 同 `whenTls` 一样 export：proto-codec 的 HIGH-1 清除门复用同一谓词，显隐与下发单一真值。
 */
export const whenHttpTls = (v: FormValues): boolean => v.tls === true;

/**
 * 传输层参数的显隐门（vless/vmess/trojan 共用；同 `whenTls` 一样 export 给 proto-codec 做单一真值）。
 *
 * 门的分档**照 Rust `generate_transport_config` 的实际读取面**（`builder/outbound.rs`）而不是按传输名分：
 *  - `ws` 与 `httpupgrade` **同读 `wsSettings`** —— ws 走 `path`（含 `?ed=` 早数据约定）+ 整份
 *    `headers`；httpupgrade 走 `path` + `headers.get("Host")`（缺失时再回落 `tlsSettings.serverName`）。
 *    两者对 path/Host 同构，故共用一道门；**不同构的部分**（ws 独有 `maxEarlyData`/`earlyDataHeaderName`、
 *    httpupgrade 不解析 `?ed=`）本批不建模，留第二批。
 *  - `grpc` 读 `grpcSettings.serviceName`（`multiMode` 本批不建模）。
 *  - `http`/`h2` 读的是 **`httpSettings`**（另一个结构体，还有 host/method/headers），不与上面共用键，
 *    故本批不并入 `whenWsLike` —— 上游 `buildTransportSettings` 把 `wsPath`/`wsHost` 同时喂给
 *    httpSettings，那是它自己的复用，与 Rust 的字段归属无关。
 */
export const whenWsLike = (v: FormValues): boolean => v.net === 'ws' || v.net === 'httpupgrade';
export const whenGrpc = (v: FormValues): boolean => v.net === 'grpc';

/**
 * HTTP/2 传输（`O_NET` 的 `http` 档）参数的门 —— 读 **`httpSettings`**，与 `whenWsLike` 是两个结构体。
 *
 * 档位本身早就选得到（`O_NET` 第 5 项，vless/vmess 一直有，trojan 由 `3d5b0ef` 补上），
 * 选完却一个输入框都没有 ⇒ 与 ws 那条同型的「选了就废」，只是缺省 `/` 比 ws 的更常见、不算阻断级。
 * 四个键在 `generate_transport_config` 的 `"http" | "h2"` 腿全被读取（`builder/outbound.rs`）：
 * `host`（`Vec<String>`，长度 1 时序列化成裸串、否则数组）/ `path`（缺省 `/`）/ `method` / `headers`。
 *
 * 草稿键是 **`net`** 不是 `network` —— 后者是 snell 表单的键，读错会让谓词恒 false、控件永不渲染
 * （`3d5b0ef` 实测过的死门形态）。已加正向对照断言逐档取值。
 */
export const whenH2 = (v: FormValues): boolean => v.net === 'http';

/**
 * **只认 `ws`**（不含 httpupgrade）—— 早数据两键的门。
 *
 * 与 `whenWsLike` 分开是按 Rust 的实际读取面定的：`generate_transport_config` 的 `httpupgrade`
 * 分支只取 `ws_settings` 的 `path` 与 `headers["Host"]`，**根本不读** `max_early_data` /
 * `early_data_header_name`（内核 schema 的 httpupgrade 传输也没有这两键）。给 httpupgrade 显示
 * 这两个框就是拨了不生效的假控件。
 */
export const whenWs = (v: FormValues): boolean => v.net === 'ws';

/**
 * TLS 高级组的两道二级门（**都必须叠在各自的 TLS 一级门之上**）。
 *
 * `whenTls`/`whenHttpTls` 是「本表单开没开 TLS」；这两条是「TLS 组内某个开关拨没拨」。
 * 不叠一级门的话，`sec='none'` 的 vless 上仍会露出 ECH 配置框 —— 而那时整个 `tlsSettings`
 * 会被 HIGH-1 清除，填了也不下发。
 *
 * spoof 的门是「方法非空」而不是一个额外开关：后端 `validate_tls_spoof_default` 的第一条判据就是
 * `is_valid_tls_spoof_method`，方法为空即整项不生效，此时再要一个诱饵 SNI 输入框没有意义。
 */
export const whenTlsSpoof = (v: FormValues): boolean => whenTls(v) && v.spoofMethod !== '' && v.spoofMethod !== undefined;
export const whenTlsEch = (v: FormValues): boolean => whenTls(v) && v.ech === true;
export const whenHttpTlsSpoof = (v: FormValues): boolean =>
  whenHttpTls(v) && v.spoofMethod !== '' && v.spoofMethod !== undefined;

/**
 * 🔴 engine 还要**再去掉 reality 一档**（spoof/ech 不用）—— 三件套里只有它有这条额外限制。
 *
 * 实测（`builder/outbound.rs` 的 Reality 段 + 本仓 Rust 侧断言
 * `reality_branch_drops_tls_engine_but_keeps_spoof_and_ech`）：`security.is_reality()` 时该段用
 * **一个新造的 `OutboundTls` 整体替换**上面刚装好的那块，而新块的 `engine` 写死 `None`
 * ⇒ 用户在 reality 下选的引擎会被静默丢掉。而 `spoof`/`spoof_method`/`ech` 是在**替换之后**由
 * `apply_anti_censorship_options` 补上去的，故那两项在 reality 下照常生效。
 *
 * 内核本身并不禁止 `engine` 与 `reality` 并存（schema 里两者是 `OutboundTLSOptions` 的平级属性），
 * 所以这是**本仓 builder 的缺口而非内核限制**；在它被修掉之前，UI 侧只能诚实地不显示这一档 ——
 * 显示就是一个拨了必然不生效的控件。**存量值仍保全**（见 `tlsAdvPatch`：reality 下不写也不删），
 * 将来 builder 修好即自动生效。
 */
export const whenTlsEngine = (v: FormValues): boolean => whenTls(v) && !whenReality(v);

/**
 * Multiplex 的两道门。
 *
 * `whenMuxAvail` **逐字镜像后端的 vision 判据**（`builder/outbound.rs` 的
 * `flow.to_ascii_lowercase().contains("vision")`）：带 vision flow 时后端整段跳过 multiplex
 * ⇒ 控件留着就是假控件（同 上游 `vless-form.tsx:342` 的 `disabled` + 说明行，只是本仓的
 * `FieldSpec.disabled` 是**静态布尔**、表达不了「随草稿变化」，故改为整组隐藏）。
 *
 * 没有 `flow` 键的表单（vmess/trojan/shadowsocks）上 `v.flow` 是 `undefined` ⇒ 恒为**可用**
 * —— 与「谓词读错草稿键就恒 false、控件永不渲染」那类死门方向相反，但同样必须有正向对照断言，
 * 否则哪天键名写错了会变成「vless 上 vision 也不隐藏」而没人发现。
 */
export const whenMuxAvail = (v: FormValues): boolean =>
  !String(v.flow ?? '').toLowerCase().includes('vision');
export const whenMux = (v: FormValues): boolean => v.mux === true && whenMuxAvail(v);

/**
 * 传输层参数三件套（vless / vmess / trojan 共用）—— **这三颗控件缺席时传输下拉是「选了就废」**：
 * 选 ws 后 `generate_transport_config` 落 `path: "/"`（`ws.and_then(path).unwrap_or("/")`），
 * 而机场节点的 ws path 绝大多数不是 `/` ⇒ 手工建的 ws 节点必然连不上，且 UI 上没有任何字段能修
 * （与 ShadowTLS 空壳同型的「半假控件」，见 shadowsocks 段注释）。
 *
 * 三颗都是 `opt`：留空 = 不下发该键，后端各有缺省（ws path → `/`；ws Host → 不发该 header，
 * httpupgrade 再回落 `tlsSettings.serverName`；grpc serviceName → `unwrap_or_default()` 即空串）
 * —— 与「不建这三个块」逐字节同结果，故默认不填不会动金样。
 */
const F_TRANSPORT: FieldSpec[] = [
  { t: 'text', k: 'wsPath', label: 'node.field.wsPath', ph: '/path', opt: true, when: whenWsLike },
  { t: 'text', k: 'wsHost', label: 'node.field.wsHost', ph: 'example.com', opt: true, when: whenWsLike },
  // ws 早数据（0-RTT）两键 —— **只在 `ws` 下显示**，见 `whenWs` 注释。
  //
  // 与「路径」的交互是**路径赢**，不是二选一：后端 `generate_transport_config` 的 ws 腿先跑
  // `parse_ws_early_data(path)`，再 `ed.max_early_data.or_else(|| ws.max_early_data)` —— 路径里带了
  // `?ed=N` 就用 N（并把 `ed`/`eh` 从 path 上摘掉），只有路径里没有 `?ed=` 时才回落到这里填的值。
  // 头名同理（路径里 `?eh=` > 这里 > 缺省 `Sec-WebSocket-Protocol`）。故这两个框是给「机场只给了
  // 裸 path、要自己开早数据」的场景用的；粘贴的 `?ed=` 链接照旧生效且优先。
  { t: 'number', k: 'wsMaxEarlyData', label: 'node.field.wsMaxEarlyData', ph: '2560', opt: true, when: whenWs },
  { t: 'text', k: 'wsEdHeader', label: 'node.field.wsEdHeader', ph: 'Sec-WebSocket-Protocol', opt: true, when: whenWs },
  { t: 'text', k: 'grpcServiceName', label: 'node.field.grpcServiceName', ph: 'GunService', opt: true, when: whenGrpc },
  // HTTP/2 传输四件套（`httpSettings`，门是 `whenH2`）—— 与上面 ws 那批**不共用键**。
  //
  // 上游 `field-schemas.ts::buildTransportSettings` 把同一对 `wsPath`/`wsHost` 输入框按 network 分派到
  // wsSettings 或 httpSettings；本仓不照抄，理由是 h2 腿比 ws 腿多两个键（method/headers）且 host 是
  // **数组**（ws 的 Host 是 headers 里的一个单值），复用一对框表达不了，切回 ws 时还会互相串值。
  //
  // 留空语义逐键按 Rust 定，四个都是「删键」：
  //  · `path` 缺席 → `unwrap_or("/")`，与显式写 `/` 逐字节同结果；
  //  · `host` 缺席 → 不发该键（内核回落 TLS server_name / 节点地址）；写空数组不等价，故走 `listFromText`；
  //  · `method` 缺席 → 不发（内核用 h2 默认方法）；内核 schema 对它无 enum，故是自由文本不是下拉；
  //  · `headers` 缺席 → 不发。
  { t: 'text', k: 'h2Path', label: 'node.field.h2Path', ph: '/path', opt: true, when: whenH2 },
  // 这三条的说明走 **hint**（标签后的统一 `InfoIcon`），不再并进标签 —— 原注释「`text`/`textarea` 分支没有 hint 位，
  // 不为两个字段去扩 union」的前提在 2026-08-07 变了两处：`hint` 已提到 `FieldBase`（见 field-spec.tsx），
  // 且 `styles/text-fit.test.ts` 的 `.fld-l` 2 行预算把它们逐条顶了出来（en-US/fa/ru 三语各占满 2 行）。
  // 标签是控件的名字，占到第 3 行说明它其实是一句说明。ssh 的 `hostKey`「（逗号分隔，留空接受所有）」
  // 等同型标签**本批未动**：它们只占 2 行的一部分，不是同一档紧度，改动面另计。
  { t: 'text', k: 'h2Host', label: 'node.field.h2Host', hint: 'node.field.h2HostHint', ph: 'a.example.com,b.example.com', opt: true, when: whenH2 },
  { t: 'text', k: 'h2Method', label: 'node.field.h2Method', hint: 'node.field.h2MethodHint', ph: 'GET', opt: true, when: whenH2 },
  { t: 'textarea', k: 'h2Headers', label: 'node.field.h2Headers', hint: 'node.field.h2HeadersHint', mono: true, rows: 3, ph: 'X-Forwarded-For: 1.2.3.4', opt: true, when: whenH2 },
];

/**
 * TLS 高级组（alpn / fragment / engine / spoof / ech）—— vless·vmess·trojan·anytls 与 http 共用同一批控件，
 * 差别只在**门谓词**（前四者有 `sec` 选择器走 `whenTls`；http 只有一个 `tls` 开关走 `whenHttpTls`）。
 * 抽成工厂而不是复制两份：这些字段各跨 5 个协议，复制必然漂移（fp/alpn 那批的教训）。
 *
 * 逐字段的空值语义按 Rust 定，**没有一刀切**：
 *  - `alpn` 空 = 删键。后端 `final_alpn` 对所有走标准 TLS 栈的协议都读 `tls_settings.alpn`，
 *    **只有 trojan 有专属缺省** `["http/1.1"]`（`final_alpn.is_none()` 时补上）⇒ 占位符按协议给
 *    （`alpnPh` 参数），其余协议留空就是「不声明 ALPN」。写空数组会顶掉 trojan 那条缺省，故走
 *    `listFromText` 而不是裸 split（`" , , "` 必须落回删键，见其注释）；
 *  - `fragment` 关 = **删键**，不写 `false`。后端 `apply_anti_censorship_options` 的判据是
 *    `tls_s.fragment == Some(true)`（严格等于 `Some(true)`）⇒ `None` 与 `Some(false)` 走同一条腿、
 *    逐字节同结果，写 `false` 只会给每份存量配置凭空多一个键（同 hy2 `noParrot` / tuic `zeroRtt` 的既有写法）；
 *  - `engine` 空 = 删键，后端等价 `go`（见 `O_TLS_ENGINE`）；
 *  - `spoofMethod`/`spoofSni` **齐备才写**（照 ShadowTLS 那条 `f9d2f3a` 的先例）：后端
 *    `validate_tls_spoof_default` 要求方法合法 **且** 诱饵 SNI 非空，只写一半在磁盘上是一对
 *    永不生效的死键。**这一点与 上游 不同** —— 它 `buildTlsSpoofSettings` 方法合法就写、SNI 允许空；
 *  - `ech` 关 = 删键（同 hy2/tuic 的既有写法），`echConfig` 仅在 ech 开时写。
 */
function tlsAdvFields(
  gate: (v: FormValues) => boolean,
  engineGate: (v: FormValues) => boolean,
  spoofGate: (v: FormValues) => boolean,
  alpnPh: string
): FieldSpec[] {
  return [
    { t: 'text', k: 'alpn', label: 'node.field.alpn', ph: alpnPh, opt: true, when: gate },
    /**
     * TLS ClientHello 分片（抗 SNI-DPI）——**门是一级门 `gate`，不叠 `!whenReality`**。
     *
     * 与 `engine` 那条额外限制的区别已按 Rust 核过：Reality 段确实用新造的 `OutboundTls` 整体替换掉
     * 上面装好的那块（`engine` 因此被吞），但 `fragment` 与 `spoof`/`ech` 一样是**替换之后**由
     * `apply_anti_censorship_options` 补上去的 ⇒ reality 下照常生效，加 `!whenReality` 反而会藏掉真控件。
     *
     * 键名是内核 `tls.fragment`（boolean），**不是 `record_fragment`** —— 后者是 TLS 记录层分片，
     * 随包核 beta.7 的 `OutboundTLSOptions` 里两个键并列存在、语义不同，本仓 `TlsSettings` 只建模了前者。
     * 内核另有配套的 `fragment_fallback_delay`（Duration，非必填：`OutboundTLSOptions` 无 `required` 列表），
     * **本仓 Rust 未建模、本批也不建模** —— 它只调「分片失败后多久回退」，缺席即用内核缺省，
     * 不建模不会让 `fragment` 变成半成品（实测五协议单给 `fragment:true` 的 `sing-box check` 全 exit=0）。
     */
    { t: 'switch', k: 'fragment', label: 'node.field.tlsFragment', hint: 'node.field.tlsFragmentHint', when: gate },
    { t: 'select', k: 'engine', label: 'node.field.tlsEngine', hint: 'node.field.tlsEngineHint', options: O_TLS_ENGINE, when: engineGate },
    { t: 'select', k: 'spoofMethod', label: 'node.field.spoofMethod', hint: 'node.field.spoofMethodHint', options: O_SPOOF_METHOD, when: gate },
    { t: 'text', k: 'spoofSni', label: 'node.field.spoofSni', ph: 'www.bing.com', when: spoofGate },
  ];
}

/**
 * vless / vmess / trojan / anytls 的 TLS 高级组（alpn + fragment + 三件套 + ECH）。
 *
 * 工厂化只为 **`alpn` 的占位符按协议分叉**：trojan 留空回落后端专属缺省 `["http/1.1"]`，
 * 其余协议留空就是不声明 ALPN ⇒ 用同一个占位符会在两边各错一半。除此之外两份逐字节相同。
 */
const tlsAdvGroup = (alpnPh: string): FieldSpec[] => [
  ...tlsAdvFields(whenTls, whenTlsEngine, whenTlsSpoof, alpnPh),
  // ECH：与 hy2/tuic 上早已落地的那两颗同形，只是门要叠 `whenTls`（那两个表单 TLS 恒开、无 sec 键）。
  { t: 'switch', k: 'ech', label: 'node.field.ech', hint: 'node.field.echHint', when: whenTls },
  { t: 'textarea', k: 'echConfig', label: 'node.field.echConfig', mono: true, rows: 3, opt: true, when: whenTlsEch },
];
const F_TLS_ADV: FieldSpec[] = tlsAdvGroup('h2,http/1.1');
/** trojan 专用：留空 ⇒ 后端补 `["http/1.1"]`，占位符必须是那条缺省而不是通用示例。 */
const F_TLS_ADV_TROJAN: FieldSpec[] = tlsAdvGroup('http/1.1');

/**
 * http 的 TLS 高级组 —— **有 alpn + fragment + engine + spoof，没有 ECH**。
 * ECH 的缺席与 上游 `http-form.tsx` 一致（它引 `TlsEngineField` + `TlsSpoofField`，未引 `EchField`）：
 * 后端对 http 并不禁 ECH（`apply_anti_censorship_options` 只要 `ob.tls` 在就会装配），
 * 故这属「两边都没有」而非移植遗漏，与 socks `version` 的 T4 裁定同一口径。
 */
// engine 的门与一级门同一个：http 表单没有 `sec` 键 ⇒ `whenReality` 恒 false，reality 那条额外限制
// 在这里不成立（已加正向对照断言，防止「以为加了门其实恒真/恒假」）。
const F_HTTP_TLS_ADV: FieldSpec[] = tlsAdvFields(whenHttpTls, whenHttpTls, whenHttpTlsSpoof, 'h2,http/1.1');

/**
 * Multiplex 组（vless / vmess / trojan / shadowsocks 共用）—— 协议面**逐字取自后端**
 * `apply_anti_censorship_options` 里的 `matches!(server.protocol, Vless | Trojan | Vmess | Shadowsocks)`，
 * 不是按「谁看起来该有」选的。给其它协议加这组就是假控件（后端那句 `matches!` 直接跳过）。
 *
 * 五个键与 `MultiplexSettings` 一一对应且**无未建模项**（Rust 结构体正好 5 个字段），
 * 故 codec 侧整块重建、不需要 `...base` 起底。
 */
const F_MUX: FieldSpec[] = [
  { t: 'switch', k: 'mux', label: 'node.field.mux', hint: 'node.field.muxHint', when: whenMuxAvail },
  { t: 'select', k: 'muxProto', label: 'node.field.muxProto', options: O_MUX_PROTO, when: whenMux },
  // 留空 = 不下发该键 = 内核自选（`Option<u32>`，非 0）。
  { t: 'number', k: 'muxMax', label: 'node.field.muxMax', opt: true, when: whenMux },
  { t: 'number', k: 'muxMin', label: 'node.field.muxMin', opt: true, when: whenMux },
  { t: 'switch', k: 'muxPad', label: 'node.field.muxPad', hint: 'node.field.muxPadHint', when: whenMux },
];

// Snell version 驱动的互斥分支谓词（obfs 系仅 v4 / mode 系仅 v6，sing-box check 实证互斥）。
const whenSnellV4 = (v: FormValues): boolean => v.version === '4';
const whenSnellV6 = (v: FormValues): boolean => v.version === '6';
const whenSnellObfsHttp = (v: FormValues): boolean => v.version === '4' && v.obfsMode === 'http';

// Hysteria2 gecko obfs 的随机填充包长仅 gecko 有意义（salamander 忽略）——非 gecko 隐藏防误填脏下发。
const whenObfsGecko = (v: FormValues): boolean => v.obfs === 'gecko';
// ECH（Encrypted Client Hello）config 仅 ech 开关打开时显；关闭时无需填 ECHConfigList。hy2/tuic 共用。
const whenEch = (v: FormValues): boolean => v.ech === true;
// ShadowTLS 参数组仅在 ss 的 stls 开关打开时显（该表单里 `stls` 是真实存在的草稿键，谓词不会恒 false）。
const whenStls = (v: FormValues): boolean => v.stls === true;

/**
 * ND_SPEC —— 每协议 { cred, adv }。字段键与 protoCodec 读写键一一对应。
 * label/hint 用 `node.field.*` i18n 键，**没有 zh 缺省**（真值源只有 locale 一处，见文件头）。
 */
export type NodeFieldGroupId = 'basic' | 'transport' | 'routing' | 'advanced';

export interface NodeFieldGroup {
  id: NodeFieldGroupId;
  fields: FieldSpec[];
}

interface NodeSpec {
  cred: FieldSpec[];
  adv: FieldSpec[];
  /**
   * 有专属任务模型的组网 endpoint 信息架构。普通代理的展示分组由 `nodeFormGroups` 基于
   * `cred/adv` 生成，避免把展示分组混入 codec 的字段真值。
   */
  groups?: NodeFieldGroup[];
}

export const ND_SPEC: Record<NodeProto, NodeSpec> = {
  vless: {
    cred: [{ t: 'text', k: 'uuid', label: 'node.field.uuid', mono: true }],
    adv: [
      { t: 'select', k: 'flow', label: 'node.field.flow', options: [['', 'none'], ['xtls-rprx-vision', 'xtls-rprx-vision']] },
      { t: 'select', k: 'net', label: 'node.field.net', options: O_NET },
      ...F_TRANSPORT,
      { t: 'select', k: 'sec', label: 'node.field.sec', options: [['none', 'None'], ['tls', 'TLS'], ['reality', 'Reality']] },
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'www.microsoft.com', when: whenTls },
      { t: 'select', k: 'fp', label: 'node.field.fp', options: O_FP, when: whenTls },
      { t: 'text', k: 'pbk', label: 'node.field.pbk', mono: true, when: whenReality },
      { t: 'text', k: 'sid', label: 'node.field.sid', mono: true, when: whenReality },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint', when: whenTls },
      ...F_TLS_ADV,
      ...F_MUX,
    ],
  },
  vmess: {
    cred: [
      { t: 'text', k: 'uuid', label: 'node.field.uuid', mono: true },
      { t: 'number', k: 'aid', label: 'node.field.aid', ph: '0' },
    ],
    adv: [
      // enc 档位对齐 上游 vmess-form.tsx:230-234（auto/aes-128-gcm/chacha20-poly1305/none/zero）。
      // `zero` = 不加密且不做完整性校验（内核 `vmess.security` 合法值，随包 beta.7 `sing-box check` 实证）。
      // 内核另有 `aes-128-cfb` 一档，上游 也不给 —— 那是另一个判断，不在本批。
      { t: 'select', k: 'enc', label: 'node.field.enc', options: [['auto', 'auto'], ['aes-128-gcm', 'aes-128-gcm'], ['chacha20-poly1305', 'chacha20-poly1305'], ['none', 'none'], ['zero', 'zero']] },
      { t: 'select', k: 'net', label: 'node.field.net', options: O_NET },
      ...F_TRANSPORT,
      { t: 'select', k: 'sec', label: 'node.field.sec', options: [['none', 'None'], ['tls', 'TLS']] },
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', when: whenTls },
      // uTLS 指纹：后端对 vmess 的缺省是 `none`（不下发 utls 块），故用带空档的 O_FP_OPT，见其注释。
      { t: 'select', k: 'fp', label: 'node.field.fp', options: O_FP_OPT, when: whenTls },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint', when: whenTls },
      ...F_TLS_ADV,
      ...F_MUX,
    ],
  },
  trojan: {
    cred: [{ t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true }],
    adv: [
      // 传输档位改用全仓 O_NET（补回 httpupgrade / HTTP2）：`generate_transport_config` 的分支只按
      // `server.network` 分派、**不按协议门控**，trojan 与 vless/vmess 走同一段；排除名单只有
      // hy2/anytls/naive（`build_proxy_outbound` 的 `matches!`）。故这两档一直可用，缺的只是下拉项。
      { t: 'select', k: 'net', label: 'node.field.net', options: O_NET },
      ...F_TRANSPORT,
      { t: 'select', k: 'sec', label: 'node.field.sec', options: [['tls', 'TLS'], ['none', 'None']] },
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', when: whenTls },
      { t: 'select', k: 'fp', label: 'node.field.fp', options: O_FP_OPT, when: whenTls },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint', when: whenTls },
      // ALPN 已并入 `F_TLS_ADV_TROJAN`（此前是这里一颗独立控件）—— 同一个键跨 5 个协议，留在
      // 各协议块里必然漂移。**留空 ≠ 下发空数组**：后端对 trojan 有专属缺省
      // `final_alpn.is_none() → ["http/1.1"]`，占位符即该缺省值，这是本协议独用一份工厂产物的全部原因。
      ...F_TLS_ADV_TROJAN,
      ...F_MUX,
    ],
  },
  shadowsocks: {
    cred: [
      { t: 'select', k: 'method', label: 'node.field.method', options: O_SS_METHOD },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true },
    ],
    adv: [
      // SIP003 插件（`ss.plugin` / `plugin_opts`，后端 `outbound.rs` 的 Shadowsocks 分支直接透传）。
      // 留空 = 删键；两者独立可选（内核允许只给 plugin 不给 opts）。
      { t: 'text', k: 'plugin', label: 'node.field.plugin', ph: 'obfs-local', opt: true },
      { t: 'text', k: 'pluginOpts', label: 'node.field.pluginOpts', ph: 'obfs=http;obfs-host=bing.com', opt: true },
      { t: 'switch', k: 'stls', label: 'node.field.stls', hint: 'node.field.stlsHint' },
      // ShadowTLS 参数组（开关打开才显）——**没有这四颗控件时 stls 开关是个「造坏节点」按钮**：
      // codec 旧实现一开就写 `{password:'', sni:''}`，后端 `builder/outbounds.rs` 的
      // `apply_shadow_tls_postprocess` 据此造出 password 为空串、server_name 缺席的外层 shadowtls 出站，
      // 并把 SS 的 detour 指过去 ⇒ 该节点必然连不上，而 UI 上没有任何字段能修。齐备门在 proto-codec
      // （password && sni 同时非空才下发，照 上游 ss-form.tsx）。
      { t: 'text', k: 'stlsPwd', label: 'node.field.stlsPwd', mono: true, when: whenStls },
      { t: 'text', k: 'stlsSni', label: 'node.field.stlsSni', ph: 'www.microsoft.com', when: whenStls },
      // 指纹沿用全仓 O_FP（vless/anytls 同一份）；后端 `ShadowTlsSettings::fingerprint` 是开放 String +
      // 消费点归一，缺省回落 chrome，故本表不必与 uTLS 取值集同步扩张。存量的表外值由渲染层保留（见 field-spec）。
      { t: 'select', k: 'stlsFp', label: 'node.field.stlsFp', options: O_FP, when: whenStls },
      // 真实端口：Rust 侧 `port: Option<u16>`，None **或 0** 都降级用节点主端口（outbounds.rs 显式判 `p != 0`），故 opt。
      { t: 'number', k: 'stlsPort', label: 'node.field.stlsPort', ph: '443', opt: true, when: whenStls },
      ...F_MUX,
    ],
  },
  hysteria2: {
    cred: [{ t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true }],
    adv: [
      { t: 'number', k: 'up', label: 'node.field.up', ph: '100' },
      { t: 'number', k: 'down', label: 'node.field.down', ph: '500' },
      { t: 'select', k: 'obfs', label: 'node.field.obfs', options: [['', 'none'], ['salamander', 'salamander'], ['gecko', 'gecko']] },
      { t: 'text', k: 'obfspwd', label: 'node.field.obfspwd', mono: true, opt: true },
      // gecko obfs 随机填充包长（sing-box obfs.min/max_packet_size，1.14）——仅 gecko 有意义，salamander 时隐藏。
      { t: 'number', k: 'obfsMin', label: 'node.field.obfsMin', opt: true, when: whenObfsGecko },
      { t: 'number', k: 'obfsMax', label: 'node.field.obfsMax', opt: true, when: whenObfsGecko },
      // BBR 拥塞控制 profile（sing-box bbr_profile，1.14）——空=核心默认；仅 standard/aggressive/conservative 合法。
      { t: 'select', k: 'bbr', label: 'node.field.bbr', options: [['', 'common.default'], ['standard', 'Standard'], ['aggressive', 'Aggressive'], ['conservative', 'Conservative']] },
      { t: 'text', k: 'ports', label: 'node.field.ports', ph: '20000:30000', opt: true },
      { t: 'text', k: 'hopInterval', label: 'node.field.hopInterval', ph: '30s', opt: true },
      // 出站可用网络（`hysteria2.network`，后端 `outbound.rs` 的 `ob.network = h.network.clone()`）——
      // 与 snell 那颗同形同取值集（内核 schema 对 hy2 的 `network` 是 `tcp`/`udp` 的 enum）。
      // **首项空串 = 不下发该键 = tcp+udp 都走**，这是内核缺省；选单侧会把另一侧的流量挡在这个出站之外。
      // 此前它是覆盖门 per-struct 判据下被 snell 的 `{k:'network'}` 遮蔽的实例之一（记账记成零债务）。
      {
        t: 'select', k: 'network', label: 'node.field.net', options: [['', 'tcp+udp'], ['tcp', 'TCP'], ['udp', 'UDP']],
        hint: 'node.field.hy2NetworkHint',
      },
      // 关闭 Chrome QUIC 握手拟态（sing-box 1.14.0-beta.7 disable_chrome_parrot）——**回归逃生舱**，不是调优项：
      // beta.7 起 hy2 默认拟态 Chrome 握手，而 Chrome 不声明 Ed25519 ⇒ 服务端 Ed25519 证书必然握手失败，
      // 用户侧只看到「连不上」。核心默认 false（拟态开）⇒ 开关默认关、关时整键不下发。
      { t: 'switch', k: 'noParrot', label: 'node.field.noParrot', hint: 'node.field.noParrotHint' },
      // TLS 组：hy2 TLS 恒开（后端 TLS_PROTOCOLS 含 hysteria2），本表单无 sec 选择器 ⇒ 草稿里根本没有
      // `sec` 键，加 `when: whenTls` 会让谓词恒 false、控件永不显示。故照 anytls 不加门（恒显）。
      // sni 留空 = 后端回落节点地址（outbound.rs `unwrap_or_else(|| server.address)`），故 opt。
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true },
      // ALPN：hy2 **不在** `is_quic_managed_tls` 挡掉的那五个键里 —— 那道门管的是 engine/fingerprint/
      // fragment/spoof，`final_alpn` 对 hy2 照常下发（tuic 的表单早就有这个框）。留空 = 删键 = 内核默认 `h3`。
      { t: 'text', k: 'alpn', label: 'node.field.alpn', ph: 'h3', opt: true },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint' },
      // ECH（反审查，加密 ClientHello 隐藏 SNI）——hy2 TLS 恒开（QUIC 自管）。echConfig 空=从 DNS HTTPS RR 自取。
      { t: 'switch', k: 'ech', label: 'node.field.ech', hint: 'node.field.echHint' },
      { t: 'textarea', k: 'echConfig', label: 'node.field.echConfig', mono: true, rows: 3, opt: true, when: whenEch },
    ],
  },
  tuic: {
    cred: [
      { t: 'text', k: 'uuid', label: 'node.field.uuid', mono: true },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true },
    ],
    adv: [
      { t: 'select', k: 'cc', label: 'node.field.cc', options: [['bbr', 'bbr'], ['cubic', 'cubic'], ['new_reno', 'new_reno']] },
      { t: 'select', k: 'udp', label: 'node.field.udp', options: [['native', 'native'], ['quic', 'quic']] },
      // 0-RTT 握手（`tuic.zero_rtt_handshake`）：内核默认 false ⇒ 关时**整键不下发**（同 hy2 的
      // noParrot），写 `false` 只会给每份存量配置凭空多一个语义等价的键。
      { t: 'switch', k: 'zeroRtt', label: 'node.field.zeroRtt', hint: 'node.field.zeroRttHint' },
      // 心跳间隔（`tuic.heartbeat`）：后端 `normalize_duration` 会给裸数字补 `ms`，带单位的原样透传。
      { t: 'text', k: 'heartbeat', label: 'node.field.heartbeat', ph: '10s', opt: true },
      { t: 'text', k: 'alpn', label: 'node.field.alpn', ph: 'h3' },
      // TLS 组：tuic TLS 恒开（后端 TLS_PROTOCOLS 含 tuic），本表单无 sec 选择器 ⇒ 同 hy2，不加 when 门。
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint' },
      // ECH（反审查）——tuic TLS 恒开（QUIC 自管）。echConfig 空=从 DNS HTTPS RR 自取。
      { t: 'switch', k: 'ech', label: 'node.field.ech', hint: 'node.field.echHint' },
      { t: 'textarea', k: 'echConfig', label: 'node.field.echConfig', mono: true, rows: 3, opt: true, when: whenEch },
    ],
  },
  socks: {
    cred: [
      { t: 'text', k: 'user', label: 'node.field.user', opt: true },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true, opt: true },
    ],
    adv: [],
  },
  http: {
    cred: [
      { t: 'text', k: 'user', label: 'node.field.user', opt: true },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true, opt: true },
    ],
    adv: [
      { t: 'switch', k: 'tls', label: 'node.field.httpTls', hint: 'node.field.httpTlsHint' },
      // TLS 组（与 hy2/tuic 同批的移植遗漏）：http 打开 TLS 后 `security='tls'`，后端走
      // `outbound.rs` 与 trojan/vless 同一段装配（server_name 缺省回落节点地址、insecure 缺省 false），
      // 一直支持，缺的只是控件。门谓词是 `whenHttpTls`（读 `tls` 开关）——**不是 `whenTls`**，
      // 本表单没有 `sec` 键，用 whenTls 会恒 false、控件永不显示。
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true, when: whenHttpTls },
      // uTLS 指纹：http **不在** `is_quic_managed_tls` 里，`final_fp != "none"` 时 utls 块照常下发
      // ⇒ 缺的一直只是控件。取值集必须是 **`O_FP_OPT`（带空首项）而不是 `O_FP`**：`final_fp` 的缺省
      // 按协议分叉，vless/anytls → chrome、其余 → none，http 属「其余」；用 `O_FP` 会让
      // `draftFromSpecs` 把首项 chrome 当默认 seed，新建的 http 节点凭空多出 utls 块（`3d5b0ef` 的同款陷阱）。
      { t: 'select', k: 'fp', label: 'node.field.fp', options: O_FP_OPT, when: whenHttpTls },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint', when: whenHttpTls },
      ...F_HTTP_TLS_ADV,
    ],
  },
  // AnyTLS：TLS 恒开（后端 TLS_PROTOCOLS 强制），故 sni/fp/insecure **仍不加 when 门**（恒显）——
  // 见下面 sec 那一项的注释：本表单的 sec 只有 tls/reality 两档，`whenTls` 在两档下都为 true，
  // 加门等于加一个恒真谓词，只会多一处可漂移的判据。
  anytls: {
    cred: [{ t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true }],
    adv: [
      // 安全层：**没有 None 档**。anytls 在 `TLS_PROTOCOLS` 里 ⇒ 后端无条件下发 TLS 块，给出「None」
      // 会是个拨了不生效的假控件（同 上游 anytls-form.tsx:35 的 `z.enum(['tls','reality'])`）。
      // Reality 由 `security.is_reality()` 单独判、**不按协议门控**（`outbound.rs` 的 Reality 段），
      // 且该段会用 reality 版 TLS 块**整体替换**上面那块 —— 替换后 server_name/insecure/utls 仍取自
      // `tlsSettings`（只是 server_name 不再回落节点地址），所以 sni/fp/insecure 在 reality 下照样有效。
      { t: 'select', k: 'sec', label: 'node.field.sec', options: [['tls', 'TLS'], ['reality', 'Reality']] },
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true },
      { t: 'select', k: 'fp', label: 'node.field.fp', options: O_FP },
      { t: 'text', k: 'pbk', label: 'node.field.pbk', mono: true, when: whenReality },
      { t: 'text', k: 'sid', label: 'node.field.sid', mono: true, when: whenReality },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure', hint: 'node.field.insecureHint' },
      // TLS 高级组在 anytls 上**带 `whenTls` 门**（与上面无门的 sni/fp/insecure 不同）：本表单的 sec
      // 只有 tls/reality 两档、`whenTls` 恒真 ⇒ 门在此恒开、行为与「不加门」逐字节相同，加它换来的是
      // 与另外三个协议共用**同一份** `F_TLS_ADV`（三件套各跨 4 协议，复制必漂移）。恒真已有正向对照断言。
      ...F_TLS_ADV,
      { t: 'text', k: 'idleCheck', label: 'node.field.idleCheck', ph: '30s', opt: true },
      { t: 'text', k: 'idleTimeout', label: 'node.field.idleTimeout', ph: '30s', opt: true },
      { t: 'number', k: 'minIdle', label: 'node.field.minIdle', ph: '0', opt: true },
    ],
  },
  // NaiveProxy：username+password 后端必填；TLS 大部分由 Cronet 自管 —— insecure / alpn / uTLS /
  // fragment / min_version / cipher_suites / curve_preferences / reality / client_certificate 这批
  // **随包核 beta.7 会点名 FATAL**（`… is not supported on naive outbound`），故一律不建模。
  //
  // **ECH 是例外，实测为准**（2026-08-06，随包核 beta.7）：它不在核那张拒绝名单里，且喂一份坏 PEM
  // 时 naive 与 trojan 报**同一句** `invalid ECH configs pem` ⇒ 走的是同一条 ECH 装配路径、不是被忽略。
  // 本仓侧也通得到：`Protocol::Naive` 分支虽写死 `ech: None`，但 `apply_anti_censorship_options` 在它
  // **之后**运行且只看 `ob.tls.is_some()`，会把 `tlsSettings.ech` 覆盖上去。两头都通 ⇒ 补控件。
  naive: {
    cred: [
      { t: 'text', k: 'user', label: 'node.field.user' },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true },
    ],
    adv: [
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true },
      { t: 'switch', k: 'http3', label: 'node.field.http3', hint: 'node.field.http3Hint' },
      // naive TLS 恒开（分支无条件建 `ob.tls`）⇒ 与 hy2/tuic 一样不加 when 门（本表单没有 sec/tls 键，
      // 加 `whenTls`/`whenHttpTls` 会恒 false、控件永不渲染）。
      { t: 'switch', k: 'ech', label: 'node.field.ech', hint: 'node.field.echHint' },
      { t: 'textarea', k: 'echConfig', label: 'node.field.echConfig', mono: true, rows: 3, opt: true, when: whenEch },
    ],
  },
  // Snell：version 主开关驱动 obfs（v4）/ mode（v6）互斥分支；psk 复用 ServerConfig.password（同 trojan/hy2 惯例）。
  snell: {
    cred: [
      { t: 'select', k: 'version', label: 'node.field.snellVer', options: [['4', 'v4'], ['6', 'v6']] },
      { t: 'text', k: 'pwd', label: 'node.field.psk', mono: true },
    ],
    adv: [
      { t: 'select', k: 'obfsMode', label: 'node.field.obfsMode', options: [['none', 'none'], ['http', 'http']], when: whenSnellV4 },
      { t: 'text', k: 'obfsHost', label: 'node.field.obfsHost', ph: 'bing.com', when: whenSnellObfsHttp, opt: true },
      { t: 'select', k: 'mode', label: 'node.field.snellMode', options: [['default', 'default'], ['unshaped', 'unshaped'], ['unsafe-raw', 'unsafe-raw']], when: whenSnellV6 },
      { t: 'select', k: 'network', label: 'node.field.net', options: [['', 'tcp+udp'], ['tcp', 'TCP'], ['udp', 'UDP']] },
      { t: 'switch', k: 'reuse', label: 'node.field.reuse', hint: 'node.field.reuseHint' },
      { t: 'text', k: 'userkey', label: 'node.field.userkey', mono: true, opt: true },
    ],
  },
  // ── Hysteria v1（2026-08-11）──
  // 与 hysteria2 **是两个协议**，不是同一协议的版本档：v1 的 obfs 是裸字符串（不是 {type,password}
  // 对象）、认证走 authStr、带宽是必填语义。TLS 恒开（后端 TLS_PROTOCOLS 含 hysteria）——
  // 随包核对缺 TLS 的 v1 出站判 `initialize outbound[0]: TLS required`，故无 sec 开关，同 hy2。
  hysteria: {
    cred: [
      { t: 'text', k: 'authStr', label: 'node.field.authStr', mono: true },
      { t: 'number', k: 'up', label: 'node.field.up', ph: '10' },
      { t: 'number', k: 'down', label: 'node.field.down', ph: '50' },
    ],
    adv: [
      // v1 的 obfs 是**混淆口令字符串**本身，不是类型选单 —— 与 hy2 那颗同名不同义，别复用它的 options。
      { t: 'text', k: 'obfs', label: 'node.field.obfs', mono: true, opt: true },
      // 端口跳跃：抗封锁常用，故进表单而非透传袋（调优旋钮族一律进袋）。
      { t: 'text', k: 'ports', label: 'node.field.ports', ph: '20000:30000', opt: true },
      { t: 'text', k: 'hopInterval', label: 'node.field.hopInterval', ph: '30s', opt: true },
      { t: 'text', k: 'sni', label: 'node.field.sni', ph: 'example.com', opt: true },
      { t: 'text', k: 'alpn', label: 'node.field.alpn', ph: 'h3', opt: true },
      { t: 'switch', k: 'insecure', label: 'node.field.insecure' },
      // ECH：与 hy2 同待遇（三者走同一个 tls.NewClient，TLS 由 QUIC 栈接管）。
      { t: 'switch', k: 'ech', label: 'node.field.ech' },
      { t: 'text', k: 'echConfig', label: 'node.field.echConfig', mono: true, opt: true },
      // ── 透传袋入口（2026-08-11）──
      // 表单是**精选子集**，其余键（openconnect 61 键里的 csd/cookie/compression_mode…）
      // 此前只有「从本地文件导入」才进得了袋子，手建节点根本够不到 —— 等于「支持」只对导入成立。
      // 与其再铺几十个控件，不如给袋子一个入口：一个控件覆盖全部剩余字段，
      // 且**不需要改用自定义协议**（那会丢掉本协议的表单与校验）。
      // 内容原样合并进下发配置；同名键由上面的具名字段压过（生成侧显式 remove，见 outbound.rs）。
      { t: 'textarea', k: 'extraJson', label: 'node.field.extraJson', hint: 'node.field.extraJsonHint', mono: true, rows: 4, opt: true },
    ],
  },
  // ── 内嵌 Tor（2026-08-11）──
  // **无 server/port**：实测给随包核传 server 得 `unknown field "server"`，是 decode 阶段硬失败、
  // 整个核起不来。生成侧显式清空这两个键；表单侧也不给凭据字段。
  tor: {
    cred: [],
    adv: [
      { t: 'text', k: 'torExec', label: 'node.field.torExec', ph: '/usr/bin/tor', mono: true, opt: true },
      { t: 'text', k: 'torDataDir', label: 'node.field.torDataDir', mono: true, opt: true },
      { t: 'text', k: 'torArgs', label: 'node.field.torArgs', hint: 'node.field.torArgsHint', mono: true, opt: true },
      // torrc：内核那个键是 `object`（string→string），但**表单用 torrc 原生语法**（每行 `Key Value`）——
      // 用户已经会写、可直接从现成 torrc 粘贴，比键值对控件或裸 JSON 都低摩擦。
      // 注释行与空行在解析时丢弃：内核侧是 map，本来就承载不了它们（不是本控件的损失）。
      { t: 'textarea', k: 'torrcText', label: 'node.field.torrc', hint: 'node.field.torrcHint', mono: true, rows: 4, opt: true },
      // ── 透传袋入口（2026-08-11）──
      // 表单是**精选子集**，其余键（openconnect 61 键里的 csd/cookie/compression_mode…）
      // 此前只有「从本地文件导入」才进得了袋子，手建节点根本够不到 —— 等于「支持」只对导入成立。
      // 与其再铺几十个控件，不如给袋子一个入口：一个控件覆盖全部剩余字段，
      // 且**不需要改用自定义协议**（那会丢掉本协议的表单与校验）。
      // 内容原样合并进下发配置；同名键由上面的具名字段压过（生成侧显式 remove，见 outbound.rs）。
      { t: 'textarea', k: 'extraJson', label: 'node.field.extraJson', hint: 'node.field.extraJsonHint', mono: true, rows: 4, opt: true },
    ],
  },
  // ── OpenConnect（2026-08-11）──
  // 一个协议覆盖六家商用 VPN，由 flavor 区分。内核需要的 `server: host:port` 由 NodeDialog 顶部
  // 公共地址/端口派生，表单不再维护第二份 server 真值。
  // csd / hip / tncc / fortinet_host_check / form_entries 刻意**不进表单**：各家专有的外部认证
  // 脚本钩子，语义强绑定厂商，做成通用控件只会误导 —— 需要的走「自定义」协议直通。
  openconnect: {
    cred: [],
    adv: [],
    groups: [
      {
        id: 'basic',
        fields: [
          { t: 'text', k: 'user', label: 'node.field.user' },
          { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true, secret: true },
          { t: 'select', k: 'flavor', label: 'node.field.flavor', options: [
            ['anyconnect', 'Cisco AnyConnect'], ['gp', 'Palo Alto GlobalProtect'], ['fortinet', 'Fortinet'],
            ['f5', 'F5'], ['pulse', 'Pulse Secure'], ['nc', 'Juniper Network Connect'],
          ] },
          { t: 'text', k: 'authGroup', label: 'node.field.authGroup', opt: true },
          { t: 'text', k: 'token', label: 'node.field.token', mono: true, opt: true, secret: true },
        ],
      },
      {
        id: 'routing',
        fields: [
          { t: 'textarea', k: 'meshRoutes', label: 'node.field.meshRoutes', hint: 'node.field.meshRoutesHint', mono: true, rows: 3, opt: true, ph: '10.10.0.0/16' },
          { t: 'switch', k: 'sysIface', label: 'node.field.sysIface', hint: 'node.field.sysIfaceHint' },
        ],
      },
      {
        id: 'advanced',
        fields: [
          { t: 'number', k: 'mtu', label: 'node.field.mtu', opt: true },
          { t: 'switch', k: 'noUdp', label: 'node.field.noUdp' },
          { t: 'switch', k: 'pfs', label: 'node.field.pfs' },
          { t: 'switch', k: 'insecureCrypto', label: 'node.field.insecureCrypto', hint: 'node.field.insecureCryptoHint' },
          { t: 'text', k: 'userAgent', label: 'node.field.userAgent', opt: true },
          { t: 'text', k: 'reportedOs', label: 'node.field.reportedOs', opt: true },
          { t: 'textarea', k: 'extraJson', label: 'node.field.extraJson', hint: 'node.field.extraJsonHint', mono: true, rows: 4, opt: true },
        ],
      },
    ],
  },
  // ── OpenVPN 客户端（2026-08-11）──
  // tls **必填**：缺了内核判 `initialize endpoint[0]: missing \`tls\` options`。
  // 只给 certificate（CA）——peer_fingerprint 必须是规范小写十六进制，做成输入框会制造一类
  // 只有真连才暴露的错误。server 端不做。
  'openvpn-client': {
    cred: [],
    adv: [],
    groups: [
      {
        id: 'basic',
        fields: [
          { t: 'text', k: 'user', label: 'node.field.user' },
          { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true, secret: true },
          { t: 'textarea', k: 'ovpnCa', label: 'node.field.ovpnCa', hint: 'node.field.ovpnCaHint', mono: true, rows: 5 },
          { t: 'textarea', k: 'ovpnCert', label: 'node.field.ovpnCert', mono: true, rows: 5, opt: true },
          { t: 'textarea', k: 'ovpnKey', label: 'node.field.ovpnKey', mono: true, rows: 5, opt: true, secret: true },
        ],
      },
      {
        id: 'routing',
        fields: [
          { t: 'textarea', k: 'meshRoutes', label: 'node.field.meshRoutes', hint: 'node.field.meshRoutesHint', mono: true, rows: 3, opt: true, ph: '10.10.0.0/16' },
          { t: 'switch', k: 'redirectGw', label: 'node.field.redirectGw', hint: 'node.field.redirectGwHint' },
          { t: 'switch', k: 'sysIface', label: 'node.field.sysIface', hint: 'node.field.sysIfaceHint' },
        ],
      },
      {
        id: 'advanced',
        fields: [
          { t: 'select', k: 'network', label: 'node.field.net', options: [['', 'UDP (default)'], ['tcp', 'TCP'], ['udp', 'UDP']] },
          { t: 'text', k: 'cipher', label: 'node.field.cipher', ph: 'AES-256-GCM', opt: true },
          { t: 'text', k: 'ovpnAuth', label: 'node.field.ovpnAuth', ph: 'SHA256', opt: true },
          { t: 'number', k: 'mtu', label: 'node.field.mtu', opt: true },
          { t: 'textarea', k: 'ovpnTlsExtraJson', label: 'node.field.ovpnTlsExtraJson', hint: 'node.field.ovpnTlsExtraJsonHint', mono: true, rows: 4, opt: true },
          { t: 'textarea', k: 'extraJson', label: 'node.field.extraJson', hint: 'node.field.extraJsonHint', mono: true, rows: 4, opt: true },
        ],
      },
    ],
  },
  // SSH：无强制必填字段（后端 protocol_requirement_ok 仅需 address/port）。
  // 算法协商四项（hostKeyAlgorithms / cipher / mac / kexAlgorithm）2026-08-06 已补齐 —— 此前这里写着
  // 「不建模，与 vless 的 ECH/spoof/engine 同级高级逃生舱」，而那批同样已补齐，那句说辞本身就是
  // 「没做完之后补的定性」（详见覆盖矩阵 T1/T2 的裁定）。四项都是 `Vec<String>`，逗号分隔文本框，
  // 与既有的 hostKey 同一写法（同一个 `listFromText` 解析）。
  ssh: {
    cred: [
      { t: 'text', k: 'user', label: 'node.field.user', ph: 'root', opt: true },
      { t: 'text', k: 'pwd', label: 'node.field.pwd', mono: true, opt: true },
    ],
    adv: [
      { t: 'textarea', k: 'privateKey', label: 'node.field.privateKey', mono: true, opt: true, rows: 4 },
      { t: 'text', k: 'privateKeyPath', label: 'node.field.privateKeyPath', ph: '$HOME/.ssh/id_rsa', opt: true },
      { t: 'text', k: 'privateKeyPassphrase', label: 'node.field.privateKeyPassphrase', mono: true, opt: true },
      { t: 'text', k: 'hostKey', label: 'node.field.hostKey', ph: 'ssh-ed25519 AAAA...', opt: true },
      { t: 'text', k: 'hostKeyAlgorithms', label: 'node.field.hostKeyAlgorithms', ph: 'ssh-ed25519,rsa-sha2-256', opt: true },
      { t: 'text', k: 'clientVersion', label: 'node.field.clientVersion', opt: true },
      // 算法协商覆盖（`cipher` / `mac` / `kex_algorithm`，键名以内核 schema 为准：单数、非 ciphers/macs）。
      // 留空 = 删键 = 用 golang.org/x/crypto/ssh 的默认算法集，对接老服务端时才需要覆盖。
      { t: 'text', k: 'cipher', label: 'node.field.cipher', ph: 'aes128-ctr,aes256-gcm@openssh.com', opt: true },
      { t: 'text', k: 'mac', label: 'node.field.mac', ph: 'hmac-sha2-256', opt: true },
      { t: 'text', k: 'kexAlgorithm', label: 'node.field.kexAlgorithm', ph: 'curve25519-sha256', opt: true },
    ],
  },
  // Custom：raw-JSON 透传（第三方内核协议如 snell 之外的更冷门实现）。address/port 由 JSON 内部携带，
  // ServerConfig.address/port 在此协议下不被后端消费（build_proxy_outbound 对 Custom 提前 return）——
  // 顶部「地址/端口」仍要求填写（NodeDialog 通用必填校验，未按协议特判），已知的可接受 UX 折衷，见报告。
  custom: {
    cred: [
      { t: 'textarea', k: 'outbound', label: 'node.field.customOutbound', mono: true, rows: 8, ph: '{"type":"...","server":"...","server_port":443}' },
    ],
    adv: [
      { t: 'switch', k: 'isEndpoint', label: 'node.field.isEndpoint', hint: 'node.field.isEndpointHint' },
      // 说明走 hint（同 h2 那三条的理由）：这条标签在 en-US/fa/ru 下都正好占满 `.fld-l` 的 2 行预算。
      { t: 'text', k: 'secretKeys', label: 'node.field.secretKeys', hint: 'node.field.secretKeysHint', ph: 'password,psk', opt: true },
    ],
  },
};

/** 某协议的全部字段（cred + adv 展平），供草稿构造/显隐过滤。 */
export function allFields(proto: NodeProto): FieldSpec[] {
  const spec = ND_SPEC[proto];
  return spec.groups
    ? spec.groups.flatMap((group) => group.fields)
    : [...spec.cred, ...spec.adv];
}

/**
 * 普通协议仍以 `cred/adv` 作为 codec 真值；这里只描述 UI 信息架构中少量需要移位的字段。
 * 未列入 basic/advanced 的 `adv` 字段自然归入“传输”，因此新增连接参数不会静默消失。
 */
const BASIC_FIELDS_FROM_ADV: Partial<Record<NodeProto, readonly string[]>> = {
  tor: ['torExec', 'torDataDir'],
  ssh: ['privateKey', 'privateKeyPath', 'privateKeyPassphrase'],
};

const ADVANCED_FIELD_KEYS: Partial<Record<NodeProto, readonly string[]>> = {
  vless: ['fragment', 'engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig', 'mux', 'muxProto', 'muxMax', 'muxMin', 'muxPad'],
  vmess: ['fragment', 'engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig', 'mux', 'muxProto', 'muxMax', 'muxMin', 'muxPad'],
  trojan: ['fragment', 'engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig', 'mux', 'muxProto', 'muxMax', 'muxMin', 'muxPad'],
  shadowsocks: ['mux', 'muxProto', 'muxMax', 'muxMin', 'muxPad'],
  hysteria2: ['obfsMin', 'obfsMax', 'bbr', 'noParrot', 'ech', 'echConfig'],
  tuic: ['zeroRtt', 'heartbeat', 'ech', 'echConfig'],
  http: ['fragment', 'engine', 'spoofMethod', 'spoofSni'],
  anytls: ['fragment', 'engine', 'spoofMethod', 'spoofSni', 'ech', 'echConfig', 'idleCheck', 'idleTimeout', 'minIdle'],
  snell: ['reuse', 'userkey'],
  hysteria: ['ech', 'echConfig', 'extraJson'],
  tor: ['torArgs', 'torrcText', 'extraJson'],
  ssh: ['hostKeyAlgorithms', 'clientVersion', 'cipher', 'mac', 'kexAlgorithm'],
  custom: ['isEndpoint', 'secretKeys'],
};

/**
 * 节点表单的统一任务分组：
 * - 普通代理：基础 / 传输（有连接字段时）/ 高级；
 * - OpenConnect、OpenVPN：沿用其明确的基础 / 路由 / 高级模型。
 *
 * 高级分组即使没有协议私有字段也保留，因为所有协议都在这里提供 detour；不会生成空折叠段。
 */
export function nodeFormGroups(proto: NodeProto): NodeFieldGroup[] {
  const spec = ND_SPEC[proto];
  if (spec.groups) return spec.groups;

  const basicFromAdv = new Set(BASIC_FIELDS_FROM_ADV[proto] ?? []);
  const advancedKeys = new Set(ADVANCED_FIELD_KEYS[proto] ?? []);
  const basic = [...spec.cred, ...spec.adv.filter((field) => basicFromAdv.has(field.k))];
  const transport = spec.adv.filter(
    (field) => !basicFromAdv.has(field.k) && !advancedKeys.has(field.k),
  );
  const advanced = spec.adv.filter((field) => advancedKeys.has(field.k));

  return [
    { id: 'basic', fields: basic },
    ...(transport.length > 0 ? [{ id: 'transport' as const, fields: transport }] : []),
    { id: 'advanced', fields: advanced },
  ];
}

// ── C10：custom 协议内核兼容性 probe（`kernel:probeOutbound`）显示态 ──────────────────────
//
// 对齐 `SubDialog.tsx` 的 `runPreview`/`previewMsg` 套路（先探测/预检、按 ok/error 出内联结果条），
// 不是这里首创的新形态。

/**
 * `api.proxy.probeOutbound` 的 IPC 返回形状（对齐 Rust `commands/proxy.rs::probe_verdict`）：
 * `ok` 恒有；`indeterminate`/`error`/`errorPath`/`errorRaw` 均可选——`errorPath` 只在核吐出的诊断里
 * 解析出键路径时才下发（`None` 不下发这个键，不是空串，见 Rust 侧注释）。
 */
export interface ProbeOutboundResult {
  ok: boolean;
  indeterminate?: boolean;
  error?: string;
  errorPath?: string;
  errorRaw?: string;
}

/**
 * 探测结果 → 展示态（discriminated union，穷尽渲染分支）。纯函数，供 `NodeDialog` 渲染 + vitest 直测
 * （`node-spec.test.ts`）——本仓 vitest 无 jsdom，`NodeDialog` 本身测不了，映射逻辑必须能脱离渲染独立验证
 * （同 `field-spec.tsx` 里 `toCselOptions` 的抽离理由）。
 */
export type ProbeDisplay =
  | { kind: 'supported' }
  | { kind: 'indeterminate' }
  | { kind: 'unsupported'; keyPath?: string; message: string; raw: string }
  | { kind: 'invalidJson' };

/**
 * `ProbeOutboundResult` → `ProbeDisplay`。
 *
 * **`indeterminate` 腿不采信后端 `error` 文案**：`probe_verdict` 对这一态目前固定回一句中文
 * （`src-tauri/src/commands/proxy.rs`），若原样透出，非中文界面会看到一句写死的中文——本函数改为
 * 只读 `indeterminate` 标志位，文案由调用方用本地 i18n key 渲染（5 语言各自对）。`unsupported` 腿的
 * `message`/`path`/`raw` 则是核自己的诊断原文，本就是英文技术文本，不翻译（翻了反而对不上用户能
 * 搜到的上游 issue，与本文件其它协议错误提示的既有做法一致）。
 */
export function describeProbeResult(r: ProbeOutboundResult): ProbeDisplay {
  if (r.ok) return { kind: 'supported' };
  if (r.indeterminate) return { kind: 'indeterminate' };
  return {
    kind: 'unsupported',
    keyPath: r.errorPath,
    message: r.error ?? '',
    raw: r.errorRaw ?? r.error ?? '',
  };
}
