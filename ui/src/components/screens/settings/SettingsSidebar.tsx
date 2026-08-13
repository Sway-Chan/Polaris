/**
 * SettingsSidebar —— settings 子侧栏（原型 #side-settings L1598-1627）。
 *
 * 结构对齐原型：
 *   .side
 *     .side-chrome（mac 交通灯槽位）
 *     .brand（logo 磁贴 + Polaris + 折叠 chev → backToApp）
 *     nav-group 分组：
 *       [常规]   general / display
 *       [网络栈] network / dns / tun
 *       [系统]   update / backup / helper
 *       [关于]   about
 *     spacer
 *     返回应用（贴底，对齐主侧栏 settings 入口位置）
 *
 * nav-store.SettingsScreen 9 项与原型 9 个 set-section 一一对应：
 *   general | display | network | dns | tun | update | backup | helper | about
 */

import { useTranslation } from 'react-i18next';

import { useNavStore } from '@/store/nav-store';
import type { SettingsScreen } from '@/store/nav-store';
import { cn } from '@/lib/utils';
import { ChevronLeftIcon } from '@/components/icons';
import type { ReactNode } from 'react';

/** 设置子页导航项定义（对齐原型 .set-nav[data-set]）。 */
interface SetNavDef {
  key: SettingsScreen;
  label: ReactNode;
  /** 小图标（原型 set-nav svg 17×17） */
  Icon: (p: { className?: string }) => ReactNode;
}

/* 各图标沿用原型 L1612-1623 path */
const IconGeneral = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M4 8h10M18 8h2M4 16h2M10 16h10" />
    <circle cx="16" cy="8" r="2" />
    <circle cx="8" cy="16" r="2" />
  </svg>
);
const IconDisplay = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <rect x="3" y="4" width="18" height="13" rx="2" />
    <path d="M8 20h8M12 17v3" />
  </svg>
);
const IconNetwork = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <circle cx="12" cy="12" r="9" />
    <path d="M3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18" />
  </svg>
);
const IconDns = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M12 3v18M5 8l7-5 7 5v8l-7 5-7-5z" />
  </svg>
);
const IconTun = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M2 12h4l2-5 4 10 2-5h8" />
  </svg>
);
const IconUpdate = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M12 3v11M8 10l4 4 4-4" />
    <path d="M4 17v3h16v-3" />
  </svg>
);
const IconBackup = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <ellipse cx="12" cy="6" rx="8" ry="3" />
    <path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6" />
  </svg>
);
const IconHelper = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <path d="M12 3l7 3v6c0 4.5-3 7.5-7 9-4-1.5-7-4.5-7-9V6z" />
    <path d="M9 12l2 2 4-4" />
  </svg>
);
const IconAbout = (p: { className?: string }) => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} {...p}>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 11v5M12 8h.01" />
  </svg>
);

/**
 * 分组表在**组件内**按 t 现算，不做模块级常量：常量在 import 期求值，那时 i18n 的语言还没被
 * `syncLanguageChoice` 校正过，切语言也不会让已求值的字面量跟着变。
 *
 * `DNS` / `TUN` 保持字面量（产品名跨语种同形，不进 locale——同 SettingsNetwork 里终端 shell 组名的理由）。
 */
function settingsGroups(
  t: (key: string) => string,
): { header: string | null; items: SetNavDef[] }[] {
  /* 原型 §7：general/display 是「裸头对」——前面没有 nav-group 标题；
   * 之后才是 网络栈/系统/关于 三个有标题的分组（L1628 comment: bare-first pair + category headers）。 */
  return [
    {
      header: null,
      items: [
        { key: 'general', label: t('settings.nav.general'), Icon: IconGeneral },
        { key: 'display', label: t('settings.nav.display'), Icon: IconDisplay },
      ],
    },
    {
      header: t('settings.nav.groupNetStack'),
      items: [
        { key: 'network', label: t('settings.nav.network'), Icon: IconNetwork },
        { key: 'dns', label: 'DNS', Icon: IconDns },
        { key: 'tun', label: 'TUN', Icon: IconTun },
      ],
    },
    {
      header: t('settings.nav.groupSystem'),
      items: [
        { key: 'update', label: t('settings.nav.update'), Icon: IconUpdate },
        { key: 'backup', label: t('settings.nav.backup'), Icon: IconBackup },
        { key: 'helper', label: t('settings.nav.helper'), Icon: IconHelper },
      ],
    },
    {
      header: t('settings.nav.about'),
      items: [{ key: 'about', label: t('settings.nav.about'), Icon: IconAbout }],
    },
  ];
}

function SetNavItem({
  def,
  active,
  onClick,
}: {
  def: SetNavDef;
  active: boolean;
  onClick: () => void;
}) {
  const { Icon } = def;
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? 'page' : undefined}
      className={cn('set-nav', 'nav-item', active && 'on')}
    >
      <Icon />
      <span>{def.label}</span>
    </button>
  );
}

function GroupHeader({ label }: { label: string }) {
  return <div className="nav-group">{label}</div>;
}

export default function SettingsSidebar() {
  const { t } = useTranslation();
  const settingsScreen = useNavStore((s) => s.settingsScreen);
  const setSettingsScreen = useNavStore((s) => s.setSettingsScreen);
  const backToApp = useNavStore((s) => s.backToApp);
  // 折叠态与主侧栏共享同一 store 字段——两侧 .side 同步折叠/展开（原型 .side.collapsed 不分 id）。
  const collapsed = useNavStore((s) => s.sidebarCollapsed);
  const toggleSidebar = useNavStore((s) => s.toggleSidebar);

  return (
    <nav aria-label="Settings" className={cn('side', collapsed && 'collapsed')}>
      {/* chrome 头：顶部净空 + 窗口拖动槽（空槽，不自绘灯）。拖动靠 `data-tauri-drag-region`，
          不是 `-webkit-app-region`（Electron 约定，Tauri 不认）。见 AppShell 同规注释。 */}
      <div className="side-chrome" data-tauri-drag-region />

      {/* brand：logo 磁贴对齐主侧栏；点击 = 折叠/展开（原型 L1622 data-act="toggle-collapse"，
          折叠开关本体与主侧栏同款——不是「返回应用」，返回应用是贴底那个 nav-item）。 */}
      <button
        type="button"
        onClick={toggleSidebar}
        aria-label={collapsed ? t('sidebar.expand') : t('sidebar.collapse')}
        className="brand"
      >
        <span className="mk">
          <svg viewBox="-46 -46 92 92">
            <use href="#polarisStar" />
          </svg>
        </span>
        <span className="wm">Polaris</span>
        <ChevronLeftIcon className="brand-chev" />
      </button>

      {settingsGroups(t).map((group) => (
        <div key={group.header ?? 'bare'}>
          {group.header && <GroupHeader label={group.header} />}
          {group.items.map((def) => (
            <SetNavItem
              key={def.key}
              def={def}
              active={settingsScreen === def.key}
              onClick={() => setSettingsScreen(def.key)}
            />
          ))}
        </div>
      ))}

      <div className="spacer" />

      {/* 返回应用（贴底，镜像主侧栏 settings 入口位置） */}
      <button type="button" onClick={backToApp} className="nav-item">
        <ChevronLeftIcon />
        <span>{t('settings.nav.back')}</span>
      </button>
    </nav>
  );
}
