/**
 * 选择性备份 / 恢复的**类别枚举**（跨语言类型镜像 + UI 展示顺序）。
 *
 * 6 类（前 5 类对应「数据备份与恢复」卡的统计维度，第 6 类是通用设置）：
 *   manualNodes     手动节点   —— servers（无 subscriptionId、非 endpoint 协议）
 *   meshNodes       组网节点   —— servers（无 subscriptionId、endpoint 协议，如 Tailscale/WireGuard）
 *   subscriptions   订阅源     —— subscriptions[] + 其展开节点；两者一体进出（离线也能恢复）
 *   customRules     自定义规则 —— customRules[] + customRuleSets[]
 *   appRules        应用分流   —— appRules[]
 *   generalSettings 通用设置   —— 其余所有 config 字段（排除法，自动涵盖未来新增设置）
 *
 * ## 为什么这里只剩类型与常量（2026-07-16，§C5 落地时裁定）
 *
 * 原本这里还有 `countCategory` / `detectCategories` / `pickCategories` / `mergeCategories`
 * 四个函数（从 上游 逐字照搬，共 ~200 行）。它们已**全部删除**，单一真值移到 Rust
 * `crates/store/src/backup.rs`。
 *
 * 判据（§D 三问决策树，判定单元是**导出组**不是文件）：
 * - **Q1「结果进 config / 落盘？」→ 是** → Rust 是唯一权威。这四个函数的产物直接决定写进
 *   `config.json` 的内容（整类替换 / 空跳过 / selectedServerId 归零），属**数据安全**逻辑
 *   ——合错一步就是用户节点被空覆盖。绝不容双份。
 * - **实证：前端对这四个函数零消费**（2026-07-16 grep：`countCategory` / `detectCategories` /
 *   `mergeCategories` 各 0 处外部引用；`pickCategories` 唯一命中是 ipc-channels.ts 里的一句注释）。
 *   契约上也不可能有消费：`backup:importPick` 由后端算好 `available` + `counts` 下发
 *   （**派生字段随 DTO 下发 > 前端谓词**），`backup:importApply` 由后端 merge 后保存。
 * - **故留着 = 纯漂移源**：两侧同时存在又无对拍测试（前端无 vitest），改一侧忘另一侧必然发生。
 *
 * 保留的两个导出各有其合法性：
 * - `BackupCategory`：①**跨语言类型镜像**（§E 裁定：无 codegen 时 TS/Rust 的必要对应物），
 *   对应 Rust `polaris_store::backup::BackupCategory`。
 * - `BACKUP_CATEGORIES`：③**UI 展示顺序** + 全选基准（`SettingsBackup.tsx` 按此序渲染勾选行）。
 *   顺序即契约：Rust `detect_categories` 按同序返回；该不变式由 Rust 侧
 *   `backup_categories_order_matches_frontend` 测试锁住（任一侧改序 → Rust 测试转红）。
 */

export type BackupCategory =
  | 'manualNodes'
  | 'meshNodes'
  | 'subscriptions'
  | 'customRules'
  | 'appRules'
  | 'generalSettings';

/**
 * 有序类别清单（UI 展示顺序 + 全选基准）。
 *
 * 与 Rust `polaris_store::backup::BACKUP_CATEGORIES` **同序**——顺序即契约，勿随手调整。
 */
export const BACKUP_CATEGORIES: readonly BackupCategory[] = [
  'manualNodes',
  'meshNodes',
  'subscriptions',
  'customRules',
  'appRules',
  'generalSettings',
];
