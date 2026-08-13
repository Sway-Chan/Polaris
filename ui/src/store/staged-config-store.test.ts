/**
 * 暂存 store 的门 —— 总开关的零副作用、持久化往返、版本失配丢弃、**存储不可用时的降级**（R4），
 * 以及 Q1-b 清除时机 ④「正常退出标记」这条腿的渲染端半边。
 *
 * 存储降级这一条只能验「不崩、不丢内存里的编辑」。④ 的**后端半边**（标记只在真退出腿落、
 * 重载 / 轻量模式销毁重建不落）本文件判不了：那要一个跑起来的 Tauri 进程。本文件只钉
 * 「拿到真 ⇒ 清、拿到假 ⇒ 恢复、一个 webview 只问一次」，标记本身的读即清由
 * `src-tauri/src/clean_exit.rs` 的单测钉。
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

// IPC mock：`get` / `save` / `applyNow` 是本 store 仅有的三处外部副作用，全部经这三颗桩观测。
// `get` 是 P5 加的：Q8-b 的第一步就是「拉磁盘现值」，保存不再拿调用方给的 config 当磁盘。
const saveMock = vi.fn(
  async (_config: unknown, _deferRestart?: boolean, _baseVersion?: string): Promise<SaveOutcome> => ({
    status: 'saved',
    version: 'v-after-save',
  })
);
const getMock = vi.fn(async (): Promise<UserConfig> => CONFIG);
const applyMock = vi.fn(async () => ({ ok: true, status: 'applied' as const }));
/** Q1-b ④：「上次是不是正常退出」。默认假 = 强杀/重载/没退过 ⇒ 照常恢复（旧行为）。 */
const cleanExitMock = vi.fn(async () => false);
vi.mock('@/ipc', () => ({
  api: {
    config: {
      get: () => getMock(),
      save: (c: unknown, d?: boolean, b?: string) => saveMock(c, d, b),
    },
    proxy: { applyPendingChanges: () => applyMock() },
    window: { takeCleanExitFlag: () => cleanExitMock() },
  },
}));

import { STAGED_CONFIG_ENABLED, STAGED_STORAGE_KEY, configBaseVersion, type StagedEntry } from '@/lib/staged-config';
import type { SaveOutcome, UserConfig } from '@/contracts/types';
import { hydrateStagedConfig, useStagedConfigStore } from './staged-config-store';

const CONFIG = {
  servers: [{ id: 'n1', name: 'n1', port: 443 }],
  subscriptions: [{ id: 's1', url: 'https://x', etag: 'W/\"v1\"' }],
  mixedPort: 7890,
} as unknown as UserConfig;
/** 只改了 `mixedPort` 这个**设置实体**的另一版盘。 */
const OTHER_CONFIG = { ...CONFIG, mixedPort: 1080 } as unknown as UserConfig;

function entry(id: string, port = 443): StagedEntry {
  return {
    id: `server:${id}`,
    kind: 'server',
    label: `编辑节点 ${id}`,
    entityPath: ['servers', id],
    nextValue: { id, name: id, port },
  };
}

/** 可注入的假存储：可让任意方法抛，用来验降级路径。 */
class FakeStorage implements Storage {
  private map = new Map<string, string>();
  throwOn: 'none' | 'get' | 'set' = 'none';
  get length(): number {
    return this.map.size;
  }
  clear(): void {
    this.map.clear();
  }
  getItem(key: string): string | null {
    if (this.throwOn === 'get') throw new Error('storage unavailable');
    return this.map.get(key) ?? null;
  }
  key(index: number): string | null {
    return [...this.map.keys()][index] ?? null;
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
  setItem(key: string, value: string): void {
    if (this.throwOn === 'set') throw new Error('quota exceeded');
    this.map.set(key, value);
  }
}

let fake: FakeStorage;

/** 排空微任务队列：首次 hydrate 要先 await 一次 IPC（Q1-b ④ 的正常退出标记）。 */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function resetStore(enabled: boolean): void {
  useStagedConfigStore.setState({
    enabled,
    entries: [],
    baseVersion: null,
    baseline: null,
    hydrated: false,
    saveStatus: 'idle',
    saveError: null,
    conflict: null,
  });
}

beforeEach(() => {
  fake = new FakeStorage();
  vi.stubGlobal('localStorage', fake);
  saveMock.mockClear();
  saveMock.mockImplementation(async () => ({ status: 'saved', version: 'v-after-save' }));
  getMock.mockClear();
  getMock.mockImplementation(async () => CONFIG);
  applyMock.mockClear();
  applyMock.mockImplementation(async () => ({ ok: true, status: 'applied' as const }));
  cleanExitMock.mockClear();
  cleanExitMock.mockImplementation(async () => false);
  resetStore(true);
});

afterEach(() => {
  vi.unstubAllGlobals();
  resetStore(STAGED_CONFIG_ENABLED);
});

describe('总开关关闭 = 今天行为（零残留副作用）', () => {
  /**
   * 关掉后 store 的四个 action 全是 no-op：不改状态、**不碰存储**。
   *
   * 牙：删掉任一 action 里的 `if (!enabled) return` → 该 action 会写状态或写存储 → 转红。
   * 这条与 `staged-config.test.ts` 的 `editRoute(key,false)` 合起来构成「关掉 = 今天行为」的两半：
   * 那条钉「编辑入口不进暂存」，本条钉「暂存层自己也不动任何东西」。
   */
  it('stage / revert / reset / hydrate 全 no-op，存储零写', () => {
    resetStore(false);
    const s = useStagedConfigStore.getState();
    s.stage(entry('n1'));
    s.revert('server:n1');
    s.reset();
    s.hydrate(CONFIG);
    const after = useStagedConfigStore.getState();
    expect(after.entries).toEqual([]);
    expect(after.baseVersion).toBeNull();
    expect(after.hydrated).toBe(false);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });

  /**
   * 薄封装（app-store 的调用点走的是它，不是 store 方法）关掉时同样连 store 都不碰。
   * 首次 hydrate 现在是异步的（要先问后端「上次怎么退的」），故 flush 之后再断言 ——
   * 否则 `hydrated:false` 会因为「还没轮到」而恒真，这条门就没牙了。
   */
  it('hydrateStagedConfig 在开关为关时连 store 都不碰', async () => {
    resetStore(false);
    hydrateStagedConfig(CONFIG);
    await flush();
    expect(useStagedConfigStore.getState().hydrated).toBe(false);
    expect(cleanExitMock).not.toHaveBeenCalled();
  });

  /**
   * **两个开关必须同源**：`stage`/`revert`/`reset`/`editRoute` 判的都是 store 的 `enabled`，
   * 薄封装若改判编译期常量就造出一个「半开」态 —— 谁在运行期把 `enabled` 置 true
   *（该字段的存在理由正是让单测两侧都跑到），hydrate 却永不执行 ⇒ `baseVersion` 恒 null
   * ⇒ `persist()` 走 `removeItem` 腿 ⇒ 暂存能用但**永不持久化**，还会顺手删掉已有存储项。
   *
   * 牙：把 `hydrateStagedConfig` 的判据改回 `STAGED_CONFIG_ENABLED` → 本条转红。
   * 生产行为不受影响：store 的 `enabled` 初值就取自那个常量。
   */
  it('开关打开时薄封装真的执行（与 stage/revert/reset 同源，不留半开态）', async () => {
    resetStore(true);
    hydrateStagedConfig(CONFIG);
    await flush();
    const after = useStagedConfigStore.getState();
    expect(after.hydrated).toBe(true);
    expect(after.baseVersion).not.toBeNull();
  });
});

describe('暂存 + 持久化往返', () => {
  it('stage 写状态并落存储；重置清空存储', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1', 8443));
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toContain('server:n1');

    useStagedConfigStore.getState().reset();
    expect(useStagedConfigStore.getState().entries).toEqual([]);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });

  it('撤销最后一条 ⇒ 存储项被移除（不留空数组残渣）', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1'));
    useStagedConfigStore.getState().revert('server:n1');
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });

  it('非法条目：不入表、不写存储', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage({ id: 'x', kind: 'server', label: '', entityPath: ['servers', 'n1'], nextValue: { id: 'n2' } });
    expect(useStagedConfigStore.getState().entries).toEqual([]);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });
});

describe('hydrate —— 重载恢复与版本失配（T1-17）', () => {
  /** NFR-1：webview 自愈重载后 staged 计数不变。模拟「重载」= store 重建 + 同一份存储。 */
  it('版本相符 ⇒ 恢复条目', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1', 8443));

    resetStore(true); // 模拟渲染端重载：store 归零，存储还在
    useStagedConfigStore.getState().hydrate(CONFIG);
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
    expect(useStagedConfigStore.getState().baseVersion).toBe(configBaseVersion(CONFIG));
  });

  /**
   * **本轮修掉的数据丢失路径**（spec §Q1-b「失配即丢弃」已随 P5 作废）。
   *
   * 旧行为：版本失配 ⇒ `restored = []`，用户暂存的编辑被静默清零、无任何提示。而后台写盘者
   *（订阅调度器写 `subscriptions[].etag`、规则资源调度器写 `ruleResources[].updatedAt`、
   * `enforce_backend_authoritative_fields` 写托盘 MRU）刷版本极频繁 ⇒ 暂存 N 条 + 后台写一次盘
   * + 一次重载 = N 条无声蒸发。「陈旧 staged 对上新磁盘」正是 P5 的 `conflictingEntries` + `replay`
   * 要处理的那件事（Q8-b 四步），不该在恢复腿上提前丢掉。
   *
   * 牙 ①：把恢复腿改回「失配即 `[]`」→ `entries` 为空 → 转红。
   * 牙 ②：把恢复出来的 baseline 换成**当前盘值** `config` → `baseline` 成了 OTHER_CONFIG → 转红
   *（那会让冲突检出两侧恒等 ⇒ 永远判不出冲突 ⇒ 静默吃掉别人的改动）。
   */
  it('版本失配 ⇒ 条目保留，基准取存储里那份 baseline（不再静默丢弃）', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1', 8443));

    resetStore(true); // 模拟重载：store 归零，存储还在
    useStagedConfigStore.getState().hydrate(OTHER_CONFIG); // 盘在此期间被后台改过
    const after = useStagedConfigStore.getState();
    expect(after.entries).toHaveLength(1);
    expect(after.baseline).toEqual(CONFIG);
    expect(after.baseVersion).toBe(configBaseVersion(OTHER_CONFIG));
    // 存储里那份也得留着 —— 清掉等于下次重载再丢一次。
    expect(fake.getItem(STAGED_STORAGE_KEY)).toContain('server:n1');
  });

  /**
   * 老形态载荷（`{baseVersion, entries}`、无 `baseline`）⇒ **退回今天的丢弃行为**并清除残留。
   * 没有基准就判不了冲突；带一个凭空捏造的空基准去恢复，只会把每个实体都判成「磁盘侧变了」。
   * 这里用的是**版本相符**的老载荷，把「因为缺 baseline 而丢」与「因为版本」彻底分开。
   *
   * 牙：让 `decodeStagedPayload` 在缺 `baseline` 时兜一个 `{}` 而不是返 `null` → 条目被恢复 → 转红。
   */
  it('老形态载荷（无 baseline）⇒ 丢弃并清除存储', () => {
    fake.setItem(
      STAGED_STORAGE_KEY,
      JSON.stringify({ baseVersion: configBaseVersion(CONFIG), entries: [entry('n1', 8443)] })
    );
    useStagedConfigStore.getState().hydrate(CONFIG);
    const after = useStagedConfigStore.getState();
    expect(after.entries).toEqual([]);
    expect(after.baseline).toEqual(CONFIG);
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });

  /**
   * 恢复只发生一次：此后 config 重载（订阅调度器写盘等）只把 baseVersion 跟到新盘值，
   * **不动用户正在编辑的条目**。
   *
   * 牙：把 `hydrated` 闸门删掉 → 后台一次写盘就把用户 staged 全清 → 转红。
   */
  it('已 hydrate 后 config 变更只刷 baseVersion，不清内存条目', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1', 8443));

    useStagedConfigStore.getState().hydrate(OTHER_CONFIG);
    const after = useStagedConfigStore.getState();
    expect(after.entries).toHaveLength(1);
    expect(after.baseVersion).toBe(configBaseVersion(OTHER_CONFIG));
    // 存储里那份**不跟着盘走**：载荷里已经没有随盘刷新的字段（baseline 冻着、条目没动），
    // 后台每写一次盘就回写一次同量级载荷只会白白逼近配额。
    // 牙：让 hydrate 的已 hydrate 腿拿当前 config 回写存储 → 下面 baseline 变成 OTHER_CONFIG → 转红。
    const stored = JSON.parse(fake.getItem(STAGED_STORAGE_KEY) as string);
    expect(stored.baseline).toEqual(CONFIG);
    expect(stored.entries).toHaveLength(1);
  });

  /**
   * P5 补的另一半（T1-15）：`baseVersion` 跟着盘走，**`baseline` 却必须冻住**。
   *
   * 两者管的不是同一件事 —— 前者管「存储里那份 staged 还新不新鲜」，后者管「这批编辑是相对
   * 哪一版盘做的」。让 baseline 也跟着盘走 ⇒ 冲突检出的两侧恒等 ⇒ 永远判不出冲突 ⇒
   * 别人的改动被静默吃掉。
   *
   * 牙：把 hydrate 里的 `entries.length === 0 ? {...baseline} : {...}` 改成无条件带上 baseline
   * → 本条转红。
   */
  it('有 staged 时 baseline 冻在建立那一刻，不跟着盘走', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    s.stage(entry('n1', 8443));

    useStagedConfigStore.getState().hydrate(OTHER_CONFIG);
    expect(useStagedConfigStore.getState().baseline).toEqual(CONFIG);

    // 没有 staged 时反过来：baseline 必须跟到新盘值，否则下一批编辑会拿一份陈旧基准。
    useStagedConfigStore.getState().reset();
    useStagedConfigStore.getState().hydrate(OTHER_CONFIG);
    expect(useStagedConfigStore.getState().baseline).toEqual(OTHER_CONFIG);
  });
});

describe('R4 降级：存储不可用不得崩、不得丢内存里的编辑', () => {
  /**
   * 载荷带上 baseline 后单次写入与 `config.json` 同量级（大 1~2 个数量级），配额超已不是理论情况。
   * 硬约束：写不进去 ⇒ **只退化成本次不持久化**，状态一个字段都不许动。
   *
   * 牙：把 `persist` 的 catch 改成清状态（哪怕只清 `baseline`）→ 下面两条断言之一转红；
   * 把 catch 去掉 → `stage` 抛穿到调用方（编辑入口）→ 第一条转红。
   */
  it('setItem 抛（配额满/隐私模式）⇒ 条目与 baseline 仍在内存里', () => {
    const s = useStagedConfigStore.getState();
    s.hydrate(CONFIG);
    fake.throwOn = 'set';
    expect(() => useStagedConfigStore.getState().stage(entry('n1', 8443))).not.toThrow();
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
    // baseline 是保存腿唯一的比对基准；持久化失败若顺手把它清了，这批编辑就再也保存不出去。
    expect(useStagedConfigStore.getState().baseline).toEqual(CONFIG);
  });

  it('getItem 抛 ⇒ hydrate 退化成「没有可恢复的编辑」，不抛', () => {
    fake.throwOn = 'get';
    expect(() => useStagedConfigStore.getState().hydrate(CONFIG)).not.toThrow();
    const after = useStagedConfigStore.getState();
    expect(after.hydrated).toBe(true);
    expect(after.entries).toEqual([]);
  });

  it('运行环境根本没有 localStorage ⇒ 全链路不抛，退化为会话内记忆', () => {
    vi.stubGlobal('localStorage', undefined);
    const s = useStagedConfigStore.getState();
    expect(() => {
      s.hydrate(CONFIG);
      useStagedConfigStore.getState().stage(entry('n1', 8443));
      useStagedConfigStore.getState().revert('server:n1');
      useStagedConfigStore.getState().reset();
    }).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// P4 三动作 —— 「重置 / 保存 / 立即应用」。硬约束只有一条但贯穿全部失败腿：**staged 绝不丢**（NFR-1）。
// ---------------------------------------------------------------------------

/** 装好一条 staged：hydrate（拿到 baseVersion + baseline）→ stage。 */
function seedStaged(port = 8443): void {
  useStagedConfigStore.getState().hydrate(CONFIG);
  useStagedConfigStore.getState().stage(entry('n1', port));
}

describe('save —— 落盘但不排重启（FR-5）', () => {
  /**
   * FR-5 的全部技术含量就在第二个实参上：不传 `deferRestart` 时后端走今天的路径（落盘即去抖重启），
   * 「保存」与「立即应用」的语义差会整个消失、后者沦为装饰（Q4 结论 3）。
   * 变异对照：把 `api.config.save(cfg, true)` 的 `true` 去掉或改成 `false` → 本条转红。
   */
  it('调 config.save 且 deferRestart === true', async () => {
    seedStaged();
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(saveMock).toHaveBeenCalledTimes(1);
    expect(saveMock.mock.calls[0][1]).toBe(true);
  });

  /**
   * 落盘的是 replay 出来的 effectiveConfig，不是磁盘现值本身。
   * 变异对照：把 `replay(disk, entries)` 换成 `disk` → 本条转红（用户的编辑一个都没进磁盘，
   * 而 UI 会照常清空 staged 报成功 —— 这是最坏的一种「静默丢编辑」）。
   */
  it('落盘的是磁盘现值 + staged 重放后的结果', async () => {
    seedStaged(9443);
    await useStagedConfigStore.getState().save();
    const written = saveMock.mock.calls[0][0] as { servers: { id: string; port: number }[] };
    expect(written.servers.find((s) => s.id === 'n1')?.port).toBe(9443);
  });

  /**
   * 成功后清 staged + 清存储 + 回 idle。
   * 变异对照：删掉成功腿的 `set({ entries: [] })` → 本条转红（这批意图已经是磁盘现值，
   * 留着会让「N 项待保存」永不归零，且下次保存重复施加）。
   */
  it('成功 ⇒ 清空条目、清除存储、回 idle', async () => {
    seedStaged();
    expect(fake.getItem(STAGED_STORAGE_KEY)).toContain('server:n1');
    await useStagedConfigStore.getState().save();
    const after = useStagedConfigStore.getState();
    expect(after.entries).toEqual([]);
    expect(after.saveStatus).toBe('idle');
    expect(after.saveError).toBeNull();
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });

  /**
   * NFR-1：落盘失败 ⇒ 条目一条不丢，原因如实带出。
   * 变异对照：在 catch 腿里加一句 `set({ entries: [] })`（或把 catch 改成吞掉不改状态）→ 本条转红。
   */
  it('落盘失败 ⇒ staged 全保留 + saveFailed + 带出原因', async () => {
    seedStaged();
    saveMock.mockImplementation(async () => {
      throw new Error('EACCES: config.json 只读');
    });
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    const after = useStagedConfigStore.getState();
    expect(after.entries).toHaveLength(1);
    expect(after.saveStatus).toBe('saveFailed');
    expect(after.saveError).toContain('EACCES');
    // 存储里那份也得留着：失败后用户重载 webview 不该把编辑一起丢掉。
    expect(fake.getItem(STAGED_STORAGE_KEY)).toContain('server:n1');
  });

  /**
   * 没有 staged ⇒ 判成功且**不落盘**。这条腿是 `clean × pending` 那一格「立即应用」的通路。
   * 变异对照：去掉 `entries.length === 0` 短路 → 每次点「立即应用」都会先无谓地整份重写一次
   * config.json（并触发一次 switch_mode）→ 本条转红。
   */
  it('没有 staged ⇒ 直接判成功，一次 IPC 都不发', async () => {
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(saveMock).not.toHaveBeenCalled();
  });

  /**
   * baseline 缺席（`hydrate` 还没跑过 = config 还没到）⇒ 报失败保住条目，绝不落半份。
   * 变异对照：删掉 `baseline === null` 那道判 → 没有比对基准就无从判冲突，等于对一份来历不明的
   * 磁盘现值做无条件整份覆盖 → 本条转红。
   */
  it('baseline 为 null 且有 staged ⇒ 判失败、不落盘、条目保留', async () => {
    useStagedConfigStore.getState().stage(entry('n1', 8443));
    expect(useStagedConfigStore.getState().baseline).toBeNull();
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    expect(saveMock).not.toHaveBeenCalled();
    expect(getMock).not.toHaveBeenCalled();
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
    expect(useStagedConfigStore.getState().saveStatus).toBe('saveFailed');
  });

  // 总开关关 ⇒ 与今天逐字节相同：不落盘、不改状态。
  // 变异对照：删掉 save 里的 `!enabled` 判 → 转红。
  it('总开关关 ⇒ 判成功但零 IPC', async () => {
    resetStore(false);
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(saveMock).not.toHaveBeenCalled();
  });
});

describe('applyNow —— 保存 + force-restart（FR-6）', () => {
  it('保存过关 ⇒ 调 applyPendingChanges 并回传排程结果', async () => {
    seedStaged();
    await expect(useStagedConfigStore.getState().applyNow()).resolves.toEqual({
      saved: true,
      status: 'applied',
    });
    expect(saveMock).toHaveBeenCalledTimes(1);
    expect(applyMock).toHaveBeenCalledTimes(1);
  });

  /**
   * 保存没过 ⇒ **核一下都不能碰**。
   * 变异对照：把 `if (!(await get().save(baseline)))` 那道闸去掉 → 落盘失败后仍会 force-restart：
   * 断流几秒，换来的是核重新读到那份**没被改过**的磁盘 config，即零变化。本条转红。
   */
  it('保存失败 ⇒ 不调 applyPendingChanges，且如实告知没保存', async () => {
    seedStaged();
    saveMock.mockImplementation(async () => {
      throw new Error('disk full');
    });
    await expect(useStagedConfigStore.getState().applyNow()).resolves.toEqual({
      saved: false,
      status: null,
    });
    expect(applyMock).not.toHaveBeenCalled();
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
  });

  /**
   * `clean × pending` 那一格：没有 staged 时「立即应用」照样要把已落盘的差集推进核。
   * 变异对照：把 save 的空条目腿改成返 `false` → 这条路被闸掉，条上那颗按钮永远不动核 → 本条转红。
   */
  it('没有 staged ⇒ 不落盘但照常 apply（今天那条路不能断）', async () => {
    await expect(useStagedConfigStore.getState().applyNow()).resolves.toEqual({
      saved: true,
      status: 'applied',
    });
    expect(saveMock).not.toHaveBeenCalled();
    expect(applyMock).toHaveBeenCalledTimes(1);
  });

  // apply 的 IPC 失败 ⇒ status 归 null 由调用方点红，不把异常抛给渲染路径（抛出去 = 白屏）。
  // 变异对照：把 `.catch(() => null)` 去掉 → 本条从「resolves」变成「rejects」→ 转红。
  it('applyPendingChanges 抛 ⇒ status 归 null，不外抛', async () => {
    applyMock.mockImplementation(async () => {
      throw new Error('ipc down');
    });
    await expect(useStagedConfigStore.getState().applyNow()).resolves.toEqual({
      saved: true,
      status: null,
    });
  });
});

/**
 * **并发保存不得产出「盘存好了、条说失败了」**（陈先生点名的竞态）。
 *
 * `performSave` 是一段 read-modify-write（`config.get()` → 重放 → `config.save(…, diskVersion)`），
 * 两个 `await` 之间此前没有互斥。两次交错的保存读到**同一个** `diskVersion`，先到的写成功并把盘
 * 版本顶高，后到的被后端乐观并发闸判 `conflict`、一个字节都不写，前端把它落成 `saveFailed`
 * —— 而 staged 已被先到那次清空。净效果：**存成功了、条说失败了、且没有东西可重试**。
 *
 * 这里的 `saveMock` **必须**忠实模拟后端 `config_save_core` 的那道闸（`base_version != 盘版本
 * ⇒ Conflict 且不写`）。用恒返 `saved` 的默认桩测这件事是没有信息量的：无论串行与否都绿。
 */
describe('并发保存（withConfigWriteLock）', () => {
  /** 装一个带版本的假盘 + 忠实的乐观并发闸。 */
  function installVersionedDisk(): { writes: number } {
    let disk = CONFIG;
    const counter = { writes: 0 };
    getMock.mockImplementation(async () => disk);
    saveMock.mockImplementation(async (cfg, _defer, base) => {
      // 后端 `config_save_core`：只有传了 base 才校验；不符即 Conflict 且**不写**。
      if (base !== undefined && base !== configBaseVersion(disk)) {
        return { status: 'conflict', diskVersion: configBaseVersion(disk) };
      }
      disk = cfg as UserConfig;
      counter.writes += 1;
      return { status: 'saved', version: configBaseVersion(disk) };
    });
    return counter;
  }

  /**
   * 变异对照（实跑过）：把 `performSave` 的 `withConfigWriteLock(...)` 去掉、直接
   * `return performSaveLocked(...)` → 两次并发各自带着同一个旧版本提交 → 第二次拿 conflict
   * → `saveStatus` 变 `'saveFailed'` → 本条转红。
   */
  it('同时点两次保存 ⇒ 只写一次盘，且绝不出现 saveFailed', async () => {
    const disk = installVersionedDisk();
    seedStaged();
    const s = useStagedConfigStore.getState();
    const [a, b] = await Promise.all([s.save(), s.save()]);

    expect([a, b]).toEqual([true, true]);
    expect(disk.writes).toBe(1); // 第二次进临界区时 entries 已空 ⇒ 无害 no-op，不是第二次落盘
    expect(useStagedConfigStore.getState().saveStatus).toBe('idle');
    expect(useStagedConfigStore.getState().saveError).toBeNull();
    expect(useStagedConfigStore.getState().entries).toHaveLength(0);
  });

  /** `applyNow` 与 `save` 交错走的是同一条队列（`applyNow` 的前半就是 `save`）。 */
  it('save 与 applyNow 同时发 ⇒ 同样只写一次、无假失败', async () => {
    const disk = installVersionedDisk();
    seedStaged();
    const s = useStagedConfigStore.getState();
    const [saved, applied] = await Promise.all([s.save(), s.applyNow()]);

    expect(saved).toBe(true);
    expect(applied.saved).toBe(true);
    expect(disk.writes).toBe(1);
    expect(useStagedConfigStore.getState().saveStatus).toBe('idle');
  });

  /**
   * **正向对照**：闸门只串行，不该把真正的外部冲突吞掉。别人（托盘 / 另一个窗口 / 后台写盘）
   * 在两次 IPC 之间改了盘 ⇒ 那是真冲突，必须照实报 `saveFailed` 且**一条 staged 都不丢**（NFR-1），
   * 用户点「重试保存」会基于新盘重放。少了这条，上面两条可以靠「永远不报冲突」作弊通过。
   */
  it('外部写盘造成的真冲突仍照实报，且 staged 一条不丢', async () => {
    seedStaged();
    getMock.mockImplementation(async () => CONFIG);
    saveMock.mockImplementation(async () => ({
      status: 'conflict',
      diskVersion: configBaseVersion(OTHER_CONFIG),
    }));
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    expect(useStagedConfigStore.getState().saveStatus).toBe('saveFailed');
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
  });
});

describe('reset —— 「重置」同时是条上唯一的清红出口', () => {
  /**
   * 保存失败后点「重置」：条目丢掉，红也必须跟着灭。
   * 变异对照：把 `set({ entries: [], saveStatus: 'idle', saveError: null })` 里的后两项去掉 →
   * 条目没了、条却永远停在「保存失败」且再没有任何按钮能改变它 → 本条转红。
   */
  it('saveFailed 态下重置 ⇒ 条目清空且红态清除', async () => {
    seedStaged();
    saveMock.mockImplementation(async () => {
      throw new Error('boom');
    });
    await useStagedConfigStore.getState().save();
    expect(useStagedConfigStore.getState().saveStatus).toBe('saveFailed');

    useStagedConfigStore.getState().reset();
    const after = useStagedConfigStore.getState();
    expect(after.entries).toEqual([]);
    expect(after.saveStatus).toBe('idle');
    expect(after.saveError).toBeNull();
    expect(fake.getItem(STAGED_STORAGE_KEY)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// P5 乐观并发 + 自动合并 + 冲突弹窗（spec §2.5 Q8-b）。
//
// 全部经 store 的 `save` 生产路径驱动 —— 若测试自己调 `conflictingEntries` 再断言，
// 「store 里那条腿被删掉」对测试是恒绿的（同 config_save_core 那条纪律）。
// ---------------------------------------------------------------------------

/** 磁盘侧只动了**别的实体**（订阅调度器写 etag）——Q8-b 闸 3 的真实形态。 */
const DISK_OTHER_ENTITY = {
  ...CONFIG,
  subscriptions: [{ id: 's1', url: 'https://x', etag: 'W/"v2"' }],
} as unknown as UserConfig;

/** 磁盘侧动了**同一个节点**（另一个窗口/CLI 改了 n1）。 */
const DISK_SAME_ENTITY = {
  ...CONFIG,
  servers: [{ id: 'n1', name: 'n1-renamed', port: 443 }],
} as unknown as UserConfig;

describe('save —— Q8-b 四步（拉盘 → 算冲突 → 合并 → 落盘）', () => {
  /**
   * 第一步就是**重新拉磁盘现值**，不是拿 store 里的 baseVersion 当现值。
   *
   * 因果：§Q8 明确记着有一条**不广播**的落盘（`subscription.rs` 落验证器时特意不发
   * `event:configChanged`，避免触发 switch_mode 断流）⇒ 那次写盘之后 store 的 `baseVersion`
   * 仍是旧的。拿它当基准 = 结构性漏检。
   *
   * 牙：把 `await api.config.get()` 换成用 `get().baseVersion` ⇒ 第一个断言转红。
   */
  it('先拉磁盘现值，再落盘，且带上刚算出来的 baseVersion', async () => {
    seedStaged();
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(getMock).toHaveBeenCalledTimes(1);
    expect(saveMock).toHaveBeenCalledTimes(1);
    expect(saveMock.mock.calls[0][2]).toBe(configBaseVersion(CONFIG));
  });

  /**
   * **自动合并腿**（Q8-b 第 3 步）：磁盘变了、但变的是别的实体 ⇒ 静默合并，用户无感。
   *
   * 落盘内容必须是 `replay(disk, entries)` —— 既带上用户的编辑，也**保住磁盘侧那条新 etag**。
   * 牙：把 `replay(disk, entries)` 换回 `replay(baseline, entries)` ⇒ 第三个断言转红
   * （订阅调度器刚写的 etag 被用户的一次保存抹回旧值 ⇒ 机场按 UA 下发变体时恒 304）。
   */
  it('磁盘变了但实体不重叠 ⇒ 静默合并落盘，不弹窗、不丢磁盘侧的改动', async () => {
    seedStaged(9443);
    getMock.mockImplementation(async () => DISK_OTHER_ENTITY);

    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    const written = saveMock.mock.calls[0][0] as unknown as {
      servers: { id: string; port: number }[];
      subscriptions: { etag: string }[];
    };
    expect(written.servers.find((s) => s.id === 'n1')?.port).toBe(9443);
    expect(written.subscriptions[0].etag).toBe('W/"v2"');
    expect(saveMock.mock.calls[0][2]).toBe(configBaseVersion(DISK_OTHER_ENTITY));
  });

  /**
   * **冲突腿**（Q8-b 第 4 步）：同一实体两边都动过 ⇒ 弹窗，**一个字节都不落盘**，staged 一条不丢。
   *
   * 牙：把冲突集非空那条 `return false` 删掉（继续往下落盘）⇒ 第二个断言转红 ——
   * 那正是「静默吃掉另一个人的改动」，NFR-2 违反。
   */
  it('同一实体两边都动过 ⇒ 开弹窗、不落盘、条目保留', async () => {
    seedStaged(9443);
    getMock.mockImplementation(async () => DISK_SAME_ENTITY);

    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    const after = useStagedConfigStore.getState();
    expect(saveMock).not.toHaveBeenCalled();
    expect(after.entries).toHaveLength(1);
    expect(after.conflict).toHaveLength(1);
    expect(after.conflict?.[0].entryId).toBe('server:n1');
    // 两侧值都要给出来，否则用户没有判断依据。
    expect(after.conflict?.[0].mine).toContain('9443');
    expect(after.conflict?.[0].disk).toContain('n1-renamed');
    // 弹窗盖住了条，条不该同时转圈（否则关掉弹窗会留一条永远转下去的「保存中…」）。
    expect(after.saveStatus).toBe('idle');
  });

  /** 磁盘侧把该实体删了 ⇒ 也是冲突，且 `disk` 侧如实报「已删除」而不是空串。 */
  it('磁盘侧删了该实体 ⇒ 冲突且 disk 侧为 null（弹窗显示「已删除」）', async () => {
    seedStaged(9443);
    getMock.mockImplementation(async () => ({ ...CONFIG, servers: [] }) as unknown as UserConfig);

    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    expect(useStagedConfigStore.getState().conflict?.[0].disk).toBeNull();
  });

  /**
   * `config.get` 抛（IPC 降级）⇒ 报失败保住条目，**绝不**退化成「拿 baseline 当磁盘现值硬写」。
   * 牙：把 `get` 的 catch 改成回落 `baseline` ⇒ 本条转红（那等于在看不见磁盘的情况下整份覆盖）。
   */
  it('拉磁盘现值失败 ⇒ saveFailed、不落盘、条目保留', async () => {
    seedStaged();
    getMock.mockImplementation(async () => {
      throw new Error('ipc down');
    });
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    expect(saveMock).not.toHaveBeenCalled();
    expect(useStagedConfigStore.getState().entries).toHaveLength(1);
    expect(useStagedConfigStore.getState().saveError).toContain('ipc down');
  });

  /**
   * **后端判冲突**（TOCTOU：拉盘与写盘之间又有人写了）⇒ saveFailed，**不自动重试**。
   *
   * 自动重试要么无界（活锁），要么就得再加一层「重试了几次」的状态；而「点[重试保存]」本身
   * 就会把整条腿（含重新拉盘 + 冲突检出）从头跑一遍，是更诚实也更可控的出口。
   *
   * 牙：把 `conflict` 那条腿当成功处理（清空 entries）⇒ 第二、三个断言转红 ——
   * 那是最坏的一种静默丢编辑：一个字节都没写，UI 却报「已保存」并把条目清了。
   */
  it('后端返 conflict ⇒ saveFailed、条目保留、不自动重试', async () => {
    seedStaged();
    saveMock.mockImplementation(async () => ({ status: 'conflict', diskVersion: 'v-newer' }));

    await expect(useStagedConfigStore.getState().save()).resolves.toBe(false);
    const after = useStagedConfigStore.getState();
    expect(saveMock).toHaveBeenCalledTimes(1);
    expect(after.entries).toHaveLength(1);
    expect(after.saveStatus).toBe('saveFailed');
    expect(fake.getItem(STAGED_STORAGE_KEY)).toContain('server:n1');
  });

  /**
   * **T1-16 合并后刷锚点**：落盘成功后 `baseVersion` / `baseline` 必须跟到落盘后的新值。
   *
   * 不刷会怎样：下一批 staged 拿着**上一批之前**的 baseline 去比对，于是「用户自己刚保存的
   * 那次改动」被认成「磁盘侧变了」⇒ 第二次保存平白弹一次冲突窗，而两侧其实都是用户自己。
   *
   * 牙：删掉成功腿里的 `baseVersion: outcome.version` / `baseline: replay(disk, entries)`
   * ⇒ 第二次保存的 `conflict` 非空 → 转红。
   */
  it('落盘成功 ⇒ 锚点刷到新版本，同一批 staged 的后继编辑不再走合并腿', async () => {
    seedStaged(8443);
    await useStagedConfigStore.getState().save();
    const merged = saveMock.mock.calls[0][0] as unknown as UserConfig;
    expect(useStagedConfigStore.getState().baseVersion).toBe('v-after-save');
    expect(useStagedConfigStore.getState().baseline).toEqual(merged);

    // 第二批：磁盘现值 = 刚落盘那份（前端还没收到 configChanged 回程）。
    getMock.mockImplementation(async () => merged);
    useStagedConfigStore.getState().stage(entry('n1', 9443));
    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    expect(saveMock).toHaveBeenCalledTimes(2);
  });

  /**
   * **恢复腿 × 合并腿的收官对照**：暂存 → 后台写盘（订阅 etag）→ 重载恢复 → 保存。
   * 这条把「不再按版本丢弃」串到它的落点上：恢复回来的陈旧 staged 正是靠 Q8-b 闸 3
   *（后台写盘者与用户编辑的实体天然不重叠）静默合并的，两侧一个都不丢。
   *
   * 牙 ①：恢复腿改回「失配即丢」⇒ `entries` 为空 ⇒ `save` 走短路、`saveMock` 一次都不调
   *      ⇒ `calls[0]` 为 undefined → 转红。
   * 牙 ②：`persist` 不写 baseline（回到只存 `baseVersion` 的老契约）⇒ 恢复腿拿不到基准
   *      ⇒ 同样退化成丢弃 → 转红。
   */
  it('后台写盘 → 重载恢复 → 保存：用户暂存与后台改动都不丢', async () => {
    useStagedConfigStore.getState().hydrate(CONFIG);
    useStagedConfigStore.getState().stage(entry('n1', 9443));

    resetStore(true); // 重载：store 归零，存储还在
    getMock.mockImplementation(async () => DISK_OTHER_ENTITY);
    useStagedConfigStore.getState().hydrate(DISK_OTHER_ENTITY); // 冷启看到的是后台写过的盘

    await expect(useStagedConfigStore.getState().save()).resolves.toBe(true);
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    const written = saveMock.mock.calls[0][0] as unknown as {
      servers: { id: string; port: number }[];
      subscriptions: { etag: string }[];
    };
    expect(written.servers.find((s) => s.id === 'n1')?.port).toBe(9443);
    expect(written.subscriptions[0].etag).toBe('W/"v2"');
  });
});

describe('冲突弹窗的落定（resolveConflict / dismissConflict）', () => {
  /** 把 store 推到「弹窗开着」的状态。 */
  async function seedConflict(): Promise<void> {
    seedStaged(9443);
    useStagedConfigStore.getState().stage({
      id: 'setting:mixedPort',
      kind: 'setting',
      label: '改混合端口',
      entityPath: ['mixedPort'],
      nextValue: 1080,
    });
    getMock.mockImplementation(async () => DISK_SAME_ENTITY);
    await useStagedConfigStore.getState().save();
  }

  it('前提：弹窗只列冲突的那一条，不冲突的条目不进来', async () => {
    await seedConflict();
    expect(useStagedConfigStore.getState().conflict?.map((c) => c.entryId)).toEqual(['server:n1']);
  });

  /**
   * 「用我的」⇒ 该条目保留并落盘；「用磁盘的」⇒ 该条目**从 staged 里彻底移除**。
   *
   * 移除而不是「这次跳过」：留着它只会在下一次保存把同一个问题再问一遍，而用户已经答过了。
   * 牙：把 `entries.filter(keep)` 改成只过滤本次落盘的 payload、不改 store 的 entries
   * ⇒ 最后一个断言转红（条目数不降，条上「N 项待保存」赖着不走）。
   */
  it('选「用我的」⇒ 落盘含我的值；未选中的条目被丢弃', async () => {
    await seedConflict();
    await expect(
      useStagedConfigStore.getState().resolveConflict(['server:n1'])
    ).resolves.toBe(true);

    const written = saveMock.mock.calls[0][0] as unknown as {
      servers: { id: string; port: number }[];
      mixedPort: number;
    };
    expect(written.servers.find((s) => s.id === 'n1')?.port).toBe(9443);
    expect(written.mixedPort).toBe(1080);
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    expect(useStagedConfigStore.getState().entries).toEqual([]);
  });

  /**
   * 选「用磁盘的」⇒ 该条目丢弃、**不覆盖磁盘侧的值**；其余条目照常落盘。
   * 牙：把过滤方向写反（保留未选中的）⇒ 第一个断言转红。
   */
  it('选「用磁盘的」⇒ 该实体保持磁盘现值，其余条目照常落盘', async () => {
    await seedConflict();
    await expect(
      useStagedConfigStore.getState().resolveConflict(['setting:mixedPort'])
    ).resolves.toBe(true);

    const written = saveMock.mock.calls[0][0] as unknown as {
      servers: { id: string; name: string }[];
      mixedPort: number;
    };
    expect(written.servers.find((s) => s.id === 'n1')?.name).toBe('n1-renamed');
    expect(written.mixedPort).toBe(1080);
  });

  /**
   * 裁决之后**不再问第二遍**：这一轮的冲突刚被用户亲手选过，再检一次必然又是同一批
   * （用户选的就是「覆盖」）⇒ 会卡成一个关不掉的弹窗循环。
   *
   * 牙：把 `performSave(set, get, true)` 的第三参改成 `false` ⇒ 本条转红（弹窗又开回来了）。
   */
  it('裁决后重走保存不再触发第二轮冲突检出', async () => {
    await seedConflict();
    await useStagedConfigStore.getState().resolveConflict(['server:n1']);
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    expect(saveMock).toHaveBeenCalledTimes(1);
  });

  /**
   * 冲突项全选「用磁盘的」⇒ 冲突项丢弃，**没冲突的条目照常落盘**。
   *
   * 这条与下一条合起来钉住「裁决只作用于冲突集」这个射程：按 `keepEntryIds` 直接过滤整张表
   * 会把同一批里没被问过的编辑一起丢掉 —— 用户没被告知、也没有任何 UI 痕迹，是纯静默丢编辑。
   * 牙：把 `resolveConflict` 的过滤改回 `entries.filter((e) => keep.has(e.id))` ⇒ 本条转红。
   */
  it('冲突项全选「用磁盘的」⇒ 只丢冲突项，其余条目照常落盘', async () => {
    await seedConflict();
    await expect(useStagedConfigStore.getState().resolveConflict([])).resolves.toBe(true);
    const written = saveMock.mock.calls[0][0] as unknown as {
      servers: { id: string; name: string }[];
      mixedPort: number;
    };
    expect(written.servers.find((s) => s.id === 'n1')?.name).toBe('n1-renamed');
    expect(written.mixedPort).toBe(1080);
    expect(useStagedConfigStore.getState().entries).toEqual([]);
  });

  /** 冲突项是唯一条目、又选了「用磁盘的」⇒ 没有要落盘的东西了，判成功且零 IPC。 */
  it('唯一条目被选「用磁盘的」⇒ 不落盘、判成功、staged 归零', async () => {
    seedStaged(9443);
    getMock.mockImplementation(async () => DISK_SAME_ENTITY);
    await useStagedConfigStore.getState().save();
    expect(useStagedConfigStore.getState().conflict).toHaveLength(1);

    await expect(useStagedConfigStore.getState().resolveConflict([])).resolves.toBe(true);
    expect(saveMock).not.toHaveBeenCalled();
    expect(useStagedConfigStore.getState().entries).toEqual([]);
  });

  /**
   * 取消（ESC / scrim / X）⇒ 只关窗，**staged 一条不丢**（NFR-1）。
   * 牙：让 `dismissConflict` 顺手清 entries ⇒ 第二个断言转红。
   */
  it('取消 ⇒ 关窗但条目全留、不落盘', async () => {
    await seedConflict();
    useStagedConfigStore.getState().dismissConflict();
    expect(useStagedConfigStore.getState().conflict).toBeNull();
    expect(useStagedConfigStore.getState().entries).toHaveLength(2);
    expect(saveMock).not.toHaveBeenCalled();
  });

  /** 「重置」也是弹窗的出口之一：条目清空 + 窗关上（否则留一个指向已不存在条目的弹窗）。 */
  it('重置 ⇒ 条目清空且弹窗关闭', async () => {
    await seedConflict();
    useStagedConfigStore.getState().reset();
    const after = useStagedConfigStore.getState();
    expect(after.entries).toEqual([]);
    expect(after.conflict).toBeNull();
  });
});
