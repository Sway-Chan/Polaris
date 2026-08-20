/**
 * 「核在跑的真实日志级别」徽标的纯投影（Logs 屏用）。
 *
 * # 它现在管的是哪一格（2026-08-09 口径收窄，随核日志改吃 `SubscribeLog` 一并定案）
 *
 * 核日志改由管理 API 的 `SubscribeLog` 送达后，**本页显示的核日志已经不归它管**了：那条流恒是全级别，
 * 筛在客户端（`logging::set_level` 的 `max_level`），受管 `logs/singbox.log` 也由同一 sink 写，
 * 改完即刻跟上。于是这颗徽标唯一还管着的是**核在 pre-ready/helper stderr 使用的默认级别**。
 *
 * 它仍是起核问题的关键凭据：主 relay 尚未建立前、以及 helper 侧用于 FATAL 分类的 stderr，只有这一档。
 *
 * # 一致性：任一方向不同都要明示
 *
 * 这颗徽标不只是「导出会不会缺行」的警报，也是用户核对「我刚选的级别是否已进核」的
 * 唯一运行态凭据。因此 INFO → WARN 时核仍在 INFO，即便只是多记了几行，也必须标「待同步」；
 * 否则界面看起来就是无解释地比下拉选项慢一拍。显示层仅在分叉或读取失败时复用既有警示标签，
 * 相同 / 未运行 / 读取中不额外占位。
 *
 * # 为什么「读不到」不能回落成某个级别
 *
 * 核未运行时上游 `GetDefaultLogLevel` 必然报错（服务端先查 `serviceStatus.Status ∈
 * {STARTING, STARTED}`）。此时若回落成 `config.logLevel`，显示的恰恰又是那个「我写下的值」——
 * 自证退化成它本要揭穿的那句谎，只是换了个地方说。故 `level == null` 一律进 `notRunning` /
 * `unavailable` 两个明说「不知道」的态，**没有第三条回落路径**。
 *
 * # 隐私锁那条分叉去哪了（它不在这里，因为这里看不见它）
 *
 * 隐私锁开启时生成侧 `LogLevel::effective(privacy)` 把 info/debug 抬到 warn，核确实会与控件不一致 ——
 * 但 `privacyMode === true` 时 `LockOverlay` 是 `aria-modal` 的整窗遮罩（`layout/LockOverlay.tsx`），
 * **本页连同这颗徽标一起被盖住**。做一个只在看不见的时候才亮的状态，是自欺。故本模块不收隐私锁这一路
 * 输入；解锁后若核确实是按抬级后的值起的，它照常落进 `coreRestart`（「核是按旧级别起的」）——
 * 那句话对那个状态同样成立。
 *
 * 顺带记一条**曾被误判成缺口、已撤回**的事实，免得下一轮再走一遍：上锁前起的那个核会继续按
 * info/debug 把连接明细写进 helper stderr（`config_set_privacy_mode` 只翻进程内标志位，既不重生成
 * 配置也不重启核），而管理 API 只有 `GetDefaultLogLevel`、**没有 setter**（上游
 * `daemon/started_service.proto` 全表 46 个 rpc 核对过）⇒ 除重启核外无第二条收紧路径。
 *
 * 这**不是**缺口。判据是隐私锁自己写给用户的那两句话，不是谁的推断：
 * `logs.privacyNote`「隐私锁开启中 — **日志流**已对域名与 IP 脱敏」（射程 = 日志流，从来不含盘上文件）、
 * `privacy.subtitle`「输入密码解锁。**代理仍在运行。**」。后一句直接否掉「上锁时重启核」这条补法 ——
 * 那会断掉全部连接，与 app 自己承诺的相反。盘上那份从不在锁的承诺内。
 *
 * （`core_log_privacy_floor` 那道下限不受本条影响：它挡的是「我们自己把核连接明细**新引入**到
 * 受管日志」这种自伤回归，与锁的承诺射程无关。）
 *
 * # 为什么做成纯函数而不是写在组件里
 *
 * 「不回落」与「任一方向不同都明示」都是**不变量**，不是渲染细节；它们得有能单独变异验证的判据
 * （见 `runtime-level.test.ts`）。混在 JSX 里只能靠 review 记得。
 */

import type { LogLevel, RuntimeLogLevel } from '@/contracts/types';

/**
 * 分叉的成因 —— 两者的**补救动作不同**，故不能合并成一个 boolean。
 *
 * - `unsaved`：级别改动还在暂存区（盘上仍是旧值）。补救 = 应用 + 重启内核。
 * - `coreRestart`：改动已落盘，核是按旧级别起的。补救 = 重启内核。
 */
export type RuntimeLevelDrift = 'unsaved' | 'coreRestart';

/** 徽标的四个互斥态。`pending` = 还没拿到第一份回答（不显示任何东西，别急着说「读不到」）。 */
export type RuntimeLevelView =
  | { kind: 'pending' }
  | { kind: 'notRunning' }
  | { kind: 'unavailable' }
  /** 读到了核在跑的级别。`drift` = 非 null 即分叉现形的时刻，取值说明见 [`RuntimeLevelDrift`]。 */
  | { kind: 'known'; level: string; drift: RuntimeLevelDrift | null };

/**
 * 把后端回答 + 控件当前显示值 + 盘上值，投影成徽标要呈现的态。
 *
 * @param resp 后端 `logs:runtimeLevel` 的回答；`null` = 尚未取回（首次渲染 / IPC 抛错后仍留空）。
 * @param shown 级别分段控件此刻高亮的那个值（= staged 合并后「我写下的值」）。
 * @param savedLevel **盘上**那份 config 的 `logLevel`（非 staged 合并值）。`null` = 还没水合 ⇒
 *   判不出「暂存未应用」，此时一律归 `coreRestart`（不猜）。
 */
export function runtimeLevelView(
  resp: RuntimeLogLevel | null,
  shown: LogLevel,
  savedLevel: LogLevel | null,
): RuntimeLevelView {
  if (!resp) return { kind: 'pending' };
  if (resp.level === null || resp.level === undefined) {
    // 后端只有两种「读不到」的理由；出现第三种（或字段缺失）也一律按 unavailable 呈现 ——
    // 宁可说「读不到」，也不能悄悄编一个级别出来。
    return resp.reason === 'notRunning' ? { kind: 'notRunning' } : { kind: 'unavailable' };
  }
  return { kind: 'known', level: resp.level, drift: driftOf(resp.level, shown, savedLevel) };
}

/** 「改动还在暂存区」= staged 合并后的值与盘上的值不一致。 */
function pendingCause(shown: LogLevel, savedLevel: LogLevel | null): RuntimeLevelDrift {
  return savedLevel !== null && savedLevel !== shown ? 'unsaved' : 'coreRestart';
}

function driftOf(
  core: string,
  shown: LogLevel,
  savedLevel: LogLevel | null,
): RuntimeLevelDrift | null {
  if (core === shown) return null;
  return pendingCause(shown, savedLevel);
}
