/**
 * WgDialog 纯逻辑单测（vitest, node）：.conf 解析→表单草稿→ServerConfig 的接线与往返对称。
 * 解析器本体（`domain/wg-quick.ts`）不在此测（其归属批已覆盖）；本测锁定 D3 新增的映射/构造/校验。
 */

import { describe, expect, it } from 'vitest';
import {
  splitCsv,
  emptyWgDraft,
  parseConfToDraft,
  draftFromServer,
  buildWgServer,
  buildWarpSettings,
  validateWgDraft,
  parseReserved,
  reservedInputInvalid,
  isWarpDraft,
  type WgDraft,
} from './wg-logic';
import type { ServerConfig, WireGuardSettings } from '@/contracts/types';

const SAMPLE_CONF = `
[Interface]
PrivateKey = wOEabc=
Address = 10.0.0.2/32, fd00::2/128
MTU = 1420

[Peer]
PublicKey = HIgxyz=
PresharedKey = PSKkey=
Endpoint = 203.0.113.7:51820
AllowedIPs = 0.0.0.0/0, ::/0, 10.0.0.0/24
PersistentKeepalive = 30
`;

describe('splitCsv', () => {
  it('去空 trim + 过滤空段', () => {
    expect(splitCsv('a, b ,, c ')).toEqual(['a', 'b', 'c']);
    expect(splitCsv('')).toEqual([]);
    expect(splitCsv(undefined)).toEqual([]);
    expect(splitCsv(42)).toEqual([]);
  });
});

describe('parseConfToDraft (.conf → 草稿接线)', () => {
  it('合法 .conf → 字段填入；catch-all 归 allowInternet，allowedIPs 只留具体段', () => {
    const d = parseConfToDraft(SAMPLE_CONF);
    expect(d).not.toBeNull();
    const draft = d as WgDraft;
    expect(draft.address).toBe('203.0.113.7');
    expect(draft.port).toBe(51820);
    expect(draft.privateKey).toBe('wOEabc=');
    expect(draft.localAddress).toBe('10.0.0.2/32, fd00::2/128');
    expect(draft.peerPublicKey).toBe('HIgxyz=');
    expect(draft.preSharedKey).toBe('PSKkey=');
    // 0.0.0.0/0 与 ::/0 抽走 → 只剩 10.0.0.0/24
    expect(draft.allowedIPs).toBe('10.0.0.0/24');
    expect(draft.allowInternet).toBe(true);
    expect(draft.persistentKeepalive).toBe(30);
    expect(draft.mtu).toBe(1420);
  });

  it('缺必填（无 Endpoint）→ null（提示改手填）', () => {
    const bad = '[Interface]\nPrivateKey = x\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = y';
    expect(parseConfToDraft(bad)).toBeNull();
  });

  it('无 catch-all → allowInternet false，具体段全留', () => {
    const conf =
      '[Interface]\nPrivateKey = p\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = q\nEndpoint = h:1\nAllowedIPs = 10.0.0.0/24, 192.168.0.0/16';
    const d = parseConfToDraft(conf) as WgDraft;
    expect(d.allowInternet).toBe(false);
    expect(d.allowedIPs).toBe('10.0.0.0/24, 192.168.0.0/16');
  });
});

describe('buildWgServer (草稿 → ServerConfig)', () => {
  it('组装 protocol/address/port + wireguardSettings；两开关默认开', () => {
    const draft = parseConfToDraft(SAMPLE_CONF) as WgDraft;
    const s = buildWgServer('自建 WG', draft);
    expect(s.protocol).toBe('wireguard');
    expect(s.name).toBe('自建 WG');
    expect(s.address).toBe('203.0.113.7');
    expect(s.port).toBe(51820);
    expect(s.id).toBe(''); // 新增态无 base
    const ws = s.wireguardSettings!;
    expect(ws.privateKey).toBe('wOEabc=');
    expect(ws.localAddress).toEqual(['10.0.0.2/32', 'fd00::2/128']);
    expect(ws.peerPublicKey).toBe('HIgxyz=');
    expect(ws.preSharedKey).toBe('PSKkey=');
    expect(ws.allowedIPs).toEqual(['10.0.0.0/24']);
    expect(ws.allowInternet).toBe(true);
    expect(ws.alwaysRouteSubnets).toBe(true);
    expect(ws.persistentKeepalive).toBe(30);
    expect(ws.mtu).toBe(1420);
  });

  it('空 port → 兜底 51820（不硬塞 0）；空 psk/allowed 不落键', () => {
    const draft: WgDraft = { ...emptyWgDraft(), address: 'h', privateKey: 'p', localAddress: '10.0.0.2/32', peerPublicKey: 'q', port: undefined };
    const s = buildWgServer('n', draft);
    expect(s.port).toBe(51820);
    expect(s.wireguardSettings!.preSharedKey).toBeUndefined();
    expect(s.wireguardSettings!.allowedIPs).toBeUndefined();
  });

  it('编辑态：base.id / warpDevice 等非表单字段被起底保全（R5）', () => {
    const base: ServerConfig = {
      id: 'srv-1',
      name: 'old',
      protocol: 'wireguard',
      address: '1.1.1.1',
      port: 1,
      wireguardSettings: {
        privateKey: 'old',
        localAddress: ['10.0.0.9/32'],
        peerPublicKey: 'oldpub',
        warpDevice: { deviceId: 'd', token: 't' },
      },
    };
    const draft = draftFromServer(base);
    const s = buildWgServer('newname', { ...draft, address: '2.2.2.2' }, base);
    expect(s.id).toBe('srv-1');
    expect(s.address).toBe('2.2.2.2');
    // warpDevice 非表单字段，经 base.wireguardSettings 起底保全
    expect(s.wireguardSettings!.warpDevice).toEqual({ deviceId: 'd', token: 't' });
  });
});

describe('往返对称 (parse → build → draftFromServer)', () => {
  it('关键字段回填一致', () => {
    const draft0 = parseConfToDraft(SAMPLE_CONF) as WgDraft;
    const server = buildWgServer('rt', draft0);
    const draft1 = draftFromServer(server);
    expect(draft1.address).toBe(draft0.address);
    expect(draft1.port).toBe(draft0.port);
    expect(draft1.privateKey).toBe(draft0.privateKey);
    expect(draft1.localAddress).toBe(draft0.localAddress);
    expect(draft1.peerPublicKey).toBe(draft0.peerPublicKey);
    expect(draft1.allowedIPs).toBe(draft0.allowedIPs);
    expect(draft1.allowInternet).toBe(draft0.allowInternet);
  });
});

/**
 * 接入模式（reverseMesh）——「缺省即默认」这条纪律**没有任何自动门守得住**：类型/build/覆盖门
 * 都只看得见「有没有这个键」，看不见「没开时写没写 false」。故本组是它唯一的牙。
 * 判据出处见 `wg-logic.ts` 文件头（回显口径 = 消费侧 `meshUsesSystemInterface` 的 `=== true`）。
 */
describe('reverseMesh：缺省即默认', () => {
  const wgNode = (ws: WireGuardSettings): ServerConfig => ({
    id: 'w',
    name: 'n',
    protocol: 'wireguard',
    address: '203.0.113.7',
    port: 51820,
    wireguardSettings: ws,
  });
  const base: WireGuardSettings = { privateKey: 'p', localAddress: ['10.0.0.2/32'], peerPublicKey: 'q' };
  /** 提交必填齐，否则测的就不是本字段。 */
  const filled = (d: WgDraft): WgDraft => ({
    ...d,
    address: d.address || '203.0.113.7',
    privateKey: 'p',
    localAddress: '10.0.0.2/32',
    peerPublicKey: 'q',
  });

  it('缺席回显 false（不是 true，也不是 undefined）', () => {
    // 牙：把 `s?.reverseMesh === true` 写成 `!== false` → 缺席变 true → 红。
    expect(draftFromServer(wgNode(base)).reverseMesh).toBe(false);
    expect(emptyWgDraft().reverseMesh).toBe(false);
    expect((parseConfToDraft(SAMPLE_CONF) as WgDraft).reverseMesh).toBe(false);
  });

  it('存量 true 回显 true（起底值不能在编辑态丢失）', () => {
    expect(draftFromServer(wgNode({ ...base, reverseMesh: true })).reverseMesh).toBe(true);
  });

  it('用户没开 → **不落键**（写 false 就是把当下默认复制进磁盘）', () => {
    // 牙：把 `else delete settings.reverseMesh` 改成 `else settings.reverseMesh = false` → 红。
    const s = buildWgServer('n', filled(emptyWgDraft()));
    expect(Object.prototype.hasOwnProperty.call(s.wireguardSettings!, 'reverseMesh')).toBe(false);
  });

  it('关掉存量 true → 删键（`...base` 起底会把旧值带过来，不删就关不掉）', () => {
    // 牙：删掉 `else delete` 那一支 → 旧 true 经 `...base?.wireguardSettings` 幸存 → 红。
    const node = wgNode({ ...base, reverseMesh: true });
    const draft = { ...draftFromServer(node), reverseMesh: false };
    const s = buildWgServer('n', draft, node);
    expect(Object.prototype.hasOwnProperty.call(s.wireguardSettings!, 'reverseMesh')).toBe(false);
  });

  it('用户开了 → 落 true', () => {
    const s = buildWgServer('n', { ...filled(emptyWgDraft()), reverseMesh: true });
    expect(s.wireguardSettings!.reverseMesh).toBe(true);
  });

  it('往返：true 存下去再读回来仍是 true', () => {
    const first = buildWgServer('n', { ...filled(emptyWgDraft()), reverseMesh: true });
    expect(draftFromServer(first).reverseMesh).toBe(true);
  });
});

/**
 * WARP 否决 —— WARP 的 `system:true` 会与主 TUN 抢内核 utun → `Connect: resource busy` FATAL。
 *
 * 「读」那一侧现在两端都已否决（渲染端 `meshUsesSystemInterface` + Rust
 * `builder/endpoint_routes.rs:120` 的 `crate::warp::is_warp_server`，同源由
 * `contracts/warp-veto-parity.test.ts` 守）。**但那守的是不发射，不是不落盘**：一个
 * `reverseMesh:true` 的 WARP 节点仍能躺在 config.json 里，随下一次编辑经 `...base` 复制下去。
 * 控件禁用同样只挡新写入。故提交侧必须自己再拦一道 —— 本组就是它的牙。
 */
describe('reverseMesh：WARP 恒 gVisor', () => {
  const WARP_ADDR = 'engage.cloudflareclient.com';
  const draft = (over: Partial<WgDraft>): WgDraft => ({
    ...emptyWgDraft(),
    address: '203.0.113.7',
    privateKey: 'p',
    localAddress: '10.0.0.2/32',
    peerPublicKey: 'q',
    ...over,
  });

  it('按端点域名判 WARP（旧/导入的 WARP 无 warpDevice 标记）', () => {
    expect(isWarpDraft(draft({ address: WARP_ADDR }))).toBe(true);
    expect(isWarpDraft(draft({}))).toBe(false);
  });

  it('按 base.warpDevice 判 WARP（地址被改过也仍是 WARP）', () => {
    const base: ServerConfig = {
      id: 'w',
      name: 'n',
      protocol: 'wireguard',
      address: WARP_ADDR,
      port: 2408,
      wireguardSettings: { privateKey: 'p', localAddress: ['10.0.0.2/32'], peerPublicKey: 'q', warpDevice: { deviceId: 'd', token: 't' } },
    };
    expect(isWarpDraft(draft({ address: '203.0.113.7' }), base)).toBe(true);
  });

  it('WARP 草稿即使 reverseMesh=true 也不落键', () => {
    // 牙：把 `&& !isWarpDraft(draft, base)` 去掉 → WARP 落 reverseMesh:true → 红。
    const s = buildWgServer('warp', draft({ address: WARP_ADDR, reverseMesh: true }));
    expect(Object.prototype.hasOwnProperty.call(s.wireguardSettings!, 'reverseMesh')).toBe(false);
  });
});

/**
 * WarpDialog 的提交腿 —— `buildWarpSettings`。
 *
 * 这里钉的核心是**否决与控件无关**：WARP 弹窗不展示 System 接入模式，但真正会把
 * `reverseMesh:true` 送上盘的是 `...base` ——
 * 存量值来自导入配置 / 手改 config.json / 从 上游 迁移这三条**不经渲染端**的入口。
 * 故下面每一条都刻意让 `base` 带着 true 进来。
 */
describe('buildWarpSettings：WARP 提交腿恒否决 System 接入模式', () => {
  const baseDraft = { mtu: undefined, keepalive: undefined };

  it('base 带 reverseMesh:true（导入/手改/迁移的存量值）→ 提交后不落键', () => {
    // 牙：删掉 `delete s.reverseMesh` → true 经 `...base` 幸存 → 红。
    const s = buildWarpSettings({ privateKey: 'p', reverseMesh: true }, { ...baseDraft });
    expect(Object.prototype.hasOwnProperty.call(s, 'reverseMesh')).toBe(false);
  });

  it('draft 里硬塞 reverseMesh:true（绕过禁用控件）→ 同样不落键', () => {
    const s = buildWarpSettings({ privateKey: 'p' }, { ...baseDraft, reverseMesh: true });
    expect(Object.prototype.hasOwnProperty.call(s, 'reverseMesh')).toBe(false);
  });

  it('删的是键不是写 false —— 缺省即默认，不在盘上造第二个默认值真值源', () => {
    const s = buildWarpSettings({ reverseMesh: true }, { ...baseDraft });
    expect(s.reverseMesh).toBeUndefined();
    expect(JSON.stringify(s)).not.toContain('reverseMesh');
  });

  it('阴性对照：否决只针对 reverseMesh，base 的其余字段原样带过', () => {
    // 防「一刀切把 base 清了」——注册态的 warpDevice / reserved 丢了 = WARP 连得上但不通。
    const s = buildWarpSettings(
      { privateKey: 'p', peerPublicKey: 'q', reserved: [1, 2, 3], warpDevice: { deviceId: 'd', token: 't' }, reverseMesh: true },
      { ...baseDraft }
    );
    expect(s.privateKey).toBe('p');
    expect(s.peerPublicKey).toBe('q');
    expect(s.reserved).toEqual([1, 2, 3]);
    expect(s.warpDevice).toEqual({ deviceId: 'd', token: 't' });
  });

  it('MTU 与保活可覆盖；0 保活保留为关闭语义', () => {
    const s = buildWarpSettings({}, { ...baseDraft, mtu: 1400, keepalive: 0 });
    expect(s.mtu).toBe(1400);
    expect(s.persistentKeepalive).toBe(0);
  });

  it('留空恢复协议默认；注册下发的 reserved 仍原样保留', () => {
    const s = buildWarpSettings(
      { mtu: 1400, persistentKeepalive: 30, reserved: [5, 6, 7] },
      { ...baseDraft }
    );
    expect(s.mtu).toBeUndefined();
    expect(s.persistentKeepalive).toBeUndefined();
    expect(s.reserved).toEqual([5, 6, 7]);
  });

  it('旧 WARP 的自定义 AllowedIPs/路由开关被清理，回到全隧道 peer 缺省语义', () => {
    const s = buildWarpSettings(
      {
        allowedIPs: ['10.0.0.0/24'],
        allowInternet: false,
        alwaysRouteSubnets: false,
      },
      { ...baseDraft, route: 'custom', allowedIPs: '192.168.0.0/16' }
    );
    expect(s.allowedIPs).toBeUndefined();
    expect(s.allowInternet).toBeUndefined();
    expect(s.alwaysRouteSubnets).toBeUndefined();
  });
});

/**
 * Reserved —— 缺省即默认 + 前端校验口径 = 后端消费侧谓词。
 *
 * 这一族没有任何自动门守得住（覆盖门只问「键名在编辑器文件里出现过没有」），故逐条钉在这里。
 */
describe('reserved：缺省即默认 + 口径对齐消费侧谓词', () => {
  const wgNode = (ws: WireGuardSettings): ServerConfig => ({
    id: 'w',
    name: 'n',
    protocol: 'wireguard',
    address: '203.0.113.7',
    port: 51820,
    wireguardSettings: ws,
  });
  const base: WireGuardSettings = { privateKey: 'p', localAddress: ['10.0.0.2/32'], peerPublicKey: 'q' };
  const filled = (d: WgDraft): WgDraft => ({
    ...d,
    address: '203.0.113.7',
    privateKey: 'p',
    localAddress: '10.0.0.2/32',
    peerPublicKey: 'q',
  });
  const hasReserved = (s: ServerConfig) =>
    Object.prototype.hasOwnProperty.call(s.wireguardSettings!, 'reserved');

  it('parseReserved 只收「恰 3 项 × 0–255 整数」', () => {
    // 牙：把 `parts.length !== 3` 删掉 → 前两条转红；把 `n <= 255` 放宽 → 第 4 条转红；
    //     把 `Number.isInteger` 换成 `Number.isFinite` → 第 5、6 条转红。
    expect(parseReserved('1, 2, 3')).toEqual([1, 2, 3]);
    expect(parseReserved('0,0,0')).toEqual([0, 0, 0]); // 全 0 是合法值，不等于「没填」
    expect(parseReserved('')).toBeUndefined();
    expect(parseReserved('1, 2')).toBeUndefined();
    expect(parseReserved('1, 2, 3, 4')).toBeUndefined();
    expect(parseReserved('1, 2, 256')).toBeUndefined();
    expect(parseReserved('1, 2, -1')).toBeUndefined(); // Vec<u32>：负数会让整条 IPC 反序列化失败
    expect(parseReserved('1, 2, 1.5')).toBeUndefined();
    expect(parseReserved('a, b, c')).toBeUndefined();
    expect(parseReserved(undefined)).toBeUndefined();
    // 上游的 filter-then-count 会把这串悄悄改写成 [1,2,3]；此处整体作废。
    expect(parseReserved('1, 2, 999, 3')).toBeUndefined();
  });

  it('reservedInputInvalid：空 = 合法（没填），填错才拦', () => {
    expect(reservedInputInvalid('')).toBe(false);
    expect(reservedInputInvalid('   ')).toBe(false);
    expect(reservedInputInvalid(undefined)).toBe(false);
    expect(reservedInputInvalid('1, 2, 3')).toBe(false);
    expect(reservedInputInvalid('1, 2')).toBe(true);
    expect(reservedInputInvalid('1, 2, 256')).toBe(true);
  });

  it('缺席回显空串（不是 "0, 0, 0"，也不是 undefined）', () => {
    expect(draftFromServer(wgNode(base)).reserved).toBe('');
    expect(emptyWgDraft().reserved).toBe('');
    // wg-quick .conf 里没有 Reserved 这个键 ⇒ 解析来的草稿恒空。
    expect((parseConfToDraft(SAMPLE_CONF) as WgDraft).reserved).toBe('');
  });

  it('存量值原样回显（含不满足谓词的残值 —— 先看得见才改得掉）', () => {
    expect(draftFromServer(wgNode({ ...base, reserved: [10, 20, 30] })).reserved).toBe('10, 20, 30');
    expect(draftFromServer(wgNode({ ...base, reserved: [1, 2] })).reserved).toBe('1, 2');
  });

  it('用户没填 → **不落键**（不是 `[]`、不是 `[0,0,0]`）', () => {
    // 牙：把 `else delete settings.reserved` 改成 `else settings.reserved = []` → 红。
    expect(hasReserved(buildWgServer('n', filled(emptyWgDraft())))).toBe(false);
  });

  it('填了合法值 → 落 3 项', () => {
    const s = buildWgServer('n', { ...filled(emptyWgDraft()), reserved: '10, 20, 30' });
    expect(s.wireguardSettings!.reserved).toEqual([10, 20, 30]);
  });

  it('清空存量值 → 删键（`...base` 起底会把旧值带过来，不删就清不掉）', () => {
    // 牙：删掉 `else delete` 那一支 → 旧 [10,20,30] 经 `...base?.wireguardSettings` 幸存 → 红。
    const node = wgNode({ ...base, reserved: [10, 20, 30] });
    const s = buildWgServer('n', { ...draftFromServer(node), reserved: '' }, node);
    expect(hasReserved(s)).toBe(false);
  });

  it('往返：存下去再读回来逐字一致', () => {
    const first = buildWgServer('n', { ...filled(emptyWgDraft()), reserved: '1, 2, 3' });
    expect(draftFromServer(first).reserved).toBe('1, 2, 3');
  });

  it('WARP 节点经 WgDialog 编辑：CF 下发的 3 字节不因保存而丢失', () => {
    // 注册腿写下的 reserved 来自 client_id（`mesh/warp.rs:179`，恒 3 字节）⇒ 恰好满足谓词 ⇒
    // 用户在 WG 弹窗里改别的字段并保存时，它必须原样留下。
    const node = wgNode({ ...base, reserved: [5, 6, 7], warpDevice: { deviceId: 'd', token: 't' } });
    const s = buildWgServer('warp', { ...draftFromServer(node), mtu: 1280 }, node);
    expect(s.wireguardSettings!.reserved).toEqual([5, 6, 7]);
  });
});

describe('validateWgDraft', () => {
  it('名称空 → name', () => {
    expect(validateWgDraft('', emptyWgDraft())).toEqual({ field: 'name' });
  });
  it('有名无地址 → address', () => {
    expect(validateWgDraft('n', emptyWgDraft())).toEqual({ field: 'address' });
  });
  it('全填 → null', () => {
    const draft = parseConfToDraft(SAMPLE_CONF) as WgDraft;
    expect(validateWgDraft('n', draft)).toBeNull();
  });
});
