/**
 * 「实时日志脱敏」开关的 localStorage 持久 store（C18）—— 与 `use-node-sort-store` 同惯例。
 *
 * 背景：LogsScreen 的 `redactSensitive`（B1）此前**仅**在隐私锁开启（privacyMode）时对日志流的域名/IP
 * 打码。C18 把它扩到**常态可选**：本开关开 → 无论是否隐私锁，实时日志显示层恒对域名/IP 脱敏（复用同一
 * `redactSensitive`，不重写正则）。导出侧（diagnostic_export）本已后端打码，本项只补**实时显示层**。
 *
 * 为何 localStorage 而非 UserConfig（同 use-node-sort-store 理由）：
 *  - 这是**显示偏好**（纯渲染端视图态），非易变数据；用户设一次该跨重启保留。
 *  - 存 UserConfig 会触发 CONFIG_CHANGED → 重启代理（且需改后端 crates/store sanitize 白名单）；显示偏好
 *    不该有此副作用，也不该碰后端配置面。
 *
 * 默认 **false**（关）：默认行为与 B1 一致（仅隐私锁时脱敏），不默认遮蔽域名——实时日志是代理调试的核心
 * 面（看哪个域名命中哪条规则），默认全遮会伤调试。开是 opt-in（愿以调试可见性换常态隐私的用户自取）。
 */
import { create } from 'zustand';

const STORAGE_KEY = 'polaris.logRedactSensitive';

/** 导出供单测直接验证。 */
export function loadLogRedact(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY) === 'true';
  } catch {
    /* 无 localStorage / 受限上下文 → 默认关 */
    return false;
  }
}

function persist(value: boolean): void {
  try {
    localStorage.setItem(STORAGE_KEY, value ? 'true' : 'false');
  } catch {
    /* 持久化失败不阻断 → 退化为会话内记忆 */
  }
}

interface LogRedactState {
  /** true=实时日志常态脱敏域名/IP，false=仅隐私锁时脱敏（默认）。 */
  redactLogs: boolean;
  setRedactLogs: (value: boolean) => void;
  toggleRedactLogs: () => void;
}

export const useLogRedactStore = create<LogRedactState>((set, get) => ({
  redactLogs: loadLogRedact(),
  setRedactLogs: (value) => {
    if (get().redactLogs === value) return; // 值未变，省写 + 省订阅者重渲染
    persist(value);
    set({ redactLogs: value });
  },
  toggleRedactLogs: () => get().setRedactLogs(!get().redactLogs),
}));
