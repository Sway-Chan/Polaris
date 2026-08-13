/**
 * ResUrlDialog —— URL 下载规则资源弹窗（§表8 #8，原型 #resource-dialog :2679）。
 *
 * D1 的**端到端最简载体**：证明 Modal 原语 + Csel + dialog-store 全链路通。
 *  - 字段：资源 URL（必填，http/https）+ 名称（从 URL 自动推断，可覆盖）+ 分类（Csel，可覆盖推断）；
 *  - 提交走 `api.ruleResources.download([{url,name,category}])`；
 *  - **后端现状**：`rule_resources_download` 是**真下载**（`commands/rules.rs`：SSRF-safe 拉取 + SRS/JSON 校验
 *    + 落盘 + upsert config）。返回结果与入参逐项同序，各项独立容错（`ok`/`error`），前端据此逐项报错。
 *  - 脏态取消 → 嵌套 confirm（放弃更改）：D1 的嵌套两层叠放验证路径。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { api } from '@/ipc';
import type { RuleResourceCategory } from '@/contracts/types';
import { Modal } from './Modal';
import { Csel, type CselOption } from './Csel';
import { useDialogStore } from './dialog-store';
import {
  inferResource,
  validateResName,
  validateResUrl,
  type ResUrlError,
} from './res-url-logic';

export function ResUrlDialog() {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);

  const [url, setUrl] = useState('');
  const [name, setName] = useState('');
  const [nameDirty, setNameDirty] = useState(false);
  const [category, setCategory] = useState<RuleResourceCategory>('custom');
  const [catDirty, setCatDirty] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const catOptions: CselOption[] = [
    { value: 'geosite', label: t('resources.urlDlgCatGeosite', 'Geosite（域名规则集）') },
    { value: 'geoip', label: t('resources.urlDlgCatGeoip', 'GeoIP（IP 规则集）') },
    { value: 'geosite-lite', label: 'Geosite Lite' },
    { value: 'geoip-lite', label: 'GeoIP Lite' },
    { value: 'custom', label: t('resources.urlDlgCatCustom', '自定义') },
  ];

  const onUrlChange = (v: string) => {
    setUrl(v);
    const inf = inferResource(v);
    if (!nameDirty) setName(inf.name);
    if (!catDirty) setCategory(inf.category);
  };

  const errText = (code: ResUrlError | string): string => {
    switch (code) {
      case 'urlEmpty':
        return t('resources.urlDlgErrUrlEmpty', '请输入资源 URL');
      case 'urlInvalid':
        return t('resources.urlDlgErrUrlInvalid', 'URL 格式不合法（需 http/https）');
      case 'nameEmpty':
        return t('resources.urlDlgErrNameEmpty', '请输入名称');
      case 'downloadUnavailable':
        return t('resources.urlDlgErrUnavailable', '下载后端尚未接入（需 HTTP 下载能力）');
      case 'downloadFailed':
        return t('resources.urlDlgErrFailed', '下载失败');
      default:
        return code;
    }
  };

  const requestClose = () => {
    if (url.trim() || name.trim()) {
      // 脏态 → 嵌套确认（放弃更改）。onConfirm 关两层：confirm + 本弹窗。
      open({
        kind: 'confirm',
        payload: {
          title: t('resources.urlDlgDiscardTitle', '放弃下载？'),
          message: t('resources.urlDlgDiscardMsg', '已填写的内容将不会保存。'),
          confirmLabel: t('resources.urlDlgDiscard', '放弃'),
          danger: true,
          onConfirm: () => {
            close(); // pop confirm
            close(); // pop 本弹窗
          },
        },
      });
    } else {
      close();
    }
  };

  const handleSubmit = async () => {
    const ue = validateResUrl(url);
    if (ue) {
      toast.error(errText(ue));
      return;
    }
    const ne = validateResName(name);
    if (ne) {
      toast.error(errText(ne));
      return;
    }
    setSubmitting(true);
    try {
      const results = await api.ruleResources.download([
        { url: url.trim(), name: name.trim(), category },
      ]);
      const r = results[0];
      if (!r) {
        // 防御：下载结果与入参逐项同序，单项提交理应恒有结果；空数组仅可能于后端异常，兜为不可用态。
        toast.error(errText('downloadUnavailable'));
        return;
      }
      if (!r.ok) {
        toast.error(errText('downloadFailed'), r.error ?? r.errorCode ?? undefined);
        return;
      }
      close();
    } catch (e) {
      toast.error(errText('downloadFailed'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      titleId="res-dlg-title"
      title={t('resources.urlDlgTitle', '下载规则资源')}
      onClose={requestClose}
      icon={
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M9 15l6-6M8 8a3 3 0 10-3 3" />
        </svg>
      }
      footer={
        <>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button type="button" className="btn flow" onClick={() => void handleSubmit()} disabled={submitting}>
            {t('resources.download', '下载')}
          </button>
        </>
      }
    >
      <div className="fld">
        <label className="fld-l" htmlFor="res-url">
          {t('resources.urlDlgUrlLabel', '资源 URL（.srs / .txt / .json）')}
        </label>
        <input
          id="res-url"
          className="input mono"
          value={url}
          onChange={(e) => onUrlChange(e.target.value)}
          placeholder="https://example/geosite-cn.srs"
        />
      </div>

      <div className="fld">
        <label className="fld-l" htmlFor="res-name">
          {t('resources.urlDlgNameLabel', '名称（自动推断）')}
        </label>
        <input
          id="res-name"
          className="input"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setNameDirty(true);
          }}
          placeholder="geosite-cn"
        />
      </div>

      <div className="fld">
        <label className="fld-l" htmlFor="res-cat">
          {t('resources.urlDlgCatLabel', '分类')}
        </label>
        <Csel
          id="res-cat"
          ariaLabel={t('resources.urlDlgCatLabel', '分类')}
          value={category}
          onChange={(v) => {
            setCategory(v as RuleResourceCategory);
            setCatDirty(true);
          }}
          options={catOptions}
        />
      </div>


      <div className="card-sub">
        {t('resources.urlDlgHint', '类型由扩展名/名称推断，可手动调整；下载遵循 GitHub 加速设置。')}
      </div>
    </Modal>
  );
}

export default ResUrlDialog;
