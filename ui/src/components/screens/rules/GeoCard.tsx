/**
 * GeoCard —— 地区分流卡（1:1 提取自原型 polaris-prototype.html L1877-1900 .geo-card）。
 *
 * 原型 DOM（.card.geo-card，样式见 src/styles/prototype.css「RULES」段，勿改该文件）：
 *   头部：card-h + card-sub + .swt 总开关
 *   .geo-flow > .field-lbl + .rl-chain-flow（分流优先级流程条，rl-step「自定义规则」恒 .on，静态展示非动态）
 *   .field-lbl「你所在的地区」+ .geo-region（cn/ir/ru，margin-top:0 覆盖默认 10px）+ .geo-rev-btn（回国）+ info-i
 *
 * 数据源：UserConfig.regionRouting（经 effectiveRegionRouting 取生效值）。
 * 写入经 config.setValue('regionRouting', {...})。
 */

import { useTranslation } from 'react-i18next';
import type { RegionId, RegionRoutingConfig } from '@/contracts/types';
import { cn } from '@/lib/utils';

const ROUTING_CHAIN = ['custom', 'app', 'mesh', 'lan', 'smart', 'default'] as const;

export interface GeoCardProps {
  regionRouting: RegionRoutingConfig;
  onChange: (next: RegionRoutingConfig) => void;
  /**
   * 当前是否智能分流模式。**唯一用途**是决定要不要挂「仅智能分流模式生效」那条提示：
   * 智能模式下它恒真、说了等于没说，只是常驻噪音；非智能模式下才是有效信息。
   * 对齐 上游 `region-routing-card.tsx:97-102`（`{!isSmart && <div className="rl-smartonly">…}`）。
   */
  isSmartMode: boolean;
}

function InfoIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5M12 8h.01" />
    </svg>
  );
}

export function GeoCard({ regionRouting, onChange, isSmartMode }: GeoCardProps) {
  const { t } = useTranslation();
  const { enabled, region, reverse } = regionRouting;

  return (
    <div className="card geo-card">
      <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
        <div style={{ flex: 1 }}>
          <div className="card-h">{t('rules.regionRouting')}</div>
          {/* 说明必须跟随 reverse 反转：回国模式下语义相反（所在地区走代理、其余直连），
              旧实现恒显正向文案 → 误导（真机 2026-07-20 §1.4）。
              key 走扁平命名而非 `rules.regionRouting.sub`：`rules.regionRouting` 已是字符串，
              i18next 无法再向下取 `.sub`，旧 key 恒落 defaultValue（en/fa/ru 下也显中文）。 */}
          <div className="card-sub">
            {reverse
              ? t('rules.regionRoutingSubReverse')
              : t('rules.regionRoutingSub')}
          </div>
          {/* 「仅智能分流模式生效」原先硬编码在上面那句的尾巴上、常显：智能模式（绝大多数时间）下
              它恒真、纯噪音；真正需要它的非智能模式反而被淹在同一行灰字里。拆成独立条件提示，
              只在真不生效时出现（上游 region-routing-card.tsx:97 同做法）。 */}
          {!isSmartMode && (
            <div className="card-sub" style={{ color: 'hsl(var(--warn))' }}>
              {t('rules.regionRoutingSmartOnly')}
            </div>
          )}
        </div>
        <span
          className={cn('swt', enabled && 'on')}
          role="switch"
          aria-checked={enabled}
          tabIndex={0}
          onClick={() => onChange({ ...regionRouting, enabled: !enabled })}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onChange({ ...regionRouting, enabled: !enabled });
            }
          }}
        />
      </div>

      {/* 分流优先级流程条：讲的是**全局**优先级链，与地区分流开关无关（关掉它不改变链本身），
          故不随 enabled 收起——只有下面「你所在的地区 / 回国」这些地区分流自己的参数才收。 */}
      <div className="geo-flow">
        <div className="field-lbl">
          <span>
            {t('rules.routingPriority')}
          </span>
        </div>
        <div className="rl-chain-flow">
          {ROUTING_CHAIN.map((step, i) => (
            <span key={step} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
              <span className={cn('rl-step', step === 'custom' && 'on')}>
                {t(`rules.chain.${step}`)}
              </span>
              {i < ROUTING_CHAIN.length - 1 && <span className="rl-arrow">›</span>}
            </span>
          ))}
        </div>
      </div>

      {/* **关态收起**（对齐 上游 region-routing-card.tsx:38 的 `{region.enabled && (…)}`）：
          总开关关掉后，地区选择与「回国」原先仍全量渲染且可点 = **假的可操作性** —— 点了不生效，
          却照样把值写进 config，真机表现成「切了地区没反应」。 */}
      {enabled && (
        <>
          <div className="field-lbl" style={{ marginTop: 14 }}>
            <span>{t('rules.yourRegion')}</span>
          </div>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              gap: 12,
              flexWrap: 'wrap',
            }}
          >
            <div
              className="geo-region"
              role="group"
              aria-label={t('rules.yourRegion')}
              style={{ marginTop: 0 }}
            >
              {(['cn', 'ir', 'ru'] as RegionId[]).map((r) => (
                <button
                  key={r}
                  type="button"
                  className={cn(region === r && 'on')}
                  onClick={() => onChange({ ...regionRouting, region: r })}
                >
                  {r === 'cn'
                    ? t('rules.region.cn')
                    : r === 'ir'
                      ? t('rules.region.ir')
                      : t('rules.region.ru')}
                </button>
              ))}
            </div>
            <div style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
              <button
                type="button"
                className={cn('geo-rev-btn', reverse && 'on')}
                role="switch"
                aria-checked={reverse}
                onClick={() => onChange({ ...regionRouting, reverse: !reverse })}
              >
                <span className="geo-rev-dot" />
                <b>{t('rules.backHome')}</b>
              </button>
              <span
                className="info-i"
                tabIndex={0}
                aria-label={t('rules.backHomeTip')}
                data-tip={t('rules.backHomeTip')}
              >
                <InfoIcon />
              </span>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

export default GeoCard;
