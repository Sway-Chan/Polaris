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
 *
 * 复活它的方向不是「后端补一句 `format!`」，理由是**「不再新增后端产出的用户可见文案」**：
 * 此前的登记欠账（`emit_progress` 的 `message` 硬编码中文直出）已随 U1（错误码化）拆除，
 * 但这条纪律不变：后端每多拼一个给人看的串，同类口子就宽一格。数字过线、渲染端拼串。
 *
 * ⚠️ **别把这条说成「换到前端就本地化了」**（初版注释是那么写的，不成立）：`fmtBytes` 本身
 * **语种无关** —— 拉丁数字、`.` 作小数点、写死 `B/KB/MB/GB/TB`，fa 用户看到的仍是 `18.3 MB`。
 * 真要按语种给数字形/小数点，得走 `Intl.NumberFormat`，那是独立一件事，本批没做。
 *
 * 换算**复用全仓那一个** `fmtBytes`（`components/screens/shared/format.ts`，纯函数、零依赖，
 * Rollup 只会把它一个拖进本入口）——不新写一份：单位由它给出（B/KB/MB/GB/TB），调用点不得再拼
 * 一个死单位，那条不变量本仓已有专门的门守着（`format.test.ts` 的「fmtBytes 后紧跟裸单位」扫描，
 * 起因是 `SubInfoBar` 真机渲染出过「1.20 TB GB」）。
 *
 * ⚠️ **与设置页下载卡的显示口径今天并不一致**（如实登记，超本批射程）：`SettingsUpdate.tsx:821`
 * 恒 `toFixed(1)` 恒拼死 `MB`，而 `fmtBytes` 按量级切单位 ⇒ 同一次 2 GiB 的下载，设置页写
 * `2048.0 MB`、本窗写 `2.00 GB`。两边**除数相同**（都是 1024 进制），分歧只在显示层。统一口径
 * 要动设置页那张卡与钉它的门，单列。
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
 * done 态的主语：**下的是哪一版、包叫什么**。两者都缺就返回空串（调用方不渲染那一行）。
 *
 * 这一行存在的理由不只是「多显示点东西」：`done` 此前不带任何随行事实，于是它与「什么都没下」
 * 在屏幕上**长得一模一样** —— 那正是本批修的第一条缺陷。
 *
 * # 只取文件名，不显示完整路径
 *
 * 载荷里带的是完整落位路径（那是 `done` 的必填随行事实，也是设置页「重启并安装」要的东西），
 * 但**这一屏**放不下它：卡片可用宽 380 − 16×2 = 348px，而三平台样本实测（仓内 `textPx`，12px）
 * 全长路径要 579.5 / 700.6 / 760.4px（溢出 1.7–2.2 倍；与 `style.css` 的 `.sub.path` 规则注释
 * 同一次测量、同一套数——🟢#5：本行此前抄着上一轮的 533–747px 残留样本，两处两套数）。
 * 取文件名后 300.0 / 283.1 / 282.5px，均留余量。而 `.sub.path` 是**尾截断**省略号 ⇒ 显示全长
 * 时屏幕上留下的恰好是恒定的 `<cache>/updates` 前缀，**「下的是哪个包」那一半被永久裁掉**，
 * 对用户零信息量。
 *
 * 目录是固定的（`app_cache_dir()/updates`），版本号已单独在前，故文件名是这一行里唯一带信息的
 * 部分。分隔符按 `/` 与 `\` 双取（Windows 路径同样走这条渲染腿）。
 *
 * ⚠️ 🟢#9 已知显示层局限（登记不改）：posix 宿主上文件名**合法地**含 `\` 时，双取分隔符会把
 * `\` 当 Windows 路径分隔符切一刀，文件名前半被截掉。纯显示层、触发面极窄（资产名带反斜杠），
 * 接受；要修得按宿主平台选分隔符，路径事实从后端来，届时一并。
 */
export function doneSubject(version?: string, filePath?: string): string {
  const name = filePath?.split(/[/\\]/).pop();
  return [version, name].filter((s): s is string => !!s && s.trim() !== '').join(' · ');
}
