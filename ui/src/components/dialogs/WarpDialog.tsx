/**
 * WarpDialog —— Cloudflare WARP 接入/编辑弹窗（原型 #warp-dialog :2867，warpSetMode :4974）。
 *
 * **单例槽**：WARP 至多一个节点（`domain/warp.ts#findWarpNode`）。两提交路径共用一个弹窗：
 *  - **注册态**（edit 假）：`api.server.registerWarp(license?)`（REAL：X25519 keypair + Cloudflare 匿名注册）
 *    → 拿 `WarpWireGuardDraft` 草稿 → 用表单覆盖（名称/路由/MTU/…）后 `api.server.add` 落为 WireGuard 节点
 *    （registerWarp 本身只返草稿不落盘，见 warp.rs / warp.ts 头注释 + server.rs:320）。
 *  - **编辑态**（edit 真）：预填现有 WARP 节点；WARP+ 许可经 `api.server.applyWarpLicense`（REAL，原地升级免重建），
 *    名称/路由改动经 `api.server.update`。
 *
 * 表单：顶部（名称 / 许可类型 seg2 / WARP+ 密钥）手写；高级折叠（端点/路由/MTU/保活/Reserved/detour）
 * 走 D2 FieldSpec 表 + FieldRenderer（§1.4 多字段驱动，复用节点表单同一渲染器）。
 * R1：`key` 绑编辑目标 id（见导出包装）+ useState 同步初始化。
 */

import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveServers } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { splitStagedOnly, stagedOnlyIds } from '@/lib/staged-config';
import { api } from '@/ipc';
import type { ServerConfig, WireGuardSettings } from '@/contracts/types';
import { findWarpNode, type WarpWireGuardDraft } from '@/domain/warp';
import { registerWarpIfSlotFree } from '@/domain/mesh-singleton-guard';
import { Modal } from './Modal';
import {
  FieldRenderer,
  type FieldSpec,
  type FormValue,
  type FormValues,
  type SelectOption,
} from './field-spec';
import { applyDetour, endpointDetourOptions, DETOUR_NONE } from './detour-options';
// 表单 → WireGuardSettings 的整段接线共用 `wg-logic.ts`（含 `parseReserved`：判据 = 后端消费侧
// 谓词，见该函数文档；此处原有一份更松的私有副本，与后端 `Vec<u32>` + `len()==3` 两处都对不上）。
import { buildWarpSettings } from './wg-logic';
import { useDialogStore } from './dialog-store';
import { Fold } from '@/components/Fold';

const CATCH_ALL = new Set(['0.0.0.0/0', '::/0']);

function WarpIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <circle cx="12" cy="12" r="9" />
      <path d="M3 12h18M12 3a14 14 0 000 18M12 3a14 14 0 010 18" />
    </svg>
  );
}

/** "host:port" → {host,port}；非法返 null（IPv6 用 [::1]:port）。 */
function parseHostPort(ep: string): { host: string; port: number } | null {
  const s = ep.trim();
  if (!s) return null;
  let host: string;
  let portStr: string;
  if (s.startsWith('[')) {
    const c = s.indexOf(']');
    if (c < 0) return null;
    host = s.slice(1, c);
    const rest = s.slice(c + 1);
    if (!rest.startsWith(':')) return null;
    portStr = rest.slice(1);
  } else {
    const i = s.lastIndexOf(':');
    if (i < 0) return null;
    host = s.slice(0, i);
    portStr = s.slice(i + 1);
  }
  const port = Number(portStr);
  if (!host || !Number.isInteger(port) || port <= 0 || port > 65535) return null;
  return { host, port };
}

function advSpec(detourOpts: readonly SelectOption[]): FieldSpec[] {
  return [
    { t: 'text', k: 'endpoint', label: 'warp.endpoint', zh: '端点 Endpoint', mono: true },
    {
      t: 'select',
      k: 'route',
      label: 'warp.route',
      zh: '路由 AllowedIPs',
      options: [
        ['all', '全局出口（0.0.0.0/0, ::/0）'],
        ['custom', '仅特定网段'],
      ],
    },
    { t: 'text', k: 'allowedIPs', label: 'warp.allowed', zh: '自定义网段', ph: '10.0.0.0/24, 192.168.1.0/24', mono: true, when: (v) => v.route === 'custom' },
    { t: 'number', k: 'mtu', label: 'warp.mtu', zh: 'MTU', ph: '1280', mono: true },
    { t: 'number', k: 'keepalive', label: 'warp.keepalive', zh: '保活（秒）', ph: '25', mono: true },
    { t: 'text', k: 'reserved', label: 'warp.reserved', zh: 'Reserved', ph: '0, 0, 0', mono: true, opt: true },
    // 接入模式 —— **可见但禁用**，位置与 `WgDialog` 的 `wgSpec` 那条对齐（同在 `reserved` 之后）。
    //
    // 此前本弹窗**干脆不渲染**这一项，`WgDialog` 那条也用 `when` 把 WARP 草稿整条隐掉。隐藏是对的
    // （WARP 走 system 内核接口会与主 TUN 抢 utun ⇒ `Connect: resource busy` FATAL，真机实证记在
    // `domain/warp.ts:39`），**但「隐藏且不解释」不是**：用户在 WG 弹窗见过这个开关，到 WARP 弹窗它
    // 凭空消失，无从分辨是「不支持」「没做」还是「藏在别处」。改为禁用 + 写明后果，两个弹窗对同一个
    // 概念呈现一致（同 `ts-settings-logic.ts#exitNodeOptions` 对「未广告出口的对端」的处理）。
    //
    // label 复用 `wg.*` 键：WARP 就是 WireGuard 节点，同一个概念不再造第二份五语翻译。
    // ⚠️ 禁用只挡住「这次编辑新写入」，提交侧另有恒否决（`wg-logic.ts#buildWarpSettings`）。
    { t: 'switch', k: 'reverseMesh', label: 'wg.reverseMesh', zh: 'System 接入模式（内核接口）', disabled: true, disabledHint: 'wg.reverseMeshWarp', disabledHintZh: 'WARP 不支持 System 接入模式：它会与主 TUN 抢同一个内核接口，开启后内核直接起不来（Connect: resource busy）。WARP 固定走 gVisor 用户态。' },
    // 前置代理 —— **对 上游的有意偏离**（它的 WARP 表单没有这一项）。本轮之前这个控件是个
    // **装饰开关**：值写进了 `server.detour`，但 Rust 侧 `Endpoint` 结构体压根没有 detour 字段，
    // 序列化时被丢掉。现已真生效（`builder/outbounds.rs` 的 WG 腿；WARP = reverseMesh 的 WG 节点）。
    //
    // hint 那句 UDP 与 `WgDialog` 同源同因：WARP 就是 WireGuard，握手走 UDP，前置代理不支持
    // UDP 转发就静默不通且不回落直连（实测见 `singbox/endpoint.rs`）。
    { t: 'select', k: 'detour', label: 'warp.detour', zh: '前置代理 / detour', options: detourOpts, hint: 'warp.detourHint', hintZh: '先经该节点再拨号。WARP 是 WireGuard，握手走 UDP，前置代理必须支持 UDP 转发（SOCKS5 UDP ASSOCIATE），否则本节点静默不通——不会回落直连。' },
  ];
}

interface WarpFormProps {
  editNode?: ServerConfig;
  servers: ServerConfig[];
}

function WarpForm({ editNode, servers }: WarpFormProps) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagedEntries = useStagedConfigStore((s) => s.entries);
  /** staged-only 差集的两个入参（展示面 effective + 操作面磁盘镜像），判据与节点卡同一函数。 */
  const diskServers = useAppStore((s) => s.servers);
  const stagedOnly = useMemo(() => stagedOnlyIds(servers, diskServers), [servers, diskServers]);
  const isEdit = editNode != null;

  // 前置代理候选。此前是「除自身外的全部节点」—— 其中的 WG/TS 节点在生成侧会被
  // `is_mesh_protocol` 那一支直接丢弃（选了等于没选）。收口到与 `WgDialog` /
  // `TsSettingsDialog` 同一个函数，排除判据一处定义、三处生效。
  const detourOpts = endpointDetourOptions(servers, editNode?.id, t('node.detourDirect', '直连（不串联）'));
  const spec = advSpec(detourOpts);

  // R1 同步初始化。
  const initWs = editNode?.wireguardSettings;
  const initSpecific = (initWs?.allowedIPs ?? []).filter((a) => !CATCH_ALL.has(a));
  // 新建默认名 `WARP`（不是 `Cloudflare WARP`）：节点卡的名字位很窄，长名会被截断成「Cloudflare W…」，
  // 而「Cloudflare」这一段对用户毫无区分度 —— 单例槽位里只可能有一个 WARP。改名不影响身份判定：
  // `isWarpServer` 认的是 `warpDevice` 凭据与 `*.cloudflareclient.com` 端点域名，从不看 name。
  const [name, setName] = useState(editNode?.name ?? 'WARP');
  const [plan, setPlan] = useState<'free' | 'plus'>('free');
  const [license, setLicense] = useState('');
  const [draft, setDraft] = useState<FormValues>(() =>
    editNode
      ? {
          endpoint: `${editNode.address}:${editNode.port}`,
          route: initWs?.allowInternet === false ? 'custom' : 'all',
          allowedIPs: initSpecific.join(', '),
          mtu: initWs?.mtu ?? 1280,
          keepalive: initWs?.persistentKeepalive ?? 25,
          reserved: (initWs?.reserved ?? []).join(', '),
          detour: editNode.detour || DETOUR_NONE,
        }
      : {
          endpoint: 'engage.cloudflareclient.com:2408',
          route: 'all',
          allowedIPs: '',
          mtu: 1280,
          keepalive: 25,
          reserved: '',
          detour: DETOUR_NONE,
        },
  );

  const [errName, setErrName] = useState(false);
  const [errLicense, setErrLicense] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [done, setDone] = useState(false);
  const [dirty, setDirty] = useState(false);

  const setField = (k: string, v: FormValue) => {
    setDraft((d) => ({ ...d, [k]: v }));
    setDirty(true);
  };
  const visible = spec.filter((f) => !f.when || f.when(draft));

  const requestClose = () => {
    if (!dirty || done) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('warp.discardTitle', '放弃更改？'),
        message: t('warp.discardMsg', '已填写的内容将不会保存。'),
        confirmLabel: t('warp.discard', '放弃'),
        danger: true,
        onConfirm: () => {
          close();
          close();
        },
      },
    });
  };

  // 表单值 → WireGuardSettings 覆盖片段（注册草稿/编辑基础之上叠加）。实现在 `wg-logic.ts`：
  // 与 WG 那条同名否决并排放，且作为纯函数可被 `wg-logic.test.ts` 直接上牙。
  const settingsOverride = (base: Partial<WireGuardSettings>): WireGuardSettings =>
    buildWarpSettings(base, draft);

  const currentDetour = typeof draft.detour === 'string' ? draft.detour : DETOUR_NONE;

  const doRegister = async (ep: { host: string; port: number }) => {
    // 单例闸**前置到 CF 请求之前**：registerWarp 在 Cloudflare 侧真建一台匿名设备（远端副作用、
    // 本地拦不回来）。接入区卡片只在「打开弹窗那一刻」无 WARP 时才给入口，弹窗停留期间槽位可能被
    // 克隆/导入/WgDialog 抢走 —— 那时先打请求再拦 = 白烧一台孤儿设备。闸不过：toast + 返回 null，零请求。
    const draftResp: WarpWireGuardDraft | null = await registerWarpIfSlotFree(servers, t, () =>
      api.server.registerWarp(plan === 'plus' ? license.trim() : undefined),
    );
    if (!draftResp) return; // 已 toast；保持弹窗打开，用户可取消或先去处理现有 WARP
    const base: Partial<WireGuardSettings> = {
      privateKey: draftResp.privateKey,
      localAddress: draftResp.localAddress,
      peerPublicKey: draftResp.peerPublicKey,
      reserved: draftResp.reserved,
      mtu: draftResp.mtu,
      warpDevice: draftResp.warpDevice,
    };
    const settings = settingsOverride(base);
    const server: Omit<ServerConfig, 'id'> = {
      name: name.trim(),
      protocol: 'wireguard',
      address: ep.host,
      port: ep.port,
      wireguardSettings: settings,
    };
    applyDetour(server, currentDetour);
    await api.server.add(server);
  };

  const doEdit = async (ep: { host: string; port: number }) => {
    if (!editNode) return;
    // `block`（ENTITY_ACTION_TABLE）：applyWarpLicense 改的是远端账户等级、update 改的是一台
    // **已注册**的设备 —— 盘上没有这个节点，两者都没有作用对象。
    const split = splitStagedOnly(
      'warp.edit',
      [editNode.id],
      stagedOnly,
      stagedEntries,
      'servers'
    );
    if (split.blocked.length > 0) {
      // 同 NodesScreen:687 / TsSettingsDialog：「还不能做」走 info 单参，文案自述完整。
      toast.info(
        t('home.stagedOnlyBlocked', '该项还没保存到配置文件，此操作要保存后才能进行。点条上的「保存」后再试')
      );
      throw new Error('staged-only-blocked');
    }
    if (plan === 'plus' && license.trim()) {
      const r = await api.server.applyWarpLicense(editNode.id, license.trim());
      if (!r.ok) {
        // r.error 是后端原始串（非自述）⇒ 套 title；no-credentials 那支本身自述，单参。
        if (r.error === 'no-credentials') {
          toast.error(t('warp.errNoCreds', '该节点缺少 WARP 设备凭据，无法应用 WARP+ 许可'));
        } else {
          toast.error(t('warp.errLicense', 'WARP+ 许可应用失败'), r.error ?? undefined);
        }
        throw new Error('applyLicense-failed');
      }
    }
    const settings = settingsOverride(editNode.wireguardSettings ?? {});
    const server: ServerConfig = {
      ...editNode,
      name: name.trim(),
      address: ep.host,
      port: ep.port,
      wireguardSettings: settings,
    };
    applyDetour(server, currentDetour);
    await api.server.update(server);
  };

  const handleSubmit = async () => {
    if (done) {
      close();
      return;
    }
    if (!name.trim()) {
      setErrName(true);
      return;
    }
    if (plan === 'plus' && !license.trim()) {
      setErrLicense(true);
      return;
    }
    // M6：端点解析失败必须内联报错并保持弹窗打开，不得静默回退旧地址后假装提交成功。
    const ep = parseHostPort(String(draft.endpoint ?? ''));
    if (!ep) {
      toast.error(t('warp.errEndpoint', '端点格式应为 host:port（IPv6 用 [::1]:port），请检查后重试'));
      return;
    }
    setSubmitting(true);
    try {
      if (isEdit) {
        await doEdit(ep);
        void loadConfig(true);
        close();
      } else {
        await doRegister(ep);
        void loadConfig(true);
        setDone(true);
      }
    } catch (e) {
      if (!(e instanceof Error && e.message === 'applyLicense-failed')) {
        toast.error(t('common.saveFailed', '保存失败'), e instanceof Error ? e.message : String(e));
      }
    } finally {
      setSubmitting(false);
    }
  };

  const primaryLabel = done
    ? t('common.done', '完成')
    : isEdit
      ? t('common.save', '保存')
      : t('warp.register', '注册并接入');

  return (
    <Modal
      titleId="warp-dlg-title"
      title={isEdit ? t('warp.editTitle', '编辑 WARP') : t('warp.addTitle', '接入 Cloudflare WARP')}
      onClose={requestClose}
      icon={<WarpIcon />}
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
            {submitting && <span className="spinner spin-inline" style={{ marginRight: 6 }} />}
            {primaryLabel}
          </button>
        </>
      }
    >
      {!done && (
        <>
          {!isEdit && (
            <div className="mesh-note">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                <path d="M18 10h-1.26A8 8 0 109 20h9a5 5 0 000-10z" />
              </svg>
              <span>
                {t('warp.note', '一键注册匿名 WARP 设备并加入为 WireGuard 节点——无需填写密钥；之后可在「编辑」里微调路由。')}
              </span>
            </div>
          )}

          <div className="fld" style={{ marginTop: 12 }}>
            <label className="fld-l" htmlFor="warp-name">
              {t('warp.name', '节点名称')}
            </label>
            <input
              id="warp-name"
              className="input"
              value={name}
              onChange={(e) => {
                setName(e.target.value);
                setErrName(false);
                setDirty(true);
              }}
            />
            {errName && <div className="err-line">{t('warp.errName', '请填写节点名称')}</div>}
          </div>

          <div className="fld">
            <label className="fld-l">{t('warp.plan', '许可类型')}</label>
            <div className="seg2" role="group" aria-label={t('warp.plan', '许可类型')}>
              <button
                type="button"
                className={plan === 'free' ? 'on' : ''}
                onClick={() => {
                  setPlan('free');
                  setErrLicense(false);
                  setDirty(true);
                }}
              >
                {t('warp.planFree', '免费 WARP')}
              </button>
              <button
                type="button"
                className={plan === 'plus' ? 'on' : ''}
                onClick={() => {
                  setPlan('plus');
                  setDirty(true);
                }}
              >
                WARP+
              </button>
            </div>
          </div>

          {plan === 'plus' && (
            <div className="fld">
              <label className="fld-l" htmlFor="warp-license">
                {t('warp.licenseLabel', 'WARP+ 许可密钥')}
              </label>
              <input
                id="warp-license"
                className="input mono"
                value={license}
                onChange={(e) => {
                  setLicense(e.target.value);
                  setErrLicense(false);
                  setDirty(true);
                }}
                placeholder="xxxxxxxx-xxxxxxxx-xxxxxxxx"
              />
              {errLicense && <div className="err-line">{t('warp.errLicense2', '请填写 WARP+ 许可密钥')}</div>}
              <div className="card-sub" style={{ marginTop: 6 }}>
                {t('warp.licenseHint', '填入 WARP+ 许可密钥即注册/升级为 WARP+；免费版无需填写。')}
              </div>
            </div>
          )}

          <Fold title={t('warp.advanced', '路由 / 高级')}>
            {visible.map((f) => (
              <FieldRenderer key={f.k} spec={f} value={draft[f.k]} onChange={(v) => setField(f.k, v)} />
            ))}
          </Fold>

          {submitting && !isEdit && (
            <div className="ts-status">
              <span className="spinner spin-inline" />
              <span>{t('warp.registering', '正在注册匿名设备…')}</span>
            </div>
          )}
        </>
      )}

      {done && (
        <div className="mesh-success">
          <svg viewBox="0 0 24 24" width={18} fill="none" stroke="currentColor" strokeWidth={2.6} style={{ color: 'hsl(var(--ok))' }}>
            <path d="M20 6L9 17l-5-5" />
          </svg>
          <div>
            <b>{t('warp.doneTitle', 'WARP 设备已注册')}</b>
            <div className="card-sub" style={{ marginTop: 3 }}>
              {t('warp.doneSub', '已加入为 WireGuard 节点。')}
            </div>
          </div>
        </div>
      )}
    </Modal>
  );
}

export function WarpDialog({ edit }: { edit?: boolean }) {
  // 展示面：编辑基准 + WARP 单例槽判据（含暂存节点更保守：暂存里已有 WARP 时不再发远端注册）。
  const servers = useEffectiveServers();
  const editNode = edit ? findWarpNode(servers) : undefined;
  // R1：key 绑「edit + 目标节点 id」—— 注册态↔编辑态切换重挂重init。
  return <WarpForm key={editNode?.id ?? (edit ? 'edit' : 'new')} editNode={editNode} servers={servers} />;
}

export default WarpDialog;
