/**
 * 测速反馈文案（Home / Nodes 共用，纯函数）。
 *
 * 两屏都调 `api.server.speedTest`，反馈口径必须一致——否则同一个后端 code 在两处显示成不同说法，
 * 用户会以为是两种毛病。抽这里而非各屏内联，也让 vitest（node 环境，不引 jsdom）能直接测。
 */

import type { TFunction } from 'i18next';
import { IpcError } from '@/ipc';
import type { SpeedTestInvokeResult } from '@/contracts/speed-test';
import type { SpeedTestBlockReason } from '../nodes/nodes-logic';

/**
 * 测速失败 → 展示文案。按后端结构化 code 分流：笼统报「测速失败」会让用户以为节点坏了，
 * 而真实原因是「本轮测不了这些节点」（见 `commands/speedtest.rs` 的 `SpeedTestPlan` 边界登记）。
 *
 * `SPEEDTEST_PROBE_POOL_UNWIRED` 的**语义已变**（码名是历史遗留）：探针池早已接线
 * （`run_pool_speed_test` 分波批量测速是常规路径），该码现在只在**本次起核时池端口分配失败已回退**
 * 的降级态下出现 ⇒ 文案必须说成「本次不可用」的暂态，不能说成「产品只支持测当前节点」的常态。
 */
export function speedTestErrorMessage(err: unknown, t: TFunction): string {
  const code = err instanceof IpcError ? err.code : undefined;
  switch (code) {
    case 'SPEEDTEST_NO_ACTIVE_EXIT':
      return t('nodes.speedTestNoActiveExit', '当前出口为直连或未选节点，无可测出站');
    case 'SPEEDTEST_PROBE_POOL_UNWIRED':
      return t(
        'nodes.speedTestOnlyActive',
        '测速探测池本次不可用（端口分配失败已回退），仅当前出口可测'
      );
    default:
      return err instanceof Error ? err.message : String(err);
  }
}

/**
 * 本波「请求了但没测」的节点 → 提示文案；全测到则返 null（不打扰）。
 *
 * # 两类缺席**分开报**，不再合计（2026-07-31 修）
 *
 * 后端把它们分成两个键，因为它们是不同的物理事实、有**不同的修法**：
 *  - `notInPool`（`commands/speedtest.rs::partition_pool`）= 不在**运行核**的测速池里（订阅新增/改址后
 *    没重启核 ⇒ 其出站 tag 不是 `probe-selector-k` 成员）→ 修法是重启内核纳入；
 *  - `tsNotReady`（同文件 `partition_ts_not_ready`，判据 `ts_node_ready`）= 协议为 tailscale 但**尚未登录
 *    就绪**。此时运行核对该出口已让位直连（`login_fallback`），测它量到的是直连 RTT ⇒ 波前缺席 →
 *    修法是**去登录那个节点**。
 *
 * 合计成一条会把 TS 未登录说成「未入运行核测速池，重启内核后纳入」—— 用户照着去重启内核，重启完照旧，
 * 因为真正缺的是登录。本函数此前正是这么写的（连带一条注释断言 `tsNotReady` 判据「至今未接线、
 * `run_pool_speed_test` 恒返空数组」，那已不成立：`server_speed_test` 在
 * `commands/speedtest.rs` 里以 `ts_pending` 实参喂进 `partition_pool`，该列表在真机上非空）。
 *
 * 两类并存 → 两句都报（只报一半会让用户按错误的修法折腾，与后端 `zero_testable_envelope` 的
 * 「每一类非零的数都报」同口径）。`dirty`（已编辑未生效）后端另有独立键，渲染端尚未接线 ——
 * 同一事实由 Home 的「N 项待应用」操作条承载，故非静默，登记为已知残留。
 */
export function notInPoolMessage(
  r: Pick<SpeedTestInvokeResult, 'notInPool' | 'tsNotReady'>,
  t: TFunction
): string | null {
  const parts: string[] = [];
  const notInPool = r?.notInPool?.length ?? 0;
  const tsNotReady = r?.tsNotReady?.length ?? 0;
  if (notInPool > 0) {
    parts.push(
      t('nodes.speedTestSkipped', {
        count: notInPool,
        defaultValue: '{{count}} 个节点未纳入本次测速（未入运行核测速池，重启内核后纳入）',
      })
    );
  }
  if (tsNotReady > 0) {
    parts.push(
      t('nodes.speedTestSkippedTsNotReady', {
        count: tsNotReady,
        defaultValue: '{{count}} 个 Tailscale 节点未纳入本次测速（尚未登录就绪，登录后可测）',
      })
    );
  }
  return parts.length > 0 ? parts.join('\n') : null;
}

/**
 * 不可测原因码 → 已本地化说明。**Home / Nodes 共用同一份措辞**。
 *
 * 原本只在 `NodesScreen` 内联（挂灰 ⚡ 的 tooltip）。首页「网络检测」改成只测当前出口后，
 * 它在「当前出口结构上不可测」时同样要把原因讲出来（不然按钮点下去毫无动静，与失灵无从区分）——
 * 两处若各写一套 switch，同一个 `ts-no-exit` 会在 tooltip 和 toast 里说成两种话。
 *
 * **逐分支静态 `t('...')`，不做键名拼接**：i18n 的可寻址性门（`locale-parity.test.ts::extractTKeys`）
 * 只扫得到字面量键，动态拼键会绕过它、把缺译留到运行期才发现。
 */
export function speedTestBlockedMessage(reason: SpeedTestBlockReason, t: TFunction): string {
  switch (reason) {
    case 'staged-only':
      return t(
        'nodes.speedTestBlockedStagedOnly',
        '该节点还没保存到配置文件：运行核里没有它，测不出真实延迟。点条上的「保存」后即可测速'
      );
    case 'system-interface':
      return t(
        'nodes.speedTestBlockedSystem',
        '该节点绑定 System 内核接口：测速会走系统默认路由，测出的是直连的假数值，故不可测'
      );
    case 'ts-no-exit':
      return t(
        'nodes.speedTestBlockedTsNoExit',
        'Tailscale 未设置出口节点（Exit Node）：没有公网出口，测速必然超时'
      );
    case 'lan-only':
      return t(
        'nodes.speedTestBlockedLanOnly',
        '该组网节点关闭了「允许访问外网」：只承载内网网段，公网测速必然超时'
      );
    case 'ts-core-not-ready':
      return t(
        'nodes.speedTestBlockedTsCoreNotReady',
        'Tailscale 出口测速需要代理正在运行（主核测速池），请先连接再测'
      );
    case 'custom-endpoint':
      return t(
        'nodes.speedTestBlockedCustomEndpoint',
        '自定义端点的原始 JSON 判不出出口形态，测速结果不可信，故不可测'
      );
    default:
      return t('nodes.speedTestNotApplicable', '不支持测速');
  }
}
