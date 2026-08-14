/**
 * SubDialog —— 添加/编辑订阅弹窗（原型 #sub-dialog :2550；subEdit 编辑态 :3745）。
 *
 * 复杂度「中」但**单一用途**：名称/URL/UA + 高级折叠（仅「自动更新」开关 + 诚实断流警告）。
 * 按 §1.4 判据「单协议单字段的简单弹窗直接写 JSX，不强行套 FieldSpec」——故本组件手写 JSX，
 * FieldSpec/FieldRenderer 是给节点/组网那种「多态字段表」用的，订阅无多态，套上去是反例。
 *
 * props 由 stub 冻结：`SubDialog({ subId })`（undefined=新增，defined=按 id 从 app-store config 预填）。
 * 提交走真后端：新增 `api.subscription.add` / 编辑 `api.subscription.update`（均 REAL）。
 * 可选预检：`api.subscription.preview`（REAL，本批新接线）—— add 前先拉取解析、不写 config，返节点数或分类错误。
 *
 * R1 同步初始化 + `key={subId ?? 'new'}` 重挂（见导出包装）；脏态取消 → 嵌套 confirm（复用 D1 ConfirmDialog）。
 *
 * 高级区：「自动更新」开关 + 「经代理更新」per-sub 开关（对齐 上游 订阅弹窗）：
 *  - 更新间隔为**全局** `config.subscriptionUpdateIntervalHours`，无 per-sub 后端字段 → 原 seg2 是纯展示态
 *    死控件（intervalH 从不进 handleSubmit），会误导用户以为可 per-sub 设定，已移除（反伪造/不留死控件；
 *    上游 订阅弹窗同样无 per-sub 间隔字段）。
 *  - 「经代理更新」是**per-sub 覆盖**开关：每个订阅各自持有 `SubscriptionConfig.updateViaProxy`。全局
 *    `subscriptionProxyPolicy`（follow/proxy/direct）是**默认策略**而非替代——`follow` 时按各订阅本开关决定，
 *    `proxy`/`direct` 时强制覆盖全部订阅、本开关置灰并提示（与 上游 + 后端 handler 同一真值口径
 *    `resolveSubscriptionViaProxy`）。提交把 `updateViaProxy` 写回 config；预检 viaProxy 用「策略 × 本开关实时值」求值。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { api } from '@/ipc';
import type { SubscriptionConfig } from '@/contracts/types';
import { resolveSubscriptionViaProxy } from '@/domain/subscription-proxy';
import { SUBSCRIPTION_ERROR_I18N_KEY } from '@/contracts/subscription-preview';
import { refreshSubscriptionWithToast } from '@/domain/subscription-refresh';
import { toast } from '@/lib/error-handler';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';
import { Fold } from '@/components/Fold';
import { InfoIcon } from '@/components/InfoIcon';

function SubIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M4 11a9 9 0 019 9M4 4a16 16 0 0116 16" />
      <circle cx="5" cy="19" r="1.5" />
    </svg>
  );
}

interface SubFormProps {
  base?: SubscriptionConfig;
  onAdded?: (subId: string) => void;
  /**
   * 打开时自动聚焦的字段。订阅「更多」菜单的「重命名」/「编辑 URL」两项都开本弹窗（Polaris 只有
   * 一个订阅表单），靠这个落点区分——否则两个菜单项点下去完全一样，等于摆了个同义按钮。
   */
  focus?: 'name' | 'url';
}

function SubForm({ base, focus, onAdded }: SubFormProps) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const loadConfig = useAppStore((s) => s.loadConfig);
  // 全局订阅代理策略（默认 follow）：非 follow 时覆盖 per-sub 本开关，此处据它决定预检求值与开关置灰。
  const proxyPolicy = useEffectiveConfig((c) => c?.subscriptionProxyPolicy) ?? 'follow';
  // 全局策略 proxy/direct 时强制覆盖 per-sub → 本弹窗「经代理更新」开关置灰、只显示被覆盖后的实际值。
  const proxyOverridden = proxyPolicy !== 'follow';

  const isEdit = base != null;

  // R1：同步初始化（挂载即带正确值，绝不挂载后 reset）。
  const [name, setName] = useState(base?.name ?? '');
  const [url, setUrl] = useState(base?.url ?? '');
  const [ua, setUa] = useState(base?.userAgent ?? '');
  const [autoUpdate, setAutoUpdate] = useState(base?.autoUpdate ?? false);
  // per-sub「经代理更新」覆盖（默认关）：写回 SubscriptionConfig.updateViaProxy。
  const [viaProxy, setViaProxy] = useState(base?.updateViaProxy ?? false);

  // 预检经代理与否：全局策略 × 本开关实时值求值（proxy=强制经代理 / direct=强制直连 / follow=用本开关值）。
  const previewViaProxy = resolveSubscriptionViaProxy(proxyPolicy, viaProxy);

  const [dirty, setDirty] = useState(false);
  const [errName, setErrName] = useState(false);
  const [errUrl, setErrUrl] = useState(false);
  const [previewMsg, setPreviewMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [previewing, setPreviewing] = useState(false);

  const touch = () => {
    setDirty(true);
  };

  const validUrl = (u: string): boolean => {
    const s = u.trim();
    if (!s) return false;
    try {
      const p = new URL(s);
      return p.protocol === 'http:' || p.protocol === 'https:';
    } catch {
      return false;
    }
  };

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('sub.discardTitle'),
        message: t('sub.discardMsg'),
        confirmLabel: t('sub.discard'),
        danger: true,
        onConfirm: () => {
          close(); // pop confirm
          close(); // pop 本弹窗
        },
      },
    });
  };

  const runPreview = async () => {
    if (!validUrl(url)) {
      setErrUrl(true);
      return;
    }
    setPreviewing(true);
    setPreviewMsg(null);
    try {
      const r = await api.subscription.preview(url.trim(), {
        viaProxy: previewViaProxy,
        userAgent: ua.trim() || undefined,
      });
      if (r.ok) {
        setPreviewMsg({
          ok: true,
          text: t('sub.previewOk', {
            n: r.nodeCount ?? 0,
          }),
        });
      } else {
        const key = r.errorKind
          ? SUBSCRIPTION_ERROR_I18N_KEY[r.errorKind]
          : undefined;
        setPreviewMsg({
          ok: false,
          text: key
            ? t(key.detail)
            : t('sub.previewFail'),
        });
      }
    } catch (e) {
      setPreviewMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setPreviewing(false);
    }
  };

  const handleSubmit = async () => {
    const nameEmpty = !name.trim();
    const urlBad = !validUrl(url);
    setErrName(nameEmpty);
    setErrUrl(urlBad);
    if (nameEmpty || urlBad) return;

    setSubmitting(true);
    try {
      if (isEdit && base) {
        const next: SubscriptionConfig = {
          ...base,
          name: name.trim(),
          url: url.trim(),
          autoUpdate,
          userAgent: ua.trim() || undefined,
          updateViaProxy: viaProxy,
        };
        // 编辑：仅写 config（对齐 上游 updateSubscription——edit 不自动拉取）+ 成功 toast。
        await api.subscription.update(next);
        toast.success(t('sub.updated'));
        void loadConfig(true);
        close();
      } else {
        // 预检先行（对齐 上游 saveSubscription）：坏/不可达 URL 不建记录、留窗报分类错误，杜绝
        // 「add 只写 config 不拉取 → 静默变成空订阅」。预检通过才真正 add。
        const pre = await api.subscription.preview(url.trim(), {
          viaProxy: previewViaProxy,
          userAgent: ua.trim() || undefined,
        });
        if (!pre.ok) {
          const key = pre.errorKind ? SUBSCRIPTION_ERROR_I18N_KEY[pre.errorKind] : undefined;
          // 错误表本就是 title/detail 两段（contracts/subscription-preview.ts:45），此前挤在一条
          // `.dlg-err` 里只显 detail、title 沦为兜底默认值；toast 的两段结构正好把它摊开。
          if (key) toast.error(t(key.title), t(key.detail));
          else toast.error(t('sub.previewFail'));
          return; // 留窗、保留用户输入；不建任何记录（无先加后删闪现）
        }
        const newSub = await api.subscription.add({
          name: name.trim(),
          url: url.trim(),
          autoUpdate,
          userAgent: ua.trim() || undefined,
          updateViaProxy: viaProxy,
        });
        toast.success(t('sub.added'));
        // add 命令只写 config、不拉取节点 → 主动拉一次并三态 toast（对齐 上游「add 后 updateServers」），
        // 让用户明确知道解析到几个节点，而非误以为「add 成功=订阅可用」。
        await refreshSubscriptionWithToast(newSub.id, t);
        // 先刷新 store、再通知节点页切 Tab；反过来 activeTab 的失效组守卫会把尚不存在的新 id
        // 立即回落到第一组，形成“添加成功却看不到新订阅”的假象。
        await loadConfig(true);
        onAdded?.(newSub.id);
        close();
      }
    } catch (e) {
      toast.error(t('common.saveFailed'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      titleId="sub-title"
      title={isEdit ? t('sub.editTitle') : t('sub.addTitle')}
      onClose={requestClose}
      icon={<SubIcon />}
      className="entry-form-dlg"
      footer={
        <>
          <button type="button" className="btn ghost sm" onClick={() => void runPreview()} disabled={previewing || submitting} style={{ marginRight: 'auto' }}>
            {previewing ? <span className="spinner spin-inline" /> : null}
            {/* 键名故意避开 `sub.preview.*`（locale 里是预检失败错误文案的嵌套命名空间，
                同名会被 i18next 判定为对象而非字符串，控制台报 "returned an object instead of string"，
                按钮显示原始警告文本而非「预检」——实测发现，见 SubDialog 交付说明）。 */}
            <span>{t('sub.previewBtn')}</span>
          </button>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn flow" onClick={() => void handleSubmit()} disabled={submitting}>
            {isEdit ? t('common.save') : t('sub.add')}
          </button>
        </>
      }
    >
      <div className="fld">
        <label className="fld-l" htmlFor="sub-name">
          <span>{t('sub.name')}</span> <span className="req-star">*</span>
        </label>
        <input
          id="sub-name"
          className="input"
          {...(focus === 'name' ? { 'data-autofocus': '' } : {})}
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setErrName(false);
            touch();
          }}
          placeholder={t('sub.namePh')}
        />
        {errName && <div className="err-line">{t('sub.errName')}</div>}
      </div>

      <div className="fld">
        <label className="fld-l" htmlFor="sub-url">
          <span>{t('sub.url')}</span> <span className="req-star">*</span>
        </label>
        <input
          id="sub-url"
          className="input mono"
          {...(focus === 'url' ? { 'data-autofocus': '' } : {})}
          value={url}
          onChange={(e) => {
            setUrl(e.target.value);
            setErrUrl(false);
            setPreviewMsg(null);
            touch();
          }}
          placeholder="https://example.com/sub?token=…"
        />
        {errUrl && <div className="err-line">{t('sub.errUrl')}</div>}
      </div>

      <div className="fld">
        <label className="fld-l fld-l-info" htmlFor="sub-ua">
          <span>{t('sub.ua')}</span>
          <span className="fld-opt"> {t('common.optional')}</span>
          <InfoIcon tip={t('sub.uaHint')} />
        </label>
        <input
          id="sub-ua"
          className="input mono"
          value={ua}
          onChange={(e) => {
            setUa(e.target.value);
            touch();
          }}
          placeholder={t('sub.uaPh')}
        />
      </div>

      <Fold title={t('common.advanced')}>
        <div className="fld swt-row">
          <div className="swt-tx">
            <span className="swt-label">
              <b>{t('sub.autoUpdate')}</b>
              <InfoIcon tip={t('sub.autoUpdateHint')} />
            </span>
          </div>
          <button
            type="button"
            role="switch"
            aria-checked={autoUpdate}
            aria-label={t('sub.autoUpdate')}
            className={`swt${autoUpdate ? ' on' : ''}`}
            onClick={() => {
              setAutoUpdate((v) => !v);
              touch();
            }}
          />
        </div>

        {autoUpdate && (
          <div className="warn-line">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 9v4M12 17h.01M10.3 3.9L2 18a2 2 0 001.7 3h16.6a2 2 0 001.7-3L13.7 3.9a2 2 0 00-3.4 0z" />
            </svg>
            <span>{t('sub.autoUpdateWarn')}</span>
          </div>
        )}

        {/* per-sub「经代理更新」：全局策略 follow 时可设；proxy/direct 被全局覆盖 → 置灰并提示（对齐 上游）。 */}
        <div className="fld swt-row" style={{ marginTop: 14 }}>
          <div className="swt-tx">
            <span className="swt-label">
              <b>{t('sub.viaProxy')}</b>
              <InfoIcon
                tip={proxyPolicy === 'proxy'
                  ? t('sub.viaProxyOverrideProxy')
                  : proxyPolicy === 'direct'
                    ? t('sub.viaProxyOverrideDirect')
                    : t('sub.viaProxyHint')}
              />
            </span>
          </div>
          <button
            type="button"
            role="switch"
            aria-checked={viaProxy}
            aria-label={t('sub.viaProxy')}
            className={`swt${viaProxy ? ' on' : ''}`}
            disabled={proxyOverridden}
            onClick={() => {
              setViaProxy((v) => !v);
              touch();
            }}
          />
        </div>
      </Fold>

      {previewMsg && (
        <div className={previewMsg.ok ? 'mesh-success' : 'dlg-err'}>
          {previewMsg.ok ? (
            <>
              <svg viewBox="0 0 24 24" width={18} fill="none" stroke="currentColor" strokeWidth={2.6} style={{ color: 'hsl(var(--ok))' }}>
                <path d="M20 6L9 17l-5-5" />
              </svg>
              <div><b>{previewMsg.text}</b></div>
            </>
          ) : (
            previewMsg.text
          )}
        </div>
      )}
    </Modal>
  );
}

export function SubDialog({
  subId,
  focus,
  onAdded,
}: {
  subId?: string;
  focus?: 'name' | 'url';
  onAdded?: (subId: string) => void;
}) {
  const config = useEffectiveConfig();
  const base = subId ? config?.subscriptions?.find((s) => s.id === subId) : undefined;
  // R1：key 绑 subId + focus —— 切换编辑目标或聚焦落点 = 重挂 = 同步重新初始化（autoFocus 仅挂载生效，
  // 不进 key 的话「重命名」开着时再点「编辑 URL」不会重新聚焦）。
  return (
    <SubForm key={`${subId ?? 'new'}:${focus ?? ''}`} base={base} focus={focus} onAdded={onAdded} />
  );
}

export default SubDialog;
