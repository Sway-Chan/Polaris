/**
 * AppRuleHoverCard —— 应用条目 hover 详情卡内容（设计文档 §1.5 apprule；原型 registerTipCard('apprule', …)
 * polaris-prototype.html :5213-5225）：应用名 · 匹配条件 pill 墙（geosite/geoip/process，取自 AppPreset 真实
 * 字段而非原型 mock 的单值 .proc/.geosite 简化）· 分隔线 · 出口行（策略 pill + 目标节点；代理态无显式目标
 * 时显示「跟随全局」，与 PolicySelector 的 NodePicker 选项口径一致）。
 *
 * 挂载方式见 HoverCard.tsx；本文件只导出内容，调用方（AppPolicyScreen.tsx 的 AppCard / ap-row）负责
 * useHoverCard() + <HoverCardPanel> 组装。
 */
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import type { AppRule } from '@/contracts/types';
import type { AppPreset } from '@/domain/app-rules-preset';

/**
 * 应用展示名单一收口：内置预设的 `labelKey` 是 i18n key（对应 `rules.apps.XXX`，Rust SoT 下发，
 * 见 domain/app-rules-preset.ts:20），需过 t() 才是终端展示名；自定义应用的 `labelKey` 直接是用户输入
 * 的真实名称（customToPreset 注释），绝不能喂进 t()——否则用户把自定义应用取名 "netflix" 会被误翻成
 * "Netflix"。isCustom 由调用方按 builtinPresets 集合判定（与 AppPolicyScreen 的 builtinIds 同一口径）。
 * 找不到翻译（未知 labelKey）时 t() 的 defaultValue 回退显示原值，不会露出 `rules.apps.xxx` 这种 raw key。
 */
export function appPresetLabel(
  t: TFunction,
  labelKey: string | undefined,
  fallbackId: string,
  isCustom: boolean,
): string {
  if (!labelKey) return fallbackId;
  return isCustom ? labelKey : t(`rules.apps.${labelKey}`, labelKey);
}

export function AppRuleHoverCardContent({
  preset,
  rule,
  targetNodeName,
  isCustom,
}: {
  preset: AppPreset;
  rule: AppRule | undefined;
  targetNodeName?: string;
  /** 是否自定义应用（决定 preset.labelKey 是否需要过 t()，见 appPresetLabel）。 */
  isCustom: boolean;
}) {
  const { t } = useTranslation();
  const conds: { type: string; value: string }[] = [
    ...preset.geositeTags.map((v) => ({ type: 'geosite', value: v })),
    ...(preset.geoipTags ?? []).map((v) => ({ type: 'geoip', value: v })),
    ...(preset.processNames ?? []).map((v) => ({ type: 'process', value: v })),
  ];
  const action = rule?.action ?? 'proxy';

  return (
    <>
      <div className="tip-t">{appPresetLabel(t, preset.labelKey, preset.id, isCustom)}</div>
      {conds.length > 0 && (
        <div className="tc-conds">
          {conds.map((c, i) => (
            <span className="tc-cond" key={i}>
              <b>{c.type}</b> {c.value}
            </span>
          ))}
        </div>
      )}
      <div className="tc-sep" />
      <div className="tc-row">
        <span className="tc-lbl">{t('appPolicy.target')}</span>
        {action === 'direct' ? (
          <span className="pill act-direct">{t('rules.targetDirect')}</span>
        ) : action === 'block' ? (
          <span className="pill act-block">{t('rules.targetBlock')}</span>
        ) : (
          <>
            <span className="pill act-proxy">{t('appPolicy.action.proxy')}</span>
            <span>→ {targetNodeName ?? t('appPolicy.followGlobal')}</span>
          </>
        )}
      </div>
    </>
  );
}
