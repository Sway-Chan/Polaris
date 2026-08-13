/**
 * protoCodec 对称层 —— ServerConfig ⇄ 表单草稿 的成对映射（§1.4 淬火 R3/R4/R5）。
 *
 * **R5 对称一屏可见**：每协议的 `fromConfig`/`toConfig` 成对定义在同一对象、相邻两行 —— review 时
 * 「回填键 ↔ 提交键」是否对称一眼可核；配套 `proto-codec.test.ts` 逐协议往返 `toConfig(fromConfig(c)) ⊇ c`
 * 给对称性真牙（上游 反例：submit-keys ↔ reset-keys 悄悄不对称丢字段）。
 *
 * **R3/R4 归一只在 config→form 边界一处**（fromConfig）：读取时 `.toLowerCase()` 把存量大写/别名值
 * 归一为规范小写，再进草稿；渲染器与提交路径永远只见规范值（选项集固定小写）。覆盖：
 *  - R3：network / security / vmessSecurity(enc) / tuic congestionControl(cc) / tuic udpRelayMode(udp) /
 *        http isHttps（security==='tls' → tls 开关）；
 *  - R4：uTLS fingerprint(fp) / vless flow。
 * 上游 反例是各表单各自散写归一、漏一个就编辑显占位符；这里边界唯一 → 修一处全好。
 *
 * **保存端非模型字段保全**（R5 延伸）：`toConfig(draft, base)` 以 base 为底 `...base` 起手，只覆写本
 * 协议建模的字段、merge 进嵌套 settings —— 未建模的高级字段（`recordFragment` 之类本仓 Rust 尚未建模的 tls 项等）
 * 随 base 原样保留，编辑不丢。跨协议改型由 NodeDialog 传「干净 base」规避旧协议残留（见 NodeDialog）。
 *
 * **HIGH-1 显隐门（覆盖上面的「保全」）**：`when`-门控的字段组（TLS：sni/fp/insecure；Reality：pbk/sid）
 * 只在当前草稿下**可见**时才下发。判据复用 node-spec 导出的 `whenTls`/`whenReality`（与表单显隐同一谓词，
 * 单一真值）：sec='none' 时整个 `tlsSettings`/`realitySettings` 块被清除（连 base 未建模的 ech 一并丢），
 * 否则 Rust 端 `security.is_tls() || tls_settings.is_some()` 会因残留的 phantom `tlsSettings` 对明文口误开
 * TLS → 代理静默失联。即：TLS 关时「清除」优先于「保全」。
 *
 * 正确性权威在 Rust（§三 Q1）：前端草稿→config 是即时装配层，提交最终门是 invoke 校验。
 */

import type { ServerConfig, Network, Security } from '@/contracts/types';
import type {
  GrpcSettings,
  HttpSettings,
  MultiplexSettings,
  TlsSettings,
  WebSocketSettings,
} from '@/contracts/types/protocol-settings';
import type { FormValue, FormValues } from './field-spec';
import { draftFromSpecs } from './field-spec';
import {
  allFields,
  whenTls,
  whenReality,
  whenHttpTls,
  whenWsLike,
  whenWs,
  whenGrpc,
  whenH2,
  whenMux,
  type NodeProto,
} from './node-spec';

// ── 归一/取值小工具 ──

/** 小写归一（R3/R4）：非空字符串 → 小写；否则 undefined。 */
function lc(v: unknown): string | undefined {
  return typeof v === 'string' && v.trim() !== '' ? v.trim().toLowerCase() : undefined;
}

/** 草稿取字符串：非空 trim 后返回，否则 undefined（空字段 → 不下发该键）。 */
function str(v: FormValue): string | undefined {
  return typeof v === 'string' && v.trim() !== '' ? v.trim() : undefined;
}

/**
 * 多行文本 → CIDR 数组。空行/空白丢弃；一项不剩返回 `undefined`（删键而非写空数组）。
 *
 * 空数组与缺席在这里是实打实的差别：`meshRoutes` 非空 ⇒ 该节点算组网节点（进组网分组、发 force-route）。
 * 写 `[]` 会让 JSON 里留一个空键，而 `isMeshNode` 判的是「有没有非空项」—— 两者语义相同但落盘噪声不同，
 * 与本文件其余字段的既定口径保持一致：不下发的键就不要出现。
 */
function cidrLines(v: FormValue): string[] | undefined {
  if (typeof v !== 'string') return undefined;
  const out = v
    .split(/\r?\n/)
    .map((x) => x.trim())
    .filter(Boolean);
  return out.length ? out : undefined;
}

/** 草稿取数值：仅有限数返回，否则 undefined（number 字段已由 parseNumberField 保证 undefined 语义）。 */
function num(v: FormValue): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

/**
 * 逗号分隔文本 → 字符串数组；**切完一项不剩也返回 undefined**（删键），不写空数组。
 *
 * 「空数组 ≠ 没填」在这批字段上是实打实的差别：ssh 的 `cipher`/`mac`/`kex_algorithm` 下发
 * `[]` 等于显式声明「一个算法都不接受」（而非用 golang.org/x/crypto/ssh 的默认集），
 * trojan 的 `alpn: []` 会把后端 `["http/1.1"]` 的专属缺省顶掉。故 `" , , "` 这种只剩分隔符的
 * 输入必须落回 undefined —— `str()` 只挡得住纯空白，挡不住这种。
 *
 * ssh 的 hostKey / hostKeyAlgorithms / cipher / mac / kexAlgorithm 五个 `Vec<String>`
 * 与 trojan 的 alpn 共用。
 */
function listFromText(v: FormValue): string[] | undefined {
  const items = (str(v) ?? '')
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean);
  return items.length > 0 ? items : undefined;
}

/**
 * 合并进嵌套 settings 块：base 起底保留未建模项，patch 里 undefined 的键**删除**、有值的键覆写。
 * 结果无键 → undefined（不留空壳）。
 */
function mergeBlock<T>(base: T | undefined, patch: Record<string, unknown>): T | undefined {
  const merged: Record<string, unknown> = { ...base };
  for (const [key, val] of Object.entries(patch)) {
    if (val === undefined) delete merged[key];
    else merged[key] = val;
  }
  return Object.keys(merged).length > 0 ? (merged as T) : undefined;
}

/**
 * 合并进 TlsSettings：base 起底保留未建模项（导入器写进来、本仓尚未建模的 tls 项）。
 * 结果空对象 → undefined（security='none' 无 tls 项时不留空壳）。
 */
function mergeTls(base: TlsSettings | undefined, patch: Partial<TlsSettings>): TlsSettings | undefined {
  return mergeBlock<TlsSettings>(base, patch as Record<string, unknown>);
}

/**
 * ws/httpupgrade 的 `Host` 请求头 —— **只增删 `Host` 这一个键**，base 里的其它自定义头原样保留。
 *
 * 两点都是按 Rust 定的，不是风格选择：
 *  - **键名必须是大写 `Host`**：`builder/outbound.rs` 的 httpupgrade 分支读的就是
 *    `headers.get("Host")`（ws 分支则是整份 headers 透传），全仓导入器（clash/xray/singbox/share_link）
 *    写的也都是 `"Host"`。写成小写会让 httpupgrade 找不到、静默回落 `tlsSettings.serverName`。
 *  - **不整份替换**：上游 `buildTransportSettings` 是 `headers: wsHost ? {Host} : undefined`，
 *    编辑一次就把用户其它头（订阅带来的 `User-Agent` 之类）全丢了 —— 那是 `da97add` 修过的同类
 *    数据丢失，不照搬。
 */
function withHostHeader(
  base: Record<string, string> | undefined,
  host: string | undefined
): Record<string, string> | undefined {
  const h: Record<string, string> = { ...base };
  if (host) h.Host = host;
  else delete h.Host;
  return Object.keys(h).length > 0 ? h : undefined;
}

/**
 * `名称: 值` 多行文本 ⇄ `Record<string, string[]>`（h2 传输的 `httpSettings.headers`）。
 *
 * 形态按 Rust 定：`BTreeMap<String, Vec<String>>`，下发时每个头名恒是**数组**
 * （`builder/outbound.rs` 的 h2 腿走 `OneOrMany::Many`），故同名多行合并成一组值而不是后者覆盖前者。
 *
 * 三条丢弃规则（都是「不下发」而非「下发空」）：无冒号的行、头名为空的行、值为空的行 ——
 * `{"X-Foo": [""]}` 在 HTTP 语义上不是「没这个头」，写出去会真发一个空值头。
 * 全部丢完 → `undefined`（删键），同 `listFromText` 对 `" , , "` 的处理。
 */
function headersFromText(v: FormValue): Record<string, string[]> | undefined {
  const out: Record<string, string[]> = {};
  for (const line of (str(v) ?? '').split('\n')) {
    const at = line.indexOf(':');
    if (at <= 0) continue;
    const name = line.slice(0, at).trim();
    const value = line.slice(at + 1).trim();
    if (name === '' || value === '') continue;
    if (out[name] === undefined) out[name] = [];
    out[name].push(value);
  }
  return Object.keys(out).length > 0 ? out : undefined;
}

/** `Record<string, string[]>` → `名称: 值` 多行文本（与 [`headersFromText`] 成对，往返恒等）。 */
function headersToText(h: Record<string, string[]> | undefined): string {
  if (h === undefined) return '';
  return Object.entries(h)
    .flatMap(([name, values]) => values.map((value) => `${name}: ${value}`))
    .join('\n');
}

/**
 * 传输块（ws/httpupgrade 的 `wsSettings` + grpc 的 `grpcSettings`）的成对读写，vless/vmess/trojan 共用。
 *
 * **不匹配当前传输 ⇒ 整块清除**（同 上游 `buildTransportSettings` 的 `null`，也同本文件 TLS 组的
 * HIGH-1 清除门）：留着旧块虽然在 Rust 侧是惰性的（`generate_transport_config` 按 `network` 单分支
 * 取值，不会串读），但那是把「配置里有什么」和「生效的是什么」拆成两件事——切回 ws 时又会拿到
 * 一份用户以为早就删掉的旧 path。
 *
 * 留空语义逐字段按 Rust 定，三个都是「不下发该键」而非「下发缺省值」：
 *  - `path` 缺席 → `ws.and_then(|w| w.path).unwrap_or("/")` = `/`，与显式写 `/` 逐字节同结果；
 *  - `Host` 缺席 → ws 不发该 header；httpupgrade 回落 `tlsSettings.serverName`（**显式写空串会把这条
 *    回落堵死**，故必须删键）；
 *  - `serviceName` 缺席 → `unwrap_or_default()` = 空串，与显式写空串同结果。
 */
function transportPatch(
  draft: FormValues,
  base: ServerConfig
): Pick<ServerConfig, 'wsSettings' | 'grpcSettings'> {
  return {
    wsSettings: whenWsLike(draft)
      ? mergeBlock<WebSocketSettings>(base.wsSettings, {
          path: str(draft.wsPath),
          headers: withHostHeader(base.wsSettings?.headers, str(draft.wsHost)),
          // 早数据两键**只在 `ws` 腿参与 patch**（既不写也不删）：httpupgrade 分支在 Rust 侧
          // 根本不读它们（内核 schema 的 httpupgrade 传输也没这两键）⇒ 在那条腿上它们既非
          // 「隐藏的可见字段」（HIGH-1 要清的那种残留会改变行为，这两个不会），也不该被顺手删掉
          // 而丢掉用户从 ws 带过来的值。故按传输腿决定它们进不进 patch。
          ...(whenWs(draft)
            ? {
                maxEarlyData: num(draft.wsMaxEarlyData),
                earlyDataHeaderName: str(draft.wsEdHeader),
              }
            : {}),
        })
      : undefined,
    grpcSettings: whenGrpc(draft)
      ? mergeBlock<GrpcSettings>(base.grpcSettings, { serviceName: str(draft.grpcServiceName) })
      : undefined,
  };
}

/** 传输块 → 草稿（五个键与 `F_TRANSPORT` 的 ws/grpc 那几颗 `k` 一一对应）。 */
function transportDraft(cfg: ServerConfig, d: FormValues): void {
  d.wsPath = cfg.wsSettings?.path ?? '';
  d.wsHost = cfg.wsSettings?.headers?.Host ?? '';
  d.wsMaxEarlyData = cfg.wsSettings?.maxEarlyData;
  d.wsEdHeader = cfg.wsSettings?.earlyDataHeaderName ?? '';
  d.grpcServiceName = cfg.grpcSettings?.serviceName ?? '';
}

/**
 * HTTP/2 传输块（`httpSettings`）的成对读写，vless / vmess / trojan 共用。
 *
 * **刻意与 [`transportPatch`] 分成两个函数**，不是拆得过细：`protocol-settings-coverage.test.ts` 的
 * 归属过滤按「helper 文本里点没点名该结构体」决定它算不算某结构体的覆盖。合成一个函数的话，它会
 * 同时点名 `WebSocketSettings` 与 `HttpSettings`，于是 ws 那份 `path:`/`headers:` 又会盖到
 * `HttpSettings.path`/`headers` 头上 —— 那正是批 C 花力气消掉的那笔假账。分开写，两个结构体的
 * 覆盖各自独立可核。
 *
 * **不匹配当前传输 ⇒ 整块清除**（同 `transportPatch` 与 上游 `buildTransportSettings` 的 `null`）：
 * 切到 ws 之后留着旧的 `httpSettings` 会让「配置里有什么」和「生效的是什么」变成两件事。
 */
function httpPatch(draft: FormValues, base: ServerConfig): Pick<ServerConfig, 'httpSettings'> {
  return {
    httpSettings: whenH2(draft)
      ? mergeBlock<HttpSettings>(base.httpSettings, {
          path: str(draft.h2Path),
          // `Vec<String>`：单元素时后端序列化成裸串、多元素成数组（`OneOrMany`），两种都合法。
          // 走 `listFromText` 而非裸 split —— `" , , "` 必须落回删键而不是 `[]`（内核会当成「没有 Host」
          // 的另一种写法，但空数组在别的键上咬过人，此处保持全仓一致的空值语义）。
          host: listFromText(draft.h2Host),
          method: str(draft.h2Method),
          headers: headersFromText(draft.h2Headers),
        })
      : undefined,
  };
}

/** h2 传输块 → 草稿（四个键与 `F_TRANSPORT` 里 `whenH2` 那几颗 `k` 一一对应）。 */
function httpDraft(cfg: ServerConfig, d: FormValues): void {
  d.h2Path = cfg.httpSettings?.path ?? '';
  d.h2Host = cfg.httpSettings?.host?.join(',') ?? '';
  d.h2Method = cfg.httpSettings?.method ?? '';
  d.h2Headers = headersToText(cfg.httpSettings?.headers);
}

/**
 * TLS 高级组（alpn / fragment / engine / spoof / ech）的 patch 片段 —— vless/vmess/trojan/anytls/http 共用，
 * 与 `node-spec.ts` 的 `tlsAdvFields` 成对（一个管显隐、一个管读写，键名单一真值）。
 *
 * `ech` 由 `withEch` 控制**是否进 patch**（不是写不写）：http 那张表没有 ECH 控件，若把
 * `ech: undefined` 塞进 patch，`mergeBlock` 会把存量 http 节点上由订阅/导入带来的 ech 删掉 ——
 * 未建模字段该走 base 起底保全，不该被一个不存在的控件顺手清空。
 *
 * `alpn` 无条件进 patch（五个协议的表单都有这颗控件）。留空 → 删键：后端 `final_alpn` 只有 trojan
 * 有专属缺省 `["http/1.1"]`，下发 `[]` 会把它顶掉；其余协议 `[]` 与「没填」在内核侧也不等价。
 */
function tlsAdvPatch(draft: FormValues, withEch: boolean): Partial<TlsSettings> {
  const spoofMethod = str(draft.spoofMethod);
  const spoofSni = str(draft.spoofSni);
  // **齐备才写**（同 ShadowTLS）：后端 `validate_tls_spoof_default` 要求方法合法 **且** 诱饵 SNI 非空，
  // 只落一半在磁盘上是一对永不生效的死键，而用户会以为自己开了 spoof。
  const spoofOn = spoofMethod !== undefined && spoofSni !== undefined;
  // 返回类型写死 `Partial<TlsSettings>`（而非 `Record<string, unknown>`）是为了让键名拼错编译期就红：
  // 这几个键都是「写错了也只是静默不生效」的形态，没有运行时报错兜底。
  return {
    // 空 / 纯分隔符 → 删键（见函数头与 `listFromText` 注释）。
    alpn: listFromText(draft.alpn),
    // 关 → 删键（**不写 `false`**）：后端判据是 `tls_s.fragment == Some(true)`，`None` 与 `Some(false)`
    // 逐字节同结果 ⇒ 写 false 只是给每份存量配置多一个语义等价的键（同 muxPad / zeroRtt 的既有写法）。
    fragment: draft.fragment === true ? true : undefined,
    // 空 → 删键，后端等价 `go`（`should_emit_tls_engine` 只认 windows/apple 且要平台匹配）。
    //
    // **reality 下不需要特判**：控件那时是隐藏的（`whenTlsEngine`），用户改不动，而 `fromConfig`
    // 已把存量值原样读进草稿 ⇒ 照写回去就是保全，与「跳过不写」逐字节同结果。曾写过一个
    // `whenReality(draft) ? {} : {...}` 的条件分支，变异实测证明它是**等价变异**（删掉它没有任何
    // 断言转红，因为草稿与 base 在这条链路上恒同源），按简约阶梯去掉——多一个分支只是多一处要维护的
    // 判据。防「reality 下露出假控件」那件事全靠 `whenTlsEngine` 那道门（变异实测 M16 转红）。
    engine: str(draft.engine) as TlsSettings['engine'],
    spoofMethod: spoofOn ? (spoofMethod as TlsSettings['spoofMethod']) : undefined,
    spoofSni: spoofOn ? spoofSni : undefined,
    ...(withEch
      ? {
          ech: draft.ech === true ? true : undefined,
          echConfig: draft.ech === true ? str(draft.echConfig) : undefined,
        }
      : {}),
  };
}

/** TLS 高级组 → 草稿（`withEch` 语义同 `tlsAdvPatch`）。 */
function tlsAdvDraft(cfg: ServerConfig, d: FormValues, withEch: boolean): void {
  d.alpn = cfg.tlsSettings?.alpn?.join(',') ?? '';        // 不归一：ALPN 协议名大小写敏感（`h2` ≠ `H2`）
  d.fragment = cfg.tlsSettings?.fragment === true;        // 存量的 false / 缺席都回落成「关」
  d.engine = lc(cfg.tlsSettings?.engine) ?? '';           // R3：'Windows' 这类变体归一，否则后端精确匹配不上
  d.spoofMethod = lc(cfg.tlsSettings?.spoofMethod) ?? ''; // R3：同上（is_valid_tls_spoof_method 是精确比较）
  d.spoofSni = cfg.tlsSettings?.spoofSni ?? '';
  if (withEch) {
    d.ech = cfg.tlsSettings?.ech === true;
    d.echConfig = cfg.tlsSettings?.echConfig ?? '';
  }
}

/**
 * Multiplex 的成对读写（vless / vmess / trojan / shadowsocks）。
 *
 * **整块重建、不 `...base` 起底**：`MultiplexSettings` 的 5 个字段全部建模，没有需要保全的未建模项，
 * 起底反而会把「用户关掉某项」变成「旧值残留」。
 *
 * **门是 `whenMux`（含 vision 判据）**，与表单显隐同一谓词：后端
 * `apply_anti_censorship_options` 在 `flow` 含 `vision` 时整段跳过 multiplex ⇒ 那时留着
 * `multiplexSettings` 就是「配置里有、实际不生效」。上游的 `buildMultiplexSettings(…, {skipVisionFlow:true})`
 * 同样返回 undefined，此处行为一致。
 */
function muxPatch(draft: FormValues): Pick<ServerConfig, 'multiplexSettings'> {
  return {
    multiplexSettings: whenMux(draft)
      ? {
          enabled: true,
          // 空不可能（选项集无空档）；后端缺省也是 h2mux，显式写与不写同结果。
          protocol: str(draft.muxProto) as MultiplexSettings['protocol'],
          maxConnections: num(draft.muxMax),
          minStreams: num(draft.muxMin),
          // 关 → 删键（内核默认 false）：写 false 只是给每份配置多一个语义等价的键。
          padding: draft.muxPad === true ? true : undefined,
        }
      : undefined,
  };
}

/** Multiplex → 草稿（五个键与 `F_MUX` 的 `k` 一一对应）。 */
function muxDraft(cfg: ServerConfig, d: FormValues): void {
  d.mux = cfg.multiplexSettings?.enabled === true;
  d.muxProto = lc(cfg.multiplexSettings?.protocol) ?? 'h2mux'; // R3；缺省与后端 unwrap_or("h2mux") 同值
  d.muxMax = cfg.multiplexSettings?.maxConnections;
  d.muxMin = cfg.multiplexSettings?.minStreams;
  d.muxPad = cfg.multiplexSettings?.padding === true;
}

/** 协议编解码对（R5：from/to 成对）。 */
export interface ProtoCodec {
  /** ServerConfig → 表单草稿（含 R3/R4 归一）。缺省字段回落 draftFromSpecs 的默认。 */
  fromConfig(cfg: ServerConfig): FormValues;
  /** 表单草稿 → ServerConfig（以 base 保全非模型字段）。 */
  toConfig(draft: FormValues, base: ServerConfig): ServerConfig;
}

/** 该协议草稿默认（select 首项 / switch false / number undefined / text ''）。 */
function base0(proto: NodeProto): FormValues {
  return draftFromSpecs(allFields(proto));
}

/** torrc map ⇄ 原生语法文本（每行 `Key Value`）。 */
const torrcToText = (m: unknown): string =>
  m && typeof m === 'object'
    ? Object.entries(m as Record<string, string>)
        .map(([k, v]) => (v ? `${k} ${v}` : k))
        .join('\n')
    : '';
/** 解析 torrc 文本 → map。空行与 `#` 注释丢弃（内核侧是 map，承载不了它们）。 */
const textToTorrc = (v: unknown): Record<string, string> => {
  const t = typeof v === 'string' ? v : '';
  const out: Record<string, string> = {};
  for (const raw of t.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const i = line.search(/\s/);
    if (i < 0) out[line] = '';
    else out[line.slice(0, i)] = line.slice(i + 1).trim();
  }
  return out;
};

/** 从设置对象里取出「非建模键」= 透传袋内容。建模键名单与 Rust 侧同源。 */
const MODELED_SETTING_KEYS: readonly string[] = [
  'authStr', 'auth', 'upMbps', 'downMbps', 'obfs', 'serverPorts', 'hopInterval',
  'executablePath', 'dataDirectory', 'extraArgs', 'torrc',
  'server', 'server_port', 'username', 'password', 'flavor', 'auth_group', 'token', 'mtu',
  'no_udp', 'pfs', 'allow_insecure_crypto', 'user_agent', 'reported_os', 'system',
  'network', 'cipher', 'redirect_gateway', 'tls',
];
const bagOf = (settings: unknown): Record<string, unknown> => {
  if (!settings || typeof settings !== 'object') return {};
  return Object.fromEntries(
    Object.entries(settings as Record<string, unknown>).filter(
      ([k]) => !MODELED_SETTING_KEYS.includes(k)
    )
  );
};

/** OpenVPN `tls` 是独立嵌套命名空间，不能复用父 settings 的建模键表。 */
const OPENVPN_TLS_KEYS = ['certificate', 'client_certificate', 'client_key'] as const;
const openvpnTlsBagOf = (tls: unknown): Record<string, unknown> => {
  if (!tls || typeof tls !== 'object') return {};
  return Object.fromEntries(
    Object.entries(tls as Record<string, unknown>).filter(
      ([key]) => !OPENVPN_TLS_KEYS.includes(key as (typeof OPENVPN_TLS_KEYS)[number])
    )
  );
};

/** OpenConnect 的内核字段为单串 `host:port`；IPv6 地址必须补方括号。 */
export function endpointHostPort(address: string, port: number): string {
  const host = address.trim();
  const bracketed = host.startsWith('[') && host.endsWith(']');
  return `${host.includes(':') && !bracketed ? `[${host}]` : host}:${port}`;
}

/** 透传袋 ⇄ 表单：袋子在表单里是一段原样 JSON。空对象 → 空串（不给用户看 `{}`）。 */
const bagToText = (bag: unknown): string => {
  const m = bag && typeof bag === 'object' ? (bag as Record<string, unknown>) : {};
  const keys = Object.keys(m);
  return keys.length ? JSON.stringify(m, null, 2) : '';
};
/** 解析失败 → `undefined`（保留 base 里的旧袋，不把用户手误变成静默清空）。 */
const textToBag = (v: unknown): Record<string, unknown> | undefined => {
  const t = typeof v === 'string' ? v.trim() : '';
  if (!t) return {};
  try {
    const parsed: unknown = JSON.parse(t);
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : undefined;
  } catch {
    return undefined;
  }
};

export const protoCodec: Record<NodeProto, ProtoCodec> = {
  vless: {
    fromConfig(cfg) {
      const d = base0('vless');
      d.uuid = cfg.uuid ?? '';
      d.flow = lc(cfg.flow) ?? '';                       // R4
      d.net = lc(cfg.network) ?? 'tcp';                  // R3
      d.sec = lc(cfg.security) ?? 'none';                // R3
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.fp = lc(cfg.tlsSettings?.fingerprint) ?? '';     // R4
      d.pbk = cfg.realitySettings?.publicKey ?? '';
      d.sid = cfg.realitySettings?.shortId ?? '';
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      transportDraft(cfg, d);
      httpDraft(cfg, d);
      tlsAdvDraft(cfg, d, true);
      muxDraft(cfg, d);
      return d;
    },
    toConfig(draft, base) {
      const pbk = str(draft.pbk);
      return {
        ...base,
        uuid: str(draft.uuid),
        flow: str(draft.flow),
        network: str(draft.net) as Network | undefined,
        security: str(draft.sec) as Security | undefined,
        ...transportPatch(draft, base),
        ...httpPatch(draft, base),
        ...muxPatch(draft),
        // HIGH-1：TLS 组仅在 whenTls（sec ∈ {tls,reality}，与表单显隐同源）可见时下发；sec='none' 时
        // 整块清除（含 base 未建模项如 fragment），否则 Rust `tls_settings.is_some()` 会对明文口误开 TLS → 代理静默失联。
        tlsSettings: whenTls(draft)
          ? mergeTls(base.tlsSettings, {
              serverName: str(draft.sni),
              fingerprint: str(draft.fp),
              allowInsecure: draft.insecure === true ? true : undefined,
              ...tlsAdvPatch(draft, true),
            })
          : undefined,
        // HIGH-1 + LOW-9：Reality 组仅在 whenReality（sec==='reality'）且 pbk 有值时下发；关闭 reality 或
        // 清空 pbk → 整块清除（旧实现回落 base.realitySettings 留陈旧 publicKey，无法清除）。
        realitySettings: whenReality(draft) && pbk
          ? { ...base.realitySettings, publicKey: pbk, shortId: str(draft.sid) }
          : undefined,
      };
    },
  },

  vmess: {
    fromConfig(cfg) {
      const d = base0('vmess');
      d.uuid = cfg.uuid ?? '';
      d.aid = typeof cfg.alterId === 'number' ? cfg.alterId : undefined;
      d.enc = lc(cfg.vmessSecurity) ?? 'auto';           // R3
      d.net = lc(cfg.network) ?? 'tcp';                  // R3
      d.sec = lc(cfg.security) ?? 'none';                // R3
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.fp = lc(cfg.tlsSettings?.fingerprint) ?? '';     // R4；空=后端缺省 none（不下发 utls 块）
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      transportDraft(cfg, d);
      httpDraft(cfg, d);
      tlsAdvDraft(cfg, d, true);
      muxDraft(cfg, d);
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        uuid: str(draft.uuid),
        alterId: num(draft.aid),
        vmessSecurity: str(draft.enc),
        network: str(draft.net) as Network | undefined,
        security: str(draft.sec) as Security | undefined,
        ...transportPatch(draft, base),
        ...httpPatch(draft, base),
        ...muxPatch(draft),
        // HIGH-1：sec='none' 时不下发 tlsSettings（含 base 既有项），防 Rust is_some 误开 TLS。
        tlsSettings: whenTls(draft)
          ? mergeTls(base.tlsSettings, {
              serverName: str(draft.sni),
              fingerprint: str(draft.fp),
              allowInsecure: draft.insecure === true ? true : undefined,
              ...tlsAdvPatch(draft, true),
            })
          : undefined,
      };
    },
  },

  trojan: {
    fromConfig(cfg) {
      const d = base0('trojan');
      d.pwd = cfg.password ?? '';
      d.net = lc(cfg.network) ?? 'tcp';                  // R3
      d.sec = lc(cfg.security) ?? 'tls';                 // R3
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.fp = lc(cfg.tlsSettings?.fingerprint) ?? '';     // R4；空=后端缺省 none（不下发 utls 块）
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      transportDraft(cfg, d);
      httpDraft(cfg, d);
      tlsAdvDraft(cfg, d, true);
      muxDraft(cfg, d);
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        password: str(draft.pwd),
        network: str(draft.net) as Network | undefined,
        security: str(draft.sec) as Security | undefined,
        ...transportPatch(draft, base),
        ...httpPatch(draft, base),
        ...muxPatch(draft),
        // HIGH-1：sec='none' 时不下发 tlsSettings（含 base 既有项），防 Rust is_some 误开 TLS。
        tlsSettings: whenTls(draft)
          ? mergeTls(base.tlsSettings, {
              serverName: str(draft.sni),
              fingerprint: str(draft.fp),
              // `alpn` 由 `tlsAdvPatch` 统一处理（此前是这里一行）—— 同一个键跨 5 个协议，
              // 留在各协议块里必然漂移。空值语义不变：删键 → 后端补 trojan 专属缺省 `["http/1.1"]`。
              allowInsecure: draft.insecure === true ? true : undefined,
              ...tlsAdvPatch(draft, true),
            })
          : undefined,
      };
    },
  },

  shadowsocks: {
    fromConfig(cfg) {
      const d = base0('shadowsocks');
      d.method = lc(cfg.shadowsocksSettings?.method) ?? '2022-blake3-aes-128-gcm'; // R3
      d.pwd = cfg.shadowsocksSettings?.password ?? '';
      d.plugin = cfg.shadowsocksSettings?.plugin ?? '';
      d.pluginOpts = cfg.shadowsocksSettings?.pluginOptions ?? '';
      muxDraft(cfg, d);
      d.stls = cfg.shadowTlsSettings != null;
      d.stlsPwd = cfg.shadowTlsSettings?.password ?? '';
      d.stlsSni = cfg.shadowTlsSettings?.sni ?? '';
      d.stlsFp = lc(cfg.shadowTlsSettings?.fingerprint) ?? 'chrome';  // R4；缺省与后端消费点默认一致
      d.stlsPort = typeof cfg.shadowTlsSettings?.port === 'number' ? cfg.shadowTlsSettings.port : undefined;
      return d;
    },
    toConfig(draft, base) {
      const stlsPwd = str(draft.stlsPwd);
      const stlsSni = str(draft.stlsSni);
      return {
        ...base,
        ...muxPatch(draft),
        shadowsocksSettings: {
          ...base.shadowsocksSettings,
          method: str(draft.method) ?? '',
          password: str(draft.pwd) ?? '',
          // SIP003 插件：留空 → 删键（后端 `ob.plugin = ss.plugin.clone()` 原样透传 Option，
          // 写空串会让内核收到 `"plugin": ""` 并去找一个叫空串的插件）。两键各自独立。
          plugin: str(draft.plugin),
          pluginOptions: str(draft.pluginOpts),
        },
        // ShadowTLS：**齐备才写**（照 上游 ss-form.tsx 的 `enableShadowTls && password && sni`）。
        // 旧实现是「开关一开就写 `{password:'', sni:''}`」——后端 `builder/outbounds.rs`
        // `apply_shadow_tls_postprocess` 只看 `shadow_tls_settings.is_some()`，于是造出 password 为空串、
        // server_name 缺席的外层 shadowtls 出站并把 SS 的 detour 指过去 ⇒ 该节点必然连不上。
        // 缺一项即整块不下发（= 用户改回了「不启用」），而不是下发半成品。
        shadowTlsSettings: draft.stls === true && stlsPwd && stlsSni
          ? {
              ...base.shadowTlsSettings,
              password: stlsPwd,
              sni: stlsSni,
              // 空 → 删键；后端 `unwrap_or_else(|| "chrome")` 兜底，与草稿默认同值。
              fingerprint: str(draft.stlsFp),
              // 空 → 删键；后端 None/0 都降级用节点主端口。
              port: num(draft.stlsPort),
            }
          : undefined,
      };
    },
  },

  // OpenConnect：server 是 `host:port` **单串**（内核这支就是整串，不拆两个键）。
  // 其余键名与 sing-box 逐字一致 —— Rust 侧把设置结构整体序列化后 flatten 进 endpoint，
  // 这里改键名就是改下发内容。
  openconnect: {
    fromConfig(cfg) {
      const d = base0('openconnect');
      d.user = cfg.openconnectSettings?.username ?? '';
      d.pwd = cfg.openconnectSettings?.password ?? '';
      d.flavor = cfg.openconnectSettings?.flavor ?? 'anyconnect';
      d.authGroup = cfg.openconnectSettings?.auth_group ?? '';
      d.token = cfg.openconnectSettings?.token ?? '';
      d.mtu = cfg.openconnectSettings?.mtu;
      d.noUdp = cfg.openconnectSettings?.no_udp === true;
      d.pfs = cfg.openconnectSettings?.pfs === true;
      d.insecureCrypto = cfg.openconnectSettings?.allow_insecure_crypto === true;
      d.userAgent = cfg.openconnectSettings?.user_agent ?? '';
      d.reportedOs = cfg.openconnectSettings?.reported_os ?? '';
      d.sysIface = cfg.openconnectSettings?.system === true;
      d.meshRoutes = (cfg.meshRoutes ?? []).join('\n');
      d.extraJson = bagToText(bagOf(cfg.openconnectSettings));
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        // 顶层字段：**不进 openconnectSettings** —— 那个块整体 flatten 下发给内核，塞个内核不认的键会硬报错。
        meshRoutes: cidrLines(draft.meshRoutes),
        openconnectSettings: {
          ...base.openconnectSettings,
          ...(textToBag(draft.extraJson) ?? bagOf(base.openconnectSettings)),
          server: base.address && base.port ? endpointHostPort(base.address, base.port) : undefined,
          username: str(draft.user),
          password: str(draft.pwd),
          flavor: str(draft.flavor),
          auth_group: str(draft.authGroup),
          token: str(draft.token),
          mtu: num(draft.mtu),
          no_udp: draft.noUdp === true ? true : undefined,
          pfs: draft.pfs === true ? true : undefined,
          allow_insecure_crypto: draft.insecureCrypto === true ? true : undefined,
          user_agent: str(draft.userAgent),
          reported_os: str(draft.reportedOs),
          system: draft.sysIface === true ? true : undefined,
        },
      };
    },
  },

  // OpenVPN 客户端：证书类字段在表单里是**多行文本**，落盘是 PEM 逐行数组（内核要的形态）。
  'openvpn-client': {
    fromConfig(cfg) {
      const d = base0('openvpn-client');
      d.user = cfg.openvpnClientSettings?.username ?? '';
      d.pwd = cfg.openvpnClientSettings?.password ?? '';
      d.ovpnCa = cfg.openvpnClientSettings?.tls?.certificate?.join('\n') ?? '';
      d.ovpnCert = cfg.openvpnClientSettings?.tls?.client_certificate?.join('\n') ?? '';
      d.ovpnKey = cfg.openvpnClientSettings?.tls?.client_key?.join('\n') ?? '';
      d.network = cfg.openvpnClientSettings?.network ?? '';
      d.cipher = cfg.openvpnClientSettings?.cipher ?? '';
      d.ovpnAuth = cfg.openvpnClientSettings?.auth ?? '';
      d.mtu = cfg.openvpnClientSettings?.mtu;
      d.redirectGw = cfg.openvpnClientSettings?.redirect_gateway === true;
      d.sysIface = cfg.openvpnClientSettings?.system === true;
      d.meshRoutes = (cfg.meshRoutes ?? []).join('\n');
      d.extraJson = bagToText(bagOf(cfg.openvpnClientSettings));
      d.ovpnTlsExtraJson = bagToText(openvpnTlsBagOf(cfg.openvpnClientSettings?.tls));
      return d;
    },
    toConfig(draft, base) {
      const lines = (v: FormValue | undefined): string[] | undefined => {
        const t = str(v);
        return t ? t.split(/\r?\n/).map((x) => x.trim()).filter(Boolean) : undefined;
      };
      return {
        ...base,
        meshRoutes: cidrLines(draft.meshRoutes),
        openvpnClientSettings: {
          ...base.openvpnClientSettings,
          ...(textToBag(draft.extraJson) ?? bagOf(base.openvpnClientSettings)),
          server: base.address || undefined,
          server_port: base.port || undefined,
          username: str(draft.user),
          password: str(draft.pwd),
          network: str(draft.network),
          cipher: str(draft.cipher),
          auth: str(draft.ovpnAuth),
          mtu: num(draft.mtu),
          // 关闭时**显式写 false**，不留缺省：这个开关的关态正是「只走声明的内网段、其余直连」那个
          // 用法的表达，而 `meshAllowsInternet` 判的就是它显式为 false。写 undefined 会让谓词按缺省
          // 判「承载全隧道」⇒ 用户关了开关，出口兜底却不生效。
          redirect_gateway: draft.redirectGw === true ? true : false,
          system: draft.sysIface === true ? true : undefined,
          tls: {
            ...(textToBag(draft.ovpnTlsExtraJson) ?? openvpnTlsBagOf(base.openvpnClientSettings?.tls)),
            certificate: lines(draft.ovpnCa),
            client_certificate: lines(draft.ovpnCert),
            client_key: lines(draft.ovpnKey),
          },
        },
      };
    },
  },

  // Hysteria v1：与 hysteria2 同名不同义的字段有两个 —— obfs（v1 是裸口令串，v2 是类型选单）
  // 与认证（v1 走 authStr，v2 走 password）。刻意各写各的，不复用 hy2 那条腿。
  hysteria: {
    fromConfig(cfg) {
      const d = base0('hysteria');
      d.authStr = cfg.hysteriaSettings?.authStr ?? '';
      d.up = cfg.hysteriaSettings?.upMbps;
      d.down = cfg.hysteriaSettings?.downMbps;
      d.obfs = cfg.hysteriaSettings?.obfs ?? '';
      d.ports = cfg.hysteriaSettings?.serverPorts ?? '';
      d.hopInterval = cfg.hysteriaSettings?.hopInterval ?? '';
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.alpn = cfg.tlsSettings?.alpn?.join(',') ?? '';
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      d.ech = cfg.tlsSettings?.ech === true;
      d.echConfig = cfg.tlsSettings?.echConfig ?? '';
      d.extraJson = bagToText(bagOf(cfg.hysteriaSettings));
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        hysteriaSettings: {
          ...base.hysteriaSettings,
          authStr: str(draft.authStr),
          upMbps: num(draft.up),
          downMbps: num(draft.down),
          obfs: str(draft.obfs),
          serverPorts: str(draft.ports),
          hopInterval: str(draft.hopInterval),
          ...(textToBag(draft.extraJson) ?? bagOf(base.hysteriaSettings)),
        },
        // v1 的 TLS 恒开（后端 TLS_PROTOCOLS 含 hysteria），无 sec 开关，故与 hy2 同样无清除门。
        tlsSettings: mergeTls(base.tlsSettings, {
          serverName: str(draft.sni),
          alpn: str(draft.alpn) ? str(draft.alpn)!.split(',').map((x) => x.trim()) : undefined,
          allowInsecure: draft.insecure === true ? true : undefined,
          ech: draft.ech === true ? true : undefined,
          echConfig: str(draft.echConfig),
        }),
      };
    },
  },

  // Tor：**无 server/port**（生成侧显式清空；实测传 server 会让整个核 decode 失败）。
  // 故本条不读写 address/port，也没有凭据字段。
  tor: {
    fromConfig(cfg) {
      const d = base0('tor');
      d.torExec = cfg.torSettings?.executablePath ?? '';
      d.torDataDir = cfg.torSettings?.dataDirectory ?? '';
      d.torArgs = cfg.torSettings?.extraArgs?.join(' ') ?? '';
      d.torrcText = torrcToText(cfg.torSettings?.torrc);
      d.extraJson = bagToText(bagOf(cfg.torSettings));
      return d;
    },
    toConfig(draft, base) {
      const args = str(draft.torArgs);
      return {
        ...base,
        torSettings: {
          ...base.torSettings,
          executablePath: str(draft.torExec),
          dataDirectory: str(draft.torDataDir),
          extraArgs: args ? args.split(/\s+/).filter(Boolean) : undefined,
          torrc: textToTorrc(draft.torrcText),
          ...(textToBag(draft.extraJson) ?? bagOf(base.torSettings)),
        },
      };
    },
  },

  hysteria2: {
    fromConfig(cfg) {
      const d = base0('hysteria2');
      d.pwd = cfg.password ?? '';
      d.up = cfg.hysteria2Settings?.upMbps;
      d.down = cfg.hysteria2Settings?.downMbps;
      d.obfs = lc(cfg.hysteria2Settings?.obfs?.type) ?? '';   // R3：混淆类型归一（salamander/gecko）
      d.obfspwd = cfg.hysteria2Settings?.obfs?.password ?? '';
      d.obfsMin = cfg.hysteria2Settings?.obfs?.minPacketSize;   // gecko 随机填充包长（仅 gecko 显）
      d.obfsMax = cfg.hysteria2Settings?.obfs?.maxPacketSize;
      d.bbr = lc(cfg.hysteria2Settings?.bbrProfile) ?? '';    // R3：bbr_profile 归一（空=核心默认）
      d.noParrot = cfg.hysteria2Settings?.disableChromeParrot === true; // 关 Chrome 拟态（Ed25519 逃生舱）
      d.ports = cfg.hysteria2Settings?.serverPorts ?? '';
      d.hopInterval = cfg.hysteria2Settings?.hopInterval ?? '';
      d.network = lc(cfg.hysteria2Settings?.network) ?? '';   // R3：出站可用网络（tcp/udp，空=不限制）
      d.sni = cfg.tlsSettings?.serverName ?? '';              // hy2 TLS 恒开，无 sec 门（同 anytls）
      d.alpn = cfg.tlsSettings?.alpn?.join(',') ?? '';        // 不归一：ALPN 协议名大小写敏感
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      d.ech = cfg.tlsSettings?.ech === true;                  // ECH（hy2 TLS 恒开，QUIC 自管）
      d.echConfig = cfg.tlsSettings?.echConfig ?? '';
      return d;
    },
    toConfig(draft, base) {
      const obfsType = str(draft.obfs);
      const isGecko = obfsType === 'gecko';
      return {
        ...base,
        password: str(draft.pwd),
        hysteria2Settings: {
          ...base.hysteria2Settings,
          upMbps: num(draft.up),
          downMbps: num(draft.down),
          serverPorts: str(draft.ports),
          hopInterval: str(draft.hopInterval),
          // 空 → 删键 = 内核缺省「tcp+udp 都走」；选单侧才下发（`ob.network = h.network.clone()`）。
          network: str(draft.network) as 'tcp' | 'udp' | undefined,
          // 空=核心默认拥塞控制；仅 standard/aggressive/conservative 合法（选项集已约束，Rust 端 sing-box 终校）。
          bbrProfile: str(draft.bbr) as 'standard' | 'aggressive' | 'conservative' | undefined,
          // 关=删键（不写 false）：核心默认就是 false，写进去只会给每份配置凭空多一个键，语义并无差别。
          disableChromeParrot: draft.noParrot === true ? true : undefined,
          obfs: obfsType
            ? {
                ...base.hysteria2Settings?.obfs,
                type: obfsType as 'salamander' | 'gecko',
                password: str(draft.obfspwd),
                // 随机填充包长仅 gecko 有意义——salamander 清空防脏下发（对齐后端 build 只在 gecko 读 min/max）。
                minPacketSize: isGecko ? num(draft.obfsMin) : undefined,
                maxPacketSize: isGecko ? num(draft.obfsMax) : undefined,
              }
            : undefined,
        },
        // TLS：hy2 TLS 恒开（后端 TLS_PROTOCOLS 含 hysteria2），无 sec 开关，故无 HIGH-1 清除门；
        // mergeTls 以 base 起底保全其余未建模 tls 项（fragment/spoof…）。已建模的键空→删键、有值→写入。
        tlsSettings: mergeTls(base.tlsSettings, {
          serverName: str(draft.sni),
          // hy2 **不受 `is_quic_managed_tls` 那道门约束**（它挡的是 engine/fingerprint/fragment/spoof）：
          // `final_alpn` 照常下发。留空 → 删键 → 内核用 hy2 自己的缺省 `h3`。
          alpn: listFromText(draft.alpn),
          allowInsecure: draft.insecure === true ? true : undefined,
          ech: draft.ech === true ? true : undefined,
          echConfig: draft.ech === true ? str(draft.echConfig) : undefined,
        }),
      };
    },
  },

  tuic: {
    fromConfig(cfg) {
      const d = base0('tuic');
      d.uuid = cfg.uuid ?? '';
      d.pwd = cfg.password ?? '';
      d.cc = lc(cfg.tuicSettings?.congestionControl) ?? 'bbr';   // R3
      d.udp = lc(cfg.tuicSettings?.udpRelayMode) ?? 'native';    // R3
      d.zeroRtt = cfg.tuicSettings?.zeroRttHandshake === true;
      d.heartbeat = cfg.tuicSettings?.heartbeat ?? '';
      d.alpn = cfg.tlsSettings?.alpn?.join(',') ?? '';
      d.sni = cfg.tlsSettings?.serverName ?? '';                 // tuic TLS 恒开，无 sec 门（同 anytls）
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      d.ech = cfg.tlsSettings?.ech === true;                     // ECH（tuic TLS 恒开，QUIC 自管）
      d.echConfig = cfg.tlsSettings?.echConfig ?? '';
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        uuid: str(draft.uuid),
        password: str(draft.pwd),
        tuicSettings: {
          ...base.tuicSettings,
          congestionControl: str(draft.cc) as 'bbr' | 'cubic' | 'new_reno' | undefined,
          udpRelayMode: str(draft.udp) as 'native' | 'quic' | undefined,
          // 关 → 删键（内核默认 false，同 hy2 的 disableChromeParrot）。
          zeroRttHandshake: draft.zeroRtt === true ? true : undefined,
          // 空 → 删键；后端 `normalize_duration` 给裸数字补 `ms`，带单位原样透传。
          heartbeat: str(draft.heartbeat),
        },
        // tuic TLS 恒开（后端 TLS_PROTOCOLS 含 tuic），mergeTls 起底保全未建模项；ech 与 alpn 并存不互斥。
        tlsSettings: mergeTls(base.tlsSettings, {
          serverName: str(draft.sni),
          allowInsecure: draft.insecure === true ? true : undefined,
          // 改用全仓统一的 `listFromText`：原先的裸 split 对 `" , , "` 会产出 `[]` —— 与 `809f476`
          // 在 ssh/trojan 上修掉的是同一个缺陷（空数组 ≠ 没填），tuic 这条当时漏在射程外。
          alpn: listFromText(draft.alpn),
          ech: draft.ech === true ? true : undefined,
          echConfig: draft.ech === true ? str(draft.echConfig) : undefined,
        }),
      };
    },
  },

  socks: {
    fromConfig(cfg) {
      const d = base0('socks');
      d.user = cfg.username ?? '';
      d.pwd = cfg.password ?? '';
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        username: str(draft.user),
        password: str(draft.pwd),
      };
    },
  },

  http: {
    fromConfig(cfg) {
      const d = base0('http');
      d.user = cfg.username ?? '';
      d.pwd = cfg.password ?? '';
      d.tls = lc(cfg.security) === 'tls';                // R3：http isHttps（security==='tls' 严格归一）
      d.sni = cfg.tlsSettings?.serverName ?? '';         // TLS 组（门 = tls 开关，非 sec）
      d.fp = lc(cfg.tlsSettings?.fingerprint) ?? '';     // R4；空=后端缺省 none（不下发 utls 块）
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      tlsAdvDraft(cfg, d, false);                        // http 无 ECH 控件（同 上游 http-form），见 F_HTTP_TLS_ADV
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        username: str(draft.user),
        password: str(draft.pwd),
        security: draft.tls === true ? 'tls' : 'none',
        // HIGH-1：门谓词是 `whenHttpTls`（http 的开关键是 `tls`，用 `whenTls` 会恒 false）。关 TLS 时
        // 整块清除 tlsSettings（含 base 未建模项），否则 Rust 的 `tls_settings.is_some()` 会绕过
        // `security='none'` 对明文口误开 TLS —— 与 vless/vmess/trojan 同一处理。
        tlsSettings: whenHttpTls(draft)
          ? mergeTls(base.tlsSettings, {
              serverName: str(draft.sni),
              // http 不在 `is_quic_managed_tls` 里 ⇒ `final_fp != "none"` 时 utls 块照常下发。
              // 空 → 删键 → 后端对非 vless/anytls 的缺省 `none` ⇒ 整个 utls 块不下发。
              fingerprint: str(draft.fp),
              allowInsecure: draft.insecure === true ? true : undefined,
              ...tlsAdvPatch(draft, false),
            })
          : undefined,
      };
    },
  },

  anytls: {
    fromConfig(cfg) {
      const d = base0('anytls');
      d.pwd = cfg.password ?? '';
      // R3 + 折叠：anytls 只有 tls/reality 两态（TLS 恒开，见 node-spec 的 sec 注释），故存量里的
      // 'none'/缺省/大写变体一律折成 'tls'（同 上游 anytls-form.tsx:66-69）——不用 `lc() ?? 'tls'`
      // 是因为那会把 'none' 原样带进草稿，再经 toCselOptions 并成第三档假选项。
      d.sec = lc(cfg.security) === 'reality' ? 'reality' : 'tls';
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.fp = lc(cfg.tlsSettings?.fingerprint) ?? '';      // R4
      d.pbk = cfg.realitySettings?.publicKey ?? '';
      d.sid = cfg.realitySettings?.shortId ?? '';
      d.insecure = cfg.tlsSettings?.allowInsecure === true;
      tlsAdvDraft(cfg, d, true);
      d.idleCheck = cfg.anyTlsSettings?.idleSessionCheckInterval ?? '';
      d.idleTimeout = cfg.anyTlsSettings?.idleSessionTimeout ?? '';
      d.minIdle = typeof cfg.anyTlsSettings?.minIdleSession === 'number' ? cfg.anyTlsSettings.minIdleSession : undefined;
      return d;
    },
    toConfig(draft, base) {
      const pbk = str(draft.pbk);
      return {
        ...base,
        password: str(draft.pwd),
        security: draft.sec === 'reality' ? 'reality' : 'tls',
        // TLS 恒开（后端 TLS_PROTOCOLS 强制），两档 sec 下都要下发，故无需 HIGH-1 式清除门 ——
        // 选 reality 时后端仍从 `tlsSettings` 取 server_name/insecure/utls（只是换成 reality 版 TLS 块）。
        tlsSettings: mergeTls(base.tlsSettings, {
          serverName: str(draft.sni),
          fingerprint: str(draft.fp),
          allowInsecure: draft.insecure === true ? true : undefined,
          ...tlsAdvPatch(draft, true),
        }),
        // 与 vless 同一条 HIGH-1 + LOW-9 规则：非 reality 或 pbk 为空 → 整块清除。
        // 后端的 Reality 装配判据是 `security.is_reality() && reality_settings.is_some()`，
        // **不按协议门控** ⇒ anytls 一直支持 reality，缺的只是这两颗控件。
        realitySettings: whenReality(draft) && pbk
          ? { ...base.realitySettings, publicKey: pbk, shortId: str(draft.sid) }
          : undefined,
        anyTlsSettings: {
          ...base.anyTlsSettings,
          idleSessionCheckInterval: str(draft.idleCheck),
          idleSessionTimeout: str(draft.idleTimeout),
          minIdleSession: num(draft.minIdle),
        },
      };
    },
  },

  naive: {
    fromConfig(cfg) {
      const d = base0('naive');
      d.user = cfg.username ?? '';
      d.pwd = cfg.password ?? '';
      d.sni = cfg.tlsSettings?.serverName ?? '';
      d.http3 = cfg.naiveSettings?.useHttp3 === true;
      d.ech = cfg.tlsSettings?.ech === true;
      d.echConfig = cfg.tlsSettings?.echConfig ?? '';
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        username: str(draft.user),
        password: str(draft.pwd),
        // serverName + ECH 两项建模。naive 分支自造的 TLS 块把 insecure/alpn/utls/engine/spoof/fragment
        // 全写死 None（真下发这几个键随包核会点名 FATAL），**唯独 ech 到得了内核**：
        // `apply_anti_censorship_options` 在该分支之后运行、只看 `ob.tls.is_some()` 就会覆盖上去，
        // 而核那张 `… is not supported on naive outbound` 拒绝名单里没有 ech（实测见 node-spec 的 naive 段）。
        tlsSettings: mergeTls(base.tlsSettings, {
          serverName: str(draft.sni),
          ech: draft.ech === true ? true : undefined,
          echConfig: draft.ech === true ? str(draft.echConfig) : undefined,
        }),
        naiveSettings: {
          ...base.naiveSettings,
          useHttp3: draft.http3 === true ? true : undefined,
        },
      };
    },
  },

  snell: {
    fromConfig(cfg) {
      const d = base0('snell');
      d.version = cfg.snellSettings?.version === 6 ? '6' : '4';
      d.pwd = cfg.password ?? '';
      d.obfsMode = lc(cfg.snellSettings?.obfsMode) ?? 'none';
      d.obfsHost = cfg.snellSettings?.obfsHost ?? '';
      d.mode = lc(cfg.snellSettings?.mode) ?? 'default';
      d.reuse = cfg.snellSettings?.reuse === true;
      d.network = lc(cfg.snellSettings?.network) ?? '';
      d.userkey = cfg.snellSettings?.userkey ?? '';
      return d;
    },
    toConfig(draft, base) {
      const version = draft.version === '6' ? 6 : 4;
      return {
        ...base,
        password: str(draft.pwd),
        snellSettings: {
          ...base.snellSettings,
          version,
          // obfs（v4）/ mode（v6）互斥——非当前分支的一侧提交时清空，防脏下发（node-spec.ts 文件头注释）。
          obfsMode: version === 4 ? (str(draft.obfsMode) as 'none' | 'http' | undefined) : undefined,
          obfsHost: version === 4 && draft.obfsMode === 'http' ? str(draft.obfsHost) : undefined,
          mode: version === 6 ? (str(draft.mode) as 'default' | 'unshaped' | 'unsafe-raw' | undefined) : undefined,
          reuse: draft.reuse === true ? true : undefined,
          network: str(draft.network) as 'tcp' | 'udp' | undefined,
          userkey: str(draft.userkey),
        },
      };
    },
  },

  ssh: {
    fromConfig(cfg) {
      const d = base0('ssh');
      d.user = cfg.sshSettings?.user ?? '';
      d.pwd = cfg.sshSettings?.password ?? '';
      d.privateKey = cfg.sshSettings?.privateKey ?? '';
      d.privateKeyPath = cfg.sshSettings?.privateKeyPath ?? '';
      d.privateKeyPassphrase = cfg.sshSettings?.privateKeyPassphrase ?? '';
      d.hostKey = cfg.sshSettings?.hostKey?.join(',') ?? '';
      d.hostKeyAlgorithms = cfg.sshSettings?.hostKeyAlgorithms?.join(',') ?? '';
      d.clientVersion = cfg.sshSettings?.clientVersion ?? '';
      d.cipher = cfg.sshSettings?.cipher?.join(',') ?? '';
      d.mac = cfg.sshSettings?.mac?.join(',') ?? '';
      d.kexAlgorithm = cfg.sshSettings?.kexAlgorithm?.join(',') ?? '';
      return d;
    },
    toConfig(draft, base) {
      return {
        ...base,
        // ssh 走 sshSettings.user/password（非顶层 username/password，与 socks/http/naive 惯例不同——见后端
        // outbound.rs `Protocol::Ssh => { ob.user = s.user...; ob.password = s.password... }`，读 ssh_settings）。
        sshSettings: {
          ...base.sshSettings,
          user: str(draft.user),
          password: str(draft.pwd),
          privateKey: str(draft.privateKey),
          privateKeyPath: str(draft.privateKeyPath),
          privateKeyPassphrase: str(draft.privateKeyPassphrase),
          // 五个 `Vec<String>` 同一写法：留空 → 删键（后端 `.clone()` 原样透传 Option ⇒
          // 下发空数组等于显式声明「一个算法都不接受」，与「用默认算法集」不是一回事）。
          hostKey: listFromText(draft.hostKey),
          hostKeyAlgorithms: listFromText(draft.hostKeyAlgorithms),
          clientVersion: str(draft.clientVersion),
          cipher: listFromText(draft.cipher),
          mac: listFromText(draft.mac),
          kexAlgorithm: listFromText(draft.kexAlgorithm),
        },
      };
    },
  },

  custom: {
    fromConfig(cfg) {
      const d = base0('custom');
      d.outbound = JSON.stringify(cfg.customSettings?.outbound ?? {}, null, 2);
      d.isEndpoint = cfg.customSettings?.isEndpoint === true;
      d.secretKeys = cfg.customSettings?.secretKeys?.join(',') ?? '';
      return d;
    },
    toConfig(draft, base) {
      const raw = typeof draft.outbound === 'string' ? draft.outbound.trim() : '';
      let parsed: unknown;
      try {
        parsed = raw ? JSON.parse(raw) : {};
      } catch (e) {
        // 硬约束：JSON 非法须显式报错阻断提交，绝不吞掉静默存半成品 outbound。
        // handleSubmit 的 try/catch 会把此 throw 转成 submitErr 展示（同其它协议的异常路径）。
        throw new Error(`自定义协议 JSON 格式错误：${e instanceof Error ? e.message : String(e)}`);
      }
      if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
        throw new Error('自定义协议 JSON 必须是对象');
      }
      const outbound = parsed as Record<string, unknown>;
      const typeVal = outbound.type;
      // 镜像后端 store/validate.rs#protocol_requirement_ok("custom")：outbound.type 非空才算齐备，
      // 提前拦在前端避免往返一次 IPC 才发现缺字段。
      if (typeof typeVal !== 'string' || typeVal.trim() === '') {
        throw new Error('自定义协议 JSON 缺少必填的 "type" 字段');
      }
      const secretKeys = str(draft.secretKeys);
      return {
        ...base,
        customSettings: {
          outbound,
          isEndpoint: draft.isEndpoint === true ? true : undefined,
          secretKeys: secretKeys ? secretKeys.split(',').map((s) => s.trim()).filter(Boolean) : undefined,
        },
      };
    },
  },
};
