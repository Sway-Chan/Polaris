/**
 * 弹窗载荷里**随行事实**的渲染（纯函数）。
 *
 * 单独成文件的理由与同目录 `exit-action.ts` 逐字相同：`main.ts` 一 import 就会碰 `document` /
 * `./style.css`，本仓 vitest 是 `environment:'node'`（无 jsdom，有意为之）⇒ 写在 `main.ts` 里的
 * 逻辑测不了，只剩源码级守卫，而那守得住「写没写那一行」，守不住「写出来的是什么」。
 *
 * # 为什么字节数在这一侧拼，而不是后端拼好一个串发过来
 *
 * 本文件的 `bytesText` 顶替的是 Rust 侧那个 `bytes_text: Option<String>` —— 有字段、有 serde 单测、
 * 这边有读点，**唯独全仓没有任何生产写点**，于是渲染端恒回落 `${pct}%`，用户一次都没见过字节数。
 * 复活它的方向不是「后端补一句 `format!`」：数字的小数点、千分位、数字形（fa 用 ۰۱۲۳）全部随语言
 * 变，后端拼串就是又一条绕过 i18n 的老路（本仓已有一条登记在案：`emit_progress` 的硬编码中文
 * message）。故后端只发数字，拼串留在这一侧。
 *
 * 换算**复用全仓那一个** `fmtBytes`（`components/screens/shared/format.ts`，纯函数、零依赖，
 * Rollup 只会把它一个拖进本入口）——不新写一份：单位由它给出（B/KB/MB/GB/TB），调用点不得再拼
 * 一个死单位，那条不变量本仓已有专门的门守着（`format.test.ts` 的「fmtBytes 后紧跟裸单位」扫描，
 * 起因是 `SubInfoBar` 真机渲染出过「1.20 TB GB」）。
 */
import { fmtBytes } from '@/components/screens/shared/format';

/**
 * progress 态的字节文案。返回 `undefined` = 这一帧没有可报的字节数（调用方回落百分比）。
 *
 * 分母未知（后端没给 / 给了 0）时只报已收量，**绝不拿已收字节凑一个假分母** —— 那会让进度条
 * 旁边写着一路 100% 再跳回去（同后端 `progress_percent` 的第一条规则）。
 */
export function bytesText(received?: number, total?: number): string | undefined {
  if (typeof received !== 'number' || !Number.isFinite(received) || received < 0) return undefined;
  if (typeof total === 'number' && Number.isFinite(total) && total > 0) {
    return `${fmtBytes(received)} / ${fmtBytes(total)}`;
  }
  return fmtBytes(received);
}

/**
 * done 态的主语：**下的是哪一版、落在哪儿**。两者都缺就返回空串（调用方不渲染那一行）。
 *
 * 这一行存在的理由不只是「多显示点东西」：`done` 此前不带任何随行事实，于是它与「什么都没下」
 * 在屏幕上**长得一模一样** —— 那正是本批修的第一条缺陷。
 */
export function doneSubject(version?: string, filePath?: string): string {
  return [version, filePath].filter((s): s is string => !!s && s.trim() !== '').join(' · ');
}
