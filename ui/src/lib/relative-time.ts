/**
 * 「多久以前」的相对时间文案 —— **全应用共用一份档位**。
 *
 * # 为什么要抽出来
 *
 * 这套四档（刚刚 / N 小时前 / 昨天 / N 天前）此前在仓里存了**两份实现**：首页的「上次检测」
 * （`HomeScreen.fmtRelativeTime`，入参 epoch ms）与节点页订阅栏的「上次更新」
 * （`SubInfoBar.relativeTime`，入参 ISO 串）。同一个应用里两处新鲜度用不同粒度描述，
 * 会让用户以为它们量的是不同的东西 —— 而两份实现正是让阈值悄悄分叉的唯一必要条件。
 *
 * 分叉已经发生过一次，只是发生在**文案**这一侧而不是阈值：首页那份走 i18n 键，
 * 订阅栏那份把四句中文**写死在函数里** ⇒ 五个语种的用户在节点页都看到中文。
 * （`i18n-coverage.test.ts` 的 G1 一直把它记作 4 条裸 CJK 债务。）
 *
 * # 入参统一成 epoch ms
 *
 * 两个调用点的时间源形态不同（一个 epoch ms、一个 ISO 串），但「把 ISO 转成 ms」是调用点
 * 一行的事，而「四档阈值」是需要单一真值的领域规则。故这里只收 ms，转换留在调用点。
 */

/** i18next 的 `t`（只用到 key + 插值两参形态，不引 react-i18next 的类型以便纯函数单测）。 */
export type Translate = (key: string, opts?: Record<string, unknown>) => string;

/**
 * `at`（epoch ms）距今多久。档位与阈值是契约的一部分，改动前先看两个调用点的截图对照。
 *
 * `now` 可注入，纯函数单测靠它固定时基。
 */
export function relativeTimeText(at: number, t: Translate, now: number = Date.now()): string {
  const diffH = (now - at) / 3_600_000;
  if (diffH < 1) return t('common.relJustNow');
  if (diffH < 24) return t('common.relHoursAgo', { count: Math.floor(diffH) });
  if (diffH < 48) return t('common.relYesterday');
  return t('common.relDaysAgo', { count: Math.floor(diffH / 24) });
}

/** ISO 串版本：解析失败（含 `undefined`）返回空串 —— 调用点按「没有这个时间」渲染。 */
export function relativeTimeTextIso(iso: string | undefined, t: Translate, now?: number): string {
  if (!iso) return '';
  const at = new Date(iso).getTime();
  if (Number.isNaN(at)) return '';
  return relativeTimeText(at, t, now);
}
