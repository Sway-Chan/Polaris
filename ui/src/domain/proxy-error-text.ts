/**
 * 代理错误码 → 用户可见文案的**唯一解析点**（三段式：i18n key → 后端 message → 通用兜底）。
 *
 * # 修的是什么（根因）
 *
 * 后端 `event:proxyError` / `event:proxyLifecycle` 的载荷同时带 `errorCode`（结构化分类）与
 * `message`（**Rust 侧写死的中文串**）。渲染端此前一律写成
 *
 * ```ts
 * toast.error(data.message || t('home.proxyCrashed', '代理已断开'));
 * ```
 *
 * —— `message` **压过** i18n key。而 `emit_proxy_error(message, error_code)` 两个参数都非可选，
 * 于是 `||` 右边那半永远短路不到：那些 key 名义上存在、实际是死键，俄语/波斯语用户在最高频的
 * 错误路径上看到的是中文。2026-07-31 刚给 Rust 侧原生对话框/托盘/提权引导接了 i18n
 * （`src-tauri/src/i18n.rs` + `locales/auxiliary/` 的 `native.*`），若这条路径不修，「Rust 接了 i18n」
 * 在用户最常撞见的那一面上等于没做。
 *
 * # 为什么不是「把 Rust message 也翻译了」
 *
 * 那会让同一句话有两个真值源（Rust 一份、locale 一份），且要为 60 余条错误在 Rust 侧各建一个键。
 * 分类信息**后端已经给了**（`errorCode`），前端只需按它取自己的键 —— 跨进程只传分类、不传文案，
 * 是这类问题唯一不产生第二真值源的解法。
 *
 * # 三段式（第 2 段不许省）
 *
 *  1. `errorCode` 有、且 [`PROXY_ERROR_TEXT_KEY`] 里有对应键 → 用键（**恒是用户语种**）。
 *  2. 否则回落 `message` —— 它至少是**准确**的，只是语种可能不对。
 *  3. `message` 也没有 → 交回 `null`，由调用方落到自己的通用兜底句。
 *
 * **第 2 段是本函数的重点**，不是补丁：`errorCode` 是**可选**的（见
 * `contracts/types.ts::ProxyLifecycleEvent.errorCode` 的注释：「后端『无可诚实断言的分类』那一类」
 * 会缺失），且门里的豁免表有两个码是**故意**不配键的。省掉第 2 段
 * 就是把「后端诚实地说不出分类」翻译成「用户看到一句无信息量的通用错误」——
 * 拿准确性换语种，比原缺陷更糟。
 *
 * # 谁在消费
 *
 *  - `App.tsx::handleProxyErrorEvent`（`event:proxyError` 分腿的 toast 文案）。那条通道上
 *    Rust 恒发码、且门锁死「每个被路由的码都在表里」⇒ 实际恒命中第 1 段。
 *  - `components/layout/pending-bar-logic.ts::applyStateOnLifecycle` 与 `PendingChangesBar`
 *    的 `onError` 腿（「应用失败：{{reason}}」里的 `reason`）。**三段全部可达**：
 *    `STARTUP_FAILED` / `ROOT_ORPHAN_BLOCKED` 走第 2 段，无码失败腿走第 2 段，
 *    连 message 都没有时走第 3 段（`reason: null` → 不带原因的那句 `home.pendingApplyFailed`）。
 *
 * 覆盖门在 `contracts/proxy-error-key-coverage.test.ts`（读 Rust 源码对账 + 三段行为断言）。
 */

/** `t(key, fallback?)` 的最小结构（i18next `TFunction` 的重载形不与本窄接口结构兼容，故各调用点自行收窄）。 */
export type ProxyErrorTranslate = (key: string, fallback?: string) => string;

/** 本模块认得的错误载荷（`ProxyErrorEvent` 与 `ProxyLifecycleEvent` 的公共子集）。 */
export interface ProxyErrorTextInput {
  errorCode?: string;
  message?: string;
}

/**
 * `errorCode` → i18n 键。真值源是 Rust `runtime/proxy.rs` 的 `pub mod code`。
 *
 * # 为什么是**显式映射表**而不是约定拼接（`errors.<camelCase(code)>`）
 *
 * 拼接更省字，但三条各自致命：
 *
 *  1. **失效方式不同**。拼接法在「新增码没配键」时把裸键名（`errors.rootOrphanBlocked`）直接渲染
 *     给用户 —— i18next 找不到键就返回键名，那是一串开发者标识符。映射表在同样情形下取到
 *     `undefined`，于是**落到第 2 段**、用户看到的是后端那句准确的话。「新码来不及配键」是必然会
 *     发生的事，两种失效里只有后者仍对用户负责。
 *  2. **本仓的键**根本不是一码一键：`PROCESS_EXITED` 与 `AUTO_RESTART_FAILED` 共用
 *     `home.proxyCrashed`、`SYSTEM_PROXY_FAILED` 与 `EXIT_MISMATCH` 共用 `home.proxyMisdirected`
 *     ——**共用是刻意的**（同一句话不得在两处措辞分叉）。拼接法逼出四个键、两两内容相同，
 *     随后必然漂移。且键跨 `home.*` / `errors.*` 两个命名空间，没有一条前缀规则能同时覆盖。
 *  3. **豁免无处可写**。门里的豁免表有两个码是**判断过后决定不配键**的（理由见该表），
 *     拼接法表达不了「这个码故意没有键」与「这个码漏配了键」的区别，门也就无从穷尽校验。
 *
 * 代价：多一张表要维护。这正是门存在的理由 —— Rust 码集 = 本表键集 ∪ 豁免表键集，双向精确相等。
 */
export const PROXY_ERROR_TEXT_KEY: Readonly<Record<string, string>> = {
  // 崩溃腿（终态，核不再运行）。两码共用一句：用户下一步动作相同（重连）。
  PROCESS_EXITED: 'home.proxyCrashed',
  AUTO_RESTART_FAILED: 'home.proxyCrashed',
  // 出口误导腿（非终态，核仍在跑，只是流量没按预期经代理）。
  SYSTEM_PROXY_FAILED: 'home.proxyMisdirected',
  EXIT_MISMATCH: 'home.proxyMisdirected',
  // 能力降级腿（非终态）。**不与上一对合并**：指引指向「规则资源」页下载，与直连风险是两回事。
  RULE_RESOURCES_MISSING: 'home.ruleResourcesMissing',
  // 权限/环境轴。四条各有各的可操作指引，合并任意两条都会把指引冲掉。
  TUN_ROUTE_NOT_CAPTURED: 'errors.tunRouteNotCaptured',
  // TUN 网卡从未建出来（#327）。**不与上一条合并**：那条的下一步是「断开另一个 VPN」，本条是
  // 「查 wintun 驱动 / 安全软件拦截」——两条指引互相说不通，合并等于两边都指错。
  TUN_ADAPTER_MISSING: 'errors.tunAdapterMissing',
  // TUN 地址装不上（#332）。与上两条同属「TUN 起不来」，但三条的下一步动作两两不同：
  // 断开别的 VPN / 查 wintun 驱动 / 清掉占着这个地址的残留网卡。
  TUN_ADDRESS_UNAVAILABLE: 'errors.tunAddressUnavailable',
  HELPER_GATE_ABORTED: 'errors.helperGateAborted',
  HELPER_NOT_INSTALLED: 'errors.helperNotInstalledDesc',
};

/**
 * **判断过后决定不配键**的码（`STARTUP_FAILED` / `ROOT_ORPHAN_BLOCKED`）住在门那边 ——
 * `contracts/proxy-error-key-coverage.test.ts` 的 `KEY_EXEMPT`，逐条带理由。
 *
 * 为什么豁免表不在本文件：它是**纯门元数据**，运行期一次都不读（同
 * `protocol-settings-coverage.test.ts` 把 `EXEMPT` 放在门里的既有先例）。放这儿只会往产物里
 * 塞一段永不执行的数据，还让「本表 ∪ 豁免表 = Rust 码集」这条判据横跨两个文件。
 *
 * 那两条的共同判据（详版见门里）：它们的后端 `message` 携带**本次失败独有的可诊断信息**
 * （就绪门是超时还是核启动期退出、核自己吐的那行、残留 root 孤儿的 pid），而任何 i18n 键只能
 * 给出「启动失败，请检查服务器配置」这类同义反复。给它们配键 = 用「语种正确」换掉「唯一能指出
 * 下一步动作的那句话」—— 那正是第 2 段存在的全部意义。
 */

/**
 * 三段式的前两段：命中键 → 译文；否则 → 后端 message；两者皆无 → `null`。
 *
 * `null` 是**有含义的返回值**（第 3 段的入口），不是失败：调用方据此落到自己的通用兜底句
 * （`PendingChangesBar` 落 `home.pendingApplyFailed`，[`proxyErrorText`] 落 `errors.operationFailed`）。
 *
 * 空串 / 全空白的 `message` 与缺字段同等对待 —— 否则 toast 会渲染出一条空白提示，
 * 或「应用失败：」后面跟着空。
 */
export function proxyErrorReason(
  data: ProxyErrorTextInput,
  t: ProxyErrorTranslate
): string | null {
  // ① `errorCode` 有、且前端有对应键 → 用键。
  //    `hasOwnProperty` 而非直接下标：`errorCode` 是从 IPC 收来的任意串，`'toString'` /
  //    `'constructor'` 会从 Object.prototype 上取到函数，落进 `t()` 就是一串 `[native code]`。
  const code = data.errorCode;
  if (code !== undefined && Object.prototype.hasOwnProperty.call(PROXY_ERROR_TEXT_KEY, code)) {
    return t(PROXY_ERROR_TEXT_KEY[code]);
  }
  // ② 回落后端 message —— 语种可能不对，但**准确**。删掉这一段就是拿准确性换语种。
  const message = data.message?.trim();
  if (message !== undefined && message !== '') return message;
  // ③ 后端既说不出分类、也没给话。
  return null;
}

/**
 * 三段式全量（第 3 段落到通用兜底句）—— toast 这类**必须有字才能显示**的出口用它。
 *
 * `errors.operationFailed` 是本仓既有的通用失败句（`ErrorHandler.handleApiError` 同款），
 * 五语齐备，不新造键。
 */
export function proxyErrorText(data: ProxyErrorTextInput, t: ProxyErrorTranslate): string {
  return proxyErrorReason(data, t) ?? t('errors.operationFailed');
}
