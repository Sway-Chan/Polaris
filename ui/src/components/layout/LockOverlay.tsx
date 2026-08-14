/**
 * 隐私锁全屏遮罩 —— 1:1 移植原型 `#lock-overlay`（L2473-2485）。
 *
 * `privacyMode === true` 时铺满 `.win` 卡片（`#lock-overlay` id 选择器带 blur backdrop），
 * 品牌星 + 「Polaris 已锁定」+ 密码框 + 解锁按钮 + 错误行。挂 AppShell（主窗唯一 chrome 容器）。
 *
 * # 解锁链（后端 ground-truth，非信 brief）
 * `privacy_unlock` 只校验密码返 `{ok}`，**不翻转 PRIVACY_MODE、也不 emit exit**（config.rs:424-438 实证）。
 * 故 ok:true 后必须显式 `config_set_privacy_mode(false)` 才真正退出隐私态 → 后端 emit exitPrivacyMode
 * → App 全局订阅 `setPrivacyMode(false)` → 本遮罩卸载。**单一收敛点走事件**，不在此手改 store（对齐
 * proxyStarted/configChanged 的收敛范式：本窗与托盘/idle 触发的启停都靠后端事件归一）。
 *
 * 未设密码（hasPassword=false）：空密码 attempt='unlock' → 后端 unlock_core 空 hash 恒 ok → 自由解锁。
 * 失败：后端 `apply_unlock_rate_limit` sleep(300ms) 弱限速，await 期间禁用按钮/输入防连点暴力。
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useAppStore } from '@/store/app-store';
import { api } from '@/ipc';
import { resolveUnlockAttempt } from '@/domain/privacy';

export default function LockOverlay() {
  const { t } = useTranslation();
  const privacyMode = useAppStore((s) => s.privacyMode);
  const [pw, setPw] = useState('');
  const [hasPassword, setHasPassword] = useState(false);
  const [err, setErr] = useState('');
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // 进入锁定态：查是否设了密码（决定空密码放行）+ 复位输入/错误 + 聚焦密码框（对齐原型 openLock）。
  useEffect(() => {
    if (!privacyMode) return;
    setPw('');
    setErr('');
    void api.privacy
      .hasPassword()
      .then(setHasPassword)
      .catch(() => setHasPassword(false));
    const id = setTimeout(() => inputRef.current?.focus(), 50);
    return () => clearTimeout(id);
  }, [privacyMode]);

  const submit = useCallback(async () => {
    if (busy) return;
    if (resolveUnlockAttempt(hasPassword, pw) === 'require-input') {
      setErr(t('privacy.enterPassword'));
      inputRef.current?.focus();
      return;
    }
    setBusy(true);
    setErr('');
    try {
      const r = await api.privacy.unlock(pw); // 失败：后端弱限速 sleep(300ms)
      if (r.ok) {
        // 真正退出隐私态 → emit exitPrivacyMode → App 订阅卸载本遮罩（不在此手改 store）。
        await api.config.setPrivacyMode(false);
      } else {
        setErr(t('privacy.wrongPassword'));
        setPw('');
        inputRef.current?.focus();
      }
    } catch {
      setErr(t('privacy.wrongPassword'));
    } finally {
      setBusy(false);
    }
  }, [busy, hasPassword, pw, t]);

  if (!privacyMode) return null;

  return (
    <div className="overlay" id="lock-overlay" role="dialog" aria-modal="true" aria-label={t('privacy.title')}>
      <div className="lock-box">
        <div className="lock-mk">
          <svg viewBox="-46 -46 92 92">
            <use href="#polarisStar" />
          </svg>
        </div>
        <h3>{t('privacy.title')}</h3>
        <p>{t('privacy.subtitle')}</p>
        <div className="lock-field">
          <input
            ref={inputRef}
            className="input"
            id="lock-pw"
            type="password"
            placeholder="••••••••"
            autoComplete="off"
            value={pw}
            disabled={busy}
            onChange={(e) => setPw(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void submit();
            }}
          />
          <button className="btn flow" disabled={busy} onClick={() => void submit()}>
            {busy ? (
              <span className="spinner spin-inline" role="status" aria-label={t('common.loading')} />
            ) : (
              <span>{t('privacy.unlock')}</span>
            )}
          </button>
        </div>
        <div
          id="lock-err"
          role="alert"
          style={{ fontSize: '11.5px', color: 'hsl(var(--err))', marginTop: 10, height: 14 }}
        >
          {err}
        </div>
      </div>
    </div>
  );
}
