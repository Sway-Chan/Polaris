/**
 * Sidebar —— 主侧栏（原型 .side #side-main L131-194 / markup L1596-1616）。
 *
 * 结构对齐原型（语义类走 components.css，per-os 宽度/透明由 :root[data-os] 驱动）：
 *  .side(.collapsed)
 *    .side-chrome（mac 顶部净空 36px 拖动槽；非 mac 12px 顶部可拖条 —— HTML data-tauri-drag-region）
 *    .brand（logo 磁贴 + "Polaris" wm + .brand-chev；点击 = 折叠/展开，即折叠开关本体，CSS no-drag）
 *    nav-item home / nodes
 *    nav-group "分流" → rules / apppolicy / resources
 *    nav-group "诊断" → connections / logs
 *    .spacer（flex:1）
 *    nav-item settings（贴底）
 *
 * 折叠态：加 .collapsed 类，宽/图标-only 布局全由 CSS（.side.collapsed …）驱动；label <span> 常渲染，
 * 折叠时 CSS 隐藏（.side.collapsed .nav-item > span:not(.cnt)），DOM 与原型一致。
 */

import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { useNavStore } from '@/store/nav-store';
import type { MainScreen } from '@/store/nav-store';
import {
  NavHomeIcon,
  NavNodesIcon,
  NavRulesIcon,
  NavAppPolicyIcon,
  NavResourcesIcon,
  NavConnectionsIcon,
  NavLogsIcon,
  NavSettingsIcon,
  ChevronLeftIcon,
} from '@/components/icons';

interface NavDef {
  key: MainScreen;
  Icon: (p: { className?: string }) => JSX.Element;
  /** i18n key under sidebar.* */
  labelKey: string;
}

/** 主屏导航项（对齐原型顺序：home/nodes → 分流[rules/apppolicy/resources] → 诊断[connections/logs]）。 */
const NAV_HOME: NavDef[] = [
  { key: 'home', Icon: NavHomeIcon, labelKey: 'sidebar.home' },
  { key: 'nodes', Icon: NavNodesIcon, labelKey: 'sidebar.server' },
];
const NAV_ROUTING: NavDef[] = [
  { key: 'rules', Icon: NavRulesIcon, labelKey: 'sidebar.rules' },
  { key: 'apppolicy', Icon: NavAppPolicyIcon, labelKey: 'sidebar.appPolicy' },
  { key: 'resources', Icon: NavResourcesIcon, labelKey: 'sidebar.ruleResources' },
];
const NAV_DIAG: NavDef[] = [
  { key: 'connections', Icon: NavConnectionsIcon, labelKey: 'sidebar.connections' },
  { key: 'logs', Icon: NavLogsIcon, labelKey: 'sidebar.logs' },
];

function NavItem({
  def,
  active,
  collapsed,
  onClick,
}: {
  def: NavDef;
  active: boolean;
  collapsed: boolean;
  onClick: () => void;
}) {
  const { t } = useTranslation();
  const { Icon } = def;
  const label = t(def.labelKey);
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? 'page' : undefined}
      // 折叠态才补 tooltip（展开态文字可见，无需）。**方向锁右**（原型 :3088 `dataset.tipSide='right'`）：
      // 默认的 top 会让 tip 压在侧栏自身的上一个导航项上——折叠态图标间距小，正是原生 title
      // （方向不可控、跟随鼠标）在这里最难受的地方。
      data-tip={collapsed ? label : undefined}
      data-tip-side="right"
      className={cn('nav-item', active && 'on')}
    >
      <Icon />
      <span>{label}</span>
    </button>
  );
}

export default function Sidebar() {
  const { t } = useTranslation();
  const scope = useNavStore((s) => s.scope);
  const mainScreen = useNavStore((s) => s.mainScreen);
  const navigate = useNavStore((s) => s.navigate);
  const enterSettings = useNavStore((s) => s.enterSettings);
  const collapsed = useNavStore((s) => s.sidebarCollapsed);
  const toggleSidebar = useNavStore((s) => s.toggleSidebar);

  // settings scope 下主侧栏隐藏（原型 #side-main hidden）
  if (scope === 'settings') return null;

  return (
    <nav aria-label={t('sidebar.primaryNav')} className={cn('side', collapsed && 'collapsed')}>
      {/* chrome 头：顶部净空 + 窗口拖动槽（空槽，不自绘灯）。拖动靠 `data-tauri-drag-region`，
          不是 `-webkit-app-region`（Electron 约定，Tauri 不认 → 此前一直拖不动）。见 AppShell 同规注释。 */}
      <div className="side-chrome" data-tauri-drag-region />

      {/* brand：logo 磁贴 + Polaris + .brand-chev；点击折叠/展开（折叠开关本体） */}
      <button
        type="button"
        onClick={toggleSidebar}
        aria-label={collapsed ? t('sidebar.expand') : t('sidebar.collapse')}
        className="brand"
      >
        <span className="mk">
          {/* Polaris 极光之星（sprite 自带渐变，见 PolarisStarSprite，AppShell 顶层挂载一次） */}
          <svg viewBox="-46 -46 92 92">
            <use href="#polarisStar" />
          </svg>
        </span>
        <span className="wm">Polaris</span>
        <ChevronLeftIcon className="brand-chev" />
      </button>

      {/* nav 主项 */}
      {NAV_HOME.map((def) => (
        <NavItem
          key={def.key}
          def={def}
          active={mainScreen === def.key}
          collapsed={collapsed}
          onClick={() => navigate(def.key)}
        />
      ))}
      <div className="nav-group">{t('sidebar.group.routing')}</div>
      {NAV_ROUTING.map((def) => (
        <NavItem
          key={def.key}
          def={def}
          active={mainScreen === def.key}
          collapsed={collapsed}
          onClick={() => navigate(def.key)}
        />
      ))}
      <div className="nav-group">{t('sidebar.group.diagnostics')}</div>
      {NAV_DIAG.map((def) => (
        <NavItem
          key={def.key}
          def={def}
          active={mainScreen === def.key}
          collapsed={collapsed}
          onClick={() => navigate(def.key)}
        />
      ))}

      <div className="spacer" />

      {/* settings 贴底 */}
      <button
        type="button"
        onClick={() => enterSettings()}
        data-tip={collapsed ? t('sidebar.settings') : undefined}
        data-tip-side="right"
        className="nav-item"
      >
        <NavSettingsIcon />
        <span>{t('sidebar.settings')}</span>
      </button>
    </nav>
  );
}
