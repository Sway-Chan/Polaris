/**
 * TsSettingsDialog 纯逻辑单测 —— 重点是「**缺省即默认**」这条纪律，它没有别的门守着。
 *
 * 类型检查、build、覆盖门（`contracts/protocol-settings-coverage.test.ts`）**都发现不了**
 * 「把默认值写成了显式值」：键在、类型对、覆盖面也齐，只是磁盘上多了一份默认值副本。
 * 它的代价要到「日后改默认」那天才显形——存量配置不跟随，两个真值源开始分叉。故单独钉。
 */
import { describe, it, expect } from 'vitest';
import type { ServerConfig, TailscaleSettings } from '@/contracts/types';
import type { TailscaleStatusPeer } from '@/contracts/tailscale-status';
import {
  buildTsSettings,
  exitNodeOptions,
  initTsDraft,
  invalidTsCidrs,
  EXIT_CUSTOM,
  type ExitNodeLabels,
} from './ts-settings-logic';

function node(ts: TailscaleSettings): ServerConfig {
  return { id: 'ts1', name: 'TS', protocol: 'tailscale', address: '', port: 0, tailscaleSettings: ts };
}

describe('initTsDraft：缺席回显 = 该字段真实缺省', () => {
  it('空设置（TsLoginDialog 新建节点写的就是 {}）不凭空造值', () => {
    const d = initTsDraft(node({}));
    // 缺省 true 的那一个（meshAlwaysRoutesSubnets / mesh_always_routes_subnets 都是 unwrap_or(true)）
    expect(d.alwaysRouteSubnets).toBe(true);
    // 缺省 false 的那一批
    expect(d.reverseMesh).toBe(false);
    expect(d.resolveByName).toBe(false);
    expect(d.acceptRoutes).toBe(false);
    expect(d.acceptDefaultResolvers).toBe(false);
    expect(d.sshServer).toBe(false);
    expect(d.ephemeral).toBe(false);
    // 未设 ≠ 0（R2：number 空 = undefined，绝不塞 0）
    expect(d.relayServerPort).toBeUndefined();
    expect(d.listenPort).toBeUndefined();
    expect(d.routes).toBe('');
  });

  it('显式值照原样回显（含 alwaysRouteSubnets 的显式 false）', () => {
    const d = initTsDraft(
      node({
        alwaysRouteSubnets: false,
        reverseMesh: true,
        resolveByName: true,
        relayServerPort: 41641,
        routes: ['192.168.50.0/24', '10.0.0.0/24'],
      })
    );
    expect(d.alwaysRouteSubnets).toBe(false);
    expect(d.reverseMesh).toBe(true);
    expect(d.resolveByName).toBe(true);
    expect(d.relayServerPort).toBe(41641);
    expect(d.routes).toBe('192.168.50.0/24, 10.0.0.0/24');
  });

  it('无节点（未登录）也给出全套合法草稿，不是 undefined 洞', () => {
    const d = initTsDraft(undefined);
    expect(d.alwaysRouteSubnets).toBe(true);
    expect(d.reverseMesh).toBe(false);
    expect(d.exitNode).toBe('');
  });
});

describe('buildTsSettings：用户没动过就不写显式值（删键而非写 false/0/[]）', () => {
  /**
   * 牙：把 `else delete next.reverseMesh` 改成 `else next.reverseMesh = false` → `'reverseMesh' in out`
   * 变 true → 转红。四个键各一条，逐个钉。
   */
  it('空设置往返一圈后，四个新字段一个都不落到磁盘上', () => {
    const out = buildTsSettings({}, initTsDraft(node({})));
    expect(Object.prototype.hasOwnProperty.call(out, 'reverseMesh')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'resolveByName')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'acceptDefaultResolvers')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'relayServerPort')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'routes')).toBe(false);
  });

  it('开了才写，且只写 true（不写 false 占位）', () => {
    const d = { ...initTsDraft(node({})), reverseMesh: true, resolveByName: true };
    const out = buildTsSettings({}, d);
    expect(out.reverseMesh).toBe(true);
    expect(out.resolveByName).toBe(true);
  });

  it('从开到关：磁盘上的旧显式值被**删掉**，不是留一个 false', () => {
    const base: TailscaleSettings = { reverseMesh: true, resolveByName: true, routes: ['10.0.0.0/8'] };
    const out = buildTsSettings(base, { ...initTsDraft(node(base)), reverseMesh: false, resolveByName: false, routes: '' });
    expect(Object.prototype.hasOwnProperty.call(out, 'reverseMesh')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'resolveByName')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(out, 'routes')).toBe(false);
  });

  it('acceptDefaultResolvers 受 resolveByName 门控：关了按名解析就不留残值', () => {
    // 后端只在 resolve_by_name==Some(true) 的分支里读它（builder/dns.rs:1069/1214）——
    // 留一个孤立的 true 就是磁盘上躺着一个永不生效的值。
    const d = { ...initTsDraft(node({})), resolveByName: false, acceptDefaultResolvers: true };
    expect(Object.prototype.hasOwnProperty.call(buildTsSettings({}, d), 'acceptDefaultResolvers')).toBe(false);
    const d2 = { ...d, resolveByName: true };
    expect(buildTsSettings({}, d2).acceptDefaultResolvers).toBe(true);
  });

  it('relayServerPort 越界/0/非整数一律按未设（越界会让整份 UserConfig 反序列化失败）', () => {
    const mk = (v: unknown) => buildTsSettings({}, { ...initTsDraft(node({})), relayServerPort: v as number });
    for (const bad of [0, -1, 65536, 99999, 1.5, undefined]) {
      expect(
        Object.prototype.hasOwnProperty.call(mk(bad), 'relayServerPort'),
        `relayServerPort=${bad} 不该落盘`
      ).toBe(false);
    }
    expect(mk(1).relayServerPort).toBe(1);
    expect(mk(65535).relayServerPort).toBe(65535);
    expect(mk(41641).relayServerPort).toBe(41641);
  });

  /**
   * `listenPort` 与 `relayServerPort` 现在共用 `portOrUndefined` —— 正因为共用，这条不能省：
   * 抽公共判据的收益是「不漂」，代价是「一处改错两个字段一起坏」，所以两个字段都要各自有门。
   */
  it('listenPort 同一口径：越界/0/非整数按未设，合法值原样落盘', () => {
    const mk = (v: unknown) =>
      buildTsSettings({}, { ...initTsDraft(node({})), listenPort: v as number });
    for (const bad of [0, -1, 65536, 99999, 1.5, undefined]) {
      expect(
        Object.prototype.hasOwnProperty.call(mk(bad), 'listenPort'),
        `listenPort=${bad} 不该落盘`
      ).toBe(false);
    }
    expect(mk(1).listenPort).toBe(1);
    expect(mk(65535).listenPort).toBe(65535);
    expect(mk(41641).listenPort).toBe(41641);
  });

  it('listenPort 从设到清：磁盘上的旧值被删掉，不是留一个 0', () => {
    const base: TailscaleSettings = { listenPort: 41641 };
    const out = buildTsSettings(base, { ...initTsDraft(node(base)), listenPort: undefined });
    expect(Object.prototype.hasOwnProperty.call(out, 'listenPort')).toBe(false);
  });

  it('填了 routes 自动开 acceptRoutes（否则 tsnet 不接收这些段，路由白配）', () => {
    const d = { ...initTsDraft(node({})), routes: '192.168.50.0/24', acceptRoutes: false };
    const out = buildTsSettings({}, d);
    expect(out.acceptRoutes).toBe(true);
    expect(out.routes).toEqual(['192.168.50.0/24']);
  });

  it('routes 与 advertiseRoutes 各走各的，绝不互相灌入', () => {
    const d = { ...initTsDraft(node({})), routes: '10.1.0.0/24', advertiseRoutes: '10.2.0.0/24' };
    const out = buildTsSettings({}, d);
    expect(out.routes).toEqual(['10.1.0.0/24']);
    expect(out.advertiseRoutes).toEqual(['10.2.0.0/24']);
  });

  it('authKey 等未建模字段经 base 原样保全（本弹窗不该覆写 TsLoginDialog 的产物）', () => {
    const out = buildTsSettings({ authKey: 'tskey-auth-xxx' }, initTsDraft(node({})));
    expect(out.authKey).toBe('tskey-auth-xxx');
  });

  it('不为 Tailscale 写 allowInternet（该语义由 exitNode 派生，写了就是第二个真值源）', () => {
    const out = buildTsSettings({}, { ...initTsDraft(node({})), exitNode: 'peer-1' });
    expect(Object.prototype.hasOwnProperty.call(out, 'allowInternet')).toBe(false);
    expect(out.exitNode).toBe('peer-1');
  });
});

describe('invalidTsCidrs：口径与后端 sanitize_cidr_list 一致', () => {
  it('合法段全过', () => {
    const d = { ...initTsDraft(node({})), routes: '192.168.50.0/24, 10.0.0.0/8', advertiseRoutes: 'fd7a:115c:a1e0::/48' };
    expect(invalidTsCidrs(d)).toEqual([]);
  });

  it('后端会静默丢弃的那几类必须被前端逮住', () => {
    // 掩码越界 / 八位组越界 / 非 IP —— Rust is_valid_ip_cidr 判非法后 sanitize 直接剔除该条。
    const d = { ...initTsDraft(node({})), routes: '10.0.0.0/40, 300.300.300.300, abc', advertiseRoutes: '' };
    expect(invalidTsCidrs(d)).toEqual(['10.0.0.0/40', '300.300.300.300', 'abc']);
  });

  it('两个字段都查（此前只有 advertiseRoutes 有输入口，也一样没人校验过）', () => {
    const d = { ...initTsDraft(node({})), routes: '', advertiseRoutes: '1.2.3.4/33' };
    expect(invalidTsCidrs(d)).toEqual(['1.2.3.4/33']);
  });

  it('空输入不算非法（可选字段）', () => {
    expect(invalidTsCidrs(initTsDraft(node({})))).toEqual([]);
  });
});

// ════════════════════════════════════════════════════════════════════════════
// exitNodeOptions
// ════════════════════════════════════════════════════════════════════════════

/**
 * 出口下拉的候选构造 —— 本仓 vitest 是 `environment:'node'`（无 jsdom），组件层测不了，
 * 故「列不列 / 禁不禁 / 标不标」必须活在纯函数里才有门可守。
 *
 * 用 ASCII 假文案而非真译文：本测试断言的是**哪条注记出现在哪一行**，不是译得对不对
 * （译文齐不齐由 `i18n/locale-parity.test.ts` 守）。
 */
const L: ExitNodeLabels = {
  none: '<none>',
  custom: '<custom>',
  inUse: '<in-use>',
  offline: '<offline>',
  notAdvertised: '<not-adv>',
};

function peer(p: Partial<TailscaleStatusPeer> & { hostName: string }): TailscaleStatusPeer {
  return {
    ip: `100.64.0.${p.hostName.length}`,
    online: true,
    exitNode: false,
    exitNodeOption: true,
    active: false,
    ...p,
  };
}

/** 只取设备行（掐掉首尾的「无 / 自定义…」），断言时不必每条都数偏移。 */
const devices = (opts: readonly (readonly [string, string, boolean?])[]) => opts.slice(1, -1);
const labelOf = (opts: readonly (readonly [string, string, boolean?])[], value: string) =>
  opts.find((o) => o[0] === value)?.[1];
const disabledOf = (opts: readonly (readonly [string, string, boolean?])[], value: string) =>
  opts.find((o) => o[0] === value)?.[2];

describe('exitNodeOptions：列全部 peer（不再按 exitNodeOption 过滤）', () => {
  /**
   * 牙：把「列全部」改回「只列 exitNodeOption」（例如给 `peers` 加一道
   * `.filter((p) => p.exitNodeOption)`）→ 未广告的那台整行消失 → 本条转红。
   */
  it('未广告出口的 peer 也在列表里，且被禁用 + 标注原因', () => {
    const opts = exitNodeOptions([peer({ hostName: 'adv' }), peer({ hostName: 'plain', exitNodeOption: false })], '', L);
    expect(devices(opts).map((o) => o[0])).toEqual(['adv', 'plain']);
    expect(disabledOf(opts, 'plain')).toBe(true);
    expect(labelOf(opts, 'plain')).toContain('<not-adv>');
    // 可作出口的那台不禁用、也不带「未广告」注记。
    expect(disabledOf(opts, 'adv')).toBe(false);
    expect(labelOf(opts, 'adv')).not.toContain('<not-adv>');
  });

  it('首尾恒为「无 / 自定义…」，值分别是空串与哨兵', () => {
    const opts = exitNodeOptions([peer({ hostName: 'a' })], '', L);
    expect(opts[0]).toEqual(['', '<none>']);
    expect(opts[opts.length - 1]).toEqual([EXIT_CUSTOM, '<custom>']);
  });

  it('零 peer（核没跑 / 未登录）时只剩「无 + 自定义…」，不崩不空白', () => {
    expect(exitNodeOptions([], '', L)).toEqual([['', '<none>'], [EXIT_CUSTOM, '<custom>']]);
  });
});

describe('exitNodeOptions：三条注记各自独立叠加', () => {
  /** 牙：删掉 `if (peer.exitNode) parts.push(labels.inUse)` → 本条转红。 */
  it('使用中', () => {
    const opts = exitNodeOptions([peer({ hostName: 'cur', exitNode: true })], '', L);
    expect(labelOf(opts, 'cur')).toBe('cur · 100.64.0.3 · <in-use>');
  });

  /** 牙：删掉 `if (!peer.online) parts.push(labels.offline)` → 本条转红。 */
  it('离线（此前离线 peer 混在候选里，没有任何标记）', () => {
    const opts = exitNodeOptions([peer({ hostName: 'down', online: false })], '', L);
    expect(labelOf(opts, 'down')).toBe('down · 100.64.0.4 · <offline>');
  });

  /** 牙：删掉 `if (!peer.exitNodeOption) parts.push(labels.notAdvertised)` → 本条转红。 */
  it('未广告出口', () => {
    const opts = exitNodeOptions([peer({ hostName: 'plain', exitNodeOption: false })], '', L);
    expect(labelOf(opts, 'plain')).toBe('plain · 100.64.0.5 · <not-adv>');
  });

  /**
   * 牙：把三条改成互斥分支（`else if`）→ 只出现第一条 → 本条转红。
   * 一台机器可以同时「离线 · 未广告出口」，这正是 上游 注释点名的独立叠加。
   */
  it('离线 + 未广告出口同时命中时两条都出现，顺序固定', () => {
    const opts = exitNodeOptions(
      [peer({ hostName: 'dead', online: false, exitNodeOption: false })],
      '',
      L
    );
    expect(labelOf(opts, 'dead')).toBe('dead · 100.64.0.4 · <offline> · <not-adv>');
  });

  it('三条全中时按「使用中 · 离线 · 未广告出口」排列', () => {
    const opts = exitNodeOptions(
      [peer({ hostName: 'x', ip: '100.64.0.9', exitNode: true, online: false, exitNodeOption: false })],
      '',
      L
    );
    expect(labelOf(opts, 'x')).toBe('x · 100.64.0.9 · <in-use> · <offline> · <not-adv>');
  });

  it('在线 + 已广告 + 未使用 = 一条注记都不加（`hostName · ip` 原样）', () => {
    const opts = exitNodeOptions([peer({ hostName: 'ok', ip: '100.64.0.1' })], '', L);
    expect(labelOf(opts, 'ok')).toBe('ok · 100.64.0.1');
  });
});

describe('exitNodeOptions：当前已配置项恒豁免禁用', () => {
  /**
   * 牙：把 `peerDisabled` 里的 `&& !peerMatches(peer, savedExit)` 删掉 → 已配置行被禁用
   * → 用户既选不回来也看不出为什么 → 本条转红。
   */
  it('已配置的那台即使没广告出口也可选', () => {
    const opts = exitNodeOptions(
      [peer({ hostName: 'mine', exitNodeOption: false }), peer({ hostName: 'other', exitNodeOption: false })],
      'mine',
      L
    );
    expect(disabledOf(opts, 'mine')).toBe(false);
    expect(disabledOf(opts, 'other')).toBe(true);
    // 豁免只豁免「能不能选」，不粉饰事实：注记照旧写明未广告出口。
    expect(labelOf(opts, 'mine')).toContain('<not-adv>');
  });

  /** 牙：把 `peerMatches` 的 ip 分支删掉（只比 hostName）→ 本条转红。 */
  it('按 ip 配置也命中（sing-box exit_node 两种写法都合法），且不另生重复行', () => {
    const opts = exitNodeOptions([peer({ hostName: 'nas', ip: '100.64.0.7', exitNodeOption: false })], '100.64.0.7', L);
    expect(devices(opts)).toHaveLength(1);
    // 该行的选项值取「已配置值」，回显✓ 落在设备行上而不是另加一行裸 IP。
    expect(devices(opts)[0][0]).toBe('100.64.0.7');
    expect(disabledOf(opts, '100.64.0.7')).toBe(false);
    expect(labelOf(opts, '100.64.0.7')).toBe('nas · 100.64.0.7 · <not-adv>');
  });

  /**
   * 牙：把 `peerMatches` 开头的 `if (!saved) return false` 删掉 → ip 为空的 peer 会被
   * 「空配置值」误命中而豁免掉禁用 → 本条转红。
   */
  it('未配置（空串）不豁免任何人，含 ip 为空的 peer', () => {
    const opts = exitNodeOptions([peer({ hostName: 'noip', ip: '', exitNodeOption: false })], '', L);
    expect(disabledOf(opts, 'noip')).toBe(true);
  });
});

describe('exitNodeOptions：已保存值恒可寻址（Csel 的 value 必须命中某一行）', () => {
  /**
   * 牙：删掉末尾那段兜底行 → 核没跑时下拉触发器显示空白、已配置的出口凭空消失 → 本条转红。
   * 这一格**不是**禁用豁免能替代的：豁免只能救列表里已有的行，救不了空列表。
   */
  it('peers 为空时补一行原样值（核没跑的主场景）', () => {
    const opts = exitNodeOptions([], 'ghost-host', L);
    expect(opts).toEqual([['', '<none>'], ['ghost-host', 'ghost-host'], [EXIT_CUSTOM, '<custom>']]);
  });

  it('手填的 IP（自定义出口存盘后回显）同样补得到行', () => {
    const opts = exitNodeOptions([peer({ hostName: 'a' })], '100.99.99.99', L);
    expect(opts.map((o) => o[0])).toEqual(['', 'a', '100.99.99.99', EXIT_CUSTOM]);
  });

  it('已保存值命中列表里的 peer 时**不**补重复行', () => {
    const opts = exitNodeOptions([peer({ hostName: 'a' })], 'a', L);
    expect(opts.map((o) => o[0])).toEqual(['', 'a', EXIT_CUSTOM]);
  });

  it('哨兵值 `__custom__` 不当作已保存主机名补行', () => {
    const opts = exitNodeOptions([peer({ hostName: 'a' })], EXIT_CUSTOM, L);
    expect(opts.map((o) => o[0])).toEqual(['', 'a', EXIT_CUSTOM]);
  });
});

describe('exitNodeOptions：排序与去重（Csel 靠 value 唯一定位选中行）', () => {
  it('可作出口 → 在线 → 名称，可用出口不会被埋在禁用行中间', () => {
    const opts = exitNodeOptions(
      [
        peer({ hostName: 'z-adv-online' }),
        peer({ hostName: 'b-plain-online', exitNodeOption: false }),
        peer({ hostName: 'a-adv-offline', online: false }),
        peer({ hostName: 'a-plain-offline', online: false, exitNodeOption: false }),
        peer({ hostName: 'a-adv-online' }),
      ],
      '',
      L
    );
    expect(devices(opts).map((o) => o[0])).toEqual([
      'a-adv-online',
      'z-adv-online',
      'a-adv-offline',
      'b-plain-online',
      'a-plain-offline',
    ]);
  });

  /**
   * 牙：去掉去重（或把去重判据换回 hostName 之外的东西）→ 出现两个同值行 →
   * `Csel.tsx:129` 的 findIndex 永远命中第一行，第二行选不中、两行同时打勾 → 本条转红。
   */
  it('同一 peer 出现在多个 TS 节点的快照里只留一行', () => {
    const p = peer({ hostName: 'shared', ip: '100.64.0.6' });
    const opts = exitNodeOptions([p, { ...p }], '', L);
    expect(devices(opts)).toHaveLength(1);
  });

  it('同名不同机时留下的是「可作出口 / 在线」那一台（排序在前，去重取先）', () => {
    const opts = exitNodeOptions(
      [
        peer({ hostName: 'dup', ip: '100.64.0.1', online: false, exitNodeOption: false }),
        peer({ hostName: 'dup', ip: '100.64.0.2' }),
      ],
      '',
      L
    );
    expect(devices(opts)).toHaveLength(1);
    expect(labelOf(opts, 'dup')).toBe('dup · 100.64.0.2');
  });

  it('hostName 缺失退到 ip 作选项值；两者皆空的 peer 无法寻址，只能丢', () => {
    const opts = exitNodeOptions(
      [peer({ hostName: '', ip: '100.64.0.8' }), peer({ hostName: '', ip: '' })],
      '',
      L
    );
    expect(devices(opts).map((o) => o[0])).toEqual(['100.64.0.8']);
    expect(labelOf(opts, '100.64.0.8')).toBe('100.64.0.8');
    // 「无」那一行的空串值没有被 peer 抢走（否则选「无」等于选那台机器）。
    expect(opts[0]).toEqual(['', '<none>']);
  });

  it('不改动入参数组（组件里 peers 来自 useState，就地排序会是隐性写状态）', () => {
    const input = [peer({ hostName: 'b' }), peer({ hostName: 'a' })];
    exitNodeOptions(input, '', L);
    expect(input.map((p) => p.hostName)).toEqual(['b', 'a']);
  });
});
