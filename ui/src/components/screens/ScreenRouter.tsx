/**
 * ScreenRouter —— 按 nav-store active screen 渲染对应页面。
 *
 * 已接入真实组件：home / nodes / rules / apppolicy / resources / connections / logs / settings。
 * settings scope 渲染 SettingsPage（内部按 settingsScreen 路由 9 子页）。
 */

import { lazy, Suspense, type ReactNode } from 'react';
import { useNavStore } from '@/store/nav-store';
import { Spinner } from './settings/primitives';

// 页面级代码分割：首次只加载当前屏，避免连接、日志、规则编辑器等互不相关的大模块全部进入首屏堆。
// 切换后模块由浏览器缓存，不改变各屏原有的卸载/重挂语义。
const SettingsPage = lazy(() => import('./settings/SettingsPage'));
const HomeScreen = lazy(() => import('./home/HomeScreen'));
const ConnectionsScreen = lazy(() => import('./connections/ConnectionsScreen'));
const LogsScreen = lazy(() => import('./logs/LogsScreen'));
const NodesScreen = lazy(() => import('./nodes/NodesScreen'));
const RulesScreen = lazy(() => import('./rules/RulesScreen'));
const AppPolicyScreen = lazy(() => import('./apppolicy/AppPolicyScreen'));
const ResourcesScreen = lazy(() => import('./resources/ResourcesScreen'));

function loadingScreen(): ReactNode {
  return (
    <section className="screen" style={{ display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
      <Spinner />
    </section>
  );
}

export function ScreenRouter() {
  const scope = useNavStore((s) => s.scope);
  const mainScreen = useNavStore((s) => s.mainScreen);

  // settings scope：渲染 9 子页容器（SettingsPage 内部按 settingsScreen 路由）。
  // 子侧栏 SettingsSidebar 由 AppShell 在 settings scope 下替换主 Sidebar 渲染。
  if (scope === 'settings') {
    return <Suspense fallback={loadingScreen()}><SettingsPage /></Suspense>;
  }

  let screen: ReactNode;
  switch (mainScreen) {
    case 'home':
      screen = <HomeScreen />;
      break;
    case 'nodes':
      screen = <NodesScreen />;
      break;
    case 'rules':
      screen = <RulesScreen />;
      break;
    case 'apppolicy':
      screen = <AppPolicyScreen />;
      break;
    case 'resources':
      screen = <ResourcesScreen />;
      break;
    case 'connections':
      screen = <ConnectionsScreen />;
      break;
    case 'logs':
      screen = <LogsScreen />;
      break;
    default:
      // 防御性兜底：未来新增 MainScreen 未接组件时显式占位，不静默白屏。
      return (
        <section className="screen">
          <div className="phead">
            <h1>{mainScreen}</h1>
          </div>
        </section>
      );
  }
  return <Suspense fallback={loadingScreen()}>{screen}</Suspense>;
}

export default ScreenRouter;
