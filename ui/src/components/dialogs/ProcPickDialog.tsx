/**
 * ProcPickDialog —— 进程选择器（嵌套弹窗，原型 #proc-pick-dialog :2659-2676；procPick* :4694-4716）。
 *
 * 原型是**多选批量加入**（非单击即选）：每行 `<label class="proc-pick-row">` 左侧 `.nd-check` 复选框，
 * 点击整行切换选中；footer「添加」按钮按选中数计文案（disabled=0 选中），一次性把全部选中项交给
 * 调用方（`open({kind:'proc-pick', onPick})`），由其去重追加进自身值。
 *
 * **当前唯一调用方 = 自定义应用弹窗**（`AppAddDialog.tsx:728`）。规则弹窗那个入口已于 2026-07-30 删除：
 * 规则弹窗重设计后进程条件自带行内勾选区，再挂一个「从进程选择」按钮是两个入口做同一件事（补丁式）。
 * 本组件**不随之删除** —— 它仍是自定义应用侧的进程来源。
 *
 * 「隐藏系统进程」（`.pp-sys` 开关，原型默认开）用与原型相同的路径前缀启发式 `PROC_SYS_RE`（上游
 * system-path heuristic，原型 :4674 标注非新发明）—— `SystemProcessInfo` 契约本身不带 system 位，
 * 纯前端对 `path` 做正则判定，不臆造后端字段。
 *
 * 从规则表单的进程条件「从进程选择」按钮 / 应用表单「从进程选择」按钮 push 打开。
 * ESC 优先关本弹窗再关上一层弹窗：由原生 top-layer + Modal 的 cancel 链自动完成（不在此重复实现）。
 *
 * 数据源 `api.system.listProcesses` 是**真实枚举**（`commands/system.rs`：按平台读 `/proc` 或跑 `ps`/`tasklist`）。
 * 拉取失败 / 无进程时优雅空态（「暂无进程数据 / 后端未接入」），不崩、不阻断规则/应用表单。
 * 搜索框按名/路径过滤（原型 procPickRender :4700）。
 */

import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import type { SystemProcessInfo } from '@/contracts/types';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

// 上游 system-path heuristic（原型 PROC_SYS_RE :4674，逐字对齐——纯前端启发式，非后端字段）。
const PROC_SYS_RE = /^(\/usr\/|\/System\/|\/sbin\/|\/bin\/|[A-Za-z]:\\Windows\\)/i;

function ProcIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <rect x="4" y="4" width="16" height="16" rx="2" />
      <path d="M9 9h6v6" />
    </svg>
  );
}
// 原型 AAD_CK（:4649，与 .cat-ck/.nd-check 共用同一勾选图形）。
function CheckIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={3}>
      <path d="M5 12l5 5 9-11" />
    </svg>
  );
}

type LoadState =
  | { phase: 'loading' }
  | { phase: 'ready'; procs: SystemProcessInfo[] }
  | { phase: 'error'; message: string };

export function ProcPickDialog({ onPick }: { onPick: (procs: SystemProcessInfo[]) => void }) {
  const { t } = useTranslation();
  const close = useDialogStore((s) => s.close);

  const [state, setState] = useState<LoadState>({ phase: 'loading' });
  const [query, setQuery] = useState('');
  const [hideSys, setHideSys] = useState(true); // 原型 procPick.hideSys 默认 true（:4694）
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [reloadTick, setReloadTick] = useState(0);

  useEffect(() => {
    let alive = true;
    setState({ phase: 'loading' });
    api.system
      .listProcesses()
      .then((procs) => {
        if (alive) setState({ phase: 'ready', procs: Array.isArray(procs) ? procs : [] });
      })
      .catch((e) => {
        if (alive) setState({ phase: 'error', message: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      alive = false;
    };
  }, [reloadTick]);

  const filtered = useMemo(() => {
    if (state.phase !== 'ready') return [];
    const q = query.trim().toLowerCase();
    return state.procs.filter((p) => {
      if (hideSys && PROC_SYS_RE.test(p.path ?? '')) return false;
      if (!q) return true;
      return p.name.toLowerCase().includes(q) || (p.path ?? '').toLowerCase().includes(q);
    });
  }, [state, query, hideSys]);

  const toggle = (name: string) => {
    setSel((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  const handleAdd = () => {
    if (state.phase !== 'ready' || sel.size === 0) return;
    const chosen = state.procs.filter((p) => sel.has(p.name));
    onPick(chosen);
    close(); // pop 本弹窗（回到规则/应用弹窗）
  };

  return (
    <Modal
      titleId="pp-dlg-title"
      title={t('procPick.title')}
      onClose={close}
      icon={<ProcIcon />}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn flow" onClick={handleAdd} disabled={sel.size === 0}>
            <span>
              {sel.size > 0
                ? t('procPick.addN', { n: sel.size })
                : t('procPick.add')}
            </span>
          </button>
        </>
      }
    >
      <div className="card-sub" style={{ marginTop: -4 }}>
        {t('procPick.hint')}
      </div>

      <div className="pp-tools">
        <label className="input" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 11px' }}>
          <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8} style={{ color: 'hsl(var(--fg-faint))', flex: 'none' }}>
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3-3" />
          </svg>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('procPick.searchPh')}
            aria-label={t('procPick.searchPh')}
            style={{ border: 0, background: 'none', outline: 'none', flex: 1, padding: '8px 0', font: 'inherit', color: 'inherit' }}
          />
        </label>
        <button
          type="button"
          className="btn ghost sm pp-refresh"
          aria-label={t('procPick.refresh')}
          data-tip={t('procPick.refresh')}
          onClick={() => setReloadTick((n) => n + 1)}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M4 4v6h6M20 20v-6h-6M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
          </svg>
        </button>
      </div>

      <div className="pp-sys">
        <span>{t('procPick.hideSys')}</span>
        <span
          className={`swt${hideSys ? ' on' : ''}`}
          role="switch"
          aria-checked={hideSys}
          tabIndex={0}
          onClick={() => setHideSys((v) => !v)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              setHideSys((v) => !v);
            }
          }}
        />
      </div>

      <div className="proc-pick-list">
        {state.phase === 'loading' && (
          <div className="proc-pick-empty">{t('procPick.loading')}</div>
        )}
        {state.phase === 'error' && (
          <div className="proc-pick-empty">
            {t('procPick.error')}
          </div>
        )}
        {state.phase === 'ready' && filtered.length === 0 && (
          <div className="proc-pick-empty">
            {state.procs.length === 0
              ? t('procPick.empty')
              : t('procPick.noMatch')}
          </div>
        )}
        {state.phase === 'ready' &&
          filtered.map((p) => {
            const checked = sel.has(p.name);
            return (
              <label
                key={p.name}
                className={`proc-pick-row${checked ? ' on' : ''}`}
                tabIndex={0}
                onClick={() => toggle(p.name)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    toggle(p.name);
                  }
                }}
              >
                <span className={`nd-check${checked ? ' checked' : ''}`} role="checkbox" aria-checked={checked}>
                  <CheckIcon />
                </span>
                <span className="pp-main">
                  <span className="pp-nm">{p.name}</span>
                  {p.path && (
                    <span className="pp-path" data-tip={p.path}>
                      {p.path}
                    </span>
                  )}
                </span>
                {p.count > 1 && <span className="pp-cnt mono">×{p.count}</span>}
              </label>
            );
          })}
      </div>
    </Modal>
  );
}

export default ProcPickDialog;
