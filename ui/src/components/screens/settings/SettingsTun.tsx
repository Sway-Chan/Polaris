/**
 * SettingsTun —— TUN 子页（原型 [data-sec="tun"] L2221-2266）。
 *
 * 四块：
 *  1. TUN 接管：协议栈（Auto/Mixed/gVisor/System）+ 自动路由 + 严格路由 + IPv6（+ FakeIP 联动提示）
 *     + <details> 三平台机制与建议（原生元素，靠 [open] 驱动折叠箭头，非自绘按钮）
 *  2. 排除网段（route_exclude / bypassLANList CIDR）
 *  3. 连入来源排除（inboundExcludeCidrs）+ Linux-only 提示（纯 CSS `:root[data-os="lin"] .plat-warn`门控）
 *  4. 局域网网关（契约 L102）：邻居短名解析 neighborDomains（TUN + Linux/macOS）
 *     + TUN MAC 过滤 macFilterMode/macFilterList（仅 Linux）
 *
 * 配置落在 config.tunConfig（TunModeConfig）+ config.enableIPv6 + config.dnsConfig.enableFakeIp。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import type { UserConfig, TunModeConfig, TunStack, UdpNatType } from '@/contracts/types';
import { isValidMacAddress, isValidNeighborDomain } from '@/domain/neighbor';
import { autoMtuFor, MTU_MAX, MTU_MIN, parseMtuInput } from '@/domain/tun-mtu';
import { useNavStore } from '@/store/nav-store';
import { Fold } from '@/components/Fold';
import { Phead, SetBlock, SetRow, Switch, Select, TextInput, Button, Pill } from './primitives';
import { ListEditor } from './ListEditor';
import { bypassLanState, shellPlatformFromDataOs } from './settings-logic';
import { revealOnToggle } from '@/components/reveal';

export interface SettingsTunProps {
  config: UserConfig;
  update: (patch: Partial<UserConfig>) => Promise<void>;
}

const STACK_OPTIONS: { value: TunStack; label: string }[] = [
  { value: 'auto', label: 'Auto' },
  { value: 'mixed', label: 'Mixed' },
  { value: 'gvisor', label: 'gVisor' },
  { value: 'system', label: 'System' },
];

/**
 * NAT 类型档。`'default'` 是**只存在于这颗控件里**的哨兵，落库时映射回 `udpNatType: undefined`
 * （删键）—— 同 `macFilterMode` 的 `'off'` 档，理由也同：Csel 的 value 是字符串，表达不了 undefined。
 *
 * 值的顺序即「松 → 严」，与 desc 里那句排序说法必须一致。协议栈那颗下拉的 label 是产品名
 * （Auto/Mixed/gVisor/System，跨语种同形故不进 locale），这颗不是 —— 「受限锥」是要翻译的，
 * 故 label 走 i18n key。
 */
const NAT_TYPE_OPTIONS: { value: UdpNatType | 'default'; key: string }[] = [
  { value: 'default', key: 'settings.tun.natTypeDefault' },
  { value: 'fullCone', key: 'settings.tun.natTypeFullCone' },
  { value: 'restrictedCone', key: 'settings.tun.natTypeRestricted' },
  { value: 'portRestrictedCone', key: 'settings.tun.natTypePortRestricted' },
];

/**
 * 清单里是否存在「非空但不合法」的条目。
 *
 * 空白条目**不报错**：那是 ListEditor 的编辑中间态（刚点「添加」得到的空行），对它报错会让用户
 * 每次点添加都先挨一句红字（同 `SettingsDns` 自定义 DoH 上游那条提示的取舍）。
 */
function hasInvalidEntry(
  list: readonly string[],
  isValid: (v: string) => boolean,
): boolean {
  return list.some((v) => v.trim() !== '' && !isValid(v));
}

export default function SettingsTun({ config, update }: SettingsTunProps) {
  const { t } = useTranslation();
  const navigate = useNavStore((s) => s.navigate);
  // 排除网段清单与「网络」页旁路清单同源于 bypassLANList，故同受 bypassLAN 总开关管辖（见下方块注释）。
  const bypassLan = bypassLanState(config);
  const tun: TunModeConfig = config.tunConfig ?? {
    stack: 'auto',
    autoRoute: true,
    strictRoute: true,
  };

  function patchTun(patch: Partial<TunModeConfig>) {
    void update({ tunConfig: { ...tun, ...patch } });
  }

  // 折叠段计数与编辑器清单取同一个数组，防「计数说 3 条、点开是 0 条」的分叉。
  const bypassList = config.bypassLANList ?? ['localhost', '127.0.0.1', '192.168.0.0/16'];
  const inboundExcludeCidrs = tun.inboundExcludeCidrs ?? ['100.64.0.0/10'];

  // MTU 输入草稿。**不能每键落库**：输入 4064 的中途会经过 "4"/"40"/"406"，逐键提交就是逐键
  // 写盘 + 逐键判非法（"4" 越界标红），且中间那几个值都会真的进配置。故本地持草稿，失焦/回车才提交。
  const [mtuDraft, setMtuDraft] = useState(tun.mtu === undefined ? '' : String(tun.mtu));
  const [mtuInvalid, setMtuInvalid] = useState(false);
  // 外部改动（配置回显、导入备份、另一处改了 tunConfig）要同步进草稿，否则框里留着陈旧值。
  useEffect(() => {
    setMtuDraft(tun.mtu === undefined ? '' : String(tun.mtu));
    setMtuInvalid(false);
  }, [tun.mtu]);

  /** 清空 = 回到自动：**删键**而不是写 `undefined`，落盘才是真缺席（见 contracts/types.ts 的注释）。 */
  function commitMtu() {
    const parsed = parseMtuInput(mtuDraft);
    if (parsed.invalid) {
      setMtuInvalid(true);
      return;
    }
    setMtuInvalid(false);
    const next: TunModeConfig = { ...tun };
    if (parsed.mtu === undefined) delete next.mtu;
    else next.mtu = parsed.mtu;
    void update({ tunConfig: next });
  }

  /**
   * NAT 类型落库。回到「跟随内核默认」走**删键**而不是写 `undefined` —— 与 `commitMtu` 同一条理由：
   * `patchTun` 的 `{...tun, ...patch}` 会把 `undefined` 当成一个存在的键铺进对象，落盘后是
   * `"udpNatType": null` 而不是缺席，Rust 侧 `Option` 反序列化虽仍是 `None`、但配置文件与「从未设过」
   * 不再逐字节相同（金样与 diff 都看得见）。
   */
  function commitNatType(v: UdpNatType | 'default') {
    const next: TunModeConfig = { ...tun };
    if (v === 'default') delete next.udpNatType;
    else next.udpNatType = v;
    void update({ tunConfig: next });
  }

  const isTun = config.proxyModeType === 'tun';
  const interceptLabel = isTun
    ? 'TUN'
    : config.proxyModeType === 'systemProxy'
      ? t('settings.tun.interceptSystemProxy')
      : t('settings.tun.interceptManual');
  // 局域网网关（契约 L102）：邻居短名解析仅 Linux/macOS 内核有实现，MAC 过滤仅 Linux；均只在 TUN 下有义。
  // 平台取不到时本块 **fail-closed（不渲染）**：这里判定的是「内核层是否真支持」——Windows 上邻居解析器
  // 根本没有实现（`neighbor_resolver_stub.go` 直接 ErrInvalid）、MAC 过滤发射即 FATAL，错判成可用 =
  // 让用户填一份永不生效的清单，比少显示一个块糟得多（同 RuleDialog 的取向）。
  const platform = shellPlatformFromDataOs();
  const showsLanGateway = isTun && (platform === 'lin' || platform === 'mac');
  const neighborDomains = tun.neighborDomains ?? [];
  const macFilterList = tun.macFilterList ?? [];
  // IPv6 开启但 FakeIP 关闭时，节点若不支持 IPv6 可能连不上——提示一键开 FakeIP（仅 TUN 下有意义）
  const showIpv6Hint = isTun && !!config.enableIPv6 && !(config.dnsConfig?.enableFakeIp ?? true);

  return (
    <section className="screen" data-sec="tun">
      <Phead title="TUN" sub={t('settings.tun.pageSub')} />

      <div className="tun-anchor">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 3l7 3v6c0 4.5-3 7.5-7 9-4-1.5-7-4.5-7-9V6z" />
        </svg>
        <span id="tun-anchor-tx">{t('settings.tun.currentIntercept', { mode: interceptLabel })}</span>
        {/* 原型 `goto-home-intercept`（:4052）不是「只导航」：落地首页后还要在接管方式分段控件上
            冒一枚「在此切换接管方式」浮标。**先落意图再导航**，顺序反了首页会在意图落库前挂载完
            （同 HomeScreen.goServerPage 的理由）。 */}
        <a
          role="button"
          tabIndex={0}
          onClick={() => {
            navigate('home');
          }}
        >
          {t('settings.tun.switchOnHome')}
        </a>
      </div>

      {/* 1. TUN 接管 */}
      <SetBlock header={t('settings.tun.takeoverBlock')}>
        <SetRow label={t('settings.tun.stack')} desc={t('settings.tun.stackDesc')}>
          <Select
            id="tun-stack-sel"
            value={tun.stack}
            onChange={(e) => patchTun({ stack: e.target.value as TunStack })}
            aria-label="TUN stack"
            style={{ width: '150px' }}
          >
            {STACK_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </Select>
        </SetRow>
        {/* MTU 紧贴协议栈：默认 MTU 是**栈的函数**（gvisor 吃得下 65535，system/mixed 在 65535 下
            塌到 11 Mbps），两项分开放会让「换了栈占位符里的数也变了」显得莫名其妙。 */}
        <SetRow
          label="MTU"
          desc={t('settings.tun.mtuDesc')}
        >
          <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: '4px' }}>
            <TextInput
              id="tun-mtu"
              type="text"
              inputMode="numeric"
              className="mono"
              value={mtuDraft}
              placeholder={t('settings.tun.mtuAutoPlaceholder', {
                n: autoMtuFor(tun.stack, platform),
              })}
              onChange={(e) => {
                setMtuDraft(e.target.value);
                setMtuInvalid(false);
              }}
              onBlur={commitMtu}
              onKeyDown={(e) => {
                if (e.key === 'Enter') e.currentTarget.blur();
              }}
              aria-label="TUN MTU"
              aria-invalid={mtuInvalid || undefined}
              style={{ width: '150px' }}
            />
            {mtuInvalid && (
              <div className="err-line" style={{ marginTop: 0 }}>
                {t('settings.tun.mtuInvalid', { min: MTU_MIN, max: MTU_MAX })}
              </div>
            )}
          </div>
        </SetRow>
        <SetRow label={t('settings.tun.autoRoute')} desc={t('settings.tun.autoRouteDesc')}>
          <Switch checked={tun.autoRoute} onChange={(v) => patchTun({ autoRoute: v })} />
        </SetRow>
        <SetRow
          label={t('settings.tun.strictRoute')}
          desc={t('settings.tun.strictRouteDesc')}
        >
          <Switch checked={tun.strictRoute} onChange={(v) => patchTun({ strictRoute: v })} />
        </SetRow>
        {/* NAT 类型：内核侧是 udp_mapping × udp_filtering 两个字段，这里刻意收成**一颗**下拉。
            两个字段各 3 个取值 = 9 种组合，只有 4 种对应真实 NAT 语义，逐字段暴露等于把 5 个必然无意义
            的格子摆给用户，而用户认得的词是「NAT 类型」不是 udp_mapping。同页 macFilterMode 已是同一
            形态（一颗下拉 → include/exclude 两个互斥内核字段），这里跟它，不另起一套。
            映射表 SoT 在 Rust（builder/inbounds.rs `udp_nat_behaviors`），前端只传档名——照抄一份到
            这里就会有两份表，且没有任何门守它们相等。
            宽度 190 而非邻居的 150：ru 的「Ограниченный конус」在 150px 下会被 .csv 的 ellipsis 截断，
            而档名被截断正是这颗控件唯一要避免的事（选项看不全 = 选不对）。 */}
        <SetRow label={t('settings.tun.natType')} desc={t('settings.tun.natTypeDesc')}>
          <Select
            id="tun-nat-type"
            value={tun.udpNatType ?? 'default'}
            onChange={(e) => commitNatType(e.target.value as UdpNatType | 'default')}
            aria-label={t('settings.tun.natType')}
            style={{ width: '190px' }}
          >
            {NAT_TYPE_OPTIONS.map((o) => (
              <option key={o.value} value={o.value}>
                {t(o.key)}
              </option>
            ))}
          </Select>
        </SetRow>
        {/* 手写而非 SetRow：sr-tx 下需要 desc 文案 + ipv6-hint 两个平级 div（原型 L2230-2232），
            SetRow 的 desc 只支持单层包裹，容不下这个三段结构。 */}
        <div className="set-row" style={{ alignItems: 'flex-start', borderBottom: 0 }}>
          <div className="sr-tx">
            <b>{t('settings.general.enableIPv6')}</b>
            <div>{t('settings.network.enableIPv6Desc')}</div>
            <div className="ipv6-hint" hidden={!showIpv6Hint}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <circle cx="12" cy="12" r="9" />
                <path d="M12 8v5M12 16h.01" />
              </svg>
              <span>{t('settings.network.ipv6NodeFakeIpHint')}</span>
              <Button
                variant="ghost"
                size="sm"
                onClick={() =>
                  void update({
                    dnsConfig: {
                      ...(config.dnsConfig ?? { domesticDns: '', foreignDns: '', enableFakeIp: true }),
                      enableFakeIp: true,
                    },
                  })
                }
              >
                <span>{t('settings.network.enableFakeIpAction')}</span>
              </Button>
            </div>
          </div>
          <div className="sr-ctl">
            {/* 开关自身即反馈（拨动即翻位，不发 toast）——**这一颗不再是例外**。
                此前照原型 `:4107` 额外 notify「已启用 IPv6（不建议）」，理由是「开关翻到开说不出
                风险忠告」。忠告现已由上方**常驻的 desc 文案**承担（写明「出口节点不支持 IPv6 时，
                走 IPv6 的连接会一直等到超时」），且常驻优于 toast：toast 几秒即散，事后回看这颗开关
                读不到任何风险信息；desc 一直在，什么时候看都在。故本开关回到通用基线，两个方向都不发 toast。 */}
            <Switch
              id="ipv6-swt"
              checked={!!config.enableIPv6}
              onChange={(v) => {
                void update({ enableIPv6: v });
              }}
              aria-label="Enable IPv6"
            />
          </div>
        </div>

        {/* 三平台机制与建议（原生 <details>，折叠箭头由 CSS [open] 驱动，非自绘按钮态） */}
        <details className="tun-details" onToggle={revealOnToggle}>
          <summary>{t('settings.tun.detailsSummary')}</summary>
          {/* 每条的 `<b>` 里是协议栈名 / 平台名（Mixed·gVisor·System·Auto·macOS·Windows·Linux）——
              产品名与平台名跨语种同形，不进 locale；破折号之后的说明才是文案，逐条走键。 */}
          <div className="tun-details-body">
            <div className="tun-det-h">{t('settings.tun.detStackHead')}</div>
            <div>
              <b>Mixed</b> — {t('settings.tun.detStackMixed')}
            </div>
            <div>
              <b>gVisor</b> — {t('settings.tun.detStackGvisor')}
            </div>
            <div>
              <b>System</b> — {t('settings.tun.detStackSystem')}
            </div>
            <div>
              <b>Auto</b> — {t('settings.tun.detStackAuto')}
            </div>
            <div>
              <b>macOS</b> — {t('settings.tun.detStackMac')}
            </div>
            <div>
              <b>Windows</b> — {t('settings.tun.detStackWin')}
            </div>
            <div>
              <b>Linux</b> — {t('settings.tun.detStackLinux')}
            </div>
            <div className="tun-det-h">{t('settings.tun.detRouteHead')}</div>
            <div>
              <b>Windows</b> — {t('settings.tun.detRouteWin')}
            </div>
            <div>
              <b>macOS</b> — {t('settings.tun.detRouteMac')}
            </div>
            <div>
              <b>Linux</b> — {t('settings.tun.detRouteLinux')}
            </div>
          </div>
        </details>
      </SetBlock>

      {/* 2. 排除网段 */}
      <SetBlock header={t('settings.tun.routeExcludeBlock')}>
        {/*
          route_exclude 无独立 config 字段：内核侧 TUN route_exclude_address 由
          bypass_lan_cidrs(effective_bypass_lan(config)) 从 bypassLANList 的 CIDR 子集派生
          （见 crates/config-engine/.../inbounds.rs + system_proxy_bypass.rs）。故本编辑器与
          「网络」页系统代理旁路列表同源于 bypassLANList（单一清单，systemProxyBypass 已并入），
          读当前值 + 经 update 持久化，不再丢弃用户输入。

          同源也意味着同受 bypassLAN 总开关管辖：总开关关闭时 effective_bypass_lan 返回空清单，
          route_exclude 随之为空 → 此处清单一条都不生效，故与「网络」页同样隐藏（隐藏而非禁用：
          不生效的可编辑清单是误导）。总开关本体只放在「网络 · 系统代理」一处（单一控制点），
          这里仅给出指引，避免两处开关互相打架。
        */}
        {/* 折叠体在 showList 门控**内侧**：总开关关掉时整块换成下面的 plat-warn 提示，
            不会留一个点开是空壳的折叠标题。 */}
        {bypassLan.showList ? (
          <Fold
            id="fold-route-exclude"
            title={t('settings.tun.routeExcludeFold')}
            count={bypassList.length}
          >
            <div className="fld-hint" style={{ marginTop: 0 }}>
              {t('settings.tun.routeExcludeHint')}
              {/* 同源提示与「网络 · 系统代理」页对称：两处各自独立折叠，但编辑的是同一份 bypassLANList。
                  加粗的半句与其余半句分成两个键：只为保住 `<b>` 的排版而拆，别把它们合成一句。 */}
              <b>{t('settings.tun.sharedListBold')}</b>
              {t('settings.tun.sharedListRest')}
            </div>
            <ListEditor
              id="cidr-list"
              value={bypassList}
              onChange={(next) => void update({ bypassLANList: next })}
              placeholder="172.16.0.0/12"
              ariaLabel="CIDR"
              addLabel={t('settings.tun.addCidr')}
              importLabel={t('common.bulkImport')}
            />
          </Fold>
        ) : (
          <div className="plat-warn" id="cidr-bypass-off-note" style={{ display: 'flex' }}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <circle cx="12" cy="12" r="9" />
              <path d="M12 8v5M12 16h.01" />
            </svg>
            <span>
              {t(
                'settings.advanced.bypassLANOffNote',
                '「绕过局域网」总开关已关闭，排除网段不生效（内核侧排除清单为空）。如需恢复，请到「网络 · 系统代理」开启该开关。',
              )}
            </span>
          </div>
        )}
      </SetBlock>

      {/* 3. 连入来源排除 */}
      <SetBlock header={t('settings.tun.inboundExcludeBlock')}>
        <Fold
          id="fold-inbound-exclude"
          title={t('settings.tun.inboundExcludeFold')}
          count={inboundExcludeCidrs.length}
        >
          <div className="fld-hint" style={{ marginTop: 0 }}>
            {t('settings.tun.inboundExcludeHint')}
          </div>
          {/* plat-warn：默认 display:none，仅 :root[data-os="lin"] 时 CSS 显示——常渲染，不做 JS 平台判断 */}
          <div id="inbound-lin-warn" className="plat-warn" style={{ margin: 0 }}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <circle cx="12" cy="12" r="9" />
              <path d="M12 8v5M12 16h.01" />
            </svg>
            <span>{t('settings.tun.inboundLinuxNote')}</span>
          </div>
          <ListEditor
            id="inbound-cidr-list"
            value={inboundExcludeCidrs}
            onChange={(next) => patchTun({ inboundExcludeCidrs: next })}
            placeholder="100.64.0.0/10"
            ariaLabel="CIDR"
            addLabel={t('settings.tun.addCidr')}
            importLabel={t('common.bulkImport')}
          />
        </Fold>
      </SetBlock>

      {/* 4. 局域网网关（契约 L102）——本机作 LAN 网关时的 sing-box 1.14 设备识别簇。
          平台门控走**组件层**（不渲染），而非 CSS 隐藏：这两项在不支持的平台上不是「样式问题」，
          而是内核根本没有实现（Windows 无邻居解析器；MAC 过滤发射即 FATAL）。
          条目校验（MAC 形状 / 后缀前导点）由构建期负责：`inbounds.rs:306-330` 过滤非法 MAC、
          `dns.rs:357-367` 归一化后缀并去重，故此处不重复一套前端校验。 */}
      {showsLanGateway && (
        <SetBlock
          id="set-lan-gateway"
          header={
            <>
              {t('settings.advanced.lanGateway', '局域网网关')}{' '}
              <Pill variant="region">{t('settings.network.onlyTunLinuxMac', '仅 TUN · Linux / macOS')}</Pill>
            </>
          }
        >
          {/* neighborDomains → dns-local 的 neighbor_domain（builder/dns.rs:355-378） */}
          <Fold
            id="fold-neighbor-domains"
            title={t('settings.advanced.neighborDomains', '局域网短名解析')}
            count={neighborDomains.length}
          >
            <div className="fld-hint" style={{ marginTop: 0 }}>
              {t('settings.advanced.neighborDomainsHint')}
            </div>
            <ListEditor
              id="neighbor-domain-list"
              value={neighborDomains}
              onChange={(next) => patchTun({ neighborDomains: next })}
              placeholder=".lan"
              ariaLabel={t('settings.advanced.neighborDomains', '局域网短名解析')}
              addLabel={t('settings.tun.addSuffix')}
              importLabel={t('common.bulkImport')}
            />
            {/* 内联校验：生成期 `builder/dns.rs:355-378` 只做 `normalize_neighbor_domain`（补前导点）+ 去重，
                形状本身不判——脏后缀会一路带到内核 init。此前 UI 也不判，用户对此零反馈。 */}
            {hasInvalidEntry(neighborDomains, isValidNeighborDomain) && (
              <div className="err-line">{t('settings.advanced.neighborDomainInvalid')}</div>
            )}
          </Fold>

          {/* MAC 过滤仅 Linux，且内核要求 auto_route 开启（inbounds.rs:306 `&& auto_route`）。
              autoRoute 关闭时这个下拉是死控件 → 走本仓「假可用」惯例：disabled + tip 说明原因（统一 tooltip 引擎），
              而不是照常可点然后静默不生效。 */}
          {platform === 'lin' && (
            <>
              <SetRow
                id="set-mac-filter"
                label={
                  <>
                    {t('settings.advanced.macFilter', '按 MAC 过滤设备')}{' '}
                    <Pill variant="region">{t('settings.network.onlyLinux', '仅 Linux')}</Pill>
                  </>
                }
                desc={t('settings.advanced.macFilterDesc')}
              >
                <Select
                  id="mac-filter-mode"
                  value={tun.macFilterMode ?? 'off'}
                  disabled={!tun.autoRoute}
                  tip={!tun.autoRoute ? t('settings.advanced.macFilterHint') : undefined}
                  onChange={(e) =>
                    patchTun({
                      macFilterMode:
                        e.target.value === 'off' ? undefined : (e.target.value as 'include' | 'exclude'),
                    })
                  }
                  aria-label={t('settings.advanced.macFilter', '按 MAC 过滤设备')}
                  style={{ width: '150px' }}
                >
                  <option value="off">{t('settings.advanced.macFilterOff', '关闭')}</option>
                  <option value="include">{t('settings.advanced.macFilterInclude', '仅允许')}</option>
                  <option value="exclude">{t('settings.advanced.macFilterExclude', '排除')}</option>
                </Select>
              </SetRow>
              {/* 清单只在模式已选时渲染：模式 off 时内核完全不消费 macFilterList（inbounds.rs:306
                  以 mac_filter_mode 为入口），留个可编辑清单同样是误导。 */}
              {tun.macFilterMode && (
                <Fold id="fold-mac-filter" title={t('settings.advanced.macFilter', '按 MAC 过滤设备')} count={macFilterList.length}>
                  <div className="fld-hint" style={{ marginTop: 0 }}>
                    {t('settings.advanced.macFilterHint')}
                  </div>
                  <ListEditor
                    id="mac-filter-list"
                    value={macFilterList}
                    onChange={(next) => patchTun({ macFilterList: next })}
                    placeholder="00:11:22:33:44:55"
                    ariaLabel="MAC"
                    addLabel={t('settings.tun.addMac')}
                    importLabel={t('common.bulkImport')}
                  />
                  {/* 内联校验：生成期 `builder/inbounds.rs:341-350` 用 `is_valid_mac_address` **静默过滤**
                      坏条目 —— 全填错时整个 include/exclude 段不发射，用户看到的是「设了 MAC 过滤但没生效」
                      且没有任何提示。这里用同一个谓词（`domain/neighbor.ts`，与 Rust 侧同口径）当场标出。 */}
                  {hasInvalidEntry(macFilterList, isValidMacAddress) && (
                    <div className="err-line">{t('settings.advanced.macInvalid')}</div>
                  )}
                </Fold>
              )}
            </>
          )}
        </SetBlock>
      )}
    </section>
  );
}
