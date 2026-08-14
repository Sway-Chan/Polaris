/**
 * SettingsAbout —— 关于子页（原型 [data-sec="about"] L2423-2440）。
 *
 * 两块：
 *  1. about 卡：logo 磁贴 + Polaris + 版本号 + tagline + 描述 + 链接（发行版/反馈/开源许可/第三方声明）
 *     + 零遥测声明
 *  2. danger-zone：自卸载（appApi.uninstallAll，原地二次点击确认）
 *
 * 版本号走 versionApi.getInfo() 真值；卸载前经 `useConfirmTwice`（`lib/confirm-twice.ts`）原地二次
 * 点击确认，1:1 对齐原型 `uninstallApp()` :5185 —— 不弹窗、不依赖 `dialog:allow-confirm` 授权。
 * 「导出诊断报告」不在原型 about 里（诊断导出属于 Logs 屏 diagnostic.export 的接线面，见批 C3），此处不重复加。
 */

import { useEffect, useState, type MouseEvent } from 'react';
import { useTranslation } from 'react-i18next';
import {
  versionApi,
  appApi,
  type VersionInfo,
  type UninstallReport,
  type UninstallStep,
  type UninstallOutcomeKind,
} from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import { Phead, CardSub, Button } from './primitives';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { invoke } from '@/ipc/ipc-client';
import { IPC_CHANNELS } from '@/domain/ipc-channels';

/** 本页唯一的原地二次确认项（原型 :5185 `uninstallApp`）。 */
const UNINSTALL_KEY = 'uninstall-app';
/** 卸载成功后到自动退出之间的停顿：够看清成功 toast，又不至于让人以为卡住了。 */
const UNINSTALL_EXIT_DELAY_MS = 1500;

/** 步骤 → i18n key（顺序即后端的因果执行序，渲染时按后端返回的数组序，不在前端重排）。 */
const STEP_KEY: Record<UninstallStep, string> = {
  stopCore: 'settings.about.uninstallStepStopCore',
  autostart: 'settings.about.uninstallStepAutostart',
  helper: 'settings.about.uninstallStepHelper',
  userConfig: 'settings.about.uninstallStepUserConfig',
  cacheDir: 'settings.about.uninstallStepCacheDir',
  preferences: 'settings.about.uninstallStepPreferences',
  appBundle: 'settings.about.uninstallStepAppBundle',
};

/**
 * 结果态 → i18n key + 配色。
 *
 * `skipped`（本就无事可做）用中性色而非成功色：它不是「做成了」。
 * `unsupported` / `notAttempted` 用警示色 —— 用户必须看见「这项没做」，而不是扫一眼绿就走。
 */
const KIND_META: Record<UninstallOutcomeKind, { key: string; tone: string }> = {
  done: { key: 'settings.about.uninstallKindDone', tone: 'hsl(var(--ok))' },
  skipped: { key: 'settings.about.uninstallKindSkipped', tone: 'hsl(var(--fg-dim))' },
  unsupported: { key: 'settings.about.uninstallKindUnsupported', tone: 'hsl(var(--warn))' },
  failed: { key: 'settings.about.uninstallKindFailed', tone: 'hsl(var(--err))' },
  notAttempted: { key: 'settings.about.uninstallKindNotAttempted', tone: 'hsl(var(--warn))' },
};

const VERDICT_META = {
  complete: { key: 'settings.about.uninstallVerdictComplete', tone: 'hsl(var(--ok))' },
  incomplete: { key: 'settings.about.uninstallVerdictIncomplete', tone: 'hsl(var(--warn))' },
  failed: { key: 'settings.about.uninstallVerdictFailed', tone: 'hsl(var(--err))' },
} as const;

/** 仓库根的 LICENSE（MIT，Polaris 本体）与 NOTICE（第三方组件及其许可）。 */
const LICENSE_URL = 'https://github.com/Sway-Chan/polaris/blob/main/LICENSE';
const NOTICE_URL = 'https://github.com/Sway-Chan/polaris/blob/main/NOTICE';

export default function SettingsAbout() {
  const { t } = useTranslation();
  const [info, setInfo] = useState<VersionInfo | null>(null);
  const [uninstalling, setUninstalling] = useState(false);
  const [report, setReport] = useState<UninstallReport | null>(null);
  const { armed, confirmTwice } = useConfirmTwice();
  const confirmingUninstall = armed === UNINSTALL_KEY;

  useEffect(() => {
    void versionApi.getInfo().then(setInfo).catch(() => undefined);
  }, []);

  async function openExternal(url: string, e: MouseEvent) {
    e.preventDefault();
    try {
      const { systemApi } = await import('@/ipc/api-client');
      await systemApi.openExternal(url);
    } catch {
      window.open(url, '_blank', 'noopener');
    }
  }

  /**
   * 自卸载：**原地二次点击**确认 → appApi.uninstallAll() → **逐项**呈现结果。
   *
   * 确认形态 1:1 对齐原型 `uninstallApp()`（:5185）：`confirmTwice(t, '再次点击确认卸载（不可逆）', …)`
   * —— 不弹窗、不申请 `dialog:allow-confirm`，2.6s 未二次点击自动复位。此前这里走的是自绘
   * `ConfirmDialog` 的两段 prompt，与原型、与本仓其余破坏性操作（清空日志 / 关闭全部连接）三方分裂。
   *
   * 「会删什么 + 不可恢复」不再靠弹窗一次性告知，而是**常驻**在按钮左侧的 `uninstallDesc` 里
   * （由 `SettingsAbout.uninstall-honesty.test.ts` 逐项钉死）—— 从「点开才看得见」变成「一直看得见」。
   *
   * ⚠️ **「没抛异常」不等于卸载成功**。后端外层恒 `success:true`（否则 `ipc-client` 会 throw 并把
   * 逐项报告一起丢掉），真值在 `report.verdict`：只有 `complete` 才配显示成「已卸载」。
   * 这里据此选 toast 档位，明细整块渲染在 danger-zone 里 —— 一句「已卸载」正是本功能最不该说的话。
   */
  function uninstall() {
    confirmTwice(UNINSTALL_KEY, () => {
      void (async () => {
        setUninstalling(true);
        setReport(null);
        try {
          const r = await appApi.uninstallAll();
          setReport(r);
          const title = t(VERDICT_META[r.verdict].key);
          if (r.verdict === 'complete') {
            toast.success(title);
            // 卸载干净之后**主动退出**（陈先生 2026-07-29 真机报：卸完窗口还在，要手动退，不合理）。
            // 只在 `complete` 退：其余 verdict 的逐项报告正是留给用户看「哪几项没删掉」的，
            // 退掉等于把唯一的诊断信息一起关了。
            // 留 `UNINSTALL_EXIT_DELAY_MS` 让成功 toast 有机会被看到；走 `tray_quit` 而不是自己拼一条
            // 退出腿 —— 它已经处理了 `QuitState`（正常退出标记）与 `app.exit(0)` 的顺序。
            setTimeout(() => {
              void invoke(IPC_CHANNELS.TRAY_QUIT).catch(() => {});
            }, UNINSTALL_EXIT_DELAY_MS);
          } else toast.error(title, t('settings.about.uninstallResultTitle'));
        } catch (err) {
          // 调用处是 `onClick={uninstall}`，返回值被丢弃 ⇒ 不在这里 catch 就是没人能接的
          // promise rejection，用户零反馈。
          toast.error(
            t('settings.about.uninstallFail'),
            err instanceof Error ? err.message : String(err),
          );
        } finally {
          setUninstalling(false);
        }
      })();
    });
  }

  return (
    <section className="screen" data-sec="about">
      <Phead title={t('settings.nav.about')} />

      <div className="card about">
        <div className="about-mk">
          <svg viewBox="-46 -46 92 92">
            <use href="#polarisStar" />
          </svg>
        </div>

        <h2>Polaris</h2>
        <div className="ver">
          v{info?.appVersion ?? '0.1.0-alpha'}
          {info?.singBoxVersion ? ` · sing-box ${info.singBoxVersion}` : ''}
        </div>

        <p className="tagline">{t('settings.about.tagline')}</p>
        <p className="about-desc">{t('settings.about.intro')}</p>

        <div className="about-links">
          <a
            className="about-link"
            href="https://github.com/Sway-Chan/polaris/releases"
            target="_blank"
            rel="noopener"
            onClick={(e) => void openExternal('https://github.com/Sway-Chan/polaris/releases', e)}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 3v11M8 10l4 4 4-4M4 19h16" />
            </svg>
            <span>{t('settings.about.releases')}</span>
          </a>
          <a
            className="about-link"
            href="https://github.com/Sway-Chan/polaris/issues"
            target="_blank"
            rel="noopener"
            onClick={(e) => void openExternal('https://github.com/Sway-Chan/polaris/issues', e)}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <circle cx="12" cy="12" r="9" />
              <path d="M12 8v5M12 16h.01" />
            </svg>
            <span>{t('settings.about.reportIssue')}</span>
          </a>
          {/* 开源许可入口：仓库根有 LICENSE（MIT，本体）与 NOTICE（以子进程/二进制形式集成的第三方
              组件及其许可，含 GPLv3 的 sing-box）。此前 about 页零许可入口 —— 一个分发中的开源客户端
              不给用户看许可，是合规缺口而不只是排版缺项。用 openExternal 走系统浏览器，与上面两条同款。 */}
          <a
            className="about-link"
            href={LICENSE_URL}
            target="_blank"
            rel="noopener"
            onClick={(e) => void openExternal(LICENSE_URL, e)}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M6 3h9l4 4v14H6z" />
              <path d="M9 12h6M9 16h4" />
            </svg>
            <span>{t('settings.about.license')}</span>
          </a>
          <a
            className="about-link"
            href={NOTICE_URL}
            target="_blank"
            rel="noopener"
            onClick={(e) => void openExternal(NOTICE_URL, e)}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <circle cx="12" cy="12" r="9" />
              <path d="M12 11v5M12 8h.01" />
            </svg>
            <span>{t('settings.about.notice')}</span>
          </a>
        </div>

        <CardSub style={{ marginTop: 18 }}>{t('settings.about.noTelemetry')}</CardSub>
      </div>

      {/* ── danger-zone：完全卸载 ───────────────────────────────────────────────────
          入口位置沿用原型 [data-sec="about"] L2437-2438 的 danger-zone（`data-act="uninstall-app"`，
          文案「移除提权助手、用户数据与内核。需双重确认」）。

          确认方式 = 原型的 `confirmTwice`（同一颗按钮点两下、2.6s 超时复位）。2026-07-29 从自绘
          `ConfirmDialog` 两段 prompt 改回来：判据已切成「原型 ↔ 后端双向对拍」，而「本仓先例」是
          自我循环的理由。「会删什么 + 不可恢复」改由**常驻**的 `uninstallDesc` 承载（就在按钮左边，
          不点也看得见），比藏在弹窗里更早触达；`SettingsAbout.uninstall-honesty.test.ts` 逐项钉死它。
          ⚠️ 完全卸载的 blast radius 与「清空日志」不在一个量级，是否额外加重一道闸是**待裁定项**，
          本轮只消差异、不自行加码。

          结果**逐项呈现**而非一句「已卸载」：四类目标里每一类都可能独立成功/跳过/不支持/失败，
          尤其应用本体在 Windows 便携版与 Linux 包管理器安装下是**做不到**的（后端如实返
          `unsupported`）。把它糊成一句成功，就是这个功能此前被判定为「名不副实」的那个形态。 */}
      <div className="danger-zone">
        <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <div style={{ flex: 1 }}>
            <b style={{ fontSize: 13, color: 'hsl(var(--err))' }}>
              {t('settings.about.uninstallTitle')}
            </b>
            <CardSub>
              {t('settings.about.uninstallDesc')}
            </CardSub>
          </div>
          {/* 复用 primitives.tsx 的 Button（而非原生 <button className="btn">）——Button 内部已处理
              disabled+title 的 hover 命中修复，原生 button 单独维护一份是重复劳动。 */}
          <Button
            variant="ghost"
            size="sm"
            className={confirmingUninstall ? 'confirming' : undefined}
            style={{ color: 'hsl(var(--err))', borderColor: 'hsl(var(--err)/0.3)' }}
            onClick={uninstall}
            disabled={uninstalling}
          >
            {/* 确认态换按钮文案（原型 confirmTwice 换的就是 `<span>` 的 textContent），
                `.confirming` 类负责翻红实心（components.css `.btn.confirming`）。 */}
            <span>
              {uninstalling
                ? t('settings.about.uninstallInProgress')
                : confirmingUninstall
                  ? t('settings.about.uninstallConfirmAgain')
                  : t('settings.about.uninstallAction')}
            </span>
          </Button>
        </div>

        {report && (
          <div style={{ marginTop: 14 }} data-testid="uninstall-report">
            <b style={{ fontSize: 13, color: VERDICT_META[report.verdict].tone }}>
              {t(VERDICT_META[report.verdict].key)}
            </b>
            <ul style={{ margin: '8px 0 0', padding: 0, listStyle: 'none' }}>
              {/* 按后端返回的数组序渲染 —— 那就是因果执行序，前端不重排、不筛掉任何一项。
                  「未执行」的项**必须**也显示：它是 fail-fast 生效的证据，藏起来等于假装没这回事。 */}
              {report.steps.map((s) => (
                <li key={s.step} style={{ display: 'flex', gap: 8, padding: '4px 0' }}>
                  <span style={{ flex: '0 0 auto', color: KIND_META[s.outcome.kind].tone }}>
                    {t(KIND_META[s.outcome.kind].key)}
                  </span>
                  <span style={{ flex: 1, minWidth: 0 }}>
                    <span>{t(STEP_KEY[s.step])}</span>
                    {/* detail 由后端给（含真实路径 / 失败原因 / 为什么本平台做不到），不在前端复述 */}
                    <CardSub style={{ wordBreak: 'break-word' }}>{s.outcome.detail}</CardSub>
                  </span>
                </li>
              ))}
            </ul>
            {report.requiresExit && (
              <CardSub style={{ marginTop: 10, color: 'hsl(var(--warn))' }}>
                {t('settings.about.uninstallExitHint')}
              </CardSub>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
