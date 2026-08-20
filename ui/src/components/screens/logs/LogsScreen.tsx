/**
 * Logs 屏 —— 逐字对齐原型 polaris-prototype.html #s-logs（L2041-2076）。
 *
 * 页面结构：
 *  - .phead（h1 + .sub）
 *  - #log-privacy-note.mode-warn（隐私锁提示条，原型内联覆盖 flow 色而非默认 warn 色）
 *  - .card.log-toolbar：
 *      .log-tb-primary（日志级别 / 来源 + 诊断模式 / 导出 / 日志目录）
 *      .log-tb-main（搜索 + 自动滚动 / 脱敏 / 复制 / 清空）
 *    开关态由底色与 `aria-pressed` 同时表达；清空使用原地二次确认，暂停缓冲数由 `.cnt` 徽标承载。
 *  - #log-view.log-view（当前页 = 原型 renderLogs() 的 .log-line 模板逐行 .map）
 *  - .log-foot（.log-live + 完整结果行数 + 分页）
 *
 * 接线（保留，见 vault ~/docs/polaris/design/polaris-ui-rebuild-plan.md C3 台账）：
 *  - useAppStore(config/saveConfig/privacyMode)
 *  - api.logs.get（初始水合 + 页面所有权登记）/ .onReceivedBatchReady（监听就绪后接 ~150ms 合批）/
 *    .unsubscribe（离页释放）/ .clear / .export（纯日志导出）
 *  - api.diagnostic.export（Markdown 诊断报告）与 api.logs.export（纯日志）收进同一个 GUI 导出菜单。
 *  - 级别：configApi.save 写 config.logLevel（核记录 + 视图显示同一级别，无独立视图侧级别）。
 *    后端经 config.rs::broadcast_config_changed → logging::set_level 即刻改 max_level，而**内核日志
 *    也由同一个 max_level 在客户端筛**（proxy.rs 的核日志 relay 订阅管理 API `SubscribeLog`，该流恒
 *    全级别）⇒ 本页两侧日志与受管落盘改完即刻跟上。仍需重启内核的只有 pre-ready/helper stderr 级别。
 *  - 会话诊断：后端进程态 `logs:setDiagnostic` 临时把 app sink + sing-box 实时 relay 抬到 DEBUG，
 *    不写 config、不重启核；页面卸载不误关，应用退出后状态自然消失。pre-ready/helper stderr 级别
 *    仍由内核状态标签如实显示（管理 API 没有 setter，不能伪装成已改变）。
 *  - 核在跑的真实级别：api.logs.runtimeLevel（管理 API `GetDefaultLogLevel`）→ `.log-core-lvl` 徽标。
 *    分段控件显示的是**我写下的值**，这颗徽标显示的是**核此刻实际在用的值**。
 *    **它管的是核 pre-ready/helper stderr 的默认级别**；本页显示和受管实时日志不受它影响
 *    （两者恒按分段控件选的级别在客户端筛）。
 *    与控件相同、核未运行或首次读取中均不占位置；仅不一致或读取失败时显示，成因二分
 *    （暂存未应用 / 核没重启）见 runtime-level.ts。
 *  - 来源：后端 logging.rs::ui_source 把 target 归一为 'sing-box' | 'app'（裸 log::info! 默认 target
 *    是 Rust 模块路径，不归一则「应用」筛选恒空）
 *  - follow：暂停时缓冲新行入 pendingRef，恢复回填（对齐原型 logPending + updateLogPausedLabel）；
 *    用户上滚脱离底部亦自动暂停 follow 并露出「回到底部」（契约：自动吸底跟随 + 回到底部）
 *  - 清空：confirmTwice 双击确认模式（对齐原型 L3211，2.6s 超时自动回退，非 onBlur）
 *
 * #log-view 空状态不渲染占位文案：原型 renderLogs() 对空数组是 `[].map().join('')`＝空字符串，
 *     无占位 markup；逐字复现故不额外发明空态提示。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute } from '@/lib/staged-config';
import { useLogRedactStore } from '@/store/use-log-redact-store';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { redactSensitive, shouldRedactLogs } from '@/domain/privacy';
import { mergeHydration, maxLogId, type LogRow } from './logs-buffer';
import { runtimeLevelView } from './runtime-level';
import type { LogLevel, RuntimeLogLevel } from '@/contracts/types';
import { Csel, type CselOption } from '@/components/dialogs/Csel';
import { InfoIcon } from '@/components/InfoIcon';
import { ListPager, pageWindow } from '@/components/ListPager';
import { fmtBytes } from '@/components/screens/shared/format';
import { isUpwardLogScrollKey, shouldPauseLogFollow } from './follow-scroll';

/** 级别 → 数字权重（对齐原型 LVL，越大越严重）。 */
const LVL: Record<LogLevel, number> = {
  debug: 0,
  info: 1,
  warn: 2,
  error: 3,
  fatal: 4,
};
const LEVEL_OPTS: LogLevel[] = ['debug', 'info', 'warn', 'error', 'fatal'];
const LEVEL_SELECT_OPTIONS: CselOption[] = LEVEL_OPTS.map((value) => ({
  value,
  label: value.toUpperCase(),
}));
/** IPC / 页面内存结果上限。它不是检索历史上限；非空搜索由后端扫描完整保留环。 */
const MAX_BUFFERED_ROWS = 500;
/** 只限制实际挂载的日志 DOM；500 条内存结果、搜索范围与实时到达节奏均保持不变。 */
const LOG_PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 180;
let logSubscriptionSeq = 0;

/** 同一 renderer 内单调唯一即可；后端按 window + token 防陈旧 cleanup 误退新页面。 */
function nextLogSubscriptionId(): string {
  logSubscriptionSeq += 1;
  return `logs-${Date.now()}-${logSubscriptionSeq}`;
}
/** 本屏的原地二次确认键；超时/复位语义全在 `lib/confirm-twice.ts`，不另起定时器。 */
const CLEAR_KEY = 'logs-clear';
const DELETE_LEGACY_KEY = 'logs-delete-legacy';
/** 核内级别的重读间隔（仅 Logs 屏挂载期间）。理由见下方轮询 effect 的注释。 */
const RUNTIME_LEVEL_POLL_MS = 5000;

type LogSource = 'all' | 'sing-box' | 'app';

interface LogSearchSnapshot {
  key: string;
  rows: LogRow[];
}

function searchKey(query: string, level: LogLevel, source: LogSource): string {
  return `${level}\u0000${source}\u0000${query}`;
}

function matchesLog(row: LogRow, threshold: number, source: LogSource, query: string): boolean {
  return (
    LVL[row.level] >= threshold &&
    (source === 'all' || row.source === source) &&
    (!query || (row.message + row.level).toLowerCase().includes(query))
  );
}

/* `LogRow` 与两条腿的合并语义已下沉 `./logs-buffer.ts`（纯函数 + 单测），见该文件模块头。 */

/** 时间戳 → HH:MM:SS（对齐原型 log-ts 格式）。 */
function fmtTs(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function LogsScreen() {
  const { t } = useTranslation();
  /** 展示口径：级别下拉要回显用户刚改的值，否则暂存一开「改完级别没变」。 */
  const config = useEffectiveConfig();
  /** 磁盘口径：落盘入参的基准（拿暂存合成值当基准会把未应用的暂存值一并落盘）。 */
  const diskConfig = useAppStore((s) => s.config);
  const saveConfig = useAppStore((s) => s.saveConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  const privacyMode = useAppStore((s) => s.privacyMode);
  // C18 实时日志脱敏偏好（localStorage，与隐私锁正交）：开 → 常态对域名/IP 打码；隐私锁开时恒脱敏。
  const redactLogs = useLogRedactStore((s) => s.redactLogs);
  const toggleRedactLogs = useLogRedactStore((s) => s.toggleRedactLogs);

  const [logs, setLogs] = useState<LogRow[]>([]);
  /** 核在跑的真实日志级别（`null` = 还没取回；轮询见下方 effect）。 */
  const [runtimeLevel, setRuntimeLevel] = useState<RuntimeLogLevel | null>(null);
  /** `null` = 尚未从后端进程态水合；boolean = 本次应用运行是否临时抬到 DEBUG。 */
  const [diagnosticMode, setDiagnosticMode] = useState<boolean | null>(null);
  const [diagnosticBusy, setDiagnosticBusy] = useState(false);
  const [legacyLog, setLegacyLog] = useState<{ exists: boolean; bytes: number; path: string } | null>(null);
  const [legacyActionBusy, setLegacyActionBusy] = useState(false);
  const [exportMenuOpen, setExportMenuOpen] = useState(false);
  const [source, setSource] = useState<LogSource>('all');
  const [search, setSearch] = useState('');
  const [searchSnapshot, setSearchSnapshot] = useState<LogSearchSnapshot | null>(null);
  const [logStreamReady, setLogStreamReady] = useState(false);
  const [follow, setFollow] = useState(true);
  const [logPage, setLogPage] = useState(0);
  /** 直播监听只在 mount 时建立一次；用 ref 读取最新 follow，避免每次暂停都退订、重水合。 */
  const followRef = useRef(follow);
  followRef.current = follow;
  /** 暂停时缓冲的新行（恢复即回填，对齐原型 logPending）；count 单独入 state 供 label 响应式渲染。 */
  const pendingRef = useRef<LogRow[]>([]);
  const [pendingCount, setPendingCount] = useState(0);
  /** 内核级别读请求的世代号：轮询与生命周期事件并发时，只让最新一次回包落地。 */
  const runtimeReadSeqRef = useRef(0);
  const runtimeMountedRef = useRef(false);
  /**
   * 已收下的最大 `_id`（去重游标）。清空日志**不复位**：后端 seq 全局单调、清空也不重置发号器，
   * 复位反而会让清空前的残留批次被当成新行重新收下。
   */
  const lastIdRef = useRef(-1);
  /** 只记最近一次明确的用户滚动输入；首次水合/程序化吸底没有这个证据。 */
  const lastUserScrollIntentAtRef = useRef<number | null>(null);
  /** follow 态的真实末页会随结果数变化；暂停前把这一页固化，避免瞬间跳回旧页。 */
  const displayedLogPageRef = useRef(0);

  /** 清空的原地二次确认 —— 走全仓唯一实现（`lib/confirm-twice.ts`），不再自己管定时器。 */
  const { armed, confirmTwice } = useConfirmTwice();
  const confirmClear = armed === CLEAR_KEY;
  const confirmDeleteLegacy = armed === DELETE_LEGACY_KEY;

  const viewRef = useRef<HTMLDivElement>(null);
  const exportMenuRef = useRef<HTMLDivElement>(null);

  const level: LogLevel = config?.logLevel ?? 'info';
  /** 诊断模式不改持久级别控件，只临时把本页实际显示门槛抬到 DEBUG。 */
  const displayLevel: LogLevel = diagnosticMode ? 'debug' : level;
  const threshold = LVL[displayLevel];
  const query = search.trim().toLowerCase();
  const activeSearchKey = searchKey(query, displayLevel, source);
  /** 直播回调只挂载一次，经 ref 读取当前查询，避免每次键入都重建底层事件监听。 */
  const searchCriteriaRef = useRef({
    key: activeSearchKey,
    query,
    threshold,
    source,
  });
  searchCriteriaRef.current = {
    key: activeSearchKey,
    query,
    threshold,
    source,
  };
  /**
   * 核在跑的真实级别徽标（纯投影）。第三参取**盘上**那份 logLevel（非 staged 合并值），
   * 用来区分「改动还在暂存区」与「已落盘但核没重启」—— 两者补救动作不同。
   * 不变量与方向性见 runtime-level.ts。
   */
  const runtimeView = runtimeLevelView(runtimeLevel, level, diskConfig?.logLevel ?? null);
  /**
   * 徽标浮窗按分叉成因分文案（补救动作不同：应用+重启 / 只需重启）。
   * 写成三元而非 `t(\`logs.coreLevelDrift.${d}\`)` 是**故意的**：i18n-coverage 的 G6b
   * （声明而无消费点 = 死键）只认静态字面量键，拼出来的键会让这两条被判成死键。
   */
  const coreLevelTip =
    runtimeView.kind === 'known' && runtimeView.drift === 'unsaved'
      ? t('logs.coreLevelDriftUnsaved')
      : runtimeView.kind === 'known' && runtimeView.drift === 'coreRestart'
        ? t('logs.coreLevelDriftRestart')
        : t('logs.coreLevelHint');
  /** 相同、未运行与读取中的状态都不占工具栏；只有不一致或读取失败才需要用户注意。 */
  const runtimeLevelNotice =
    runtimeView.kind === 'known' && runtimeView.drift
      ? t('logs.coreLevelPending', { level: runtimeView.level.toUpperCase() })
      : runtimeView.kind === 'unavailable'
        ? t('logs.coreLevelUnavailable')
        : null;

  /**
   * 重读核内级别的单一入口。定时兜底、生命周期事件都走这里，避免各存一份错误处理。
   * 世代号防止「旧轮询慢回包」覆盖「重启 ready 后的新真值」。
   */
  const refreshRuntimeLevel = useCallback(async () => {
    const seq = ++runtimeReadSeqRef.current;
    try {
      const next = await api.logs.runtimeLevel();
      if (runtimeMountedRef.current && seq === runtimeReadSeqRef.current) {
        setRuntimeLevel(next);
      }
    } catch {
      /* 非 Tauri（mock）/ IPC 抛错：保持上一份可自证状态，不编一个级别。 */
    }
  }, []);

  /**
   * 按 `_id` 去重并推进游标：只放行 `_id` 大于已见最大值的行。
   *
   * 缺 `_id`（非 Tauri mock / 旧后端）→ 一律放行：宁可偶有重复行，也不能因为字段缺失把日志吞掉——
   * 日志页是排障的最后一根线。
   */
  const dedupe = useCallback((batch: LogRow[]): LogRow[] => {
    const fresh: LogRow[] = [];
    for (const l of batch) {
      if (typeof l._id !== 'number') {
        fresh.push(l);
        continue;
      }
      if (l._id <= lastIdRef.current) continue;
      lastIdRef.current = l._id;
      fresh.push(l);
    }
    return fresh;
  }, []);

  /* ── 页面级日志所有权：先监听，再水合 / 登记；卸载时监听与后端订阅一起释放 ──
   *
   * **合并进缓冲，不是整体替换，也不走游标去重**：两条腿同时起跑，订阅腿完全可能先送到一批
   * （核在高频输出时必然如此）。走 `dedupe` + `setLogs(...)` 的那一版会把游标推到 N 之后再拿快照
   * 去重成空，然后用空数组替换掉已入列的行 ⇒ 进页历史区恒空直到新行到达。理由详见 logs-buffer.ts。
   * 游标仍要**快进到快照最大 id**，否则快照里比游标新的行会被随后的流式批当成新行再收一次。 */
  useEffect(() => {
    let alive = true;
    const subscriptionId = nextLogSubscriptionId();
    let off: (() => void) | null = null;
    const onBatch = (raw: LogRow[]) => {
      if (!Array.isArray(raw) || raw.length === 0) return;
      const batch = dedupe(raw);
      if (batch.length === 0) return;
      const criteria = searchCriteriaRef.current;
      if (criteria.query) {
        const matched = batch.filter((row) =>
          matchesLog(row, criteria.threshold, criteria.source, criteria.query)
        );
        if (matched.length > 0) {
          setSearchSnapshot((previous) => ({
            key: criteria.key,
            rows: mergeHydration(
              previous?.key === criteria.key ? previous.rows : [],
              matched,
              MAX_BUFFERED_ROWS
            ),
          }));
        }
      }
      if (followRef.current) {
        setLogs((prev) => {
          const next = [...prev, ...batch];
          return next.length > MAX_BUFFERED_ROWS ? next.slice(-MAX_BUFFERED_ROWS) : next;
        });
      } else {
        pendingRef.current.push(...batch);
        if (pendingRef.current.length > MAX_BUFFERED_ROWS) {
          pendingRef.current = pendingRef.current.slice(-MAX_BUFFERED_ROWS);
        }
        setPendingCount(pendingRef.current.length);
      }
    };

    void (async () => {
      // Tauri 的 listen 是异步登记：必须等监听真就绪后才让 logs_get 唤醒 emitter，消除首批丢失窗口。
      off = await api.logs.onReceivedBatchReady(onBatch);
      if (!alive) {
        off();
        off = null;
        return;
      }
      setLogStreamReady(true);
      const batch = await api.logs.get(subscriptionId, MAX_BUFFERED_ROWS);
      if (!alive) {
        // invoke 可能晚于 React cleanup 返回；token 退订只会删除这个陈旧实例。
        void api.logs.unsubscribe(subscriptionId);
        return;
      }
      if (!Array.isArray(batch)) return;
      const snapshot = batch.slice(-MAX_BUFFERED_ROWS);
      const top = maxLogId(snapshot);
      if (top !== null && top > lastIdRef.current) lastIdRef.current = top;
      setLogs((prev) => mergeHydration(prev, snapshot, MAX_BUFFERED_ROWS));
    })()
      .catch(() => {
        /* 非 Tauri 忽略 */
      });
    return () => {
      alive = false;
      off?.();
      off = null;
      void api.logs.unsubscribe(subscriptionId);
    };
  }, [dedupe]);

  /* 非空查询走后端完整保留环；结果上限只约束 IPC/DOM。直播匹配行由上方 onBatch 同步并入同一快照。 */
  useEffect(() => {
    if (!query) {
      setSearchSnapshot(null);
      return;
    }
    if (!logStreamReady) return;
    let alive = true;
    const key = activeSearchKey;
    const timer = window.setTimeout(() => {
      void api.logs
        .search(query, displayLevel, source, MAX_BUFFERED_ROWS)
        .then((batch) => {
          if (!alive || !Array.isArray(batch)) return;
          setSearchSnapshot((previous) => ({
            key,
            rows: mergeHydration(
              previous?.key === key ? previous.rows : [],
              batch,
              MAX_BUFFERED_ROWS
            ),
          }));
        })
        .catch(() => {
          /* 保持直播已命中的结果；IPC 失败会由收口层记录。 */
        });
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [activeSearchKey, displayLevel, logStreamReady, query, source]);

  /* 会话诊断态由后端进程持有：换屏会卸载本组件，但不能因此误关诊断；重挂时读回即可。 */
  useEffect(() => {
    let alive = true;
    api.logs
      .diagnosticState()
      .then((enabled) => {
        if (alive) setDiagnosticMode(enabled);
      })
      .catch(() => {
        if (alive) setDiagnosticMode(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    runtimeMountedRef.current = true;
    return () => {
      runtimeMountedRef.current = false;
      // 让所有已在飞的请求失效，即使它们在卸载后才回包也不会写 state。
      runtimeReadSeqRef.current += 1;
    };
  }, []);

  /* ── 核在跑的真实级别：事件推送为主，5s 轮询兜底（仅本屏挂载期间）──
   *
   * # 为什么是轮询，而不是「监听某个事件后重取」
   *
   * 核内级别只在**起核那一刻**定下（生成配置时注入）。`event:proxyLifecycle`
   * 在 ready / stopped / failed 真跃迁点发出，收到即重读，消除「核已重启但徽标还旧」的 0—5s 人为滞后。
   *
   * 5s 轮询仍保留为事件丢失/非标准换核的兜底；本屏卸载即停，不留后台轮询。
   */
  useEffect(() => {
    void refreshRuntimeLevel();
    const timer = window.setInterval(() => void refreshRuntimeLevel(), RUNTIME_LEVEL_POLL_MS);
    return () => {
      window.clearInterval(timer);
    };
  }, [refreshRuntimeLevel]);

  /* 内核真跃迁后立即重读；事件只是信号，级别真值仍由 logs:runtimeLevel 回读。 */
  useEffect(() => api.proxy.onLifecycle(() => void refreshRuntimeLevel()), [refreshRuntimeLevel]);

  /* 导出是 GUI 内动作菜单：点击外部 / Esc 收起，避免系统原生菜单在三平台呈现不同皮肤。 */
  useEffect(() => {
    if (!exportMenuOpen) return;
    const onDown = (event: MouseEvent) => {
      if (!exportMenuRef.current?.contains(event.target as Node)) setExportMenuOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setExportMenuOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [exportMenuOpen]);

  const scrollToBottom = useCallback(() => {
    const el = viewRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  const pauseFollow = useCallback(() => {
    followRef.current = false;
    setLogPage(displayedLogPageRef.current);
    setFollow(false);
  }, []);

  /* ── follow 时自动滚动到底 ── */
  useEffect(() => {
    if (follow) scrollToBottom();
  }, [logs, follow, scrollToBottom]);

  /* ── 离底检测：只有用户滚动脱离底部才暂停 follow（新行转入 pending）──
   *
   * 无条件吸底的话，用户往上翻查一条报错时每来一批新日志就被拽回底部，等于看不了历史。
   *
   * 只做「离底 → 暂停」，不做「回底 → 自动恢复」：恢复要把 pending 一次性回填并跳到底部，那是个有
   * 可见后果的动作，得由「回到底部」/「自动滚动」按钮显式触发，不该被一次滚过头的滚轮顺带做掉。
   *
   * 关键是 scroll 不等于用户滚动：首次水合（缓冲可达 500 行、DOM 按页）、字体/layout 变化、`scrollTop`
   * 程序化吸底都可能发 scroll。因此先记 wheel/touch/pointer/向上键盘的近期意图，再由
   * [`shouldPauseLogFollow`] 合取“正在 follow + 有用户意图 + 已离底”；程序事件不能再把 follow 打掉。 */
  useEffect(() => {
    const el = viewRef.current;
    if (!el) return;
    const markUserIntent = () => {
      lastUserScrollIntentAtRef.current = performance.now();
    };
    const onPointerMove = (event: PointerEvent) => {
      if (event.buttons !== 0) markUserIntent();
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (isUpwardLogScrollKey(event.key, event.shiftKey)) markUserIntent();
    };
    const onScroll = () => {
      if (
        shouldPauseLogFollow({
          follow: followRef.current,
          metrics: el,
          lastUserIntentAt: lastUserScrollIntentAtRef.current,
          now: performance.now(),
        })
      ) {
        pauseFollow();
      }
    };
    el.addEventListener('wheel', markUserIntent, { passive: true });
    el.addEventListener('touchstart', markUserIntent, { passive: true });
    el.addEventListener('pointerdown', markUserIntent, { passive: true });
    el.addEventListener('pointermove', onPointerMove, { passive: true });
    el.addEventListener('keydown', onKeyDown);
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => {
      el.removeEventListener('wheel', markUserIntent);
      el.removeEventListener('touchstart', markUserIntent);
      el.removeEventListener('pointerdown', markUserIntent);
      el.removeEventListener('pointermove', onPointerMove);
      el.removeEventListener('keydown', onKeyDown);
      el.removeEventListener('scroll', onScroll);
    };
  }, [pauseFollow]);

  /* 清空按钮的两段式确认超时由 `useConfirmTwice` 自己在卸载时清理，此处不再各管一份。 */

  /* ── 级别切换：写 config.logLevel（核记录 + 视图显示同一级别）── */
  const onLevelChange = useCallback(
    async (v: LogLevel) => {
      if (!diskConfig) return;
      // 配置暂存闸门（与 NodeDialog 同形）。`logLevel` 本轮起是 `UserConfig` 字段（Class B）——
      // 它喂 sing-box 的 `log.level`，改了核要重启才跟上（「第四类重启」，spec §Q3-b E-3）。
      // 此前被判豁免直落盘 ⇒ 核继续按旧级别跑而暂存条只字不提，正是要修的那条静默。
      if (editRoute('logLevel', stagingEnabled) === 'staged') {
        stage({
          id: 'setting:logLevel',
          kind: 'setting',
          label: `${t('logs.currentLevel')} ${v}`,
          entityPath: ['logLevel'],
          nextValue: v,
        });
        return; // 零 IPC 写、零磁盘写（FR-1）
      }
      try {
        await saveConfig({ ...diskConfig, logLevel: v });
      } catch (err) {
        console.error('[logs] set level failed:', err);
      }
    },
    [diskConfig, saveConfig, stagingEnabled, stage, t]
  );

  /** 会话级诊断：只改本次 Rust 进程的有效门槛，绝不走 saveConfig / staged config。 */
  const onToggleDiagnostic = useCallback(async () => {
    if (diagnosticBusy) return;
    setDiagnosticBusy(true);
    try {
      const enabled = await api.logs.setDiagnostic(!(diagnosticMode ?? false));
      setDiagnosticMode(enabled);
    } catch (err) {
      console.error('[logs] toggle diagnostic failed:', err);
      toast.error(
        t('logs.diagnosticFailed'),
        err instanceof Error ? err.message : undefined,
      );
    } finally {
      setDiagnosticBusy(false);
    }
  }, [diagnosticBusy, diagnosticMode, t]);

  /* ── 清空：confirmTwice 原地二次确认（对齐原型 :4130 log-clear，2.6s 未二次点击自动回退）──
   * 原型 log-clear → confirmTwice 后 notify('日志已清空')（中性 kind）。 */
  const onClearClick = useCallback(() => {
    confirmTwice(CLEAR_KEY, () => {
      void (async () => {
        try {
          await api.logs.clear();
          setLogs([]);
          setSearchSnapshot(null);
          pendingRef.current = [];
          setPendingCount(0);
          toast.info(t('logs.clearDone'));
        } catch (err) {
          console.error('[logs] clear failed:', err);
          toast.error(
            t('logs.clearFailed'),
            err instanceof Error ? err.message : undefined,
          );
        }
      })();
    });
  }, [confirmTwice, t]);

  /* ── 导出 Markdown 诊断报告：配置、版本、运行态与脱敏日志。取消保存对话框不算失败。 */
  const onExportDiag = useCallback(async () => {
    try {
      const res = await api.diagnostic.export();
      if (res.success) {
        toast.success(t('logs.exportDiagDone'));
      } else if (res.error !== 'cancelled') {
        toast.error(t('logs.exportDiagFailed'), res.error);
      }
    } catch (err) {
      console.error('[logs] export diag failed:', err);
      toast.error(t('logs.exportDiagFailed'), err instanceof Error ? err.message : undefined);
    }
  }, [t]);

  /* ── 导出纯日志：不含配置与运行态；与诊断报告是导出菜单里的两个明确产物。 */
  const onExportLogs = useCallback(async () => {
    try {
      const res = await api.logs.export();
      if (res.success) {
        toast.success(t('logs.exportDone'));
      } else if (res.error !== 'cancelled') {
        toast.error(t('logs.exportFailed'), res.error);
      }
    } catch (err) {
      console.error('[logs] export failed:', err);
      toast.error(t('logs.exportFailed'), err instanceof Error ? err.message : undefined);
    }
  }, [t]);

  /* ── 打开日志目录（G3，原型 :2065 `open-log-dir` + :4122 notify「已在文件管理器中打开日志目录」）──
   * 路径解析与 shell.open 都在后端一步做完（前端拼路径会在三平台 / portable 形态上分叉）。 */
  const onOpenLogDir = useCallback(async () => {
    try {
      await api.logs.openDir();
      toast.success(t('logs.openDirDone'));
    } catch (err) {
      console.error('[logs] open dir failed:', err);
      toast.error(
        t('logs.openDirFailed'),
        err instanceof Error ? err.message : undefined,
      );
    }
  }, [t]);

  /* W26 历史日志只读探测：当前受管日志已搬到 logs/，旧 singbox.log 不会被自动改写/删除。 */
  useEffect(() => {
    let active = true;
    void api.logs
      .legacyInfo()
      .then((info) => {
        if (active) setLegacyLog(info);
      })
      .catch((err) => console.error('[logs] read legacy log info failed:', err));
    return () => {
      active = false;
    };
  }, []);

  const onArchiveLegacy = useCallback(async () => {
    if (legacyActionBusy) return;
    setLegacyActionBusy(true);
    try {
      const result = await api.logs.archiveLegacy();
      if (result.success) {
        setLegacyLog((current) => current && { ...current, exists: false, bytes: 0 });
        if (result.archived) toast.success(t('logs.archiveLegacyDone'));
      } else if (result.error !== 'cancelled') {
        toast.error(t('logs.archiveLegacyFailed'), result.error);
      }
    } catch (err) {
      console.error('[logs] archive legacy log failed:', err);
      toast.error(t('logs.archiveLegacyFailed'), err instanceof Error ? err.message : undefined);
    } finally {
      setLegacyActionBusy(false);
    }
  }, [legacyActionBusy, t]);

  const onDeleteLegacyClick = useCallback(() => {
    confirmTwice(DELETE_LEGACY_KEY, () => {
      void (async () => {
        if (legacyActionBusy) return;
        setLegacyActionBusy(true);
        try {
          const result = await api.logs.deleteLegacy();
          setLegacyLog((current) => current && { ...current, exists: false, bytes: 0 });
          if (result.deleted) toast.success(t('logs.deleteLegacyDone'));
        } catch (err) {
          console.error('[logs] delete legacy log failed:', err);
          toast.error(
            t('logs.deleteLegacyFailed'),
            err instanceof Error ? err.message : undefined,
          );
        } finally {
          setLegacyActionBusy(false);
        }
      })();
    });
  }, [confirmTwice, legacyActionBusy, t]);

  /* 空搜索过滤当前绘制尾部；非空搜索消费后端完整保留环返回的独立结果集。 */
  const visible = useMemo(
    () => {
      const rows = query
        ? searchSnapshot?.key === activeSearchKey
          ? searchSnapshot.rows
          : []
        : logs;
      return rows.filter((row) => matchesLog(row, threshold, source, query));
    },
    [activeSearchKey, logs, query, searchSnapshot, source, threshold]
  );
  const logPagination = pageWindow(
    visible.length,
    follow ? Number.MAX_SAFE_INTEGER : logPage,
    LOG_PAGE_SIZE
  );
  displayedLogPageRef.current = logPagination.page;
  const renderedLogs = visible.slice(logPagination.start, logPagination.end);

  useEffect(() => {
    if (!follow && logPage !== logPagination.page) setLogPage(logPagination.page);
  }, [follow, logPage, logPagination.page]);

  /** 是否对日志正文脱敏（隐私锁 或 C18 常态脱敏偏好）—— 渲染与复制**同一判定、同一函数**。 */
  const redacting = shouldRedactLogs(privacyMode, redactLogs);
  const renderMessage = useCallback(
    (msg: string) => (redacting ? redactSensitive(msg) : msg),
    [redacting]
  );

  /* ── 复制当前筛选结果到剪贴板（分页只约束 DOM，不缩小复制范围）──
   * 复用 `visible` + `renderMessage`：复制路径此前自己抄了一份过滤条件、且**绕过了脱敏**——脱敏开关/
   * 隐私锁全开时点复制，粘出来的仍是明文域名与 IP（用户以为已脱敏，直接贴进 issue）。两份过滤条件
   * 各自演化本身就是这类漂移的成因，故一并收敛到同一处，而不是在副本上补一个 redact 调用。 */
  const onCopy = useCallback(async () => {
    const text = visible
      .map(
        (l) =>
          `[${fmtTs(l.timestamp)}] ${l.level.toUpperCase()}: ${renderMessage(l.message)}`
      )
      .join('\n');
    try {
      await navigator.clipboard.writeText(text);
      // 原型 :log-copy notify('已复制日志','ok')。
      toast.success(t('logs.copyDone'));
    } catch (err) {
      console.error('[logs] copy failed:', err);
      toast.error(t('common.copyFail'));
    }
  }, [visible, renderMessage, t]);

  /* ── 恢复跟随：回填 pending 并吸回底部（对齐原型 log-follow case）── */
  const resumeFollow = useCallback(() => {
    if (pendingRef.current.length) {
      const pending = pendingRef.current;
      pendingRef.current = [];
      setPendingCount(0);
      setLogs((prev) => {
        const merged = [...prev, ...pending];
        return merged.length > MAX_BUFFERED_ROWS ? merged.slice(-MAX_BUFFERED_ROWS) : merged;
      });
    }
    followRef.current = true;
    setFollow(true);
    scrollToBottom();
  }, [scrollToBottom]);

  const toggleFollow = useCallback(() => {
    if (follow) pauseFollow();
    else resumeFollow();
  }, [follow, pauseFollow, resumeFollow]);

  const onLogPageChange = useCallback((page: number) => {
    followRef.current = false;
    setFollow(false);
    setLogPage(page);
    const element = viewRef.current;
    if (element) element.scrollTop = 0;
  }, []);

  /* ── 搜索高亮：转义正则，<mark> 包裹命中（对齐原型 renderLogs 的 mark 替换）── */
  const highlight = useCallback(
    (msg: string) => {
      if (!query) return msg;
      try {
        const esc = query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        const parts = msg.split(new RegExp(`(${esc})`, 'ig'));
        return parts.map((p, i) =>
          p.toLowerCase() === query ? <mark key={i}>{p}</mark> : <span key={i}>{p}</span>
        );
      } catch {
        return msg;
      }
    },
    [query]
  );

  /* follow 按钮的文案已固定为「自动滚动」（态由底色表达），但**暂停期间缓冲了多少行**这条信息
     不能跟着丢 —— 它原先挂在 `autoScrollPausedCount` 那个变体文案里，是全屏唯一的出口。
     改由按钮上的 `.cnt` 计数徽标承载（同 `.nav-item .cnt` 的既有形态）。 */
  const pendingBadge = !follow && pendingCount > 0 ? pendingCount : null;
  const sourceOptions: CselOption[] = [
    { value: 'all', label: t('common.all') },
    { value: 'sing-box', label: 'sing-box' },
    { value: 'app', label: t('logs.sourceApp') },
  ];

  return (
    <section className="screen" id="s-logs" hidden={false}>
      <div className="phead">
        <div>
          <h1>{t('logs.pageTitle')}</h1>
          <div className="sub">{t('logs.pageDesc')}</div>
        </div>
      </div>

      {/* 隐私锁提示条（原型内联覆盖为 flow 色，非默认 mode-warn 的 warn 色） */}
      {privacyMode && (
        <div
          className="mode-warn show"
          id="log-privacy-note"
          style={{
            color: 'hsl(var(--flow-hi))',
            background: 'hsl(var(--flow-weak))',
            borderColor: 'hsl(var(--flow)/0.3)',
          }}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
            <rect x="5" y="11" width="14" height="9" rx="2" />
            <path d="M8 11V8a4 4 0 018 0v3" />
          </svg>
          <span>{t('logs.privacyNote')}</span>
        </div>
      )}

      {legacyLog?.exists && (
        <div className="mode-warn show" id="log-legacy-note">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
            <path d="M4 7h16M6 7l1 13h10l1-13M9 7V4h6v3" />
          </svg>
          <span>
            <strong>{t('logs.legacyTitle')}</strong>{' '}
            {t('logs.legacyBody', { size: fmtBytes(legacyLog.bytes) })}
          </span>
          <div className="legacy-log-actions">
            <button
              type="button"
              className="btn ghost sm"
              disabled={legacyActionBusy}
              onClick={() => void onArchiveLegacy()}
            >
              {t('logs.archiveLegacy')}
            </button>
            <button
              type="button"
              className={`btn ghost sm danger-text${confirmDeleteLegacy ? ' confirming' : ''}`}
              disabled={legacyActionBusy}
              onClick={onDeleteLegacyClick}
            >
              {confirmDeleteLegacy ? t('common.confirmAgain') : t('logs.deleteLegacy')}
            </button>
          </div>
        </div>
      )}

      <div className="card log-toolbar">
        {/* 第一行：查看范围在左，诊断/导出/目录在右。筛选与操作同排但不混作同一字段。 */}
        <div className="log-tb-primary">
          <div className="log-filter-field log-level-filter">
            <span className="log-filter-label">
              <span>{t('logs.currentLevel')}</span>
              <InfoIcon tip={`${t('logs.levelCaption')}${t('logs.levelCoreRestartHint')}`} />
            </span>
            <div className="log-filter-value">
              <Csel
                className="log-level-select"
                value={displayLevel}
                disabled={diagnosticMode === true || diagnosticBusy}
                onChange={(value) => void onLevelChange(value as LogLevel)}
                options={LEVEL_SELECT_OPTIONS}
                ariaLabel={t('logs.levelAria')}
              />
            </div>
          </div>

          <div className="log-filter-field log-source-filter">
            <span className="log-filter-label">{t('logs.sourceLabel')}</span>
            <Csel
              className="log-source-select"
              value={source}
              onChange={(value) => setSource(value as LogSource)}
              options={sourceOptions}
              ariaLabel={t('logs.sourceAria')}
            />
          </div>

          <div className="log-primary-actions">
            <button
              type="button"
              className={`btn ghost sm log-diagnostic-toggle${diagnosticMode ? ' on' : ''}`}
              disabled={diagnosticMode === null || diagnosticBusy}
              onClick={() => void onToggleDiagnostic()}
              data-tip={diagnosticMode ? t('logs.diagnosticTipOn') : t('logs.diagnosticTipOff')}
              aria-pressed={diagnosticMode === true}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M9 3h6M10 3v6l-5 8a2 2 0 001.7 3h10.6a2 2 0 001.7-3l-5-8V3" />
                <path d="M8 14h8" />
              </svg>
              <span>{t('logs.diagnosticMode')}</span>
            </button>
            {runtimeLevelNotice && (
              <span className="pill warn log-core-lvl" data-tip={coreLevelTip}>
                {runtimeLevelNotice}
              </span>
            )}
            <div className="log-export" ref={exportMenuRef}>
              <button
                type="button"
                className={`btn ghost sm${exportMenuOpen ? ' on' : ''}`}
                onClick={() => setExportMenuOpen((open) => !open)}
                aria-haspopup="menu"
                aria-expanded={exportMenuOpen}
              >
                <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                  <path d="M12 3v11M8 10l4 4 4-4M4 19h16" />
                </svg>
                <span>{t('logs.export')}</span>
                <svg className="log-export-chevron" viewBox="0 0 24 24" width="12" fill="none" stroke="currentColor" strokeWidth="2">
                  <path d="M6 9l6 6 6-6" />
                </svg>
              </button>
              {exportMenuOpen && (
                <div className="log-export-menu" role="menu">
                  <button
                    type="button"
                    className="log-export-option"
                    role="menuitem"
                    onClick={() => {
                      setExportMenuOpen(false);
                      void onExportDiag();
                    }}
                  >
                    <span className="log-export-option-title">
                      {t('logs.exportReport')}
                      <span className="pill proto">{t('logs.recommended')}</span>
                    </span>
                    <span>{t('logs.exportReportDesc')}</span>
                  </button>
                  <button
                    type="button"
                    className="log-export-option"
                    role="menuitem"
                    onClick={() => {
                      setExportMenuOpen(false);
                      void onExportLogs();
                    }}
                  >
                    <span className="log-export-option-title">{t('logs.exportLogsOnly')}</span>
                    <span>{t('logs.exportLogsOnlyDesc')}</span>
                  </button>
                </div>
              )}
            </div>
            <button
              type="button"
              className="btn ghost sm"
              onClick={onOpenLogDir}
              data-tip={t('logs.openDirTip')}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M3 7a1 1 0 011-1h5l2 3h9a1 1 0 011 1v9a1 1 0 01-1 1H4a1 1 0 01-1-1z" />
              </svg>
              <span>{t('logs.openDir')}</span>
            </button>
          </div>
        </div>

        {/* 第二行：搜索与当前结果操作。复制/清空紧邻脱敏，所见、所复制和所清理的范围保持连贯。 */}
        <div className="log-tb-main">
          <label className="input search-box log-tb-search">
            <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
              <circle cx="11" cy="11" r="7" />
              <path d="M20 20l-3-3" />
            </svg>
            <input
              id="log-search"
              type="search"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={t('logs.searchPlaceholder')}
            />
          </label>

          <div className="log-tb-actions">
            <button
              type="button"
              className={`btn ghost sm${follow ? ' on' : ''}`}
              onClick={toggleFollow}
              data-tip={follow ? t('logs.followTipOn') : t('logs.followTipOff')}
              aria-pressed={follow}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M12 5v14M6 13l6 6 6-6" />
              </svg>
              <span id="log-follow-state">{t('logs.follow')}</span>
              {pendingBadge !== null && <span className="cnt">{pendingBadge}</span>}
            </button>
            <button
              type="button"
              className={`btn ghost sm${shouldRedactLogs(privacyMode, redactLogs) ? ' on' : ''}`}
              onClick={toggleRedactLogs}
              data-tip={t('logs.redactTip')}
              aria-pressed={redactLogs}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="5" y="11" width="14" height="9" rx="2" />
                <path d="M8 11V8a4 4 0 018 0v3" />
              </svg>
              <span>{t('logs.redact')}</span>
            </button>
            <button
              type="button"
              className="btn ghost sm"
              onClick={onCopy}
              disabled={visible.length === 0}
              data-tip={t('logs.copyTip')}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect x="9" y="9" width="11" height="11" rx="2" />
                <path d="M5 15V5a2 2 0 012-2h10" />
              </svg>
              <span>{t('common.copy')}</span>
            </button>
            <button
              type="button"
              className={`btn ghost sm${confirmClear ? ' confirming' : ''}`}
              onClick={onClearClick}
              disabled={logs.length === 0 && pendingCount === 0 && visible.length === 0}
              data-tip={confirmClear ? t('logs.clearConfirm') : t('logs.clearTip')}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13" />
              </svg>
              <span>{confirmClear ? t('logs.clearConfirm') : t('logs.clear')}</span>
            </button>
          </div>
        </div>
      </div>

      {/* 日志流：原型 renderLogs() 的 .log-line 模板逐行 .map，空数组即空渲染（不发明占位态） */}
      <div
        className="log-view"
        id="log-view"
        ref={viewRef}
        tabIndex={0}
        aria-label={t('logs.liveStream')}
      >
        {renderedLogs.map((l, i) => (
          // key 用后端单调 `_id`（环形缓冲 seq）：缓冲滑动丢最旧时剩余行 key 不变。
          // 回落 `timestamp-index` 仅为缺字段的非 Tauri / 旧后端兜底（那时才有整列重 key 的老问题）。
          <div className="log-line" key={l._id ?? `${l.timestamp}-${i}`}>
            {/* 脱敏（域名/IP）：隐私锁开（#log-privacy-note 承诺 + 覆盖抬级前缓冲的旧行）**或**用户开了
                「常态脱敏」偏好（C18，redact 工具栏开关）时生效——统一走 shouldRedactLogs 判定，复用同一
                redactSensitive（不重写正则）；复制路径走同一个 renderMessage。 */}
            <span className="log-ts">[{fmtTs(l.timestamp)}]</span> <span className={`log-${l.level.toUpperCase()}`}>{l.level.toUpperCase()}:</span> {highlight(renderMessage(l.message))}
          </div>
        ))}
      </div>

      <div className="log-foot">
        {/* 底栏只承载“流状态”：直播状态、当前行数与暂停后的恢复入口作为一个左侧簇，
            避开应用统一占用的右下 toast 区域。 */}
        <span className="log-stream-state">
          <span className="log-live">
            <span className="log-live-dot" />
            <span>{t('logs.liveStream')}</span>
          </span>
          <span className="log-count">
            <b id="log-count">{visible.length}</b>{' '}
            <span>{t('logs.linesUnit')}</span>
          </span>
          {/* 回到底部：follow 暂停时才出现（手动按暂停 或 上滚脱离底部）。点它＝回填 pending + 吸回底部
              + 恢复跟随，是「恢复直播」的唯一显式入口。 */}
          {!follow && (
            <button
              type="button"
              className="btn ghost sm"
              onClick={resumeFollow}
              /* 这颗只在暂停态出现，点它就是恢复 ⇒ 恒用 off 文案（「已暂停 · 点击恢复」）。 */
              data-tip={t('logs.followTipOff')}
            >
              <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth="1.8">
                <path d="M12 5v14M6 13l6 6 6-6" />
              </svg>
              {/* logs 域自己的键：借 `home.scrollToBottom` 会让首页改文案时把日志页一起改掉
                  （两处的按钮语义相近但不同——首页那个只滚动，这里还要回填 pending + 恢复跟随）。 */}
              <span>{t('logs.scrollToBottom')}</span>
            </button>
          )}
        </span>
        <ListPager
          {...logPagination}
          total={visible.length}
          onPageChange={onLogPageChange}
        />
      </div>
    </section>
  );
}

export default LogsScreen;
