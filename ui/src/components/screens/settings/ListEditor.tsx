/**
 * ListEditor —— 字符串列表编辑器（原型 .cidr-list.list-ed + .le-foot，如 L2129-2135/2211-2216）。
 *
 * 原型中 bypass / cidr / inbound-cidr / fakeip-filter / dns-custom 五处共用同款 UI：
 * 每行一个 .input.mono + .cidr-del 删除按钮，底部「添加 / 批量导入」两个 .btn.ghost.sm。
 * `.cidr-list` 与 `.le-foot` 在原型里是同级兄弟（不互相嵌套）——用 Fragment 而非包一层 div。
 *
 * 这里抽出复用；placeholder 由调用方提供（CIDR / 域名 / DoH URL 等）。
 *
 * 「批量导入」此前是 `onChange([...value,'','',''])` —— 只追加 3 个空行，没有任何粘贴/解析，
 * 是个名不副实的控件（用户按名字理解成"粘贴一批进来"，得到的是三个空框）。现改为真导入：
 * 展开一个 textarea，粘贴的多行/逗号分隔文本按 `/[,\n]/` 拆分、trim、去空、**大小写不敏感去重**
 * （与既有条目及批内互相去重，同 AppAddDialog.handleProcPick 的既定口径），再一次性追加。
 */

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { revealElement } from '@/components/reveal';
import { TextInput, Button } from './primitives';

export interface ListEditorProps {
  value: string[];
  onChange: (next: string[]) => void;
  placeholder?: string;
  ariaLabel?: string;
  /** 添加按钮文案 */
  addLabel?: string;
  /** 批量导入按钮文案 */
  importLabel?: string;
  /** 上限（如 dns-custom 最多 1 个）；超过禁用添加按钮 */
  max?: number;
  /** 是否使用等宽字体（默认 true，对齐 .input.mono） */
  mono?: boolean;
  /** 额外挂在 .cidr-list 根上的 class（如 race-custom） */
  className?: string;
  id?: string;
}

/**
 * 批量导入的解析/合并（**纯函数，单测在 ListEditor.bulk-import.test.ts**）：
 * 拆分 `/[,\n]/` → trim → 去空 → 与既有条目及批内互相去重（大小写不敏感）→ 尊重 max 上限。
 *
 * 抽成自由函数不是为了复用（只有一个调用点），是为了**可测**：留在组件闭包里就只能靠渲染断言，
 * 而本仓 vitest 跑在 node 环境（无 jsdom），那等于这段边界最多的逻辑一条断言都没有。
 *
 * @param draft 用户粘贴的原文
 * @param existing 当前列表（原样保留，含用户点「添加」留下的空行）
 * @param max 上限；达到即停止追加（不报错，与「添加」按钮的 atCap 语义一致）
 * @returns 合并后的**完整**新列表
 */
export function parseBulkEntries(draft: string, existing: string[], max?: number): string[] {
  const next = [...existing];
  // 去重基准里剔掉空串：既有的空行不该让粘贴内容里的第一个条目被误判成重复。
  const seen = new Set(next.map((s) => s.trim().toLowerCase()).filter(Boolean));
  for (const raw of draft.split(/[,\n]/)) {
    const entry = raw.trim();
    if (!entry) continue;
    const key = entry.toLowerCase();
    if (seen.has(key)) continue;
    if (max !== undefined && next.length >= max) break;
    seen.add(key);
    next.push(entry);
  }
  return next;
}

/** 两个列表逐项相同（长度 + 顺序 + 每一项）。 */
export function sameEntries(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((s, i) => s === b[i]);
}

/**
 * 外部改动到达时草稿该取什么值 —— draft+onBlur 这套里**最容易写错的一格**，故抽成纯函数单测。
 *
 * 判据同 `SettingsDns` 的 `seededRef` 守卫：
 *  - 草稿 ≠ 上次种子 ⇒ 用户正在编辑，**保留草稿**（外部刷新绝不能把人正在敲的字符抹掉）；
 *  - 草稿 == 上次种子 ⇒ 用户没动过，**跟随新配置**（托盘 / 备份恢复 / 另一屏保存要能回填）。
 *
 * 第三条是本仓 props 身份易变带来的：内容已相同就返回**原引用**，避免 `value` 每次父级重渲
 * 都换新数组时白白多一次 `setState` 重渲。
 */
export function nextDraft(
  cur: readonly string[],
  seed: readonly string[],
  incoming: readonly string[]
): readonly string[] {
  if (!sameEntries(cur, seed)) return cur; // 用户改过 → 不打断
  if (sameEntries(cur, incoming)) return cur; // 内容已一致 → 别换引用
  return incoming;
}

export function ListEditor({
  value,
  onChange,
  placeholder,
  ariaLabel = 'Entry',
  addLabel = 'Add',
  importLabel = 'Bulk import',
  max,
  mono = true,
  className,
  id,
}: ListEditorProps) {
  const { t } = useTranslation();
  const [importOpen, setImportOpen] = useState(false);
  // 内联导入面板不是 <details>，拿不到 toggle 事件，故用挂载后的 effect 触发同一条「展开即露出」
  // （判据与理由见 @/components/reveal）。它长在设置页主滚动区里，靠底部展开时整块落在视口外。
  const importPanelRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (importOpen && importPanelRef.current) revealElement(importPanelRef.current);
  }, [importOpen]);
  const [importDraft, setImportDraft] = useState('');

  /* ── 本地草稿 + onBlur 提交（**照 `SettingsDns.tsx:235-265` 那套既有做法，不另发明**）──
   *
   * 此前每个字符都直接调父级 `onChange` ⇒ 经 `useConfig().update` 漏斗 → `editRoute` → `stage()`，
   * 于是**每敲一个字符**跑一遍 sanitize+validate、一次落盘（原子写 tmp+rename）、一次
   * `broadcast_config_changed` → 一次 `spawn(switch_mode)`。DNS 那三个文本框当初正是这个毛病、
   * 已改成 draft+onBlur（见那里的头注），只有本组件没跟上 —— 它是 7 个挂点的共同源头
   * （bypassLANList ×2 / tunConfig ×3 / dnsConfig / fakeIpFilterList，全在 29 个核心键内）。
   *
   * 分工：**打字只动草稿**；blur / Enter 提交。删除 / 添加 / 批量导入是**离散动作**，照旧立即提交，
   * 但一律基于**草稿**而非 `value` —— 基于后者会把另一行里还没提交的编辑一起丢掉。 */
  const [draft, setDraft] = useState<readonly string[]>(value);
  const seededRef = useRef<readonly string[]>(value);
  useEffect(() => {
    setDraft((cur) => nextDraft(cur, seededRef.current, value));
    seededRef.current = value;
  }, [value]);

  const atCap = max !== undefined && draft.length >= max;

  /** 提交到父级。与 `value` 逐项相同就**不写** —— 同 `commitDns` 的「无变化不写」，免一次无谓落盘。 */
  function commit(next: readonly string[]) {
    setDraft(next);
    if (!sameEntries(next, value)) onChange([...next]);
  }

  /** 打字：**只动草稿**，绝不碰父级（这一行就是本次修复的全部要点）。 */
  function editRow(idx: number, next: string) {
    setDraft((cur) => cur.map((s, i) => (i === idx ? next : s)));
  }
  function remove(idx: number) {
    commit(draft.filter((_, i) => i !== idx));
  }
  function add() {
    if (atCap) return;
    commit([...draft, '']);
  }

  function applyImport() {
    commit(parseBulkEntries(importDraft, [...draft], max));
    setImportDraft('');
    setImportOpen(false);
  }

  return (
    <>
      <div id={id} className={className ? `cidr-list list-ed ${className}` : 'cidr-list list-ed'}>
        {draft.map((entry, idx) => (
          <div key={idx} className="cidr-row">
            {/* onChange 只动草稿；onBlur 提交，Enter 触发 blur 即提交（同 SettingsDns 的三个文本框）。 */}
            <TextInput
              value={entry}
              onChange={(e) => editRow(idx, e.target.value)}
              onBlur={() => commit(draft)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') e.currentTarget.blur();
              }}
              aria-label={ariaLabel}
              placeholder={placeholder}
              className={mono ? 'mono' : undefined}
            />
            <button type="button" onClick={() => remove(idx)} aria-label="Remove" className="cidr-del">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M5 5l14 14M19 5L5 19" />
              </svg>
            </button>
          </div>
        ))}
      </div>
      <div className="le-foot">
        <Button variant="ghost" size="sm" onClick={add} disabled={atCap}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M12 5v14M5 12h14" />
          </svg>
          <span>{addLabel}</span>
        </Button>
        <Button
          variant="ghost"
          size="sm"
          onClick={() => setImportOpen((v) => !v)}
          disabled={atCap}
          aria-expanded={importOpen}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M4 6h13M4 12h8M4 18h8M17 14v7M14 17l3 3 3-3" />
          </svg>
          <span>{importLabel}</span>
        </Button>
      </div>
      {/* 面板类名用原型既有的 .le-imp / .le-imp-acts / .le-imp-hint（prototype.css §S-#2 就是为
          「inline import panel」写的，此前无人渲染 = 死样式），不新造类。textarea 复用 .input.mono
          （同 RuleDialog 的条件值输入）。 */}
      {importOpen && (
        <div className="le-imp" ref={importPanelRef}>
          <textarea
            className={mono ? 'input mono' : 'input'}
            rows={4}
            value={importDraft}
            onChange={(e) => setImportDraft(e.target.value)}
            placeholder={placeholder}
            aria-label={importLabel}
          />
          <div className="le-imp-acts">
            <Button variant="flow" size="sm" onClick={applyImport} disabled={!importDraft.trim()}>
              <span>{t('common.confirm', '确认')}</span>
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setImportDraft('');
                setImportOpen(false);
              }}
            >
              <span>{t('common.cancel', '取消')}</span>
            </Button>
            <span className="le-imp-hint">
              {t('settings.listImportHint', '每行或逗号分隔一个条目，重复项自动跳过')}
            </span>
          </div>
        </div>
      )}
    </>
  );
}
