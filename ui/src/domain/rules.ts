/**
 * 自定义规则（Rule）的 UI 侧逻辑：类型常量、分组、逐类型值校验、端口解析。
 * config 落盘的权威校验在 Rust（user_config/rule.rs），本文件是渲染端 rule-dialog 的即时反馈层。
 *
 * **旧数据迁移不在这里**：`DomainRule → Rule` 的迁移唯一真值源是 Rust `crates/store/src/migrate.rs`
 * （`migrate_custom_rules` / `is_legacy_domain_rule` / `migrate_legacy_domain_rule`），在 `validateConfig`
 * 载盘时执行。本文件曾持有同构的第二份 TS 实现，全仓零调用点（迁移早已下沉到 Rust）—— 留着只会
 * 在两份判据间漂移，已删。
 */
import type { Rule, RuleType, RuleCondition } from '../contracts/types';
import { isValidMacAddress, isValidSourceHostname, isSourceDeviceMatchSupported } from './neighbor';

export const RULE_TYPE_IDS: RuleType[] = [
  'domain',
  'domainSuffix',
  'domainKeyword',
  'domainRegex',
  'ipCidr',
  'sourceIpCidr',
  'port',
  'sourcePort',
  'sourceMac',
  'sourceHostname',
  'processName',
  'processPath',
  'geosite',
  'geoip',
  'ruleSet',
];

// device = 按源设备识别（MAC / 主机名，sing-box 1.14 局域网网关场景，仅 Linux/macOS）。
export type RuleCategory = 'domain' | 'network' | 'device' | 'process' | 'ruleset';

/** 分类在「匹配类型」下拉里的出现顺序（分组本体来自描述符的 `category`，此处仅定序）。 */
export const RULE_CATEGORY_ORDER: readonly RuleCategory[] = [
  'domain',
  'network',
  'device',
  'process',
  'ruleset',
];

/** 分类名的 i18n 键（值在五个 locale 的 `rules.cat.*`）。 */
export const ruleCategoryLabelKey = (cat: RuleCategory): string => `rules.cat.${cat}`;

// ─────────────────────────────────────────────────────────────────────────────
// 15 份类型描述符 —— 规则弹窗的**唯一**物料源
//
// 反补丁的判据是「新增第 16 个类型时要改什么」：只加一份描述符 ⇒ 成立；要动弹窗 JSX ⇒ 不成立。
// 此前元数据是分裂的：分类/平台/校验在本文件，而**显示名 / hint / placeholder / 分类顺序 /
// 分类标签 / 进程类判定 / 可测试性**七项散在 `RuleDialog.tsx` 里（`RULE_TYPE_META`、`CAT_ORDER`、
// `CAT_LABEL`、`PROCESS_TYPES`、`DOMAIN_TESTABLE`/`IP_TESTABLE`、`computeTestMatch` 里的逐类型
// 分支）—— 加第 16 个类型要改七处，其中六处在 JSX 文件里。全部收拢进本表后，弹窗只按
// `source.kind` / `source.pool` 这类**结构字段**分派，源码里一个 `RuleType` 字面量都不再出现
// （`rules.test.ts` 的「弹窗不得出现 RuleType 字面量」那道门钉住这件事）。
// ─────────────────────────────────────────────────────────────────────────────

/**
 * 候选池的分组轴。**目前只有一条**：来源（随包内置 / 已下载外置）。
 *
 * 进程刻意**没有**分组轴（陈先生 2026-07-30 裁定：平铺，只靠搜索）。此前唯一想得到的
 * 「系统/用户」维度在两个平台上都是坏的 —— 判据是纯前端路径前缀启发式（`ProcPickDialog`
 * 的 `PROC_SYS_RE`），而契约 `SystemProcessInfo` 不带 system 位：Windows 的 `tasklist`
 * 不给路径 ⇒ `path` 恒 `None` ⇒ 一个都隐不掉；Linux 内核线程无 `exe` 回落 `comm`，本机实测
 * 356 个名字里 272 个无 path。按坏判据分组比不分组更糟（用户以为筛过了）。
 */
export type GroupAxis = 'origin';

/**
 * 值的来源 —— 只有两档，判据是「有没有真实候选源」而不是「想不想给个列表」。
 *
 * 给零候选源的类型造勾选清单就是造**假清单**，是补丁式思考的另一种形态：域名四类是天然无限集；
 * IP/端口/MAC/主机名四族查过 command 全集确认无枚举源（无网卡枚举、无常用端口表、无
 * ARP/邻居发现/DHCP 租约读取）。故 10 个类型恒 `free`。
 */
export type ValueSource =
  | { readonly kind: 'free' }
  | {
      readonly kind: 'pool';
      /** 候选源。`geoTag` = `api.ruleResources.list()`；`process` = `api.system.listProcesses()`。 */
      readonly pool: 'geoTag' | 'process';
      /**
       * 池项 → 条件值的**寻址**方式。同一个 geoTag 池被三个类型用，差别只在这里：
       *  - `bare`：值 = 池项 tag 去掉 `<类型 id>-` 前缀（`geosite-youtube` → `youtube`），
       *    候选面亦按该前缀过滤。生成端 `builder/custom_rules.rs:119-134` 正是把值回拼成
       *    `<类型 id>-<值>`，故前缀由**描述符 id 自己**决定，第 16 个 geo 类型无需改代码。
       *  - `res-id`：值 = `res:<资源 id>`（生成端 `custom_rules.rs:139` 只认这个前缀，
       *    其余一律 warn + 跳过 = fail-closed 静默不生效）。
       *  - `proc-name` / `proc-path`：值 = `SystemProcessInfo` 的 name / path。
       *    `proc-path` 会把**无 path 的进程整条剔出候选面**（Windows 的 tasklist 恒无 path）——
       *    回落成 name 会产出一个过不了 `validateRuleValue('processPath', …)` 的值。
       */
      readonly addressing: 'bare' | 'res-id' | 'proc-name' | 'proc-path';
      /** 有轴 ⇒ 渲染分类切换；`null` ⇒ 不渲染（不是渲染一个禁用的假控件）。 */
      readonly groupBy: GroupAxis | null;
      /** 参与检索的池项字段名（投影由池提供者按名取，见 `rule-cond.ts` 的 `poolOptions`）。 */
      readonly searchFields: readonly string[];
      /** 量级 —— 决定要不要滚动分批。`large` 的实测依据见各条注释。 */
      readonly scale: 'small' | 'large';
      /**
       * 是否保留手填文本区。**当前 5 个池类型恒 `true`，但三条理由各不相同**：
       *  - `ruleSet`：高级用户手填 `res:<id>`（placeholder 就是这个形态）；
       *  - `geosite`/`geoip`：要引一个**上游有、本地还没下载**的 tag —— 先写下去，
       *    去规则资源页下好即刻生效（候选面只列本地已有的）；
       *  - `processName`/`processPath`：picker 只列**当前在跑**的进程，给未运行的应用建规则必须手填。
       *
       * 三条理由都可能单独消失（比如某个未来类型的候选面真的封闭），那时这个字段才有第二个取值。
       * 现在留着不是「预留扩展点」，是**记录这三条理由各自独立**：删掉字段等于宣称「池类型永远可手填」，
       * 而那句话没有任何依据。
       */
      readonly allowFreeInput: boolean;
    };

/** 「测试匹配」的被测输入（原串 + 小写归一，两者都要：正则匹配用原串，域名/IP 比较用小写）。 */
export interface RuleTestProbe {
  readonly raw: string;
  readonly lower: string;
}

/**
 * 该类型在「测试匹配」里的语义。`axis` 决定**适用性**（域名轴的类型对 IP 输入不适用，反之亦然），
 * `match` 是逐类型的命中判据。`null` = 无法用域名/IP 测（端口 / 进程 / MAC / 主机名 / 规则集）。
 *
 * 判据本身是**客户端启发式**（对齐原型 ruleTest :5090），权威匹配在内核；它只提供即时反馈。
 */
export interface RuleTypeTest {
  readonly axis: 'domain' | 'ip';
  /** `tokens` 已 trim + 小写。 */
  match(tokens: readonly string[], probe: RuleTestProbe): boolean;
}

export interface RuleTypeDescriptor {
  readonly id: RuleType;
  readonly category: RuleCategory;
  readonly source: ValueSource;
  readonly test: RuleTypeTest | null;
}

/** 类型名 / 逐类型填写提示 / placeholder 的 i18n 键（locale 里 `rules.types.*` 与 `rules.typeHints.*` 两张现成表）。 */
export const ruleTypeNameKey = (id: RuleType): string => `rules.types.${id}.name`;
export const ruleTypeHintKey = (id: RuleType): string => `rules.typeHints.${id}`;
export const ruleTypePlaceholderKey = (id: RuleType): string => `rules.types.${id}.placeholder`;

/** 域名轴的常用判据：逐 token 比较（大小写已由调用方归一）。 */
const eqDomain = (tokens: readonly string[], p: RuleTestProbe) => tokens.some((x) => p.lower === x);
const substrDomain = (tokens: readonly string[], p: RuleTestProbe) =>
  tokens.some((x) => p.lower.includes(x));
/** IP 轴的前缀启发式（对齐原型：只比前两段，不做真正的掩码运算）。 */
const ipPrefix = (tokens: readonly string[], p: RuleTestProbe) =>
  tokens.some((x) => {
    const pre = x.split('/')[0].split('.').slice(0, 2).join('.');
    return !!pre && p.lower.startsWith(pre);
  });

const FREE: ValueSource = { kind: 'free' };

/** geo tag 池的公共形状（`geosite`/`geoip` 用，两者只差 id 前缀 —— 由 `addressing:'bare'` 从 id 推）。 */
const geoTagBare = (): ValueSource => ({
  kind: 'pool',
  pool: 'geoTag',
  addressing: 'bare',
  groupBy: 'origin',
  // 池项只有一个可读字段（裸 tag），name 与 id 在这里是同一个串。
  searchFields: ['tag'],
  // 随包 geosite 21 / geoip 7，加已下载的几条。既有 `.tag-pick` 在 96–110px 容器里一次性
  // 渲染 21 个 chip 已验证够用（AppAddDialog:676），不分批。
  scale: 'small',
  allowFreeInput: true,
});

/** 进程池的公共形状（`processName`/`processPath` 只差取哪个字段当值）。 */
const procPool = (addressing: 'proc-name' | 'proc-path'): ValueSource => ({
  kind: 'pool',
  pool: 'process',
  addressing,
  groupBy: null, // 见 GroupAxis 头注：唯一想得到的轴在两个平台上都是坏的
  searchFields: ['name', 'path'],
  scale: 'large', // 本机实测 356 条
  allowFreeInput: true,
});

/**
 * 15 份描述符。**加第 16 个类型只改这张表**（外加 `RULE_TYPE_IDS` 与契约里的 `RuleType` 联合）。
 * 展示名、提示与占位符只由 `ruleType*Key` 指向 locale，不在描述符里复制第二份文案。
 */
export const RULE_TYPES: Record<RuleType, RuleTypeDescriptor> = {
  domain: {
    id: 'domain',
    category: 'domain',
    source: FREE,
    test: { axis: 'domain', match: eqDomain },
  },
  domainSuffix: {
    id: 'domainSuffix',
    category: 'domain',
    source: FREE,
    test: {
      axis: 'domain',
      match: (tokens, p) => tokens.some((x) => p.lower === x || p.lower.endsWith('.' + x)),
    },
  },
  domainKeyword: {
    id: 'domainKeyword',
    category: 'domain',
    source: FREE,
    test: { axis: 'domain', match: substrDomain },
  },
  domainRegex: {
    id: 'domainRegex',
    category: 'domain',
    source: FREE,
    test: {
      axis: 'domain',
      // 用**原串**而非小写：正则里的大小写是作者写的意图，归一会改写它（`[A-Z]` 会失效）。
      match: (tokens, p) =>
        tokens.some((x) => {
          try {
            return new RegExp(x, 'i').test(p.raw);
          } catch {
            return false;
          }
        }),
    },
  },
  ipCidr: {
    id: 'ipCidr',
    category: 'network',
    source: FREE,
    test: { axis: 'ip', match: ipPrefix },
  },
  sourceIpCidr: {
    id: 'sourceIpCidr',
    category: 'network',
    source: FREE,
    test: { axis: 'ip', match: ipPrefix },
  },
  port: {
    id: 'port',
    category: 'network',
    source: FREE,
    test: null,
  },
  sourcePort: {
    id: 'sourcePort',
    category: 'network',
    source: FREE,
    test: null,
  },
  sourceMac: {
    id: 'sourceMac',
    category: 'device',
    source: FREE,
    test: null,
  },
  sourceHostname: {
    id: 'sourceHostname',
    category: 'device',
    source: FREE,
    test: null,
  },
  processName: {
    id: 'processName',
    category: 'process',
    source: procPool('proc-name'),
    test: null,
  },
  processPath: {
    id: 'processPath',
    category: 'process',
    source: procPool('proc-path'),
    test: null,
  },
  geosite: {
    id: 'geosite',
    category: 'ruleset',
    source: geoTagBare(),
    // 启发式：geosite 标签背后是一张域名表，客户端拿不到 ⇒ 退化成「标签名是域名子串」。
    test: { axis: 'domain', match: substrDomain },
  },
  geoip: {
    id: 'geoip',
    category: 'ruleset',
    source: geoTagBare(),
    // 启发式：假定被测 IP 落在地区标签内（客户端无 GeoIP 库）。
    test: { axis: 'ip', match: () => true },
  },
  ruleSet: {
    id: 'ruleSet',
    category: 'ruleset',
    // 生成端 `custom_rules.rs:139` 只认 `res:` 前缀，其余一律 warn + 跳过（fail-closed）——
    // 照裸 tag 填出来的规则会**静默不生效**，只在日志里留一行 warn（2026-07-30 真机反馈）。
    source: {
      kind: 'pool',
      pool: 'geoTag',
      addressing: 'res-id',
      groupBy: 'origin',
      // 随包行 name 是裸 tag（`geosite-cn`），下载行 name 是 catalog 名（`youtube`）而只有 id 含
      // `geosite-` 前缀 —— 只匹配一个会让「搜 geosite」在两组里表现不一致（同 ruleSetPickMatches）。
      searchFields: ['name', 'id'],
      scale: 'large', // 内置 28 + 外置 2000+
      allowFreeInput: true,
    },
    test: null,
  },
};

/**
 * 新建条件的默认类型。**必须在本文件而不是弹窗里**：它是「15 个类型里先给哪一个」这条产品判断，
 * 与描述符表同源；留在 JSX 里就是加第 16 个类型时要动的第二处。
 */
export const DEFAULT_RULE_TYPE: RuleType = 'domain';

/**
 * 「为一个**观测到的主机名**建规则」时用哪个类型 —— 拓扑/连接页右键菜单那一族入口的单一真值。
 *
 * 为什么必须收成一个常量：同一个菜单此前产出**两种**规则类型 —— 「代理 / 直连」直写腿写
 * `domainSuffix`（对齐原型 dnodeMenu 的「为其加规则」语义），而「加入自定义规则」弹窗腿预填的是
 * `DEFAULT_RULE_TYPE`（= `domain`，精确匹配）。同一个菜单、同一次点击意图，两条腿类型不同。
 *
 * 取 `domainSuffix` 而不是 `domain` 的判据：
 *  1. 弹窗腿那个 `domain` **不是一次产品决定**，是「新建空白条件默认给哪个类型」这个常量顺着
 *     `presetDomain` 的路径漏出来的；直写腿的 `domainSuffix` 才是照着原型语义显式选的。
 *  2. 主机名是**完整**主机名（`isRuleableHost` 已排除聚合 sentinel），故 `domainSuffix` 相对
 *     `domain` 只多命中它自己的子域，扩宽面很窄；反过来把直写腿收成 `domain` 则要推翻原型语义。
 *  3. 弹窗腿是**可见可改**的（用户提交前看得到类型下拉），直写腿不是 —— 让可见的那条去迁就
 *     不可见的那条，代价最小。
 *
 * 与 `DEFAULT_RULE_TYPE` 分开而不复用：那个回答「空白条件先给哪一个」，这个回答「拿到一个主机名
 * 该怎么匹配」，两个问题的答案没有理由永远相同。
 */
export const PRESET_HOST_RULE_TYPE: RuleType = 'domainSuffix';

/** 类型 → 分类（**派生自描述符表**，勿另立第二张表）。 */
export const RULE_TYPE_CATEGORY: Record<RuleType, RuleCategory> = Object.fromEntries(
  RULE_TYPE_IDS.map((id) => [id, RULE_TYPES[id].category])
) as Record<RuleType, RuleCategory>;

export const BYPASS_FAKEIP_TYPES: RuleType[] = ['domain', 'domainSuffix', 'domainKeyword'];

/**
 * 规则类型在指定平台是否受内核支持：device 类别（source_mac_address / source_hostname，sing-box 1.14 邻居
 * 解析）仅 Linux/macOS（真机 check 实证，见 shared/neighbor），其余类别全平台。渲染层据此隐藏不支持的类型
 * 选项、主进程据此 fail-closed 丢弃条件，单一真值（避免 UI 与生成层对平台支持面漂移）。
 */
export function isRuleTypePlatformSupported(
  type: RuleType,
  platform: NodeJS.Platform | string | undefined
): boolean {
  return RULE_TYPE_CATEGORY[type] !== 'device' || isSourceDeviceMatchSupported(platform);
}

/**
 * 下一个可新增的条件类型（未被使用 ∧ 当前平台支持），无则 undefined。rule-dialog 的「添加条件」按钮显隐
 * 与 addCondition 取值共用本函数，保证「按钮显示 ⟺ 点击有结果」（口径单一来源，防二者漂移）。
 */
export function findAddableRuleType(
  usedTypes: ReadonlySet<RuleType>,
  platform: NodeJS.Platform | string | undefined
): RuleType | undefined {
  return RULE_TYPE_IDS.find(
    (tp) => !usedTypes.has(tp) && isRuleTypePlatformSupported(tp, platform)
  );
}

// 域名：标签 1-63 字符、可含通配 *. 前缀（domainSuffix 容忍）；不强校验 TLD（geosite 标签另有规则）
const DOMAIN_RE =
  /^(\*\.)?([a-zA-Z0-9_](?:[a-zA-Z0-9_-]{0,61}[a-zA-Z0-9_])?\.)*[a-zA-Z0-9_-]{1,63}$/;
// geo 标签：小写字母数字 + ! - _（如 geolocation-!cn、category-ads-all）
// geo 标签大小写不敏感（用户输 CN 也接受）；生成期统一 lowercase（远程 .srs 文件名为小写）
const GEO_TAG_RE = /^[a-z0-9!_-]+$/i;
const PORT_RE = /^\d{1,5}(-\d{1,5})?$/;

/** 严格 IPv4：恰 4 段、每段 0-255、禁前导零（sing-box netip 拒 `010.0.0.1`）。 */
function isStrictIpv4(addr: string): boolean {
  const octets = addr.split('.');
  return (
    octets.length === 4 && octets.every((o) => /^(0|[1-9]\d{0,2})$/.test(o) && Number(o) <= 255)
  );
}

/**
 * 结构化 IPv6 校验：处理 `::` 压缩（至多一次）、每段 1-4 hex、末段可为内嵌 IPv4。匹配 sing-box netip.ParsePrefix——
 * 拒 `12345::1`（段>4 位）、`1:2:3:4:5:6:7:8:9`（>8 段）、`::::`/`dead::beef::1`（多个 ::）、`fe80:`、`:` 等会 FATAL 的形态。
 */
function isStrictIpv6(addr: string): boolean {
  const parts = addr.split('::');
  if (parts.length > 2) return false; // 多于一个 ::
  const hasCompression = parts.length === 2;
  const left = parts[0] === '' ? [] : parts[0].split(':');
  const right = hasCompression ? (parts[1] === '' ? [] : parts[1].split(':')) : [];
  const groups = [...left, ...right];
  let ipv4Suffix = false;
  for (let i = 0; i < groups.length; i++) {
    const g = groups[i];
    if (i === groups.length - 1 && g.includes('.')) {
      if (!isStrictIpv4(g)) return false; // 末段内嵌 IPv4（如 ::ffff:192.168.1.1）
      ipv4Suffix = true;
    } else if (!/^[0-9a-fA-F]{1,4}$/.test(g)) {
      return false;
    }
  }
  const count = groups.length + (ipv4Suffix ? 1 : 0); // 内嵌 IPv4 占 2 个 16-bit 段
  // 无 :: 须恰 8 段；有 :: 时它代表 ≥1 个零段，故显式段须 ≤7。
  return hasCompression ? count <= 7 : count === 8;
}

/**
 * IPv4/IPv6（可选 CIDR 掩码）严格校验。**含范围+结构检查**：八位组 ≤255、禁前导零、掩码 v4≤32 / v6≤128、
 * IPv6 结构合法。旧的纯形状正则会放过 `10.0.0.0/40`、`300.300.300.300`、`abc`、`12345::1`、`010.0.0.1` 等，
 * 这些值会让 sing-box 启动 FATAL（且落在 endpoints[]/route.rules[] 时启动前 gate 无法按 outbounds 索引剪除→整体启动失败）。
 * ConfigManager 对 endpoint allowedIPs/routes/localAddress 用本函数 sanitize（丢弃非法项防 FATAL），customRules
 * 校验亦共用——单一真值，杜绝「校验通过却启动炸」。
 */
export function isValidIpCidr(value: string): boolean {
  const v = value.trim();
  if (!v) return false;
  const slash = v.indexOf('/');
  const addr = slash === -1 ? v : v.slice(0, slash);
  const maskStr = slash === -1 ? undefined : v.slice(slash + 1);
  const isV6 = addr.includes(':');
  if (maskStr !== undefined) {
    if (!/^\d{1,3}$/.test(maskStr)) return false;
    if (Number(maskStr) > (isV6 ? 128 : 32)) return false;
  }
  return isV6 ? isStrictIpv6(addr) : isStrictIpv4(addr);
}

function validPortToken(v: string): boolean {
  if (!PORT_RE.test(v)) return false;
  const parts = v.split('-').map((n) => parseInt(n, 10));
  return parts.every((n) => n >= 1 && n <= 65535) && (parts.length === 1 || parts[0] <= parts[1]);
}

/** 单条规则值是否合法（按类型）。空串一律非法。 */
export function validateRuleValue(type: RuleType, value: string): boolean {
  const v = value.trim();
  if (!v) return false;
  switch (type) {
    case 'domain':
    case 'domainSuffix':
    case 'domainKeyword':
      // keyword 允许任意非空子串，**但拒含冒号**：`domain_keyword` 是对域名做子串匹配，DNS 名里
      // 不可能出现 `:` ⇒ 含冒号的关键词恒不命中，是一条内核不报错、用户以为配好了的死配置。
      // 挡住 IPv6 字面量（裸写 `2001:db8::1` 与 URL 写法 `[2001:db8::1]` 都含冒号）。
      // 与 Rust 权威 `rule_validate.rs::validate_rule_value` 逐字同源，改一处必须改两处。
      // 不推广到 IPv4：`1.2.3.4` 能命中 `1.2.3.4.nip.io` 这类真实域名，拒掉是砍合法能力。
      // domain/suffix 走域名形状（DOMAIN_RE 无冒号字符类，本就拒）。
      return type === 'domainKeyword' ? !v.includes(':') : DOMAIN_RE.test(v);
    case 'domainRegex':
      // sing-box 用 Golang RE2：拒绝 RE2 不支持的 lookahead/lookbehind/反向引用（过 JS 校验却启动 FATAL）
      if (/\(\?[=!<]|\\[1-9]/.test(v)) return false;
      try {
        new RegExp(v);
        return true;
      } catch {
        return false;
      }
    case 'ipCidr':
    case 'sourceIpCidr':
      return isValidIpCidr(v);
    case 'port':
    case 'sourcePort':
      return validPortToken(v);
    case 'sourceMac':
      // EUI-48 MAC（冒号/连字符/Cisco 点分）；脏 MAC 会让 TUN 侧 check / 启动 FATAL。
      return isValidMacAddress(v);
    case 'sourceHostname':
      // DHCP 租约主机名形状（字母数字 + 连字符）。
      return isValidSourceHostname(v);
    case 'processName':
      return !v.includes('/') && !v.includes('\\');
    case 'processPath':
      return v.startsWith('/') || /^[A-Za-z]:[\\/]/.test(v);
    case 'geosite':
    case 'geoip':
      return GEO_TAG_RE.test(v);
    case 'ruleSet':
      // 本地资源引用 res:<id> 或 http(s) URL
      if (v.startsWith('res:')) return v.length > 4;
      return /^https?:\/\/.+/i.test(v);
    default:
      return false;
  }
}

/** 规则的条件列表（**唯一遍历入口**）：多条件取 conditions，否则退化为单条件 [{type,values}]。 */
export function ruleConditions(
  rule: Pick<Rule, 'type' | 'values' | 'conditions'>
): RuleCondition[] {
  return rule.conditions && rule.conditions.length > 0
    ? rule.conditions
    : [{ type: rule.type, values: rule.values }];
}

/** 一条规则里的全部 ipCidr 值（trim/去空，扫所有条件）。供「与组网 mesh 段重叠」提醒共用。 */
export function ruleIpCidrs(rule: Pick<Rule, 'type' | 'values' | 'conditions'>): string[] {
  return ruleConditions(rule)
    .filter((c) => c.type === 'ipCidr')
    .flatMap((c) => (Array.isArray(c.values) ? c.values : []))
    .map((v) => (typeof v === 'string' ? v.trim() : ''))
    .filter(Boolean);
}

/** 聚合校验一条规则：每个条件类型合法 + 至少一个合法值；combineMode 合法；镜像 type 合法（旁路写防御）。 */
export function validateRule(
  rule: Pick<Rule, 'type' | 'values' | 'conditions' | 'combineMode'>
): boolean {
  if (rule.combineMode !== undefined && rule.combineMode !== 'and' && rule.combineMode !== 'or') {
    return false;
  }
  // 镜像 type 必须合法：消费点/回滚兼容读 rule.type，非法镜像会让 ConfigManager 整条丢弃
  if (!RULE_TYPE_IDS.includes(rule.type)) return false;
  const conds = ruleConditions(rule);
  if (conds.length === 0) return false;
  return conds.every((c) => {
    if (!c || !RULE_TYPE_IDS.includes(c.type)) return false;
    // 非数组/非字符串值防御：旁路 config:save 可注入，避免 v.trim() 抛 TypeError 冒泡成内部错误
    if (!Array.isArray(c.values)) return false;
    const vals = c.values.filter((v) => typeof v === 'string' && v.trim());
    return vals.length > 0 && vals.every((v) => validateRuleValue(c.type, v));
  });
}

/** 端口值数组 → sing-box 的 port(单端口) 与 port_range("start:end") 两组。 */
export function parsePortValues(values: string[]): { ports: number[]; ranges: string[] } {
  const ports: number[] = [];
  const ranges: string[] = [];
  for (const raw of values) {
    const v = raw.trim();
    if (!validPortToken(v)) continue;
    if (v.includes('-')) {
      const [a, b] = v.split('-');
      ranges.push(`${parseInt(a, 10)}:${parseInt(b, 10)}`);
    } else {
      ports.push(parseInt(v, 10));
    }
  }
  return { ports, ranges };
}
