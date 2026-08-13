/**
 * SettingsDisplay —— 显示子页（原型 [data-sec="display"]）。
 *
 * 三块：
 *  1. 外观：语言（system / zh / en）+ 主题（system / dark / light）
 *  2. 窗口与通知：关闭主窗口行为（收进托盘 / 退出应用）+ 记忆窗口大小 + 桌面通知 + 自动轻量模式
 *  3. 图形兼容：硬件加速（D 类合成层白屏的用户自救逃生门；仅 Linux/Windows 渲染，见下）
 */

import { useTranslation } from 'react-i18next';

import type { UserConfig } from '@/contracts/types';
import { useAppStore } from '@/store/app-store';
import { Phead, SetBlock, SetRow, Switch, Select, Segmented } from './primitives';
import {
  closeBehaviorOf,
  languageDescKey,
  minimizeToTrayFor,
  shellPlatformFromDataOs,
  showsHardwareAccelRow,
  windowEffectsDescKey,
  type CloseBehavior,
} from './settings-logic';

export interface SettingsDisplayProps {
  config: UserConfig;
  update: (patch: Partial<UserConfig>) => Promise<void>;
}

export default function SettingsDisplay({ config, update }: SettingsDisplayProps) {
  const { t } = useTranslation();
  // 关闭窗口行为派生（minimizeToTray=true → to-tray，false → quit）：双向映射抽在 settings-logic，
  // 由单测锁死往返无损，避免正反两处各写一遍而漂移。
  const closeBehavior: CloseBehavior = closeBehaviorOf(config);

  // 语言归一：config.language 用 'auto' 或语言码；UI 兼容旧 'system' 字面
  const lang = config.language === 'auto' || !config.language ? 'system' : config.language;
  const theme = config.uiTheme ?? 'system';
  // 图形块按平台门控（硬件加速行 mac 不渲染 / 窗口特效文案分 mac·win 两版）。
  const os = shellPlatformFromDataOs();

  return (
    <section className="screen" data-sec="display">
      <Phead title={t('settings.nav.display')} sub={t('settings.display.pageSub')} />

      <SetBlock header={t('settings.nav.appearance')}>
        {/* 语言说明按平台选键：mac 上多一句「原生对话框重启后跟随」（见 languageDescKey）。 */}
        <SetRow label={t('settings.appearance.language')} desc={t(languageDescKey(os))}>
          <Select
            value={lang}
            onChange={(e) => void update({ language: e.target.value === 'system' ? 'auto' : e.target.value })}
            aria-label="Language"
            style={{ width: '180px' }}
          >
            {/* 语言名一律用**该语言自己的写法**（endonym），不随界面语言翻译 —— 界面正显示着一门
                你看不懂的语言时，能认出的恰恰是母语的那个写法。English / Русский / فارسی 本就如此，
                简体中文 / 繁體中文 同理，故这六项不入 locale。 */}
            <option value="system">{t('settings.appearance.system')}</option>
            <option value="zh-CN">简体中文</option>
            <option value="zh-TW">繁體中文</option>
            <option value="en-US">English</option>
            <option value="ru">Русский</option>
            <option value="fa">فارسی</option>
          </Select>
        </SetRow>
        <SetRow label={t('settings.appearance.theme')}>
          <Select
            value={theme}
            onChange={(e) => {
              const next = e.target.value as UserConfig['uiTheme'];
              void update({ uiTheme: next });
              // 同步 app-store（AppShell 读它切 data-theme）：useConfig 与 app-store 是两份独立 config
              // 副本，只更 useConfig 则主题不即时生效（真机「浅色没生效」根因之一，另一为 AppShell 之前
              // 根本没接 uiTheme——已修）。
              useAppStore.setState((s) => (s.config ? { config: { ...s.config, uiTheme: next } } : {}));
            }}
            aria-label="Theme"
            style={{ width: '180px' }}
          >
            <option value="system">{t('settings.appearance.system')}</option>
            <option value="dark">{t('settings.appearance.dark')}</option>
            <option value="light">{t('settings.appearance.light')}</option>
          </Select>
        </SetRow>
      </SetBlock>

      {/* 本块四个控件现已全部接线，无 disabled 标记：
          - minimizeToTray：main.rs 的 `CloseRequested` 读 `config.minimizeToTray` 决定「收进托盘」
            还是「退出应用」，不再只按托盘是否存在二选一。
          - rememberWindowSize：`tauri-plugin-window-state` 按此配置 gate 注册——开启时插件恢复/
            持久化窗口几何，关闭时不注册（窗口回到默认尺寸）。
          - desktopNotifications：App.tsx onError → notifyDesktop（tauri-plugin-notification，setter 同步开关门控）。
          - autoLightweightMode：**主进程**闲置巡检（`src-tauri/src/idle_lightweight.rs`，30s 一拍）
            → tray_enter_lightweight 销毁主窗 webview 释放内存。计时不在前端：要销毁的就是这个
            renderer，把计时器放在里面等于让它自己判自己（隐藏窗的 visibilityState 依平台、
            定时器又被 WKWebView 节流，mac 上因此从不触发）。 */}
      <SetBlock header={t('settings.display.groupWindowNotify')}>
        <SetRow
          label={t('settings.display.closeBehavior')}
          desc={t('settings.display.closeBehaviorDesc')}
        >
          <Segmented<CloseBehavior>
            ariaLabel="Close behavior"
            value={closeBehavior}
            onChange={(v) => void update({ minimizeToTray: minimizeToTrayFor(v) })}
            options={[
              { value: 'to-tray', label: t('settings.display.closeToTray') },
              { value: 'quit', label: t('settings.display.closeQuit') },
            ]}
          />
        </SetRow>
        <SetRow
          label={t('settings.general.rememberWindowSize')}
          desc={t('settings.general.rememberWindowSizeDesc')}
        >
          <Switch
            checked={config.rememberWindowSize !== false}
            onChange={(v) => void update({ rememberWindowSize: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.general.desktopNotifications')}
          desc={t('settings.general.desktopNotificationsDesc')}
        >
          <Switch
            checked={config.desktopNotifications !== false}
            onChange={(v) => void update({ desktopNotifications: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.general.autoLightweightMode')}
          desc={t('settings.general.autoLightweightModeDesc')}
        >
          <Switch
            checked={!!config.autoLightweightMode}
            onChange={(v) => void update({ autoLightweightMode: v })}
          />
        </SetRow>
      </SetBlock>

      {/* 图形兼容：按行按平台门控。**硬件加速行走组件层门控**（`showsHardwareAccelRow(os)`），不再靠
          CSS `:root[data-os="mac"] #set-hwaccel` 隐藏——组件层 render 时按已落定的 data-os 直接决定是否
          入树，比 CSS 隐藏可靠（不吃 data-os post-paint 写入的首帧/时机窗口）。mac 无关 GPU 途径 → 该行
          去掉（用户 2026-07-22 要求），Win/Linux 保留（真能关、是排障逃生门）。窗口特效行仍由 CSS 隐藏
          Linux（`#set-window-effects`，见 components.css），本批不动。
          i18n key 在 `settings.general.*` 命名空间：文案自 上游 承袭、5 语种齐全；Polaris 把图形块归到
          显示页（原型 §B3 显示页自通用页拆出），故键名与页归属不同源，属历史遗留。 */}
      <SetBlock id="set-graphics" header={t('settings.general.groupGraphics', '图形兼容')}>
        {showsHardwareAccelRow(os) && (
          <SetRow
            id="set-hwaccel"
            label={t('settings.general.hardwareAcceleration', '硬件加速')}
            desc={t(
              'settings.general.hardwareAccelerationDesc',
              '关闭后使用软件渲染，可解决部分设备的白屏、花屏、GPU 进程反复崩溃。需重启 Polaris 生效。'
            )}
          >
            {/* 正向语义：默认 true（开），仅显式 false 才判禁 —— 与后端 graphics_compat 的 `!== false` 同口径，
                存量配置（缺该键）行为逐字节不变。 */}
            <Switch
              checked={config.hardwareAcceleration !== false}
              onChange={(v) => void update({ hardwareAcceleration: v })}
            />
          </SetRow>
        )}
        {/* 窗口特效（mac vibrancy / Win Mica）。Linux 由 CSS 隐藏整行：WebKitGTK 无等价物，且后端特效分支
            `#[cfg(mac/win)]` 根本不编译进 Linux → 结构性 no-op，不留「拨了没反应」的死开关。
            文案按平台分两版（mac 毛玻璃 / win Mica），键由 `windowEffectsDescKey(os)` 选。
            正向语义同上：默认 true，仅显式 false 才关，与后端 `should_apply_window_effects` 同口径。
            文案已如实标注需重启（transparent 是 builder-only、运行期不可改，做不了热切）。 */}
        <SetRow
          id="set-window-effects"
          label={t('settings.general.windowEffects', '窗口特效')}
          desc={t(
            windowEffectsDescKey(os),
            'macOS 毛玻璃。关闭后使用纯色背景，可解决窗口空白、合成异常。需重启 Polaris 生效。'
          )}
        >
          <Switch
            checked={config.windowEffects !== false}
            onChange={(v) => void update({ windowEffects: v })}
          />
        </SetRow>
      </SetBlock>
    </section>
  );
}
