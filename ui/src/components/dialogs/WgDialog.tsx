/**
 * WgDialog —— WireGuard 添加/编辑弹窗（原型 #wg-dialog :2835；wgSetMode :4910）。
 *
 * 两来源（seg2）：手动填写 / 粘贴 wg-quick .conf。**.conf 解析复用** `domain/wg-quick.ts#parseWgQuickConf`
 * （经 `wg-logic.ts` 的 `parseConfToDraft` 薄封装接线，勿重写解析器）——解析成功即填入同一套表单字段
 * （原型语义：parse 填 manual 表单），再提交。多字段驱动走 D2 FieldSpec 表 + FieldRenderer。
 *
 * WG 允许多实例（携 serverId，异于 WARP 单例槽）：serverId 定义 → 编辑态预填（`draftFromServer`）。
 * 提交经 `api.server.add`（新增）/ `api.server.update`（编辑），protocol:'wireguard' + wireguardSettings
 * 由 `buildWgServer` 组装。R1：`key` 绑 serverId（见导出包装）+ useState 同步初始化。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveServers } from '@/store/app-store';
import { api } from '@/ipc';
import type { ServerConfig } from '@/contracts/types';
import { Modal } from './Modal';
import {
  FieldRenderer,
  type FieldSpec,
  type FormValue,
  type FormValues,
  type SelectOption,
} from './field-spec';
import { endpointDetourOptions } from './detour-options';
import {
  emptyWgDraft,
  draftFromServer,
  parseConfToDraft,
  buildWgServer,
  validateWgDraft,
  reservedInputInvalid,
  isWarpDraft,
  splitCsv,
  type WgDraft,
} from './wg-logic';
import { blockedByMeshSingleton } from '@/domain/mesh-singleton-guard';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute } from '@/lib/staged-config';
import { useDialogStore } from './dialog-store';
import { groupWgFields, type WgFormTab } from './vpn-form-layout';

const CATCH_ALL = new Set(['0.0.0.0/0', '::/0']);

/**
 * WG 字段表。`draft` / `base` 只被接入模式那一项的禁用判定用到（WARP 判据 = 端点域名 + `warpDevice`
 * 标记，两者分别来自草稿与存量设置），故收成函数（同 `TsSettingsDialog` 的 `mainSpec(exitOpts)`）
 * 而非模块级常量。
 *
 * `draft` 是**必传**：`FieldSpec.disabled` 是静态布尔（见 `field-spec.tsx` 那条的文档），少传一个
 * 可选参数就会把禁用静默退化成可用，而这个开关禁用与否是阻断级的。
 */
export function wgSpec(
  draft: FormValues,
  base: ServerConfig | undefined,
  detourOpts: readonly SelectOption[]
): FieldSpec[] {
  return [
    { t: 'text', k: 'address', label: 'wg.address', zh: '对端地址', ph: '203.0.113.7', mono: true },
    { t: 'number', k: 'port', label: 'wg.port', zh: '端口', ph: '51820', mono: true },
    { t: 'text', k: 'privateKey', label: 'wg.priv', zh: '本机私钥 PrivateKey', ph: 'wOE... =', mono: true, secret: true },
    { t: 'text', k: 'localAddress', label: 'wg.local', zh: '接口地址 Address', ph: '10.0.0.2/32, fd00::2/128', mono: true },
    { t: 'text', k: 'peerPublicKey', label: 'wg.pub', zh: '对端公钥 PublicKey', ph: 'HIg... =', mono: true },
    { t: 'text', k: 'preSharedKey', label: 'wg.psk', zh: '预共享密钥', mono: true, opt: true, secret: true },
    { t: 'text', k: 'allowedIPs', label: 'wg.allowed', zh: '路由网段 AllowedIPs', ph: '10.0.0.0/24', mono: true, opt: true },
    { t: 'number', k: 'persistentKeepalive', label: 'wg.keep', zh: '保活（秒）', ph: '25', mono: true },
    { t: 'number', k: 'mtu', label: 'wg.mtu', zh: 'MTU', ph: '1408', mono: true },
    // Reserved：**对所有 WG 节点开放**，不是 WARP 专属。上游 `wireguard-form.tsx:488` 同样把它放在
    // 通用 WG 表单里（其 `:53` 注释「reserved 仅 Cloudflare WARP 等需要」说的是**用途**，不是限制）——
    // 「等」不是虚指：任何在 WG 之上做多路复用的服务端（WARP 类网关、自建 xray/sing-box 对端）都靠
    // 这 3 字节把包分派回正确的会话，填不上就只是**连得上、不通**，没有任何报错。此前它只有
    // `WarpDialog` 有入口，普通 WG 节点在 UI 上改不了（`wg-logic.ts` 靠 `...base` 原样带过）。
    { t: 'text', k: 'reserved', label: 'wg.reserved', zh: 'Reserved', ph: '0, 0, 0', mono: true, opt: true },
    // 接入模式（上游 `shared/mesh-fields.tsx:32` 的 `AccessModeField`，同绑 reverseMesh）：上游 归常显的
    // 「接入与出口」段、紧邻全隧道开关，故摆在 allowInternet 之前，不入任何折叠——它决定
    // `meshUsesSystemInterface`，进而决定该节点是否参与测速（domain/endpoint-routes.ts:320）。
    // 藏起来 = 用户卡在「为什么这个节点测不出速」。
    //
    // **形态：开关，不是 上游的 gVisor/System 置灰选择器**（与 TsSettingsDialog:69 同一条理由）。
    // 上游 那个选择器在非 TUN / Windows 上置灰显示 gVisor，是渲染端自行复刻平台探测；Polaris 的降级
    // 由后端兜底且**两族同一条代码**：`builder/outbounds.rs:145` 一次算出 `downgrade_mesh`
    // （= mesh_uses_system_interface && !system_interface_available），WG 分支 `:156-159` 打回
    // `system=false` / `name=None`，TS 分支 `:170-172` 打回 `system_interface=false`；
    // `system_interface_available` 本身 = TUN 模式 && 非 Windows（`builder/generate.rs:259-260`）。
    // 故此处只需 hint 如实写明降级条件，不必把平台探测接线搬进渲染端。
    //
    // WARP 下**可见但禁用**，不再整条隐掉（此前是 `when: v => !isWarpDraft(v, base)`，上游
    // `wireguard-form.tsx:299` 的 `{!isWarp && …}` 同款）。不能开是对的（判据与真机实证见
    // `wg-logic.ts#isWarpDraft`），但**隐藏且不解释**会让用户分不清「不支持」与「没做」——
    // 同一形态本轮已在 `WarpDialog` 一并改掉，两个弹窗对同一个概念呈现一致。
    // ⚠️ 禁用只挡住「这次编辑新写入」，提交侧另有否决（`buildWgServer` 的 `isWarpDraft` 那支）。
    { t: 'switch', k: 'reverseMesh', label: 'wg.reverseMesh', zh: 'System 接入模式（内核接口）', hint: 'wg.reverseMeshHint', hintZh: '建真实内核接口：可被对端反向访问、可替子网转发。需 TUN 模式，Windows 不支持，不满足时自动降级为 gVisor 用户态。开启后该节点不参与测速。', disabled: isWarpDraft(draft, base), disabledHint: 'wg.reverseMeshWarp', disabledHintZh: 'WARP 不支持 System 接入模式：它会与主 TUN 抢同一个内核接口，开启后内核直接起不来（Connect: resource busy）。WARP 固定走 gVisor 用户态。' },
    { t: 'switch', k: 'allowInternet', label: 'wg.allowInternet', zh: '允许访问外网（全隧道）', hint: 'wg.allowInternetHint', hintZh: '在对端 AllowedIPs 注入 0.0.0.0/0, ::/0' },
    { t: 'switch', k: 'alwaysRouteSubnets', label: 'wg.alwaysRoute', zh: '始终路由子网', hint: 'wg.alwaysRouteHint', hintZh: '非当前出口时也保持上面网段可达' },
    // 前置代理 —— **对 上游的有意偏离**（它的 WG 表单没有这一项，`SingBoxEndpoint` 类型也没有
    // 这个键；生成侧的接线与实测见 `detour-options.ts` 文件头 / `singbox/endpoint.rs`）。
    //
    // hint 里那句 UDP 是承重的：WG 的握手走 **UDP**，经前置代理时是 SOCKS5 `UDP_ASSOCIATE`
    // （2026-07-31 loopback A/B 实测：有 detour ⇒ 直达 peer 的 UDP 包 **0**、SOCKS UDP_ASSOCIATE 15 次）。
    // 前置代理只支持 TCP ⇒ 本节点起不来，且**不回落直连**、没有任何报错 —— 用户看到的就是「连上了不通」。
    // 不写这句，这个控件就是个陷阱。
    { t: 'select', k: 'detour', label: 'wg.detour', zh: '前置代理 / detour', options: detourOpts, hint: 'wg.detourHint', hintZh: '先经该节点再拨号。WireGuard 握手走 UDP，前置代理必须支持 UDP 转发（SOCKS5 UDP ASSOCIATE），否则本节点静默不通——不会回落直连。' },
  ];
}

function WgIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z" />
    </svg>
  );
}

interface WgPreview {
  peer: string;
  address: string;
  pubKey: string;
  routing: string;
  keepalive: string;
}
function previewFromDraft(d: WgDraft): WgPreview {
  const trunc = (k: string) => (k.length > 22 ? `${k.slice(0, 22)}…` : k);
  const specific = splitCsv(d.allowedIPs).filter((a) => !CATCH_ALL.has(a));
  const full = d.allowInternet;
  const routing =
    (full ? 'full tunnel' : '') + (specific.length ? (full ? ' + ' : '') + specific.join(', ') : full ? '' : '—');
  return {
    peer: `${d.address}:${d.port ?? ''}`,
    address: d.localAddress,
    pubKey: trunc(d.peerPublicKey),
    routing,
    keepalive: `${d.persistentKeepalive ?? 25}s`,
  };
}

function WgForm({ base }: { base?: ServerConfig }) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const loadConfig = useAppStore((s) => s.loadConfig);
  // 展示面：组网单例槽判据必须含暂存节点，否则暂存了一个 WARP/TS 还能再建第二个。
  const servers = useEffectiveServers();
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  const isEdit = base != null;

  const [src, setSrc] = useState<'manual' | 'conf'>('manual');
  const [name, setName] = useState(base?.name ?? '');
  const [draft, setDraft] = useState<WgDraft>(() => (base ? draftFromServer(base) : emptyWgDraft()));
  const [confText, setConfText] = useState('');
  const [confErr, setConfErr] = useState<string | null>(null);
  const [preview, setPreview] = useState<WgPreview | null>(null);

  const [dirty, setDirty] = useState(false);
  const [errName, setErrName] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [formTab, setFormTab] = useState<WgFormTab>('basic');

  const setField = (k: string, v: FormValue) => {
    setDraft((d) => ({ ...d, [k]: v }) as WgDraft);
    setDirty(true);
  };

  // 前置代理候选：排除自身与 endpoint 类节点（判据对齐生成侧，见 `detour-options.ts`）。
  const detourOpts = endpointDetourOptions(servers, base?.id, t('node.detourDirect', '直连（不串联）'));
  const visible = wgSpec(draft, base, detourOpts).filter((f) => !f.when || f.when(draft));

  const onParse = () => {
    const d = parseConfToDraft(confText);
    if (!d) {
      setPreview(null);
      setConfErr(t('wg.parseErr', '无法解析该 .conf——请检查内容或改用「手动填写」'));
      return;
    }
    setConfErr(null);
    setDraft(d); // 填入解析字段（name 顶部独立保留，不受 .conf 覆盖）
    setPreview(previewFromDraft(d));
    setDirty(true);
  };

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('wg.discardTitle', '放弃更改？'),
        message: t('wg.discardMsg', '已填写的内容将不会保存。'),
        confirmLabel: t('wg.discard', '放弃'),
        danger: true,
        onConfirm: () => {
          close();
          close();
        },
      },
    });
  };

  const handleSubmit = async () => {
    const err = validateWgDraft(name, draft);
    if (err) {
      if (err.field === 'name') {
        setErrName(true);
      } else {
        setSrc('manual');
        setFormTab('basic');
        // 文案自述完整（列全了缺哪些字段），不套 title。
        toast.error(t('wg.errRequired', '缺少必填字段：地址 / 私钥 / 接口地址 / 对端公钥'));
      }
      return;
    }
    // 填了但不满足消费侧谓词 ⇒ 拦下。不拦的话后端**静默忽略**，用户只会看到「保存成功但没生效」
    // （判据与理由见 `wg-logic.ts#reservedInputInvalid`）。回手动填写页，让出错的那个框可见。
    if (reservedInputInvalid(draft.reserved)) {
      setSrc('manual');
      setFormTab('advanced');
      toast.error(t('wg.errReserved', 'Reserved 需填 3 个 0–255 的整数，例如 0, 0, 0'));
      return;
    }
    const server = buildWgServer(name, draft, base);
    // WARP 单例硬闸门。**本弹窗是它最真实的旁路腿**：粘贴 Cloudflare 的 wg-quick `.conf`，端点
    // `engage.cloudflareclient.com` 会被 `isWarpServer` 的域名兜底判成 WARP（`domain/warp.ts:31-37`），
    // 于是在已有 WARP 时造出第二个 → 两者抢内核 utun → `Connect: resource busy` FATAL。
    // 传 base?.id：编辑现有 WARP 节点不算「再加一个」，必须放行。
    if (blockedByMeshSingleton(server, servers, t, base?.id)) return;

    setSubmitting(true);
    try {
      // 配置暂存闸门（与 NodeDialog 同形）。`editRoute` 是**唯一**判据：总开关关 / W-0 豁免 /
      // W-1·2·3 绕过任一命中都返 'direct'，走下面那条与今天逐字节相同的直落盘腿。
      // 手填 / 粘贴 .conf 两条来源都没有远端副作用，故 WG 节点整体落默认腿（进暂存）。
      if (editRoute('servers', stagingEnabled) === 'staged') {
        // 新增时前端自铸 id：后端 `ensure_server_id` 只在落盘那一刻补 id，而条目现在就需要一个稳定的
        // 实体寻址键（同一节点重复编辑要覆盖同一条）。带 id 提交后端照收。
        const entityId = server.id !== '' ? server.id : crypto.randomUUID();
        stage({
          id: `server:${entityId}`,
          kind: 'server',
          label: `${isEdit ? t('wg.editTitle', '编辑 WireGuard 节点') : t('wg.addTitle', '添加 WireGuard 节点')} ${server.name}`,
          entityPath: ['servers', entityId],
          nextValue: { ...server, id: entityId },
        });
        close();
        return; // 零 IPC 写、零磁盘写（FR-1）
      }
      if (isEdit && base) {
        await api.server.update(server);
      } else {
        const { id: _id, ...rest } = server;
        await api.server.add(rest);
      }
      void loadConfig(true);
      close();
    } catch (e) {
      toast.error(t('common.saveFailed', '保存失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      titleId="wg-dlg-title"
      title={isEdit ? t('wg.editTitle', '编辑 WireGuard 节点') : t('wg.addTitle', '添加 WireGuard 节点')}
      onClose={requestClose}
      icon={<WgIcon />}
      className="entry-form-dlg"
      footer={
        <>
          {/* 提交中**不锁**「取消」：原型 `:2545` 的 ghost 钮无 disabled，且本仓此前四个弹窗锁、
              两个不锁（NodeDialog/SubDialog）—— 不是与原型的差，是实现自己两套。统一为不锁：
              提交卡住（IPC 无应答）时用户必须还能退出，否则弹窗成了死窗。 */}
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button type="button" className="btn flow" onClick={() => void handleSubmit()} disabled={submitting}>
            {isEdit ? t('common.save', '保存') : t('wg.add', '添加')}
          </button>
        </>
      }
    >
      <div className="fld">
        <label className="fld-l" htmlFor="wg-name">
          {t('wg.name', '节点名称')}
        </label>
        <input
          id="wg-name"
          className="input"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setErrName(false);
            setDirty(true);
          }}
          placeholder={t('wg.namePh', '自建 · WireGuard')}
        />
        {errName && <div className="err-line">{t('wg.errName', '请先填写节点名称')}</div>}
      </div>

      <div className="fld">
        <label className="fld-l">{t('wg.source', '来源')}</label>
        <div className="seg2" role="group" aria-label={t('wg.source', '来源')} style={{ display: 'flex' }}>
          <button type="button" style={{ flex: 1 }} className={src === 'manual' ? 'on' : ''} onClick={() => setSrc('manual')}>
            {t('wg.manual', '手动填写')}
          </button>
          <button type="button" style={{ flex: 1 }} className={src === 'conf' ? 'on' : ''} onClick={() => setSrc('conf')}>
            {t('wg.paste', '粘贴 .conf')}
          </button>
        </div>
      </div>

      {src === 'conf' && (
        <>
          <div className="fld">
            <label className="fld-l" htmlFor="wg-conf">
              {t('wg.pasteLabel', '粘贴 wg-quick .conf')}
            </label>
            <textarea
              id="wg-conf"
              className="input mono"
              rows={7}
              value={confText}
              onChange={(e) => {
                setConfText(e.target.value);
                setConfErr(null);
                setDirty(true);
              }}
              placeholder={
                '[Interface]\nPrivateKey = wOE...=\nAddress = 10.0.0.2/32\n\n[Peer]\nPublicKey = HIg...=\nEndpoint = 203.0.113.7:51820\nAllowedIPs = 0.0.0.0/0, ::/0'
              }
            />
          </div>
          <button type="button" className="btn ghost sm" onClick={onParse}>
            <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
            </svg>
            <span>{t('wg.parse', '解析预览')}</span>
          </button>
          {confErr && <div className="wg-conf-err">{confErr}</div>}
          {preview && (
            <div className="wg-preview">
              <div className="wpv-h">
                <svg viewBox="0 0 24 24" width={14} fill="none" stroke="currentColor" strokeWidth={2.6}>
                  <path d="M20 6L9 17l-5-5" />
                </svg>
                {t('wg.parsed', '解析成功 · 字段已填入表单')}
              </div>
              <div className="wpv-row"><span>{t('wg.pvPeer', '对端地址')}</span><span>{preview.peer}</span></div>
              <div className="wpv-row"><span>{t('wg.pvAddr', '接口地址')}</span><span>{preview.address}</span></div>
              <div className="wpv-row"><span>{t('wg.pvPub', '对端公钥')}</span><span>{preview.pubKey}</span></div>
              <div className="wpv-row"><span>{t('wg.pvRoute', '路由网段')}</span><span>{preview.routing}</span></div>
              <div className="wpv-row"><span>{t('wg.pvKeep', '保活')}</span><span>{preview.keepalive}</span></div>
            </div>
          )}
        </>
      )}

      {src === 'manual' && (() => {
        const groups = groupWgFields(visible);
        const tabs: WgFormTab[] = ['basic', 'routing', 'advanced'];
        return (
          <>
            <div className="sub-tabs form-tabs" role="tablist" aria-label={t('node.formGroup.aria', '配置分组')}>
              {tabs.map((tab) => (
                <button
                  key={tab}
                  type="button"
                  role="tab"
                  className={formTab === tab ? 'on' : ''}
                  aria-selected={formTab === tab}
                  onClick={() => setFormTab(tab)}
                >
                  {t(`node.formGroup.${tab}`)}
                </button>
              ))}
            </div>
            <div role="tabpanel" className="form-tab-panel">
              {groups[formTab].map((f) => (
                <FieldRenderer key={f.k} spec={f} value={draft[f.k]} onChange={(v) => setField(f.k, v)} />
              ))}
            </div>
          </>
        );
      })()}
    </Modal>
  );
}

export function WgDialog({ serverId }: { serverId?: string }) {
  // 展示面：编辑基准（读盘的话暂存过的节点再打开会显示改前的旧值）。
  const servers = useEffectiveServers();
  const base = serverId ? servers.find((s) => s.id === serverId) : undefined;
  // R1：key 绑 serverId —— 切换编辑目标 = 重挂 = 同步重新初始化。
  return <WgForm key={serverId ?? 'new'} base={base} />;
}

export default WgDialog;
