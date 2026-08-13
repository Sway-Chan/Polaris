/**
 * URL 下载弹窗纯逻辑 —— 从 URL 推断名称/分类 + 字段校验。
 *
 * 对齐原型 `inferRes` :5112（末段文件名去扩展名作 name；扩展名/文件名关键词推断分类）。
 * 抽为纯函数：vitest 覆盖推断与校验（往返/边界），不需 DOM。前端校验是即时反馈层，
 * 权威校验在 Rust 侧（§三 Q1）——提交最终门是 invoke 返回。
 */

import type { RuleResourceCategory } from '@/contracts/types';

export interface InferredResource {
  /** 去扩展名的文件名（可为空串，用户可覆盖） */
  name: string;
  /** 推断分类（默认 custom；文件名含 geoip/geosite 关键词时归类） */
  category: RuleResourceCategory;
}

const EXT_RE = /\.(srs|txt|json)$/i;

/**
 * 从资源 URL 推断名称与分类。
 *  - name：取末段路径（去 query）→ 去 .srs/.txt/.json 扩展名；
 *  - category：文件名含 "geoip" → geoip；含 "geosite" → geosite；否则 custom
 *    （比原型「扩展名 → 类型」更贴 RuleResourceDownloadItem.category 语义：.srs 可能是 geosite 或 geoip）。
 */
export function inferResource(url: string): InferredResource {
  const u = (url ?? '').trim();
  const base = (u.split('/').pop() ?? '').split('?')[0] ?? '';
  const name = base.replace(EXT_RE, '');
  const lower = base.toLowerCase();
  let category: RuleResourceCategory = 'custom';
  if (lower.includes('geoip')) category = 'geoip';
  else if (lower.includes('geosite')) category = 'geosite';
  return { name, category };
}

export type ResUrlError = 'urlEmpty' | 'urlInvalid' | 'nameEmpty';

/**
 * 校验 URL：非空 + http(s) 合法 URL。
 * 返回 null 表示通过，否则返回错误码（供 i18n 渲染内联 err）。
 */
export function validateResUrl(url: string): ResUrlError | null {
  const u = (url ?? '').trim();
  if (!u) return 'urlEmpty';
  let parsed: URL;
  try {
    parsed = new URL(u);
  } catch {
    return 'urlInvalid';
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    return 'urlInvalid';
  }
  return null;
}

/** 校验名称：非空。 */
export function validateResName(name: string): ResUrlError | null {
  return (name ?? '').trim() ? null : 'nameEmpty';
}
