/**
 * ScreenRouter —— 按 nav-store active screen 渲染对应页面。
 *
 * 已接入真实组件：home / nodes / rules / apppolicy / resources / connections / logs / settings。
 * settings scope 渲染 SettingsPage（内部按 settingsScreen 路由 9 子页）。
 */

import { useNavStore } from '@/store/nav-store';
import { SettingsPage } from './settings/SettingsPage';
import HomeScreen from './home/HomeScreen';
import ConnectionsScreen from './connections/ConnectionsScreen';
import LogsScreen from './logs/LogsScreen';
import NodesScreen from './nodes/NodesScreen';
import RulesScreen from './rules/RulesScreen';
import AppPolicyScreen from './apppolicy/AppPolicyScreen';
import ResourcesScreen from './resources/ResourcesScreen';

export function ScreenRouter() {
  const scope = useNavStore((s) => s.scope);
  const mainScreen = useNavStore((s) => s.mainScreen);

  // settings scope：渲染 9 子页容器（SettingsPage 内部按 settingsScreen 路由）。
  // 子侧栏 SettingsSidebar 由 AppShell 在 settings scope 下替换主 Sidebar 渲染。
  if (scope === 'settings') {
    return <SettingsPage />;
  }

  switch (mainScreen) {
    case 'home':
      return <HomeScreen />;
    case 'nodes':
      return <NodesScreen />;
    case 'rules':
      return <RulesScreen />;
    case 'apppolicy':
      return <AppPolicyScreen />;
    case 'resources':
      return <ResourcesScreen />;
    case 'connections':
      return <ConnectionsScreen />;
    case 'logs':
      return <LogsScreen />;
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
}

export default ScreenRouter;
