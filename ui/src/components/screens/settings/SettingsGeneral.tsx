/**
 * SettingsGeneral —— 通用子页（原型 [data-sec="general"] L2056-2070）。
 *
 * 两块：
 *  1. 启动：开机自启 / 静默启动 / 自动连接 / 启动时检查更新
 *  2. 隐私与安全：自动隐私锁（autoPrivacyMode）+ 锁屏密码（仅隐私锁开启时显示，契约 L111）
 *     + 关闭日志写盘（disableLogFile，隐私 / 省盘）
 *
 * 「更新与测速」（mainSessionViaProxy + speedTestUrl）上一批曾临时寄放在本页，现已按契约归位到
 * `SettingsNetwork`。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { UserConfig } from '@/contracts/types';
import { autoStartApi, privacyApi } from '@/ipc/api-client';
import { Phead, SetBlock, SetRow, Switch, Button, TextInput } from './primitives';
import { autoCheckUpdateChecked } from './settings-logic';

export interface SettingsGeneralProps {
  config: UserConfig;
  update: (patch: Partial<UserConfig>) => Promise<void>;
}

export default function SettingsGeneral({ config, update }: SettingsGeneralProps) {
  const { t } = useTranslation();
  const [hasPassword, setHasPassword] = useState(false);
  // 内联密码编辑器（替代简陋 window.prompt）：editing=展开输入行，pwDraft=草稿，pwErr=错误行，saving=提交中。
  const [editing, setEditing] = useState(false);
  const [pwDraft, setPwDraft] = useState('');
  const [pwErr, setPwErr] = useState('');
  const [saving, setSaving] = useState(false);

  // 查询是否已设密码（隐私锁状态文案依赖）
  useEffect(() => {
    void privacyApi.hasPassword().then(setHasPassword).catch(() => undefined);
  }, []);

  async function toggleAutoStart(next: boolean) {
    // autoStart 同时写 OS launch agent + config；走 autoStartApi 保证两端一致
    try {
      await autoStartApi.set(next);
      await update({ autoStart: next });
    } catch {
      // 失败回滚由 useConfig 乐观更新机制兜底
    }
  }

  function openPwEditor() {
    setPwDraft('');
    setPwErr('');
    setEditing(true);
  }

  // 保存锁屏密码：留空 = 清除（对齐后端 set_password_core：空串 remove hash）。调 privacy_set_password。
  // 后端锁屏中拒改（PRIVACY_LOCKED），但 Settings 只在未锁时可达，此路径正常不触发；仍捕获兜底显错。
  async function savePassword() {
    if (saving) return;
    setSaving(true);
    setPwErr('');
    try {
      const r = await privacyApi.setPassword(pwDraft);
      if (r.success) {
        setHasPassword(pwDraft.length > 0);
        setEditing(false);
        setPwDraft('');
      }
    } catch (e) {
      setPwErr(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="screen" data-sec="general">
      {/* 页标题用 `settings.general.pageTitle`（「通用」）而非侧栏那条 `settings.nav.general`（「常规」）：
          两处中文本就不同，i18n 收口不该顺手把它们统一掉——那是文案决定，不是搬运。
          同理侧栏「助手」与本页标题「提权助手」也各自保留。要统一请单独提。 */}
      <Phead title={t('settings.general.pageTitle')} sub={t('settings.general.pageSub')} />

      {/* 启动三开关均已接通后端，disabled 标记已删除（此前的「零消费者」块注释已过时）：
          - silentStart：`main.rs` 的 `config_silent_start` 在 setup 期合并进 `start_hidden`，
            决定主窗是否首帧显示。这一条其实早就实现，之前的 disabled 属于陈旧标记。
          - autoConnect / autoCheckUpdate：`runtime/startup_tasks.rs` 落地后，启动期分别按 2s / 5s
            的一次性任务读取这两个字段（自动连接、检查更新）。
          autoPrivacyMode 不在本块（在下方「隐私与安全」），其闲置检测 useIdlePrivacyLock 同样已接线，
          故全页无 disabled 标记。（原注写「仍未接通的 autoPrivacyMode …已接线」自相矛盾，是接线落地后
          没跟着改的陈述残留，一并订正。） */}
      <SetBlock header={t('settings.general.groupStartup')}>
        <SetRow label={t('settings.general.autoStartTitle')}>
          <Switch checked={config.autoStart} onChange={toggleAutoStart} />
        </SetRow>
        <SetRow
          label={t('settings.general.silentStart')}
          desc={t('settings.general.silentStartDesc')}
        >
          <Switch
            checked={config.silentStart}
            onChange={(v) => void update({ silentStart: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.general.autoConnect')}
          desc={t('settings.general.autoConnectDesc')}
        >
          <Switch
            checked={config.autoConnect}
            onChange={(v) => void update({ autoConnect: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.general.autoCheckUpdate')}
          desc={t('settings.general.autoCheckUpdateDesc')}
        >
          {/* 正向语义、缺省为开：后端按 `!== false` 判定（缺省 = 开）。此前写
              `checked={config.autoCheckUpdate}`，存量配置缺该键时会显示成「关」而后端按「开」跑。 */}
          <Switch
            checked={autoCheckUpdateChecked(config)}
            onChange={(v) => void update({ autoCheckUpdate: v })}
          />
        </SetRow>
      </SetBlock>

      <SetBlock header={t('settings.general.groupPrivacy')}>
        <SetRow
          label={t('settings.general.autoPrivacyMode')}
          desc={t('settings.general.autoPrivacyModeDesc')}
        >
          <Switch
            checked={!!config.autoPrivacyMode}
            onChange={(v) => {
              // 关掉隐私锁时一并收起密码编辑器：下面那行会随之卸载，留着 editing=true 会让下次
              // 开启隐私锁时直接弹出一个用户没点过的输入框。
              if (!v) setEditing(false);
              void update({ autoPrivacyMode: v });
            }}
          />
        </SetRow>
        {/* 锁屏密码：内联编辑器（替代 window.prompt）。收起态显状态文案 + 设置/修改按钮；
            展开态显密码输入 + 保存/取消（留空清除，对齐后端 set_password_core）。
            契约 L111：**仅 autoPrivacyMode 开启时显示**（上游 general-settings.tsx:166 同）——
            隐私锁关闭时密码不参与任何判定，恒显只会让人以为设了密码就有保护。 */}
        {config.autoPrivacyMode && (
          <SetRow
            label={t('settings.general.privacyPassword')}
            desc={
              hasPassword
                ? t('settings.general.privacyPasswordSet')
                : t('settings.general.privacyPasswordUnset')
            }
            align={editing ? 'start' : undefined}
          >
            {editing ? (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 6, width: 240 }}>
                <div style={{ display: 'flex', gap: 6 }}>
                  <TextInput
                    type="password"
                    autoComplete="new-password"
                    placeholder={t('settings.general.privacyPasswordPlaceholder')}
                    value={pwDraft}
                    disabled={saving}
                    autoFocus
                    onChange={(e) => setPwDraft(e.currentTarget.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') void savePassword();
                      if (e.key === 'Escape') setEditing(false);
                    }}
                    style={{ flex: 1, minWidth: 0 }}
                  />
                  <Button variant="flow" size="sm" disabled={saving} onClick={() => void savePassword()}>
                    <span>{t('common.save')}</span>
                  </Button>
                  <Button variant="ghost" size="sm" disabled={saving} onClick={() => setEditing(false)}>
                    <span>{t('common.cancel')}</span>
                  </Button>
                </div>
                {pwErr && (
                  <div style={{ fontSize: 11.5, color: 'hsl(var(--err))' }}>{pwErr}</div>
                )}
              </div>
            ) : (
              <Button variant="ghost" size="sm" onClick={openPwEditor}>
                <span>
                  {hasPassword
                    ? t('settings.general.privacyPasswordChange')
                    : t('settings.general.privacyPasswordSetBtn')}
                </span>
              </Button>
            )}
          </SetRow>
        )}
        {/* 关闭日志写盘 → sing-box log.disabled（builder/log.rs:37,51；运行中保存由
            runtime/proxy.rs:4874-4878 重启生效）。归在隐私块：它的正当用途就是「不落盘留痕」。
            用 DescFull 而非另造短描述——这项的代价（应用内实时日志与日志诊断一并失效）必须说全，
            半句话的短描述会让人以为只是少写点文件。 */}
        <SetRow
          label={t('settings.advanced.disableLogFile', '关闭日志写盘')}
          desc={t('settings.advanced.disableLogFileDescFull')}
        >
          <Switch
            id="disable-log-file-swt"
            checked={!!config.disableLogFile}
            onChange={(v) => void update({ disableLogFile: v })}
            aria-label={t('settings.advanced.disableLogFile', '关闭日志写盘')}
          />
        </SetRow>
      </SetBlock>
    </section>
  );
}
