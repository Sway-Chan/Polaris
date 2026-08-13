/**
 * 应用分流预设 —— **前端不再持有表本体**。
 *
 * 内置 16 条的单一真值在 Rust（`crates/config-engine/src/user_config/app_rules_preset_data.rs`），
 * 经 `app_presets_list` command 下发，由 `store/use-app-presets-store.ts` 启动时拉一次入 store。
 * 本模块只剩**跨语言类型镜像**（①类）+ **列表组合/查找**（②类 UI 派生）。
 *
 * 历史（本批终结）：这里曾有 `APP_PRESETS` 全表 + `QURE_BASE`，与 Rust 侧 16 条同构 —— 而 Rust 那份
 * 文件头自认「来源 = 本文件（1:1 提取）」。即**同一份数据两处维护、且两边都以为对方是抄本**。
 * 迁移前已做逐字段对拍取证：16/16 全等（labelKey/emoji/iconUrl/geositeTags/geoipTags/processNames/
 * category），并对 Rust 表做变异验证 6/6 转红 → 确认 Rust 那份是忠实转录而非有损手抄，方才删除本表。
 *
 * `defaultAppRules` / `seedDefaultAppRules` / `getAppPresetsByCategory` 一并删除：Rust 已有
 * （`default_app_rules` / `seed_default_app_rules`），且前端**零消费**（本批 grep 实证）。
 */
import { CustomAppPreset } from '../contracts/types';

export interface AppPreset {
  id: string;
  /** i18n key，对应 rules.apps.XXX；自定义应用则直接是其 name（见 appBuiltin 判定） */
  labelKey: string;
  /** iconUrl 加载失败时的兜底图标 */
  emoji: string;
  /** Qure Color 彩色图标集 */
  iconUrl?: string;
  geositeTags: string[];
  geoipTags?: string[];
  /** 用于跨平台（macOS/Windows/Linux）按进程名精准分流 */
  processNames?: string[];
  category: 'video' | 'social' | 'ai' | 'tools' | 'game';
}

/**
 * 自定义预设 → AppPreset 形态（**单一转换点**）。
 *
 * `category` 取真实值而非占位：原 `getAppPreset` 把自定义应用的 category 硬写 `'tools'`
 * （注释称「后端不消费 category，此处仅占位满足类型」），而 `AppPolicyScreen` 自己内联的那份
 * 转换用的是 `c.category ?? 'tools'`（真值）——**两份实现行为不一致，且内联那份才是对的**
 * （分类筛选/分组要真值）。收口取真值，无消费方回退。
 */
export function customToPreset(c: CustomAppPreset): AppPreset {
  return {
    id: c.id,
    labelKey: c.name, // 自定义应用直接存储名称（非 i18n key）
    emoji: c.emoji,
    iconUrl: c.iconUrl,
    geositeTags: c.geositeTags,
    geoipTags: c.geoipTags,
    // 进程名透传给配置生成（与内置预设同一消费点）；自定义应用亦可按进程名精准分流。
    processNames: c.processNames,
    category: (c.category as AppPreset['category']) ?? 'tools',
  };
}

/**
 * 合并内置 + 自定义为统一列表（**单一 merge 收口**）。
 *
 * 内置优先：id 撞内置的自定义项被丢弃 —— 与 `getAppPreset`「先内置、后自定义」及 Rust
 * `get_app_preset` 的优先级一致（自定义永远影子不了内置 id）。
 *
 * 为何合并在前端而不在 command 里：`builtin` 是启动时拉一次的静态表，`custom` 来自 config、
 * 随 `config:changed` 实时变。若在后端合并，前端那份缓存一加自定义应用就脱节。
 */
export function mergeAppPresets(builtin: AppPreset[], custom?: CustomAppPreset[]): AppPreset[] {
  if (!custom || custom.length === 0) return builtin;
  const builtinIds = new Set(builtin.map((p) => p.id));
  return [...builtin, ...custom.filter((c) => !builtinIds.has(c.id)).map(customToPreset)];
}

/**
 * 根据 appId 查找预设（内置 + 自定义合一）。
 *
 * `builtin` 由调用方从 store 传入（Rust 下发的那份）—— 本模块不再自持表，故不能像原来那样
 * 隐式闭包到模块级常量上。
 */
export function getAppPreset(
  appId: string,
  builtin: AppPreset[],
  customPresets?: CustomAppPreset[]
): AppPreset | undefined {
  const hit = builtin.find((p) => p.id === appId);
  if (hit) return hit;
  const custom = customPresets?.find((p) => p.id === appId);
  return custom ? customToPreset(custom) : undefined;
}
