/**
 * SettingsNetwork —— 网络子页（原型 [data-sec="network"] L2119-2169）。
 *
 * 六块：
 *  1. 本地端口：mixed port + 允许局域网
 *  2. 系统代理：旁路列表（Fold 折叠的 ListEditor）
 *  3. 高级流量：Block QUIC / WebRTC 防泄露 / TLS 分片 / 节点故障切换 / mesh 登录让位 / 切节点断旧连接 / 自动重启
 *  4. 更新与测速：更新检查走代理（mainSessionViaProxy）+ 测速端点 URL（speedTestUrl）
 *  5. 管理面板：sing-box 控制 API + 官方 dashboard（端口/secret/打开面板/复制连接/刷新面板资源）
 *  6. 终端代理：shell env vars 复制（**按平台分支**：当前平台常驻 + 其余平台折叠）
 *
 * 嵌入「管理面板」即任务描述的 management 子能力（不单列子页，对齐 nav-store 9 子页设计）。
 * 第 4 块曾临时寄放在 `SettingsGeneral`（当时本页被占），现按契约归位。
 */

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { UserConfig } from '@/contracts/types';
import { appApi, proxyApi } from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import { Fold } from '@/components/Fold';
import { Phead, SetBlock, SetRow, Switch, TextInput, Segmented, Button } from './primitives';
import { ListEditor } from './ListEditor';
import {
  bypassLanState,
  localProxyPort,
  controlApiPort,
  normalizePortInput,
  shellPlatformFromDataOs,
  splitTerminalEnvByPlatform,
  showsUnixPersistenceTip,
  DEFAULT_MIXED_PORT,
  DEFAULT_CONTROL_PORT,
  type TerminalEnvGroup,
} from './settings-logic';

export interface SettingsNetworkProps {
  config: UserConfig;
  update: (patch: Partial<UserConfig>) => Promise<void>;
}

type WebRTC = 'off' | 'proxy' | 'block';

const DEFAULT_BYPASS_LAN_LIST = ['localhost', '127.0.0.1', '192.168.0.0/16'];

/**
 * 测速端点是否合法 —— 与后端 `src-tauri/src/icon_cache.rs::is_http_url` 逐字同口径
 * （`http://` / `https://` 前缀，大小写不敏感），**刻意不用 `new URL()`**：后端只看前缀，
 * `new URL` 会接受一批后端随即回落默认的写法，两侧口径分叉就变成「填了没生效还不报错」。
 */
function isHttpUrl(value: string): boolean {
  const v = value.trim().toLowerCase();
  return v.startsWith('http://') || v.startsWith('https://');
}

/** 测速端点缺省（后端 `commands/speedtest.rs:67` DEFAULT_SPEED_TEST_URL）；留空即回落到它。 */
const DEFAULT_SPEED_TEST_URL = 'http://www.gstatic.com/generate_204';

/** 复制图标（分组「全部复制」与逐行复制共用）。 */
function CopyIcon({ width }: { width?: number }) {
  return (
    <svg viewBox="0 0 24 24" width={width} fill="none" stroke="currentColor" strokeWidth={1.8}>
      <rect x="9" y="9" width="11" height="11" rx="2" />
      <path d="M5 15V5a2 2 0 012-2h10" />
    </svg>
  );
}

/**
 * 一组命令（组头 + 逐行 code + 逐行复制）。
 *
 * 「全部复制」按**组**给，不给全局一个：Windows 下当前平台就有 CMD 与 PowerShell 两组，跨 shell
 * 混着复制出来的是粘哪儿都跑不通的废文本。
 */
function TerminalEnvBlock({
  group,
  onCopy,
  copyAllLabel,
}: {
  group: TerminalEnvGroup;
  onCopy: (text: string) => void;
  copyAllLabel: string;
}) {
  return (
    <>
      <div className="term-grp">
        <span>{group.label}</span>
        <Button variant="ghost" size="sm" onClick={() => onCopy(group.lines.join('\n'))}>
          <CopyIcon width={14} />
          <span>{copyAllLabel}</span>
        </Button>
      </div>
      <div className="term-env">
        {group.lines.map((line) => (
          <div key={line} className="term-row">
            <code className="mono">{line}</code>
            <button type="button" onClick={() => onCopy(line)} aria-label="Copy" className="term-copy">
              <CopyIcon />
            </button>
          </div>
        ))}
      </div>
    </>
  );
}

/** 生成新的 clash_api secret（Web Crypto，无第三方依赖）：24 字节 → 48 位十六进制。 */
function generateSecret(): string {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export default function SettingsNetwork({ config, update }: SettingsNetworkProps) {
  // i18n 实例：面板窗口的语言由 `open_singbox_dashboard(locale)` 预写，需当前语言码。
  const { t, i18n } = useTranslation();
  // 与后端 `local_proxy_port`（proxy_ports.rs:22-34）同口径：mixed>0 → http>0 → 7890。
  // 此前写 `config.mixedPort ?? 7890`，丢了两条 httpPort 回退路径，导致「说明文字里的端口对、
  // 复制出去的命令端口错」（见 settings-logic.ts::localProxyPort 注释）。
  const mixedPort = localProxyPort(config);
  const controlPort = controlApiPort(config);
  const bypassList = config.bypassLANList ?? DEFAULT_BYPASS_LAN_LIST;
  const platform = shellPlatformFromDataOs();
  const envSplit = splitTerminalEnvByPlatform(platform, mixedPort);
  // 直接从持久化配置派生：已启用时详情行随之展开，不再因 useState(false) 硬编码而失同步
  const mgmtEnabled = !!config.singboxDashboard;
  const [showSecret, setShowSecret] = useState(false);
  const secret = config.clashApiSecret ?? '';

  /* ── 两个端口 + 测速端点：本地草稿 + onBlur 提交（同 `SettingsDns` 已落地的模式）────────────
   * 逐键写盘的代价在这里最大：**代理运行中**每敲一个字符落一次盘 → 触发一次整核重启评估，且中间态
   * 恒为 `7` / `78` / `789` 这类特权或非法端口（后端 `store/validate.rs:264-279` 只挡 1..65535，
   * 中间态照落）。故一律「输入进草稿 → blur/Enter 才校验落盘」，非法标红且不写 config。
   *
   * 外部改动（托盘/备份恢复/另一屏保存 → useConfig 静默重拉）要能回填到草稿，但不能打断正在输入的
   * 用户：种子快照 `seededRef` 即这道守卫——草稿 ≠ 上次种子 = 用户已改过，保留草稿；相等 = 未动过，
   * 跟随新配置。 */
  const [mixedPortDraft, setMixedPortDraft] = useState(String(mixedPort));
  const [controlPortDraft, setControlPortDraft] = useState(String(controlPort));
  const [speedUrlDraft, setSpeedUrlDraft] = useState(config.speedTestUrl ?? '');
  const [portErr, setPortErr] = useState<{ mixed?: boolean; control?: boolean }>({});
  const [speedUrlErr, setSpeedUrlErr] = useState(false);
  const seededRef = useRef({
    mixed: String(mixedPort),
    control: String(controlPort),
    speedUrl: config.speedTestUrl ?? '',
  });
  useEffect(() => {
    const snap = {
      mixed: String(mixedPort),
      control: String(controlPort),
      speedUrl: config.speedTestUrl ?? '',
    };
    const prev = seededRef.current;
    setMixedPortDraft((cur) => (cur !== prev.mixed ? cur : snap.mixed));
    setControlPortDraft((cur) => (cur !== prev.control ? cur : snap.control));
    setSpeedUrlDraft((cur) => (cur !== prev.speedUrl ? cur : snap.speedUrl));
    seededRef.current = snap;
  }, [mixedPort, controlPort, config.speedTestUrl]);

  /**
   * 提交一个端口：非法 → 标红、保留输入待修正、**不落盘**；清空 → 回默认；无变化 → 不写（免无谓重启）。
   *
   * 合法区间取 `normalizePortInput` 的 1024..65535（上游 `network-settings.tsx:219` 同口径，
   * 见该函数注释）。`controlPort` 与 `mixedPort` 撞口由后端 `validate.rs:229-250` 自动避让，
   * **UI 不重复实现避让**——两处各写一套只会在「后端改了避让策略」时静默分叉。
   */
  function commitPort(key: 'mixedPort' | 'controlPort', raw: string) {
    const isMixed = key === 'mixedPort';
    const next = normalizePortInput(raw, isMixed ? DEFAULT_MIXED_PORT : DEFAULT_CONTROL_PORT);
    const slot = isMixed ? 'mixed' : 'control';
    if (next === null) {
      setPortErr((p) => ({ ...p, [slot]: true }));
      return;
    }
    setPortErr((p) => ({ ...p, [slot]: false }));
    if (isMixed) setMixedPortDraft(String(next));
    else setControlPortDraft(String(next));
    if (next === (isMixed ? mixedPort : controlPort)) return;
    void update({ [key]: next });
  }

  /** 提交测速端点：非法 → 标红、保留输入、**不落盘**；清空 → 删字段（后端回落默认）。 */
  function commitSpeedTestUrl(raw: string) {
    const v = raw.trim();
    if (v && !isHttpUrl(v)) {
      setSpeedUrlErr(true);
      return;
    }
    setSpeedUrlErr(false);
    const next = v || undefined;
    setSpeedUrlDraft(v);
    if (next === (config.speedTestUrl ?? undefined)) return;
    void update({ speedTestUrl: next });
  }
  // WebRTC 防泄露仅 TUN 模式生效（原型 .webrtc-row.disabled，纯 CSS 靠 disabled 类门控）
  const webrtcDisabled = config.proxyModeType !== 'tun';
  // 绕过局域网总开关显示态 + 清单是否渲染（缺省为开，与后端 effective_bypass_lan 同口径）
  const bypassLan = bypassLanState(config);

  async function copyText(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(t('settings.network.copied', '已复制到剪贴板'));
    } catch {
      toast.error(t('common.copyFail', '复制失败'));
    }
  }

  /**
   * 打开 sing-box 官方面板（应用内窗口）。
   *
   * `locale` 必须传：后端 `map_locale_to_dashboard_lang` 据它预写面板语言（en/zh-Hans/zh-Hant/fa/ru），
   * 不传则面板恒英文 —— 参数早已在 api-client 接好（`{ locale }` 而非 `{ value }`），此前只是没人传。
   *
   * 判 `r.ok`：代理未运行 / clash_api 端口为 0 时后端返 `{ ok:false }` 的**成功信封**（不抛），
   * 只写 `.catch` 会让这条最常见的失败路径静默无反应。
   */
  async function openDashboard() {
    try {
      const r = await appApi.openSingboxDashboard(i18n.language);
      if (!r?.ok) {
        toast.error(
          t('settings.network.dashboardNotRunning', '面板不可用：请先启用管理面板并连接代理'),
        );
      }
    } catch {
      toast.error(t('settings.network.openDashboardFail', '打开面板失败'));
    }
  }

  /** 复制面板连接信息（API 地址 + secret）。同 openDashboard：`ok:false` 是业务假值，须显式判。 */
  async function copyDashboardConnection() {
    try {
      const r = await appApi.getSingboxDashboardConnection();
      if (!r?.ok || !r.apiUrl) {
        toast.error(
          t('settings.network.dashboardNotRunning', '面板不可用：请先启用管理面板并连接代理'),
        );
        return;
      }
      await copyText(`API: ${r.apiUrl}\nSecret: ${r.secret}`);
    } catch {
      toast.error(t('common.copyFail', '复制失败'));
    }
  }

  /** 刷新面板资源：清缓存目录，内核下次启动重新下载（后端不重启内核，故文案强调「下次启动生效」）。 */
  async function refreshDashboardAssets() {
    try {
      const r = await appApi.refreshSingboxDashboard();
      if (r?.ok) {
        toast.success(
          t(
            'settings.network.refreshDashboardDone',
            '已清除面板资源缓存，内核下次启动时重新下载',
          ),
        );
      } else {
        toast.error(t('settings.network.refreshDashboardFail', '刷新面板资源失败'));
      }
    } catch {
      toast.error(t('settings.network.refreshDashboardFail', '刷新面板资源失败'));
    }
  }

  // B2 一键关闭系统代理：调后端 system_proxy_disable 清除其它软件遗留的系统代理设置（TUN 模式下会干扰连接）。
  // 后端带 marker 门控（只清残留、不 stomp 用户自配）；幂等——无残留时为 no-op。App.tsx 的残留检测 toast
  // （event:systemProxyResidual）提示后，用户经此按钮显式关闭（Polaris toast 无动作按钮，故动作入口落此处）。
  async function handleDisableSystemProxy() {
    try {
      await proxyApi.disableSystemProxy();
      toast.success(t('proxy.systemProxyDisabled'));
    } catch {
      toast.error(t('proxy.systemProxyDisableFailed'));
    }
  }

  return (
    <section className="screen" data-sec="network">
      <Phead title={t('settings.nav.network')} sub={t('settings.network.pageSub')} />

      {/* 1. 本地端口 */}
      <SetBlock header={t('settings.advanced.localPort')}>
        {/* onBlur 提交：onChange 只动草稿，Enter 触发 blur 即提交（同 SettingsDns 两栏 DNS）。
            **不用 `type="number"`**：非法输入时浏览器把 `value` 报成空串，用户敲错的字符会当场消失，
            标红也就无从指向；`text + inputMode="numeric"` 保留原样并交给 commitPort 判定。 */}
        <SetRow
          label={t('settings.network.mixedPort')}
          align="start"
          ctrlStyle={{ minWidth: 120, display: 'flex', flexDirection: 'column', gap: 6, alignItems: 'stretch' }}
        >
          <TextInput
            id="mixed-port-input"
            inputMode="numeric"
            value={mixedPortDraft}
            onChange={(e) => {
              setMixedPortDraft(e.currentTarget.value);
              if (portErr.mixed) setPortErr((p) => ({ ...p, mixed: false }));
            }}
            onBlur={() => commitPort('mixedPort', mixedPortDraft)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') e.currentTarget.blur();
            }}
            aria-invalid={portErr.mixed || undefined}
            style={portErr.mixed ? { borderColor: 'hsl(var(--err))' } : undefined}
            className="mono"
            aria-label="Mixed port"
          />
          {portErr.mixed && <div className="err-line">{t('settings.advanced.localPortRange')}</div>}
        </SetRow>
        <SetRow label={t('settings.network.allowLan')}>
          <Switch checked={!!config.allowLan} onChange={(v) => void update({ allowLan: v })} />
        </SetRow>
      </SetBlock>

      {/* 2. 系统代理旁路 */}
      <SetBlock header={t('settings.network.systemProxyBlock')}>
        {/* 绕过局域网总开关。后端 `system_proxy_bypass.rs::effective_bypass_lan` 早已消费 bypassLAN
            （`bypass_lan() == Some(false)` → 返回空清单，TUN route_exclude 与系统代理旁路两处都走它），
            此前只缺 UI 入口。正向语义、缺省为开，与后端 `Some(false)` 判定同口径。 */}
        <SetRow
          label={t('settings.advanced.bypassLAN')}
          desc={t('settings.advanced.bypassLANDesc')}
        >
          <Switch
            id="bypass-lan-swt"
            checked={bypassLan.checked}
            onChange={(v) => void update({ bypassLAN: v })}
            aria-label="Bypass LAN"
          />
        </SetRow>
        {/* 关掉总开关时隐藏清单：后端此时返回空清单，清单一条都不生效，
            继续展示可编辑清单会让用户以为编辑有效（误导）。 */}
        {/* 折叠体**必须**留在 showList 门控内侧：放外面会出现「总开关已关、清单不生效，折叠标题却还杵在那」
            的怪相（用户点开是空壳）。门控与折叠是两层独立状态，顺序固定为「先门控、后折叠」。 */}
        {bypassLan.showList && (
          <Fold id="fold-bypass" title={t('settings.network.bypassFold')} count={bypassList.length}>
            <div className="fld-hint" style={{ marginTop: 0 }}>
              {t('settings.network.bypassHint')}
              {/* #1 与 #3 同源：route_exclude 无独立字段，由本清单的 CIDR 子集派生（SettingsTun.tsx:180-191）。
                  两处各自独立折叠（互不联动），但必须让用户知道编辑的是同一份数据 —— 否则会以为
                  「TUN 页那份没设，得再填一遍」。加粗半句与其余半句分成两个键，只为保住 `<b>` 的排版。 */}
              <b>{t('settings.network.sharedListBold')}</b>
              {t('settings.network.sharedListRest')}
            </div>
            <ListEditor
              id="le-bypass"
              value={bypassList}
              onChange={(next) => void update({ bypassLANList: next })}
              placeholder="localhost · 10.0.0.0/8 · *.example.cn"
              ariaLabel="Bypass entry"
              addLabel={t('common.add')}
              importLabel={t('common.bulkImport')}
            />
          </Fold>
        )}
        {/* B2 一键关闭系统代理入口（对齐 上游 语义：残留检测提示后由用户显式关闭）。 */}
        <SetRow
          label={t('proxy.disableSystemProxy')}
          desc={t('settings.network.disableSystemProxyDesc')}
          style={{ borderBottom: 0 }}
        >
          <Button variant="ghost" size="sm" onClick={() => void handleDisableSystemProxy()}>
            <span>{t('proxy.disableSystemProxy')}</span>
          </Button>
        </SetRow>
      </SetBlock>

      {/* 3. 高级流量 */}
      <SetBlock header={t('settings.network.advancedTraffic')}>
        <SetRow
          label={t('settings.advanced.blockQuic')}
          desc={t('settings.network.blockQuicDescFull')}
        >
          <Switch checked={!!config.blockQuic} onChange={(v) => void update({ blockQuic: v })} />
        </SetRow>
        {/* 手写而非 SetRow：sr-tx 下需要 desc 文案 + webrtc-note 两个平级 div（原型 L2141），
            webrtc-note 常渲染，显隐交给 `.webrtc-row.disabled .webrtc-note` CSS（非 TUN 模式才可见）。 */}
        <div
          className={webrtcDisabled ? 'set-row webrtc-row disabled' : 'set-row webrtc-row'}
          id="webrtc-row"
          style={{ alignItems: 'flex-start' }}
        >
          <div className="sr-tx">
            <b>{t('settings.network.webrtcLeakProtection')}</b>
            <div>{t('settings.network.webrtcLeakDesc')}</div>
            <div className="webrtc-note" id="webrtc-note">
              {t('settings.network.webrtcTunOnlyNote')}
            </div>
          </div>
          <div className="sr-ctl">
            <Segmented<WebRTC>
              id="webrtc-seg"
              ariaLabel="WebRTC leak protection"
              value={config.webrtcLeakProtection ?? 'off'}
              onChange={(v) => void update({ webrtcLeakProtection: v })}
              options={[
                { value: 'off', label: t('settings.network.webrtcLeakOff') },
                { value: 'proxy', label: t('settings.network.webrtcLeakProxy') },
                { value: 'block', label: t('settings.network.webrtcLeakBlock') },
              ]}
            />
          </div>
        </div>
        <SetRow
          label={t('settings.advanced.tlsFragment')}
          desc={t('settings.advanced.tlsFragmentDesc')}
        >
          <Switch checked={!!config.tlsFragment} onChange={(v) => void update({ tlsFragment: v })} />
        </SetRow>
        <SetRow
          label={t('settings.advanced.autoSwitchNode')}
          desc={t('settings.advanced.autoSwitchNodeDesc')}
        >
          <Switch checked={!!config.autoSwitchNode} onChange={(v) => void update({ autoSwitchNode: v })} />
        </SetRow>
        <SetRow
          label={t('settings.network.meshLoginFallback')}
          desc={t('settings.network.meshLoginFallbackDesc')}
        >
          <Switch
            checked={config.meshLoginFallbackDirect !== false}
            onChange={(v) => void update({ meshLoginFallbackDirect: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.network.interruptOnSwitch')}
          desc={t('settings.network.interruptOnSwitchDesc')}
        >
          <Switch
            checked={config.interruptConnectionsOnSwitch !== false}
            onChange={(v) => void update({ interruptConnectionsOnSwitch: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.network.resolveBeforeDial')}
          desc={t('settings.network.resolveBeforeDialDesc')}
        >
          <Switch
            checked={!!config.resolveBeforeDial}
            onChange={(v) => void update({ resolveBeforeDial: v })}
          />
        </SetRow>
        <SetRow
          label={t('settings.network.restartOnNodeChange')}
          desc={t('settings.network.restartOnNodeChangeDesc')}
        >
          <Switch
            checked={!!config.restartOnNodeChange}
            onChange={(v) => void update({ restartOnNodeChange: v })}
          />
        </SetRow>
      </SetBlock>

      {/* 4. 更新与测速（契约指定归属本页；上一批因本页被占临时寄放在通用页，此处归位） */}
      <SetBlock header={t('settings.network.updateAndSpeedTest', '更新与测速')}>
        {/* mainSessionViaProxy → runtime/http.rs:286（更新/规则下载会话）+ icon_cache.rs:379（图标抓取）。
            缺省 true（更新源多在 GitHub，墙内借道更可靠），故 `!== false`。 */}
        <SetRow
          label={t('settings.advanced.mainSessionViaProxy')}
          desc={t('settings.advanced.mainSessionViaProxyDesc')}
        >
          <Switch
            id="main-session-via-proxy-swt"
            checked={config.mainSessionViaProxy !== false}
            onChange={(v) => void update({ mainSessionViaProxy: v })}
            aria-label={t('settings.advanced.mainSessionViaProxy', '更新检查走代理')}
          />
        </SetRow>
        {/* speedTestUrl → commands/speedtest.rs:539-541 resolve_speed_test_url（非法值后端静默回落
            默认，故前端必须先标红，否则「填了个错 URL，测速照跑但量的是别的端点」无从察觉）。 */}
        <SetRow
          label={t('settings.network.speedTestUrl', '测速端点')}
          desc={t('settings.network.speedTestUrlDesc')}
          align="start"
          ctrlStyle={{ minWidth: 260, display: 'flex', flexDirection: 'column', gap: 6, alignItems: 'stretch' }}
        >
          <TextInput
            id="speed-test-url-input"
            value={speedUrlDraft}
            placeholder={DEFAULT_SPEED_TEST_URL}
            onChange={(e) => {
              setSpeedUrlDraft(e.currentTarget.value);
              if (speedUrlErr) setSpeedUrlErr(false);
            }}
            onBlur={() => commitSpeedTestUrl(speedUrlDraft)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') e.currentTarget.blur();
            }}
            aria-invalid={speedUrlErr || undefined}
            style={speedUrlErr ? { borderColor: 'hsl(var(--err))' } : undefined}
            className="mono"
            aria-label={t('settings.network.speedTestUrl', '测速端点')}
          />
          {speedUrlErr && <div className="err-line">{t('settings.network.speedTestUrlInvalid')}</div>}
        </SetRow>
      </SetBlock>

      {/* 5. 管理面板（management 子能力） */}
      <SetBlock header={t('settings.network.mgmtBlock')} id="mgmt-block">
        <SetRow
          label={t('settings.network.mgmtEnable')}
          desc={t('settings.network.mgmtEnableDesc')}
        >
          <Switch checked={mgmtEnabled} onChange={(v) => void update({ singboxDashboard: v })} />
        </SetRow>
        {mgmtEnabled && (
          <div className="mgmt-detail">
            {/* 同混合端口：草稿 + onBlur 提交。与 mixedPort 撞口时后端自动避让（validate.rs:229-250），
                此处只做范围校验，不复刻避让逻辑。 */}
            <SetRow
              label={t('settings.network.mgmtPort')}
              desc={t('settings.network.mgmtPortDesc')}
              align="start"
              ctrlStyle={{ minWidth: 120, display: 'flex', flexDirection: 'column', gap: 6, alignItems: 'stretch' }}
            >
              <TextInput
                id="control-port-input"
                inputMode="numeric"
                value={controlPortDraft}
                onChange={(e) => {
                  setControlPortDraft(e.currentTarget.value);
                  if (portErr.control) setPortErr((p) => ({ ...p, control: false }));
                }}
                onBlur={() => commitPort('controlPort', controlPortDraft)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') e.currentTarget.blur();
                }}
                aria-invalid={portErr.control || undefined}
                style={portErr.control ? { borderColor: 'hsl(var(--err))' } : undefined}
                className="mono"
                aria-label="Management port"
              />
              {portErr.control && (
                <div className="err-line">{t('settings.advanced.localPortRange')}</div>
              )}
            </SetRow>
            <SetRow
              label="API secret"
              desc={t('settings.network.mgmtSecretDesc')}
              ctrlStyle={{ display: 'flex', gap: 6, alignItems: 'center', width: 288 }}
            >
              <TextInput
                type={showSecret ? 'text' : 'password'}
                value={secret}
                readOnly
                className="mono"
                style={{ flex: 1, minWidth: 0 }}
                aria-label="API secret"
              />
              <button
                type="button"
                onClick={() => setShowSecret((s) => !s)}
                aria-label="Show secret"
                className="icon-btn"
                data-tip={t('settings.network.secretToggleTip')}
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z" />
                  <circle cx="12" cy="12" r="3" />
                </svg>
              </button>
              <button
                type="button"
                onClick={() => void copyText(secret)}
                aria-label="Copy secret"
                className="icon-btn"
                data-tip={t('common.copy')}
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <rect x="9" y="9" width="11" height="11" rx="2" />
                  <path d="M5 15V5a2 2 0 012-2h10" />
                </svg>
              </button>
              <button
                type="button"
                onClick={() => void update({ clashApiSecret: generateSecret() })}
                aria-label="Regenerate secret"
                className="icon-btn"
                data-tip={t('settings.network.secretRegenerateTip')}
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M4 4v6h6M20 20v-6h-6" />
                  <path d="M4 10a8 8 0 0114-3M20 14a8 8 0 01-14 3" />
                </svg>
              </button>
            </SetRow>
            {/* `open_singbox_dashboard`（commands/misc.rs:258-327）**是真实现**：真建
                `WebviewWindow`（单例，已存在则 focus）加载核 serve 的 `/dashboard/`，经
                `initialization_script` 在面板读 localStorage 之前预写后端连接（一键直连），并用
                `on_navigation` 把导航锁死在本地 api service 同源（防 clash_api secret 被第三方面板代码带走）。
                ⚠️ 旧注释与 tooltip 称「命令返回成功但不会真正打开面板窗口」——与代码事实相反，
                连同那个 `disabled` 一起是**一行之隔**把已完成的能力锁在门外，本批订正并解禁。
                代理未运行 / 端口为 0 时后端返 `{ok:false}`（**成功信封里的业务假值**，不抛），
                故必须判 `r.ok` 而非只 catch —— 只 catch 会让「没连接就点」变成静默无反应。 */}
            {/* 行标题从「打开面板」改成「官方面板」：本行现在挂三个动作，其中「刷新面板资源」并不需要
                内核在跑，沿用旧标题会让 desc 里的前提条件张冠李戴。 */}
            <SetRow
              label={t('settings.network.officialDashboard')}
              desc={t('settings.network.officialDashboardDesc')}
              style={{ borderBottom: 0 }}
            >
              <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => void openDashboard()}
                >
                  <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth={1.8}>
                    <path d="M14 3h7v7M21 3l-9 9M10 5H5a2 2 0 00-2 2v12a2 2 0 002 2h12a2 2 0 002-2v-5" />
                  </svg>
                  <span>{t('settings.advanced.openDashboard')}</span>
                </Button>
                {/* 复制连接（`get_singbox_dashboard_connection`，misc.rs:400-403）：把 API 地址与 secret
                    交给用户，供外部面板（yacd / metacubexd 等）或 curl 直连本机 clash_api。
                    两行的标签刻意**不走 i18n**：`API` / `Secret` 是外部面板输入框上的原文字段名，
                    翻译过去反而对不上（同 settings-logic.ts 里终端命令分组标签不翻译的理由）。 */}
                <Button variant="ghost" size="sm" onClick={() => void copyDashboardConnection()}>
                  <CopyIcon width={14} />
                  <span>{t('settings.network.copyDashboardConnection', '复制连接')}</span>
                </Button>
                {/* 刷新面板资源（`refresh_singbox_dashboard`，misc.rs:384-397）：真删
                    `<config_dir>/singbox-dashboard` 目录（幂等、不存在不报错）。语义**不是**立刻回落到
                    什么内置副本 —— 本仓不随包出货面板静态文件；删掉后是「内核下次启动时重新下载」。
                    也**不在此重启内核**（保「不打断连接」语义），故文案必须说清「下次启动生效」，
                    否则用户点完看不到任何变化会以为按钮坏了。 */}
                <Button variant="ghost" size="sm" onClick={() => void refreshDashboardAssets()}>
                  <span>{t('settings.network.refreshDashboard', '刷新面板资源')}</span>
                </Button>
              </div>
            </SetRow>
          </div>
        )}
      </SetBlock>

      {/* 6. 终端代理 —— 当前平台优先 + 其余平台可展开（用户拍板形态）。
          此前这里是模块级字面量常量：无平台分支（Windows 用户拿到的 4 条 `export` 在 cmd/PowerShell
          里都不成立）、端口硬编码 7890（同一块内的说明文字却用真实端口插值，自相矛盾）。 */}
      <SetBlock header={t('settings.advanced.terminalProxy')}>
        <SetRow
          label={t('settings.advanced.envVarsLabel')}
          desc={t('settings.advanced.tipHttpPort', { port: mixedPort })}
          style={{ border: 0, paddingBottom: 2 }}
        />
        <div id="term-env">
          {envSplit.current.map((g) => (
            <TerminalEnvBlock key={g.id} group={g} onCopy={copyText} copyAllLabel={t('settings.advanced.copyAllEnv')} />
          ))}
        </div>
        {/* 其余平台：折叠标题直接列出组名（Windows (CMD) · Windows (PowerShell)），比一句「其他平台」
            信息量更大，且全是产品/shell 名 —— 跨语种同形，不需要也不该翻译。
            平台判定失败时 others 为空数组（全部归 current），此处整段不渲染。 */}
        {envSplit.others.length > 0 && (
          <Fold id="fold-env-others" title={envSplit.others.map((g) => g.label).join(' · ')}>
            {envSplit.others.map((g) => (
              <TerminalEnvBlock key={g.id} group={g} onCopy={copyText} copyAllLabel={t('settings.advanced.copyAllEnv')} />
            ))}
          </Fold>
        )}
        {/* 提示段：全部复用既有五语种 i18n 键（`tipDisable` 此前已随迁移搬进 locale 但零消费者，此处接上）。 */}
        <div className="term-tips">
          <div>
            <b>{t('settings.advanced.tip')}</b>
            {t('settings.advanced.tipSessionOnly')}
          </div>
          {/* `tipPermanent` 讲的是写进 ~/.bashrc / ~/.zshrc —— Windows 下不成立（CMD 要 setx、
              PowerShell 要 profile），给 Windows 用户看它就是本次要修的缺陷换个地方复发。 */}
          {showsUnixPersistenceTip(platform) && <div>{t('settings.advanced.tipPermanent')}</div>}
          <div>{t('settings.advanced.tipDisable')}</div>
        </div>
      </SetBlock>
    </section>
  );
}
