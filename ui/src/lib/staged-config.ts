/**
 * 配置暂存层 —— 纯逻辑（条目模型 / 重放 / 撤销 / 三张判据表 / 持久化编解码）。
 *
 * 设计 SoT：`~/docs/polaris/design/polaris-staged-config-spec.md` §2.5 Q1-b / Q2 / Q3 / Q3-b。
 * 本文件**不碰 React、不碰 Storage、不碰 IPC**，全部是可脱离组件直测的纯函数；
 * 有状态的那一半在 `store/staged-config-store.ts`。
 *
 * # 三张表，互相正交，不可合并
 *
 * | 表 | 谓词 | 落法 | 会不会自己变 |
 * |---|---|---|---|
 * | **W-0 豁免** | 键 ∉ Rust `UserConfig` 字段集 | 直接落盘 | 会：Rust 加字段即自动缩小（`contracts/user-config-fields.ts`） |
 * | **W-1/2/3 绕过** | 运行期状态类 / 活态回读类 / 不可逆副作用类 | 直接落盘 | 不会：手工维护（本文件 `BYPASS_TABLE`） |
 * | 默认腿 | ¬豁免 ∧ ¬绕过 | **进暂存** | — |
 *
 * 豁免与绕过磁盘行为相同、语义不同：豁免 = 暂存对它**没有意义**（不重启、无「待应用」可言）；
 * 绕过 = 暂存对它**有害**（UI 会撒谎 / 副作用已经发生）。`selectedServerId` 是分界线：
 * 它**在** `UserConfig` 里（不豁免），但切节点必须立刻生效（绕过）。
 *
 * # 反向边界
 *
 * 三条谓词都不命中的编辑**默认进暂存**。这个默认值让「新增编辑入口时忘了接暂存」退化成
 * 「它进了暂存」（保守、可见），而不是「它绕过了暂存」（不可见、静默破坏语义）。
 */

import { isStagedExempt } from '@/contracts/user-config-fields';

// ─────────────────────────────── 总开关 ───────────────────────────────

/**
 * 暂存层总开关的**编译期默认值**。
 *
 * `true` = 默认腿的编辑（¬豁免 ∧ ¬绕过）进暂存，由条上的「保存 / 立即应用 / 重置」决定何时落盘。
 * `false` = 所有编辑入口走 P3 之前的路径（提交即落盘即入核），暂存层零副作用：不读写存储、
 * 不产生条目、`editRoute()` 恒返 `'direct'`。
 *
 * # 翻成 `true` 的前置条件（2026-07-29 已全部满足，陈先生拍板开启）
 *
 * 开关曾长期为 `false`，因为开着而三动作不存在 = 用户的编辑进了暂存却没有任何按钮能把它落盘，
 * 那不是灰度，是数据丢失路径。以下四条逐条就位后才翻：
 *
 * 1. **P4 三动作 + classify**：保存 / 立即应用 / 重置就位，且「保存」走 `deferRestart` 只落盘不重启；
 * 2. **P5 乐观并发**：`base_version` 校验 + 实体粒度冲突检出 + 自动合并，多写者不互相吞改动；
 * 3. **P6 入口全量接入 + 双向接线守卫**：写侧 `config-write-wiring` / 读侧 `config-read-wiring`
 *    钉住「新增入口漏接暂存」与「新增读点漏改回显」两个方向都会转红；
 * 4. **回显闭环**：`effectiveConfig` / `effectiveCollection` 派生层 + staged-only 角标 +
 *    行内操作策略表（`ENTITY_ACTION_TABLE`），使暂存中的编辑在列表里当场可见、且不会把
 *    盘上不存在的 id 交给后端。
 *
 * 运行期真值取 store 的 `enabled`（初值即本常量），便于单测两侧都跑到；产品行为只由本常量决定。
 */
export const STAGED_CONFIG_ENABLED = true;

// ─────────────────────────────── 条目模型（Q2）───────────────────────────────

/**
 * 受暂存管辖的实体族。`setting` 走键路径寻址，其余走集合内**主键**寻址
 * （主键是哪个字段由 `ID_ADDRESSED_COLLECTIONS` 定，不恒是 `id`）。
 *
 * `appPreset`（`customAppPresets`）与 `appRule`（`appRules`）分开而不合并：两者是不同集合、
 * 不同主键（`id` / `appId`），删一个自定义应用会同时产生两族各一条条目。
 */
export type StagedEntryKind =
  | 'server'
  | 'rule'
  | 'subscription'
  | 'appRule'
  | 'appPreset'
  | 'setting';

const ENTRY_KINDS: ReadonlySet<string> = new Set<StagedEntryKind>([
  'server',
  'rule',
  'subscription',
  'appRule',
  'appPreset',
  'setting',
]);

/**
 * config 里**按主键寻址**的集合 → 该集合的**主键字段名**（Q8-b「实体」定义的前一半）。
 * 其余顶层设置按键名寻址。
 *
 * 这张表决定 `entityPath` 的解释方式：`['servers','abc']` 是「servers 里 id=abc 那个元素」，
 * 而 `['dnsConfig','enableFakeIp']` 是「dnsConfig 对象下的 enableFakeIp 键」。
 *
 * # 为什么是映射而不是集合
 *
 * 「按哪个字段寻址」是**集合自己的属性**：`AppRule` 的主键是 `appId`，结构里根本没有 `id`
 * （`contracts/types/rules.ts`）。把主键写死成 `id` 会让整个 appRules 族在本模型里**表达不了** ——
 * 那是模型缺陷，不是业务约束。`isValidStagedEntry` / `upsertById` / `entitySnapshot` 三处
 * **共用本表**（经 `primaryKey`），不得各写各的：三处一旦分叉，校验放行的条目会被重放写到
 * 「另一个实体」上，而冲突检出还比对着第三个东西。
 */
const ID_ADDRESSED_COLLECTIONS: ReadonlyMap<string, string> = new Map([
  ['servers', 'id'],
  ['customRules', 'id'],
  ['subscriptions', 'id'],
  ['appRules', 'appId'],
  ['customAppPresets', 'id'],
  ['ruleResources', 'id'],
]);

/**
 * 该集合的主键字段名。非集合键不该走到这里（三个调用点都先过 `isCollectionPath` / `isOrderPath`），
 * 兜底返 `'id'` 而不是抛：本模块在渲染路径上，抛 = 白屏。
 */
function primaryKey(collection: string): string {
  return ID_ADDRESSED_COLLECTIONS.get(collection) ?? 'id';
}

/**
 * 一条**编辑意图**（不是一个 diff，也不是一个字段 patch）。
 *
 * 粒度选「意图条目」的因果（Q2）：
 *  - 逐项撤销要求条目可独立移除后重放。整份快照 diff 做不到——两次编辑改同一字段时 diff 已经把它们合并了。
 *  - 逐字段太细：一次节点编辑表单提交改 8 个字段，用户心智是「改了节点 A」一条，不是 8 条。
 *
 * # `nextValue` 必须是幂等的整体替换
 *
 * 而非增量（`port += 1`），否则重放对顺序敏感、重放两次结果不同。这在 Polaris 里天然成立：
 * 所有编辑入口提交的都是**整个实体**（`NodeDialog` / `RuleDialog` 传的都是完整对象），
 * 没有任何一个入口做增量修改。**这条是重放正确性的前提**，新增入口时必须守住。
 */
export interface StagedEntry {
  /**
   * 稳定 id。同一实体重复编辑**覆盖同一条**（原型 `markDirty` 的 id 语义）→ 计数不虚高。
   * 约定形态 `${kind}:${实体 id 或键路径}`，但本模块只要求非空且唯一，不解析它。
   */
  readonly id: string;
  readonly kind: StagedEntryKind;
  /** 明细 popover 的显示文案（「编辑节点 香港 IEPL 01」）。由调用方按 kind + 实体名生成。 */
  readonly label: string;
  /**
   * 目标实体寻址路径。集合实体 = `[集合键, 实体主键值]`；**整集合顺序** = `[集合键]` 单段；
   * 设置键 = 键路径（`['mixedPort']` / `['dnsConfig','enableFakeIp']`）。
   * P5 的冲突检出按这条路径取实体子树比对。
   */
  readonly entityPath: readonly string[];
  /**
   * **原子撤销组**（可选）。同组条目在撤销时**连坐**：撤其中任意一条 ⇒ 全组一起消失。
   *
   * # 为什么需要它
   *
   * 有些单次用户动作天然产生**跨集合的多条条目**（删一个自定义应用 = 删 `customAppPresets` 里的预设
   * + 删 `appRules` 里引用它的规则）。逐条撤销若允许只撤一半，用户会得到「预设还在、规则没了」——
   * 那是**谁都没要过的第三种状态**，与 Q8-b 拒绝字段级 diff 的理由同型（半个旧节点 + 半个新节点）。
   *
   * # 为什么不能由 `entityPath` 推断
   *
   * 同组条目**跨集合**（`customAppPresets` / `appRules`），路径上没有任何共同前缀可推；
   * 组是「用户动作」这一维的事实，只能由产生条目的入口显式声明。
   *
   * 绝大多数条目**没有组**（`undefined`）⇒ 行为与没有本字段时逐字节相同。
   * 组不影响 `stageEntry`：同 id 仍是覆盖，同组不同 id 是两条独立条目，只在撤销时连坐。
   */
  readonly groupId?: string;
  /**
   * 幂等的整体替换值。
   *
   * **`null` 只在集合实体上表示「删除该实体」**；键路径上 `null` 就是字面值 `null`
   * （`selectedServerId: null` 是合法配置值，不能被解释成删除——虽然它走绕过腿不进暂存，
   * 但语义不能留二义）。
   *
   * **顺序条目上是该集合的完整主键序列**（`string[]`）—— 同样是整体替换，只不过替换的那件事
   * 是「次序」而不是「内容」。
   */
  readonly nextValue: unknown;
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function isCollectionPath(path: readonly string[]): boolean {
  return path.length === 2 && ID_ADDRESSED_COLLECTIONS.has(path[0]);
}

/**
 * **整集合顺序条目**的路径形态：单段集合键（`['customRules']`）。
 *
 * 与实体路径（两段）不可能混淆，也不与设置键路径重叠 —— 集合键当首段的非两段路径此前一律被
 * `isValidStagedEntry` 拒收（`['servers']` 整份替换不是暂存的粒度），本形态是从那条禁令里
 * 精确切出来的一个例外：它替换的不是集合内容，只是**元素次序**。
 *
 * # 为什么顺序必须能进暂存，而不是让它绕过
 *
 * 同一个规则页里「改规则」进暂存、「拖排序」直落盘 ⇒ UI 自相矛盾：列表顺序当场变了，
 * 但「N 项待保存」不算它，保存后顺序又跟别的改动一起生效。排序决定命中优先级，
 * 说不清 = 用户按错的优先级理解分流结果。
 */
function isOrderPath(path: readonly string[]): boolean {
  return path.length === 1 && ID_ADDRESSED_COLLECTIONS.has(path[0]);
}

/**
 * 条目合法性。**唯一判据点**：`stageEntry` 拒收、`replay` 跳过、持久化解码丢弃，三处共用。
 *
 * 为什么要有它：`decodeStagedPayload` 的入参来自 Storage —— 那是进程外数据（用户手改、旧版本残留、
 * 半截写入），不校验就直接喂给 `replay`，一条畸形条目能把整份 config 搞坏或让渲染端抛异常白屏。
 */
export function isValidStagedEntry(value: unknown): value is StagedEntry {
  if (!isRecord(value)) return false;
  const { id, kind, label, entityPath, nextValue, groupId } = value;
  if (typeof id !== 'string' || id === '') return false;
  if (typeof kind !== 'string' || !ENTRY_KINDS.has(kind)) return false;
  if (typeof label !== 'string') return false;
  // 分组标识：缺席合法（绝大多数条目无组）；一旦出现就必须是非空串。**畸形不得被静默接受**——
  // 载荷来自进程外，把 `groupId: 123` 悄悄当成「无组」会让本该连坐的一组退化成可拆撤销，
  // 恰好还原出这个字段要消灭的那个第三种状态。整条判非法（连带丢弃）是唯一诚实的收场。
  if (groupId !== undefined && (typeof groupId !== 'string' || groupId === '')) return false;
  if (!Array.isArray(entityPath) || entityPath.length === 0) return false;
  if (!entityPath.every((seg) => typeof seg === 'string' && seg !== '')) return false;
  if (isCollectionPath(entityPath as string[])) {
    // 集合实体：要么删除（null），要么给一个**主键**与路径相符的完整对象。
    // 主键不符会让重放把实体写到「另一个实体」上，是最难查的一类静默错位。
    if (nextValue === null) return true;
    return isRecord(nextValue) && nextValue[primaryKey(entityPath[0] as string)] === entityPath[1];
  }
  if (isOrderPath(entityPath as string[])) {
    // 顺序条目：`nextValue` = 该集合的**完整主键序列**（「幂等的整体替换」在顺序这一维上的形态）。
    // 不接受增量（「把 x 挪到第 3 位」）—— 那种重放对顺序敏感、重放两次结果不同。
    return Array.isArray(nextValue) && nextValue.every((v) => typeof v === 'string' && v !== '');
  }
  // 键路径：不接受集合键当首段却路径长度不为 1 / 2 的形态（`['servers','a','b']` 无语义）。
  return !ID_ADDRESSED_COLLECTIONS.has(entityPath[0] as string);
}

// ─────────────────────────────── 重放 / 撤销（Q2）───────────────────────────────

function upsertById(
  cfg: Record<string, unknown>,
  collection: string,
  entityId: string,
  nextValue: unknown
): Record<string, unknown> {
  const raw = cfg[collection];
  const list: unknown[] = Array.isArray(raw) ? raw : [];
  const pk = primaryKey(collection);
  const idx = list.findIndex((e) => isRecord(e) && e[pk] === entityId);
  if (nextValue === null) {
    // 删除。集合本就不存在 ⇒ 原样返回（不凭空补一个空数组键，重放必须不改变无关形状）。
    if (!Array.isArray(raw) || idx < 0) return cfg;
    return { ...cfg, [collection]: [...list.slice(0, idx), ...list.slice(idx + 1)] };
  }
  if (idx < 0) return { ...cfg, [collection]: [...list, nextValue] };
  const next = [...list];
  next[idx] = nextValue;
  return { ...cfg, [collection]: next };
}

/**
 * 按主键序列重排一个集合。**不可变**，且对「序列里有已删 id」「集合里有序列没提到的元素」
 * 两种失配都收敛，不抛（本函数在渲染路径上）。
 *
 * 规则一句话：**序列提到的元素按序列排在前，没提到的按原相对序列在后**。
 *  - 序列里的 id 在集合里不存在（那条被同批的删除条目干掉了 / 磁盘侧被别人删了）⇒ 跳过。
 *  - 集合里的元素不在序列里（同批新增的实体）⇒ 落到末尾，保持它们之间的原相对序。
 *
 * 这条规则同时给出**幂等**（重排完再排一次，分区与名次都不变 ⇒ 结果相同）与 `replay` 需要的
 * **可交换性**（见 `replay` 的两趟说明）。
 */
function reorderCollection(
  cfg: Record<string, unknown>,
  collection: string,
  orderedIds: readonly string[]
): Record<string, unknown> {
  const raw = cfg[collection];
  // 集合不存在 ⇒ 原样返回（同 `upsertById` 的删除腿：重放必须不改变无关形状）。
  if (!Array.isArray(raw)) return cfg;
  const pk = primaryKey(collection);
  const rank = new Map<string, number>();
  orderedIds.forEach((id, i) => {
    if (!rank.has(id)) rank.set(id, i);
  });
  const listed: unknown[] = [];
  const rest: unknown[] = [];
  for (const e of raw) {
    const id = isRecord(e) ? e[pk] : undefined;
    if (typeof id === 'string' && rank.has(id)) listed.push(e);
    else rest.push(e);
  }
  listed.sort((a, b) => rank.get((a as Record<string, string>)[pk])! - rank.get((b as Record<string, string>)[pk])!);
  return { ...cfg, [collection]: [...listed, ...rest] };
}

function setPath(
  obj: Record<string, unknown>,
  path: readonly string[],
  value: unknown
): Record<string, unknown> {
  const [head, ...rest] = path;
  if (rest.length === 0) return { ...obj, [head]: value };
  const child = isRecord(obj[head]) ? obj[head] : {};
  return { ...obj, [head]: setPath(child, rest, value) };
}

/**
 * 把一条条目重放到 config 上。**不可变**：返回新对象，不改入参。
 *
 * 非法条目**跳过**而非抛错：本函数在渲染路径上，抛错 = 白屏。合法性由 `stageEntry` / 解码两处把关，
 * 这里是最后一道兜底（`replay` 跳过畸形条目 + 计数仍来自 store 持有的已校验条目，两者不会打架）。
 */
function applyEntry(cfg: Record<string, unknown>, entry: StagedEntry): Record<string, unknown> {
  if (!isValidStagedEntry(entry)) return cfg;
  if (isCollectionPath(entry.entityPath)) {
    return upsertById(cfg, entry.entityPath[0], entry.entityPath[1], entry.nextValue);
  }
  if (isOrderPath(entry.entityPath)) {
    return reorderCollection(cfg, entry.entityPath[0], entry.nextValue as readonly string[]);
  }
  return setPath(cfg, entry.entityPath, entry.nextValue);
}

/** 合法且是顺序条目。非法条目在这里判 `false`，由第一趟的 `applyEntry` 统一跳过（判据仍只一处）。 */
function isOrderEntry(entry: StagedEntry): boolean {
  return isValidStagedEntry(entry) && isOrderPath(entry.entityPath);
}

/**
 * 从 baseline 重放全部条目，得到 `effectiveConfig`。
 *
 * 空条目集 ⇒ 恒等（返回与 baseline 逐字段相等的对象），这是「重置」后 UI 与磁盘一致的依据（FR-4 / S-2）。
 *
 * # 两趟：先实体变更，后顺序
 *
 * 顺序条目描述的是用户拖完之后**他在列表里看到的那个排列**，而那个列表已经含同批新增/删除的实体。
 * 若单趟按条目入表先后重放，「先拖后加」与「先加后拖」会得到两种结果（新实体一个落末尾、
 * 一个落在拖到的位置）—— 收敛结果依赖条目入表顺序，而条目顺序是用户操作次序的偶然产物。
 *
 * 分两趟后二者**可交换**：新增实体不在 id 序列里 ⇒ 落末尾；已删实体的 id 在序列里 ⇒ 被忽略。
 * 每趟内部各自仍按条目序（实体条目之间同 id 覆盖、不同 id 互不相干；顺序条目每集合至多一条 ——
 * 调用方按 `order:<集合>` 铸 id，`stageEntry` 同 id 覆盖）。
 */
export function replay<T extends object>(baseline: T, entries: readonly StagedEntry[]): T {
  const entityPass = entries
    .filter((e) => !isOrderEntry(e))
    .reduce<Record<string, unknown>>(applyEntry, { ...(baseline as Record<string, unknown>) });
  return entries.filter(isOrderEntry).reduce<Record<string, unknown>>(applyEntry, entityPass) as T;
}

/**
 * 追加/覆盖一条条目。同 id **就地覆盖**（保序 —— 重放结果与条目顺序有关，覆盖若改成「删了再追加」
 * 会让先编 A 后编 B 再回头编 A 的结果与直觉不符）。
 *
 * 非法条目原样返回（不入表），调用方不必自己校验。
 */
export function stageEntry(
  entries: readonly StagedEntry[],
  entry: StagedEntry
): StagedEntry[] {
  if (!isValidStagedEntry(entry)) return [...entries];
  const idx = entries.findIndex((e) => e.id === entry.id);
  if (idx < 0) return [...entries, entry];
  const next = [...entries];
  next[idx] = entry;
  return next;
}

/**
 * 撤销单条（FR-3 / S-5）。
 *
 * **实现必须是「移除后重放」而不是「追加一条反向 patch」**：后者在「同一实体被改过两次」时会把
 * 中间态留下，与「它从未加入过」不等价。整体替换 + 重放天然提供逆操作，不需要存 inverse patch。
 *
 * # 组连坐在这里、不在调用点
 *
 * 带 `groupId` 的条目撤一条 ⇒ 全组一起走（见 `StagedEntry.groupId`）。放进这个纯函数而不是放进
 * popover：撤销入口不止一个，也不会永远只有一个，语义放在入口侧 = 每个入口都要**记得**连坐，
 * 忘一个就漏出那个第三种状态。放这里则任何撤销入口自动获得同一语义，调用方一个字都不用改。
 *
 * # 已知遗留（本轮不做）
 *
 * 明细 popover 仍把同组条目显示成**两行**，点其中一行会看到两行一起消失，观感突兀。
 * 合并成一行显示属 UI 打磨，要动 `components/layout/PendingChangesBar.tsx`，与本轮语义修复正交。
 */
export function revertEntry(entries: readonly StagedEntry[], id: string): StagedEntry[] {
  const groupId = entries.find((e) => e.id === id)?.groupId;
  // 无组（含 id 未命中）⇒ 与没有本字段时逐字节相同的那条腿。
  if (groupId === undefined) return entries.filter((e) => e.id !== id);
  return entries.filter((e) => e.groupId !== groupId);
}

/**
 * 「待保存」标记的判据：**在 effective 里、但不在 disk 里**的实体主键集合。
 *
 * 不新造字段、不新造词汇 —— 标记与 pending-bar 的「N 项待保存」是同一语义面（同一批条目的两种呈现：
 * 条上是计数，列表里是逐个实体的角标）。判据取两个集合的差而不是「条目里有没有它」，因为
 * **编辑一个已落盘的实体也会产生条目**，那种实体在盘上找得到、不该被标成「还没保存的新东西」。
 *
 * 只覆盖主键为 `id` 的集合（`servers` / `customRules`）；`appRules` 按 `appId` 寻址，
 * 需要时另给一条，不在这里塞一个多态参数（那会让调用点又开始判 class）。
 */
export function stagedOnlyIds(
  effective: readonly { id: string }[],
  disk: readonly { id: string }[]
): ReadonlySet<string> {
  const onDisk = new Set(disk.map((e) => e.id));
  return new Set(effective.filter((e) => !onDisk.has(e.id)).map((e) => e.id));
}

/**
 * 目标为该实体的那条条目。**按 `entityPath` 寻址，不解析 `entry.id` 的约定形态** ——
 * `StagedEntry.id` 的文档明写「本模块只要求非空且唯一，不解析它」，靠 `${kind}:${id}` 这个约定
 * 反查会让那句话变成谎话，且换个铸 id 的入口就悄悄失配。
 */
export function entryForEntity(
  entries: readonly StagedEntry[],
  collection: string,
  entityId: string
): StagedEntry | undefined {
  return entries.find(
    (e) => e.entityPath.length === 2 && e.entityPath[0] === collection && e.entityPath[1] === entityId
  );
}

// ────────────────── staged-only 实体的行内动作策略（三种语义，一张表）──────────────────

/**
 * 一个「把实体 id 交给后端」的动作，落在 **staged-only 实体**（在 effective 里、不在 disk 里）上时怎么办。
 *
 * # 为什么必须是三种、不能一刀切
 *
 * 后端一律按 id 在磁盘上找，staged-only 找不到 ⇒ 三个动作**失败的含义完全不同**：
 *
 * - `revert` —— 删除。用户是在**撤销自己尚未保存的新增**，这本来就是 `revertEntry` 的语义，
 *   不是变通（pending-bar 的逐条撤销已经是同一个动作）。做成「报错」等于告诉用户「你删不掉你自己刚加的东西」。
 * - `block` —— 测速。测速要求运行核里真有这个节点，盘上没有就测不出真值。
 *   **置灰 + 说明**而不是过滤掉按钮：过滤更不诚实 —— 用户不知道这张卡为什么跟别的不一样。
 * - `disk-only` —— 集合计数。那个数字承诺的是「确认之后后端会删掉几个」，而后端级联删的就是磁盘上那些；
 *   数 effective 会虚高，用户按一个虚高的数做决定。
 *
 * # 这不是「按调用点补闸门」
 *
 * 被禁的是**在调用点手工判这个键属哪个 class**。这里是同一个谓词（`stagedOnlyIds`，本文件已有，不新造）
 * 配三种动作策略：**策略只写在本表**，调用点做的是查表 + 按查到的答案分流，不含任何策略判断。
 */
export type StagedOnlyStrategy = 'revert' | 'block' | 'disk-only';

export interface EntityActionRule {
  /** 动作标识（`<实体族>.<动作>`）。与 `BYPASS_TABLE.op` 同为手工维护的动作标识，两张表正交。 */
  readonly op: string;
  readonly strategy: StagedOnlyStrategy;
  readonly why: string;
}

export const ENTITY_ACTION_TABLE: readonly EntityActionRule[] = [
  {
    op: 'server.delete',
    strategy: 'revert',
    why: '删一个还没保存的新节点 = 撤销那条条目本身；走后端只会拿到一个盘上不存在的 id',
  },
  {
    op: 'server.deleteBatch',
    strategy: 'revert',
    why: '同 server.delete，只不过一批里 staged-only 与盘上实体混在一起 —— 两半各走各的腿',
  },
  {
    op: 'rule.delete',
    strategy: 'revert',
    why: '同 server.delete。注意与「删一条盘上已有的规则」不同：那条走的是暂存的删除条目（nextValue: null）',
  },
  {
    op: 'server.speedTest',
    strategy: 'block',
    why: '测速要求运行核里真有这个节点；盘上没有就测不出真值。置灰 + 提示「保存后可测速」，不过滤掉按钮',
  },
  {
    op: 'server.tailscaleLogout',
    strategy: 'block',
    why: '登出会清磁盘上的 TS state 目录；盘上没有这个节点时该动作无对象。今天造不出 staged-only 的 TS 节点（新增腿恒直落盘，由 config-write-wiring 的「TS/WARP 新增腿恒 direct」钉住），但那是**论证**不是机械事实 —— ImportDialog 自陈 sing-box endpoints[] 分支一旦接通即失效，故显式登记而不是靠默认腿',
  },
  {
    op: 'warp.edit',
    strategy: 'block',
    why: '「编辑这台已注册的 WARP 设备」这一个用户动作会连发 applyWarpLicense（当场改远端账户等级，W-3 不可逆）+ server.update（改一台已注册设备）；盘上没有这个节点，两者都没有作用对象。合成一个 op 而不是拆两条：它们由**同一次**表查询守着，拆开会让登记表描述一个源码里不存在的分流。今天不可达是**论证**不是机械事实 —— ImportDialog 自陈 sing-box endpoints[] 分支一旦接通即失效，故显式登记而不是靠默认腿',
  },
  {
    op: 'subscription.deleteNodeCount',
    strategy: 'disk-only',
    why: '那个数字承诺「确认后后端会删掉几个」，后端级联删的就是磁盘上那些；数 effective 会虚高',
  },
];

const ENTITY_ACTION_BY_OP = new Map(ENTITY_ACTION_TABLE.map((r) => [r.op, r]));

/**
 * 查表。**未登记的 op 返回 `'block'`**（最保守：不把一个可能不存在的 id 交给后端），
 * 而不是抛 —— 本函数在点击回调里，抛 = 一个点了就炸的按钮。
 * 「忘了登记」由 `lib/entity-action-wiring.test.ts` 那盏红灯抓，不靠运行期爆炸。
 */
export function stagedOnlyStrategyOf(op: string): StagedOnlyStrategy {
  return ENTITY_ACTION_BY_OP.get(op)?.strategy ?? 'block';
}

/** 空集常量：`stagedOnly` 为空时三个字段都得是稳定引用，调用方才能靠 `toBe` 判「什么都没变」。 */
const NO_IDS: readonly string[] = [];

export interface StagedOnlySplit {
  /** 照旧交给后端的那些 id。 */
  readonly backend: readonly string[];
  /** 改走 `revertEntry` 的**条目 id**（不是实体 id —— 撤销的入参是条目）。 */
  readonly revertEntryIds: readonly string[];
  /** 策略为 `block` 时被挡下的实体 id（调用方负责说明为什么，不许静默丢）。 */
  readonly blocked: readonly string[];
}

/**
 * 把一批实体 id 按「盘上有没有」分成两半，分法由 `op` 在 `ENTITY_ACTION_TABLE` 里的策略决定。
 *
 * **`stagedOnly` 为空（总开关关着时恒成立）⇒ `backend` 就是入参本体**，另外两个是同一个空数组常量
 * ⇒ 调用方走的是与今天逐字节相同的那条路径。这条性质由 `toBe` 钉住。
 *
 * staged-only 实体在条目表里找不到对应条目（理论上不该发生：它之所以 staged-only 就是因为有条目）
 * ⇒ 落回 `backend`，让后端如实报错，而不是静默吞掉这次删除。
 */
export function splitStagedOnly(
  op: string,
  ids: readonly string[],
  stagedOnly: ReadonlySet<string>,
  entries: readonly StagedEntry[],
  collection: 'servers' | 'customRules'
): StagedOnlySplit {
  if (stagedOnly.size === 0) return { backend: ids, revertEntryIds: NO_IDS, blocked: NO_IDS };
  const strategy = stagedOnlyStrategyOf(op);
  const backend: string[] = [];
  const revertEntryIds: string[] = [];
  const blocked: string[] = [];
  for (const id of ids) {
    if (!stagedOnly.has(id)) {
      backend.push(id);
      continue;
    }
    if (strategy === 'revert') {
      const entry = entryForEntity(entries, collection, id);
      if (entry) revertEntryIds.push(entry.id);
      else backend.push(id);
      continue;
    }
    blocked.push(id);
  }
  return { backend, revertEntryIds, blocked };
}

// ─────────────────────────────── 冲突检出（Q8-b）───────────────────────────────

/**
 * 「该实体在这份 config 里长什么样」的可比较快照。**实体不存在**与「值为 `null`」必须可区分
 * （`selectedServerId: null` 是合法配置值；而实体被删掉是另一回事，见 Q8 最后一行），
 * 故缺席用一个 `stableStringify` 永不可能产出的哨兵串表达，而不是 `'null'` / `''`。
 */
const ABSENT = '<absent>';

/**
 * 该快照是否表示「实体在这份 config 里根本不存在」（而非「值是 `null`」）。
 * `<` 是 `stableStringify` 永不可能产出的首字符（JSON 值只会以 `{[">-` / 数字 / `tfn` 开头）。
 */
export function isAbsentSnapshot(snapshot: string): boolean {
  return snapshot === ABSENT;
}

export function entitySnapshot(config: unknown, path: readonly string[]): string {
  if (isOrderPath(path)) {
    // 顺序条目的「实体」= 这个集合的**主键序列本身**，不是它元素的内容。取整份集合序列化会把
    // 「别人改了某条规则的 enabled」也判成顺序冲突 —— 实体粒度是 Q8-b 的语义，这里同样按语义取。
    const list = isRecord(config) ? config[path[0]] : undefined;
    if (!Array.isArray(list)) return ABSENT;
    const pk = primaryKey(path[0]);
    return stableStringify(list.map((e) => (isRecord(e) ? e[pk] : null)));
  }
  if (isCollectionPath(path)) {
    const list = isRecord(config) ? config[path[0]] : undefined;
    if (!Array.isArray(list)) return ABSENT;
    const pk = primaryKey(path[0]);
    const hit = list.find((e) => isRecord(e) && e[pk] === path[1]);
    return hit === undefined ? ABSENT : stableStringify(hit);
  }
  let cur: unknown = config;
  for (const seg of path) {
    if (!isRecord(cur) || !(seg in cur)) return ABSENT;
    cur = cur[seg];
  }
  return stableStringify(cur);
}

/**
 * 冲突集（Q8-b 第 2 步）：**这条条目要动的那个实体，在磁盘侧也被别人动过**。
 *
 * # 为什么是实体粒度、不做字段级 diff
 *
 * 字段级会把「两人改同一节点的不同字段」判成可合并 —— 而那正是最该问用户的情形：
 * 半个旧节点 + 半个新节点是谁都没要过的第三种东西。粒度是 U-4 拍板的语义，不是实现便利。
 *
 * # `baseline` 是什么、不是什么
 *
 * 必须是 **staged 建立那一刻的磁盘快照**。传 `effectiveConfig`（= `replay(baseline, entries)`）
 * 会把用户自己的每一条改动都算成「磁盘侧变了」⇒ 恒冲突、弹窗噪音拉满、自动合并腿永不可达。
 */
export function conflictingEntries(
  baseline: unknown,
  disk: unknown,
  entries: readonly StagedEntry[]
): StagedEntry[] {
  return entries.filter(
    (e) => entitySnapshot(baseline, e.entityPath) !== entitySnapshot(disk, e.entityPath)
  );
}

// ─────────────────────────────── W-1/2/3 绕过表（Q3）───────────────────────────────

/**
 * - **W-1 运行期状态类**：改的是运行核生命周期或运行期选择，结果被 UI 实时回显。
 *   延后生效 ⇒ 存在一个 UI 元素在延后窗口内显示假值。
 * - **W-2 活态回读类**：该字段有独立于 config 的活态真值源（OS / 运行核 / 远端），UI 上有从活态源回读的显示。
 *   暂存会让 config 侧与活态值分叉，UI 两处自相矛盾。
 * - **W-3 不可逆副作用类**：执行伴随不可逆或有远端效应的副作用。暂存语义是「还没发生」，
 *   但副作用已经发生 ⇒「重置」回滚不了，语义撒谎。
 */
export type BypassPredicate = 'W-1' | 'W-2' | 'W-3';

export interface BypassRule {
  /** 操作标识（不是 config 键——绕过的判据面是「操作」，豁免的判据面才是「键」）。 */
  readonly op: string;
  readonly predicate: BypassPredicate;
  /** 该操作写的 config 顶层键；不经 config 的操作（起停核 / 隐私锁）为 undefined。 */
  readonly configKey?: string;
  readonly why: string;
}

/**
 * 绕过表。**手工维护**，成员不随 Rust `UserConfig` 变化——这正是它必须与 W-0 豁免表分开的原因。
 *
 * 若这些操作进了暂存会怎样（因果，非罗列）：
 *  - 切节点进暂存 → 列表高亮 A、状态栏显示 A，但流量走 B，出口 IP 探测回来的是 B 的 IP，
 *    解锁检测测的是 B 的出口。这不是「延迟生效」，是 **UI 系统性撒谎**。
 *  - 模式切换进暂存 → `useSystemProxyLive` 每轮从 OS 回读的活态与 staged 值永久不等，
 *    首页降级横幅会在用户没做错任何事的情况下常亮。
 *  - 起停核进暂存 → 荒谬（不经 config，且「暂存一个断开动作」无语义）。
 */
export const BYPASS_TABLE: readonly BypassRule[] = [
  {
    op: 'proxyStartStop',
    predicate: 'W-1',
    why: '起停核不经 config 落盘，天然在暂存射程外',
  },
  {
    op: 'switchServer',
    predicate: 'W-1',
    configKey: 'selectedServerId',
    why: '首页出口框 / 状态栏节点名 / willRestartOnSelect 提示都实时回显它',
  },
  {
    op: 'switchProxyMode',
    predicate: 'W-1',
    configKey: 'proxyMode',
    why: 'useHomeModeLine 实时回显；routingBusy 单飞守卫说明它被设计成同步操作',
  },
  {
    op: 'systemProxyTakeover',
    predicate: 'W-2',
    configKey: 'proxyModeType',
    why: 'useSystemProxyLive 从 OS 回读活态；config 侧暂存值与 OS 活态分叉 → 降级横幅与状态点自相矛盾',
  },
  {
    op: 'privacyLock',
    predicate: 'W-2',
    why: '走独立命令 privacyApi、有独立后端态，不经 config 整份保存',
  },
  {
    op: 'deleteTailscaleNode',
    predicate: 'W-3',
    why: '删 = tailscale rm state，回滚只得空壳坏节点',
  },
  { op: 'deleteWarpNode', predicate: 'W-3', why: '远端注销设备，不可逆' },
  {
    op: 'refreshSubscription',
    predicate: 'W-3',
    why: '已发生网络请求 + 已拿到新节点集，「重置」无法把订阅退回旧内容',
  },
  {
    op: 'deleteSubscription',
    predicate: 'W-3',
    why: '后端 apply_subscription_delete 级联删「订阅 + 其下全部节点」并回落出口，是一次原子操作：拆成 N+1 条条目重放后会与后端级联重复执行（同一批节点被删两遍、兜底出口被重选两次）',
  },
];

const BYPASS_OPS: ReadonlySet<string> = new Set(BYPASS_TABLE.map((r) => r.op));
const BYPASS_KEYS: ReadonlySet<string> = new Set(
  BYPASS_TABLE.map((r) => r.configKey).filter((k): k is string => k !== undefined)
);

export function isBypassedOp(op: string): boolean {
  return BYPASS_OPS.has(op);
}

export function isBypassedConfigKey(key: string): boolean {
  return BYPASS_KEYS.has(key.split('.', 1)[0]);
}

// ─────────────────────────────── 路由判定（唯一闸门）───────────────────────────────

export type EditRoute = 'direct' | 'staged';

/**
 * 编辑入口的落地路径判定 —— **所有编辑入口只经这一个函数**。
 *
 * ```
 * 豁免(key) := key ∉ UserConfigFieldSet   // W-0
 * 绕过(op)  := W-1 ∨ W-2 ∨ W-3            // 与上式正交的另一张表
 * 进暂存    := 开关开 ∧ ¬豁免 ∧ ¬绕过       // 默认腿
 * ```
 *
 * `enabled=false` ⇒ 恒 `'direct'`，与今天逐字节相同（关掉总开关的行为等价性由本函数单点保证：
 * 入口侧不得再写第二处 if）。
 */
export function editRoute(
  configKey: string,
  enabled: boolean,
  op?: string
): EditRoute {
  if (!enabled) return 'direct';
  if (op !== undefined && isBypassedOp(op)) return 'direct';
  if (isBypassedConfigKey(configKey)) return 'direct';
  if (isStagedExempt(configKey)) return 'direct';
  return 'staged';
}

/**
 * 暂存层此刻**活着没有** —— 喂给 [`editRoute`] 的 `enabled` 实参由本函数单点算出。
 *
 * ```
 * 活跃 := 总开关开 ∧ ¬(核没在跑 ∧ 没有已暂存条目)
 * ```
 *
 * # 为什么核没跑时要绕开暂存
 *
 * 暂存层的价值是「攒够了再一次性入核，避免连改 N 条 = 断流 N 次」。**核没在跑时那个价值为零**：
 * 改动本来就要等下次起核才生效，先落盘与先暂存对内核毫无差别，而暂存多要求用户再点一次「保存」
 * —— 用户视角就是「我改了，什么也没发生」（陈先生原话：停核期间的改动是否应该直接生效）。
 *
 * # 判据为什么是 `coreRunning === false` 而不是「这条改动需不需要重启」
 *
 * 后者是 `(old norm, new norm)` 的函数、只有 Rust 侧落盘后算得出，前端事前算不出；在前端搞一张
 * 静态白名单就是 norm 投影的第二份副本，必然漂移。
 *
 * 而 `coreRunning === false` **不是那个判据的近似，它就是那个判据的一条腿在本地求值**：
 * `classify_switch` 的 leg 0.5 正是 `if !core_running() → NotRunning`，而 `classify_staged` 把
 * `NotRunning → "noOp" → restartRequired:false`。核没跑时两者**构造上一致**，不存在漂移。
 * （更精确的超集判据是问后端 `config:classifyStaged`——它还能覆盖「核在跑但这条可热切」，
 * 代价是把写入路由异步化、且逐条答案不能为批次组合，留作后续升级。）
 *
 * # 为什么还要 `entries.length === 0` 这一条（选项 (d) 与 (b) 的分界）
 *
 * 只判 `coreRunning` 会造出**分裂态**：老的编辑还在暂存里、新的已经落了盘。那批暂存条目是相对
 * `baseline` 建立的，直写把盘从它底下抽走 ⇒ 之后保存那批会因为**用户自己造成的改动**而弹冲突窗。
 * 故「已有暂存条目」时一律继续走暂存，让那一批干干净净地被「保存」收尾。
 *
 * 代价是行为随「有没有暂存条目」而变（有时改完即生效、有时还要点保存），但那一刻条上明写着
 * 「N 项待保存」，状态是可读的。
 *
 * # 与总开关的分工（别混用）
 *
 * 本函数只用于**路由判定**。`stage`/`revert`/`reset`/`hydrate` 内部判的仍是 store 的
 * `enabled`（「这个特性开没开」）—— 拿本函数去关那些会让已暂存的条目在核停时突然无法撤销。
 */
export function stagingActive(
  enabled: boolean,
  coreRunning: boolean,
  stagedCount: number
): boolean {
  if (!enabled) return false;
  return coreRunning || stagedCount > 0;
}

// ─────────────────────────────── 持久化编解码（Q1-b）───────────────────────────────

/**
 * 持久化 key。**不带版本后缀**（与 spec §Q1-b 字面的 `polaris.staged.v1:<baseVersion>` 有意分歧）：
 * 带 config 版本的 key 后台每写一次盘就留一条**永不回收**的残留（R4 配额那一栏最容易踩的坑），
 * 且那份 staged 从此再也找不回来 —— 正是本轮要修掉的静默丢失，只不过藏在 key 里更难看见。
 *
 * `v1` 是**编解码格式**版本。这一轮把 value 从 `{baseVersion,entries}` 换成 `{baseline,entries}`
 * 却**不升 v2**：老形态由 `decodeStagedPayload` 判 `null` 后当场覆盖/清除，升版号只会在
 * 存储里多留一条没人回收的 `v1`。
 */
export const STAGED_STORAGE_KEY = 'polaris.staged.v1';

/**
 * 持久化载荷。**存的是 `baseline` 本体，不是它的版本 hash。**
 *
 * # 为什么必须存本体（spec §Q1-b 那条「失配即丢弃」已作废）
 *
 * Q1-b 写于**没有合并器的时候**：那时恢复出来的陈旧 staged 只能被整份重放到新盘上，
 * 「陈旧 staged 参与合成 = 埋雷」成立，丢弃是当时唯一的出口。P5 落地 `conflictingEntries` +
 * `replay` 之后，「陈旧 staged 对上新磁盘」正是 Q8-b 四步处理的那件事 —— 而丢弃这条腿
 * 变成了纯粹的**静默数据丢失**：后台写盘者（订阅调度器写 `subscriptions[].etag`、规则资源
 * 调度器写 `ruleResources[].updatedAt`、`enforce_backend_authoritative_fields` 写托盘 MRU）
 * 频繁刷版本，用户暂存 N 条 → 后台写一次盘 → 重载 → N 条无声蒸发。
 *
 * 走冲突腿的代价就是这个字段：冲突检出要拿 baseline 当基准，而基准只能存本体（版本 hash
 * 判得出「变了」，判不出「哪个实体变了」）。
 *
 * # 体积（R4 配额那一栏）
 *
 * 条目侧约 10² 字节/条 × <10 条 ≈ 1KB；加上 baseline 后与 `config.json` 同量级（随节点数
 * 线性，几十 KB 到数百 KB）——**大 1~2 个数量级**。仍远小于 webview 的 MB 级配额，但不再是
 * 「可忽略」：写入失败已不是理论情况，`store` 侧 `persist()` 的 try/catch 是这条腿的兜底
 * （退化成本次不持久化，内存里的 staged 照常可用）。
 *
 * # 为什么不再存 `baseVersion`
 *
 * 它可由 `configBaseVersion(baseline)` 现算，且**失配判定这个唯一消费者已经没了**。
 * 更要命的是它与 baseline 存的从来不是同一件事：store 的 `baseVersion` 跟着**磁盘**走
 * （后台写盘也刷），baseline 在有 staged 期间**冻住** —— 两者本就会不等。留着 = 一个没人读、
 * 又必然与 `configBaseVersion(baseline)` 对不上的字段，日后必有人拿它当基准判一次错。
 *
 * 老载荷（`{baseVersion, entries}`、无 `baseline`）由 `decodeStagedPayload` 判 `null`，
 * 退化成今天的丢弃行为 —— 没有基准就判不了冲突，此时丢弃是唯一诚实的选择。
 */
export interface StagedPayload {
  /**
   * 建立这批条目那一刻的磁盘 config 本体（Q8-b 的 `baseline`）。
   * 解码只能保证「它是个对象」—— 它的字段形状由写入方（本仓 store，写进去的就是一份 `UserConfig`）
   * 负责，与 config 经 IPC 回来时同属未经运行期校验的信任面。
   */
  readonly baseline: Record<string, unknown>;
  readonly entries: StagedEntry[];
}

export function encodeStagedPayload(payload: StagedPayload): string {
  return JSON.stringify({ baseline: payload.baseline, entries: payload.entries });
}

/**
 * 解码。**任何畸形一律返 `null`**（不是抛、不是半份）：入参来自进程外，
 * 「解析不了」与「没存过」对调用方应当是同一种情况——都退化成「没有可恢复的编辑」。
 * 非法条目逐条丢弃，剩余条目仍可恢复（一条坏条目不该让用户其余编辑陪葬）。
 *
 * `baseline` 缺席（老载荷）或非对象走的是**同一条**返 `null` 的腿，不另设分支：两者对调用方
 * 的收场完全一样（没有基准 ⇒ 无从判冲突 ⇒ 只能丢弃），分开写只会多一个能分叉的状态。
 */
export function decodeStagedPayload(raw: string | null | undefined): StagedPayload | null {
  if (typeof raw !== 'string' || raw === '') return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isRecord(parsed)) return null;
  const { baseline, entries } = parsed;
  if (!isRecord(baseline)) return null;
  if (!Array.isArray(entries)) return null;
  return { baseline, entries: entries.filter(isValidStagedEntry) };
}

/**
 * config 的内容版本（Q1-b 的 `baseVersion`）：稳定序列化后取 FNV-1a 32 位短 hash。
 *
 * 不用 mtime（同秒两次写可能相等），不用自增计数（进程重启即失忆）。
 *
 * # 两侧各算，不走 IPC 往返
 *
 * Rust `commands/config.rs::config_content_hash` 是本函数的逐位镜像（同一序列化规则、
 * 同一 FNV 常量、**同一哈希单元 = UTF-16 code unit**）。`config:save` 的乐观并发校验拿它当基准，
 * 故两侧一旦分叉，每一次带 `baseVersion` 的保存都会返 conflict。
 * 由 `contracts/config-version.fixture.json` 的双侧固定 fixture 锁住（`contracts/config-version.test.ts`
 * 与 Rust 侧同名测试读同一个文件）。
 */
export function configBaseVersion(config: unknown): string {
  const text = stableStringify(config);
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) {
    hash ^= text.charCodeAt(i);
    // FNV prime 32 位乘法（Math.imul 避免 JS 双精度溢出丢低位）
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, '0');
}

/** 键序稳定的 JSON 序列化（数组保序 —— 数组顺序在 config 里是语义的一部分）。 */
function stableStringify(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(',')}]`;
  if (isRecord(value)) {
    const body = Object.keys(value)
      .sort()
      .map((k) => `${JSON.stringify(k)}:${stableStringify(value[k])}`)
      .join(',');
    return `{${body}}`;
  }
  return JSON.stringify(value) ?? 'null';
}
