/**
 * 「退出更新弹窗」在各 phase 下发哪个动作 —— 纯函数，单独成文件是为了能在 node 环境直测
 * （`main.ts` 一 import 就会碰 `document` / `./style.css`，测不了）。
 */
import type { PopupAction, PopupPhase } from '@/contracts/types/update';

/**
 * 后端 `PopupAction::is_valid_for`（`crates/updater/src/popup.rs:180-195`）是**白名单**，
 * 且非法动作**静默忽略**（`:177`，不报错、不改状态）。
 *
 * 此前渲染端 Esc 无条件发 `close`，而 `close` 只在 done/error 的白名单里 ⇒ **下载中按 Esc 是死键**：
 * 什么都不发生、也没有任何反馈，而该窗 `always_on_top` ⇒ 用户读作卡死。
 * remind 态回 `later`：它与 `close` 在后端走同一条 `close_update_popup` 分支（`updater.rs:846`），
 * 语义等价且在白名单内。
 *
 * phase 未知（渲染逻辑挂了 / 初始态缺失）时回落 `close` —— **逃生优先**：宁可发一个可能被拒的动作，
 * 也不能因为「算不出该发什么」而不发（那正是 #300 里「窗关不掉」的形态）。
 */
export function exitActionFor(phase: PopupPhase | undefined): PopupAction {
  if (phase === 'progress') return 'cancel';
  if (phase === 'remind') return 'later';
  return 'close';
}
