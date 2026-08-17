/**
 * stats 订阅接线不变量守卫 —— 钉死「**每一个** stats 订阅点都走 `createTopicSubscription`」。
 *
 * 为什么必须是源码结构守卫（而不是又一组 `createTopicSubscription` 的逻辑单测）：
 * 被守的缺陷不在状态机里。`topic-subscription.test.ts` 那 6 条一直是绿的，而首页拓扑照样滞后 ——
 * 因为**调用点没用它**，各自手写了
 *
 *     api.stats.subscribe(topic).then(() => { off = api.stats.onXxx(cb); })
 *
 * 这一形态，两条真机缺陷都由它产生：
 *  1. **首帧被丢**：三条后端 poller 的首拍都不 sleep（`runtime/stats.rs` `PollGate::next_tick` 的
 *     `ticked` 分支），订阅一落地就发首帧；而监听挂在 `.then()` 里要多等「invoke 应答回 JS」+
 *     「`plugin:event|listen` 再往返一次」两趟才注册得上 → 那帧打在没有监听的窗口上被直接丢掉，
 *     白等一整拍（1s）才见第一屏数据。改用状态机后监听的 invoke 排在 `subscribe` **之前**投递。
 *  2. **监听泄漏**：cleanup 跑在 `.then()` 之前时 `off` 还是空壳，真监听在 cleanup **之后**注册且
 *     再没人摘 —— 漏掉的监听活到进程结束，此后每一帧都白跑一遍全部死回调。
 *     （`let cancelled` 守卫能挡住第 2 条，挡不住第 1 条；连接页的 aggregate 腿连它都没有。）
 *
 * 守的是**形态**不是措辞：断言落在「哪条腿调了哪个函数 / 三件套的 topic 对不对得上」这类结构事实，
 * 改注释、改文案、改变量名不会误伤；把任一调用点写回 `.subscribe().then()` 则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';

const SRC = fileURLToPath(new URL('..', import.meta.url));

/**
 * 去掉注释后的源码 —— **所有断言都跑在它上面**，两个方向都必要：
 *  - 负向：本仓注释习惯逐字引用「被替换掉的旧形态」（本文件头就写着 `.subscribe(topic).then(`），
 *    直接扫原文会被说明文字误伤；
 *  - 正向：只在注释里提一句 `createTopicSubscription` 就能让 `toContain` 变绿 —— 那是假绿。
 * `[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

/** 递归收集 src 下的产品代码（跳过测试自身，否则本文件的示例串会污染扫描面）。 */
function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) sourceFiles(p, out);
    else if (/\.tsx?$/.test(e.name) && !/\.(test|spec)\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

/** 全部含 stats 订阅调用的产品文件（路径 → 去注释源码）。 */
const SUBSCRIBERS = new Map<string, string>(
  sourceFiles(SRC)
    .map((p) => [p, code(readFileSync(p, 'utf8'))] as const)
    // `api.stats.subscribe(` 是订阅点的唯一入口（api-client 里的定义本身不算调用点）。
    .filter(([p, src]) => /api\.stats\s*\.\s*subscribe\(/.test(src) && !p.endsWith('api-client.ts'))
);

const rel = (p: string) => p.slice(SRC.length);

describe('守卫自检：扫到的确实是源码（防扫空目录 / 过滤过头 → 恒绿）', () => {
  it('产品源码文件被真的收集到了', () => {
    const all = sourceFiles(SRC);
    expect(all.length).toBeGreaterThan(50);
    // 测试文件必须被排除：本文件头的注释里就写着被禁的旧形态，扫进来会自己咬自己。
    expect(all.some((p) => p.endsWith('.test.ts') || p.endsWith('.test.tsx'))).toBe(false);
  });

  /**
   * **正向对照**：下面 T1/T2 全是负向断言（「不得出现旧形态」），扫不到任何文件时它们会全部空跑转绿。
   * 故先钉死订阅点的实际条数 —— 少一个就是有调用点被删/改名而守卫没跟上，多一个就是新增了订阅点
   * 而作者没读过本文件，两种都该停下来看一眼。
   */
  it('恰好三个 stats 订阅点（首页拓扑 / 连接页 / 状态栏），且都被扫到', () => {
    const names = [...SUBSCRIBERS.keys()].map(rel).sort();
    expect(names).toEqual([
      'components/layout/StatusBar.tsx',
      'components/screens/connections/ConnectionsScreen.tsx',
      'components/screens/home/HomeScreen.tsx',
    ]);
  });

  it('去注释后仍是可断言的代码（防 code() 把源码整段吃掉 → 负向断言恒绿）', () => {
    for (const [p, src] of SUBSCRIBERS) {
      const raw = readFileSync(p, 'utf8');
      expect(src.length, `${rel(p)} 去注释后几乎空了`).toBeGreaterThan(raw.length / 4);
      expect(src, `${rel(p)} 的注释没被剥掉`).not.toContain('监听挂在');
    }
  });

  /**
   * **正向对照（二）：帧监听的分布**。上一条只钉「哪些文件有订阅点」，钉不住「某条腿的 `onFrame`
   * 被改成了空壳」——T1/T2 都是 `for (const m of src.matchAll(/onFrame:…api\.stats\.on/))`，
   * 端口写成 `onFrame: () => () => {}` 时匹配数归零、循环空跑，**全绿**。
   *
   * 这个缺口有过真实落点：首页 aggregate 腿刻意只持订阅令牌、不挂帧监听（判据见
   * `screens/home/topology-render-budget.test.ts` 门 4），形态与「把连接页那条也改成空壳」逐字
   * 相同。若连接页 aggregate 变成空壳，排名页实时数据静默死掉而本文件与首页那道门都不响。
   * 故把「哪个文件挂着哪些 topic 的**真**监听、各几条」整张表钉死：空壳化 ⇒ 该项消失 ⇒ 转红。
   */
  it('正向对照：真帧监听的分布（首页刻意为 0；连接页三条、状态栏一条）', () => {
    const found: Record<string, string[]> = {};
    for (const [p, src] of SUBSCRIBERS) {
      const topics = [...src.matchAll(/onFrame:\s*\((?:cb|\w+)\)\s*=>\s*api\.stats\.(on[A-Za-z]+)\(/g)]
        .map((m) => m[1])
        .sort();
      found[rel(p)] = topics;
    }
    expect(found).toEqual({
      // 首页只持 aggregate 的订阅令牌（撤掉令牌会误停三条 topic 共用的那条流），**不挂帧监听**。
      'components/screens/home/HomeScreen.tsx': [],
      'components/screens/connections/ConnectionsScreen.tsx': [
        'onConnectionsAggregate',
        'onConnectionsClosed',
        'onConnectionsDetail',
      ],
      'components/layout/StatusBar.tsx': ['onStatsUpdated'],
    });
  });
});

describe('T1：订阅点一律走 createTopicSubscription（监听先挂 + 订过必退，两条不变式随之带上）', () => {
  it('每个订阅点都 import 了状态机，且从中性目录 @/lib 取（不跨屏 import）', () => {
    for (const [p, src] of SUBSCRIBERS) {
      expect(src, `${rel(p)} 没走状态机`).toMatch(
        /import\s*\{\s*createTopicSubscription\s*\}\s*from\s*'@\/lib\/topic-subscription'/
      );
      expect(src, `${rel(p)} 里 createTopicSubscription 只出现在 import`).toMatch(
        /createTopicSubscription</
      );
    }
  });

  it('**不得**退回 `api.stats.subscribe(...).then(...)`（首帧丢在这一形态上）', () => {
    for (const [p, src] of SUBSCRIBERS) {
      expect(src, `${rel(p)} 把监听挂回了 subscribe 的 .then()`).not.toMatch(
        /api\.stats\s*\.\s*subscribe\([^)]*\)\s*\.\s*(then|catch)/
      );
    }
  });

  it('监听注册**不得**出现在任何 .then() 回调里（含被赋值给可变 off 的旧形态）', () => {
    for (const [p, src] of SUBSCRIBERS) {
      // 旧形态的判据：`off = api.stats.onXxx(` —— 状态机版本里监听只出现在 `onFrame:` 端口位上。
      expect(src, `${rel(p)} 仍把监听赋给可变 off（cleanup 可能拿到空壳）`).not.toMatch(
        /\b(off|unlisten|cleanup)\s*=\s*api\.stats\.on[A-Z]/
      );
      // 监听 getter 只允许出现在端口的 onFrame 位上。
      for (const m of src.matchAll(/api\.stats\.on[A-Za-z]+\(/g)) {
        const before = src.slice(Math.max(0, m.index - 40), m.index);
        expect(before, `${rel(p)} 有个监听注册不在 onFrame 端口位上：${m[0]}`).toMatch(
          /onFrame:\s*\((cb|\w+)\)\s*=>\s*$/
        );
      }
    }
  });
});

describe('T2：端口三件套的 topic 必须自洽（事件名 / 订 / 退三处对不上 = 该 topic 恒空）', () => {
  /** topic → 监听 getter（与 domain/ipc-channels.ts 的 STATS_TOPIC_EVENT 同一映射）。 */
  const PORT: Array<[string, string]> = [
    ['stats', 'onStatsUpdated'],
    ['aggregate', 'onConnectionsAggregate'],
    ['detail', 'onConnectionsDetail'],
    ['closed', 'onConnectionsClosed'],
  ];

  it('每个端口字面量的 onFrame / subscribe / unsubscribe 指向同一个 topic', () => {
    for (const [p, src] of SUBSCRIBERS) {
      for (const m of src.matchAll(/onFrame:\s*\((?:cb|\w+)\)\s*=>\s*api\.stats\.(on[A-Za-z]+)\(/g)) {
        const getter = m[1];
        const entry = PORT.find(([, g]) => g === getter);
        expect(entry, `${rel(p)}：${getter} 不是已知的 stats 监听 getter`).toBeDefined();
        const topic = entry![0];
        // 端口字面量是紧挨着的三行，取 onFrame 之后一小段即可覆盖 subscribe + unsubscribe。
        const port = src.slice(m.index, m.index + 260);
        expect(port, `${rel(p)}：${getter} 配的不是 subscribe('${topic}')`).toMatch(
          new RegExp(`subscribe:\\s*\\(\\)\\s*=>\\s*api\\.stats\\.subscribe\\(\\s*'${topic}'`)
        );
        expect(port, `${rel(p)}：${getter} 配的不是 unsubscribe('${topic}')`).toMatch(
          new RegExp(`unsubscribe:\\s*\\(\\)\\s*=>\\s*api\\.stats\\.unsubscribe\\(\\s*'${topic}'`)
        );
      }
    }
  });
});

describe('T3：连接页明细腿的「暂停 = 退订 + 摘监听」语义不得退化', () => {
  const CONN = code(
    readFileSync(join(SRC, 'components/screens/connections/ConnectionsScreen.tsx'), 'utf8')
  );

  it('暂停走整条 dispose 重建（而非 setWanted(false)：那样监听还在，在途帧仍会落表）', () => {
    // 判据：detail 端口所在的 effect 有一条短路守卫、其中含 `paused`，cleanup 是 `sub.dispose()`。
    //
    // 守卫**不逐字比**：该 effect 另有 `view === 'table'` 一维（拓扑视图下 detail 的产物无人消费，
    // 见 ConnectionsScreen 订阅腿注释）。本守卫只管「暂停这一维不退化成 setWanted(false)」，
    // view 那一维由 `connections-view-scope.test.ts` 守；写死整条条件会让两个守卫互相绊。
    const at = CONN.indexOf('onConnectionsDetail');
    expect(at, 'detail 端口不见了，守卫已失去判据').toBeGreaterThan(-1);
    const effect = CONN.slice(CONN.lastIndexOf('useEffect(', at), at + 600);
    const guard = /if\s*\(([^)]*)\)\s*return;/.exec(effect);
    expect(guard, '订阅 effect 的短路守卫不见了').toBeTruthy();
    expect(guard?.[1], '暂停不再短路整个订阅 effect').toContain('paused');
    expect(effect, 'cleanup 必须 dispose（同步摘监听），不能只 setWanted(false)').toMatch(
      /return\s*\(\)\s*=>\s*sub\.dispose\(\);/
    );
    expect(effect, 'effect 必须以 paused 为依赖，否则暂停不触发重建').toMatch(
      /\}\s*,\s*\[[^\]]*\bpaused\b[^\]]*\]\s*\)/
    );
  });
});
