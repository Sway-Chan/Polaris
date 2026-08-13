/**
 * 首页测速**射程**不变量守卫 —— 钉死「网络检测 = 只测当前出口」与「全部测速 = 仍是全量」的分工。
 *
 * # 为什么是源码结构守卫，而不是又一组逻辑单测
 *
 * 被守的东西不在算法里：`speedTestableIds` / `speedTestBlockReason` 都对、都有自己的单测
 * （`domain/speed-testable-ids.test.ts`、`nodes-logic.test.ts`），缺陷是**调用点选错了集合**。
 * 「网络检测」这颗按钮此前把延迟腿接到 `speedTestableIds(servers, …)`（全量），而它的另两条腿
 * （解锁重检、出口 IP 重探）只针对当前出口 —— 同一次点击两种射程，且与出口选单的「全部测速」
 * 逐字同集合（那颗按钮的合并理由恰恰是「不要重复入口」）。逻辑单测全绿、缺陷照旧。
 *
 * 守的是**形态**不是措辞：断言都跑在**剥掉注释**的源码上（本仓注释习惯逐字引用被替换掉的旧形态，
 * 扫原文会被说明文字误伤；反过来只在注释里提一句函数名也能让 `toContain` 假绿）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const RAW = readFileSync(fileURLToPath(new URL('./HomeScreen.tsx', import.meta.url)), 'utf8');

/** 去注释（`[^:]` 前瞻避免把 `https://` 当行注释切掉）。 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}
const SRC = code(RAW);

/** 取顶层 `const <name> = useCallback(` 到其收尾（列 2 缩进的 `);`）为止的函数体。 */
function callbackBody(name: string): string {
  const anchor = `const ${name} = useCallback(`;
  const start = SRC.indexOf(anchor);
  expect(start, `锚点消失，守卫已失去判据: ${anchor}`).toBeGreaterThan(-1);
  const rest = SRC.slice(start);
  const end = rest.indexOf('\n  );');
  expect(end, `找不到 ${name} 的 useCallback 收尾`).toBeGreaterThan(-1);
  return rest.slice(0, end);
}

describe('守卫自检：扫到的确实是 HomeScreen 源码（防读空文件恒绿）', () => {
  it('文件非空且是本屏', () => {
    expect(RAW.length).toBeGreaterThan(1000);
    expect(SRC).toContain('export function HomeScreen');
  });

  it('去注释没有把代码一起吃掉', () => {
    expect(SRC).toContain('api.server.speedTest');
    expect(SRC).toContain('onNetworkCheck');
  });
});

describe('网络检测（onSpeedTest）只测当前出口', () => {
  const body = () => callbackBody('onSpeedTest');

  it('请求集是 `[currentServer.id]` —— 单节点，不是任何集合表达式', () => {
    expect(body()).toContain('api.server.speedTest([currentServer.id])');
  });

  it('不再取全量集合（`speedTestableIds` 不得出现在本腿）', () => {
    // 变异对照：把这一腿改回 `speedTestableIds(servers, …)` → 本条转红。
    expect(body()).not.toContain('speedTestableIds');
  });

  it('依赖数组含 currentServer / sentinelSelected（否则闭包读到陈旧出口，切节点后仍测旧的）', () => {
    const deps = body().slice(body().lastIndexOf('}, ['));
    expect(deps).toContain('sentinelSelected');
    expect(deps).toContain('currentServer');
    // 全量时代的 `servers` 依赖必须撤掉：留着会让每次订阅刷新都重建这颗回调（无谓重渲染），
    // 且是"这腿还在看全量"的化石。
    expect(deps).not.toMatch(/\bservers\b/);
  });
});

describe('三种「测不了」都出声，且判定顺序不能乱', () => {
  const body = () => callbackBody('onSpeedTest');

  it('哨兵出口（直连/阻断）有专属提示，不冒充成「未选节点」', () => {
    expect(body()).toContain('sentinelSelected');
    expect(body()).toContain('nodes.speedTestSentinelExit');
  });

  it('出口节点结构上不可测 → 走 speedTestBlockReason + 与节点页同一句措辞', () => {
    expect(body()).toContain('speedTestBlockReason(currentServer');
    expect(body()).toContain('speedTestBlockedMessage(reason, t)');
  });

  it('selectedServerId 指向不存在的节点 → 复用后端同条件文案', () => {
    expect(body()).toContain('nodes.speedTestNoActiveExit');
  });

  /**
   * 顺序有牙：`sentinelSelected ⟹ currentServer === null`（HomeScreen 的 `currentServer` memo 在
   * 哨兵时直接返 null）。哨兵分支若排在 `!currentServer` 之后就**永远走不到**，直连/阻断会被说成
   * 「未选节点」。变异对照：两个 if 互换 → 本条转红。
   */
  it('哨兵分支必须排在 !currentServer 分支之前', () => {
    const b = body();
    const sentinel = b.indexOf('if (sentinelSelected)');
    const noServer = b.indexOf('if (!currentServer)');
    expect(sentinel, '哨兵分支不见了').toBeGreaterThan(-1);
    expect(noServer, '空出口分支不见了').toBeGreaterThan(-1);
    expect(sentinel).toBeLessThan(noServer);
  });

  /** 三条边界都必须 return —— 少一个 return 就会带着 null/不可测的出口继续往下发请求。 */
  it('三条边界各自 return，不落到 speedTest 调用', () => {
    const b = body();
    const call = b.indexOf('api.server.speedTest');
    expect(call).toBeGreaterThan(-1);
    expect(b.slice(0, call).match(/\breturn;/g)?.length ?? 0).toBeGreaterThanOrEqual(3);
  });
});

describe('全量入口原样保留在出口选单', () => {
  it('onTestAllInMenu 仍走 speedTestableIds 全量过滤', () => {
    // 变异对照：把菜单腿也改成单节点 → 产品彻底没有全量入口 → 本条转红。
    expect(callbackBody('onTestAllInMenu')).toContain('speedTestableIds(servers');
  });

  it('两条腿共用 testing 单飞标志（后端有进程级单飞闸，撞闸会报成「测速失败」）', () => {
    expect(callbackBody('onTestAllInMenu')).toContain('testing');
    expect(SRC).toContain('setTesting(true)');
  });
});

describe('网络检测三条腿仍并发且互不牵连', () => {
  it('onNetworkCheck = allSettled(测速, 解锁重检)', () => {
    const b = callbackBody('onNetworkCheck');
    expect(b).toContain('Promise.allSettled');
    expect(b).toContain('onSpeedTest()');
    expect(b).toContain('onUnlockRefresh()');
  });
});
