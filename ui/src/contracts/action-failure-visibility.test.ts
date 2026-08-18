/**
 * 「点击无反应」类的防回潮门（W10 / W14 / W15，2026-08-19 真机首曝后的修复批）。
 *
 * # 它守的是什么
 *
 * 三个真机缺陷同根：**失败被静默吞掉，用户只看到按钮点下去毫无反应**——
 *  - W10：`helper_install` 的 Rust 侧把所有失败（用户取消/脚本失败/二进制缺失）装进
 *    `success:false` 的 ok 应答；前端只读 `r.status` 不读 `r.success` ⇒ 信封永远不 reject，
 *    `catch` 等不到它，失败零反馈。真机取证：连点多次安装，一个 powershell 都没拉起（失败在
 *    早退分支），UI 一声不吭。
 *  - W14：托盘浮层所有动作 `invoke(...).catch(() => {})` 或空 `catch {}`；旧注释的
 *    「错误经主窗呈现」在主窗关闭（托盘常驻）时整条失效——那恰是托盘最常用的形态。
 *  - W15：全部节点视图零节点时内容高度塌缩，菜单「缩成一条」。
 *
 * # 为什么是源扫描而不是组件测试
 *
 * 本仓 vitest 是 node 环境、无 jsdom（既定取舍，见 settings-logic 等纯逻辑测试的组织方式），
 * 组件交互行为测不到；源扫描契约是仓内对这类缺口的既定补偿模式（同 `ci-paths-contract.test.ts`
 * 扫 workflow 文本）。判据全部取**精确字面量**（channel 名 + `.catch(() => {})` 形态、
 * `noticeActionFailure` 计数、`!r.success` 计数），不搞模糊匹配——模糊了就没牙。
 *
 * # 这门抓不到什么（别当成「组件行为已验证」）
 *
 * - handler 是否真的挂在按钮上、notice 是否真的渲染——需要组件/真机验证；
 * - 失败路径「不关浮层」的语义——只钉了 notice 调用存在，没钉控制流。
 * 真机收据由部署批次补（2026-08-19 起真机自动化验证已授权）。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

const read = (rel: string): string => readFileSync(join(REPO_ROOT, rel), 'utf8');

const TRAY_MENU = 'ui/src/tray/TrayMenu.tsx';
const SETTINGS_HELPER = 'ui/src/components/screens/settings/SettingsHelper.tsx';
const TRAY_CSS = 'ui/src/tray/tray-overlay.css';

/** 用户可感知的动作 channel——它们的 invoke 失败必须有回显，禁 `.catch(() => {})`。
 * （TRAY_HIDE / TRAY_RESIZE 是窗口管线自检，失败不属于「用户动作失败」，不在射程。） */
const ACTION_CHANNELS_BANNED_SWALLOWS = [
  'TRAY_SHOW_MAIN).catch(() => {})',
  'TRAY_ENTER_LIGHTWEIGHT).catch(() => {})',
  'TRAY_QUIT).catch(() => {})',
] as const;

/** TrayMenu 里 `noticeActionFailure` 的调点数：定义 1 + 八个动作 handler（六改 + lockNow/onSpeedTest）+ 五个按钮包裹。 */
const NOTICE_CALL_SITES = 14;

/** lockNow 的顺序语义（Med-1）：必须「await 成功 → 才 hide」。先 hide 再 await 的旧序在失败时
 * 连 notice 都无处显示。以 indexOf 顺序钉住——重排回去（hide 在 await 之前）即红。 */
const LOCKNOW_AWAIT_NEEDLE = 'await api.config.setPrivacyMode(true);';
const LOCKNOW_HIDE_NEEDLE = 'hide();';

describe('点击类失败必须可见（W10/W14/W15 防回潮）', () => {
  it('托盘动作 channel 上不得再有静默吞错', () => {
    const src = read(TRAY_MENU);
    for (const needle of ACTION_CHANNELS_BANNED_SWALLOWS) {
      expect(
        src.includes(needle),
        `${TRAY_MENU} 出现 \`${needle}\` —— 动作失败又变回零反馈的「点击无反应」（W14）`,
      ).toBe(false);
    }
  });

  it('noticeActionFailure 调点数固定（新增动作必须同样接失败回显）', () => {
    const src = read(TRAY_MENU);
    const count = src.split('noticeActionFailure').length - 1;
    expect(
      count,
      `noticeActionFailure 应恰有 ${NOTICE_CALL_SITES} 处（含定义）。少于它 = 有动作漏接；` +
        `多于它 = 改了动作面，本门计数与动作清单需同步审`,
    ).toBe(NOTICE_CALL_SITES);
  });

  it('lockNow 必须「await 成功才 hide」——先关浮层再试锁定，失败时 notice 无处显示（Med-1）', () => {
    const src = read(TRAY_MENU);
    const awaitAt = src.indexOf(LOCKNOW_AWAIT_NEEDLE);
    const lockNowStart = src.indexOf('const lockNow');
    const lockNowEnd = src.indexOf('};', lockNowStart);
    expect(awaitAt, 'lockNow 里的 setPrivacyMode 调用形态变了（await 不在了？）').toBeGreaterThan(-1);
    const hideAt = (() => {
      const region = src.slice(lockNowStart, lockNowEnd);
      const at = region.indexOf(LOCKNOW_HIDE_NEEDLE);
      return at === -1 ? -1 : lockNowStart + at;
    })();
    expect(hideAt, 'lockNow 里没有 hide()——顺序判据失效，需同步更新').toBeGreaterThan(-1);
    expect(
      awaitAt < hideAt,
      'lockNow 的 hide() 又跑到了 await 之前：失败时浮层已收起、隐私态没进、notice 无处显示',
    ).toBe(true);
  });

  it('全部节点视图有零节点空态（W15：菜单不得塌成一条）', () => {
    const src = read(TRAY_MENU);
    expect(src.includes('tray-nodes-empty'), 'TrayMenu 缺空态节点').toBe(true);
    expect(src.includes('groups.length === 0'), '空态判据不在 groups 上——改了数据源就漏判').toBe(true);
    const css = read(TRAY_CSS);
    expect(css.includes('.tray-nodes-empty'), 'CSS 缺 .tray-nodes-empty').toBe(true);
    expect(css.includes('min-height'), '空态没有最小高度兜底，高度仍会塌缩').toBe(true);
  });

  it('SettingsHelper 必须消费 helper 信封的 success/error（W10）', () => {
    const src = read(SETTINGS_HELPER);
    const count = src.split('!r.success').length - 1;
    expect(count, 'install 与 uninstall 两条腿各需一处 !r.success 判定').toBe(2);
    expect(
      src.includes("toast.error(t('helper.installFail'), r.error)"),
      '安装失败未走 toast 门面（复用既有 toast，不新造回显通道）',
    ).toBe(true);
    expect(
      src.includes("toast.error(t('helper.uninstallFail'), r.error)"),
      '卸载失败未走 toast 门面',
    ).toBe(true);
  });

  it('五个辅助语种的 actionFailed 都带双插值占位（i18n 零硬编码的落点）', () => {
    for (const lang of ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa']) {
      const rel = `ui/src/i18n/locales/auxiliary/${lang}.json`;
      const tray = JSON.parse(read(rel)).tray as Record<string, string>;
      expect(tray.actionFailed, `${rel} 缺 tray.actionFailed`).toBeTruthy();
      expect(
        tray.actionFailed.includes('{{action}}') && tray.actionFailed.includes('{{detail}}'),
        `${rel} 的 actionFailed 丢了插值占位，notice 会渲染出原始模板`,
      ).toBe(true);
    }
  });
});
