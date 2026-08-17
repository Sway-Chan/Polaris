/**
 * 协议设置的**跨语言覆盖门** —— Rust 结构体字段 ↔ 前端类型 ↔ UI 编辑器。
 *
 * # 这道门守的是什么
 *
 * 「某协议的设置字段在 Rust 侧活着、config-engine 会消费、磁盘上存得下，但 UI 既不显示也不让改」
 * 是一个**全绿的缺陷**：类型检查过、build 过、所有既有测试过，只有用户会撞上。
 *
 * 实证（本门落地时的存量）：`TailscaleSettings` 17 个字段里 `routes` / `reverseMesh` /
 * `relayServerPort` / `resolveByName` 四个从未进过 `TsSettingsDialog`。其中
 *  - `reverseMesh` 决定 `meshUsesSystemInterface` ⇒ 该节点是否参与测速（`domain/endpoint-routes.ts:320`）；
 *  - `resolveByName` 是 `acceptDefaultResolvers` 生效的前提（`builder/dns.rs:1214` 选节点的谓词就是它）
 *    ⇒ 它缺席时，弹窗里那个「接受 DNS」开关**恒无效**——一个拨了不生效的控件。
 * 两条都不是「少几个框」，是不可见的活状态。
 *
 * 对照组同样说话：`WireGuardSettings` 的 `reverseMesh` 当时也没有编辑入口（`wg-logic.ts` 只在**注释**
 * 里提了它，靠 `...base` 起底原样带过）。所以这不是「Tailscale 特殊」，是**这条不变式全仓没有门**——
 * 覆盖全不全，全凭移植当时认不认真。**该缺口已补**（WG 接入模式开关，`WgDialog.tsx` 的 `wgSpec`），
 * 对应的 `EXEMPT.WireGuardSettings.reverseMesh` 已按锁 3 的要求删除。
 *
 * # 门的形态（六把锁）
 *
 *  1. **Rust ↔ 前端类型双向锁**（全部 18 个结构体）：JSON 键集必须逐键相等。管住「加了字段只改一侧」
 *     以及反向的「UI/类型里出现 Rust 没有的键」——那是拼错或已删除字段。
 *  2. **组网结构体的 UI 覆盖锁**：`TailscaleSettings` / `WireGuardSettings` 的每个键都必须被其编辑器
 *     文件触及，否则须落进 [`EXEMPT`]，且每条豁免带理由。
 *  3. **豁免表反向锁**：豁免项必须是真实存在的键（改名后残留的死豁免 → 红），且必须**确实未覆盖**
 *     （已经补上了却还挂着豁免 → 红，防豁免表变成永久盲区）。
 *  4. **其余协议结构体的移植债务棘轮**：未覆盖键逐项钉死，**只许降不许升**（同 `locale-parity.test.ts`
 *     的 `MISSING_KEY_DEBT` 手法）。给 sing-box 加字段 → Rust 跟进 → UI 没跟进，任何协议都会转红。
 *     **判据是 per-protocol 的**（见下），键形如 `结构体::协议`。
 *  5. **归属表自身的牙**：`STRUCT_OWNERS`（结构体 → 协议集）不是一张随手写的表——协议名必须 ⊆
 *     `PROTO_OPTIONS`，并与 **Rust 侧真实消费面机器对拍**（`TLS_PROTOCOLS`、multiplex 的 `matches!`
 *     分支、传输层排除名单、`is_quic_managed_tls`、spoof 的协议门）。「Rust 给某协议新接了 TLS 而归属
 *     表没跟」会红。没有这把锁，归属表本身就是下一个盲区。
 *  6. **`NODE_EXEMPT` 与 `PORT_DEBT` 的类型区别**：「有意排除」与「还没做」**在门里是两张表**，
 *     且豁免每条必须带**可核对的代码依据**（文件 + 可选的块锚点 + 该块内必须**恰好出现一次**的字符串，
 *     见 [`Cite`]）。**不带行号**——行号那一维在 2026-08-17 拆掉了，理由写在 [`Cite`] 的注释里。
 *
 * # 为什么判据必须带 per-protocol 维度（批 C 的改造；改造前它在记假账）
 *
 * 改造前 `isCovered(key, idx)` 拿 `node-spec.ts` + `proto-codec.ts` **两个文件全文拼起来**做匹配 ⇒
 * 只有 per-struct、没有 per-protocol：某个键只要在**任意一个**协议块里出现过一次，整个结构体的该键
 * 就判「已覆盖」。坐实过的遮蔽实例：
 *  - `ShadowTlsSettings.password`/`.sni`/`.fingerprint` 被各协议的 `password:`、`{k:'sni'}`、
 *    vless 的 `.fingerprint` 遮蔽 ⇒ 「ShadowTLS 开关造出用户修不好的坏节点」那条缺陷长期活着；
 *  - `Hysteria2Settings.network` 被 snell 的 `{k:'network'}` 遮蔽 ⇒ 债务表记零债务，实际 Rust 会消费
 *    （`builder/outbound.rs:249` 的 `ob.network = h.network.clone()`）而 UI 无入口；
 *  - `HttpSettings.method` 被 shadowsocks 的 `{k:'method'}` 遮蔽；
 *  - ws 那批在 codec 里写下 `path:`/`headers:` 之后，`HttpSettings.path`/`headers` 被判「已覆盖」，
 *    **不得不从债务表删掉两个其实没修的条目** —— 门从「看不见缺口」恶化成「记录假账」，并且开始
 *    **逼迫后来者为了绕开它而给变量取名**（`withHostHeader` 的形参当时叫 `hostValue` 就是为此）。
 *    ✅ 批 D 已把它改回自然的 `host`：归属过滤上线后这条约束不再需要，且「改回来仍不制造遮蔽」
 *    由下面那条反向对照断言机器守着（`host` 在并集里判绿、在 `WebSocketSettings` 与 http 协议上判红）。
 *
 * 改法**不动 `NodeDialog`、也不动 `isCovered` 一行**，只换它的输入：
 *  1. `ND_SPEC` / `protoCodec` 都是对象字面量、键即协议名 ⇒ 用与 [`structBody`] 同款的花括号配对解析器
 *     切出 `ND_SPEC.<proto>` 与 `protoCodec.<proto>` 的值体（[`objectPropSpan`]）；
 *  2. 共享片段要跟着切片走：`...F_TRANSPORT` / `tlsAdvPatch(draft, true)` 这类**同文件模块级声明**按
 *     引用做传递闭包内联（[`protoStructIndex`]）。不内联的话每个协议都会凭空多出几十条假缺口——
 *     真正的键名全在那些共享片段里；
 *  3. 内联进来的**共享 helper 还要过一道结构体归属**：只有文本里点名了该结构体（TS 类型名如
 *     `WebSocketSettings`，或它在 `ServerConfig` 上的 JSON 字段名如 `wsSettings`）的 helper 才算数。
 *     没有这一层，`transportPatch` 里的 `path:`/`headers:` 会**同时**喂给 `WebSocketSettings` 与
 *     `HttpSettings` ⇒ 上面那笔假账原样保留。协议自己的切片恒计入，只有被内联的共享件受此约束。
 *     副作用是「helper 写了某结构体的键却不点名该结构体」会误报债务——**那是响亮的假警报**
 *     （补一个类型标注即可），比静默漏检好；本仓既有写法（`mergeBlock<WebSocketSettings>`、
 *     `Partial<TlsSettings>`、`Pick<ServerConfig,'multiplexSettings'>`）本来就点名。
 *
 * # 为什么读 Rust 源码而不是再抄一份镜像常量
 *
 * 抄镜像只是把漂移面往后挪一格。范式照抄本仓既有的 `user-config-fields.test.ts` / `unlock-detection.test.ts`：
 * 直接把 Rust 源码当真值读进来解析。
 *
 * # 自曝纪律
 *
 * 解析不到必须**抛错**，不得「读不到就跳过」——那样 Rust 文件一改名，门就静默消失，
 * 「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 *
 * # 判据为什么不是「键名出现过」（变异实测改出来的）
 *
 * 第一版用 `\bkey\b` 扫全文，跑变异时**两条阳性对照没转红**：
 *  - 把 `routes` 的接线全删、只留 `const routes = [];` —— 局部变量名撞上了键名，门照绿；
 *  - 把 `reverseMesh` 的接线全删、只留 i18n 键字符串 `'ts.reverseMesh'` —— `.` 是词边界，门照绿。
 * 于是判据收紧为「**结构性出现**」三选一（先做一趟扫描：剔注释 + 把字符串字面量抽出来另存）：
 *  1. 某个字符串字面量**整体等于**该键（`{ k: 'routes' }` 这类数据表写法）；
 *  2. 代码里有属性访问 `.key`（`ts?.routes` / `base.routes` / `draft.routes`）；
 *  3. 代码里有对象字面量键 `key:`。
 * 关键在于 1 要求**整串相等**：`'ts.reverseMesh'` 不算，局部变量 `const routes =` 也不算（既非 `.routes`
 * 也非 `routes:`）。两条变异随即转红。
 *
 * # 这道门抓不到什么（如实记，别当它是全能的）
 *
 *  - 判据是「键名在编辑器文件里**结构性出现**」，**不是**「真的接进了表单并能往返」。
 *    写一句 `const x = draft.routes;` 读了却不提交，门照绿。往返对称由 `proto-codec.test.ts` 那条腿管。
 *  - 控件**藏在哪个区**、文案对不对、默认值写没写错，门不管。
 *  - 字符串字面量恰好等于某个键名（哪怕用途无关）会被算作覆盖 —— 换取的是数据驱动表单（FieldSpec
 *    `k: '…'`）可被识别。这是有意的取舍，不是疏忽。
 *  - ~~锁 4 有跨协议同名遮蔽~~ **已由批 C 的 per-protocol 切片消除**（见上）。跨协议的草稿键重名
 *    （AnyTLS 的 `idleTimeout` 遮蔽 `GrpcSettings.idleTimeout` 之类）现在各归各的协议切片，不再互相盖绿。
 *  - **共享 helper 里由「运行时开关」控制的分支，文本上恒在** —— 剩下的一条同类盲区，有活实例：
 *    `tlsAdvPatch(draft, withEch)` 的 `...(withEch ? { ech: …, echConfig: … } : {})`，http 传的是
 *    `false`（那张表没有 ECH 控件），但这两个键在 helper 文本里恒存在 ⇒ `TlsSettings.ech`/`echConfig`
 *    在 http 上被判「已覆盖」而实际无控件。**该项本身是有意的**（上游 `http-form.tsx` 同样无 ECH，
 *    属两边都无、非移植遗漏），故不构成活的缺陷；但判据确实看不见这一格。要根治得让判据认识
 *    「调用点传的静态实参」，那是 AST 的活，不在本门的手法预算内。
 *  - **同一协议内的跨结构体同名**由「共享 helper 归属过滤」挡住了大半（`HttpSettings.path` 不再被
 *    `wsSettings` 的 `path:` 盖绿），但**协议自己的切片不过滤** ⇒ 若某协议在自己块里直接写下两个
 *    同名键分属两个结构体，仍会互盖。当前无活实例。
 *  - 只覆盖 `server_config.rs` / `protocol_settings.rs` 两份里的设置结构体；`ServerConfig` 自身的
 *    顶层字段（address/port/detour…）不在射程内。
 *  - **锁 2 的 `editors` 是「并集」，不是「每个编辑器各自」**（`reserved` 那次实测出来的射程边界）。
 *    `WireGuardSettings` 挂着两个编辑器，只要**任一份**结构性提到某键就算覆盖 ⇒ 「该键在它真正的
 *    归属弹窗里没有控件」这件事本门看不见。实证：`reserved` 长期只有 `WarpDialog` 有控件，普通 WG
 *    节点在 UI 上改不了（`wg-logic.ts` 靠 `...base` 原样带过），而本门一直是绿的。
 *    **要根治得再加一张「键 → 归属编辑器」的表**（`reserved`/`preSharedKey`/`reverseMesh` 归
 *    `WgDialog`、`warpDevice` 归 `WarpDialog`…），而不是要求每个编辑器覆盖全部键 —— 后者会逼
 *    `WarpDialog` 去「覆盖」它本就不该有的 `preSharedKey`/`reverseMesh`（WARP 不用 PSK、且恒 gVisor）。
 *    没有当场加，是因为归属表要逐键写理由、且它与「哪些文件算编辑器」这一层的取舍纠缠；
 *    **当下这条边界没有活的实例**：补上 WG 侧的 Reserved 控件后，仅被单份编辑器覆盖的只剩
 *    `warpDevice`（机器写入的设备凭据 blob，本就不该有任何控件）。它再咬人时，归属表是那时的修法。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
// 归属表的第一道牙：协议名不是自由文本，必须是表单真支持的那 13 个。直接引真常量而不是抄一份镜像，
// 理由同文件头「为什么读 Rust 源码而不是再抄一份镜像常量」。`node-spec.ts` 对 `field-spec` 只有
// `import type`（编译期擦除），不会把 React 拖进这个 node 环境的 vitest。
import { PROTO_OPTIONS, ND_SPEC, type NodeProto } from '../components/dialogs/node-spec';

// ── 源码读入（Rust = 真值源；前端类型与编辑器 = 被核对方） ──

function read(rel: string): string {
  return readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
}

const RUST_SERVER_CONFIG = read('../../../crates/config-engine/src/user_config/server_config.rs');
const RUST_PROTOCOL_SETTINGS = read(
  '../../../crates/config-engine/src/user_config/protocol_settings.rs'
);
const TS_PROTOCOL_SETTINGS = read('./types/protocol-settings.ts');

// 锁 5 的对拍源：协议 → 结构体的**真实消费面**都写在这三份里，且都是可解析的字面量。
const RUST_OUTBOUND = read('../../../crates/config-engine/src/builder/outbound.rs');
const RUST_OUTBOUND_HELPERS = read('../../../crates/config-engine/src/builder/outbound_helpers.rs');
const RUST_TLS_SPOOF = read('../../../crates/config-engine/src/user_config/tls_spoof.rs');

/**
 * 行注释剔除。**这一步是承重的**，不是整洁癖：`wg-logic.ts:137` 的注释里写着
 * `reverseMesh / warpDevice / reserved`，不剔注释的话 WG 的真实缺口会被注释「盖绿」——
 * 门会报「全覆盖」，而那三个键里 `reverseMesh` 确实没有编辑入口。Rust 侧同理（doc 注释里
 * 出现 `rename = "..."` 或 `pub foo:` 会解析出幽灵字段）。
 */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
}

/** 取 `pub struct <Name> { … }` 的体（花括号配对；已剔注释，故注释里的括号不会干扰配对）。 */
function structBody(src: string, name: string): string {
  const stripped = stripComments(src);
  const at = stripped.indexOf(`pub struct ${name} {`);
  expect(
    at,
    `Rust 侧 pub struct ${name} 解析失败（改名/重构了？）—— 解析不到必须转红，不得静默放行`
  ).toBeGreaterThanOrEqual(0);
  const open = stripped.indexOf('{', at);
  let depth = 0;
  for (let i = open; i < stripped.length; i++) {
    if (stripped[i] === '{') depth++;
    else if (stripped[i] === '}') {
      depth--;
      if (depth === 0) return stripped.slice(open + 1, i);
    }
  }
  throw new Error(`Rust 侧 pub struct ${name} 花括号不配对 —— 解析器失效，必须转红`);
}

/**
 * Rust 结构体 → **JSON 键名**（保序）。
 * 有 `#[serde(rename = "…")]` 取 rename，否则取字段名本身（`routes` / `hostname` / `mtu` / `reserved` 这类
 * 本就无需改名的）。逐字段回看「上一个字段到本字段之间」那段属性文本，避免把别人的 rename 抓过来。
 */
function rustJsonKeys(src: string, structName: string): string[] {
  const body = structBody(src, structName);
  const decls = [...body.matchAll(/pub\s+(\w+)\s*:/g)].map((m) => ({
    name: m[1],
    at: m.index as number,
  }));
  expect(decls.length, `${structName} 没解析出任何字段 —— 解析器失效，必须转红`).toBeGreaterThan(0);
  return decls.map((d, i) => {
    const attrs = body.slice(i === 0 ? 0 : decls[i - 1].at, d.at);
    const renamed = /rename\s*=\s*"([^"]+)"/.exec(attrs);
    return renamed ? renamed[1] : d.name;
  });
}

/** 前端 `export interface <Name> { … }` 的**顶层**成员键（嵌套对象字面量内的键不算，如 warpDevice 的内层）。 */
function tsInterfaceKeys(src: string, name: string): string[] {
  const stripped = stripComments(src);
  const at = stripped.indexOf(`export interface ${name} {`);
  expect(
    at,
    `前端 export interface ${name} 解析失败（改名/重构了？）—— 解析不到必须转红`
  ).toBeGreaterThanOrEqual(0);
  const open = stripped.indexOf('{', at);
  let depth = 0;
  let close = -1;
  for (let i = open; i < stripped.length; i++) {
    if (stripped[i] === '{') depth++;
    else if (stripped[i] === '}') {
      depth--;
      if (depth === 0) {
        close = i;
        break;
      }
    }
  }
  expect(close, `前端 interface ${name} 花括号不配对`).toBeGreaterThan(open);
  let nest = 0;
  let flat = '';
  for (const ch of stripped.slice(open + 1, close)) {
    if (ch === '{') nest++;
    else if (ch === '}') nest--;
    else if (nest === 0) flat += ch;
  }
  const keys = [...flat.matchAll(/(?:^|\n)\s*(\w+)\??\s*:/g)].map((m) => m[1]);
  expect(keys.length, `${name} 没解析出任何成员 —— 解析器失效，必须转红`).toBeGreaterThan(0);
  return keys;
}

/**
 * TS/TSX 单趟扫描：剔掉注释，并把字符串字面量的**内容**抽出来另存（代码里留空壳 `""`）。
 *
 * 一趟扫描而不是「先剔注释再剔字符串」两次正则：`ph: 'https://controlplane.tailscale.com'` 会被
 * 行注释正则从 `https:` 处腰斩，剩下半个未闭合的引号再去做字符串剥离必然错位。反过来先剥字符串，
 * 注释里的撇号又会吃掉后面的代码。只有按字符走一遍才没有这个先后手问题。
 */
function scanTs(src: string): { code: string; strings: Set<string> } {
  const strings = new Set<string>();
  let code = '';
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      while (i < src.length && src[i] !== '\n') i++;
      continue;
    }
    if (c === '/' && d === '*') {
      i += 2;
      while (i < src.length && !(src[i] === '*' && src[i + 1] === '/')) i++;
      i += 2;
      continue;
    }
    if (c === "'" || c === '"' || c === '`') {
      const quote = c;
      i++;
      let buf = '';
      while (i < src.length && src[i] !== quote) {
        if (src[i] === '\\') {
          buf += src[i + 1] ?? '';
          i += 2;
          continue;
        }
        buf += src[i];
        i++;
      }
      i++;
      strings.add(buf);
      code += '""';
      continue;
    }
    code += c;
    i++;
  }
  return { code, strings };
}

interface EditorIndex {
  code: string;
  strings: Set<string>;
}

function editorIndex(files: readonly string[]): EditorIndex {
  const code: string[] = [];
  const strings = new Set<string>();
  for (const f of files) {
    const r = scanTs(read(f));
    code.push(r.code);
    for (const s of r.strings) strings.add(s);
  }
  return { code: code.join('\n'), strings };
}

/** 「结构性出现」三选一，判据说明见文件头。 */
function isCovered(key: string, idx: EditorIndex): boolean {
  if (idx.strings.has(key)) return true; // 数据表写法：{ k: 'routes' }
  if (new RegExp(`\\.\\s*${key}\\b`).test(idx.code)) return true; // ts?.routes / base.routes
  if (new RegExp(`\\b${key}\\s*:`).test(idx.code)) return true; // 对象字面量键
  return false;
}

function uncoveredKeys(keys: readonly string[], idx: EditorIndex): string[] {
  return keys.filter((k) => !isCovered(k, idx));
}

// ── 被核对的结构体清单 ──

/** 组网两族：字段直接决定路由/出口/可测速性，覆盖面零容忍（缺口只能走带理由的豁免）。 */
const MESH_TARGETS = [
  {
    struct: 'TailscaleSettings',
    rust: RUST_SERVER_CONFIG,
    /**
     * `authKey` 由 `TsLoginDialog` 承担（合理拆分，不做第二个入口）。把它的纯逻辑文件**列进编辑器清单**
     * 而不是写一条豁免：豁免是「永远别管」，列文件是「它得一直真的写这个键」——
     * 哪天 TsLoginDialog 不再写 authKey，这道门会红。豁免做不到这件事。
     *
     * **有意不列 `ts-settings-logic.ts`**（TsSettingsDialog 的读写纯逻辑）：那里有接线不等于用户有控件。
     * 只认 `TsSettingsDialog.tsx`，判据就落在「FieldSpec 表里有没有这一项」——即**有没有控件**，
     * 正是这道门要守的东西。实测：只删掉 ADV_SPEC 里 `k: 'relayServerPort'` 那一行、逻辑原样保留，
     * 本门转红。代价是日后若把 FieldSpec 表也搬进 logic 文件会误红一次——那是响亮的假警报，
     * 补一行文件名即可，比静默漏检好。
     */
    editors: [
      '../components/dialogs/TsSettingsDialog.tsx',
      '../components/dialogs/ts-login-server.ts',
    ],
  },
  {
    struct: 'WireGuardSettings',
    rust: RUST_SERVER_CONFIG,
    /**
     * **有意不列 `wg-logic.ts`**，同上面 TS 侧不列 `ts-settings-logic.ts` 的理由：那里有读写接线不等于
     * 用户有控件。此前它在列表里，WG 侧的锁 2 就比 TS 侧松一档——变异实测：把 `wgSpec` 里
     * `k: 'reverseMesh'` 那一行整行删掉、`wg-logic.ts` 的读写原样保留，门**照绿**（`draft.reverseMesh`
     * 命中「属性访问」判据）。摘掉它之后同一变异转红，判据这才真的落在「FieldSpec 表里有没有这一项」。
     * 摘除后全部 12 个键仍被覆盖：11 个在 `WgDialog.tsx` 的 `wgSpec`、`reserved` / `warpDevice` 在
     * `WarpDialog.tsx`。
     */
    editors: [
      '../components/dialogs/WgDialog.tsx',
      '../components/dialogs/WarpDialog.tsx',
    ],
  },
] as const;

/**
 * **显式豁免表**：键 → 理由。没有理由的豁免不许存在。
 * 锁 3 会反向核对：豁免项必须真实存在、且必须确实未被覆盖。
 */
const EXEMPT: Record<string, Record<string, string>> = {
  TailscaleSettings: {
    allowInternet:
      'Tailscale 的「是否允许作外网出口」两侧谓词都由 exitNode 派生，存量字段被明确忽略：' +
      'domain/endpoint-routes.ts:153 与 builder/endpoint_routes.rs:76 都是 `!!exitNode`，' +
      '前者注释写明「存量 tailscaleSettings.allowInternet 字段谓词层忽略（向后兼容、不迁移）」。' +
      '给它做开关 = 一个拨了不生效的假控件 + 第二个默认值真值源。改法是改 exitNode，不是加控件。',
  },
  // WireGuardSettings：本表曾登记 `reverseMesh` 为「未移植的真实缺口」，接入模式开关补上后按锁 3 删除。
};

// ── per-protocol 切片（批 C）─────────────────────────────────────────────────

/** NodeDialog 的两份编辑面：文件 → 里头那个「协议名即键」的顶层对象字面量。 */
const NODE_OBJECTS = [
  { file: '../components/dialogs/node-spec.ts', obj: 'ND_SPEC' },
  { file: '../components/dialogs/proto-codec.ts', obj: 'protoCodec' },
] as const;

/** 旧判据（两文件全文并集）仍保留一份，只为在自检里**证明新维度确实更严**，不参与任何覆盖断言。 */
const NODE_EDITOR_FILES = NODE_OBJECTS.map((o) => o.file);

/**
 * 与源码**等长**的掩码：注释与字符串**内容**抹成空格（引号、换行原位保留），偏移量逐字符对齐原文。
 *
 * 为什么不复用 [`scanTs`]：它把字符串换成两字符的 `""`，偏移量随即错位，没法拿它的结果反查原文区间；
 * 而切片必须在**原文**上取（切完还要再喂给 `scanTs` 拿字符串字面量集）。掩码只用来**定位**
 * （花括号配对 / 标识符扫描），从不参与覆盖判据 —— 判据仍旧是 `scanTs` + `isCovered`。
 */
function maskTs(src: string): string {
  const out = src.split('');
  const blank = (from: number, to: number): void => {
    for (let j = Math.max(0, from); j < to && j < out.length; j++) if (out[j] !== '\n') out[j] = ' ';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      const st = i;
      while (i < src.length && src[i] !== '\n') i++;
      blank(st, i);
      continue;
    }
    if (c === '/' && d === '*') {
      const st = i;
      i += 2;
      while (i < src.length && !(src[i] === '*' && src[i + 1] === '/')) i++;
      i = Math.min(i + 2, src.length);
      blank(st, i);
      continue;
    }
    if (c === "'" || c === '"' || c === '`') {
      const quote = c;
      const st = i;
      i++;
      while (i < src.length && src[i] !== quote) i += src[i] === '\\' ? 2 : 1;
      i = Math.min(i + 1, src.length);
      blank(st + 1, i - 1); // 引号本身留着，内容抹白
      continue;
    }
    i++;
  }
  return out.join('');
}

interface Span {
  start: number;
  end: number;
}

/**
 * 模块级声明 → 区间。以「下一个列 0 语句的起点」为界 —— 本仓这两份文件的顶层只有 `import` 与声明，
 * 不需要真正的语法分析（同 [`structBody`] 那条「够用即止」的手法）。
 */
function topLevelDecls(masked: string): Map<string, Span> {
  const marks: { name: string | null; at: number }[] = [];
  for (const m of masked.matchAll(
    /^(?:export\s+)?(?:const|let|var|function|class|interface|type|enum)\s+([A-Za-z_$][\w$]*)/gm
  )) {
    marks.push({ name: m[1], at: m.index as number });
  }
  // 列 0 的非声明语句也是边界，否则前一个声明会把它们吞进去。
  for (const m of masked.matchAll(/^(?:import|export\s+default)\b/gm)) {
    marks.push({ name: null, at: m.index as number });
  }
  marks.sort((a, b) => a.at - b.at);
  const out = new Map<string, Span>();
  for (let i = 0; i < marks.length; i++) {
    if (marks[i].name === null) continue;
    out.set(marks[i].name as string, {
      start: marks[i].at,
      end: i + 1 < marks.length ? marks[i + 1].at : masked.length,
    });
  }
  expect(out.size, '模块级声明一个都没解析出来 —— 解析器失效，必须转红').toBeGreaterThan(0);
  return out;
}

/** 花括号配对（掩码上做，注释/字符串里的括号已抹白）。返回闭合花括号的下一位。 */
function matchBrace(masked: string, open: number, label: string): number {
  let depth = 0;
  for (let i = open; i < masked.length; i++) {
    if (masked[i] === '{') depth++;
    else if (masked[i] === '}') {
      depth--;
      if (depth === 0) return i + 1;
    }
  }
  throw new Error(`${label} 花括号不配对 —— 解析器失效，必须转红`);
}

/** 顶层对象字面量 `<declName>` 里 `\n  <prop>: { … }` 的区间（2 空格缩进 = 协议键那一层）。
 *
 * 键可能**带引号**：协议名含连字符时（`openvpn-client`）不是合法的裸标识符，对象字面量里必须写成
 * `'openvpn-client': {`。2026-08-11 加该协议时本解析器当场判红 —— 方向是对的（解析不到即红，
 * 不静默放行），但它红的理由是「解析器只认裸键」而非「ND_SPEC 真缺这一条」，故放宽键的形态。
 * 真值是 ND_SPEC 的**内容**，不是它的书写形式。 */
function objectPropSpan(
  masked: string,
  raw: string,
  decl: Span,
  declName: string,
  prop: string
): Span {
  // 键在 **raw** 里找、花括号在 **masked** 里配对：`maskTs` 保长度（上面有断言钉住），
  // 故两者偏移量一致。带引号的键（`'openvpn-client'`）在 masked 里内容已被抹掉，只能从 raw 认。
  const body = raw.slice(decl.start, decl.end);
  const m = new RegExp(`\\n  ['"]?${prop.replace(/-/g, '\\-')}['"]?:\\s*\\{`).exec(body);
  expect(
    m,
    `${declName}.${prop} 解析失败（协议键改名/缩进变了？）—— 解析不到必须转红，不得静默放行`
  ).not.toBeNull();
  const open = decl.start + (m as RegExpExecArray).index + (m as RegExpExecArray)[0].length - 1;
  return { start: open, end: matchBrace(masked, open, `${declName}.${prop}`) };
}

interface SourceIndex {
  raw: string;
  masked: string;
  decls: Map<string, Span>;
}

const SRC_CACHE = new Map<string, SourceIndex>();
function sourceIndex(rel: string): SourceIndex {
  const hit = SRC_CACHE.get(rel);
  if (hit) return hit;
  const raw = read(rel);
  const masked = maskTs(raw);
  expect(masked.length, `${rel} 掩码与原文长度不一致 —— 偏移量会错位，必须转红`).toBe(raw.length);
  const idx = { raw, masked, decls: topLevelDecls(masked) };
  SRC_CACHE.set(rel, idx);
  return idx;
}

/**
 * 结构体 → 它作为**成员字段**时的 JSON 名（`TlsSettings` → `tlsSettings`；嵌套的
 * `Hysteria2ObfsSettings` → `obfs`，它挂在 `Hysteria2Settings` 上而不是 `ServerConfig` 上）。
 * **机器解析 Rust，不手抄**：rename 一改，归属过滤就该跟着变；抄一份只是把漂移面往后挪一格。
 */
const SETTINGS_FIELD: ReadonlyMap<string, string> = (() => {
  const out = new Map<string, string>();
  for (const src of [RUST_SERVER_CONFIG, RUST_PROTOCOL_SETTINGS]) {
    const stripped = stripComments(src);
    const decls = [...stripped.matchAll(/pub\s+(\w+)\s*:\s*([^,\n]+),/g)].map((m) => ({
      name: m[1],
      ty: m[2],
      at: m.index as number,
    }));
    decls.forEach((d, i) => {
      const struct = /\b(\w+Settings)\s*>/.exec(d.ty)?.[1];
      if (!struct || out.has(struct)) return;
      const attrs = stripped.slice(i === 0 ? 0 : decls[i - 1].at, d.at);
      const renamed = /rename\s*=\s*"([^"]+)"/.exec(attrs);
      out.set(struct, renamed ? renamed[1] : d.name);
    });
  }
  expect(out.size, 'Rust 侧一个 *Settings 成员字段都没解析出来 —— 解析器失效，必须转红').toBeGreaterThan(5);
  return out;
})();

/**
 * 某协议 × 某结构体的编辑面索引。三段拼起来：
 *  1. `ND_SPEC.<proto>` 的值体（有没有控件）；
 *  2. `protoCodec.<proto>` 的值体（读写接线）；
 *  3. 上面两段**引用到的同文件模块级声明**的传递闭包 —— 但只收其中**点名了该结构体**的那些
 *     （TS 类型名或 `ServerConfig` 上的 JSON 字段名）。理由见文件头第 3 条。
 *
 * `isCovered` 一行没动：变的只是喂给它的 [`EditorIndex`]。
 */
function protoStructIndex(proto: string, struct: string): EditorIndex {
  const needles = [struct, SETTINGS_FIELD.get(struct)].filter((s): s is string => !!s);
  const code: string[] = [];
  const strings = new Set<string>();
  const take = (text: string): void => {
    const r = scanTs(text);
    code.push(r.code);
    for (const s of r.strings) strings.add(s);
  };
  for (const { file, obj } of NODE_OBJECTS) {
    const idx = sourceIndex(file);
    const decl = idx.decls.get(obj);
    expect(decl, `${file} 里没解析到顶层声明 ${obj} —— 解析不到必须转红`).toBeDefined();
    const span = objectPropSpan(idx.masked, idx.raw, decl as Span, obj, proto);
    take(idx.raw.slice(span.start, span.end)); // 协议自己的切片：恒计入，不过归属过滤
    const seen = new Set<string>([obj]);
    const queue: string[] = [idx.masked.slice(span.start, span.end)];
    while (queue.length > 0) {
      const text = queue.shift() as string;
      for (const m of text.matchAll(/[A-Za-z_$][\w$]*/g)) {
        const name = m[0];
        if (seen.has(name)) continue;
        seen.add(name);
        const d = idx.decls.get(name);
        if (!d) continue;
        const dm = idx.masked.slice(d.start, d.end);
        // 归属过滤只挡「算不算这个结构体的覆盖」，遍历照旧穿过去（helper 套 helper 的情形）。
        if (needles.some((n) => new RegExp(`\\b${n}\\b`).test(dm))) take(idx.raw.slice(d.start, d.end));
        queue.push(dm);
      }
    }
  }
  return { code: code.join('\n'), strings };
}

// ── Rust 侧真实消费面（锁 5 的对拍源，全部机器解析）────────────────────────────

/** `const NAME: &[&str] = &["a", "b"];` → 字面量集。 */
function rustStrSlice(src: string, name: string): string[] {
  const m = new RegExp(`const\\s+${name}\\s*:\\s*&\\[&str\\]\\s*=\\s*&\\[([^\\]]*)\\]`).exec(
    stripComments(src)
  );
  expect(m, `Rust 侧 const ${name} 解析失败（改名/改形了？）—— 解析不到必须转红`).not.toBeNull();
  const got = [...(m as RegExpExecArray)[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
  expect(got.length, `const ${name} 没解析出任何字面量 —— 解析器失效`).toBeGreaterThan(0);
  return got;
}

/** 锚点附近第一处 `matches!( … )` 的原文（`back=true` 时向前找，用于 `if !matches!` 在锚点之前的情形）。 */
function rustMatchesArgs(src: string, anchor: string, back = false): string {
  const s = stripComments(src);
  const at = s.indexOf(anchor);
  expect(at, `Rust 侧锚点 \`${anchor}\` 解析失败 —— 解析不到必须转红`).toBeGreaterThanOrEqual(0);
  const mi = back ? s.lastIndexOf('matches!(', at) : s.indexOf('matches!(', at);
  expect(mi, `锚点 \`${anchor}\` ${back ? '之前' : '之后'}找不到 matches!(`).toBeGreaterThanOrEqual(0);
  let depth = 0;
  for (let i = mi + 'matches!'.length; i < s.length; i++) {
    if (s[i] === '(') depth++;
    else if (s[i] === ')') {
      depth--;
      if (depth === 0) return s.slice(mi, i);
    }
  }
  throw new Error(`锚点 \`${anchor}\` 的 matches!( 括号不配对 —— 解析器失效`);
}

/** `Protocol::Xxx` → 小写协议名（枚举变体名与 `PROTO_OPTIONS` 的取值恰好只差首字母大小写）。 */
function protoVariants(seg: string, label: string): string[] {
  const got = [...seg.matchAll(/Protocol::(\w+)/g)].map((m) => m[1].toLowerCase());
  expect(got.length, `${label} 里没解析出任何 Protocol:: 变体 —— 解析器失效`).toBeGreaterThan(0);
  return [...new Set(got)];
}

/** 恒需 TLS 块的协议（`builder/outbound.rs:20`）。 */
const RUST_TLS_PROTOCOLS = rustStrSlice(RUST_OUTBOUND, 'TLS_PROTOCOLS');

/** multiplex 真正下发的协议面（`apply_anti_censorship_options` 里那句 `matches!`）。 */
const RUST_MUX_PROTOCOLS = protoVariants(
  rustMatchesArgs(RUST_OUTBOUND, 'if let Some(mux) = &server.multiplex_settings'),
  'multiplex 的 matches!'
);

/**
 * **能**生成传输层的协议（`const TRANSPORT_CAPABLE: &[Protocol]`）。
 *
 * 2026-08-07 之前这里解析的是黑名单（`if !matches!(Hysteria2|Anytls|Naive)`），方向与内核相反：
 * 内核 schema 的 20 支出站 oneOf 里**只有 trojan/vless/vmess 有 `transport`**，其余 17 支
 * `additionalProperties:false` ⇒ 黑名单放行的那 14 个协议只要拿到 `network != "tcp"`，
 * 产物就是 `FATAL decode config: outbounds[N].transport: unknown field "transport"`，整核起不来。
 * 判据换成白名单后，本门也跟着从「不相交」（弱）改成「精确相等」（两个方向都说话）。
 */
const RUST_TRANSPORT_CAPABLE = (() => {
  const m = /const\s+TRANSPORT_CAPABLE\s*:\s*&\[Protocol\]\s*=\s*&\[([^\]]*)\]/.exec(
    stripComments(RUST_OUTBOUND)
  );
  expect(m, 'Rust 侧 const TRANSPORT_CAPABLE 解析失败（改名/改形了？）—— 解析不到必须转红').not.toBeNull();
  return protoVariants((m as RegExpExecArray)[1], 'TRANSPORT_CAPABLE');
})();

/** TLS 在 QUIC 内自管的协议（`outbound_helpers.rs::is_quic_managed_tls`）。 */
const RUST_QUIC_MANAGED = (() => {
  const s = stripComments(RUST_OUTBOUND_HELPERS);
  const at = s.indexOf('pub fn is_quic_managed_tls');
  expect(at, 'Rust 侧 is_quic_managed_tls 解析失败 —— 解析不到必须转红').toBeGreaterThanOrEqual(0);
  // 只取函数体（到列 0 的 `}` 为止）——放宽到定长窗口会把下一个函数的 `"windows"`/`"apple"` 也吃进来。
  const end = s.indexOf('\n}', at);
  expect(end, 'is_quic_managed_tls 函数体没闭合 —— 解析器失效').toBeGreaterThan(at);
  const got = [...s.slice(at, end).matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  expect(got.length, 'is_quic_managed_tls 没解析出协议字面量 —— 解析器失效').toBeGreaterThan(0);
  return got;
})();

/** spoof 不适用的协议（`tls_spoof.rs::is_tls_spoof_supported_protocol` 的排除名单）。 */
const RUST_NO_SPOOF = (() => {
  const seg = rustMatchesArgs(RUST_TLS_SPOOF, 'pub fn is_tls_spoof_supported_protocol');
  const got = [...seg.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  expect(got.length, 'spoof 协议门没解析出字面量 —— 解析器失效').toBeGreaterThan(0);
  return got;
})();

/** fragment 不支持的协议（`fragment_unsupported = is_quic_managed_tls(…) || … == Protocol::Naive`）。 */
const RUST_NO_FRAGMENT = (() => {
  const s = stripComments(RUST_OUTBOUND);
  const at = s.indexOf('let fragment_unsupported =');
  expect(at, 'Rust 侧 fragment_unsupported 解析失败 —— 解析不到必须转红').toBeGreaterThanOrEqual(0);
  const seg = s.slice(at, s.indexOf(';', at));
  const out = new Set<string>();
  if (seg.includes('is_quic_managed_tls')) for (const p of RUST_QUIC_MANAGED) out.add(p);
  for (const m of seg.matchAll(/Protocol::(\w+)/g)) out.add(m[1].toLowerCase());
  expect(out.size, 'fragment_unsupported 没解析出任何协议 —— 解析器失效').toBeGreaterThan(0);
  return [...out];
})();

// ── 归属表 · 债务表 · 豁免表 ──────────────────────────────────────────────────

/**
 * **归属表**：设置结构体 → 该结构体属于哪些协议的编辑面。
 *
 * 这张表是新维度的支点，所以它自己必须有牙（锁 5）：协议名 ⊆ `PROTO_OPTIONS`，且与 Rust 的真实
 * 消费面机器对拍。**不在表里 = 该协议不核对该结构体**，所以每一条「不给」都要说得出理由：
 *  - `TlsSettings` 不给 shadowsocks / socks / snell / ssh / custom：Rust 那句
 *    `security.is_tls() || tls_settings.is_some()` 是**通用兜底**（导入器写进来什么就带什么），
 *    这五个协议在 sing-box 侧根本没有 TLS 出站语义，上游 也不给控件 ⇒ 不是编辑面。
 *    真要变（Rust 把某个加进 `TLS_PROTOCOLS`），锁 5 的 `TLS_PROTOCOLS ⊆ owners` 会红。
 *  - `HttpSettings` 给 http **协议**：不是笔误 —— `outbound.rs:343` 的 `Protocol::Http` 分支
 *    直接读 `server.http_settings` 的 `headers`/`path`，与 h2 传输那条腿是两处消费。
 */
const STRUCT_OWNERS: Record<string, readonly NodeProto[]> = {
  // 'hysteria'（v1）2026-08-11 进 Rust 的 TLS_PROTOCOLS：随包核对缺 TLS 的 v1 出站判
  // `initialize outbound[0]: TLS required`（initialize 阶段硬失败，不是可选块）。
  TlsSettings: ['vless', 'vmess', 'trojan', 'hysteria2', 'tuic', 'http', 'anytls', 'naive', 'hysteria'],
  RealitySettings: ['vless', 'anytls'],
  WebSocketSettings: ['vless', 'vmess', 'trojan'],
  GrpcSettings: ['vless', 'vmess', 'trojan'],
  HttpSettings: ['vless', 'vmess', 'trojan', 'http'],
  Hysteria2ObfsSettings: ['hysteria2'],
  Hysteria2Settings: ['hysteria2'],
  SnellSettings: ['snell'],
  MultiplexSettings: ['vless', 'vmess', 'trojan', 'shadowsocks'],
  TuicSettings: ['tuic'],
  NaiveSettings: ['naive'],
  ShadowsocksSettings: ['shadowsocks'],
  AnyTlsSettings: ['anytls'],
  SshSettings: ['ssh'],
  ShadowTlsSettings: ['shadowsocks'],
  CustomSettings: ['custom'],
};

/** `结构体::协议` —— 债务表与豁免表共用的键形。 */
const pairKey = (struct: string, proto: string): string => `${struct}::${proto}`;

/** 全部 (结构体, 协议) 组合，锁 4 逐对断言。 */
const OWNER_PAIRS: ReadonlyArray<readonly [string, NodeProto]> = Object.entries(STRUCT_OWNERS)
  .flatMap(([s, ps]) => ps.map((p) => [s, p] as const))
  .sort((a, b) => pairKey(...a).localeCompare(pairKey(...b)));

/**
 * 代码依据：**文件 + 定位锚点 + 依据串**，**不带行号**。
 *
 * `needle` 必须在 `scope` 划出的块里（没写 `scope` 就是全文）**恰好出现一次**。
 *
 * # 为什么不是 `路径:行号`（这一维是 2026-08-17 拆掉的）
 *
 * 旧形态是 `路径:行号` + 「`needle` 落在该行 ±12 行内」。那个窗口是**一份会被漂移慢慢吃掉的余量**：
 *  - 实测（拆之前的 main）：23 条依据里 **17 条行号已经不精确**（全是 `builder/outbound.rs` 的，各差 1 行），
 *    只是还没吃穿窗口，门是绿的 —— 「新写的」与「陈了很久的」在输出上不可区分；
 *  - 往 `outbound.rs` 插 12 行**纯注释**（与本门毫无关系的改动），17 条同时越界，门一次性全红。
 *    收到的信号是「17 条依据都失效了」，而真相是「一条都没失效，只是数字过期了」——
 *    修法只剩「把 17 个数字重算一遍」，那次重算既没有信息量，也是下一次假红的起点。
 *  - 更要命的是窗口**顺带放宽了消歧**：`alpn/engine/spoof/spoof_method/utls: None,` 这五个串在
 *    `outbound.rs` 里**逐字同形地出现在三处**（naive 臂 / 通用 TLS 段 / Reality 段），行号是当时唯一的
 *    消歧手段，而它只精确到 ±12 行 ⇒ 把 naive 的 `engine` 依据写成通用 TLS 段的行号（:480，真实命中 :485），
 *    旧门照绿 —— 一条 naive 的豁免可以拿 Reality 段当证据，没有人会知道。（已实测坐实。）
 *
 * 原作者不要求精确到行的理由是对的：「正常重构会让行号漂几行，那种误红除了逼人改数字没有信息量」。
 * 锚点形态把那件事解决得更彻底 —— 不是把误红的阈值调大，是**让行号不再参与判定**：
 * 上面插多少行都不红，而依据串搬出了它该在的那个块，立刻红。
 *
 * # 换来的新代价（如实记）
 *
 *  - **锚点自身被改写会红**（`Protocol::Naive => {` 若并成 `Protocol::Naive | Protocol::X => {`）。
 *    这是新增的假红面；但修它要写出「那段代码搬到哪儿去了」，是有信息量的一次编辑，
 *    与「把数字 +12」不是一类。**锚点被注释原样引用不算**——锚点在 [`maskRust`] 的掩码上找。
 *  - **依据串在块内出现两次也会红**（旧门会被「邻居」喂饱，取最近的一处判绿）。这是收紧不是放宽：
 *    一条指得到两处的依据，指着哪一处全凭读者猜。
 *  - 🔴 **没写 `scope` 的那 15 条，判定被收紧成「全文恰好一次」** —— 旧门是「全文至少一次 + 落在
 *    ±12 行内」。这是本次改动**唯一一处比旧门严**的地方，写在这里是因为它**不写就是隐式的**：
 *    触发场景是「文件里长出一个逐字同形的姊妹」（如 `tls_spoof.rs` 再加一个判据一模一样的函数），
 *    此时 4 条豁免会红，而它们**一条都没失效**。
 *    **正确修法是补 `scope` 把它钉到该去的那个块，不是把依据串写长** —— 姊妹逐字同形，写多长仍是两处。
 */
interface Cite {
  at: string;
  scope?: string;
  needle: string;
}

/**
 * naive 出站分支的定位锚点。
 * 该臂里的 `insecure/alpn/engine/spoof/spoof_method/utls: None,` 与通用 TLS 段（`engine`/`spoof`/
 * `spoof_method`/`utls`）、Reality 段（`alpn`/`engine`/`spoof`/`spoof_method`）**逐字同形**，靠它消歧。
 */
const NAIVE_ARM = 'Protocol::Naive => {';

interface Exemption {
  why: string;
  cite: readonly Cite[];
}

/** 字节偏移 → 行号。只进报错文案，不参与判定。 */
function lineAt(src: string, offset: number): number {
  return src.slice(0, offset).split('\n').length;
}

/** `needle` 在 `hay` 里的全部出现位置（不重叠）。 */
function offsetsOf(hay: string, needle: string): number[] {
  const out: number[] = [];
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + needle.length)) out.push(i);
  return out;
}

/**
 * Rust 源码的**等长掩码**：注释与字面量的**内容**抹成空格（换行原位保留），偏移量逐字符对齐原文。
 * 同 [`maskTs`] 的用途与纪律 —— 只用来**定位**（花括号配对），从不参与判据。
 *
 * 为什么不复用 [`maskTs`]（四处 Rust 特有语法，任一漏掉都会让配对被骗）：
 *  1. **块注释可嵌套**（一个块注释里能再开一个块注释），TS 不能 —— 按 TS 那样「找第一个块注释结束符」
 *     会提前收尾，剩下半截注释里的花括号照单全收；
 *  2. **raw string** `r"…"` / `r#"…"#` / `br##"…"##`：里面的 `\` 与 `"` 都不是转义/终止符，
 *     本仓 Rust 测试里成片的 `r#"{"id":"s1",…}"#` 全是这一类；
 *  3. **`'` 是重载的**：`'{'` 是字符字面量，而 `'a` / `'static` / `'outer:` 是生命周期与标签。
 *     [`maskTs`] 见 `'` 就当字符串起点，遇上 `&'a str` 会从这里一路抹到下一个 `'`，把中间的
 *     花括号连同代码一起吞掉 —— 这正是不能直接拿它来用的原因；
 *  4. 没有模板字符串（反引号）。
 *
 * 判定 `'` 的规则：后面跟 `\` ⇒ 转义字符字面量（`'\n'` / `'\''` / `'\u{1F600}'`，注意末者含花括号）；
 * 第三个字符是 `'` ⇒ 单字符字面量（`'{'`）；其余一律当生命周期/标签，只跳过这一个引号。
 */
function maskRust(src: string): string {
  const out = src.split('');
  const blank = (from: number, to: number): void => {
    for (let j = Math.max(0, from); j < to && j < out.length; j++) if (out[j] !== '\n') out[j] = ' ';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      const st = i;
      while (i < src.length && src[i] !== '\n') i++;
      blank(st, i);
      continue;
    }
    if (c === '/' && d === '*') {
      const st = i;
      let depth = 1;
      i += 2;
      while (i < src.length && depth > 0) {
        if (src[i] === '/' && src[i + 1] === '*') {
          depth++;
          i += 2;
        } else if (src[i] === '*' && src[i + 1] === '/') {
          depth--;
          i += 2;
        } else i++;
      }
      blank(st, i);
      continue;
    }
    const raw = /^b?r(#*)"/.exec(src.slice(i, i + 16));
    if (raw !== null && !/[\w]/.test(src[i - 1] ?? '')) {
      const st = i;
      const term = `"${raw[1]}`;
      const end = src.indexOf(term, i + raw[0].length);
      i = end < 0 ? src.length : end + term.length;
      blank(st, i);
      continue;
    }
    if (c === '"' || (c === 'b' && d === '"' && !/[\w]/.test(src[i - 1] ?? ''))) {
      const st = i;
      i += c === '"' ? 1 : 2;
      while (i < src.length && src[i] !== '"') i += src[i] === '\\' ? 2 : 1;
      i = Math.min(i + 1, src.length);
      blank(st, i);
      continue;
    }
    if (c === "'") {
      let end = -1;
      if (d === '\\') {
        let k = i + 3;
        while (k < src.length && src[k] !== "'") k++;
        end = k + 1;
      } else if (src[i + 2] === "'") end = i + 3;
      if (end < 0) {
        i++; // 生命周期 / 循环标签，只跳过引号本身
        continue;
      }
      blank(i, end);
      i = end;
      continue;
    }
    i++;
  }
  return out.join('');
}

/** 掩码后的 Rust 源码（依据核对只读这几份，逐次重算没必要）。 */
const MASKED_RUST = new Map<string, string>();
const maskedOf = (path: string, src: string): string => {
  const hit = MASKED_RUST.get(path);
  if (hit !== undefined) return hit;
  const m = maskRust(src);
  MASKED_RUST.set(path, m);
  return m;
};

/**
 * 取定位块的字节区间 `[from, to)`：锚点必须在文件里**唯一**，块体从它之后第一个 `{` 起花括号配对。
 *
 * **锚点与配对都在掩码上做**（[`maskRust`]），依据串本身仍在原文上找 —— 两者分工不同：
 *  · 锚点是**结构定位**，注释里原样引用一句 `Protocol::Naive => {` 不该把它变成「不唯一」；
 *  · 依据串是**证据**，可以是注释（naive 臂那句 `naive TLS 由 Cronet 自管` 就是），不能抹掉。
 *
 * 早先这里图省事在**原文**上配对，理由写的是「注释/字符串里的括号只会让配对早收或收不拢，全是红」。
 * **那是错的，漏了晚收**：naive 臂里加一行 `// 字段清单见 OutboundTls {`（`cargo check` 通过、
 * rustfmt 也认）就让块从 314–342 涨到 314–455，吞掉 Socks 与 Http 两个臂，门 81/81 全绿 ——
 * 本门要消灭的失效模式原样搬了回来，触发门槛还更低。下面的自检钉着这一格。
 */
function braceSpan(
  src: string,
  anchor: string,
  label: string,
  path: string
): { from: number; to: number } {
  const masked = maskedOf(path, src);
  const at = masked.indexOf(anchor);
  expect(
    at,
    `${label} 的定位锚点 \`${anchor}\` 在 ${path} 里找不到 —— 依据指的那段代码已经不在了（或被改写）`
  ).toBeGreaterThanOrEqual(0);
  expect(
    masked.indexOf(anchor, at + 1),
    `${label} 的定位锚点 \`${anchor}\` 在 ${path} 里出现不止一次 —— 锚点必须唯一，否则它会静默绑到第一处`
  ).toBe(-1);
  const open = masked.indexOf('{', at);
  expect(
    open,
    `${label} 的定位锚点 \`${anchor}\` 之后没有 \`{\` —— 锚点必须是一个块的开头`
  ).toBeGreaterThanOrEqual(0);
  let depth = 0;
  for (let i = open; i < masked.length; i++) {
    if (masked[i] === '{') depth++;
    else if (masked[i] === '}') {
      depth--;
      if (depth === 0) return { from: open, to: i };
    }
  }
  throw new Error(
    `${label} 的定位锚点 \`${anchor}\`（${path}:${lineAt(src, open)}）花括号不配对 —— 解析失效，必须转红`
  );
}

/**
 * 核对一条代码依据。四件事任一对不上就红：文件读不到、锚点找不到 / 不唯一、依据串在块内一次没出现、
 * 依据串在块内出现多于一次。**没有「暂时还算数」这种中间态**。
 *
 * 为什么不只校验「文件里有这个串」：那样一条 naive 的豁免可以被 Reality 段的同名行喂饱（那五个串真
 * 在三处同形出现），依据就退化成一句装饰。`scope` 顶替了行号原来干的消歧活，且它不会随无关改动过期。
 */
function verifyCite(label: string, c: Cite): void {
  // 空串必须先红：`indexOf('')` 恒返回起点 ⇒ 下面的扫描会**空转不前进**，那是挂死不是红。
  expect(c.needle.length, `${label} 的依据串是空的 —— 空串等于没有依据`).toBeGreaterThan(0);
  let src = '';
  try {
    src = read(`../../../${c.at}`);
  } catch {
    src = '';
  }
  expect(src.length, `${label} 的依据文件 \`${c.at}\` 读不到 —— 依据必须指得到真文件`).toBeGreaterThan(0);
  const span =
    c.scope === undefined ? { from: 0, to: src.length } : braceSpan(src, c.scope, label, c.at);
  const where =
    c.scope === undefined
      ? c.at
      : `${c.at} 的 \`${c.scope}\` 块（${lineAt(src, span.from)}–${lineAt(src, span.to)} 行）`;
  const hits = offsetsOf(src.slice(span.from, span.to), c.needle).map((o) =>
    lineAt(src, span.from + o)
  );
  expect(
    hits.length,
    `${label} 的依据串 \`${c.needle}\` 在 ${where} 里一次都没出现 —— 依据已失效：` +
      `要么那段代码没了（那豁免的前提也没了，该改的是豁免不是依据），要么它搬了家（改 \`scope\` 锚点）`
  ).toBeGreaterThan(0);
  expect(
    hits.length,
    `${label} 的依据串 \`${c.needle}\` 在 ${where} 里命中 ${hits.length} 次（第 ${hits.join(' / ')} 行）` +
      ` —— 依据必须唯一指得到一处，否则指着哪一处全凭读者猜。` +
      `修法是**补一个 \`scope\` 锚点**把它钉进该去的那个块；` +
      `只有在同块内确实同形时，加长依据串才有用（跨块的姊妹写多长都还是两处）`
  ).toBe(1);
}

/**
 * **有意排除**（≠ 还没做）。与 [`PORT_DEBT`] 是**两张表**，这是批 C 最要紧的一条改动。
 *
 * 为什么必须拆开：同一批字段曾在 `node-spec.ts` 里被称作「有意不建模的高级逃生舱」、在本门里
 * 又被称作「属移植进度，**不是**设计豁免」—— 两种定性同时挂在一批字段上，谁也说不清它到底该不该补。
 * 根因就是这两类在门里**没有类型区别**，只能靠人读散文注释分辨，而散文注释谁都能加。
 * 拆开之后规矩变成硬的：**「有意排除」必须指得出一行代码依据，指不出就只能进债务表。**
 */
/**
 * hy2 / tuic 共用的五条 —— **TLS 在 QUIC 里自管**，这五个键在 Rust 侧有明确的前置门把它们挡掉。
 * 五条依据各不相同（engine / fingerprint / fragment / spoof 分属四处门），逐条列而不是共用一条。
 */
const QUIC_TLS_EXEMPT: Record<string, Exemption> = {
  engine: {
    why:
      'hy2/tuic 的 TLS 在 QUIC 栈内自管，`tls.engine` 有 `is_quic_managed_tls` 前置门 ⇒ 永远不下发。' +
      '给控件 = 一个拨了必然不生效的开关。',
    cite: [
      {
        at: 'crates/config-engine/src/builder/outbound.rs',
        needle: '!is_quic_managed_tls(&protocol) && should_emit_tls_engine',
      },
      { at: 'crates/config-engine/src/builder/outbound_helpers.rs', needle: 'p == "hysteria2" || p == "tuic" || p == "hysteria"' },
    ],
  },
  fingerprint: {
    why: 'uTLS 指纹同理：`is_quic_managed_tls` 前置门挡在 `final_fp != "none"` 之前 ⇒ utls 块对这两个协议永不下发。',
    cite: [
      {
        at: 'crates/config-engine/src/builder/outbound.rs',
        needle: '!is_quic_managed_tls(&protocol) && final_fp != "none"',
      },
    ],
  },
  fragment: {
    why: 'ClientHello 分片是 TCP-TLS 的手法；`fragment_unsupported` 把 QUIC 自管的两个协议排除在外。',
    cite: [
      {
        at: 'crates/config-engine/src/builder/outbound.rs',
        needle: 'is_quic_managed_tls(&protocol_lower) || server.protocol == Protocol::Naive',
      },
    ],
  },
  spoofSni: {
    why: 'TLS spoof 要伪造一个 TCP ClientHello，QUIC 里没有；`is_tls_spoof_supported_protocol` 直接排除这两个协议。',
    cite: [
      {
        at: 'crates/config-engine/src/user_config/tls_spoof.rs',
        needle: '!matches!(p.as_str(), "hysteria2" | "tuic" | "naive")',
      },
    ],
  },
  spoofMethod: {
    why: '同 `spoofSni` —— 两键是一对，同一道协议门挡掉，单给一个也不会生效。',
    cite: [
      {
        at: 'crates/config-engine/src/user_config/tls_spoof.rs',
        needle: '!matches!(p.as_str(), "hysteria2" | "tuic" | "naive")',
      },
    ],
  },
};

/**
 * `GrpcSettings.multiMode` —— **不是欠账，是本仓建模过头**：这个键在本仓没有通往内核的路径。
 * 它是 xray 的扩展，sing-box 没有；唯一活路是 share-link 往返保真，属机器写入的保真位、非用户可调项。
 */
const GRPC_MULTIMODE_EXEMPT: Record<string, Exemption> = {
  multiMode: {
    why:
      '`singbox::Transport` 结构体根本没有 `multi_mode` 字段 ⇒ 本仓无通往内核的路径；随包核 beta.7 的 ' +
      'grpc 传输 schema 是 `additionalProperties:false` 且无此键，真下发反而 FATAL。已有 Rust 断言钉住' +
      '「将来结构体真加了该字段就转红」。给它控件 = 造一个拨了永远不生效的假开关。',
    cite: [
      { at: 'crates/config-engine/src/singbox/outbound.rs', needle: 'pub struct Transport {' },
      {
        at: 'crates/config-engine/src/builder/outbound.rs',
        needle: 'grpc_multi_mode_never_reaches_the_kernel',
      },
    ],
  },
};

const NODE_EXEMPT: Record<string, Record<string, Exemption>> = {
  [pairKey('TlsSettings', 'hysteria2')]: QUIC_TLS_EXEMPT,
  // hysteria v1 与 hy2/tuic 在上游走同一个 tls.NewClient，TLS 同由 QUIC 栈接管 ⇒ 同一份豁免。
  [pairKey('TlsSettings', 'hysteria')]: QUIC_TLS_EXEMPT,
  [pairKey('TlsSettings', 'tuic')]: QUIC_TLS_EXEMPT,
  /**
   * naive 的 TLS 由 Cronet 自管，`Protocol::Naive` 分支**新造一个 `OutboundTls` 并把除 `server_name`
   * 外的每一项写死 `None`** ⇒ 这几个键在 naive 上一律到不了内核。
   *
   * **内核侧还有一张显式拒绝名单**（2026-08-06 实测，随包核 beta.7 —— 逐项 `sing-box check`）：
   * naive 出站对 `insecure` / `alpn` / `uTLS` / `fragment` / `reality` / `min_version` /
   * `max_version` / `disable_sni` / `cipher_suites` / `curve_preferences` / `client_certificate` /
   * `client_key` / kernel TLS 一律 `FATAL … is not supported on naive outbound`。
   * 这比仓内那句散文注释硬得多：下面每条豁免既指得到本仓写死 `None` 的那一行，也有内核点名。
   *
   * ⚠️ **`ech`/`echConfig` 不在这张名单里，也不在本表里** —— 批 C 曾把它们记成债务（推理：
   * `apply_anti_censorship_options` 在 `ech: None` 之后运行且只看 `ob.tls.is_some()`）。批 D 实测坐实：
   * 内核拒绝名单里没有 ech，且喂坏 PEM 时 naive 与 trojan 报**同一句** `invalid ECH configs pem`
   * ⇒ 走同一条 ECH 装配路径。两头都通 ⇒ 已补控件，债务还清（`naive_ech_survives_the_branch_writing_none`）。
   */
  [pairKey('TlsSettings', 'naive')]: {
    allowInsecure: {
      why:
        'naive 的 TLS 由 Cronet 自管，`insecure` 写死 None；内核侧另有点名拒绝' +
        '（`insecure is not supported on naive outbound`，实测 exit=1）。',
      cite: [
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'naive TLS 由 Cronet 自管' },
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'insecure: None,' },
        {
          at: 'crates/config-engine/src/builder/outbound.rs',
          needle: 'naive_tls_branch_pins_the_kernel_reject_list',
        },
      ],
    },
    alpn: {
      why:
        '同上：naive 分支把 `alpn` 写死 None，内核点名拒绝（`alpn is not supported on naive outbound`）。' +
        '这是 上游 与本仓一致的既有结论，批 D 补上了机器可核对的出处。',
      cite: [
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'naive TLS 由 Cronet 自管' },
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'alpn: None,' },
        {
          at: 'crates/config-engine/src/builder/outbound.rs',
          needle: 'naive_tls_branch_pins_the_kernel_reject_list',
        },
      ],
    },
    engine: {
      why: 'naive 分支自造的 TLS 块把 `engine` 写死 None（Cronet 自带 TLS 栈，选谁都没有意义）。',
      cite: [
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'engine: None,' },
        {
          at: 'crates/config-engine/src/builder/outbound.rs',
          needle: 'naive_tls_branch_pins_the_kernel_reject_list',
        },
      ],
    },
    fingerprint: {
      why:
        'uTLS 块（`utls`）同样写死 None —— 指纹由 Cronet 决定；内核点名拒绝' +
        '（`uTLS is not supported on naive outbound`）。前端给档位只是假控件。',
      cite: [
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'utls: None,' },
        {
          at: 'crates/config-engine/src/builder/outbound.rs',
          needle: 'naive_tls_branch_pins_the_kernel_reject_list',
        },
      ],
    },
    fragment: {
      why: '`fragment_unsupported` 显式含 naive（与 QUIC 两协议同一处判据）。',
      cite: [
        {
          at: 'crates/config-engine/src/builder/outbound.rs',
          needle: 'is_quic_managed_tls(&protocol_lower) || server.protocol == Protocol::Naive',
        },
      ],
    },
    spoofSni: {
      why: '`is_tls_spoof_supported_protocol` 的排除名单里点名 naive；naive 分支也把 `spoof` 写死 None。',
      cite: [
        {
          at: 'crates/config-engine/src/user_config/tls_spoof.rs',
          needle: '!matches!(p.as_str(), "hysteria2" | "tuic" | "naive")',
        },
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'spoof: None,' },
      ],
    },
    spoofMethod: {
      why: '同 `spoofSni`：协议门 + naive 分支的 `spoof_method: None` 两道都挡着。',
      cite: [
        {
          at: 'crates/config-engine/src/user_config/tls_spoof.rs',
          needle: '!matches!(p.as_str(), "hysteria2" | "tuic" | "naive")',
        },
        { at: 'crates/config-engine/src/builder/outbound.rs', scope: NAIVE_ARM, needle: 'spoof_method: None,' },
      ],
    },
  },
  [pairKey('GrpcSettings', 'vless')]: GRPC_MULTIMODE_EXEMPT,
  [pairKey('GrpcSettings', 'vmess')]: GRPC_MULTIMODE_EXEMPT,
  [pairKey('GrpcSettings', 'trojan')]: GRPC_MULTIMODE_EXEMPT,
};

/**
 * **移植债务棘轮**（同 locale-parity 的 MISSING_KEY_DEBT：精确相等，只许降不许升）。
 *
 * 为什么用精确列表而不是「数量 ≤ 上限」：`≤` 挡不住「补一个又漏一个」（总数不变）也挡不住调高基线消音。
 * 精确列表两个方向都会说话——多出来 = 新字段没接线；少了 = 债务已还，把它从表里删掉。
 *
 * 表里的项 = 「Rust 会消费、NodeDialog 没做」的项，**属移植进度**。有代码行依据的「有意排除」
 * 不进本表，走 [`NODE_EXEMPT`]。空数组的行不许存在（还清了就删行，见锁 6）。
 */
const PORT_DEBT: Record<string, readonly string[]> = {
  // ── HttpSettings × **http 协议**（唯一剩余的一行）────────────────────────────
  //
  // 批 D 还清了 `TlsSettings` 的 alpn×5 / fingerprint×http / fragment×5、`Hysteria2Settings.network`、
  // `TlsSettings.ech`+`echConfig`×naive，以及 `HttpSettings` 四键 × vless/vmess/trojan（h2 传输控件）。
  // 剩下这一行**不是漏做，是做不了** —— 但也够不上「有意排除」，故仍留在债务表：
  //
  // 🔴 **`Protocol::Http` 分支产出的是一份内核拒绝加载的配置**（2026-08-06 实测，随包核 beta.7）：
  // 它把 `http_settings` 的 headers/path 塞进 `ob.transport`（`builder/outbound.rs` 的
  // `Protocol::Http` 腿，1:1 移植自 上游 `singbox-outbound-builder.ts:391-398`），而随包核的
  // **http 出站 schema 根本没有 `transport` 键**且 `additionalProperties:false` ⇒
  //   `sing-box check` → `FATAL decode config: outbounds[0].transport: json: unknown field "transport"`
  // 正向对照：同一份 headers/path 写在出站**顶层**（内核 http 出站真有这两个键）→ exit=0。
  // 另两键更彻底：`host`/`method` 在内核 http 出站的 schema 里**压根不存在**（写顶层同样 FATAL），
  // 且 `Protocol::Http` 分支也从不读它们。
  //
  // ⇒ 此刻给这张表加任何一颗控件，都是「填了就把整份配置写死」的坏功能，比没有控件更糟。
  // 修它要动生产 Rust（`singbox::Outbound` 加顶层 path/headers 两个字段 + 改 `Protocol::Http` 腿），
  // 与批 B 记下的「Reality 吞掉 tls.engine」同性质：**属另一批，在那之前不露控件**。
  // 归属表仍保留 `HttpSettings → http`（`STRUCT_OWNERS`），这一行就是它的账。
  [pairKey('HttpSettings', 'http')]: ['headers', 'host', 'method', 'path'],
};

/**
 * 全部被核对的结构体 → 它所在的 Rust 源文件。
 *
 * 清单来源从 `PORT_DEBT` 的键改成 `STRUCT_OWNERS` 的键 —— 顺手补上了一个存量漏检：
 * `AnyTlsSettings` 在 `MIN_FIELDS` 里有、在旧 `PORT_DEBT` 里没有 ⇒ 它既不在锁 1（双向锁）
 * 也不在锁 4 的射程里，`MIN_FIELDS.AnyTlsSettings` 这一行从来没被用过。
 */
const ALL_STRUCTS: ReadonlyArray<readonly [string, string]> = [
  ['WireGuardSettings', RUST_SERVER_CONFIG],
  ['TailscaleSettings', RUST_SERVER_CONFIG],
  ...Object.keys(STRUCT_OWNERS).map((n) => [n, RUST_PROTOCOL_SETTINGS] as const),
];

/**
 * 解析器自检的字段数下界 = 本门落地时的实测值。
 * 用 **≥ 而非 ==**：加字段不该让「解析器还活着吗」这条自检误红（该红的是下面的覆盖锁与双向锁），
 * 但结构体被删空/解析失效会立刻红。
 */
const MIN_FIELDS: Record<string, number> = {
  WireGuardSettings: 12,
  TailscaleSettings: 17,
  TlsSettings: 10,
  RealitySettings: 2,
  WebSocketSettings: 4,
  GrpcSettings: 2,
  HttpSettings: 4,
  Hysteria2ObfsSettings: 4,
  Hysteria2Settings: 7,
  SnellSettings: 7,
  MultiplexSettings: 5,
  TuicSettings: 4,
  NaiveSettings: 1,
  ShadowsocksSettings: 4,
  AnyTlsSettings: 3,
  SshSettings: 11,
  ShadowTlsSettings: 4,
  CustomSettings: 3,
};

// ── 断言 ──

describe('解析器自检（没解析到必须自曝）', () => {
  it('每个结构体都解析出字段，且规模不低于落地时实测值', () => {
    for (const [name, src] of ALL_STRUCTS) {
      const keys = rustJsonKeys(src, name);
      expect(keys.length, `${name} 字段数塌陷 —— 多半是解析器失效而非真删了字段`).toBeGreaterThanOrEqual(
        MIN_FIELDS[name]
      );
      expect(new Set(keys).size, `${name} 解析出重复键 —— 解析器把同一字段抓了两次`).toBe(keys.length);
    }
  });

  it('rename 解析确实生效（不是把字段名当键名蒙对的）', () => {
    // snake_case 字段 + rename 成 camelCase：若 rename 解析坏了会拿到 `always_route_subnets`。
    const ts = rustJsonKeys(RUST_SERVER_CONFIG, 'TailscaleSettings');
    expect(ts).toContain('alwaysRouteSubnets');
    expect(ts).not.toContain('always_route_subnets');
    // 无 rename 的字段按字段名原样取。
    expect(ts).toContain('routes');
    expect(ts).toContain('hostname');
    // 多行属性块里的 rename（`#[serde(\n  rename = "…",\n  …\n)]`）同样要认出来。
    expect(ts).toContain('exitNodeAllowLanAccess');
    expect(ts).toContain('acceptDefaultResolvers');
  });

  it('注释剔除生效（否则注释里的键名会把真实缺口盖绿）', () => {
    // wg-logic.ts 的注释里写着「起底非表单字段（reverseMesh / warpDevice / reserved…）」。探针取
    // `warpDevice`：它在该文件里**只出现在注释**（起底靠 `...base` 整份展开，没有逐键写法），
    // 剔注释后必须一个不剩。
    //
    // 探针原本是 `reverseMesh`——WG 接入模式开关补上后它成了真代码，那条断言会以「注释剔除失效」
    // 的名义误红。换键而非删这条：要守的不变式（注释里的键名不得算作覆盖）没变，只是原探针被填实了。
    const raw = read('../components/dialogs/wg-logic.ts');
    expect(raw, '前提变了：wg-logic.ts 注释里已不再提 warpDevice，本条自检失去意义').toContain(
      'warpDevice'
    );
    expect(scanTs(raw).code).not.toContain('warpDevice');
  });

  /**
   * [`maskRust`] 是**承重件**：块区间由它定，它被骗一次，整张豁免表的「指对地方」就同时失守。
   * 实测：naive 臂里加一行 `// … OutboundTls {`（`cargo check` 与 rustfmt 都过），在原文上配对时
   * 块从 314–342 涨到 314–455，吞掉 Socks 与 Http 两个臂，而门 81/81 全绿。
   *
   * 前半在**真文件**上核对「只抹内容、不吃结构」。后半用一小段内联 Rust —— 因为生命周期 `'a`、
   * 嵌套块注释、`'{'` 字符字面量这三样**今天的 `outbound.rs` 里一个都没有**，只测真文件对这三类的
   * 检出力是 **0**，绿了说明不了任何事；而它们恰恰是「不能直接复用 [`maskTs`]」的那三条理由。
   */
  it('Rust 掩码只抹内容、不吃结构（含真文件里暂时没有的三类语法）', () => {
    const src = read('../../../crates/config-engine/src/builder/outbound.rs');
    const masked = maskRust(src);
    expect(masked.length, '掩码与原文不等长 —— 偏移量整体错位，块区间会指到别处').toBe(src.length);
    for (const s of [
      'Protocol::Naive => {',
      'Protocol::Socks => {',
      'fn apply_anti_censorship_options',
    ]) {
      expect(masked, `掩码把结构 \`${s}\` 吃掉了 —— 锚点就再也找不到`).toContain(s);
    }
    expect(masked, '行注释的内容没抹掉').not.toContain('naive TLS 由 Cronet 自管');
    expect(masked, '原始字符串的内容没抹掉').not.toContain('"protocol":"naive"');

    const probe = [
      "fn f<'a>(x: &'a str) -> &'a str {",
      "    let _c = '{';",
      '    /* 外 /* 内 { 仍在注释里 } */ */',
      '    let _r = r#"}}{{"#;',
      '    x',
      '}',
    ].join('\n');
    const pm = maskRust(probe);
    expect(pm.length).toBe(probe.length);
    expect(pm, "生命周期 `'a` 被当成字符字面量 ⇒ 会从这里一路抹到下一个引号，把代码一起吃掉").toContain(
      "fn f<'a>(x: &'a str) -> &'a str {"
    );
    expect(
      (pm.match(/[{}]/g) ?? []).join(''),
      '掩码后只该剩函数体那一对花括号 —— 多出来的每一个都会让配对错位'
    ).toBe('{}');
  });

  /**
   * [`verifyCite`] 自身的牙 —— **把变异内建进门里**。
   *
   * 手工跑一次变异只证明「今天有牙」；下一个人把 `verifyCite` 改成早返回、或把「块内唯一」放宽成
   * 「块内出现过」，豁免表就会在无人察觉的情况下退回装饰品。⑤ 那格尤其要钉：**它正是行号 ±12 窗口
   * 守不住的那一格**（旧判据取离记录行最近的一处判绿，同块里的邻居会把依据喂饱）。
   *
   * **label 逐个取自真表，不用 `'probe'` 这类自检专用值**：用专用值时，一刀
   * `if (label !== 'probe') return;` 能让自检全绿而 23 条真依据一条没校验（实测坐实）。
   * 换成「真表里的**每一个** label 都跑一遍全套探针」之后，这条向量就关死了 ——
   * 任何按 label 放行的刀，放过某条真依据就必然放过挂同一 label 的探针（该转红的探针转绿 → 红）；
   * 拦下探针就必然也拦下那条真依据。做不出「只偏袒自检」的切法。
   *
   * 前四条是**正向对照**（真依据必须不抛，块的两条边都要钉住）。没有它们，下面五条可以被
   * 「`verifyCite` 恒抛」蒙对，块边界塌成一行也照样「全都抛」。
   */
  it('依据核对器自身有牙（块边界两侧 + 五类失效，且真依据不抛）', () => {
    const OB = 'crates/config-engine/src/builder/outbound.rs';
    const labels = Object.entries(NODE_EXEMPT).flatMap(([row, t]) =>
      Object.keys(t).map((k) => `NODE_EXEMPT["${row}"].${k}`)
    );
    expect(labels.length, '真表一条豁免都没有 —— 自检失去载体，等于没跑').toBeGreaterThan(0);
    for (const L of labels) {
      expect(() => verifyCite(L, { at: OB, scope: NAIVE_ARM, needle: 'alpn: None,' })).not.toThrow();
      // 远边：块不许**早收**（`ob.quic` 在 naive 臂的最后几行；收早了这条就找不到）。
      expect(() =>
        verifyCite(L, { at: OB, scope: NAIVE_ARM, needle: 'ob.quic = Some(true);' })
      ).not.toThrow();
      // 🔴 近邻：块不许**晚收**。`OutboundVersion::Str("5".to_string())` 是紧邻的 `Protocol::Socks`
      //    臂独有的一句，naive 的块**绝不能**含它。在原文上配对时，naive 臂里只要多一行
      //    `// … OutboundTls {`，块就从 314–342 涨到 314–455 吞掉 Socks 与 Http 两个臂，而门全绿 ——
      //    这一条就是钉那一格的（配对已改在 [`maskRust`] 的掩码上做）。
      expect(() =>
        verifyCite(L, { at: OB, scope: NAIVE_ARM, needle: 'OutboundVersion::Str("5".to_string())' })
      ).toThrow(/一次都没出现/);
      // 上一条的正向对照：那句本身还在文件里（否则它是因为被删了才「不在块内」，钉不住任何东西）。
      expect(() =>
        verifyCite(L, { at: OB, needle: 'OutboundVersion::Str("5".to_string())' })
      ).not.toThrow();
      // ① 锚点找不到（代码搬走 / 被改写）。
      expect(() =>
        verifyCite(L, { at: OB, scope: 'Protocol::NoSuchArm => {', needle: 'alpn: None,' })
      ).toThrow(/定位锚点/);
      // ② 锚点不唯一：`ob.tls = Some(OutboundTls {` 在 naive 臂 / 通用 TLS 段 / Reality 段三处同形，
      //    这种锚点会静默绑到第一处 —— 必须红，不许「反正第一处就是我要的」。
      expect(() =>
        verifyCite(L, { at: OB, scope: 'ob.tls = Some(OutboundTls {', needle: 'alpn: None,' })
      ).toThrow(/出现不止一次/);
      // ③ 依据串在块内一次都没出现（naive 臂恰恰不写 `alpn: Some(`）。
      expect(() => verifyCite(L, { at: OB, scope: NAIVE_ARM, needle: 'alpn: Some(' })).toThrow(
        /一次都没出现/
      );
      // ④ 文件读不到。
      expect(() => verifyCite(L, { at: 'crates/no/such/file.rs', needle: 'x' })).toThrow(/读不到/);
      // ⑤ 依据串在块内命中多次 ⇒ 指着哪一处全凭猜（naive 臂里 `: None,` 有一大把）。
      expect(() => verifyCite(L, { at: OB, scope: NAIVE_ARM, needle: ': None,' })).toThrow(
        /命中 \d+ 次/
      );
    }
  });
});

describe('锁 1：Rust 结构体 ↔ 前端 interface 双向锁', () => {
  /**
   * 牙（两个方向各一）：
   *  · 只改 Rust（加字段不同步 `contracts/types/protocol-settings.ts`）→ Rust 多一键 → 红。
   *  · 只改前端（拼错 `advertsieRoutes` / 留着已删字段）→ 前端多一键 → 红。
   */
  it.each(ALL_STRUCTS.map(([n]) => n))('%s 的 JSON 键集两侧相等', (name) => {
    const src = ALL_STRUCTS.find(([n]) => n === name)![1];
    const rust = [...rustJsonKeys(src, name)].sort();
    const ts = [...tsInterfaceKeys(TS_PROTOCOL_SETTINGS, name)].sort();
    expect(ts, `${name}：前端类型与 Rust 结构体的 JSON 键集漂移`).toEqual(rust);
  });
});

describe('锁 2：组网结构体的 UI 覆盖面', () => {
  it.each(MESH_TARGETS.map((t) => t.struct))('%s 每个字段都可编辑（或有带理由的豁免）', (structName) => {
    const target = MESH_TARGETS.find((t) => t.struct === structName)!;
    const keys = rustJsonKeys(target.rust, structName);
    const missing = uncoveredKeys(keys, editorIndex(target.editors));
    const exempt = EXEMPT[structName] ?? {};
    const unexplained = missing.filter((k) => !(k in exempt));
    expect(
      unexplained,
      `${structName} 有字段在 Rust 侧活着、config-engine 会消费、磁盘存得下，但 UI 既看不见也改不了。` +
        `请接进编辑器（${target.editors.join(' / ')}），或在 EXEMPT.${structName} 里写明理由。`
    ).toEqual([]);
  });
});

describe('锁 3：豁免表反向锁（豁免不许变成永久盲区）', () => {
  it('豁免项必须是真实存在的 JSON 键', () => {
    for (const [structName, table] of Object.entries(EXEMPT)) {
      const target = MESH_TARGETS.find((t) => t.struct === structName);
      expect(target, `EXEMPT 里的 ${structName} 不在被核对清单中`).toBeDefined();
      const keys = new Set(rustJsonKeys(target!.rust, structName));
      for (const k of Object.keys(table)) {
        expect(keys.has(k), `EXEMPT.${structName}.${k} 在 Rust 结构体里不存在 —— 死豁免，删掉它`).toBe(
          true
        );
      }
    }
  });

  it('豁免项必须确实未被覆盖（补上了就得删豁免）', () => {
    for (const [structName, table] of Object.entries(EXEMPT)) {
      const target = MESH_TARGETS.find((t) => t.struct === structName)!;
      const missing = new Set(uncoveredKeys(rustJsonKeys(target.rust, structName), editorIndex(target.editors)));
      for (const k of Object.keys(table)) {
        expect(
          missing.has(k),
          `EXEMPT.${structName}.${k} 已经有编辑入口了 —— 把这条豁免删掉，否则它日后回退也不会有人知道`
        ).toBe(true);
      }
    }
  });

  it('每条豁免都带非空理由', () => {
    for (const [structName, table] of Object.entries(EXEMPT)) {
      for (const [k, reason] of Object.entries(table)) {
        expect(reason.trim().length, `EXEMPT.${structName}.${k} 没写理由`).toBeGreaterThan(20);
      }
    }
  });
});

describe('per-protocol 切片自检（新维度必须真的隔离，否则整批断言都是空转）', () => {
  it('每个协议的两段都切得出来，且规模远小于全文', () => {
    const wholeCodeLen = NODE_EDITOR_FILES.reduce((n, f) => n + read(f).length, 0);
    for (const [proto] of PROTO_OPTIONS) {
      // 借 CustomSettings 之外的任一结构体取一次索引；这里只关心切片本身切没切出东西。
      const idx = protoStructIndex(proto, 'TlsSettings');
      expect(idx.code.length, `${proto} 的切片是空的 —— 切片器失效`).toBeGreaterThan(50);
      expect(idx.code.length, `${proto} 的切片和全文一样大 —— 等于没切`).toBeLessThan(wholeCodeLen * 0.8);
    }
  });

  it('正反对照：该协议有的判绿、别协议有的判红（这正是旧判据分不出来的那一格）', () => {
    // 正向：hy2 自己那块真有 bbrProfile。
    expect(isCovered('bbrProfile', protoStructIndex('hysteria2', 'Hysteria2Settings'))).toBe(true);
    // 反向：grpc 的 serviceName 是 vless/vmess/trojan 的传输参数，hy2 根本没有传输层。
    expect(isCovered('serviceName', protoStructIndex('hysteria2', 'GrpcSettings'))).toBe(false);
    // 反向：ShadowTLS 只有 shadowsocks 有；vless 那块的 `{k:'sni'}` 不该再盖到它头上。
    expect(isCovered('password', protoStructIndex('shadowsocks', 'ShadowTlsSettings'))).toBe(true);
    expect(isCovered('port', protoStructIndex('vless', 'ShadowTlsSettings'))).toBe(false);
  });

  /**
   * ⚠️ 探针必须挂在**当前还欠着的**债务项上。原先那两个（`Hysteria2Settings.network` /
   * `HttpSettings.path × vless`）已被批 D 填实 ⇒ 本条曾以「反向对照失效」的名义误红。
   * **换探针、不删这条**（同 `warpDevice` 那次的换法）：要守的不变式没变 —— 旧的全文并集判据
   * 会把「别协议 / 别结构体的同名键」判绿。现用债务表仅存的那行 `HttpSettings::http` 当探针。
   *
   * 顺带把 `withHostHeader` 改名（`hostValue` → `host`）的**回归对照**钉在这里：改名后 `host` 在
   * 全文并集里必然判绿，而归属过滤要保证它既不算 `WebSocketSettings`（该 helper 不点名 wsSettings）
   * 也不算 http 协议的 `HttpSettings` —— 这正是批 C 那条「改名要动生产文件，留给下一批」的兑现证据。
   */
  it('旧的全文并集判据确实在记假账（本次改造的存在理由，反向对照）', () => {
    const legacy = editorIndex(NODE_EDITOR_FILES);
    // ① `HttpSettings.host`：全文并集下被判绿（`withHostHeader` 的形参 + `httpPatch` 的键，两处都在）。
    expect(isCovered('host', legacy)).toBe(true);
    // ② 同一协议内**跨结构体**：vless 的 h2 那块真有 host 控件，ws 那块没有 ——
    //    `withHostHeader` 不点名 `wsSettings`/`WebSocketSettings` ⇒ 归属过滤不把它算作 ws 的覆盖。
    //    这一格就是「形参可以叫回 `host` 而不制造新遮蔽」的机器证明。
    expect(isCovered('host', protoStructIndex('vless', 'HttpSettings'))).toBe(true);
    expect(isCovered('host', protoStructIndex('vless', 'WebSocketSettings'))).toBe(false);
    // ③ **跨协议**：http 协议表单没有任何 h2 传输控件（`httpPatch` 不在它的切片里）⇒ 同一结构体判红。
    expect(isCovered('host', protoStructIndex('http', 'HttpSettings'))).toBe(false);
    // ④ `HttpSettings.method` 同型：并集里被 shadowsocks 的 `{k:'method'}` 判绿，http 协议那块没有。
    expect(isCovered('method', legacy)).toBe(true);
    expect(isCovered('method', protoStructIndex('http', 'HttpSettings'))).toBe(false);
  });

  it('注释与字符串不参与切片定位（掩码有效）', () => {
    const idx = sourceIndex('../components/dialogs/proto-codec.ts');
    // `withHostHeader` 的注释里整段引用了本门的判据说明，掩码后不得留下可被当成代码的痕迹。
    expect(idx.raw).toContain('protocol-settings-coverage');
    expect(idx.masked).not.toContain('protocol-settings-coverage');
    // 字符串内容抹白、引号留着（否则花括号配对会错位）。
    expect(idx.masked).not.toContain('自定义协议 JSON 必须是对象');
  });
});

describe('锁 4：协议结构体的移植债务棘轮（per-protocol，只许降不许升）', () => {
  /**
   * 牙（三个方向）：
   *  · 给某协议的 Rust 结构体加字段而不接 NodeDialog → 该 (结构体, 协议) 的未覆盖列表多一项 → 红；
   *  · 删掉某协议的一颗控件 → **只有那个协议**那条转红（旧判据会被别的协议遮蔽掉，这是本批的目的）；
   *  · 补齐了某项却没更新表 → 列表少一项 → 也红（提示把行删掉）。
   */
  it.each(OWNER_PAIRS.map(([s, p]) => pairKey(s, p)))('%s 未覆盖键 = 债务 + 豁免', (pair) => {
    const [struct, proto] = pair.split('::');
    const keys = rustJsonKeys(RUST_PROTOCOL_SETTINGS, struct);
    const missing = uncoveredKeys(keys, protoStructIndex(proto, struct));
    const expected = [
      ...(PORT_DEBT[pair] ?? []),
      ...Object.keys(NODE_EXEMPT[pair] ?? {}),
    ].sort();
    expect(
      [...missing].sort(),
      `${pair}：未覆盖字段与 PORT_DEBT + NODE_EXEMPT 记录不符。` +
        `多出来 = 该协议下这个字段没有编辑入口（补控件，或登记成债务/带依据的豁免）；` +
        `少了 = 已经补上了，请把那一行删掉（只许降不许升）。`
    ).toEqual(expected);
  });
});

describe('锁 5：归属表自身的牙（否则归属表就是下一个盲区）', () => {
  const owners = (s: string): readonly string[] => STRUCT_OWNERS[s] ?? [];
  const PROTO_NAMES = new Set(PROTO_OPTIONS.map(([v]) => v as string));

  it('协议名 ⊆ PROTO_OPTIONS（写错一个字母就红，不是自由文本）', () => {
    for (const [struct, ps] of Object.entries(STRUCT_OWNERS)) {
      expect(ps.length, `STRUCT_OWNERS.${struct} 一个协议都没有 —— 该结构体没人核对，等于新盲区`).toBeGreaterThan(
        0
      );
      for (const p of ps) {
        expect(PROTO_NAMES.has(p), `STRUCT_OWNERS.${struct} 里的 "${p}" 不是 PROTO_OPTIONS 里的协议`).toBe(
          true
        );
      }
      expect(new Set(ps).size, `STRUCT_OWNERS.${struct} 有重复协议名`).toBe(ps.length);
    }
  });

  it('每个结构体在 ServerConfig 上都解析得到 JSON 字段名（归属过滤的第二个 needle）', () => {
    for (const struct of Object.keys(STRUCT_OWNERS)) {
      expect(
        SETTINGS_FIELD.get(struct),
        `${struct} 在 ServerConfig 上没解析到 serde 字段名 —— 归属过滤会少一半 needle`
      ).toBeTruthy();
    }
  });

  it('Rust `TLS_PROTOCOLS` 全在 TlsSettings 的 owners 里（Rust 给某协议接了 TLS 而归属表没跟 → 红）', () => {
    for (const p of RUST_TLS_PROTOCOLS) {
      expect(
        owners('TlsSettings'),
        `builder/outbound.rs 的 TLS_PROTOCOLS 含 "${p}"（该协议恒有 TLS 块），但 STRUCT_OWNERS.TlsSettings 里没有它`
      ).toContain(p);
    }
  });

  it('Rust multiplex 的 `matches!` 分支 == MultiplexSettings 的 owners（精确相等，两个方向都说话）', () => {
    expect(
      [...owners('MultiplexSettings')].sort(),
      'multiplex 的协议面必须逐字取自 apply_anti_censorship_options 里那句 matches!，多一个是假控件、少一个是漏检'
    ).toEqual([...RUST_MUX_PROTOCOLS].sort());
  });

  it('三个传输结构体的 owners == Rust `TRANSPORT_CAPABLE`（精确相等，多一个是必炸核的假控件）', () => {
    // `http` 协议是**唯一**例外，且方向单一：它不走 `generate_transport_config`，而是
    // `Protocol::Http` 分支把 `http_settings` 的 path/headers 直接写到出站**顶层**
    // （内核 http 出站 schema 有这两键、没有 `transport`）。故它只在 HttpSettings 一侧豁免。
    for (const struct of ['WebSocketSettings', 'GrpcSettings', 'HttpSettings']) {
      const expected = [...RUST_TRANSPORT_CAPABLE];
      if (struct === 'HttpSettings') expected.push('http');
      expect(
        [...owners(struct)].sort(),
        `STRUCT_OWNERS.${struct} 必须逐字取自 builder/outbound.rs 的 TRANSPORT_CAPABLE 白名单：\n` +
          `  多一个 = 该协议的传输参数下发后内核 decode 阶段 FATAL（整份配置起不来，不止这个节点）；\n` +
          `  少一个 = 用户在那个协议上填了传输参数却被静默丢弃。`
      ).toEqual(expected.sort());
    }
  });

  it('Rust `is_quic_managed_tls` 的三个协议：归属 TlsSettings，且 engine/fingerprint 必须是带依据的豁免', () => {
    // 2026-08-11：hysteria(v1) 并入 —— 三者在上游走同一个 tls.NewClient，TLS 同由 QUIC 栈接管。
    expect(RUST_QUIC_MANAGED.length, 'is_quic_managed_tls 解析出的协议数变了 —— 先确认 Rust 侧改了什么').toBe(
      3
    );
    for (const p of RUST_QUIC_MANAGED) {
      expect(owners('TlsSettings'), `${p} 的 TLS 由 QUIC 自管，但它连 TlsSettings 的 owner 都不是`).toContain(p);
      for (const k of ['engine', 'fingerprint']) {
        expect(
          Object.keys(NODE_EXEMPT[pairKey('TlsSettings', p)] ?? {}),
          `Rust 对 QUIC 协议 ${p} 不下发 tls.${k}（is_quic_managed_tls 前置门），本门必须把它记成带依据的豁免`
        ).toContain(k);
      }
    }
  });

  it('Rust spoof 协议门排除的协议：spoofSni / spoofMethod 必须是带依据的豁免', () => {
    for (const p of RUST_NO_SPOOF) {
      if (!owners('TlsSettings').includes(p as NodeProto)) continue;
      for (const k of ['spoofSni', 'spoofMethod']) {
        expect(
          Object.keys(NODE_EXEMPT[pairKey('TlsSettings', p)] ?? {}),
          `tls_spoof.rs 的 is_tls_spoof_supported_protocol 排除了 ${p}，本门必须把 ${k} 记成带依据的豁免`
        ).toContain(k);
      }
    }
  });

  it('Rust fragment 不支持名单：fragment 必须是带依据的豁免', () => {
    for (const p of RUST_NO_FRAGMENT) {
      if (!owners('TlsSettings').includes(p as NodeProto)) continue;
      expect(
        Object.keys(NODE_EXEMPT[pairKey('TlsSettings', p)] ?? {}),
        `fragment_unsupported 含 ${p}，本门必须把 fragment 记成带依据的豁免`
      ).toContain('fragment');
    }
  });

  it('ND_SPEC 里给了安全层开关的协议必须归属 TlsSettings；给了 reality 档的必须归属 RealitySettings', () => {
    for (const [proto] of PROTO_OPTIONS) {
      const fields = [...ND_SPEC[proto].cred, ...ND_SPEC[proto].adv];
      const sec = fields.find((f) => f.k === 'sec');
      const hasTlsSwitch = sec !== undefined || fields.some((f) => f.k === 'tls');
      if (hasTlsSwitch) {
        expect(owners('TlsSettings'), `${proto} 的表单有安全层开关，却不是 TlsSettings 的 owner`).toContain(
          proto
        );
      }
      if (sec !== undefined && sec.t === 'select' && sec.options.some(([v]) => v === 'reality')) {
        expect(owners('RealitySettings'), `${proto} 的安全层有 reality 档，却不是 RealitySettings 的 owner`).toContain(
          proto
        );
      }
    }
  });

  it('ND_SPEC 的传输下拉给了 ws / grpc / http 档的协议必须分别归属三个传输结构体', () => {
    const wants: Record<string, string> = { ws: 'WebSocketSettings', grpc: 'GrpcSettings', http: 'HttpSettings' };
    for (const [proto] of PROTO_OPTIONS) {
      const net = [...ND_SPEC[proto].cred, ...ND_SPEC[proto].adv].find((f) => f.k === 'net');
      if (net === undefined || net.t !== 'select') continue;
      for (const [value] of net.options) {
        const struct = wants[value];
        if (!struct) continue;
        expect(owners(struct), `${proto} 的传输下拉有 "${value}" 档，却不是 ${struct} 的 owner`).toContain(proto);
      }
    }
  });
});

describe('锁 6：债务表 / 豁免表的反向锁（两张表都不许变成永久盲区）', () => {
  const allRows = (): string[] => [...Object.keys(PORT_DEBT), ...Object.keys(NODE_EXEMPT)];
  const OWNED = new Set(OWNER_PAIRS.map(([s, p]) => pairKey(s, p)));

  it('两表的键都是归属表里真实存在的 `结构体::协议`', () => {
    for (const row of allRows()) {
      expect(OWNED.has(row), `${row} 不在 STRUCT_OWNERS 的 (结构体, 协议) 组合里 —— 死行，删掉它`).toBe(true);
    }
  });

  it('两表都不许有空行（还清了 / 不再豁免就把行删掉）', () => {
    for (const [row, keys] of Object.entries(PORT_DEBT)) {
      expect(keys.length, `PORT_DEBT["${row}"] 是空的 —— 债已还请删行，别留一行永久的零`).toBeGreaterThan(0);
    }
    for (const [row, table] of Object.entries(NODE_EXEMPT)) {
      expect(Object.keys(table).length, `NODE_EXEMPT["${row}"] 是空的 —— 删行`).toBeGreaterThan(0);
    }
  });

  it('两表登记的键必须是该结构体真实存在的 JSON 键', () => {
    for (const row of allRows()) {
      const struct = row.split('::')[0];
      const keys = new Set(rustJsonKeys(RUST_PROTOCOL_SETTINGS, struct));
      for (const k of [...(PORT_DEBT[row] ?? []), ...Object.keys(NODE_EXEMPT[row] ?? {})]) {
        expect(keys.has(k), `${row} 里的 "${k}" 在 Rust 结构体上不存在 —— 改名后的残留，删掉它`).toBe(true);
      }
    }
  });

  it('同一个键不许同时进两张表（「有意排除」与「还没做」是互斥的定性）', () => {
    for (const row of Object.keys(NODE_EXEMPT)) {
      for (const k of Object.keys(NODE_EXEMPT[row])) {
        expect(PORT_DEBT[row] ?? [], `${row}.${k} 同时被记成豁免和债务 —— 先决定它到底是哪一类`).not.toContain(
          k
        );
      }
    }
  });

  it('每条豁免带非空理由 + 至少一条**指得到真代码行**的依据', () => {
    for (const [row, table] of Object.entries(NODE_EXEMPT)) {
      for (const [k, ex] of Object.entries(table)) {
        expect(ex.why.trim().length, `NODE_EXEMPT["${row}"].${k} 没写理由`).toBeGreaterThan(20);
        expect(
          ex.cite.length,
          `NODE_EXEMPT["${row}"].${k} 没给代码行依据 —— 指不出一行代码的「有意排除」只能进债务表`
        ).toBeGreaterThan(0);
        for (const c of ex.cite) verifyCite(`NODE_EXEMPT["${row}"].${k}`, c);
      }
    }
  });
});
