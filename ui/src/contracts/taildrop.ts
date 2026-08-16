/**
 * Taildrop 收件箱的跨层共享类型（Rust `commands/taildrop.rs` 的 1:1 镜像，serde camelCase 已对齐）。
 *
 * 数据链：sing-box 管理 API `SubscribeTaildropInbox` 首帧 → Rust 投影 → 本类型 → 收件箱弹窗。
 *
 * # 三个计数不在这里
 *
 * 未读 / 待处理 / 接收中的**计数**随 `TailscaleStatusEvent` 每帧下发（见 `contracts/tailscale-status.ts`），
 * 走的是已有的 STATUS 事件流。角标读那边，本类型只在弹窗打开时拉一次明细 —— 一个短命面板不值得
 * 常驻一条按端点翻倍的订阅。
 */

/** 已落盘、等待处理的一个文件。 */
export interface TaildropFile {
  name: string;
  /** 字节。展示用 `screens/shared/format.ts` 的 `fmtBytes`，不另写格式化。 */
  size: number;
  senderName: string;
  /** Unix **秒**（注意不是毫秒）。展示走 `lib/relative-time.ts`，本层不产生文案。 */
  modifiedAt: number;
}

/** 正在接收中的一个文件。 */
export interface TaildropReceiving {
  name: string;
  size: number;
  receivedBytes: number;
  /** 取消操作的定位键之一。**与 name 必须成对使用**：两个发件人可以同时发同名文件。 */
  senderID: string;
  senderName: string;
}

/** 一次收件箱快照。 */
export interface TaildropInbox {
  files: TaildropFile[];
  receiving: TaildropReceiving[];
}

/** 取件结果。`canceled` = 用户在原生保存框里按了取消，**不是错误**，不该提示失败。 */
export interface TaildropSaveResult {
  canceled: boolean;
  path?: string;
  bytes?: number;
}
