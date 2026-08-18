/**
 * mini 更新弹窗渲染端（380px 角落 toast，五档：remind / progress / done / noupdate / error）。
 *
 * 移植自 Polaris `src/renderer/update-popup/main.ts`，但**首帧来源不同**——这是 #300 的结构性解。
 *
 * # 首帧不依赖 IPC（#300/#301 的渲染端一半）
 *
 * 上游的弹窗靠主进程 push `UPDATE_POPUP_STATE` 才有内容；建窗路径漏发 → `lastPopupState` 恒 null
 * → 重放条件 false → `onState` 永不触发 → 页面永空 → 用户看到无按钮、Esc 失效、无法关闭的实色底窗
 * （取证见 vault 里「更新弹窗『稍后提醒』留白」那份修复记录）。
 *
 * 本移植：初始态由主进程经 Tauri `initialization_script` 注入
 * `window.__POLARIS_UPDATE_POPUP_INITIAL__`，本文件**同步读取即渲染**，零 IPC、零竞态。
 * 后续状态（下载百分比等）才走 event 订阅。于是「窗口有了但页面空着」不再可达。
 *
 * # Esc 逃生（上游 `update-popup/main.ts:150` 的教训）
 *
 * 上游 Esc 处理被 `!current` 短路——**从未收到状态时 Esc 直接失效**，正是 #300 里「无法关闭」的成因。
 * 本文件的 Esc **无条件生效**（不依赖任何状态），即便渲染逻辑整个挂了也能逃生。
 */

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { IPC_CHANNELS } from '@/domain/ipc-channels';
import type { PopupAction, PopupPhase, UpdatePopupState } from '@/contracts/types/update';
import { disableNativeContextMenu } from '@/lib/native-context-menu';
import { createAuxI18n } from '@/i18n/auxiliary';
import { updatePopup as zhCN } from '@/i18n/locales/auxiliary/zh-CN.json';
import { updatePopup as zhTW } from '@/i18n/locales/auxiliary/zh-TW.json';
import { updatePopup as enUS } from '@/i18n/locales/auxiliary/en-US.json';
import { updatePopup as ru } from '@/i18n/locales/auxiliary/ru.json';
import { updatePopup as fa } from '@/i18n/locales/auxiliary/fa.json';
import { exitActionFor } from './exit-action';
import { bytesText, doneSubject } from './state-facts';
import './style.css';

/**
 * 弹窗文案键（`tray/labels.ts` 同款类型保证：`t()` 单参 + 字面量联合 ⇒ 漏改/拼错 = 编译错误）。
 */
type PopupKey = `updatePopup.${Extract<keyof typeof enUS, string>}`;

/**
 * 弹窗 i18n。**语言解析必须自带来源**：本窗可能在主窗尚未初始化（甚至根本没建）时弹出，
 * 故不能等任何主窗状态 —— `createAuxI18n` 同步读 `localStorage['polaris.language']` +
 * `navigator.languages`（与主窗首屏同源、同为 `WebviewUrl::App` 故 localStorage 共享），
 * 零 IPC、零 await，与本文件「首帧不依赖 IPC」的既有不变式同一口径。
 *
 * 本窗是一次性 toast（用完即关），不需要 `refresh()`：语言在建窗那一刻定死即可。
 */
const { t } = createAuxI18n<PopupKey>('updatePopup', {
  'zh-CN': zhCN,
  'zh-TW': zhTW,
  'en-US': enUS,
  ru,
  fa,
});

/**
 * 主 → 弹窗的状态事件名（= Rust `runtime::update_popup::EVENT_POPUP_STATE`）。
 *
 * ⚠️ 这是**跨语言字符串派发**，两侧各硬编码一份，改一边另一边静默失联（窗永远收不到后续状态）。
 * `IPC_CHANNELS.UPDATE_POPUP_STATE` **不是**它：那条常数的值是 `update_popup_state`（命令命名法），
 * 而本事件名是 `event:updatePopupState`；仓里另有第三个同名不同值的 `events.rs:142`
 * （`update:popupState`，零引用）。三份同名不同值的常量该收成一个真值源，
 * 但那要跨 Rust/TS 建生成腿，属**未裁定**项，先把坑标出来。
 */
const EVENT_POPUP_STATE = 'event:updatePopupState';

const root = document.getElementById('update-root');

/** 发动作给主进程；失败静默（弹窗不该因上报失败再抛错）。 */
function sendAction(action: PopupAction): void {
  // 走常量表而非裸字面量：`IPC_CHANNELS` 存在的目的就是防两处漂移，而此处此前是全仓
  // 唯一绕过它的更新弹窗调用点（`IPC_CHANNELS.UPDATE_POPUP_*` 曾零引用）。
  void invoke(IPC_CHANNELS.UPDATE_POPUP_ACTION, { action }).catch(() => {
    /* 主进程不可达：Esc / 关闭仍由主进程 close 兜底 */
  });
}

/** 角标关闭钮（原型 `:2948` 的 `.ut-x`，**每一档恒在**；对拍门逐档核它挂没挂）。 */
function closeButton(phase: PopupPhase | undefined): string {
  // 不用原生 `title=`（G10：无 skip-delay / 键盘不可见 / 不跟深色主题）。本窗是独立 Tauri 窗、
  // 没装主窗那套 tooltip 引擎，故只留 `aria-label`——图标含义（×）本身自明，无需悬浮解释。
  return `<button class="ut-x" data-act="${exitActionFor(phase)}" aria-label="${esc(t('updatePopup.close'))}">
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6 6 18M6 6l12 12"/></svg>
    </button>`;
}

/** HTML 转义：弹窗文案含来自远端 GitHub 数据的字段（版本号等），按不可信输入统一转义。 */
function esc(s: string): string {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

/** 当前 phase —— Esc 按 phase 映射退出动作用（见 `exitActionFor`）。渲染即更新，永不落后一拍。 */
let currentPhase: PopupPhase | undefined;

/** 按阶段渲染。**纯函数式全量重绘**：380px 小窗无 diff 必要，全量重绘杜绝残留态。 */
function render(state: UpdatePopupState): void {
  if (!root) return;
  currentPhase = state.phase;
  const pct = Math.max(0, Math.min(100, state.percentage ?? 0));

  switch (state.phase) {
    case 'remind':
      root.innerHTML = `
        <div class="card">
          ${closeButton(state.phase)}
          <div class="title">${esc(t('updatePopup.newVersion', { version: state.version ?? '' }))}</div>
          <div class="sub">${esc(t('updatePopup.current', { version: state.currentVersion ?? '' }))}</div>
          <div class="row">
            <button class="btn ghost" data-act="skip">${esc(t('updatePopup.skip'))}</button>
            <button class="btn ghost" data-act="viewLog">${esc(t('updatePopup.viewLog'))}</button>
            <button class="btn ghost" data-act="later">${esc(t('updatePopup.later'))}</button>
            <button class="btn primary" data-act="update">${esc(t('updatePopup.update'))}</button>
          </div>
        </div>`;
      break;

    case 'progress':
      root.innerHTML = `
        <div class="card">
          ${closeButton(state.phase)}
          <div class="title">${esc(t('updatePopup.downloading'))}${
            state.mirror ? ` <span class="badge">${esc(t('updatePopup.mirror'))}</span>` : ''
          }</div>
          <div class="bar"><i style="width:${pct}%"></i></div>
          <div class="row between">
            <span class="sub">${esc(bytesText(state.receivedBytes, state.totalBytes) ?? `${pct}%`)}</span>
            <button class="btn ghost" data-act="cancel">${esc(t('updatePopup.cancel'))}</button>
          </div>
        </div>`;
      break;

    // 「完成」得说得出**下的是哪一版、是哪个包**（🟢#6：done 显示的是版本 + 文件名，不是
    // 「落在哪儿」——完整路径放不下，取的是 `doneSubject` 的文件名，见 state-facts.ts）：
    // 不带随行事实的 done 与「什么都没下」在屏幕上长得一模一样 —— 后者此前正是借用本档
    // 收场的（见 `case 'noupdate'`）。
    case 'done': {
      const subject = doneSubject(state.version, state.filePath);
      root.innerHTML = `
        <div class="card">
          ${closeButton(state.phase)}
          <div class="title">${esc(t('updatePopup.downloaded'))}</div>
          <div class="bar"><i style="width:100%"></i></div>
          ${subject ? `<div class="sub path">${esc(subject)}</div>` : ''}
        </div>`;
      break;
    }

    // 用户点了「更新」，复查回来没有任何可下载的包。**不猜成因**：后端判 NoUpdate 有五条路
    // （已是最新 / 该版本已被跳过 / 无正式发布 / 无适配本平台的资产 / 平台不受支持），回包只有
    // 一个 `hasUpdate:false`，挑一条说出来就是拿状态冒充事实。故只陈述确实知道的那件事，
    // 并带上主语（这次弹窗邀请的版本号）。
    case 'noupdate':
      root.innerHTML = `
        <div class="card">
          ${closeButton(state.phase)}
          <div class="title">${esc(t('updatePopup.noUpdate'))}</div>
          ${
            state.version
              ? `<div class="sub">${esc(t('updatePopup.noUpdateSubject', { version: state.version }))}</div>`
              : ''
          }
        </div>`;
      break;

    case 'error':
      root.innerHTML = `
        <div class="card">
          ${closeButton(state.phase)}
          <div class="title err">${esc(t('updatePopup.failed'))}</div>
          <div class="sub">${esc(state.errorText ?? t('updatePopup.unknownError'))}</div>
          <div class="row">
            <button class="btn ghost" data-act="close">${esc(t('updatePopup.close'))}</button>
            <button class="btn ghost" data-act="manualDownload">${esc(t('updatePopup.manualDownload'))}</button>
            <button class="btn primary" data-act="retry">${esc(t('updatePopup.retry'))}</button>
          </div>
        </div>`;
      break;

    default:
      // 未知 phase（Rust 侧加了新态但前端没跟上）：**不留空白窗**——如实显示并给逃生按钮。
      // 空白窗正是 #300 的形态，宁可显示一句「未知状态」也不能什么都不画。
      root.innerHTML = `
        <div class="card">
          ${closeButton(undefined)}
          <div class="title err">${esc(t('updatePopup.unknownState'))}</div>
          <div class="row"><button class="btn primary" data-act="close">${esc(t('updatePopup.close'))}</button></div>
        </div>`;
  }
}

// ── 事件接线 ──

// 弹窗是独立 webview（独立 html 入口），主窗那次监听盖不到它 —— 单独关一次系统右键菜单。
// 本窗无输入框，判据里那条「可编辑文本控件放行」在这儿只是恒不命中，不是特例（见该模块头注）。
disableNativeContextMenu();

// 按钮：事件委托（全量重绘后 listener 不失效）。
root?.addEventListener('click', (e) => {
  const el = (e.target as HTMLElement)?.closest('[data-act]');
  const act = el?.getAttribute('data-act') as PopupAction | undefined;
  if (act) sendAction(act);
});

// Esc 逃生：**无条件生效**（上游 `!current` 短路 = #300 的「Esc 失效」成因），但发什么动作按 phase 映射
// —— 恒发 `close` 会在 progress 态被后端白名单静默拒收，等于死键（见 `exitActionFor`）。
window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') sendAction(exitActionFor(currentPhase));
});

// 首帧：同步读注入的初始态。**这是 #300 整类 bug 的解**——无需等任何 IPC。
const initial = window.__POLARIS_UPDATE_POPUP_INITIAL__;
if (initial) {
  render(initial);
} else {
  // 不可达（主进程 open() 必然注入）。真到这儿说明不变式被破坏 —— 显式报错 + 给逃生按钮，
  // 绝不留空白窗（空白窗 = 无按钮 + 无法关闭 = #300 的用户体感）。
  render({ phase: 'error', errorText: t('updatePopup.bootstrapMissing') });
}

// 后续状态：订阅主进程推送（progress 百分比流转 / did-finish-load 重放）。
void listen<UpdatePopupState>(EVENT_POPUP_STATE, (ev) => render(ev.payload));
