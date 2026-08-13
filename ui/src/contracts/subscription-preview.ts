/**
 * 订阅预检契约（渲染侧展示层单一真值）。
 *
 * 动机（issue：新增订阅先加后删 + 错误不细分）：add 订阅原为「先建记录 → 再拉节点」，拉取失败要么留空订阅
 * 要么删记录（闪现/竞态），且失败只有布尔无原因。改为**预检先行**：add 前用 URL 拉取+解析但不写 config，
 * 返回节点数或**分类错误**；成功才建记录，失败留窗 + 说清原因。
 *
 * 本文件只放「渲染侧契约」（零 tauri/网络依赖）：
 *  - SubscriptionErrorKind：错误分类枚举（与 Rust 侧 serde 字面量逐字对齐）
 *  - SubscriptionPreviewResult：预检 IPC 返回形状
 *  - SUBSCRIPTION_ERROR_I18N_KEY：errorKind → i18n key（渲染侧展示用）
 *
 * **分类逻辑不在这里**：上游的 `classifySubscriptionError` 跑在 main（Node）—— catch 后把错误摊平再判。
 * Polaris 的 main = Rust，故分类已落 `crates/net-stack/src/subscription_error.rs`
 * （`classify_subscription_error`，审计 §C4），由 IPC 直接返回 `errorKind`。
 * TS 侧同名函数曾原样照搬且**零调用方**，留着只会与 Rust 双真值漂移，已删。
 */

/** 与 Rust `SubscriptionErrorKind` 的 serde 字面量逐字对齐（IPC 契约，勿单侧改）。 */
export type SubscriptionErrorKind =
  | 'dns' // 域名解析失败
  | 'timeout' // 连接/读取超时
  | 'refused' // 连接被拒绝（端口不可达）
  | 'http' // 服务器返回 4xx/5xx
  | 'ssrf' // 命中 SSRF guard（内网/本机地址），含重定向超限
  | 'scheme' // 非 http(s) 协议
  | 'toolarge' // 响应体积超上限
  | 'parse' // 拉到内容但非有效订阅格式
  | 'empty' // 解析成功但 0 节点
  | 'unknown'; // 未归类

export interface SubscriptionPreviewResult {
  ok: boolean;
  /** ok=true：解析出的可用节点数。 */
  nodeCount?: number;
  /** ok=false：错误分类。 */
  errorKind?: SubscriptionErrorKind;
  /** errorKind='http' 时的状态码（i18n `httpDetail` 的 {{status}} 插值）。 */
  httpStatus?: number;
  /** 原始（已脱敏）错误信息，仅诊断用，不直接展示给用户。 */
  message?: string;
}

/** errorKind → i18n key（渲染侧 t() 取展示文案；标题/详情各一，见 i18n `sub.preview.*`）。 */
export const SUBSCRIPTION_ERROR_I18N_KEY: Record<
  SubscriptionErrorKind,
  { title: string; detail: string }
> = {
  dns: { title: 'sub.preview.dnsTitle', detail: 'sub.preview.dnsDetail' },
  timeout: { title: 'sub.preview.timeoutTitle', detail: 'sub.preview.timeoutDetail' },
  refused: { title: 'sub.preview.refusedTitle', detail: 'sub.preview.refusedDetail' },
  http: { title: 'sub.preview.httpTitle', detail: 'sub.preview.httpDetail' },
  ssrf: { title: 'sub.preview.ssrfTitle', detail: 'sub.preview.ssrfDetail' },
  scheme: { title: 'sub.preview.schemeTitle', detail: 'sub.preview.schemeDetail' },
  toolarge: { title: 'sub.preview.toolargeTitle', detail: 'sub.preview.toolargeDetail' },
  parse: { title: 'sub.preview.parseTitle', detail: 'sub.preview.parseDetail' },
  empty: { title: 'sub.preview.emptyTitle', detail: 'sub.preview.emptyDetail' },
  unknown: { title: 'sub.preview.unknownTitle', detail: 'sub.preview.unknownDetail' },
};
