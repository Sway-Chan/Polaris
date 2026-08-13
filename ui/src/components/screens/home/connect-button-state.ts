/**
 * 首页连接圆钮三态推导 —— 纯函数（无 react/store 依赖），供 .test.ts 覆盖状态矩阵。
 * 移植自 上游 `src/renderer/components/home/connect-button-state.ts`，语义逐字对齐能力契约
 * （design/polaris-上游-capability-contract.md §主页 Home「三态圆钮」）。
 *
 * 三态（+ 进行中相位）：
 *  - start：未连接 → 点击 startProxy；未配置节点则 disabled。
 *  - stop：已连接 → 点击 stopProxy；恒可点（断开不受配置完整性约束）。
 *  - error：有错误且未连接 → 点击重试 = startProxy；未配置节点则 disabled。
 *  - starting：启动中 → busy，但**可点 = 取消**（见下方「对 上游的刻意偏离」）。
 *  - stopping：停止中 → busy 且 disabled（停止是终态意图，没有"取消停止"这回事）。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 * **对 上游 `connect-button-state.ts:28` 的刻意偏离（用户已授权，勿当迁移漏做抄回去）**
 *
 * 上游 原版：`starting → { busy: true, disabled: true }` —— 启动期圆钮完全点不动。本仓移植时逐字
 * 照搬了这一行，于是把上游的缺陷一并继承了下来。
 *
 * 为什么必须偏离（真机事故，用户亲历）：TUN 模式下起核，孤儿 root 核锁死内核 cache 文件 → 核起来跑
 * ~9s 后 FATAL → 预算内自动重试（3 次尝试 + 2s/4s 退避）。这条腿全程 `running:false`，圆钮因此停在
 * `starting` 且 disabled ≈35s。用户原话：**「甚至启动卡死阶段无法关闭启动过程」** —— 后端其实一直
 * 能取消（`proxy_stop` 无前置状态守卫、`stop_inner` 先 bump 世代、让位检查点齐全），缺的只是一个入口。
 * 只要按钮点不动，后端可取消就等于不可取消。
 *
 * 所以 `starting` 改为 `disabled: false` + `action: 'cancel'`（走 stop 通道）。与之配套的后端半是
 * 「等待本身可中断」（`runtime/proxy.rs::sleep_unless_superseded_on`）：没有那一半，点了取消也要静默
 * 等退避睡满；没有这一半，后端再可取消也没人能发出取消。两半缺一即回归。
 *
 * `stopping` **不跟着改**：停止已是用户要的终态，"取消停止"= 重新启动，那是另一个意图、不该由同一
 * 个按钮的同一次点击表达。故 stopping 保持 上游 语义。
 * ─────────────────────────────────────────────────────────────────────────────
 */
export type ConnectButtonKind = 'start' | 'stop' | 'error' | 'starting' | 'stopping';

/**
 * 点击该按钮要做的事 —— **调用方必须按它分发，不得再自行按 `isConnected` 猜**。
 *
 * 根因：`starting` 相位下 `isConnected` 恒 false，凭它分发会走进 start 分支 = 在已有起核腿之上再叠
 * 一次启动（正是 `TrayMenu.tsx` 原 :219-236 的缺陷形态）。把「什么状态」与「点了干什么」分成两个字段，
 * 消费方就没有再猜一次的余地。
 *
 * - `start`：起核（含 error 态的重试）。
 * - `stop`：停核（已连接）。
 * - `cancel`：取消**正在进行的启动** —— 走的也是 stop 通道，但语义与文案不同（用户没在断开一条已建立
 *   的连接，而是在放弃一次还没成的启动）。
 * - `none`：不可操作（`stopping`）。
 */
export type ConnectButtonAction = 'start' | 'stop' | 'cancel' | 'none';

export interface ConnectButtonState {
  kind: ConnectButtonKind;
  /** 进行中（转圈动画）。**注意：busy 不再蕴含 disabled** —— starting 是 busy 但可点（取消）。 */
  busy: boolean;
  disabled: boolean;
  /** 点击语义，见 [`ConnectButtonAction`]。 */
  action: ConnectButtonAction;
}

export interface ConnectButtonInputs {
  proxyPhase: 'idle' | 'starting' | 'stopping';
  isConnected: boolean;
  hasError: boolean;
  isServerConfigured: boolean;
}

export function deriveConnectButtonState(input: ConnectButtonInputs): ConnectButtonState {
  const { proxyPhase, isConnected, hasError, isServerConfigured } = input;
  // 启动中 → 可点取消（对 上游 :28 的刻意偏离，理由见文件头）。
  // 不受 isServerConfigured 约束：核都已经在起了，"节点没配好"不该妨碍你把它叫停。
  if (proxyPhase === 'starting') {
    return { kind: 'starting', busy: true, disabled: false, action: 'cancel' };
  }
  // 停止中 → 无可操作（保持 上游 语义，见文件头末段）。
  if (proxyPhase === 'stopping') {
    return { kind: 'stopping', busy: true, disabled: true, action: 'none' };
  }
  // 已连接优先于 error：核已运行时按钮须显「停止」（点击 stopProxy），否则残留 error + 核运行
  // 会让按钮显 error 却在点击时执行停止，自相矛盾。仅「未连接 + 有错误」才是可重试的 error 态。
  if (isConnected) return { kind: 'stop', busy: false, disabled: false, action: 'stop' };
  if (hasError) {
    return { kind: 'error', busy: false, disabled: !isServerConfigured, action: 'start' };
  }
  return { kind: 'start', busy: false, disabled: !isServerConfigured, action: 'start' };
}

/** 派生态 → 原型 `.connect-btn` 修饰类（prototype.css §连接圆钮）。 */
export function connectButtonClass(kind: ConnectButtonKind): 'busy' | 'on' | 'off' | 'err' {
  if (kind === 'starting' || kind === 'stopping') return 'busy';
  if (kind === 'stop') return 'on';
  if (kind === 'error') return 'err';
  return 'off';
}

/**
 * 启停相位归一 —— 主窗与托盘共用同一口径（两窗不共享 store，只有共用纯函数才不会分叉）。
 *
 * **`stopping` 压过 `starting`**（顺序不可颠倒）：取消一次启动时两个标志会同时为真（start 还在
 * 飞、stop 已发出）。若 starting 优先，圆钮在取消途中仍显示「可点取消」→ 用户可以重复点、每点一次
 * 多发一条 stop。停止是终态意图，一旦发出就该压住一切。
 */
export function deriveProxyPhase(input: {
  starting: boolean;
  stopping: boolean;
}): 'idle' | 'starting' | 'stopping' {
  if (input.stopping) return 'stopping';
  if (input.starting) return 'starting';
  return 'idle';
}
