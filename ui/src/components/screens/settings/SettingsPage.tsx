/**
 * SettingsPage —— settings 屏容器（原型 #s-settings L2054-2412，358 行 / 9 子页）。
 *
 * 职责：
 *  1. 渲染 SettingsSidebar（9 子页导航，对齐 nav-store.settingsScreen）；
 *  2. 用 useConfig 加载 UserConfig 一次，传给当前子页；
 *  3. 按 settingsScreen 路由到对应子页组件。
 *
 * 子页对齐原型 .set-section[data-sec]：
 *   general | display | network | dns | tun | update | backup | helper | about
 *
 * 注：SettingsSidebar 由 AppShell 在 settings scope 下替换主 Sidebar 渲染（见 ScreenRouter 协议）。
 * 本组件只渲染 main 内容区 + 子页路由；侧栏切换在 AppShell 层处理。
 */

import { useTranslation } from 'react-i18next';
import { useNavStore } from '@/store/nav-store';
import { Spinner } from './primitives';
import { useConfig } from './useConfig';
import SettingsGeneral from './SettingsGeneral';
import SettingsDisplay from './SettingsDisplay';
import SettingsNetwork from './SettingsNetwork';
import SettingsDns from './SettingsDns';
import SettingsTun from './SettingsTun';
import SettingsUpdate from './SettingsUpdate';
import SettingsBackup from './SettingsBackup';
import SettingsHelper from './SettingsHelper';
import SettingsAbout from './SettingsAbout';

export function SettingsPage() {
  const { t } = useTranslation();
  const settingsScreen = useNavStore((s) => s.settingsScreen);
  const { config, loading, error, update, reload } = useConfig();

  // loading/error 态不在原型里（静态 demo 无真实异步加载）——沿用 .screen 容器 + 原型原语按钮，
  // 不新发明布局类；居中/间距用内联 style，不是逐字复现对象。
  if (loading) {
    return (
      <section className="screen" style={{ display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <Spinner />
      </section>
    );
  }

  // 只有**加载**失败才塌成错误屏（useConfig.error 已收窄为 load-only：保存失败走 toast，
  // 否则一次瞬时保存失败会把用户正在编辑的子页整个卸载，且文案说错原因）。
  if (error || !config) {
    return (
      <section className="screen">
        <div style={{ fontSize: 13, color: 'hsl(var(--err))' }}>
          {t('common.configLoadFail')}
          {error ? `${t('common.colon')}${error}` : ''}
        </div>
        <button type="button" onClick={() => void reload()} className="btn ghost sm" style={{ marginTop: 12 }}>
          <span>{t('common.retry')}</span>
        </button>
      </section>
    );
  }

  // 9 子页统一接 { config, update }；各自按需补子 store（如 update/helper 状态）。
  switch (settingsScreen) {
    case 'general':
      return <SettingsGeneral config={config} update={update} />;
    case 'display':
      return <SettingsDisplay config={config} update={update} />;
    case 'network':
      return <SettingsNetwork config={config} update={update} />;
    case 'dns':
      return <SettingsDns config={config} update={update} />;
    case 'tun':
      return <SettingsTun config={config} update={update} />;
    case 'update':
      return <SettingsUpdate config={config} update={update} />;
    case 'backup':
      return <SettingsBackup config={config} />;
    case 'helper':
      return <SettingsHelper />;
    case 'about':
      return <SettingsAbout />;
    default:
      return <SettingsGeneral config={config} update={update} />;
  }
}

export default SettingsPage;
