/**
 * 组网信息面（`MeshInfoHoverCard.tsx`）的门 —— 守三条不变量，全部离线、零网络。
 *
 *  G1 **配置值与运行期值必须视觉可分**。这是本批唯一真正会伤到用户的缺陷形态：用户填的 `exitNode`
 *     与 tailnet 上真正生效的出口可以不同，两者同色同段列出来会被读成「填的就是生效的」。
 *     判据落在渲染产物上（段头 + 分隔线 + 值的 tone 类），不是「函数返回了两个数组」。
 *  G2 **拿不到的字段一行都不画**。WireGuard / WARP 在 sing-box 1.14 管理 API 上**没有任何**运行期
 *     RPC（`started_service.proto` 全表只有 Tailscale 那三条 + 状态/连接流 + 出站选择），故它们
 *     一行运行期都不许出现 —— 摆一个恒为「—」的「上次握手」位比不显示更坏。
 *  G3 **接线真的在**。纯逻辑对了但节点卡没挂 = 全绿的缺陷（本仓 `nodes-speedtest-wiring.test.ts`
 *     守的就是同一类形态），故加一段 NodeCard.tsx 的源码结构守卫。
 *
 * 渲染断言用 `react-dom/server` 真渲染真组件（同 `screens/nodes/sub-info-bar-progress.test.tsx`）：
 * 本仓 vitest 是 `environment:'node'`，刻意不装 jsdom / testing-library，别为这道门破例。
 * `t()` 桩返回 key 本身 —— 断言因此落在**键**上而非中文措辞，改译文不误伤、换错键必然转红。
 *
 * **射程之外**（如实记）：真实 CSS（node 下无 CSSOM，`.mi-v.live` 与 `.mi-v.cfg` 的色差要真机看）、
 * hover 的 500ms 延迟与定位（归 `HoverCard.tsx`，本卡不重复守）、`tailscaleGetStatus` 的 IPC 往返
 * （要真核，本仓禁跑触网测试 —— 故取数那一层只由 G3 断言「调用点存在」，不断言其返回值）。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ServerConfig } from '@/contracts/types';
import type { TailscaleStatusEvent } from '@/contracts/tailscale-status';

vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: 'zh-CN' } }),
}));

const { MeshInfoRows, meshConfigRows, meshRuntimeRows } = await import('./MeshInfoHoverCard');

// ── 样本 ────────────────────────────────────────────────────────────────────────
const TS: ServerConfig = {
  id: 'ts1',
  name: 'tailnet',
  protocol: 'tailscale',
  address: '',
  port: 0,
  tailscaleSettings: {
    // 用户**填的**出口 —— 与下面状态帧里真正生效的那台故意不同，G1 的核心样本。
    exitNode: 'router-old',
    routes: ['192.168.1.0/24'],
    advertiseRoutes: ['192.168.1.0/24', '10.8.0.0/24'],
    acceptRoutes: true,
  },
} as ServerConfig;

const WG: ServerConfig = {
  id: 'wg1',
  name: 'home',
  protocol: 'wireguard',
  address: 'vpn.example.com',
  port: 51820,
  wireguardSettings: {
    privateKey: 'k',
    peerPublicKey: 'p',
    localAddress: ['10.0.0.2/32'],
    allowedIPs: ['10.0.0.0/24', '0.0.0.0/0', '::/0'],
  },
} as ServerConfig;

const WARP: ServerConfig = {
  ...WG,
  id: 'warp1',
  name: 'WARP',
  address: 'engage.cloudflareclient.com',
  wireguardSettings: {
    ...WG.wireguardSettings!,
    warpDevice: { deviceId: 'd', token: 't' },
  },
} as ServerConfig;

const VLESS: ServerConfig = {
  id: 'v1',
  name: 'proxy',
  protocol: 'vless',
  address: 'a.example.com',
  port: 443,
} as ServerConfig;

const STATUS: TailscaleStatusEvent = {
  serverId: 'ts1',
  backendState: 'Running',
  loggedIn: true,
  tailscaleIPs: ['100.64.0.5', 'fd7a:115c:a1e0::5'],
  expired: false,
  peers: [
    { hostName: 'router-new', ip: '100.64.0.9', online: true, exitNode: true, exitNodeOption: true, active: true },
    { hostName: 'laptop', ip: '100.64.0.7', online: false, exitNode: false, exitNodeOption: false, active: false },
  ],
};

// ════════════════════════════════════════════════════════════════════════════════
// G2 拿不到的字段一行都不画
// ════════════════════════════════════════════════════════════════════════════════

describe('G2 运行期段只在有源的地方出现', () => {
  it('WireGuard / WARP 恒无运行期行 —— 哪怕硬塞一个状态帧进去', () => {
    // 变异对照：把 `isAccountBasedProtocol` 放宽成 `isMeshProtocol`（= 把 WG 也算进来）⇒ 本条转红。
    expect(meshRuntimeRows(WG, STATUS)).toEqual([]);
    expect(meshRuntimeRows(WARP, STATUS)).toEqual([]);
  });

  it('Tailscale 但状态帧没到 → 空（不画一排「—」占位）', () => {
    expect(meshRuntimeRows(TS, undefined)).toEqual([]);
  });

  it('非组网协议两段皆空（代理节点没有内网地址/路由这套概念）', () => {
    expect(meshRuntimeRows(VLESS, STATUS)).toEqual([]);
    expect(meshConfigRows(VLESS)).toEqual([]);
  });

  it('这一帧没带对端信息（peers 空）→ 不画对端行，而不是画「0/0」', () => {
    // 「一台都没在线」与「这帧压根没对端信息」是两回事，后者画 0/0 是在编造。
    expect(meshRuntimeRows(TS, { ...STATUS, peers: [] }).map((r) => r.id)).toEqual([
      'intranetIp',
      'activeExit',
    ]);
  });
});

// ════════════════════════════════════════════════════════════════════════════════
// 运行期取值：来源必须是状态帧，不是配置
// ════════════════════════════════════════════════════════════════════════════════

describe('运行期行取自 STATUS 帧', () => {
  it('内网 IP = self.tailscaleIPs（配置里根本没有这一项）', () => {
    const rows = meshRuntimeRows(TS, STATUS);
    expect(rows.find((r) => r.id === 'intranetIp')?.values).toEqual([
      '100.64.0.5',
      'fd7a:115c:a1e0::5',
    ]);
  });

  it('生效出口 = peers 里 exitNode=true 的那台，**不是**配置里填的那个', () => {
    // 变异对照：把 pickActiveExit 改成读 `server.tailscaleSettings.exitNode` ⇒ 本条转红。
    // 这一条就是整批的立命之本：填的 `router-old` 与生效的 `router-new` 必须分得开。
    const rows = meshRuntimeRows(TS, STATUS);
    expect(rows.find((r) => r.id === 'activeExit')?.values).toEqual(['router-new · 100.64.0.9']);
    expect(meshConfigRows(TS).find((r) => r.id === 'exitNode')?.values).toEqual(['router-old']);
  });

  it('没有任何对端被选作出口 → 生效出口为空（渲染成「无」，非静默省略）', () => {
    const noExit = { ...STATUS, peers: STATUS.peers.map((p) => ({ ...p, exitNode: false })) };
    expect(meshRuntimeRows(TS, noExit).find((r) => r.id === 'activeExit')?.values).toEqual([]);
  });

  it('对端在线数 = online 计数 / 总数', () => {
    expect(meshRuntimeRows(TS, STATUS).find((r) => r.id === 'peers')?.values).toEqual(['1/2']);
  });
});

// ════════════════════════════════════════════════════════════════════════════════
// 配置取值
// ════════════════════════════════════════════════════════════════════════════════

describe('配置行取自用户填的值', () => {
  it('Tailscale 路由 = routes ∪ advertiseRoutes 去重保序', () => {
    expect(meshConfigRows(TS).find((r) => r.id === 'routes')?.values).toEqual([
      '192.168.1.0/24',
      '10.8.0.0/24',
    ]);
  });

  it('acceptRoutes 关着时整行不出现（缺省态不占位）', () => {
    const off = { ...TS, tailscaleSettings: { ...TS.tailscaleSettings, acceptRoutes: false } };
    expect(meshConfigRows(TS).some((r) => r.id === 'acceptRoutes')).toBe(true);
    expect(meshConfigRows(off as ServerConfig).some((r) => r.id === 'acceptRoutes')).toBe(false);
  });

  it('WireGuard 路由只列具体段，catch-all 由「仅局域网」角标表达、不在此重复', () => {
    // 变异对照：去掉 `stripCatchAll` 直接用 allowedIPs ⇒ 本条转红（0.0.0.0/0 会冒出来）。
    const rows = meshConfigRows(WG);
    expect(rows.find((r) => r.id === 'localAddress')?.values).toEqual(['10.0.0.2/32']);
    expect(rows.find((r) => r.id === 'routes')?.values).toEqual(['10.0.0.0/24']);
  });
});

// ════════════════════════════════════════════════════════════════════════════════
// G1 配置值与运行期值视觉可分
// ════════════════════════════════════════════════════════════════════════════════

const html = (node: Parameters<typeof renderToStaticMarkup>[0]) => renderToStaticMarkup(node);

describe('G1 两段在渲染产物上分得开', () => {
  it('Tailscale + 活的状态帧：两个段头 + 一条分隔线 + 两种值 tone', () => {
    const out = html(<MeshInfoRows server={TS} status={STATUS} live />);
    // 段头两个都在，且顺序是「运行期在前、配置在后」（先看真值，再看填的是什么）。
    expect(out).toContain('nodes.meshInfoRuntime');
    expect(out).toContain('nodes.meshInfoConfig');
    expect(out.indexOf('nodes.meshInfoRuntime')).toBeLessThan(out.indexOf('nodes.meshInfoConfig'));
    // 分隔线（与规则 hover 卡同一个 .tc-sep，不新造）。
    expect(out).toContain('tc-sep');
    // 变异对照：把 `tone={stale ? 'stale' : 'live'}` 改成 `tone="cfg"`（两段同色）⇒ 下面两条转红。
    expect(out).toContain('mi-v mono live');
    expect(out).toContain('mi-v mono cfg');
  });

  it('新鲜度：live 时绿点无「上次已知」；核停时琥珀点 + 文字兜底（不靠颜色单通道）', () => {
    const fresh = html(<MeshInfoRows server={TS} status={STATUS} live />);
    expect(fresh).toContain('mi-dot live');
    expect(fresh).not.toContain('nodes.meshInfoStale');

    const stale = html(<MeshInfoRows server={TS} status={STATUS} live={false} />);
    expect(stale).toContain('mi-dot stale');
    // 文字兜底必须在：只翻个颜色对色觉障碍用户等于没说。
    expect(stale).toContain('nodes.meshInfoStale');
    expect(stale).toContain('mi-v mono stale');
  });

  it('两段的「出口」用不同标签键 —— 否则一张卡上两行同名，读者无从分辨哪个是真的', () => {
    const out = html(<MeshInfoRows server={TS} status={STATUS} live />);
    expect(out).toContain('nodes.meshInfoActiveExit');
    expect(out).toContain('nodes.meshInfoExitNode');
  });

  it('WireGuard / WARP：只有配置段，运行期段头一个字都没有', () => {
    for (const node of [WG, WARP]) {
      const out = html(<MeshInfoRows server={node} />);
      expect(out).toContain('nodes.meshInfoConfig');
      expect(out).not.toContain('nodes.meshInfoRuntime');
      expect(out).not.toContain('mi-v mono live');
      expect(out).not.toContain('tc-sep');
    }
  });

  it('自检：渲染器真的产出了内容（防 renderToStaticMarkup 返空 → 上面的 not.toContain 恒绿）', () => {
    const out = html(<MeshInfoRows server={WG} />);
    expect(out.length).toBeGreaterThan(80);
    expect(out).toContain('mi-row');
  });
});

// ════════════════════════════════════════════════════════════════════════════════
// G3 接线守卫（源码结构，同 nodes-speedtest-wiring.test.ts 的手法）
// ════════════════════════════════════════════════════════════════════════════════

const read = (rel: string): string =>
  readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
/** 去注释后的源码 —— 本仓注释习惯逐字引用被替换的旧形态，直接扫原文会被自己的说明文字骗绿。 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const CARD_RAW = read('../screens/nodes/NodeCard.tsx');
const CARD = code(CARD_RAW);
const SELF = code(read('./MeshInfoHoverCard.tsx'));

describe('G3 接线真的在（纯逻辑对了但没挂 = 全绿的缺陷）', () => {
  it('守卫自检：扫到的确实是 NodeCard 源码，且注释已剥掉', () => {
    expect(CARD_RAW.length).toBeGreaterThan(1000);
    expect(CARD).toContain('export function NodeCard');
    expect(CARD).not.toContain('1:1 提取自原型');
  });

  it('节点卡真的渲染了本卡（import + 挂载点都在）', () => {
    expect(CARD).toContain('MeshInfoHoverCardContent');
    expect(CARD).toContain('HoverCardPanel');
  });

  it('ⓘ 受 isMeshNode 门控 —— 不给代理协议节点画一张空卡', () => {
    // 判据是**节点级**而非协议级：openconnect / openvpn-client 只在用户声明了内网段时才有内网信息可看，
    // 没声明就是个普通出口，画出来是张空卡。
    expect(CARD).toContain('isMeshNode(server)');
  });

  it('点 ⓘ 不得触发整卡的「设为出口」/ 批选（必须 stopPropagation）', () => {
    // 变异对照：删掉那行 onClick ⇒ 本条转红。真机上的表现是「想看信息，结果换了出口节点」。
    const btn = CARD.slice(CARD.indexOf('nd-info-btn'));
    expect(btn.slice(0, btn.indexOf('</button>'))).toContain('stopPropagation');
  });

  it('键盘腿在：ⓘ 是可聚焦按钮且 focus 也能唤出卡（useHoverCard 只给鼠标 handler）', () => {
    const btn = CARD.slice(CARD.indexOf('nd-info-btn'), CARD.indexOf('</button>'));
    expect(btn).toContain('onFocus');
    expect(btn).toContain('onBlur');
    expect(btn).toContain('aria-label');
  });

  it('取数走 TAILSCALE_GET_STATUS 缓存末帧 + 新鲜度，且只对账号制协议发', () => {
    // 空白折叠：prettier 会把链式调用断行成 `api.server\n  .tailscaleGetStatus()`，
    // 判据不该被格式化左右（否则一次 `npm run format` 就能把门弄红）。
    expect(SELF.replace(/\s+/g, '')).toContain('api.server.tailscaleGetStatus()');
    expect(SELF).toContain('isAccountBasedProtocol(server.protocol)');
  });
});
