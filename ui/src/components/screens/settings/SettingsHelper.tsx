/**
 * SettingsHelper —— 提权助手子页（原型 [data-sec="helper"] L2371-2420）。
 *
 * 两块：
 *  1. helper-card：原型定义 5 态（installed / none / installing / needs-btm / needs-upgrade）+
 *     真实运行时另需的 3 态（checking / unsupported / needs-repair，原型没有对应模板——按同一视觉语言
 *     借用 none/needs-upgrade 的 markup 骨架续写，非另起设计）
 *  2. 回退方案：异常自动恢复（纯说明行，无状态徽章——理由见该段注释）
 *
 * 接 helperApi.getStatus / install / uninstall + helperApi.onUpgradeable。
 * 原型 .hc-plat 平台切换 tab（mac/win/lin）是静态 demo 用来一次性展示三平台外观的道具——
 * 真实 app 只有一个当前平台，不需要「切换看别的平台」，故不搬。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { helperApi } from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import type { HelperStatus } from '@/contracts/types/runtime';
import { Phead, SetBlock, SetRow, Pill, Dot, Button, Spinner } from './primitives';
import { useConfirmTwice } from '@/lib/confirm-twice';

/** 本页唯一的原地二次确认项（原型 :4234 `helper-uninstall`）。 */
const UNINSTALL_KEY = 'helper-uninstall';

/** Helper 卡片派生态（消费 HelperStatus）。 */
type HelperState =
  | 'checking'
  | 'unsupported'
  | 'installed'
  | 'none'
  | 'installing'
  | 'needs-upgrade'
  | 'needs-btm'
  | 'needs-repair';

/**
 * `status === null` 必须自成一态（'checking'），**不能并进 'unsupported'**：
 * null 出现在「首帧还没拉到」与「getStatus 失败」两种情况，把它们说成「本平台不支持」是编造事实
 * ——每个用户进这一页的第一帧都会看到一句关于自己系统的假话。
 */
function deriveState(s: HelperStatus | null): HelperState {
  if (!s) return 'checking';
  if (!s.supported) return 'unsupported';
  if (s.upgradeable) return 'needs-upgrade';
  // backgroundDisabled（macOS BTM 登录项被禁）比 needsRepair 更具体，需先判（contract: runtime.ts:167）。
  if (s.backgroundDisabled) return 'needs-btm';
  if (s.needsRepair) return 'needs-repair';
  if (s.ready) return 'installed';
  if (s.installed) return 'needs-repair';
  return 'none';
}

export default function SettingsHelper() {
  // 卡片正文已全部走 `helper.*`（此前是硬编码中文，en/ru/fa 用户看到的也是中文）：状态标题类接既有键，
  // 步骤/说明/按钮等本页独有的句子本批新立键，五语种齐备。
  const { t } = useTranslation();
  const [status, setStatus] = useState<HelperStatus | null>(null);
  const [state, setState] = useState<HelperState>('checking');
  const [busy, setBusy] = useState(false);
  const { armed, confirmTwice } = useConfirmTwice();
  const confirmingUninstall = armed === UNINSTALL_KEY;

  useEffect(() => {
    void helperApi
      .getStatus()
      .then((s) => {
        setStatus(s);
        setState(deriveState(s));
      })
      // 失败时停在 'checking'（不谎报成 unsupported/none）——该态自带「重新检测」按钮，是可恢复的
      // 死角而非死路。挂载即弹 toast 会在后端未就绪的正常启动窗口里制造噪音，故只记 console。
      .catch((err) => console.error('[SettingsHelper] getStatus failed:', err));

    const off = helperApi.onUpgradeable(() => {
      // .catch 不可省：事件回调里的 promise 无人接管，抛出即未捕获 rejection。刷新失败只意味着
      // 卡片停在旧状态（用户可点「重新检测」），不值得打断，故静默。
      void helperApi
        .getStatus()
        .then((s) => {
          setStatus(s);
          setState(deriveState(s));
        })
        .catch(() => undefined);
    });
    return off;
  }, []);

  /**
   * 以下三个动作都经 `onClick={install}` 这类形式挂到按钮上 —— React **丢弃**返回的 promise，
   * 故每个都必须自带 catch，否则 helperApi 一旦 reject 就是没人能接的未捕获 rejection：
   * 用户只看到按钮转一圈回到原样，不知道失败了、更不知道为什么。
   */
  function reportFailure(title: string, err: unknown) {
    toast.error(title, err instanceof Error ? err.message : String(err));
  }

  /** 安装/升级/修复共用（三态按钮都指向它）。 */
  async function install() {
    setBusy(true);
    try {
      const r = await helperApi.install();
      setStatus(r.status);
      setState(deriveState(r.status));
    } catch (err) {
      reportFailure(t('helper.installFail'), err);
    } finally {
      setBusy(false);
    }
  }

  /**
   * 卸载提权助手 —— **破坏性操作**，确认闸门必须真生效。
   *
   * 最早写成 `if (!window.confirm(...)) return;`：Tauri 下 confirm 返 Promise（恒 truthy），
   * 取消与确定等价 ⇒ 实为「无确认直接卸载」。后改自绘 `ConfirmDialog`，2026-07-29 再统一到
   * 原型的原地二次点击（:4234 `helper-uninstall` → `confirmTwice(t, '卸载提权助手？', …)`）。
   */
  function uninstall() {
    confirmTwice(UNINSTALL_KEY, () => {
      void (async () => {
        setBusy(true);
        try {
          const r = await helperApi.uninstall();
          setStatus(r.status);
          setState(deriveState(r.status));
        } catch (err) {
          // `onClick={uninstall}` 丢弃返回值 ⇒ 不在这里 catch 就是没人能接的 rejection，用户零反馈。
          reportFailure(t('helper.uninstallFail'), err);
        } finally {
          setBusy(false);
        }
      })();
    });
  }

  async function recheck() {
    setBusy(true);
    try {
      const s = await helperApi.getStatus(true);
      setStatus(s);
      setState(deriveState(s));
    } catch (err) {
      reportFailure(t('helper.statusCheckFail'), err);
    } finally {
      setBusy(false);
    }
  }

  /**
   * 无「打开登录项设置」IPC 命令：best-effort 复制设置路径，供用户手动前往。
   *
   * 路径本身也走 i18n（`helper.loginItemsPath`）：它要与用户那台 macOS「系统设置」里**实际显示的**
   * 路径逐字对上才有用，照抄中文给英文系统的用户等于给了一串对不上的字。界面语言通常跟随系统语言，
   * 故按界面语言给是当前能做到的最接近的口径。同一串也用在上面的 `helper.btmDesc` 里。
   */
  async function copyLoginItemsHint() {
    try {
      await navigator.clipboard.writeText(t('helper.loginItemsPath'));
    } catch {
      // 剪贴板 API 不可用（非安全上下文）：静默兜底
    }
  }

  return (
    <section className="screen" data-sec="helper">
      <Phead title={t('helper.title')} sub={t('helper.pageSub')} />

      <div className="card helper-card" id="helper-card" data-state={state}>
        <div className="hc-head">
          <span className="hc-ic">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 3l7 3v6c0 4.5-3 7.5-7 9-4-1.5-7-4.5-7-9V6z" />
              <path d="M9 12l2 2 4-4" />
            </svg>
          </span>
          <div className="hc-tx">
            <b id="hc-daemon-name">com.polaris.helper</b>
            <div id="hc-daemon-desc">{t('helper.daemonDesc')}</div>
          </div>
        </div>

        <div className="hc-body">
          {state === 'checking' && (
            <div className="helper-state" data-s="checking">
              <div className="hc-status">
                <Spinner />
                <b>{t('helper.statusChecking')}</b>
              </div>
              <div style={{ display: 'flex', gap: 9, marginTop: 14 }}>
                <Button variant="ghost" size="sm" onClick={recheck} disabled={busy}>
                  <span>{t('helper.recheck')}</span>
                </Button>
              </div>
            </div>
          )}

          {state === 'unsupported' && (
            <div className="helper-state" data-s="unsupported">
              <div className="hc-status">
                <Dot variant="idle" />
                {/* 旧文案「提权助手尚未接入」写于 HelperStatusSnapshot 还没有 supported 字段的年代
                    （那时 `!s.supported` 恒真，人人可见，故只能写成"没接线"）。后端早已返真值
                    （runtime/helper.rs::platform_supported → mac/win/linux 全 true），本态如今**只有真正
                    不支持的平台**才到得了，继续说"尚未接入"就成了新的错报。首帧/检测失败已由
                    'checking' 态承接，不再挤在这里。 */}
                <b>{t('helper.unsupportedTitle')}</b>
                <span className="card-sub">
                  {t('helper.unsupportedDesc')}
                </span>
              </div>
            </div>
          )}

          {state === 'installed' && (
            <div className="helper-state" data-s="installed">
              <div className="hc-status">
                <Dot variant="ok" />
                <b>{t('helper.statusInstalled')}</b>
                <Pill variant="ok" style={{ marginLeft: 'auto' }}>
                  {t('helper.protocolVersion', { version: status?.version ?? 3 })}
                </Pill>
              </div>
              <div className="card-sub">{t('helper.installedDesc')}</div>
              <div style={{ display: 'flex', gap: 9, marginTop: 14, flexWrap: 'wrap' }}>
                <Button variant="ghost" size="sm" onClick={recheck} disabled={busy}>
                  <span>{t('helper.recheck')}</span>
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  className={confirmingUninstall ? 'confirming' : undefined}
                  onClick={uninstall}
                  disabled={busy}
                  style={{ color: 'hsl(var(--err))' }}
                >
                  {/* 确认态换 `<span>` 文案（原型 confirmTwice 就是换 span 的 textContent）。 */}
                  <span>
                    {confirmingUninstall ? t('helper.uninstallConfirmAgain') : t('helper.uninstall')}
                  </span>
                </Button>
              </div>
            </div>
          )}

          {state === 'none' && (
            <div className="helper-state" data-s="none">
              <div className="hc-status">
                <Dot variant="idle" />
                <b>{t('helper.statusNotInstalled')}</b>
              </div>
              <div className="card-sub">{t('helper.noneDesc')}</div>
              <div className="hc-steps">
                <div className="hc-step">
                  <span className="st-n">1</span>
                  <span>{t('helper.stepCopyBinary')}</span>
                </div>
                <div className="hc-step">
                  <span className="st-n">2</span>
                  <span>{t('helper.stepRegisterDaemon')}</span>
                </div>
              </div>
              <Button variant="flow" onClick={install} disabled={busy}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M12 3v11M8 10l4 4 4-4" />
                </svg>
                <span>{t('helper.installAction')}</span>
              </Button>
            </div>
          )}

          {state === 'installing' && (
            <div className="helper-state" data-s="installing">
              <div className="hc-status">
                <Spinner />
                <b>{t('helper.installing')}</b>
              </div>
              <div className="card-sub">{t('helper.installingDesc')}</div>
            </div>
          )}

          {state === 'needs-upgrade' && (
            <div className="helper-state" data-s="needs-upgrade">
              <div className="hc-status">
                <Dot variant="err" style={{ background: 'hsl(var(--warn))' }} />
                <b style={{ color: 'hsl(var(--warn))' }}>{t('helper.statusUpgradeable')}</b>
                <Pill variant="warn" style={{ marginLeft: 'auto' }}>
                  {t('helper.protocolVersionBelow', { version: status?.version ?? 2, required: 3 })}
                </Pill>
              </div>
              <div className="card-sub">{t('helper.upgradeDescCard')}</div>
              <div style={{ display: 'flex', gap: 9, marginTop: 14 }}>
                <Button variant="flow" size="sm" onClick={install} disabled={busy}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M12 3v11M8 10l4 4 4-4" />
                  </svg>
                  <span>{t('helper.upgradeHelperAction')}</span>
                </Button>
              </div>
            </div>
          )}

          {state === 'needs-btm' && (
            <div className="helper-state" data-s="needs-btm">
              <div className="hc-status">
                <Dot variant="err" />
                <b id="hc-btm-title" style={{ color: 'hsl(var(--warn))' }}>
                  {t('helper.btmTitle')}
                </b>
              </div>
              <div className="card-sub" id="hc-btm-desc">
                {t('helper.btmDesc')}
              </div>
              <div style={{ display: 'flex', gap: 9, marginTop: 14, flexWrap: 'wrap' }}>
                <Button variant="flow" size="sm" onClick={copyLoginItemsHint} disabled={busy}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <rect x="9" y="9" width="11" height="11" rx="2" />
                    <path d="M5 15V5a2 2 0 012-2h10" />
                  </svg>
                  <span id="hc-btm-btn">{t('helper.btmCopyPath')}</span>
                </Button>
                <Button variant="ghost" size="sm" onClick={recheck} disabled={busy}>
                  <span>{t('helper.recheck')}</span>
                </Button>
              </div>
            </div>
          )}

          {state === 'needs-repair' && (
            <div className="helper-state" data-s="needs-repair">
              <div className="hc-status">
                <Dot variant="err" />
                <b style={{ color: 'hsl(var(--warn))' }}>{t('helper.statusNeedsRepair')}</b>
              </div>
              <div className="card-sub">{t('helper.repairDesc')}</div>
              <div style={{ display: 'flex', gap: 9, marginTop: 14 }}>
                <Button variant="flow" size="sm" onClick={install} disabled={busy}>
                  <span>{t('helper.repairAction')}</span>
                </Button>
              </div>
            </div>
          )}
        </div>
      </div>

      {/* 右侧此前挂着 `<Pill variant="region">待命</Pill>` —— 纯静态装饰：自动恢复是 Rust
          core-supervisor 内部行为（crash_recovery.rs 的 auto_restart_enabled），**没有任何 IPC 把它的
          实时状态发给渲染端**（api-client.ts:120 已记过同类教训：声明了后端从不发的字段）。恒显「待命」
          的徽章会被读成真状态（"现在是待命，异常时会变别的"），实际永远不变 ⇒ 删掉，宁可无状态也不留假状态。
          接线那天：由 helperApi 或 proxy 状态事件补真值，再把徽章加回来。 */}
      <SetBlock header={t('helper.fallbackTitle')}>
        <SetRow
          label={t('helper.autoRecover')}
          tip={t('helper.autoRecoverDesc')}
        />
      </SetBlock>
    </section>
  );
}
