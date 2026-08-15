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

import { lazy, Suspense, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavStore } from '@/store/nav-store';
import { Spinner } from './primitives';
import { useConfig } from './useConfig';

// 设置页也按真实功能边界分块；“进入设置”不再同时加载更新、备份、Helper 等九套表单逻辑。
const SettingsGeneral = lazy(() => import('./SettingsGeneral'));
const SettingsDisplay = lazy(() => import('./SettingsDisplay'));
const SettingsNetwork = lazy(() => import('./SettingsNetwork'));
const SettingsDns = lazy(() => import('./SettingsDns'));
const SettingsTun = lazy(() => import('./SettingsTun'));
const SettingsUpdate = lazy(() => import('./SettingsUpdate'));
const SettingsBackup = lazy(() => import('./SettingsBackup'));
const SettingsHelper = lazy(() => import('./SettingsHelper'));
const SettingsAbout = lazy(() => import('./SettingsAbout'));

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
  let page: ReactNode;
  switch (settingsScreen) {
    case 'general':
      page = <SettingsGeneral config={config} update={update} />;
      break;
    case 'display':
      page = <SettingsDisplay config={config} update={update} />;
      break;
    case 'network':
      page = <SettingsNetwork config={config} update={update} />;
      break;
    case 'dns':
      page = <SettingsDns config={config} update={update} />;
      break;
    case 'tun':
      page = <SettingsTun config={config} update={update} />;
      break;
    case 'update':
      page = <SettingsUpdate config={config} update={update} />;
      break;
    case 'backup':
      page = <SettingsBackup config={config} />;
      break;
    case 'helper':
      page = <SettingsHelper />;
      break;
    case 'about':
      page = <SettingsAbout />;
      break;
    default:
      page = <SettingsGeneral config={config} update={update} />;
  }
  return <Suspense fallback={<section className="screen" style={{ display: 'flex', alignItems: 'center', justifyContent: 'center' }}><Spinner /></section>}>{page}</Suspense>;
}

export default SettingsPage;
