/**
 * RuleDialog —— 自定义规则增删改弹窗（第二复杂表单，原型 #rule-dialog :2722）。
 *
 * 15 类型 / 5 分组（分组走扩展 Csel 的 optgroup，见 Csel.tsx + csel-logic.ts）、
 * 目标出站按节点分组 + 默认折叠（同一套 optgroup，组带 id ⇒ 可折叠；判据见下方 targetGroups）、多条件 AND/OR、
 * 每条件多值 textarea（逗号/换行分隔，splitVals=/[,\n]/）、BYPASS_FAKEIP 条件显隐（FieldSpec `when`）、
 * 进程条件的「从进程选择」嵌套 ProcPickDialog、测试匹配折叠、编辑态 footer-左删除入口。
 *
 * 数据物料（**CONSUME domain/rules.ts，不重写**）：`RULE_TYPES`（15 份描述符 —— 分类 / 显示名 /
 * hint / placeholder / 候选源 / 可测试性的唯一源）/ RULE_CATEGORY_ORDER / BYPASS_FAKEIP_TYPES /
 * ruleConditions。**本文件不得出现任何 `RuleType` 字面量**：一切逐类型差异都从描述符的结构字段
 * （`source.kind` / `source.pool` / `source.addressing` …）读，加第 16 个类型只改那张表。
 * 这条有门守（`domain/rules.test.ts`）。
 *
 * 提交门：**两层**。渲染层先 `validateRule`（决定能不能提交）+ `invalidCondValues`（说清哪个值不对），
 * 省掉一次「填错 → IPC 往返 → 回显」；Rust 侧 `api.rules.add`/`update` 写时再校验一次，仍是权威。
 * RULE_INVALID → 展示校验消息、弹窗不关、让用户改；其它失败 → 通用「保存失败，可重试」。
 *
 * 淬火复用（对齐 NodeDialog）：R1 无 radix/RHF —— 外层 `key={ruleId ?? 'new'}` 重挂 + useState 同步初始化，
 * 无「挂载后 reset」路径；Csel 受控无懒挂 Portal。脏态取消 → 嵌套 ConfirmDialog（复用 D1）。
 */

import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import {
  useAppStore,
  useEffectiveConfig,
  useEffectiveRules,
  useEffectiveServers,
} from '@/store/app-store';
import { api, IpcError } from '@/ipc';
import type {
  Rule,
  RuleType,
  RuleCondition,
  RuleAction,
} from '@/contracts/types';
import {
  RULE_TYPE_IDS,
  RULE_TYPES,
  DEFAULT_RULE_TYPE,
  RULE_CATEGORY_ORDER,
  ruleCategoryLabelKey,
  ruleTypeNameKey,
  ruleTypeHintKey,
  ruleTypePlaceholderKey,
  BYPASS_FAKEIP_TYPES,
  findAddableRuleType,
  isRuleTypePlatformSupported,
  ruleConditions,
  validateRule,
  type RulePreset,
} from '@/domain/rules';
import {
  computeTestMatch,
  geoPoolOptions,
  invalidCondValues,
  matchRuleValueOptions,
  offPoolSelectedOptions,
  processPoolOptions,
  selectedValueSet,
  setCondTypeAt,
  sortRuleValueOptions,
  splitVals,
  toggleCondValueAt,
  type Cond,
  type RuleValueGroup,
  type RuleValueOption,
} from './rule-cond';
import { groupServersBySubscription, defaultOpenGroupIds } from '@/domain/server-grouping';
// 节点行国旗：与首页出口选单 / 托盘同一渲染器 + 同一数据源（跨目录只读引用，同 shared/format 先例）。
import { flagCodeForName } from '@/components/screens/nodes/nd-flag';
import { FlagImg } from '@/components/flag-img';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { useNavStore } from '@/store/nav-store';
import { editRoute } from '@/lib/staged-config';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { useRuleDelete } from '@/lib/use-rule-delete';
import { useScrollBatch } from '@/lib/use-scroll-batch';
import { cn } from '@/lib/utils';
import { Modal } from './Modal';
import { Csel, type CselGroup } from './Csel';
import { missingRuleSetRefs, ruleSetPickState } from './rule-set-pick';
import { useDialogStore } from './dialog-store';
import { FieldRenderer, type FieldSpec, type FormValues } from './field-spec';
import { Fold } from '@/components/Fold';
import { revealOnToggle } from '@/components/reveal';

/**
 * 「目标出站」三个快速策略的行首图标 —— **与首页出口选单 / 应用分流策略菜单同一组图形**
 * （`AppPolicyScreen` 的 `QUICK_PICKS` 与 `NodeMenu` 的直连/阻断行用的就是这三条 path）。
 *
 * 此前这个下拉的选项行**什么图标都没有**，靠 label 里的文本前缀「代理 →」代替 —— 同一件事
 * （「这是一条策略」/「这是一个节点」）在首页画图标、在这里写文字，是三处不一致里最刺眼的一处。
 * `.csel-ico` 那个类不是装饰：prototype 的 `.sel svg{position:absolute}` 会命中 `.sel.csel`
 * 子树里每个 svg，必须靠它复位（规则 + 根因见 styles/index.css 轴 4）。
 */
const TARGET_ICON_PATHS: Record<'proxy' | 'direct' | 'block', string> = {
  proxy: 'M12 5v14M5 12h14',
  direct: 'M4 12h16',
  block: 'M5 5l14 14',
};
function TargetIcon({ kind }: { kind: keyof typeof TARGET_ICON_PATHS }) {
  return (
    <svg className="csel-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d={TARGET_ICON_PATHS[kind]} />
    </svg>
  );
}

function RuleIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
    </svg>
  );
}
function Chevron() {
  return (
    <svg className="rule-test-caret" viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M6 9l6 6 6-6" />
    </svg>
  );
}

/** `<html data-os>`（mac/win/lin）→ domain 层平台判定认的 node 风格值。取不到 → undefined（domain
 * 侧对 undefined 一律判「不支持」，即 fail-closed，与主进程丢弃条件的口径同向，不会造成假可用）。 */
const DATA_OS_TO_NODE: Record<string, NodeJS.Platform> = {
  mac: 'darwin',
  win: 'win32',
  lin: 'linux',
};
function nodePlatformFromDataOs(): NodeJS.Platform | undefined {
  const os = document.documentElement.getAttribute('data-os');
  return os ? DATA_OS_TO_NODE[os] : undefined;
}

interface RuleFormProps {
  base?: Rule;
  isEdit: boolean;
  preset?: RulePreset;
}

/** footer 左侧「删除此规则」的原地二次确认 key（原型 :4095 `rule-del-dlg`）。 */
const RULE_DEL_KEY = 'rule-del-dlg';

/** 分组切换的取值：`all` 是**默认**——它让检索永远跨组生效（分两个 tab 时「搜到了却在另一个 tab」
 *  是一类静默失败）；两个具体值来自描述符的 `GroupAxis:'origin'`。 */
type GroupFilter = 'all' | RuleValueGroup;

/** 稳定的取数引用（放模块级：写成 `() => api.x.y()` 会每次渲染换身份，把惰性 effect 变成轮询）。 */
const listRuleResources = () => api.ruleResources.list();
const listProcesses = () => api.system.listProcesses();

/** 空快照（模块级常量：写成行内 `new Set()` 会每次渲染换身份，把 useMemo 变成每帧重排）。 */
const EMPTY_SNAP: ReadonlySet<string> = new Set<string>();

/**
 * 惰性拉一份候选清单。`enabled` 为真才拉、只拉一次（成功或失败都不再自动重试 —— 来回切类型会
 * 变成隐式重试风暴，同 `AppAddDialog` 的 `galleryStatus` 门）。
 *
 * `failed` 与「拉到了但是空」必须分开：把**加载失败**说成**结果为空**会让用户去改搜索词，
 * 而真正的问题是清单压根没拉到（本仓同题定论见 `rule-set-pick.ts` 的 `RuleSetPickState`）。
 */
function useLazyPool<T>(
  enabled: boolean,
  load: () => Promise<T[]>
): { items: T[] | null; loading: boolean; failed: boolean } {
  const [items, setItems] = useState<T[] | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (!enabled || items !== null) return;
    let alive = true;
    void load().then(
      (list) => alive && setItems(Array.isArray(list) ? list : []),
      () => {
        if (!alive) return;
        setFailed(true);
        setItems([]);
      }
    );
    return () => {
      alive = false;
    };
  }, [enabled, items, load]);
  return { items, loading: enabled && items === null, failed };
}

/**
 * 候选勾选区 —— 一个类型一个实例。
 *
 * 独立组件而不是内联 JSX：分批渲染要 `useScrollBatch`，而条件数是变的（0–5 个池条件），
 * 在 `conds.map` 里调 hook 会违反 hooks 的调用序稳定性。
 *
 * chip 形态复用 `.tagchip`（AppAddDialog 的标签勾选区同款）。它的浅色分离度 2026-07-30 实测过：
 * ΔL*=5.29 / 4.61:1，落在「已接受」档，故直接复用不另立视觉（结论与其载荷记在
 * `styles/index.css` 的「C) chip 族的浅色实测」段，有门守）。
 */
function RuleValuePick({
  options,
  batch,
  resetKey,
  selected,
  onToggle,
  emptyText,
  moreText,
  ariaLabel,
}: {
  options: readonly RuleValueOption[];
  /** 描述符的 `scale === 'large'`：只有大池才分批（小池一次渲完，省一层滚动监听）。 */
  batch: boolean;
  /** 结果集身份（检索词 + 分组）—— 一变即回首批。 */
  resetKey: string;
  selected: ReadonlySet<string>;
  onToggle: (value: string) => void;
  emptyText: string;
  moreText: (shown: number, total: number) => string;
  ariaLabel: string;
}) {
  const { count, onScroll } = useScrollBatch(options.length, resetKey);
  const shown = batch && options.length > count ? options.slice(0, count) : options;
  return (
    <div className="rv-pick" role="group" aria-label={ariaLabel} onScroll={batch ? onScroll : undefined}>
      {options.length === 0 ? (
        <div className="card-sub rv-note">{emptyText}</div>
      ) : (
        <>
          {shown.map((o) => {
            const on = selected.has(o.value.toLowerCase());
            return (
              <button
                key={o.value}
                type="button"
                /* `off-pool` = 已选但候选池里没有（手填 / 本地还没下载 / 进程没在跑）。虚线描边 +
                   warn 色边把它与「池里的正常候选」显式分开，`data-tip` 说清是哪一种。 */
                className={cn('tagchip', on && 'on', o.offPool && 'off-pool')}
                aria-pressed={on}
                data-tip={o.hint}
                onClick={() => onToggle(o.value)}
              >
                {o.label}
              </button>
            );
          })}
          {options.length > shown.length && (
            <div className="card-sub rv-note">{moreText(shown.length, options.length)}</div>
          )}
        </>
      )}
    </div>
  );
}

function RuleForm({ base, isEdit, preset }: RuleFormProps) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  /** 「前往规则资源」用（同 AppAddDialog:78,82 的既有做法：跳屏 + 收掉整条弹窗栈）。 */
  const closeAll = useDialogStore((s) => s.closeAll);
  const navigate = useNavStore((s) => s.navigate);
  /* 删除走原地二次点击；`requestClose` 的「放弃更改？」保留弹窗 —— 后者不是破坏性操作确认，
     原型里根本没有对应形态（`destructive-confirm-wiring.test.ts` T3 头注登记为「实现单方面新增、
     方向更好」），且它要在**关窗动作发生前**打断，按钮上没有可武装的落点。 */
  const { armed, confirmTwice } = useConfirmTwice();
  // 展示面：规则目标下拉的节点枚举（选中只写进本条规则，不触发任何按 id 查盘的后端调用）。
  const servers = useEffectiveServers();
  /** 目标下拉的分组名来源（订阅名）。展示面：暂存中新增/改名的订阅要立刻反映到组头上。 */
  const subscriptions = useEffectiveConfig((c) => c?.subscriptions);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  /** 删除腿：与规则列表的行内垃圾桶**同一个** hook（暂存分流/撤销/直落盘三条腿都在那里）。 */
  const deleteRule = useRuleDelete();
  // 平台判定用的值：AppShell 已把权威平台（tauri-plugin-os）落在 <html data-os> 上，这里读它即可，
  // 不再重复嗅探。**必须映射**：data-os 是 UI 分区用的短名（mac/win/lin），而 domain 层的
  // isSourceDeviceMatchSupported 认的是 node 风格（darwin/win32/linux）——直传短名会让 macOS 被
  // 误判成「不支持设备匹配规则」，比不过滤更糟（把本来能用的功能藏掉）。
  const nodePlatform = useMemo(() => nodePlatformFromDataOs(), []);

  // 同步初始化（R1）：编辑态从 base 预填，入口 preset 显式携带类型和值，新建默认单条件。
  const [conds, setConds] = useState<Cond[]>(() => {
    if (base) {
      const cs = ruleConditions(base).map((c) => ({ t: c.type, v: c.values.join(', ') }));
      return cs.length ? cs : [{ t: DEFAULT_RULE_TYPE, v: '' }];
    }
    return [{ t: preset?.type ?? DEFAULT_RULE_TYPE, v: preset?.value ?? '' }];
  });
  // 默认 **or**：`combineMode` 缺省的权威语义就是 or —— 契约 `contracts/types/rules.ts:59`
  // 「'or'(默认，命中任一)」、Rust 生成端 `config-engine/builder/custom_rule_files.rs:273`
  // `rule.combine_mode.unwrap_or(CombineMode::Or)`、hover 卡 `RuleHoverCard.tsx:40`
  // `rule.combineMode ?? 'or'` 三处一致。此前本表单独自默认 'and'，于是**新建的多条件规则**会被写成
  // and，而单条件规则（不写 combineMode）在 hover 卡上被标「满足任一」—— 表单说 AND、卡片说 OR，
  // 同一条规则两种说法。统一到 or 后，表单默认与「不写该字段时的实际行为」对齐。
  const [logic, setLogic] = useState<'and' | 'or'>(base?.combineMode === 'and' ? 'and' : 'or');
  const [target, setTarget] = useState<string>(() => {
    if (!base) return 'proxy';
    if (base.action === 'direct') return 'direct';
    if (base.action === 'block') return 'block';
    return base.targetServerId ? `node:${base.targetServerId}` : 'proxy';
  });
  const [name, setName] = useState(base?.remarks ?? preset?.value ?? '');
  const [bypassFakeIP, setBypassFakeIP] = useState(base?.bypassFakeIP === true);
  const [test, setTest] = useState('');

  const [dirty, setDirty] = useState(false);
  /** 名称必填的提交后校验态（口径同 SubDialog:85 / AppAddDialog:65：提交才亮，输入即灭）。 */
  const [errName, setErrName] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const touch = () => setDirty(true);

  // 分组类型选项（15×5，唯一映射源 = RULE_TYPE_CATEGORY）；禁用「已被其它条件占用」的类型（类型唯一）
  // 或「当前平台内核不支持」的类型（device 类 = source_mac/source_hostname，仅 Linux/macOS）。
  // 不做平台过滤的后果不是显示问题而是**静默失效**：Windows 用户能选中并保存成功，但生成 sing-box
  // 配置时 custom_rules.rs 会把整条条件丢掉且不报错 —— 规则实际行为与 UI 所示不符。
  // `id === currentType` 豁免：在 macOS/Linux 建的规则拿到 Windows 上打开，仍要能看到它当前的类型。
  const typeGroups = (currentType: RuleType): CselGroup[] => {
    const used = new Set(conds.map((c) => c.t));
    return RULE_CATEGORY_ORDER.map((cat) => ({
      label: t(ruleCategoryLabelKey(cat)),
      options: RULE_TYPE_IDS.filter((id) => RULE_TYPES[id].category === cat).map((id) => ({
        value: id,
        label: t(ruleTypeNameKey(id)),
        disabled:
          id !== currentType && (used.has(id) || !isRuleTypePlatformSupported(id, nodePlatform)),
      })),
    }));
  };

  /** 「哪些池要拉」由**描述符**说了算（不点名类型）：出现该池的条件才拉，绝大多数规则一次 IPC 都不加。 */
  const usesPool = (pool: 'geoTag' | 'process') =>
    conds.some((c) => {
      const s = RULE_TYPES[c.t].source;
      return s.kind === 'pool' && s.pool === pool;
    });
  /**
   * 已下载 / 内置的规则资源清单 —— geoTag 池（规则集 / geosite / geoip 三个类型共用）的候选源。
   * 失败落空数组 = 勾选区只剩一行说明，手填腿仍在（`allowFreeInput`），不把整个条件类型堵死。
   */
  const geoPool = useLazyPool(usesPool('geoTag'), listRuleResources);
  const resItems = geoPool.items;
  /** 在跑的进程清单 —— process 池（进程名 / 进程路径）的候选源。 */
  const procPool = useLazyPool(usesPool('process'), listProcesses);
  /** 池 → 它的加载态（描述符只说「用哪个池」，两个池的取数各自惰性）。 */
  const poolPhase = (pool: 'geoTag' | 'process') => (pool === 'geoTag' ? geoPool : procPool);

  /**
   * 每个池条件自己的检索词与分组选择。**按类型键存**而不是按下标：规则里类型**唯一**
   * （`used` 集合强制），故类型是稳定的身份 —— 按下标存会在删掉中间某个条件时把检索词错位挪给邻居。
   */
  const [poolQuery, setPoolQuery] = useState<Partial<Record<RuleType, string>>>({});
  const [poolGroup, setPoolGroup] = useState<Partial<Record<RuleType, GroupFilter>>>({});
  /** 「只看已选」开关（第二行右侧那颗 `已选 N` chip）。同上按类型键存。 */
  const [poolOnlySel, setPoolOnlySel] = useState<Partial<Record<RuleType, boolean>>>({});

  /**
   * 候选排序用的「已选**快照**」——「已选优先」与「编辑过程不动排序」是配套的一条需求
   * （陈先生 2026-07-30），判据全文见 `sortRuleValueOptions` 头注。
   *
   * **只有两个重建时机**：① 打开弹窗（本初始化器，R1 下 `key` 重挂 = 重新初始化）；
   * ② 切换条件类型（`setCondType` —— 那时值被清空，快照跟着归零）。
   * 勾 / 取消勾**绝不重建** —— 一重建排序就成了实时的，症状是「勾一个跳一个」，
   * 而那正是本设计要避免的东西。这条有门守（`rule-cond.test.ts` 的接线组）。
   */
  const [poolSnap, setPoolSnap] = useState<Partial<Record<RuleType, ReadonlySet<string>>>>(
    () =>
      Object.fromEntries(conds.map((c) => [c.t, selectedValueSet(c.v)])) as Partial<
        Record<RuleType, ReadonlySet<string>>
      >,
  );

  /**
   * 候选面投影 —— 按**类型**而非按条件算并缓存：2000+ 条的 ruleSet 池不该在用户每敲一个字符
   * （`conds` 变）时重投一次。故依赖只取「出现了哪些类型」+ 两个池的数据。
   */
  const poolTypesKey = conds.map((c) => c.t).join('|');
  const poolOptions = useMemo(() => {
    const m = new Map<RuleType, RuleValueOption[]>();
    for (const tp of poolTypesKey.split('|').filter(Boolean) as RuleType[]) {
      const s = RULE_TYPES[tp]?.source;
      if (!s || s.kind !== 'pool' || m.has(tp)) continue;
      const raw =
        s.pool === 'geoTag'
          ? geoPoolOptions(tp, geoPool.items ?? [])
          : processPoolOptions(tp, procPool.items ?? []);
      /* 排序键取**快照**，不取实时勾选态 —— 依赖数组里因此没有 `conds`，只有 `poolSnap`
         （它一轮编辑里只在切类型时变一次）。这是本设计最容易退化的一处。 */
      m.set(tp, sortRuleValueOptions(raw, poolSnap[tp] ?? EMPTY_SNAP));
    }
    return m;
  }, [poolTypesKey, geoPool.items, procPool.items, poolSnap]);

  /**
   * 本条件引用了、但本地不可用的规则集（判据在 `rule-set-pick.ts`，与规则列表角标同一条线）。
   *
   * 清单未到位时恒空，**两种情形都要挡**：
   *  - `null` = 惰性拉取还在飞（有真实空窗期，见上方 useEffect）；
   *  - `[]` = 拉取**失败**。成功的 `rule_resources_list` 恒不为空 —— 它无条件把随包表
   *    （`builtin_geo_rulesets()`）逐条投影进结果，故空数组只可能来自上面那条 catch 腿。
   * 两种情形下 available 集合都是空的，不挡就会把每一条已有引用都报成「缺失」= 假告警。
   */
  const ruleSetMissing = (currentVal: string): string[] =>
    resItems && resItems.length > 0 ? missingRuleSetRefs(splitVals(currentVal), resItems) : [];

  /**
   * 「一条都挑不出来」时那一句话 —— **三态各说各的，绝不混用**。
   *
   * 把**加载失败**说成**结果为空**是谎报，而且比一般谎报更坏：用户会去改搜索词，而真正的问题是
   * 清单压根没拉到。同题定论见 `ResCatalogDialog.extStatusText`（特意把 `preload` 与 `cache` 分开，
   * 就为了「任何一态都不谎称『已从远程获取』」）。
   *
   * 同一个字符串同时喂给**勾选区里那一行**与**「前往规则资源」提示行**，于是两处不可能各说一套。
   * 挑得出来时取空串：那时压根不显示这一句。
   */
  const poolEmptyText = (loading: boolean, failed: boolean, matched: number): string =>
    loading
      ? t('common.loading')
      : failed
        ? t('rules.candidatesFailed')
        : matched === 0
          ? t('common.noResults')
          : '';

  /** 类型切换 —— 判据与「为什么一律清空」在 `rule-cond.ts` 的 `setCondTypeAt` 头注。 */
  const setCondType = (i: number, tp: RuleType) => {
    setConds((prev) => setCondTypeAt(prev, i, tp));
    // 快照的第二个、也是**最后一个**重建时机：切类型 ⇒ 值被清空（`setCondTypeAt`）⇒ 快照归零。
    // 这一句以外不得有第二处 `setPoolSnap`，否则排序就退化成实时的（有门守）。
    setPoolSnap((prev) => ({ ...prev, [tp]: EMPTY_SNAP }));
    touch();
  };
  /** 勾选 / 取消一个候选值（与手填文本区共用同一份 `c.v` —— 结构上不可能与勾选态失同步）。 */
  const toggleCondValue = (i: number, value: string) => {
    setConds((prev) => toggleCondValueAt(prev, i, value));
    touch();
  };
  const setCondVal = (i: number, v: string) => {
    setConds((prev) => prev.map((c, idx) => (idx === i ? { ...c, v } : c)));
    touch();
  };
  const addCond = () => {
    const used = new Set(conds.map((c) => c.t));
    // findAddableRuleType = 「未占用 ∧ 本平台支持」，与下方按钮显隐同一口径（domain/rules.ts 的文档
    // 明写二者必须共用它，防「按钮显示但点了没结果」）。
    const next = findAddableRuleType(used, nodePlatform);
    if (!next) return;
    setConds((prev) => [...prev, { t: next, v: '' }]);
    touch();
  };
  const removeCond = (i: number) => {
    setConds((prev) => (prev.length > 1 ? prev.filter((_, idx) => idx !== i) : prev));
    touch();
  };

  const bypassEligible = conds.some((c) => BYPASS_FAKEIP_TYPES.includes(c.t));
  const bypassSpec: FieldSpec = {
    t: 'switch',
    k: 'bypassFakeIP',
    label: 'rules.bypassFakeIP',
    hint: 'rules.bypassFakeIPHint',
    when: (vals) => vals._bypassEligible === true,
  };
  const bypassValues: FormValues = { bypassFakeIP, _bypassEligible: bypassEligible };

  /**
   * 目标出站下拉 —— **按订阅/分组折叠**（与应用分流的策略菜单、托盘「全部节点」同一套语义）。
   *
   * 此前是一条平铺列表：节点一多（机场订阅动辄几十上百）就得在几百行里滚着找，且直连/阻断被顶到
   * 列表最末端 —— 那两个是常用项，却因为夹在节点后面而最难够到。改成分组后：
   *  - 三个快速策略单独一组、**不带 id ⇒ 不可折叠恒展开**（主路径不能被折进去）；
   *  - 每个节点分组（自建/组网/各订阅）一组，带 id ⇒ 可折叠，默认全折叠，
   *    只展开含当前已选节点的那一组（`defaultOpenGroupIds`，三处选择器共用的单一判据）。
   *
   * 节点项文案保留 `代理 → <名称>` 前缀不动：触发器显示的就是被选项的 label，剥掉前缀会让
   * 收起态从「代理 → 香港01」变成裸节点名，丢掉「这是代理到某节点」这层语义。
   */
  const nodeGroups = useMemo(
    () => groupServersBySubscription(servers, subscriptions ?? []),
    [servers, subscriptions],
  );
  /** 当前已选的节点 id（`node:` 前缀是本下拉的值编码，非节点项时为 undefined ⇒ 全折叠）。 */
  const targetNodeId = target.startsWith('node:') ? target.slice(5) : undefined;
  const targetGroups: CselGroup[] = useMemo(
    () => [
      {
        // 复用应用分流菜单那句「策略」而不是新起一个 i18n 键：两处是同一概念，且新增键会动
        // locale-parity 门的债务基线（5 个语言文件都得补），不在本次改动的范围里。
        label: t('appPolicy.policy'),
        options: [
          {
            value: 'proxy',
            label: t('rules.targetDefaultProxy'),
            icon: <TargetIcon kind="proxy" />,
          },
          { value: 'direct', label: t('rules.targetDirect'), icon: <TargetIcon kind="direct" /> },
          {
            value: 'block',
            label: t('rules.targetBlock'),
            icon: <TargetIcon kind="block" />,
            // 动作标签轴：与 `.act-block` pill / `.mi.danger` / `.tray-i.danger` 同色同轴。
            // 走 `Csel` 的 `danger` 通道而不是在本页刷一层红 —— 根因是这个字段此前不存在。
            danger: true,
          },
        ],
      },
      ...nodeGroups.map((g) => ({
        id: g.id,
        // 自建/组网组的 `name` 是占位符，按 isManual/isMesh 本地化（ServerGroup 契约的明文要求）。
        label: g.isManual
          ? t('nodes.tab.manual')
          : g.isMesh
            ? t('nodes.tab.mesh')
            : g.name,
        options: g.servers.map((s) => ({
          value: `node:${s.id}`,
          label: `${t('rules.targetProxyTo')} ${s.name}`,
          // 国旗：与首页出口选单 / 托盘节点行**同一渲染器 + 同一数据源**（名称派生 `flagCodeForName`，
          // 语义 =「这个节点自称在哪」）。识别不到 → FlagImg 返回 null，什么都不画（不回退地球）。
          // 延迟色点刻意**不加**：本弹窗不订阅测速 store，也不该订阅 —— 「目标出站」回答的是
          // 「把流量路由到哪」，在这里画延迟点是给出一条与该决策无关的判据。
          icon: <FlagImg code={flagCodeForName(s.name)} />,
        })),
      })),
    ],
    [nodeGroups, t],
  );
  const targetOpenGroups = useMemo(
    () => defaultOpenGroupIds(nodeGroups, targetNodeId),
    [nodeGroups, targetNodeId],
  );

  const testResult = useMemo(() => computeTestMatch(conds, logic, test), [conds, logic, test]);

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('rules.discardTitle'),
        message: t('rules.discardMsg'),
        confirmLabel: t('rules.discard'),
        danger: true,
        onConfirm: () => {
          close(); // pop confirm
          close(); // pop 本弹窗
        },
      },
    });
  };

  /**
   * 删除本条规则 —— 原地二次点击（原型 :4095 `rule-del-dlg`），不再叠一层弹窗。
   *
   * 三条腿（撤销条目 / 暂存删除条目 / 直落盘）全在 `useRuleDelete` 里，与列表行内那颗垃圾桶共用；
   * 本处只留两件弹窗自己的事：成功即关窗、失败发一条右下角 toast（不关窗，让用户看得见原因）。
   */
  const requestDelete = () => {
    if (!base) return;
    confirmTwice(RULE_DEL_KEY, () => {
      void (async () => {
        try {
          await deleteRule(base);
          close();
        } catch (e) {
          toast.error(t('common.saveFailed'), e instanceof Error ? e.message : String(e));
        }
      })();
    });
  };

  const handleSubmit = async () => {
    // 名称必填：与 SubDialog / NodeDialog / WarpDialog / WgDialog / AppAddDialog 同一口径（errName +
    // .err-line），此前本表单是全仓唯一放行空名的 —— 空 remarks 会让规则列表的标题回落成裸类型名
    // （`ruleTitle()`：无 remarks 就显 `domain` / `ruleSet`），多条同类型规则在列表和 hover 卡上
    // 完全无法区分，而排序又直接决定命中优先级。
    const nameEmpty = !name.trim();
    setErrName(nameEmpty);
    const filled = conds.filter((c) => splitVals(c.v).length);
    if (!filled.length) {
      toast.error(t('rules.invalidHead'), t('rules.errNoCond'));
      // 名称也空时两条错误一起显示（不让用户改完一个再发现另一个）。
      return;
    }
    if (nameEmpty) return;
    const rconds: RuleCondition[] = filled.map((c) => ({ type: c.t, values: splitVals(c.v) }));
    const multi = rconds.length > 1;

    /* 渲染端校验层 —— 此前**从未接上**：`validateRuleValue`（15/15 全覆盖）与 `validateRule`
       两个函数生产调用点为零，提交只校验「名称非空」+「至少一个条件有值」，值本身全靠后端返
       `RULE_INVALID` 再回显。一次 IPC 往返之后才知道自己那行 `10.0.0.0/40` 不合法，而这些值
       落进 endpoints[]/route.rules[] 时启动前的 gate 按 outbounds 索引剪不掉 → 直接 FATAL。

       **`validateRule` 决定能不能提交，`invalidCondValues` 负责说清哪一个值不对**，不是两道
       重复的门：前者还看 `combineMode` 与镜像 `type`（逐值校验看不到的两项），后者才拿得出
       用户能照着改的信息。后端仍是权威（Rust 写时再校验一次），这层只是把往返省掉。 */
    const draft = {
      type: rconds[0].type,
      values: rconds[0].values,
      conditions: multi ? rconds : undefined,
      combineMode: multi ? logic : undefined,
    };
    if (!validateRule(draft)) {
      const bad = invalidCondValues(filled);
      toast.error(
        t('rules.invalidHead'),
        bad.length > 0
          ? t('rules.errInvalidValues', {
              detail: bad
                .slice(0, 4)
                .map((b) => `${t(ruleTypeNameKey(b.type))}: ${b.value}`)
                .join('; ') + (bad.length > 4 ? '…' : ''),
            })
          : t('rules.errInvalidRule'),
      );
      return;
    }
    const action: RuleAction = target === 'direct' ? 'direct' : target === 'block' ? 'block' : 'proxy';
    const targetServerId = action === 'proxy' && target.startsWith('node:') ? target.slice(5) : undefined;
    const bypass = bypassEligible && bypassFakeIP ? true : undefined;
    const remarks = name.trim();

    setSubmitting(true);
    try {
      if (isEdit && base) {
        // base 起底保全非模型字段（tlsSpoof 等，R5）；单条件时显式清 conditions/combineMode。
        const full: Rule = {
          ...base,
          id: base.id,
          type: rconds[0].type,
          values: rconds[0].values,
          conditions: multi ? rconds : undefined,
          combineMode: multi ? logic : undefined,
          action,
          targetServerId,
          enabled: base.enabled,
          bypassFakeIP: bypass,
          remarks,
        };
        // 配置暂存闸门（与 NodeDialog 同形）：`customRules` Class B，提交的是完整 Rule ⇒ 天然满足
        // 重放要求的「幂等整体替换」。
        if (editRoute('customRules', stagingEnabled) === 'staged') {
          stage({
            id: `rule:${full.id}`,
            kind: 'rule',
            label: `${t('rules.editTitle')} ${remarks}`,
            entityPath: ['customRules', full.id],
            nextValue: full,
          });
          close();
          return; // 零 IPC 写、零磁盘写（FR-1）
        }
        await api.rules.update(full);
      } else {
        const rest: Omit<Rule, 'id'> = {
          type: rconds[0].type,
          values: rconds[0].values,
          conditions: multi ? rconds : undefined,
          combineMode: multi ? logic : undefined,
          action,
          enabled: true,
          targetServerId,
          bypassFakeIP: bypass,
          remarks,
        };
        // 同上。新增时前端自铸 id：后端 `rules_add` 只在落盘那一刻发 id，而条目现在就需要稳定的
        // 实体寻址键（同一条规则改两次要覆盖同一条条目，否则计数虚高）。
        if (editRoute('customRules', stagingEnabled) === 'staged') {
          const entityId = crypto.randomUUID();
          stage({
            id: `rule:${entityId}`,
            kind: 'rule',
            label: `${t('rules.newTitle')} ${remarks}`,
            entityPath: ['customRules', entityId],
            nextValue: { ...rest, id: entityId },
          });
          close();
          return; // 零 IPC 写、零磁盘写（FR-1）
        }
        await api.rules.add(rest);
      }
      void loadConfig(true); // 同上：不刷则列表看不到新增/编辑结果
      close();
    } catch (e) {
      // 写时校验（Rust 权威）：RULE_INVALID → 展示校验消息、弹窗不关；其它 → 通用可重试失败。
      if (e instanceof IpcError && e.code === 'RULE_INVALID') {
        toast.error(t('rules.invalidHead'), e.message);
      } else {
        toast.error(t('common.saveFailed'), e instanceof Error ? e.message : String(e));
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      titleId="rule-dlg-title"
      title={isEdit ? t('rules.editTitle') : t('rules.newTitle')}
      onClose={requestClose}
      icon={<RuleIcon />}
      footer={
        <>
          {isEdit && (
            <button
              type="button"
              className={cn('btn ghost', armed === RULE_DEL_KEY && 'confirming')}
              style={{ marginRight: 'auto', color: 'hsl(var(--err))', borderColor: 'hsl(var(--err)/0.3)' }}
              onClick={requestDelete}
            >
              {armed === RULE_DEL_KEY
                ? t('rules.deleteConfirmAgain')
                : t('rules.delete')}
            </button>
          )}
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void handleSubmit()}
            disabled={submitting}
          >
            {isEdit ? t('common.save') : t('rules.add')}
          </button>
        </>
      }
    >
      {/* 规则名称（remarks，必填） */}
      <div className="fld">
        <label className="fld-l" htmlFor="rule-name">
          <span>{t('rules.name')}</span> <span className="req-star">*</span>
        </label>
        <input
          id="rule-name"
          className="input"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setErrName(false);
            touch();
          }}
          placeholder={t('rules.namePh')}
        />
        {errName && <div className="err-line">{t('rules.errName')}</div>}
      </div>

      {/* 匹配条件（多条件 AND/OR） */}
      <div className="fld">
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
          <label className="fld-l" style={{ margin: 0 }}>
            {t('rules.conditions')}
          </label>
          {conds.length >= 2 && (
            <div className="logic-toggle" role="group" aria-label={t('rules.combineMode')}>
              {/* 裸 AND/OR 对非技术用户无信息量（且与 hover 卡上的中文「全部满足 / 满足任一」是
                  两套说法）。改用与 hover 卡**同一组 i18n key**（rules.combineAnd / combineOr），
                  弹窗与卡片再不会各说各话。 */}
              <button
                type="button"
                className={logic === 'and' ? 'on' : ''}
                onClick={() => {
                  setLogic('and');
                  touch();
                }}
              >
                {t('rules.combineAnd')}
              </button>
              <button
                type="button"
                className={logic === 'or' ? 'on' : ''}
                onClick={() => {
                  setLogic('or');
                  touch();
                }}
              >
                {t('rules.combineOr')}
              </button>
            </div>
          )}
        </div>

        {conds.map((c, i) => {
          const desc = RULE_TYPES[c.t];
          const src = desc.source;
          /** 该条件是否走 `res:<id>` 寻址（= 规则集）—— 描述符字段，不点名类型。 */
          const isResRef = src.kind === 'pool' && src.addressing === 'res-id';
          const query = poolQuery[c.t] ?? '';
          const group = poolGroup[c.t] ?? 'all';
          /* 候选面：`kind==='free'` ⇒ 恒 null，右侧**不渲染任何控件**（渲染一个禁用的搜索框
             是假控件：它宣称「这里可以搜」，而这个类型压根没有候选源）。 */
          const all = src.kind === 'pool' ? (poolOptions.get(c.t) ?? []) : null;
          const matched = all ? matchRuleValueOptions(all, query) : null;
          /* 分组切换只在「描述符声明了轴」且「两组都真有东西」时出现：外置一条没下载时给个空 tab
             是让用户去点一个必然空的东西。 */
          const groupsPresent =
            src.kind === 'pool' && src.groupBy !== null && all
              ? (['builtin', 'external'] as const).filter((g) => all.some((o) => o.group === g))
              : [];
          const selected = selectedValueSet(c.v);
          const onlySel = poolOnlySel[c.t] === true;
          const poolLoading = src.kind === 'pool' && poolPhase(src.pool).loading;
          const poolFailed = src.kind === 'pool' && poolPhase(src.pool).failed;
          /* 「已选但候选池里没有」的值 —— 手填的 / 引用了本地还没下载的 tag / 给未运行的应用建的
             进程规则。文本框折叠之后它们若不在这里露面就**看不见也删不掉**（判据见
             `offPoolSelectedOptions` 头注）。

             **加载中一律不判**（同 `ruleSetMissing` 的既有口径）：那一刻 `all` 是空的，不挡就会
             把这条规则里**每一个**已选值都标成「本地暂无」，等清单到了再全部翻回去 —— 一次
             秒级的、内容完全相反的闪烁，比晚半秒露面糟得多。
             **加载失败则必须露面**：清单永远不会来了，此刻它们是唯一入口；但提示词换成
             「候选清单加载失败」—— 三态各说各的，把加载失败说成「本地暂无」会让用户跑去
             下载一个本来就在本地的东西（同题定论见 `poolEmptyText`）。
             正常态的提示词按**池**分（描述符字段，不点名类型）：geo 池 = 本地暂无；
             进程池 = 那个进程当前没在跑。 */
          const offHint = poolFailed
            ? t('rules.candidatesFailed')
            : src.kind === 'pool' && src.pool === 'process'
              ? t('rules.candidateNotRunning')
              : t('rules.candidateNotLocal');
          const offPool = all && !poolLoading ? offPoolSelectedOptions(c.v, all, offHint) : null;
          const grouped =
            matched && groupsPresent.length > 1 && group !== 'all'
              ? matched.filter((o) => o.group === group)
              : matched;
          const poolShown =
            grouped && onlySel ? grouped.filter((o) => selected.has(o.value.toLowerCase())) : grouped;
          /* 池外已选**恒排最前、且不受分类切换影响** —— 它们既不属内置也不属外置，按来源把一个
             压根没有来源的值筛掉，等于把刚露出来的值又藏了一次。检索词仍作用于它们（用户主动
             缩小范围时不该有豁免项）；「只看已选」对它们天然是恒真。 */
          const shownOpts =
            offPool && poolShown ? [...matchRuleValueOptions(offPool, query), ...poolShown] : poolShown;
          const emptyText = poolEmptyText(poolLoading, poolFailed, shownOpts?.length ?? 0);
          /* 「一条都挑不出来」与缺失引用**各自对应一个真实时刻**（陈先生 2026-07-30 裁定：
             两个都要，不互斥）。三态文案各不相同，但「前往规则资源」三态都给 —— 都是有效出路。
             `ruleSetPickState` 与勾选区共用同一份 name+id 过滤口径（`geoPoolOptions` 的
             `searchFields:['name','id']`），故「提示行说挑不出来」⟺「勾选区真的空」。 */
          const rsMissing = isResRef ? ruleSetMissing(c.v) : [];
          const rsEmpty = isResRef && ruleSetPickState(resItems, query) !== 'ok';
          /* 手填腿。有候选区时**默认折叠**（原型 `.fld-fold`，与传输层/detour/订阅高级同一形态）。 */
          const valInput = (
            <textarea
              className={cn('input mono cond-val-input', all && 'compact')}
              rows={all ? 2 : 4}
              value={c.v}
              onChange={(e) => setCondVal(i, e.target.value)}
              placeholder={t(ruleTypePlaceholderKey(c.t))}
              aria-label={t('rules.condValues')}
            />
          );
          return (
            <div className="cond-row" key={i}>
              <div className="cond-fields">
                {/* 条件行头部：[类型 170px][搜索框 flex:1] 同排，[分类切换] 独占第二行
                    （`kind==='free'` ⇒ 右侧留空，**不渲染禁用的假控件** —— 一个禁用的搜索框在宣称
                    「这里可以搜」，而这个类型压根没有候选源）。为什么分类切换不挤在同一排：
                    206px 的余量塞不下三件套，而换行时先掉下去的恰是检索框（见 styles/index.css）。
                    `.cond-row` 的 grid 不动，右列仍是 `.cond-del`。 */}
                <div className="cond-head">
                  <Csel
                    ariaLabel={t('rules.matchType')}
                    value={c.t}
                    onChange={(v) => setCondType(i, v as RuleType)}
                    options={typeGroups(c.t)}
                  />
                  {all && (
                    <label className="input search-box">
                      <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
                        <circle cx="11" cy="11" r="7" />
                        <path d="M20 20l-3-3" />
                      </svg>
                      <input
                        type="search"
                        value={query}
                        onChange={(e) => setPoolQuery((prev) => ({ ...prev, [c.t]: e.target.value }))}
                        placeholder={t('common.search')}
                        aria-label={t('common.search')}
                      />
                    </label>
                  )}
                  {/* 第二行 = `[内置 | 外置 seg2]` + `[已选 N]` chip。geo 池两个都有，进程池只有后者
                      （它无 `groupBy`，本来没这一行）。
                      为什么筛选做成 chip 而不是「已选/未选/全部」三档 seg2：这一行已被分类切换占，
                      再塞一组约 120px 的 seg2 在 384px 的 `.cond-fields` 里会挤爆（弹窗不加宽是既有
                      裁定）；且「未选」那一档几乎无场景 —— 已选已被排序顶到最前，其余就是未选。
                      chip 把计数与开关二合一，约 60px。取消勾选后 N 会变，但**列表不重排**（排序键
                      是快照）。
                      分类切换本身：描述符声明了轴、且**两组都真有东西**才出现（外置一条没下载时给个
                      空 tab，是让用户去点一个必然空的东西）。`全部` 是默认档 —— 分两个 tab 时
                      「搜到了却在另一个 tab」是一类静默失败，默认跨组就没有这回事。 */}
                  {all && (
                    <div className="cond-grp-row">
                      {groupsPresent.length > 1 && (
                        <div className="seg2 cond-grp" role="group" aria-label={t('rules.candidateGroup')}>
                          {(['all', ...groupsPresent] as GroupFilter[]).map((g) => (
                            <button
                              key={g}
                              type="button"
                              className={cn(group === g && 'on')}
                              onClick={() => setPoolGroup((prev) => ({ ...prev, [c.t]: g }))}
                            >
                              {g === 'all'
                                ? t('common.all')
                                : g === 'builtin'
                                  ? t('resCatalog.builtin')
                                  : t('resCatalog.external')}
                            </button>
                          ))}
                        </div>
                      )}
                      <button
                        type="button"
                        className={cn('ap-chip cond-sel-chip', onlySel && 'on')}
                        aria-pressed={onlySel}
                        onClick={() => setPoolOnlySel((prev) => ({ ...prev, [c.t]: !onlySel }))}
                      >
                        {t('rules.candidateSelected', { n: selected.size })}
                      </button>
                    </div>
                  )}
                </div>
                {/* 逐类型填写提示 —— locale 里那张 `rules.typeHints.*` 表（五语齐全）此前**零消费点**，
                    15 条提示一条都没显示过。放在勾选区之上：geosite/geoip 那两句写着「或从下方候选中
                    勾选」，指的就是紧接其下的那块勾选区（旧词是 上游 遗留的「常用标签」，引用了一个
                    当时不存在的控件，本批随控件落地一并改词，五语同改）。 */}
                <div className="card-sub">{t(ruleTypeHintKey(c.t))}</div>
                {/* 勾选区。**与下面的文本区并存**，不是二选一（`allowFreeInput`：候选面只列本地已有
                    / 当前在跑的，三条不同的理由见描述符注释）。两者共用同一份 `c.v` ⇒ 勾选态与
                    文本在结构上不可能失同步。自带 max-height + 滚动：不给的话 2000 条规则集会把
                    弹窗那唯一的滚动容器撑成几十屏。 */}
                {shownOpts && (
                  <RuleValuePick
                    /* 按**类型**取 key（类型在一条规则里唯一）：`.cond-row` 用的是下标 key，
                       删掉中间某条后下标会平移，分批计数会跟着串到别的条件上。 */
                    key={c.t}
                    options={shownOpts}
                    batch={src.kind === 'pool' && src.scale === 'large'}
                    resetKey={`${query}|${group}|${onlySel}`}
                    selected={selected}
                    onToggle={(v) => toggleCondValue(i, v)}
                    emptyText={emptyText}
                    moreText={(shown, total) =>
                      t('appAdd.galleryMore', {
                        shown,
                        total,
                      })
                    }
                    ariaLabel={t(ruleTypeNameKey(c.t))}
                  />
                )}
                {/* 原型 .cond-fields > textarea.input.mono.cond-val-input（无 .fld 包裹/无可见标签，:3921）。
                    有勾选区时收矮一档 + **默认折叠**（陈先生 2026-07-30：「规则对应的文本框隐藏不显示，
                    避免误修改」）。折叠而不是删掉，因为 `allowFreeInput` 的三条理由一条都没消失：
                    手填 `res:<id>` / 引用上游有而本地还没下载的 tag / 给未运行的应用建规则。
                    而已存在规则里那些池外的值不再依赖这个框才看得见 —— 上面的勾选区已经把它们
                    显式列出来且可点掉（`offPoolSelectedOptions`），文本框这才敢藏。
                    **无候选区（`kind==='free'`）时不折叠**：那时 textarea 是这个类型唯一的入口，
                    折起来等于把整个条件行变成一片空白。 */}
                {all ? (
                  <Fold className="cond-manual" title={t('rules.manualInput')}>
                    {valInput}
                  </Fold>
                ) : (
                  valInput
                )}
                {isResRef && (
                  <>
                    {/* 「前往规则资源」的出路 —— 与应用分流那条（`AppAddDialog` 的 `.warn-line` +
                        同一按钮键）同形。**两个触发条件，各自对应一个真实时刻**（陈先生 2026-07-30
                        裁定：不互斥，都要）：
                         ① `rsMissing` = 本条件引用了本地不可用的规则集。这是有真实后果的那个 ——
                            生成端 fail-closed 剪掉该条件、只留一行 warn ⇒ 规则静默不工作，
                            而保存前只有这里说得出来。
                         ② `rsEmpty` = 勾选区里一条都挑不出来。此刻用户的真实需求就是「我要的还没有」。
                            它**不是常驻噪音**：这一行只在挑不出来时出现，等于给勾选区里那一句说明
                            补上出路。**三态文案各不相同**（还在加载 / 清单加载失败 / 检索无命中，见
                            `poolEmptyText`）—— 把加载失败说成「无匹配」会让用户去改搜索词，而真正的
                            问题是清单压根没拉到。按钮三态都给：都是有效出路。
                        文案按严重度取：①有后果、②只是没得挑 ⇒ ① 优先；两者同时成立时勾选区里那句
                        说明已经把 ② 说了，不重复。按钮两条腿共用（去处相同）。
                        注：腿① 在 loading/failed 两态被 `ruleSetMissing` 恒抑制（那时 available 集合
                        是空的，不抑制就会把每一条已有引用报成「缺失」）⇒ 不存在「失败态说资源缺失」
                        这种不准的话；两态下显示的恒是 ② 的文案。 */}
                    {(rsMissing.length > 0 || rsEmpty) && (
                      <div className="warn-line">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                          <path d="M12 9v4M12 17h.01" />
                          <path d="M10.3 3.9 1.8 18a2 2 0 001.7 3h17a2 2 0 001.7-3L13.7 3.9a2 2 0 00-3.4 0z" />
                        </svg>
                        <span>
                          {rsMissing.length > 0
                            ? t('rules.ruleSetMissingHint', {
                                n: rsMissing.length,
                              })
                            : emptyText}
                        </span>
                        <button
                          type="button"
                          className="btn ghost sm"
                          onClick={() => {
                            navigate('resources');
                            closeAll();
                          }}
                        >
                          {t('appAdd.gotoResources')}
                        </button>
                      </div>
                    )}
                  </>
                )}
              </div>
              {conds.length > 1 ? (
                <button
                  type="button"
                  className="cond-del"
                  aria-label={t('rules.removeCondition')}
                  onClick={() => removeCond(i)}
                >
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M5 5l14 14M19 5L5 19" />
                  </svg>
                </button>
              ) : (
                <span />
              )}
            </div>
          );
        })}

        {/* 显隐口径 = addCond 的取值口径（findAddableRuleType），不能用 conds.length < 总数：
            Windows 少 2 个可用类型，按长度比会在无类型可加时仍显示按钮。 */}
        {findAddableRuleType(new Set(conds.map((c) => c.t)), nodePlatform) !== undefined && (
          <button type="button" className="btn ghost sm" style={{ marginTop: 8 }} onClick={addCond}>
            <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 5v14M5 12h14" />
            </svg>
            <span>{t('rules.addCondition')}</span>
          </button>
        )}
        <div className="card-sub" style={{ marginTop: 8 }}>
          {t('rules.conditionsHint')}
        </div>
      </div>

      {/* 绕过 FakeIP（仅域名类条件显隐，FieldSpec when） */}
      {[bypassSpec]
        .filter((f) => !f.when || f.when(bypassValues))
        .map((f) => (
          <FieldRenderer
            key={f.k}
            spec={f}
            value={bypassValues[f.k]}
            onChange={(v) => {
              setBypassFakeIP(v === true);
              touch();
            }}
          />
        ))}

      {/* 目标出站 */}
      <div className="fld">
        <div className="fld-l">
          {t('rules.target')}
        </div>
        <Csel
          id="rule-target"
          ariaLabel={t('rules.target')}
          value={target}
          onChange={(v) => {
            setTarget(v);
            touch();
          }}
          options={targetGroups}
          openGroupIds={targetOpenGroups}
        />
      </div>

      {/* 测试匹配（折叠，客户端启发式即时反馈） */}
      <div className="fld">
        <details className="rule-test-det" onToggle={revealOnToggle}>
          <summary className="fld-l" style={{ display: 'flex', alignItems: 'center', gap: 6, cursor: 'pointer' }}>
            <span>{t('rules.testMatch')}</span>
            <Chevron />
          </summary>
          <label className="input" style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 11px', marginTop: 8 }}>
            <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8} style={{ color: 'hsl(var(--fg-faint))', flex: 'none' }}>
              <circle cx="11" cy="11" r="7" />
              <path d="M20 20l-3-3" />
            </svg>
            <input
              value={test}
              onChange={(e) => setTest(e.target.value)}
              placeholder={t('rules.testPh')}
              aria-label={t('rules.testMatch')}
              style={{ border: 0, background: 'none', outline: 'none', flex: 1, padding: '8px 0', font: 'inherit', color: 'inherit' }}
            />
          </label>
          <div className="card-sub" style={{ marginTop: 6 }}>
            {testResult === 'hit' && (
              <span style={{ color: 'hsl(var(--ok))' }}>✓ {t('rules.testHit')}</span>
            )}
            {testResult === 'miss' && (
              <span style={{ color: 'hsl(var(--fg-faint))' }}>{t('rules.testMiss')}</span>
            )}
            {testResult === 'untestable' && (
              <span style={{ color: 'hsl(var(--fg-faint))' }}>
                {t('rules.testUntestable')}
              </span>
            )}
          </div>
        </details>
      </div>
    </Modal>
  );
}

export function RuleDialog({ ruleId, preset }: { ruleId?: string; preset?: RulePreset }) {
  // 展示面：编辑基准。读盘的话暂存过的规则再打开会显示改前的旧值。
  const rules = useEffectiveRules();
  const base = ruleId ? rules.find((r) => r.id === ruleId) : undefined;
  // R1：key 绑定 ruleId —— 切换编辑目标 = 重挂 = 同步重新初始化，杜绝挂载后 reset。
  const formKey = ruleId ?? `new:${preset?.type ?? ''}:${preset?.value ?? ''}`;
  return <RuleForm key={formKey} base={base} isEdit={base != null} preset={preset} />;
}

export default RuleDialog;
