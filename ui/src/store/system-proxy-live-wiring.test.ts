/**
 * 系统代理活态接线不变量守卫 —— 钉死「**只有一份轮询**，两个消费方读**同一份**活态」。
 *
 * 为什么必须是源码结构守卫：被守的缺陷不在算法里。`deriveTakeoverConnState` 是纯函数、已有完整
 * 单测（`connection-state.test.ts`），把取数钩子复制两份照样全绿，而用户看到的是：
 *  - 双倍 exec `networksetup`（mac 上每次 1 列服务 + 3 读协议）/ `gsettings` / `reg`；
 *  - 两条轮询链起点不同（HomeScreen 随路由挂载、StatusBar 随布局挂载）⇒ 同一时刻可能一处已判
 *    `not-effective`、另一处还停在上一拍 `effective` ⇒ **首页说「未生效」、状态栏还亮绿灯**，
 *    正是活态这个功能本身要根治的那类自相矛盾。
 *
 * 故沿用本仓既有的源码不变量守卫模式（`store/latency-wiring-invariants.test.ts`、
 * `components/screens/nodes/nodes-speedtest-wiring.test.ts`）。守的是**形态**不是措辞：
 * 断言都跑在剥掉注释的源码上，改注释/改文案不会误伤，把轮询搬回组件则必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { resolve } from 'node:path';

const SRC = resolve(__dirname, '..');
const read = (rel: string): string => readFileSync(resolve(SRC, rel), 'utf8');

/** 递归收集 `src` 下全部 `.ts/.tsx`（跳过 node_modules）。 */
function listSources(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    if (name === 'node_modules') continue;
    const full = resolve(dir, name);
    if (statSync(full).isDirectory()) out.push(...listSources(full));
    else if (/\.tsx?$/.test(name)) out.push(full);
  }
  return out;
}

/**
 * 去注释 —— 两个方向都必要：负向上本文件与被守文件的注释都逐字引用了「禁止的旧形态」
 * （`getSystemProxyStatus`、`useSystemProxyLive(active)`），直接扫原文会自我误伤；正向上只在注释里
 * 提一句函数名就能让 `toContain` 变绿，那是假绿。`[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const STORE = 'store/use-system-proxy-live.ts';
const APP = 'App.tsx';
const HOME = 'components/screens/home/HomeScreen.tsx';
const STATUSBAR = 'components/layout/StatusBar.tsx';
/** 两个消费方（同屏共存：StatusBar 在 AppShell 的 `<main>` 底部、HomeScreen 在其上方内容区）。 */
const CONSUMERS = [HOME, STATUSBAR];

describe('T1：轮询只有一份，且挂在窗口级持久位置', () => {
  it('后端查询语句只出现在共享 store 一处（全 src 扫描）', () => {
    // 直接扫全部 .ts/.tsx 而非逐个枚举已知消费方：任何**未来新增**的组件/hook 里冒出第二处调用
    // 也在此转红。匹配的是调用形态 `xxx.getSystemProxyStatus(`，故 api-client 里的方法**定义**
    // （`async getSystemProxyStatus(`，无前导点）不计入。
    const callers = listSources(SRC)
      .filter((f) => /\.getSystemProxyStatus\s*\(/.test(code(readFileSync(f, 'utf8'))))
      .map((f) => f.slice(SRC.length + 1))
      .sort();
    expect(callers).toEqual([STORE]);
  });

  it('轮询驱动挂 App.tsx 顶层（全窗口生命周期只建一次）', () => {
    expect(code(read(APP))).toContain('useSystemProxyLivePolling()');
  });

  it.each(CONSUMERS)('%s **不得**自建轮询驱动（挂组件里 = 每个消费方一条链）', (f) => {
    expect(code(read(f))).not.toContain('useSystemProxyLivePolling');
    // 原状形态：`useSystemProxyLive(active)` —— 带参数 = 组件自己在驱动取数。
    expect(code(read(f))).not.toMatch(/useSystemProxyLive\(\s*[^)\s]/);
  });
});

describe('T2：两个消费方拿的是同一份活态（共享 store，不是各自的 state）', () => {
  it.each(CONSUMERS)('%s 从共享 store 读活态', (f) => {
    const src = code(read(f));
    expect(src).toContain('useSystemProxyLive()');
    expect(src).toContain("from '@/store/use-system-proxy-live'");
  });

  it.each(CONSUMERS)('%s **不得**把活态存回组件私有 useState', (f) => {
    const src = code(read(f));
    expect(src).not.toMatch(/useState<SystemProxyLive>/);
    expect(src).not.toMatch(/\[\s*\w*[lL]ive\s*,\s*set\w*[lL]ive\s*\]/);
  });

  it.each(CONSUMERS)('%s 把活态真的喂进了判定（拿到不用等于没接）', (f) => {
    // `deriveTakeoverConnState({ ... systemProxyLive })` —— 简写或显式赋值都算。
    expect(code(read(f))).toMatch(/deriveTakeoverConnState\(\{[^}]*systemProxyLive/s);
  });
});

describe('T3：三层节流不得丢（活态在常驻期近乎零开销的全部依据）', () => {
  const store = code(read(STORE));

  it('① 适用范围门：只在「核稳定运行 + 非 starting + systemProxy」时查', () => {
    expect(store).toContain('isSystemProxyLiveApplicable');
    expect(store).toMatch(/running\s*&&\s*!starting/);
    // 兜底口径必须与 deriveTakeoverConnState 内那条一致（config 未水合 → 按 systemProxy）。
    expect(store).toMatch(/proxyModeType\s*\?\?\s*'systemProxy'/);
  });

  it('② 窗口不可见不查也不排期，且 visibilitychange 唤醒立刻补一发', () => {
    expect(store).toContain("document.visibilityState !== 'visible'");
    expect(store).toContain("addEventListener('visibilitychange'");
    // 排下一拍前**再判一次可见性** —— 隐藏期一个 timer 都不留。
    expect(store).toMatch(/document\.visibilityState === 'visible'[\s\S]{0,120}setTimeout/);
  });

  it('③ inFlight 单飞（可见性唤醒与定时到点可能同时触发）', () => {
    expect(store).toMatch(/if\s*\(\s*cancelled\s*\|\|\s*inFlight\s*\)\s*return/);
  });

  it('失败折 unknown 而非 not-effective（读不到 ≠ 没生效）', () => {
    expect(store).toMatch(/catch\s*\{[\s\S]{0,80}setLive\('unknown'\)/);
  });

  it('退出适用范围立刻丢弃上一轮结论', () => {
    expect(store).toMatch(/if\s*\(!active\)\s*\{[\s\S]{0,80}setLive\('unknown'\)/);
  });
});
