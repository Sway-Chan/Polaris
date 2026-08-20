/**
 * ScreenRouter —— 按 nav-store active screen 渲染对应页面。
 *
 * 已接入真实组件：home / nodes / rules / apppolicy / resources / connections / logs / settings。
 * settings scope 渲染 SettingsPage（内部按 settingsScreen 路由 9 子页）。
 */

import { lazy, Suspense, useLayoutEffect, type ReactNode } from 'react';
import { useNavStore } from '@/store/nav-store';
import { reportRendererReady } from '@/lib/renderer-ready';
import { Spinner } from './settings/primitives';

// 页面级代码分割：首次只加载当前屏，避免连接、日志、规则编辑器等互不相关的大模块全部进入首屏堆。
// 切换后模块由浏览器缓存，不改变各屏原有的卸载/重挂语义。
const screenLoaders = {
  settings: () => import('./settings/SettingsPage'),
  home: () => import('./home/HomeScreen'),
  connections: () => import('./connections/ConnectionsScreen'),
  logs: () => import('./logs/LogsScreen'),
  nodes: () => import('./nodes/NodesScreen'),
  rules: () => import('./rules/RulesScreen'),
  apppolicy: () => import('./apppolicy/AppPolicyScreen'),
  resources: () => import('./resources/ResourcesScreen'),
} as const;

const SettingsPage = lazy(screenLoaders.settings);
const HomeScreen = lazy(screenLoaders.home);
const ConnectionsScreen = lazy(screenLoaders.connections);
const LogsScreen = lazy(screenLoaders.logs);
const NodesScreen = lazy(screenLoaders.nodes);
const RulesScreen = lazy(screenLoaders.rules);
const AppPolicyScreen = lazy(screenLoaders.apppolicy);
const ResourcesScreen = lazy(screenLoaders.resources);

// i18n 初始化会在 React mount 前异步加载当前语言；默认屏若等到第一次 render 才发起 import，就在语言包
// 后面又串出一段瀑布。模块求值时按 nav-store 的首帧真值只预取**当前一屏**，让两份本地 chunk 并行；
// 其它页面仍保持按需加载，不扩大常驻模块集。托盘注入的 settings 首帧意图也已在 nav-store 求值时消费。
const initialRoute = useNavStore.getState();
const initialLoader =
  initialRoute.scope === 'settings'
    ? screenLoaders.settings
    : screenLoaders[initialRoute.mainScreen];
void initialLoader().catch(() => {
  /* React.lazy 正式渲染仍会走自己的错误边界；预取失败不在这里另报一次 */
});

/**
 * `renderer:ready` 必须落在 Suspense **内容腿**而非 fallback 腿：旧信号在 App 首次 commit 就发，主窗会
 * 带着转圈外壳先上屏，真实页面随后补入，正是 W27 的“窗口已出现但渲染仍滞后”。layout effect 在真实
 * 页面整棵 DOM 提交后、业务 passive effects 前运行；文档级去重见 `reportRendererReady`。
 */
function RendererReadyBoundary({ children }: { children: ReactNode }) {
  useLayoutEffect(() => reportRendererReady(), []);
  return children;
}

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
    return (
      <Suspense fallback={loadingScreen()}>
        <RendererReadyBoundary>
          <SettingsPage />
        </RendererReadyBoundary>
      </Suspense>
    );
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
      screen = (
        <section className="screen">
          <div className="phead">
            <h1>{mainScreen}</h1>
          </div>
        </section>
      );
      break;
  }
  return (
    <Suspense fallback={loadingScreen()}>
      <RendererReadyBoundary>{screen}</RendererReadyBoundary>
    </Suspense>
  );
}

export default ScreenRouter;
