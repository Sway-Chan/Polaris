/**
 * AppUpdateBanner —— 应用（Polaris 本体）新版本的**常驻横幅**。
 *
 * 契约要求（`polaris-上游-capability-contract.md:126`，About 段）：
 * 「App 更新 —— `update:check`→`download`(进度)→`install`/`skip`；**常驻 banner + 独立 mini 更新窗**」。
 * mini 更新窗此前已移植（`update-popup/` + `runtime/update_popup.rs`，启动 5s 自动检查后弹），
 * 常驻 banner 一直缺 —— `components/layout/*` 此前零 update 引用，用户关掉那个 mini 窗之后，
 * 主窗里**再也没有任何「有新版本」的痕迹**，只能自己想起来去设置页点检查。本组件补的就是这一条。
 *
 * # 挂载位置
 *
 * `AppShell` 的 `.main-chrome` 与 `.main-scroll` 之间 —— 在滚动区**之外**，故切换任何屏幕、
 * 滚到任何位置它都在（这是「常驻」的字面意思）。挂进 `.main-scroll` 会随内容滚走。
 *
 * # 数据来源与「只查一次」
 *
 * 直接消费 `update_check`（后端已按 `skippedVersion` 过滤，见 `commands/updater.rs:248`）。
 * 每会话只查一次（`checkedRef` 闸门）：后端 `startup_tasks` 的 5s 自动检查腿只把结果送进 mini 窗、
 * 不广播给主窗，主窗要拿到「有没有新版本」这个事实只能自己查一次；查两次以上纯属浪费 GitHub 配额。
 * 且受 `autoCheckUpdate` 门控（与后端 `should_auto_check_update` 同口径）—— 用户关掉「启动时检查更新」
 * 就不该被这条横幅偷偷发一次请求。
 *
 * 判定逻辑抽在 `./app-update-banner.ts`，由单测锁死（本仓 vitest 无 jsdom，组件本身测不了）。
 */

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { updateApi } from '@/ipc/api-client';
import { useEffectiveConfig } from '@/store/app-store';
import { useNavStore } from '@/store/nav-store';
import { Button } from '../screens/settings/primitives';
import {
  appUpdateBannerState,
  dismissedAppVersions,
  markAppVersionDismissed,
  markAppVersionSkipped,
  shouldBannerCheckUpdate,
  skippedAppVersions,
  subscribeAppVersionSkipped,
  type AppUpdateSnapshot,
} from './app-update-banner';

export default function AppUpdateBanner() {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<AppUpdateSnapshot | null>(null);
  // 「忽略」是**按版本**的会话态（模块级），不是组件 state —— 组件 state 随重挂丢掉，
  // 表现为「点了忽略、切个屏它又回来了」，而相邻的「跳过此版本」是持久的：两颗按钮语义强度
  // 差一个数量级、UI 上却没有任何提示，用户只会判定「忽略坏了」。订阅共用下面那条 bump 通道。
  // 会话级「已跳过」集合是模块态（设置页更新卡也会写它），订阅后靠这个计数触发重渲。
  const [, bumpSkipped] = useState(0);
  const checkedRef = useRef(false);
  const autoCheckUpdate = useEffectiveConfig((c) => c?.autoCheckUpdate);
  const configLoaded = useEffectiveConfig((c) => c !== null);
  const enterSettings = useNavStore((s) => s.enterSettings);

  useEffect(() => subscribeAppVersionSkipped(() => bumpSkipped((n) => n + 1)), []);

  useEffect(() => {
    // config 未载入前不查（`shouldBannerCheckUpdate(null)` 恒 false）；载入后按 autoCheckUpdate 判。
    if (!shouldBannerCheckUpdate(configLoaded ? { autoCheckUpdate } : null)) return;
    // 每会话一次。StrictMode 下 effect 会跑两遍，ref 闸门保证只发一次请求。
    if (checkedRef.current) return;
    checkedRef.current = true;
    // include_prerelease=false：与后端自动检查腿同口径 —— 预发布只在用户手动到更新页时才查。
    void updateApi
      .check(false)
      .then((r) =>
        setSnapshot({ hasUpdate: r.hasUpdate, version: r.updateInfo?.version ?? null }),
      )
      // 失败**保持不渲染**（snapshot 留 null）：后台检查失败不该抢用户注意力，
      // 与后端 `spawn_auto_check_update` 的「失败只记日志」同取向。真要看错因走设置页手动检查。
      .catch(() => undefined);
  }, [configLoaded, autoCheckUpdate]);

  const state = appUpdateBannerState({
    snapshot,
    skipped: skippedAppVersions(),
    dismissed: dismissedAppVersions(),
  });
  if (!state.visible || !state.version) return null;
  const version = state.version;

  return (
    <div className="app-update-banner" id="app-update-banner" role="status">
      <span className="aub-ic" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 3v11M8 10l4 4 4-4M4 19h16" />
        </svg>
      </span>
      <div className="aub-tx">
        <b>{t('settings.update.bannerTitle', { version })}</b>
        <span>
          {t('settings.update.bannerDesc')}
        </span>
      </div>
      {/* 主动作只做**导航**，不在横幅里直接起下载：下载/安装是有进度、有 advisory 二次确认、
          有便携版手动替换分支的一整套状态机（`SettingsUpdate` 的 7 态），横幅这么窄的条塞不下，
          塞一半会让用户在没有进度与失败反馈的地方触发长耗时下载。 */}
      <Button variant="flow" size="sm" onClick={() => enterSettings('update')}>
        <span>{t('settings.update.bannerAction')}</span>
      </Button>
      <Button
        variant="ghost"
        size="sm"
        onClick={() => {
          // 与设置页「跳过此版本」同一条后端命令（持久化 skippedVersion，跨会话生效），
          // 外加本会话立刻隐藏 —— 否则点完按钮横幅还杵着，直到下次启动才消失。
          void updateApi.skip(version).catch((e: unknown) => {
            console.error('[update] skip failed:', e);
          });
          markAppVersionSkipped(version);
        }}
      >
        <span>{t('settings.update.bannerSkip')}</span>
      </Button>
      <Button
        variant="ghost"
        size="sm"
        className="aub-dismiss"
        onClick={() => markAppVersionDismissed(version)}
        data-tip={t('settings.coreVersion.dismiss')}
        aria-label={t('settings.coreVersion.dismiss')}
      >
        <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M18 6 6 18M6 6l12 12" />
        </svg>
      </Button>
    </div>
  );
}
