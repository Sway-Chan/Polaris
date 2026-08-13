/**
 * Tailscale 登录态判决门 + 组网卡状态派生（vitest，node 环境）。
 *
 * 钉的是「什么时候**才许**改写已知登录态」。被守的缺陷不是算法错，而是接线上无条件采信每一帧：
 * 后端的 `loggedIn` 是折叠值（`backendState ∈ {Running,Starting} 且未过期`，
 * `src-tauri/src/runtime/tailscale_status.rs:140`），核启动早期的 `NoState` 帧折叠出 false ——
 * 那是「后端还没启完」，不是「凭据无效」。而 `setTailscaleLoginState` 是**双写**
 * （内存 + localStorage），假 false 会被写穿进缓存，让下次冷启动直接显示「需登录」。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isDefinitiveTsLoginFrame, deriveTsCardState } from './tailscale-conn-state';

const frame = (backendState: string, loggedIn: boolean, expired = false) => ({
  backendState,
  loggedIn,
  expired,
});

describe('isDefinitiveTsLoginFrame —— 只有 definitive 帧才许裁定登录态', () => {
  it('definitive-in：loggedIn=true 恒采信（Running / Starting 折叠而来）', () => {
    expect(isDefinitiveTsLoginFrame(frame('Running', true))).toBe(true);
    expect(isDefinitiveTsLoginFrame(frame('Starting', true))).toBe(true);
  });

  it('definitive-out：控制面明说凭据不能用 → 采信（NeedsLogin / NeedsMachineAuth / 已过期）', () => {
    expect(isDefinitiveTsLoginFrame(frame('NeedsLogin', false))).toBe(true);
    expect(isDefinitiveTsLoginFrame(frame('NeedsMachineAuth', false))).toBe(true);
    // 过期腿与 backendState 正交：Running 但 key 过期，后端已折叠成 loggedIn=false，这是真结论。
    expect(isDefinitiveTsLoginFrame(frame('Running', false, true))).toBe(true);
  });

  /**
   * 核心那条：启动过渡帧**不采信**。变异对照：把函数改成恒 `true`（=回到无条件写）→ 本条转红。
   */
  it('启动过渡帧（NoState / Stopped）不采信 —— 那是「没启完」不是「凭据无效」', () => {
    expect(isDefinitiveTsLoginFrame(frame('NoState', false))).toBe(false);
    expect(isDefinitiveTsLoginFrame(frame('Stopped', false))).toBe(false);
    expect(isDefinitiveTsLoginFrame(frame('', false))).toBe(false);
  });

  it('未来新增的未知 backendState 一律不采信（保守方向：宁可保留旧值，不凭空翻转）', () => {
    expect(isDefinitiveTsLoginFrame(frame('SomeFutureState', false))).toBe(false);
  });

  /**
   * 反向自检：这道门**不得**把真正的登出/过期一起挡掉，否则用户 logout 后角标永远绿着。
   * 变异对照：把 definitive-out 三条删成 `return frame.loggedIn` → 本条转红。
   */
  it('不得挡掉真实登出：三种 definitive-out 帧必须放行 false', () => {
    for (const f of [
      frame('NeedsLogin', false),
      frame('NeedsMachineAuth', false),
      frame('Running', false, true),
    ]) {
      expect(isDefinitiveTsLoginFrame(f), `${f.backendState}/${f.expired} 被误挡`).toBe(true);
      expect(f.loggedIn).toBe(false); // 放行的确实是"未登录"这个结论
    }
  });
});

/**
 * 接线守卫：谓词存在但订阅没用它 = 缺陷照旧、逻辑单测全绿。
 * 断言跑在**剥掉注释**的源码上（注释里逐字提到了函数名，扫原文会假绿）。
 */
describe('接线：App.tsx 的 STATUS 订阅必须过这道门', () => {
  const RAW = readFileSync(fileURLToPath(new URL('../App.tsx', import.meta.url)), 'utf8');
  const SRC = RAW.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

  it('守卫自检：扫到的确实是 App.tsx 源码', () => {
    expect(RAW.length).toBeGreaterThan(1000);
    expect(SRC).toContain('onTailscaleStatus');
  });

  it('onTailscaleStatus 回调里先判 isDefinitiveTsLoginFrame 再写 store', () => {
    const at = SRC.indexOf('api.proxy.onTailscaleStatus(');
    expect(at, '订阅锚点消失，守卫已失去判据').toBeGreaterThan(-1);
    const body = SRC.slice(at, at + 400);
    expect(body).toContain('isDefinitiveTsLoginFrame(data)');
    // 顺序有牙：门必须排在写入之前，排后面等于没门。
    expect(body.indexOf('isDefinitiveTsLoginFrame')).toBeLessThan(
      body.indexOf('setTailscaleLoginState')
    );
  });
});

describe('deriveTsCardState', () => {
  const node = (over: Record<string, unknown> = {}) =>
    ({ id: 't', name: 't', protocol: 'tailscale', ...over }) as never;

  it('无节点 → no-node', () => {
    expect(deriveTsCardState(undefined, undefined, false)).toBe('no-node');
  });

  it('有 authKey → key-ready（静态凭据不进登录态）', () => {
    expect(deriveTsCardState(node({ tailscaleSettings: { authKey: 'k' } }), false, true, true)).toBe(
      'key-ready'
    );
  });

  it('登录进行中（loginActive + 有 URL + 未登录）→ logging-in', () => {
    expect(deriveTsCardState(node({ tailscaleSettings: {} }), false, true, true)).toBe('logging-in');
  });

  it('被动 always-emit 的 URL（非 loginActive）只显 needs-login，不误推进「连接中」', () => {
    expect(deriveTsCardState(node({ tailscaleSettings: {} }), false, true, false)).toBe(
      'needs-login'
    );
  });

  it('loggedIn → connected（压过 URL 残留）', () => {
    expect(deriveTsCardState(node({ tailscaleSettings: {} }), true, true, true)).toBe('connected');
  });
});
