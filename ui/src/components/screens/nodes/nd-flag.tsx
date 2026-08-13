/**
 * nd-flag —— 节点卡片右下角「国旗水印」。1:1 移植自 上游（src/renderer/components/settings/
 * flag-detect.ts + flag-assets.ts + flag-regions.json + flag-assets.generated.ts，现搬进
 * `@/domain/flag-*`），替换掉本文件原先仅 5 条硬编码正则 + 手绘 `<symbol>` 精灵的残缺实现。
 *
 * 根因：旧 `NAME_FLAG_PATTERNS` 用 `/…|HK\b/i`，`\b` 词边界在 "Hk01" 这类数字紧邻字母尾的命名
 * 上不成立（K 与 0 都属 `\w`，两者之间无词边界）→ 真机 62 个 Hk/Jp 节点全无旗。上游的
 * `getCountryCode()` 数据驱动 74 区域 + 拉丁 token 用 `(?<![a-z])…(?![a-z])`（左右非小写字母，
 * 允许数字/连字符紧邻）——Hk01/US-02/JP_03 均命中，修复此漏报，非重新设计。
 *
 * 渲染也随之改为 上游的方案：`<img src="data:image/svg+xml,...">` 本地旗面（countryCodeToFlagAsset
 * 懒缓存），不再需要全屏挂载一次的 `<symbol>` 精灵。`NdFlagDefs` 保留导出（NodesScreen.tsx 仍调用
 * `<NdFlagDefs />`）但已降级为空操作，避免为此改动去碰 NodesScreen.tsx。
 */
import type { ReactNode } from 'react';
import { getCountryCode } from '@/domain/flag-detect';
import { countryCodeToFlagAsset } from '@/domain/flag-assets';

/** 按节点名称检测国旗代码（上游 getCountryCode 单一真值：emoji 区域指示符优先，其次 74 区域 manifest 合并 token regex）；无匹配返回 null。 */
export function flagCodeForName(name: string | undefined): string | null {
  if (!name) return null;
  return getCountryCode(name);
}

/** 空操作占位：旧 `<symbol>` 精灵已随 img/data-URI 渲染方案退役，无需全局挂载；仅为不破坏 NodesScreen.tsx 既有调用点而保留导出。 */
export function NdFlagDefs(): ReactNode {
  return null;
}

/**
 * 旗面本体：本地 SVG 旗面（data URI）。卡片视图对齐 上游 server-card.tsx 的
 * `<img className="nd-flag">`（右下角淡水印）；列表视图下同一元素被 CSS 重塑为名称前的
 * 小图标（对齐 上游 server-row.tsx 的 `.nd-row-flag`：20×14，不透明，见 screens.css
 * `#s-nodes.nodes-list-view .nd-flag`），故复用同一组件与 DOM 节点，不为列表另起一份。
 * `data-flag-code` 供测试/工具按国家码断言，不影响渲染。
 */
export function NdFlag({ code }: { code: string }): ReactNode {
  const asset = countryCodeToFlagAsset(code);
  if (!asset) return null;
  return (
    <img
      className="nd-flag"
      src={asset.src}
      alt=""
      aria-hidden="true"
      draggable={false}
      data-flag-code={code}
    />
  );
}
