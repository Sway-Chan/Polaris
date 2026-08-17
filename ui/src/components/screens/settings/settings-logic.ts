/**
 * 设置屏纯逻辑 —— 抽出以便 node 环境 vitest 直测（本仓 vitest `environment:'node'`，无 jsdom/testing-library）。
 *
 * # 为什么抽出来
 *
 * 这些判定全是「UI 显示态 ↔ 后端消费口径」的对齐点，写错的后果不是样式歪，而是**UI 显示与后端实际行为
 * 分叉**（最恶劣：UI 显示「关」而后台在跑）。分叉只能靠断言锁死，而组件本身在 node 环境渲染不了 →
 * 把判定抽成纯函数，组件**直接消费**这里的导出（非并行复刻，否则测试是假绿；同 `SettingsDns.tsx` 的
 * `fakeIpTogglePatch` / `layout/pending-bar-logic.ts` 的既有约定）。
 *
 * # 「缺省为开」口径（`!== false`）
 *
 * 后端多处用 `Option<bool> == Some(false)` 判禁（如 `system_proxy_bypass.rs::effective_bypass_lan`），
 * 即「字段缺失 = 开」。UI 若写成 `!!config.x`，存量配置（无该键）会显示成「关」而后端按「开」跑 →
 * 必须统一走 `defaultOn`。
 */

/* ────────────────────────────────────────────────────────────────────────────
 * 通用：缺省为开的三态布尔
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 「缺省为开」语义：仅显式 `false` 判关，`undefined`/`null`（未设/旧配置）与 `true` 一律判开。
 * 与后端 `!= Some(false)` 严格同口径 —— 存量配置行为逐字节不变。
 */
export function defaultOn(value: boolean | undefined | null): boolean {
  return value !== false;
}

/* ────────────────────────────────────────────────────────────────────────────
 * #12 bypassLAN 绕过局域网总开关
 * ──────────────────────────────────────────────────────────────────────────── */

export interface BypassLanConfigLike {
  bypassLAN?: boolean;
}

export interface BypassLanState {
  /** 开关显示态。 */
  checked: boolean;
  /** 旁路清单编辑器是否渲染。关掉总开关后清单不生效，继续展示可编辑清单是误导 → 隐藏。 */
  showList: boolean;
}

/**
 * 绕过局域网总开关显示态。后端 `effective_bypass_lan`：`bypass_lan() == Some(false)` → 返空清单
 * （TUN route_exclude 与系统代理旁路两处都走它），故此处必须 `!== false` 而非 `!!`。
 */
export function bypassLanState(config: BypassLanConfigLike): BypassLanState {
  const on = defaultOn(config.bypassLAN);
  return { checked: on, showList: on };
}

/* ────────────────────────────────────────────────────────────────────────────
 * 本地代理端口 —— 与 Rust `local_proxy_port` 严格同口径
 * ──────────────────────────────────────────────────────────────────────────── */

/** 默认混合端口。对齐 Rust `config-engine::user_config::proxy_ports::DEFAULT_MIXED_PORT`。 */
export const DEFAULT_MIXED_PORT = 7890;

/** 默认 clash_api 控制端口。对齐 Rust `proxy_ports.rs:12 DEFAULT_CONTROL_PORT`。 */
export const DEFAULT_CONTROL_PORT = 9090;

export interface PortConfigLike {
  mixedPort?: number | null;
  httpPort?: number | null;
}

/**
 * 本地混合代理端口。**逐条对齐** `crates/config-engine/src/user_config/proxy_ports.rs:22-34`：
 * `mixedPort > 0` → 用之；否则 `httpPort > 0` → 用之；否则 `DEFAULT_MIXED_PORT`。
 *
 * 此前 UI 写的是 `config.mixedPort ?? 7890`（SettingsNetwork.tsx:46（改前）），与后端**不等价**，丢了两条
 * httpPort 回退路径：
 *   (a) `mixedPort = 0` —— `??` 只挡 null/undefined，UI 显 0 而后端已回退到 httpPort/7890；
 *   (b) `mixedPort` 未设 + `httpPort` 已设（旧配置迁移前的常见形态）—— UI 显 7890 而后端用 httpPort。
 * 后果不是显示歪，而是**用户照 UI 复制出的 `http_proxy=` 端口连不上代理**，且错得没有任何提示。
 */
export function localProxyPort(config: PortConfigLike): number {
  const mixed = config.mixedPort;
  if (typeof mixed === 'number' && mixed > 0) return mixed;
  const http = config.httpPort;
  if (typeof http === 'number' && http > 0) return http;
  return DEFAULT_MIXED_PORT;
}

export interface ControlPortConfigLike {
  controlPort?: number | null;
}

/**
 * clash_api 控制端口。**逐条对齐** `proxy_ports.rs:36-42 control_api_port`：`controlPort > 0` → 用之，
 * 否则 `DEFAULT_CONTROL_PORT`。此前 UI 写 `config.controlPort ?? 9090` —— `??` 挡不住 0，
 * 与 `localProxyPort` 当初那条缺陷同型（UI 显 0 而后端已回退 9090）。
 */
export function controlApiPort(config: ControlPortConfigLike): number {
  const p = config.controlPort;
  return typeof p === 'number' && p > 0 ? p : DEFAULT_CONTROL_PORT;
}

/**
 * 端口输入的下界。**取 1024 而非后端的 1**：
 *   · 上游 `network-settings.tsx:219-220` 就是这条（`portNum < 1024 || portNum > 65535` → 提示并回滚），
 *     且 Polaris 五语种早已随迁移带进对应文案键 `settings.advanced.localPortRange`（"端口须为 1024-65535"）——
 *     沿用即口径一致、零新增键；
 *   · 特权端口（<1024）在 Unix 下需 root 才能 bind。Polaris 的 sing-box 在系统代理模式下不提权 ⇒
 *     mixed inbound 绑 80/443 直接起核失败，而失败点在内核启动期、离用户输入很远。
 *
 * 注意后端 `crates/store/src/validate.rs:264-279 validate_port` 只挡 1..65535 —— 本函数**比后端严**，
 * 差集（1..1023）由 UI 拦下并给出文案。方向是安全的：UI 放行的后端必收，UI 拒绝的后端不会看到。
 */
export const MIN_LISTEN_PORT = 1024;

/** 端口上界（与后端 `validate_port` 同）。 */
export const MAX_LISTEN_PORT = 65535;

/**
 * 端口输入 → 落盘值。`null` = 非法（调用方标红且**不落盘**）。
 *
 * 空串 → `fallback`（「清空即回默认」，同 `SettingsDns` 两栏 DNS 的语义）。非纯数字（含负号 / 小数点 /
 * 空格）一律判非法而不做 `Number()` 宽松转换：`Number(' 80 ')` 会静默吃掉空白、`Number('7e3')` 会得 7000，
 * 两者都是「用户看到的字符串 ≠ 落盘的值」。
 */
export function normalizePortInput(raw: string, fallback: number): number | null {
  const v = raw.trim();
  if (!v) return fallback;
  if (!/^\d+$/.test(v)) return null;
  const n = Number(v);
  if (n < MIN_LISTEN_PORT || n > MAX_LISTEN_PORT) return null;
  return n;
}

/* ────────────────────────────────────────────────────────────────────────────
 * 终端代理环境变量 —— 按平台分支
 * ──────────────────────────────────────────────────────────────────────────── */

/** `<html data-os>` 的三个取值（AppShell.tsx 写入，权威源 tauri-plugin-os）。 */
export type ShellPlatform = 'mac' | 'win' | 'lin';

/**
 * 读 `<html data-os>` 得到当前平台。AppShell.tsx:78 已把权威平台（tauri-plugin-os）写在那里，
 * 这里读它即可，**不重复做 OS 嗅探**（同 RuleDialog.tsx:156 的既有做法）。
 *
 * 取不到（非 Tauri 宿主 / 属性尚未写入）→ `undefined`。**兜底方向由各消费者自行决定**，本函数不代劳：
 *   · `splitTerminalEnvByPlatform` / `showsHardwareAccelRow`：fail-open（宁可多显示一套命令 / 多留一个
 *     排障开关，也不把用户唯一能用的那套藏起来）；
 *   · `SettingsTun` 的局域网网关块：fail-closed（不渲染）—— 那里判定的是「内核层是否真支持」，
 *     错判 = 让用户填一份永不生效的清单（Windows 无邻居解析器、MAC 过滤发射即 FATAL）。
 *
 * 此前 `SettingsNetwork.tsx:44` / `SettingsDisplay.tsx:28` / `SettingsTun.tsx:45` 各有一份同实现，
 * 本函数是三份合一后的唯一副本。
 */
export function shellPlatformFromDataOs(): ShellPlatform | undefined {
  if (typeof document === 'undefined') return undefined;
  const os = document.documentElement.getAttribute('data-os');
  return os === 'mac' || os === 'win' || os === 'lin' ? os : undefined;
}

/** 命令分组 id —— 稳定标识，供测试与 React key 用（label 是展示文案，不该拿来当 key）。 */
export type TerminalEnvGroupId = 'unix' | 'win-cmd' | 'win-powershell';

export interface TerminalEnvGroup {
  id: TerminalEnvGroupId;
  /**
   * 分组标签。**刻意不走 i18n**：内容全是产品名 / shell 名（Windows、CMD、PowerShell、macOS、
   * Linux、Bash、Zsh），五语种写法一致，翻译只会引入伪造风险而不增信息。
   */
  label: string;
  /** 可直接粘进该 shell 的完整命令行。 */
  lines: string[];
}

/**
 * `no_proxy` 的固定取值。**刻意不取自 `config.bypassLANList`** —— 两者语义与语法都不等价：
 *   · `bypassLANList` 是**代理层**旁路清单，条目形如 `192.168.0.0/16`、`*.example.cn`，由 sing-box /
 *     系统代理各自解释；
 *   · `no_proxy` 是**各 CLI 库自行解析**的 shell 层约定，没有统一规范：curl 不认 CIDR、不认 `*` 通配，
 *     Go `httpproxy` 认 CIDR 但同样不认 `*`，Python `urllib` 只做后缀匹配。
 * 把用户的旁路清单直接倒进 `no_proxy`，多数条目会被静默丢弃或整串解析失败 —— 那是「看起来跟着设置走、
 * 实际不生效」的假联动，比明确写死更糟。故保持独立，并在此说明理由。
 */
const NO_PROXY_VALUE = 'localhost,127.0.0.1,::1';

/** 全部平台的命令分组（顺序固定：Unix → CMD → PowerShell）。 */
export function terminalEnvGroups(port: number): TerminalEnvGroup[] {
  const http = `http://127.0.0.1:${port}`;
  const socks = `socks5://127.0.0.1:${port}`;
  return [
    {
      id: 'unix',
      label: 'macOS / Linux (Bash · Zsh)',
      lines: [
        `export http_proxy=${http}`,
        `export https_proxy=${http}`,
        `export all_proxy=${socks}`,
        `export no_proxy=${NO_PROXY_VALUE}`,
      ],
    },
    {
      id: 'win-cmd',
      label: 'Windows (CMD)',
      lines: [
        `set http_proxy=${http}`,
        `set https_proxy=${http}`,
        `set all_proxy=${socks}`,
        `set no_proxy=${NO_PROXY_VALUE}`,
      ],
    },
    {
      id: 'win-powershell',
      label: 'Windows (PowerShell)',
      lines: [
        `$env:http_proxy="${http}"`,
        `$env:https_proxy="${http}"`,
        `$env:all_proxy="${socks}"`,
        `$env:no_proxy="${NO_PROXY_VALUE}"`,
      ],
    },
  ];
}

/** 哪些分组属于该平台（Windows 下 CMD 与 PowerShell **都要给**：同一台机器上两种 shell 都常用）。 */
const PLATFORM_GROUPS: Record<ShellPlatform, readonly TerminalEnvGroupId[]> = {
  mac: ['unix'],
  lin: ['unix'],
  win: ['win-cmd', 'win-powershell'],
};

export interface TerminalEnvSplit {
  /** 当前平台可直接用的分组，常驻展示。 */
  current: TerminalEnvGroup[];
  /** 其余平台的分组，折叠在「展开看全部」里。 */
  others: TerminalEnvGroup[];
}

/**
 * 按平台切成「当前平台 / 其余平台」两段（用户拍板的形态：当前平台优先 + 其余可展开）。
 *
 * `platform` 取不到（非 Tauri 宿主 / `<html data-os>` 未写入）时**全部归 current**：宁可多显示，
 * 也不能把用户唯一能用的那套藏进折叠里。注意这与 `RuleDialog` 的 fail-closed 取向不同 —— 那里
 * 判定的是「能力是否可用」（错判 = 假可用），这里判定的是「先显示哪一套」（错判只是多显示）。
 */
export function splitTerminalEnvByPlatform(
  platform: ShellPlatform | undefined,
  port: number,
): TerminalEnvSplit {
  const groups = terminalEnvGroups(port);
  if (platform === undefined) return { current: groups, others: [] };
  const ids = PLATFORM_GROUPS[platform];
  return {
    current: groups.filter((g) => ids.includes(g.id)),
    others: groups.filter((g) => !ids.includes(g.id)),
  };
}

/**
 * 「永久生效」提示（`tipPermanent`：写进 `~/.bashrc` / `~/.zshrc`）是否适用。
 * Windows 下这条是错的（CMD 要 `setx`、PowerShell 要 profile），显示它就是在把本次要修的
 * 「给 Windows 用户看 Unix 命令」缺陷换个地方复发。
 */
export function showsUnixPersistenceTip(platform: ShellPlatform | undefined): boolean {
  return platform === 'mac' || platform === 'lin' || platform === undefined;
}

/* ────────────────────────────────────────────────────────────────────────────
 * 图形兼容块 —— 按平台的行显隐与文案（组件层门控，取代原 CSS `:root[data-os]` 隐藏）
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 硬件加速行是否渲染。**mac 一律不渲染**：macOS(WKWebView) 没有受支持的关 GPU 途径
 * （Linux 有 `WEBKIT_DISABLE_DMABUF_RENDERER`、Windows 有 `WEBVIEW2_…--disable-gpu`，mac 无等价物，
 * 见 contracts/types.ts:404-411）→ 在 mac 是 no-op 死开关，用户 2026-07-22 要求去掉、不留。
 * Win/Linux 保留（那两平台真能关掉 GPU，是有用的白屏/花屏排障逃生门）。
 *
 * 组件层判定比原 CSS `:root[data-os="mac"] #set-hwaccel{display:none}` 可靠：render 时按已落定的
 * data-os 直接决定是否入树，不吃「data-os 由 post-paint effect 写入」的首帧/时机窗口。
 * platform 取不到（非 Tauri 预览）→ 显示：宁可多显示一个排障开关，也不误藏 Win/Linux 的逃生门。
 */
export function showsHardwareAccelRow(platform: ShellPlatform | undefined): boolean {
  return platform !== 'mac';
}

/**
 * 窗口特效说明的 i18n 键 —— 按平台区分：mac 只讲毛玻璃、win 只讲 Mica（此前混写一句让每个平台都读到
 * 一半与自己无关的文案）。该行在 Linux 由 CSS 隐藏（WebKitGTK 无等价物）、只在 mac/win 显示，故只需
 * 两版；platform 取不到（非 Tauri 预览）默认走 mac 版。
 */
export function windowEffectsDescKey(platform: ShellPlatform | undefined): string {
  return platform === 'win'
    ? 'settings.general.windowEffectsDescWin'
    : 'settings.general.windowEffectsDescMac';
}

/**
 * 语言说明的 i18n 键 —— **只有 macOS 需要额外一句**。
 *
 * 界面语言本身三平台都是即时生效（i18next 直接切）。但 macOS 上还有一层「进程有效本地化」：
 * NSOpenPanel 等 AppKit 自绘的原生 UI 取的是它，而它在进程启动时就定死了。Polaris 会把
 * 应用内的选择写进本应用的 `AppleLanguages`（`src-tauri/src/app_language.rs`），
 * 那条写入**下次启动**才被 AppKit 读到 —— 故 mac 版文案必须把「原生对话框重启后跟随」说出来。
 *
 * Linux（WebKitGTK 读环境 locale）与 Windows（WebView2 / Win32 通用对话框读系统区域设置）
 * 不经这层协商，用原来那句「即时生效」就是准确的，不该给它们看一条永远用不上的重启提示。
 *
 * platform 取不到（非 Tauri 预览）→ 走通用版：宁可少说一句 mac 专属限制，
 * 也不要在网页预览里承诺一个不存在的重启行为。
 */
export function languageDescKey(platform: ShellPlatform | undefined): string {
  return platform === 'mac'
    ? 'settings.display.languageDescMac'
    : 'settings.display.languageDesc';
}

/* ────────────────────────────────────────────────────────────────────────────
 * #9 / #15 缺省为开的两个开关
 * ──────────────────────────────────────────────────────────────────────────── */

/** 启动时检查更新：后端 `startup_tasks` 按 `!== false` 判定（缺省开），UI 同口径。 */
export function autoCheckUpdateChecked(config: { autoCheckUpdate?: boolean }): boolean {
  return defaultOn(config.autoCheckUpdate);
}

/**
 * 规则资源自动更新：后端 `rule_resource_scheduler` 按 上游 语义 `=== false 才停、缺省视为开启`。
 * UI 此前写 `!!config.ruleResourceAutoUpdate`（缺省显示成关）→ 会出现「UI 显关、后台在跑」的最恶劣
 * 不一致，故改用同口径。
 */
export function ruleResourceAutoUpdateChecked(config: { ruleResourceAutoUpdate?: boolean }): boolean {
  return defaultOn(config.ruleResourceAutoUpdate);
}

/* ────────────────────────────────────────────────────────────────────────────
 * #10 关闭主窗口行为 ↔ minimizeToTray 双向派生
 * ──────────────────────────────────────────────────────────────────────────── */

export type CloseBehavior = 'to-tray' | 'quit';

/**
 * minimizeToTray → 分段控件值。**缺省（未设）按 to-tray**：与 `store.rs:208` 的 seed 默认
 * （`minimizeToTray: true`）以及 `main.rs::resolve_close_action` 读不到配置时的兜底（恒 true）
 * 同口径。此前 UI 缺省渲染成 quit —— 配置缺该键时会显示「退出应用」而后端实际收进托盘，属
 * 「UI 显示与真实行为相反」的一类不一致（store 已 seed 故实际难触发，但兜底口径必须一致）。
 */
export function closeBehaviorOf(config: { minimizeToTray?: boolean }): CloseBehavior {
  return config.minimizeToTray !== false ? 'to-tray' : 'quit';
}

/** 分段控件值 → minimizeToTray 落盘值（反向，保证双向无损）。 */
export function minimizeToTrayFor(behavior: CloseBehavior): boolean {
  return behavior === 'to-tray';
}

/* ────────────────────────────────────────────────────────────────────────────
 * #18 后台检查间隔
 * ──────────────────────────────────────────────────────────────────────────── */

/** 「仅手动」哨兵值：后端 `subscription_scheduler.rs::select_due` 把 0 处理成「周期不跑」。 */
export const MANUAL_INTERVAL_HOURS = 0;

/** 缺省周期（小时）——未设时 UI 与后端同显 12。 */
export const DEFAULT_INTERVAL_HOURS = 12;

/** 下拉当前值（字符串，喂 `<Select value>`）。 */
export function backgroundIntervalSelectValue(config: {
  subscriptionUpdateIntervalHours?: number;
}): string {
  return String(config.subscriptionUpdateIntervalHours ?? DEFAULT_INTERVAL_HOURS);
}

/**
 * 是否「仅手动」（周期调度不跑）。缺省（未设）不是手动 —— 缺省走 12h 周期。
 */
export function isManualInterval(hours: number | undefined | null): boolean {
  return (hours ?? DEFAULT_INTERVAL_HOURS) === MANUAL_INTERVAL_HOURS;
}

/**
 * 规则资源自动更新的**真实**状态（决定状态行给绿点还是中性点）。
 *
 * 三态而非二态的理由：开关开着 ≠ 真会刷新。间隔为「仅手动」(0) 时后端
 * `rule_resource_scheduler` 周期腿整轮不跑，此时若照给绿点 +「按所选间隔在后台刷新」，
 * 就是在断言一个不会发生的行为（假绿）——与本轮 parity 要消灭的缺陷同类。
 */
export type RuleResourceAutoStatus = 'off' | 'manual' | 'active';

export function ruleResourceAutoStatus(config: {
  ruleResourceAutoUpdate?: boolean;
  subscriptionUpdateIntervalHours?: number;
}): RuleResourceAutoStatus {
  if (!ruleResourceAutoUpdateChecked(config)) return 'off';
  return isManualInterval(config.subscriptionUpdateIntervalHours) ? 'manual' : 'active';
}

/**
 * 订阅自动更新的**真实**状态，与 [`ruleResourceAutoStatus`] 同口径三态。
 *
 * # 判据取自后端 `subscription_scheduler.rs::select_due` 的门链（逐条对应，不臆造）
 *
 * 1. `autoUpdateSubscriptionOnStart != true` → 整轮返空（总开关）⇒ `off`；
 * 2. `subscriptionUpdateIntervalHours == 0`（下拉「仅手动」）**且是周期巡检腿**（`!ignore_staleness`）
 *    → 返空；**启动补更腿不受它影响**（该腿走 `ignore_staleness=true`，只守 10min 地板）⇒ `startup-only`；
 * 3. per-sub `autoUpdate != true` → 跳过该订阅（本函数不涉，属逐订阅粒度）。
 *
 * 第 2 条正是要暴露的那格：开关开着 + 间隔「仅手动」时，**启动时会更、之后不会周期更**。
 * 而卡片 desc 恒写「并按统一后台检查间隔定时刷新」⇒ 对这一格是假话（周期腿压根不跑）。
 */
export type SubscriptionAutoUpdateStatus = 'off' | 'startup-only' | 'active';

export function subscriptionAutoUpdateStatus(config: {
  autoUpdateSubscriptionOnStart?: boolean;
  subscriptionUpdateIntervalHours?: number;
}): SubscriptionAutoUpdateStatus {
  if (config.autoUpdateSubscriptionOnStart !== true) return 'off';
  return isManualInterval(config.subscriptionUpdateIntervalHours) ? 'startup-only' : 'active';
}

/* ────────────────────────────────────────────────────────────────────────────
 * #16 CoreVersionBanner 状态机
 * ──────────────────────────────────────────────────────────────────────────── */

/** `core_get_version_info` 返回体里本横幅关心的子集。 */
export interface CoreVersionInfoLike {
  hasBackup: boolean;
  pendingChangeNotice?: { previousVersion: string; currentVersion: string } | null;
}

/** `EVENT_CORE_VERSION_CHANGED` 事件载荷（当前全仓零发射点，见下方 shouldAck 注释）。 */
export interface CoreVersionChangedPayload {
  previousVersion: string;
  currentVersion: string;
  hasBackup: boolean;
}

export interface CoreBannerNotice {
  previousVersion: string;
  currentVersion: string;
  hasBackup: boolean;
}

export interface CoreBannerState {
  /** 横幅是否渲染。 */
  visible: boolean;
  /** 横幅正文所需的版本号；不可见时为 null。 */
  notice: CoreBannerNotice | null;
  /**
   * 是否渲染「回滚」按钮。对齐 上游：仅 `hasBackup` 为真才渲染。
   *
   * 后端 `core_get_version_info` 现读**真实** `.bak` 状态（换核链路已接线，备份由任何一次换核产生），
   * 故本字段不再恒 false。
   */
  showRollback: boolean;
  /**
   * 「手动换核」按钮是否禁用。
   *
   * `core_replace_manual` **已接线**（零提权，落位于 `<config_dir>/core_update/`）⇒ 恒 `false`。
   * 保留该字段是为了让「按钮可用性」仍由这一个纯函数单点决定（而非散落在组件里），
   * 将来若出现新的禁用条件（如换核进行中）在此收口。
   */
  manualReplaceDisabled: boolean;
  /**
   * 是否应调 `core_update_ack_version_change` 清持久通知（show→ack，弹一次而非每启）。
   * 与 `dismissed` 无关：ack 的是后端持久态，用户是否关掉横幅不影响。
   */
  shouldAck: boolean;
  /** 正文文案 key 后缀：有备份 changedDesc / 无备份 noBackupDesc（对齐 上游）。 */
  descKey: 'changedDesc' | 'noBackupDesc';
}

/**
 * 横幅状态机。事件载荷优先于挂载快照（事件是「刚发生」的即时推送，快照是持久通知）。
 *
 * `manualReplaceDisabled`（换核已接线 → 恒 false）与 `showRollback`（由 `hasBackup` 决定）都是对
 * 后端真实能力的如实反映，不是可配置项，故不接受入参覆写。
 */
export function coreBannerState(input: {
  versionInfo?: CoreVersionInfoLike | null;
  eventPayload?: CoreVersionChangedPayload | null;
  dismissed: boolean;
}): CoreBannerState {
  const { versionInfo, eventPayload, dismissed } = input;

  let notice: CoreBannerNotice | null = null;
  if (eventPayload) {
    notice = {
      previousVersion: eventPayload.previousVersion,
      currentVersion: eventPayload.currentVersion,
      hasBackup: eventPayload.hasBackup,
    };
  } else if (versionInfo?.pendingChangeNotice) {
    notice = {
      previousVersion: versionInfo.pendingChangeNotice.previousVersion,
      currentVersion: versionInfo.pendingChangeNotice.currentVersion,
      hasBackup: versionInfo.hasBackup,
    };
  }

  const visible = notice !== null && !dismissed;
  return {
    visible,
    notice: visible ? notice : null,
    showRollback: visible && notice!.hasBackup,
    manualReplaceDisabled: false,
    shouldAck: notice !== null,
    descKey: notice?.hasBackup ? 'changedDesc' : 'noBackupDesc',
  };
}

/* ────────────────────────────────────────────────────────────────────────────
 * #17 每会话一次去重闸门
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 「每会话一次」闸门工厂：首次调用返 true（放行），此后恒 false。
 *
 * 抽成工厂而非直接写模块级 `let`，是为了让去重语义可被单测直接锁死（模块级变量跨用例污染，
 * 且第二次调用返 false 这条正是最容易在重构中丢掉的行为）。消费方在模块顶层建单例即等价于
 * 上游的 `let coreBaselineWarnedThisSession = false`。
 */
export function createOnceGate(): () => boolean {
  let fired = false;
  return () => {
    if (fired) return false;
    fired = true;
    return true;
  };
}

/* ────────────────────────────────────────────────────────────────────────────
 * 便携版更新：「已下载，待手动替换」不是「更新失败」
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 便携版更新包的文件名口径 —— 与产出侧**逐字同口径**：
 * `crates/updater/src/github.rs::PORTABLE_ZIP_PREFIX` + `.zip`（`scripts/verify-packaging.mjs`
 * 的 `updaterPortableCandidates` 也是同一份字面量）。三处任一改名，都要一起改。
 */
const PORTABLE_ZIP_PREFIX = 'polaris-portable-';

/**
 * 已下载的更新包是否是 **Windows 便携版 zip**（⇒ 后端结构性装不了它，只能交系统 + 用户手动替换）。
 *
 * # 为什么 UI 必须把它与「形态错配」分开呈现
 *
 * 便携用户走的是**正确**路径，不是出错路径：`find_suitable_update_asset` 的 loose 分支只选
 * `polaris-portable-*.zip`（无回落，宁可不更新也不发安装器）。该 zip 走到
 * `runtime/update_install.rs::classify_installer` 时不被识别（只认 `.exe/.dmg/.appimage/.deb`）
 * ⇒ `InstallReject::UnknownAsset` ⇒ command 层 `shell.open` 交系统，返
 * `{ ok:false, reason:"form-mismatch" }`。
 *
 * `ok:false` 对调用方是**准确**的（确实一次安装都没执行，不改它）；但 UI 若把这条原样渲染成
 * error 态的「更新失败」，用户读到的是一次失败 —— 事实是**包已经下好了，只差手动解压覆盖**。
 * 两者的下一步动作完全不同（重试 vs 去解压），故必须分流。
 *
 * # 判据为什么是「前缀 + 后缀」而不是只看 `.zip`
 *
 * 只看 `.zip` 会把将来任何别的 zip 资产也说成便携版。收紧到产出侧口径后，判据失配的
 * **失败方向是回落到通用的形态错配文案** —— 保守、且不会对用户编一个不成立的场景。
 *
 * 路径分隔符 `/` `\` 都切：落点由后端拼（Windows 上是 `\`），而本函数跑在同一个 UI 里。
 */
export function isPortableZipUpdate(downloadedPath: string | null | undefined): boolean {
  if (!downloadedPath) return false;
  const base = downloadedPath.split(/[\\/]/).pop() ?? '';
  return base.startsWith(PORTABLE_ZIP_PREFIX) && base.endsWith('.zip');
}

/* ────────────────────────────────────────────────────────────────────────────
 * 更新包的摘要校验状态：「这版有没有摘要」与「这次下载校没校」是**两件事**
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 该 release 是否随包给了期望 sha256 摘要（⇒ 下载腿会做逐字节强校验）。
 *
 * # 判据与后端逐条同口径
 *
 * `commands/updater.rs::resolve_expected_digest`：字段缺失 → 无；非字符串 → 后端**报错拒装**
 * （不是「没摘要」，故也不该走本函数的 false 分支去提示「无摘要」——但本函数拿到的是 TS 侧
 * 已声明为 `string | undefined` 的值，非字符串只可能来自破损回包，判 false 是保守方向：
 * 多提示一次，不会把一次会失败的下载说成安全）；trim 后空串 / 纯空白 → 无（后端 `continue`）。
 *
 * **刻意不校验 hex 形态**：后端对「有值但写坏了」的处理是照常进校验、然后
 * `verify_hex_digest` 的 `InvalidExpectedHash` 分支**拒装**，不是当成「本来就没摘要」放行。
 * 这里若自作主张把坏 hex 判成「无摘要」，就会在一次注定失败的下载前先给用户一条不成立的说明。
 *
 * # 时机：这是**下载开始前**就能算出的判断
 *
 * `UpdateInfo.sha256` 与 `hasUpdate` 同一次 `update_check` 回包到手，故「这个版本有没有摘要」
 * 在用户还能选择「下不下」的那一刻就已知 —— 等到装机环节才说，是在风险已经承担之后才告知。
 * 它与 [`appDownloadIntegrity`] 不是一个状态：后者是下载**之后**的既成事实，两者可用时机不同，
 * 合并成一个布尔就必然要么提前撒谎、要么迟到。
 */
export function releaseShipsDigest(
  updateInfo: { sha256?: string | null } | null | undefined,
): boolean {
  const raw = updateInfo?.sha256;
  return typeof raw === 'string' && raw.trim() !== '';
}

/** 一次 `update_download` 回包的摘要校验结果。`unknown` ≠ `unverified`，成因见下。 */
export type AppDownloadIntegrity = 'verified' | 'unverified' | 'unknown';

/**
 * 从 `updateApi.download()` 的回包读出摘要校验**结果**（下载之后才存在的事实）。
 *
 * # 为什么是三态而不是布尔
 *
 * 设置页的 `downloaded` 态有**两条**到达路径，只有一条带着回包：
 *  - 用户在本卡点「下载」→ `updateApi.download()` 的返回值里有 `verified`；
 *  - 启动腿 `runtime/startup_tasks.rs::spawn_auto_download` 在后台下完 → 本卡只收到
 *    `update:progress` 的 `downloaded` 事件（`emit_progress`），**拿不到任何回包**。
 *
 * 后一条路径上本地无从得知校没校。把「不知道」折叠进 `unverified` 会凭空造一条警告；
 * 折叠进 `verified` 则是假绿。故只在**确知未校验**（`verified === false`）时才提示。
 */
export function appDownloadIntegrity(
  result: { verified?: boolean | null } | null | undefined,
): AppDownloadIntegrity {
  const verified = result?.verified;
  if (verified === true) return 'verified';
  if (verified === false) return 'unverified';
  return 'unknown';
}

/* ────────────────────────────────────────────────────────────────────────────
 * 暂存条目的**段级**译名（原型 `settingLabel:4257`）
 * ──────────────────────────────────────────────────────────────────────────── */

/**
 * 聚合型配置键 → i18n 键。原型取的是**行标题文本**（`row.querySelector('.sr-tx b')`），拼成
 * 「接管系统 DNS · 开」；本仓此前直接把配置键名塞进文案 ⇒ 明细里显示「修改设置 · dnsConfig」。
 *
 * 两层退化都可感知：① 用户读不出改的是哪一项；② DNS 段全部开关经 `patchDns` 打包进**同一个**
 * `dnsConfig` 键，N 个开关的编辑**塌成一条**，而 §1.17 的单项撤销粒度跟着变粗（撤一条 = 撤整段）。
 *
 * 只覆盖**聚合键**（对象 / 列表：一个键背后是一整段设置），而不是给 29 个 Class B 键各配译文——
 * 后者是 5×N 的翻译面，而标量键的键名与它那一个开关一一对应，认得出。**撤销粒度**那一半这里修不了：
 * 要恢复到逐开关一条，需要 `entityPath` 下沉到子字段，代价大得多，另裁。
 */
export const STAGED_SETTING_SECTION_LABELS: Readonly<Record<string, string>> = {
  tunConfig: 'settings.section.tun',
  dnsConfig: 'settings.section.dns',
  ruleResources: 'settings.section.ruleResources',
  regionRouting: 'settings.section.regionRouting',
  singboxDashboard: 'settings.section.singboxDashboard',
  bypassLANList: 'settings.section.bypassLanList',
  fakeIpFilterList: 'settings.section.fakeIpFilterList',
  bypassProcesses: 'settings.section.bypassProcesses',
  customAppPresets: 'settings.section.customAppPresets',
};
