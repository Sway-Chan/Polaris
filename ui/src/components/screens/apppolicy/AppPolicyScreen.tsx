/**
 * AppPolicyScreen —— 应用分流屏（1:1 提取自原型 polaris-prototype.html L1922-1963 #s-apppolicy）。
 *
 * 原型 DOM（class/id/层级顺序对齐，样式全走 src/styles/prototype.css「APP POLICY」段，勿改该文件）：
 *   .screen#s-apppolicy
 *     .phead（标题 + 实验性 pill + .acts：.swt 总开关）
 *     .app-summary（统计：应用数 + 跟随全局/指定节点/直连/阻断，四态用 <b> 非 .as-n）
 *     .rules-note#ap-wfp-note（Windows-only WFP 提示，纯 CSS data-os 门控）
 *     [新增] .mode-warn（预设加载失败兜底，无原型对应——见下）
 *     .mode-warn（global/direct 模式未生效 + 「切回智能」按钮）
 *     .ap-off-note（总开关关时提示）
 *     #ap-body（.ap-disabled 门控总开关/非智能态的整体降透明——控件仍可编辑）
 *       .conn-toolbar.ap-toolbar（.sel.csel.ap-cat 分类下拉 + .input 搜索 + .seg2 视图
 *         —— **有意偏离原型**：原型此处是平铺 `.ap-chips/.ap-chip`，横向占用随类目数线性增长，
 *         新增类目会挤掉同排搜索框，故改下拉；prototype.css 的 .ap-chips/.ap-chip 规则随之无消费方，
 *         但那是禁区文件不删，见 styles/index.css 同题覆盖）
 *       #ap-content
 *         .ap-group（每类目一组，含 .ap-group-head 名称+计数 + .ap-group-body 承载卡片墙/列表）
 *         .ap-add（添加自定义应用）
 *
 * ⚠️ 策略选择控件**回退原型 `.app-pol`**（内联策略 pill，点击弹出 `.mini-menu`：3 个快速策略
 * 「默认代理/直连/阻断」+ 折叠式节点分组）——**弃**此前自造的 `.pol-tile/.pol-sel` 三瓦片
 * （那是设计偏移，原型从未采用，见 design/polaris-ui-rebuild-plan.md §「现状有原型无」）。
 * 节点分组用 domain/server-grouping.ts 的 groupServersBySubscription（自建/组网/各订阅，与
 * Nodes 页 tab 分组同一真值），对应原型 nodeSelMenu 的 NODES.group 分组。
 * `.mini-menu` 用 position:fixed + getBoundingClientRect 定位（同 HoverCard.tsx 的已验证 clamp
 * 模式）——因 `.ap-list{overflow:hidden}`，position:absolute 在列表视图会被裁切。
 *
 * 数据流：
 *  - **内置预设表**：useAppPresetsStore（Rust `app_presets_list` 下发，启动时拉一次）。
 *    本屏不持有静态表——曾 `import { APP_PRESETS }` 直连前端表，与 Rust 侧同构双活，已删除。
 *  - 用户配置：useAppStore（config.appRules + config.customAppPresets + config.appRoutingEnabled +
 *    config.subscriptions + config.proxyMode）。
 *  - 二者合并经 domain/app-rules-preset.mergeAppPresets 单一收口。
 *  - 「切回智能」写 useAppStore.updateProxyMode（热切换，同 HomeScreen 分流策略切换口径，非
 *    config.setValue 直写——后者跳过 store 侧的热切换处理）。
 *  - 自定义应用移除：**原地二次点击**（`lib/confirm-twice.ts`，1:1 对齐原型 :4173 `app-remove`）。
 *    此前走 `kind:'confirm'` 弹窗，署名理由是「原型那套是 toy 交互」——诉诸外部惯例、无证据，
 *    正是产生对拍漂移的那类推理，已删。
 * 写入经 config.setValue('appRules'/'appRoutingEnabled'/'customAppPresets')。
 *
 * 预设加载失败（presetsError）：优雅降级为「仅显示自定义应用」+ 提示条，而非空白墙——
 * mergeAppPresets(builtinPresets=[], customPresets) 在内置表为空时天然只返回自定义项，不崩。
 */

import {
  Fragment,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { useAppStore, useEffectiveConfig, useEffectiveServers } from '@/store/app-store';
import { useAppPresetsStore } from '@/store/use-app-presets-store';
import { api } from '@/ipc';
import type { AppRule, RuleAction, ServerConfig, SubscriptionConfig } from '@/contracts/types';
import { mergeAppPresets, type AppPreset } from '@/domain/app-rules-preset';
import { groupServersBySubscription, defaultOpenGroupIds } from '@/domain/server-grouping';
import { iconProxySrc, ICON_PROXY_SCHEME } from '@/domain/icon-proxy';
import { brandIcon } from '@/components/brand-icons';
import { cn } from '@/lib/utils';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { toast } from '@/lib/error-handler';
import { editRoute } from '@/lib/staged-config';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { Csel } from '@/components/dialogs/Csel';
import { useHoverCard, HoverCardPanel } from '@/components/hover-cards/HoverCard';
import { AppRuleHoverCardContent, appPresetLabel } from '@/components/hover-cards/AppRuleHoverCard';
import { APP_CATEGORY_ALL, compareLabel, sortAppCategories } from './apppolicy-logic';
import { revealSiblingGroup, useRevealAfterCommit } from '@/components/reveal';

type AppView = 'cards' | 'list';
type AppCategory = 'all' | 'video' | 'social' | 'ai' | 'tools' | 'game';

/** 移除自定义应用的原地二次确认 key 前缀（原型 :4173 `app-remove`）。 */
const APP_REMOVE_PREFIX = 'app-remove:';

const CATEGORIES: { key: AppCategory; zh: string }[] = [
  { key: 'all', zh: '全部' },
  { key: 'video', zh: '视频' },
  { key: 'social', zh: '社交' },
  { key: 'ai', zh: 'AI' },
  { key: 'tools', zh: '工具' },
  { key: 'game', zh: '游戏' },
];

/** `.mini-menu` 内 3 个快速策略（原型 nodeSelMenu 行内数组，proxy 对应「默认代理」= 跟随全局）。 */
const QUICK_PICKS: { action: RuleAction; icon: string; key: string; zh: string; danger?: boolean }[] = [
  { action: 'proxy', icon: 'M12 5v14M5 12h14', key: 'appPolicy.defaultProxy', zh: '默认代理' },
  { action: 'direct', icon: 'M4 12h16', key: 'appPolicy.action.direct', zh: '直连' },
  { action: 'block', icon: 'M5 5l14 14', key: 'appPolicy.action.block', zh: '阻断', danger: true },
];

export function AppPolicyScreen() {
  // `i18n` 供分类排序取当前界面语言（`localeCompare` 不传 locale 会退化成运行时默认，对中文即码位序）。
  const { t, i18n } = useTranslation();
  /** 展示面：应用分流规则 / 自定义应用都是本屏可编辑的实体，读盘的话用户看不见自己刚做的编辑。
   *  下面三条 `api.config.setValue` 直落盘腿也拿它当基准 —— 那几条腿的键（appRules /
   *  customAppPresets / appRoutingEnabled）全是 Class B，总开关一开必走暂存腿、直落盘腿不可达；
   *  开关关着时条目恒空、effective 即入参本体 ⇒ 两侧基准都是磁盘那份，无第三种情形。 */
  const config = useEffectiveConfig();
  /** 展示面：策略 pill 上的目标节点名映射（不喂任何按 id 查盘的后端调用）。 */
  const servers = useEffectiveServers();
  const subscriptions = config?.subscriptions ?? [];
  const openDialog = useDialogStore((s) => s.open);
  /** 移除自定义应用的原地二次确认（原型 :4173 `app-remove`）。 */
  const { armed, confirmTwice } = useConfirmTwice();
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);

  const enabled = config?.appRoutingEnabled !== false; // undefined=true 兼容老配置
  const proxyMode = config?.proxyMode ?? 'smart';
  const isSmartMode = proxyMode === 'smart';

  // 内置预设表：Rust SoT，启动/进屏时拉一次（store 内部单飞 + 已拉则短路）。
  const builtinPresets = useAppPresetsStore((s) => s.presets);
  const presetsError = useAppPresetsStore((s) => s.error);
  const presetsLoading = useAppPresetsStore((s) => s.loading);
  const presetsLoaded = useAppPresetsStore((s) => s.loaded);
  const loadPresets = useAppPresetsStore((s) => s.loadPresets);
  useEffect(() => {
    void loadPresets();
  }, [loadPresets]);

  /**
   * 「表还没到」——空态文案的分流门。
   *
   * 只订 `loading` 不够：首帧 loading 还是 false（effect 尚未跑、单飞也还没 set），此时 presets=[] ⇒
   * 卡片墙直接写「无匹配应用」，把「还没加载」谎报成「查无此应用」。故必须并上 `!loaded && !error`
   * 这条「一次都还没成功拉到、且不是已经失败」的判据，把 loading 翻起来之前的那一段也盖住。
   * 失败态不算 pending：那时 presetsError 的提示条已在上方说明「仅显示自定义应用」。
   */
  const presetsPending = presetsLoading || (!presetsLoaded && !presetsError);

  const builtinIds = useMemo(() => new Set(builtinPresets.map((p) => p.id)), [builtinPresets]);

  const customPresets = config?.customAppPresets;
  // 内置 ∪ 自定义 —— 单一 merge 收口。
  const presets = useMemo(
    () => mergeAppPresets(builtinPresets, customPresets),
    [builtinPresets, customPresets],
  );

  // appId → AppRule
  const ruleByAppId = useMemo(() => {
    const m = new Map<string, AppRule>();
    (config?.appRules ?? []).forEach((r) => m.set(r.appId, r));
    return m;
  }, [config?.appRules]);

  const [view, setView] = useState<AppView>('cards');
  const [category, setCategory] = useState<AppCategory>('all');
  const [search, setSearch] = useState('');

  const visiblePresets = useMemo(() => {
    let list = presets;
    if (category !== 'all') list = list.filter((p) => p.category === category);
    if (search.trim()) {
      const q = search.trim().toLowerCase();
      // 除 id/labelKey 原值外，还要匹配真正展示出的名称（appPresetLabel）——否则内置预设翻译后
      // 卡片显示 "ChatGPT"，用户照着卡片打「chatgpt」搜却因过滤只比对未翻译的 labelKey("openai")
      // 而搜不到，所见非所搜。
      list = list.filter((p) => {
        const isCustom = !builtinIds.has(p.id);
        return (
          p.id.toLowerCase().includes(q) ||
          (p.labelKey || '').toLowerCase().includes(q) ||
          appPresetLabel(t, p.labelKey, p.id, isCustom).toLowerCase().includes(q)
        );
      });
    }
    return list;
  }, [presets, category, search, builtinIds, t]);

  // 类目在当前语言下的显示名 + 稳定排序（判据在 apppolicy-logic，可单测）。**筛选下拉与内容分组共用
  // 这一份**：两处顺序天然一致，且语言切换时一起重排（原型 APP_CATS.slice().sort(localeCompare) 同款，
  // 但原型那句没传 locale，对汉字是码位序 —— 见 apppolicy-logic 头注）。
  const sortedCats = useMemo(
    () =>
      sortAppCategories(
        CATEGORIES.map((c) => ({ key: c.key, label: t(`appPolicy.cat.${c.key}`, c.zh) })),
        i18n.language,
      ),
    [t, i18n.language],
  );

  /** 内容分组的真实类目（去掉 'all' 这颗「不过滤」伪类目）——对应原型 APP_CATS（自定义应用按其真实
   * category 归组，不是原型那颗单独 'custom' 伪类目：domain/app-rules-preset.ts customToPreset 已有
   * 定论，分组要真值）。 */
  const groupCats = useMemo(
    () => sortedCats.filter((c) => c.key !== APP_CATEGORY_ALL),
    [sortedCats],
  );

  // 统计（5 路：总数 + 跟随全局 + 指定节点 + 直连 + 阻断，恒和为 total —— 对齐原型
  // updateAppSummary :4597-4601。无显式规则 / 规则未启用 / action=proxy 且未指定节点 三种情况都
  // 归"跟随全局"，即视觉默认态）。
  const summary = useMemo(() => {
    let followGlobal = 0;
    let node = 0;
    let direct = 0;
    let block = 0;
    presets.forEach((p) => {
      const r = ruleByAppId.get(p.id);
      const action = r?.enabled ? r.action : undefined;
      if (action === 'direct') direct++;
      else if (action === 'block') block++;
      else if (action === 'proxy' && r?.targetServerId) node++;
      else followGlobal++;
    });
    return { total: presets.length, followGlobal, node, direct, block };
  }, [presets, ruleByAppId]);

  /** 暂存条目 label 用的应用显示名（与卡片上看到的一致；查不到就退回 appId）。 */
  const appLabel = (appId: string): string => {
    const p = presets.find((x) => x.id === appId);
    return p ? appPresetLabel(t, p.labelKey, p.id, !builtinIds.has(p.id)) : appId;
  };

  const applyRule = async (appId: string, patch: Omit<AppRule, 'appId'>) => {
    const newRule: AppRule = { appId, ...patch };
    // 配置暂存闸门：`appRules` 是 UserConfig 字段（Class B），改策略无远端/不可逆副作用 ⇒ 默认腿。
    // 条目按 **appId** 寻址 —— `AppRule` 里根本没有 `id` 字段，这一族此前表达不了正是因为
    // 模型把主键写死成了 `id`（现由 `ID_ADDRESSED_COLLECTIONS` 的集合→主键映射给出）。
    if (editRoute('appRules', stagingEnabled) === 'staged') {
      stage({
        id: `appRule:${appId}`,
        kind: 'appRule',
        label: t('home.stagedAppPolicy', { name: appLabel(appId), defaultValue: '应用策略 · {{name}}' }),
        entityPath: ['appRules', appId],
        nextValue: newRule,
      });
      toast.success(t('appPolicy.policyUpdated', '策略已更新'));
      return; // 零 IPC 写、零磁盘写（FR-1）
    }
    const nextRules = [...(config?.appRules ?? [])];
    const idx = nextRules.findIndex((r) => r.appId === appId);
    if (idx >= 0) nextRules[idx] = newRule;
    else nextRules.push(newRule);
    try {
      await api.config.setValue('appRules', nextRules);
      // setValue 只写后端 → 必须同步 patch store（config 从 store 读），否则策略/开关切换后 UI 不反映
      // （同 RulesScreen 地区切换的「无法切换」根因；granular 写保留，仅补 store 回流）。
      useAppStore.setState((s) => (s.config ? { config: { ...s.config, appRules: nextRules } } : {}));
      // 原型 nodeSelMenu onPick :4635 → notify('策略已更新','ok')。本函数只此一个调用点（setAppPolicy），
      // 故 toast 挂这里即等价于挂在策略提交点上。
      toast.success(t('appPolicy.policyUpdated', '策略已更新'));
    } catch (err) {
      // 写失败时 store 未 patch → pill 停在旧策略，与「菜单点了没反应」同形；且用户会以为该应用已按新策略走。
      console.error('[AppPolicyScreen] applyRule failed:', err);
      toast.error(
        t('appPolicy.policyUpdateFail', '策略更新失败'),
        err instanceof Error ? err.message : undefined
      );
    }
  };

  /** 单一策略提交点（原型 nodeSelMenu onPick 同口径：一次调用把 action+targetServerId 一并落地，
   * 避免「先设 action 再设 target」两次 config.setValue 互相用旧快照覆盖）。 */
  const setAppPolicy = (appId: string, action: RuleAction, targetServerId: string | undefined) => {
    const existing = ruleByAppId.get(appId);
    void applyRule(appId, { action, enabled: existing?.enabled ?? true, targetServerId });
  };

  /** 移除自定义应用（原型 apCardHtml/apRowHtml 的 RM_BTN，仅 custom 项渲染）：同时清掉该应用的
   * 分流规则（若有），避免 config 里留下指向已删预设的孤儿规则。 */
  const removeCustomApp = async (appId: string) => {
    const hasRule = (config?.appRules ?? []).some((r) => r.appId === appId);
    // 配置暂存闸门：`customAppPresets` / `appRules` 都是 UserConfig 字段（Class B），删自定义应用
    // 只动 config，无远端/文件副作用 ⇒ 默认腿。
    //
    // **一次删除产生两条条目**（预设一条 + 它的分流规则一条），因为二者是两个集合、两个主键 ——
    // 这也是这一族必须整族进暂存的原因：只暂存一半会在盘上留下指向已删预设的孤儿规则。
    // 两条重放后与下面那条直落盘腿的结果逐字段一致（`staged-config.test.ts` 有专测钉住）。
    //
    // 同理，撤销也必须整族一起走：`groupId` 让 `revertEntry` 连坐同组（`staged-config.ts`），
    // 否则用户逐条撤其中一条就得到「预设还在、规则没了」——一个他从没要过的第三种状态。
    // 遗留：明细 popover 仍显示成两行，点一行两行齐消，观感突兀；合并显示属 UI 打磨，本轮不做。
    if (editRoute('customAppPresets', stagingEnabled) === 'staged') {
      const removeGroup = `appRemove:${appId}`;
      stage({
        id: `appPreset:${appId}`,
        kind: 'appPreset',
        label: t('home.stagedAppRemove', { name: appLabel(appId), defaultValue: '移除应用 · {{name}}' }),
        entityPath: ['customAppPresets', appId],
        nextValue: null,
        groupId: removeGroup,
      });
      if (hasRule) {
        stage({
          id: `appRule:${appId}`,
          kind: 'appRule',
          label: t('home.stagedAppRuleRemove', {
            name: appLabel(appId),
            defaultValue: '移除应用规则 · {{name}}',
          }),
          entityPath: ['appRules', appId],
          nextValue: null,
          groupId: removeGroup,
        });
      }
      toast.info(t('appPolicy.removed', '已移除'));
      return; // 零 IPC 写、零磁盘写（FR-1）
    }
    const nextCustom = (config?.customAppPresets ?? []).filter((p) => p.id !== appId);
    try {
      await api.config.setValue('customAppPresets', nextCustom);
      useAppStore.setState((s) => (s.config ? { config: { ...s.config, customAppPresets: nextCustom } } : {}));
    } catch (err) {
      // 用户刚在确认弹窗点了「删除」，弹窗关掉但卡片还在 → 不报就是「确认了个寂寞」。
      console.error('[AppPolicyScreen] removeCustomApp failed:', err);
      toast.error(
        t('appPolicy.removeFail', '移除失败'),
        err instanceof Error ? err.message : undefined
      );
      return;
    }
    // 原型 app-remove :4173 → notify('已移除')（中性 kind）。此时卡片已消失，这条是「确实删了」的落定确认。
    toast.info(t('appPolicy.removed', '已移除'));
    const existingRules = config?.appRules ?? [];
    if (existingRules.some((r) => r.appId === appId)) {
      const cleaned = existingRules.filter((r) => r.appId !== appId);
      try {
        await api.config.setValue('appRules', cleaned);
        useAppStore.setState((s) => (s.config ? { config: { ...s.config, appRules: cleaned } } : {}));
      } catch (err) {
        // 应用本体已移除但其分流规则没清掉 —— 这是**部分失败**，上面那条「已移除」并不涵盖它。
        // config 里会留下指向已删预设的孤儿规则（仍在生效），必须单独告警，不能被成功 toast 盖过去。
        console.error('[AppPolicyScreen] removeCustomApp rule cleanup failed:', err);
        toast.warning(t('appPolicy.removeRuleCleanupFail', '应用已移除，但其分流规则未能清除'));
      }
    }
  };

  /** 原地二次点击（原型 :4173）。key 带 appId ⇒ 武装 B 应用会自动解除 A 应用。 */
  const requestRemoveCustomApp = (appId: string) => {
    confirmTwice(`${APP_REMOVE_PREFIX}${appId}`, () => {
      void removeCustomApp(appId);
    });
  };

  // 主开关关闭 或 非智能模式：整个 #ap-body 置灰（仅透明度，控件仍可编辑，编辑经 config 保存——对齐原型
  // syncAppDim :3603 `dim = off || strategy!=='smart'`，dim 目标是 #ap-body 而非整屏，故不含 phead/摘要/提示条）。
  const bodyDimmed = !enabled || !isSmartMode;

  return (
    <section id="s-apppolicy" className="screen">
      <div className="phead">
        <div>
          {/* 「实验性」徽章已移除（陈先生 2026-07-29 裁定）：该面已随本轮真机验证转入正式功能面。 */}
          <h1>{t('sidebar.appPolicy', '应用分流')}</h1>
        </div>
        <div className="acts">
          <span
            className={cn('swt', enabled && 'on')}
            role="switch"
            aria-checked={enabled}
            tabIndex={0}
            data-tip={t('appPolicy.masterTip', '应用分流总开关')}
            onClick={async () => {
              const nextEnabled = !enabled;
              // 同一个 `api.config.setValue` 调用点跨三个键（appRules / customAppPresets /
              // appRoutingEnabled），三个都是 Class B ⇒ 三个都得过闸门。漏掉这一个会让接线守卫里
              // 那一行「staged」名不副实：入口登记是 (文件, 方法) 粒度的。
              if (editRoute('appRoutingEnabled', stagingEnabled) === 'staged') {
                stage({
                  id: 'setting:appRoutingEnabled',
                  kind: 'setting',
                  label: t('appPolicy.masterTip', '应用分流总开关'),
                  entityPath: ['appRoutingEnabled'],
                  nextValue: nextEnabled,
                });
                return; // 零 IPC 写、零磁盘写（FR-1）
              }
              try {
                await api.config.setValue('appRoutingEnabled', nextEnabled);
                useAppStore.setState((s) => (s.config ? { config: { ...s.config, appRoutingEnabled: nextEnabled } } : {}));
              } catch (err) {
                // 原型总开关（.swt）成功态不 notify——开关自身跳位即反馈，故这里只补失败分支：
                // 写失败时 store 未 patch → 开关弹回原位，用户读作「开关坏了」而非「没存上」。
                console.error('[AppPolicyScreen] toggle master:', err);
                toast.error(
                  t('appPolicy.masterToggleFail', '应用分流开关保存失败'),
                  err instanceof Error ? err.message : undefined
                );
              }
            }}
          />
        </div>
      </div>

      {/* 统计摘要（5 路：总数 · 跟随全局 · 指定节点 · 直连 · 阻断——仅首个用 .as-n，其余用 <b>，
          对齐原型 updateAppSummary :4598-4602） */}
      <div className="app-summary">
        <span>
          <span className="as-n">{summary.total}</span> {t('rules.appCountUnit', '应用')}
        </span>
        <span style={{ color: 'hsl(var(--flow-hi))' }}>
          {t('appPolicy.followGlobal', '跟随全局')} <b>{summary.followGlobal}</b>
        </span>
        <span style={{ color: 'hsl(var(--flow-hi))' }}>
          {t('appPolicy.summary.node', '指定节点')} <b>{summary.node}</b>
        </span>
        <span style={{ color: 'hsl(var(--ok))' }}>
          {t('appPolicy.summary.direct', '直连')} <b>{summary.direct}</b>
        </span>
        <span style={{ color: 'hsl(var(--err))' }}>
          {t('appPolicy.summary.block', '阻断')} <b>{summary.block}</b>
        </span>
      </div>

      {/* Windows-only：WFP 进程分流尽力而为提示（原型 #ap-wfp-note :1930）。纯 CSS 按 :root[data-os="win"]
          门控（见 prototype.css），data-os 由 AppShell 写在 <html> 上——本屏不重复做 OS 检测。 */}
      <div className="rules-note" id="ap-wfp-note">
        <svg viewBox="0 0 24 24" width={15} fill="none" stroke="currentColor" strokeWidth={1.8}>
          <circle cx="12" cy="12" r="9" />
          <path d="M12 8v5M12 16h.01" />
        </svg>
        <span>
          {t('appPolicy.wfpNote', 'Windows WFP 进程分流为尽力而为，已内建 IP 兜底排除')}
        </span>
      </div>

      {/* 内置预设表拉取失败：诚实提示而非静默空墙（空墙会让用户以为"没有应用可选"）。
          自定义应用仍来自 config，故此时卡片墙降级为只剩自定义项。原型无对应元素——纯前端韧性兜底。 */}
      {presetsError && (
        <div className="mode-warn show">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 8v5M12 16h.01" />
          </svg>
          <span>
            {t('appPolicy.presetsLoadFailed', '内置应用预设加载失败，仅显示自定义应用')}
          </span>
        </div>
      )}

      {/* 模式警告（原型 #app-mode-warn :1932，含「切回智能」按钮——此前实现缺失该按钮）。 */}
      {!isSmartMode && (
        <div className="mode-warn show">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 8v5M12 16h.01" />
          </svg>
          <span>
            {t(
              'appPolicy.modeWarn',
              proxyMode === 'global'
                ? '当前为全局模式，应用分流未生效'
                : '当前为直连模式，应用分流未生效',
            )}
          </span>
          <button
            type="button"
            className="btn ghost sm"
            onClick={() => {
              void useAppStore.getState().updateProxyMode('smart');
            }}
          >
            {t('appPolicy.backToSmart', '切回智能')}
          </button>
        </div>
      )}

      {/* 总开关关提示（原型 #ap-off-note :1935） */}
      {!enabled && (
        <div className="ap-off-note">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 8v5M12 16h.01" />
          </svg>
          <span>
            {t(
              'appPolicy.offNote',
              '应用分流未启用 · 编辑已保存，启用后生效',
            )}
          </span>
        </div>
      )}

      <div id="ap-body" className={cn(bodyDimmed && 'ap-disabled')}>
        {/* 工具栏 */}
        <div className="conn-toolbar ap-toolbar">
          {/* 分类筛选：下拉而非平铺 chips（用户真机反馈）。平铺的横向占用随类目数线性增长，六个已把
              `.ap-toolbar` 首行吃掉大半，再加类目就把同排搜索框挤到换行；下拉的宽度与类目数无关。
              复用既有 Csel（非 dialog 场景它自己 portal 到 body 逃 container-type 包含块，见其文件头注），
              不新造下拉。aria-label 沿用原 `.ap-chips` 的英文原文，与同排 `.seg2` 的 "View" 一致，
              不在本次改动里顺手扩 i18n 键（会连带动 locale-parity 门的债务基线）。 */}
          <Csel
            className="ap-cat"
            id="ap-cat-filter"
            ariaLabel="Category filter"
            value={category}
            onChange={(v) => setCategory(v as AppCategory)}
            options={sortedCats.map((c) => ({ value: c.key, label: c.label }))}
          />
          <label className="input">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} style={{ width: 15, color: 'hsl(var(--fg-faint))' }}>
              <circle cx="11" cy="11" r="7" />
              <path d="M20 20l-3-3" />
            </svg>
            <input
              type="search"
              placeholder={t('appPolicy.search', '搜索应用 / 进程…')}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              style={{ border: 0, background: 'none', outline: 'none', flex: 1, color: 'inherit' }}
            />
          </label>
          <div className="seg2" role="group" aria-label="View">
            <button
              type="button"
              className={cn(view === 'cards' && 'on')}
              onClick={() => setView('cards')}
            >
              {t('appPolicy.view.cards', '卡片')}
            </button>
            <button
              type="button"
              className={cn(view === 'list' && 'on')}
              onClick={() => setView('list')}
            >
              {t('appPolicy.view.list', '列表')}
            </button>
          </div>
        </div>

        {/* 内容：按类目分组（原型 renderAppPolicy 每类目一个 .ap-group，组内按名称排序），
            每组内部再切卡片墙 / 列表两态。 */}
        <div id="ap-content">
          {visiblePresets.length === 0 ? (
            <div className="ap-empty">
              {presetsPending ? t('common.loading', '加载中…') : t('appPolicy.empty', '无匹配应用')}
            </div>
          ) : (
            groupCats.map((cat) => {
              // 按「名称」排序（原型 renderAppPolicy 的既定语义）：必须用最终展示名而非 labelKey 原值——
              // 否则内置预设排序键是未翻译的 i18n key（如 "openai"），与本批修复后实际显示的 "ChatGPT"
              // 错位，组内视觉顺序就不再是字母序（同一 bug 的排序侧，随 labelKey 展示修复一并订正）。
              // 判据与分组标题、筛选下拉同源（compareLabel），组内中文名同样吃拼音序而非码位序。
              const items = visiblePresets
                .filter((p) => p.category === cat.key)
                .slice()
                .sort((a, b) =>
                  compareLabel(
                    appPresetLabel(t, a.labelKey, a.id, !builtinIds.has(a.id)),
                    appPresetLabel(t, b.labelKey, b.id, !builtinIds.has(b.id)),
                    i18n.language,
                  ),
                );
              if (items.length === 0) return null;
              return (
                <div key={cat.key} className={cn('ap-group', view === 'list' && 'is-list')}>
                  <div className="ap-group-head">
                    <span className="apg-name">{cat.label}</span>
                    <span className="apg-count">{items.length}</span>
                  </div>
                  <div className="ap-group-body">
                    {view === 'list' ? (
                      <div className="ap-list">
                        {items.map((preset) => (
                          <ApRow
                            key={preset.id}
                            preset={preset}
                            rule={ruleByAppId.get(preset.id)}
                            servers={servers}
                            subscriptions={subscriptions}
                            isCustom={!builtinIds.has(preset.id)}
                            removeConfirming={armed === `${APP_REMOVE_PREFIX}${preset.id}`}
                            onPick={setAppPolicy}
                            onRemove={requestRemoveCustomApp}
                          />
                        ))}
                      </div>
                    ) : (
                      <div className="app-wall">
                        {items.map((preset) => (
                          <AppCard
                            key={preset.id}
                            preset={preset}
                            rule={ruleByAppId.get(preset.id)}
                            servers={servers}
                            subscriptions={subscriptions}
                            isCustom={!builtinIds.has(preset.id)}
                            removeConfirming={armed === `${APP_REMOVE_PREFIX}${preset.id}`}
                            onPick={setAppPolicy}
                            onRemove={requestRemoveCustomApp}
                          />
                        ))}
                      </div>
                    )}
                  </div>
                </div>
              );
            })
          )}

          <button
            type="button"
            className="ap-add"
            onClick={() => openDialog({ kind: 'app-add' })}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 5v14M5 12h14" />
            </svg>
            <span>{t('appPolicy.addCustom', '添加自定义应用')}</span>
          </button>
        </div>
      </div>
    </section>
  );
}

/**
 * 应用图标：内置预设优先命中随包本地品牌 SVG（brandIcon，零网络，与解锁徽章同源复用）；
 * 未 bundle 的内置项 iconUrl 是 Qure/QuanX 等第三方 CDN 直链 —— 绝不代理，直接 emoji/首字母兜底
 * （这是本函数要堵的隐私洞：旧版对着这类裸 http(s) URL 调 iconProxySrc，等于每次渲染都发起一次
 * 在线请求）。只有自定义应用的本地缓存图标（`polaris-icon://c/…`，caching 层落盘后 preset.iconUrl
 * 持有的形态）才算「已经在本地」，走 iconProxySrc（该函数对 c/ 引用本就原样直通，不二次代理）+
 * img/emoji 兜底。
 */
function AppIcon({
  preset,
  rootRef,
  rootHandlers,
}: {
  preset: AppPreset;
  /** hover 触发器挂在图标本体（logo）而非整卡——用户要求：只有 hover logo 才弹详情卡，移到策略/节点
   *  切换区不弹，避免遮挡操作。故 useHoverCard 的 ref/handlers 由 AppCard/ApRow 传入铺到根 span。 */
  rootRef?: React.Ref<HTMLSpanElement>;
  rootHandlers?: React.HTMLAttributes<HTMLSpanElement>;
}) {
  const [imgError, setImgError] = useState(false);
  const brand = brandIcon(preset.id);
  if (brand) {
    return (
      <span ref={rootRef} className="app-ico brand" {...rootHandlers}>
        {brand}
      </span>
    );
  }
  const isLocalCacheRef = preset.iconUrl?.startsWith(`${ICON_PROXY_SCHEME}://c/`) ?? false;
  const src = isLocalCacheRef ? iconProxySrc(preset.iconUrl) : '';
  return (
    <span ref={rootRef} className="app-ico" {...rootHandlers}>
      {src && !imgError ? (
        <img
          className="app-ico-img"
          src={src}
          alt=""
          onError={() => setImgError(true)}
        />
      ) : (
        <span className="ico-fb">{preset.emoji || preset.id.charAt(0).toUpperCase()}</span>
      )}
    </span>
  );
}

/** rule.targetServerId → 服务器显示名（hover 卡出口行用；action≠proxy 或未指定目标时 undefined）。 */
function targetNodeNameFor(rule: AppRule | undefined, servers: ServerConfig[]): string | undefined {
  if (!rule?.targetServerId) return undefined;
  return servers.find((s) => s.id === rule.targetServerId)?.name;
}

/** 移除按钮（原型 RM_BTN，仅自定义应用渲染；卡片态绝对定位右上角、列表态随行内 hover 显现，
 * 两处样式差异全走 prototype.css 既有规则，标记不重复写）。
 *
 * 武装态（原地二次点击的第一下）：`.app-remove.confirming` 翻实心红 + 强制 `opacity:1`
 * （见 `styles/components.css` 那条规则的注释：**原型这条是缺的**，照抄就是隐形闸门）+
 * aria-label 换成提示语（纯颜色状态对读屏用户不可达）。原型此处 msg 传空串、图标钮无 `<span>`
 * ⇒ 无文案可换，与 NodeCard 的 `node-del` 同形。 */
function RemoveButton({ confirming, onClick }: { confirming: boolean; onClick: () => void }) {
  const { t } = useTranslation();
  const label = confirming ? t('common.confirmAgain', '再点一次确认') : 'Remove';
  return (
    <button
      type="button"
      className={cn('app-remove', confirming && 'confirming')}
      aria-label={label}
      data-tip={confirming ? label : undefined}
      onClick={onClick}
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
        <path d="M5 5l14 14M19 5L5 19" />
      </svg>
    </button>
  );
}

/** 应用卡片（卡片视图）。整卡 hover 弹 AppRuleHoverCard（原型 :4573 data-tipcard="apprule" 挂在 .app-card 上）。 */
function AppCard({
  preset,
  rule,
  servers,
  subscriptions,
  isCustom,
  removeConfirming,
  onPick,
  onRemove,
}: {
  preset: AppPreset;
  rule: AppRule | undefined;
  servers: ServerConfig[];
  subscriptions: SubscriptionConfig[];
  isCustom: boolean;
  removeConfirming: boolean;
  onPick: (appId: string, action: RuleAction, targetServerId: string | undefined) => void;
  onRemove: (appId: string) => void;
}) {
  const { t } = useTranslation();
  const hc = useHoverCard<HTMLSpanElement>();
  return (
    <div className="app-card">
      <AppIcon preset={preset} rootRef={hc.triggerRef} rootHandlers={hc.triggerHandlers} />
      <div className="app-tx">
        <b>{appPresetLabel(t, preset.labelKey, preset.id, isCustom)}</b>
        <div>{t(`appPolicy.cat.${preset.category}`, preset.category)}</div>
      </div>
      <PolicySelector
        appId={preset.id}
        action={rule?.action ?? 'proxy'}
        targetServerId={rule?.targetServerId}
        servers={servers}
        subscriptions={subscriptions}
        onPick={onPick}
      />
      {isCustom && (
        <RemoveButton confirming={removeConfirming} onClick={() => onRemove(preset.id)} />
      )}
      <HoverCardPanel
        cardRef={hc.cardRef}
        open={hc.open}
        pos={hc.pos}
        onMouseEnter={hc.cardHandlers.onMouseEnter}
        onMouseLeave={hc.cardHandlers.onMouseLeave}
      >
        <AppRuleHoverCardContent
          preset={preset}
          rule={rule}
          targetNodeName={targetNodeNameFor(rule, servers)}
          isCustom={isCustom}
        />
      </HoverCardPanel>
    </div>
  );
}

/** 应用行（列表视图）。整行 hover 弹 AppRuleHoverCard（原型 :4576 data-tipcard="apprule" 挂在 .ap-row 上）。 */
function ApRow({
  preset,
  rule,
  servers,
  subscriptions,
  isCustom,
  removeConfirming,
  onPick,
  onRemove,
}: {
  preset: AppPreset;
  rule: AppRule | undefined;
  servers: ServerConfig[];
  subscriptions: SubscriptionConfig[];
  isCustom: boolean;
  removeConfirming: boolean;
  onPick: (appId: string, action: RuleAction, targetServerId: string | undefined) => void;
  onRemove: (appId: string) => void;
}) {
  const { t } = useTranslation();
  const hc = useHoverCard<HTMLSpanElement>();
  return (
    <div className="ap-row">
      <AppIcon preset={preset} rootRef={hc.triggerRef} rootHandlers={hc.triggerHandlers} />
      <div className="ap-row-tx">
        <b>{appPresetLabel(t, preset.labelKey, preset.id, isCustom)}</b>
        {preset.processNames && preset.processNames.length > 0 && (
          <span className="ap-proc">{preset.processNames.slice(0, 2).join(', ')}</span>
        )}
      </div>
      <PolicySelector
        appId={preset.id}
        action={rule?.action ?? 'proxy'}
        targetServerId={rule?.targetServerId}
        servers={servers}
        subscriptions={subscriptions}
        onPick={onPick}
      />
      {isCustom && (
        <RemoveButton confirming={removeConfirming} onClick={() => onRemove(preset.id)} />
      )}
      <HoverCardPanel
        cardRef={hc.cardRef}
        open={hc.open}
        pos={hc.pos}
        onMouseEnter={hc.cardHandlers.onMouseEnter}
        onMouseLeave={hc.cardHandlers.onMouseLeave}
      >
        <AppRuleHoverCardContent
          preset={preset}
          rule={rule}
          targetNodeName={targetNodeNameFor(rule, servers)}
          isCustom={isCustom}
        />
      </HoverCardPanel>
    </div>
  );
}

/** 策略 pill 的视觉态（class + 色点 + 文案）——对齐原型 appPolView :4553-4558。 */
function appPolicyView(
  action: RuleAction,
  targetServerId: string | undefined,
  servers: ServerConfig[],
  t: TFunction,
): { cls: string; dot: string; label: string } {
  if (action === 'direct') {
    return { cls: 'act-direct', dot: 'direct', label: t('appPolicy.action.direct', '直连') };
  }
  if (action === 'block') {
    return { cls: 'act-block', dot: 'block', label: t('appPolicy.action.block', '阻断') };
  }
  if (targetServerId) {
    const name = servers.find((s) => s.id === targetServerId)?.name;
    return { cls: 'act-proxy', dot: 'proxy', label: name ?? t('appPolicy.summary.node', '指定节点') };
  }
  return { cls: 'act-proxy', dot: 'proxy', label: t('appPolicy.followGlobal', '跟随全局') };
}

function CheckMark() {
  return (
    <svg className="mk-ck" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
      <path d="M5 12l5 5 9-11" />
    </svg>
  );
}

/**
 * 策略选择器 —— 原型 `.app-pol` 内联 pill（点击弹出 nodeSelMenu 风格 `.mini-menu`）：
 *  - 顶部 3 个快速策略：默认代理（=跟随全局，清空 targetServerId）/ 直连 / 阻断。
 *  - 分隔线后「指定节点」：按 groupServersBySubscription 分组（自建/组网/各订阅），
 *    组头可折叠展开（原型 ns-grp/ns-node，多组可同时展开，非手风琴互斥）。
 *    **默认全折叠，只展开含当前指定节点的那一组**（判据 = domain/server-grouping 的
 *    `defaultOpenGroupIds`，与规则弹窗目标出站、托盘「全部节点」共用同一条线）。
 * 菜单用 position:fixed + 实测 anchor/menu 矩形定位（同 HoverCard.tsx 已验证的 clamp 手法）——
 * `.ap-list` 有 overflow:hidden，position:absolute 在列表视图会被裁切，故不能用 CSS 默认的
 * absolute 锚定。
 */
function PolicySelector({
  appId,
  action,
  targetServerId,
  servers,
  subscriptions,
  onPick,
}: {
  appId: string;
  action: RuleAction;
  targetServerId?: string;
  servers: ServerConfig[];
  subscriptions: SubscriptionConfig[];
  onPick: (appId: string, action: RuleAction, targetServerId: string | undefined) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [openGroups, setOpenGroups] = useState<Set<string>>(new Set());
  const scheduleReveal = useRevealAfterCommit();

  const anchorRef = useRef<HTMLSpanElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  const groups = useMemo(
    () => groupServersBySubscription(servers, subscriptions),
    [servers, subscriptions],
  );

  // 两段式：先挂载测尺寸，再算位置，避免闪烁（同 HoverCard.tsx placeCard 手法）。
  useLayoutEffect(() => {
    if (!open || !anchorRef.current || !menuRef.current) return;
    const ar = anchorRef.current.getBoundingClientRect();
    const mr = menuRef.current.getBoundingClientRect();
    setPos({
      left: Math.min(ar.left, window.innerWidth - mr.width - 8),
      top: Math.min(ar.bottom + 4, window.innerHeight - mr.height - 8),
    });
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (!anchorRef.current?.contains(target) && !menuRef.current?.contains(target)) close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close();
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    window.addEventListener('scroll', close, true);
    window.addEventListener('resize', close);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('scroll', close, true);
      window.removeEventListener('resize', close);
    };
  }, [open]);

  const toggleMenu = () => {
    setOpen((v) => {
      const next = !v;
      // 每次开菜单都重算展开集（不是挂载时算一次）：菜单关着的期间该应用的指定节点可能被改掉，
      // 沿用上次的展开集就会展开错组。判据委托 domain 单一真值（三处选择器同一条线）。
      if (next) setOpenGroups(defaultOpenGroupIds(groups, targetServerId));
      return next;
    });
  };

  const pick = (nextAction: RuleAction, nextTarget: string | undefined) => {
    onPick(appId, nextAction, nextTarget);
    setOpen(false);
  };

  const view = appPolicyView(action, targetServerId, servers, t);
  const curKind: RuleAction | 'node' = action === 'proxy' && targetServerId ? 'node' : action;

  return (
    <>
      <span
        ref={anchorRef}
        className={cn('app-pol', view.cls)}
        role="button"
        tabIndex={0}
        onClick={toggleMenu}
      >
        <span className={cn('act-dot', view.dot)} />
        <span>{view.label}</span>
      </span>
      {open && (
        <div
          ref={menuRef}
          className="mini-menu"
          role="menu"
          style={{ position: 'fixed', left: pos?.left ?? -9999, top: pos?.top ?? -9999 }}
        >
          <div className="mm-lbl">{t('appPolicy.policy', '策略')}</div>
          {QUICK_PICKS.map((q) => (
            <button
              key={q.action}
              type="button"
              // `act-block-txt`：动作标签轴的常驻红，按 action 而非按 `danger` 派发 —— `danger` 是
              // 通用破坏性词汇（射程比本轴宽），见 styles/index.css「阻断配色两轴」段。
              className={cn('mi', q.danger && 'danger', q.action === 'block' && 'act-block-txt')}
              role="menuitem"
              onClick={() => pick(q.action, undefined)}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d={q.icon} />
              </svg>
              <span>{t(q.key, q.zh)}</span>
              {curKind === q.action && <CheckMark />}
            </button>
          ))}
          {groups.length > 0 && (
            <>
              <div className="mm-sep" />
              <div className="mm-lbl">{t('appPolicy.summary.node', '指定节点')}</div>
              {groups.map((g) => {
                const groupOpen = openGroups.has(g.id);
                const groupLabel = g.isManual
                  ? t('nodes.tab.manual', '自建节点')
                  : g.isMesh
                    ? t('nodes.tab.mesh', '组网')
                    : g.name;
                return (
                  <Fragment key={g.id}>
                    <button
                      type="button"
                      className="ns-grp"
                      aria-expanded={groupOpen}
                      onClick={(e) => {
                        const header = e.currentTarget;
                        setOpenGroups((prev) => {
                          const next = new Set(prev);
                          if (next.has(g.id)) next.delete(g.id);
                          else next.add(g.id);
                          return next;
                        });
                        scheduleReveal(groupOpen ? null : () => revealSiblingGroup(header));
                      }}
                    >
                      <svg
                        className={cn('ns-chev', groupOpen && 'open')}
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth={2}
                      >
                        <path d="M9 6l6 6-6 6" />
                      </svg>
                      <span>{groupLabel}</span>
                      <span className="ns-c">{g.servers.length}</span>
                    </button>
                    {groupOpen &&
                      g.servers.map((s) => (
                        <button
                          key={s.id}
                          type="button"
                          className="mi ns-node"
                          role="menuitem"
                          onClick={() => pick('proxy', s.id)}
                        >
                          <span>{s.name}</span>
                          {targetServerId === s.id && <CheckMark />}
                        </button>
                      ))}
                  </Fragment>
                );
              })}
            </>
          )}
        </div>
      )}
    </>
  );
}

export default AppPolicyScreen;
