/**
 * TsSettingsDialog —— Tailscale 设置弹窗（原型 #ts-settings-dialog :2802）。
 *
 * 本层**最长表单**（主机名 / 出口节点选择 / 接入模式 / 子网路由两向 / 高级设置）按
 * 基础连续区 / 路由分段 / 高级折叠渐进展示；footer 固定
 * 验收样本：body 超高时 `.dlg-body` 独立滚动、`.dlg-foot` 常驻（Modal 原语的三段结构自动保证）。
 * 多字段驱动走 D2 FieldSpec 表 + FieldRenderer（switch → .swt-row，select → Csel）。
 *
 * ⚠️ **FieldSpec 表是覆盖门的判据面**：`contracts/protocol-settings-coverage.test.ts` 解析 Rust
 * `TailscaleSettings` 的 serde 键集，要求每个键在这张表里有对应控件，否则转红（豁免须写进
 * 该门的 `EXEMPT` 并附理由）。所以给 Rust 结构体加字段而不在这里加一项 = 门红，不是静默漏掉。
 *
 * **后端现状**：
 *  - `api.server.tailscaleGetStatus` = REAL（`server.rs`：读 `MeshRuntime` 的 STATUS 末帧缓存，`connected`
 *    = 主核是否在跑）→ 核未跑 / 无在册 TS 节点时候选为空 → 优雅降级：下拉仅「无 / 自定义…」，
 *    给「未连接或无可用出口，可手动填写」提示，不空白卡死。
 *  - `api.server.tailscaleLogout` = REAL（清 state 目录）。
 *  - 保存经 `api.server.update`（把表单写回该 TS 节点的 tailscaleSettings）。
 *
 * 无 TS 节点时（未登录）：给 mesh-note 引导先登录，Save/Logout 置灰（无可写目标）。
 * R1：`key` 绑 TS 节点 id（见导出包装）+ useState 同步初始化。
 */

import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveServers } from '@/store/app-store';
import { api } from '@/ipc';
import type { ServerConfig } from '@/contracts/types';
import type { TailscaleSettings } from '@/contracts/types';
import type { TailscaleStatusPeer } from '@/contracts/tailscale-status';
import { Modal } from './Modal';
import {
  FormFields,
  FormSection,
  type FieldSpec,
  type FormValue,
  type FormValues,
  type SelectOption,
} from './field-spec';
import {
  buildTsSettings,
  exitNodeOptions,
  initTsDraft,
  invalidTsCidrs,
  invalidControlUrl,
  EXIT_CUSTOM,
} from './ts-settings-logic';
import { applyDetour, endpointDetourOptions } from './detour-options';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { splitStagedOnly, stagedOnlyIds } from '@/lib/staged-config';
import { editRoute } from '@/lib/staged-config';
import { useDialogStore } from './dialog-store';
import { INVALID_NODE_REASON_KEY } from '@/domain/invalid-node-reason';
import { groupTsFields } from './mesh-form-layout';

function TsSetIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6" />
    </svg>
  );
}

function mainSpec(
  exitOpts: readonly SelectOption[],
  detourOpts: readonly SelectOption[]
): FieldSpec[] {
  return [
    { t: 'text', k: 'hostname', label: 'ts.hostname', ph: 'sway-macbook' },
    { t: 'select', k: 'exitNode', label: 'ts.exitNode', options: exitOpts },
    { t: 'text', k: 'exitNodeCustom', label: 'ts.exitNodeCustom', ph: '100.x.y.z / hostname', mono: true, when: (v) => v.exitNode === EXIT_CUSTOM },
    // 接入模式（上游 `AccessModeField`，同绑 reverseMesh）：上游 归常显的「接入与出口」段，故**不入高级折叠**——
    // 它决定 `meshUsesSystemInterface`，进而决定该节点是否参与测速（domain/endpoint-routes.ts:320）。
    // 藏起来 = 用户卡在「为什么这个节点测不出速」。降级由后端兜底（builder/outbounds.rs:145 在非 TUN /
    // Windows 上把 system_interface 打回 false），故此处只需如实告知，不必复刻 上游的置灰选择器。
    { t: 'switch', k: 'reverseMesh', label: 'ts.reverseMesh', hint: 'ts.reverseMeshHint' },
    { t: 'switch', k: 'alwaysRouteSubnets', label: 'ts.alwaysRoute', hint: 'ts.alwaysRouteHint' },
    { t: 'switch', k: 'acceptRoutes', label: 'ts.acceptRoutes', hint: 'ts.acceptRoutesHint' },
    // routes ≠ advertiseRoutes（两个相反方向，上游 同样分列两处、绝不合并）：
    //   routes         = 把这些网段的流量**送进**此节点（force-route 源，等价 WG allowedIPs）；
    //   advertiseRoutes= 本机作子网路由器**对外宣告**我能到达这些段。
    { t: 'text', k: 'routes', label: 'ts.routes', ph: '192.168.50.0/24, 10.0.0.0/24', mono: true, opt: true },
    { t: 'switch', k: 'exitNodeAllowLanAccess', label: 'ts.allowLan', hint: 'ts.allowLanHint' },
    { t: 'text', k: 'advertiseRoutes', label: 'ts.advertiseRoutes', ph: '192.168.1.0/24, 10.0.0.0/8', mono: true, opt: true },
    // 前置代理 —— **对 上游的有意偏离**（它的 Tailscale 表单没有这一项）。接线与实测见
    // `detour-options.ts` 文件头 / `crates/config-engine/src/singbox/endpoint.rs`。
    //
    // **提示文案与 WG/WARP 那两处刻意不同**：Tailscale 经前置代理的是**控制面 / DERP 的 TCP 拨号**
    // （2026-07-31 loopback A/B 实测：有 detour ⇒ 控制面直连 0 次、SOCKS5 `CONNECT` 32 次），
    // 只需 TCP，没有 WG 那条「必须支持 UDP 转发」的硬约束。抄同一句话会误导用户去换代理。
    { t: 'select', k: 'detour', label: 'ts.detour', options: detourOpts, hint: 'ts.detourHint' },
  ];
}
const ADV_SPEC: FieldSpec[] = [
  { t: 'text', k: 'controlUrl', label: 'ts.controlUrl', ph: 'https://controlplane.tailscale.com', mono: true, opt: true },
  { t: 'text', k: 'advertiseTags', label: 'ts.aclTags', ph: 'tag:server, tag:exit', mono: true, opt: true },
  { t: 'switch', k: 'ephemeral', label: 'ts.ephemeral', hint: 'ts.ephemeralHint' },
  // 低频专家项，跟 上游 一样归「高级」。u16：越界值会让整份 UserConfig 反序列化失败（同 server_config.rs:208
  // 记的那类整机不可用），故提交前限 1..=65535，越界按未填处理。
  { t: 'number', k: 'relayServerPort', label: 'ts.relayPort', ph: '0', mono: true, opt: true },
  { t: 'switch', k: 'sshServer', label: 'ts.ssh', hint: 'ts.sshHint' },
  // P4b 按名解析：与 acceptDefaultResolvers 强联动。后端 `accept_default_resolvers` **只在 resolveByName
  // 为真的分支里被读**（builder/dns.rs:1069，选节点的谓词就是 resolve_by_name==Some(true)）—— 故此前
  // 「接受 DNS（MagicDNS）」那个常显开关是**恒无效**的：resolveByName 无处可设 ⇒ dns-tailscale server 永不发射。
  // 两者同归高级并加 `when` 门控，既复刻 上游 分区，也让「开了没反应」这件事结构上不再可能。
  { t: 'switch', k: 'resolveByName', label: 'ts.resolveByName', hint: 'ts.resolveByNameHint' },
  { t: 'switch', k: 'acceptDefaultResolvers', label: 'ts.acceptDefaultResolvers', hint: 'ts.acceptDefaultResolversHint', when: (v) => v.resolveByName === true },
];

function TsSettingsForm({ node }: { node?: ServerConfig }) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  const stagedEntries = useStagedConfigStore((s) => s.entries);
  /** staged-only 差集的两个入参（展示面 effective + 操作面磁盘镜像），判据与节点卡同一函数。 */
  const effectiveServers = useEffectiveServers();
  const diskServers = useAppStore((s) => s.servers);
  const stagedOnly = useMemo(
    () => stagedOnlyIds(effectiveServers, diskServers),
    [effectiveServers, diskServers]
  );

  const [peers, setPeers] = useState<TailscaleStatusPeer[]>([]);
  const [connected, setConnected] = useState<boolean | null>(null);
  const [draft, setDraft] = useState<FormValues>(() => initTsDraft(node));
  const [busy, setBusy] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [revealAdvanced, setRevealAdvanced] = useState(false);

  // 出口候选：拉状态快照（核未跑 / 无节点时为空）。connected=false 时静态提示手动填写。
  // **原样收下全部 peer**，不在这里筛 `exitNodeOption` —— 「没广告出口」与「不在 tailnet 里」
  // 此前在界面上是同一种表现（都不在列表里），用户无从知道该去哪台机器上开那个开关。
  // 筛/排/去重/禁用/注记一律下沉 `exitNodeOptions`（纯函数，有单测）。
  useEffect(() => {
    let cancelled = false;
    api.server
      .tailscaleGetStatus()
      .then((snap) => {
        if (cancelled) return;
        setConnected(snap.connected);
        setPeers(snap.statuses.flatMap((s) => s.peers));
      })
      .catch(() => {
        /* 非 Tauri / 失败 → 保持空候选，走手动填写降级 */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // 判据取**已保存值**而非草稿值（禁用豁免须在整个弹窗生命期内稳定，见 exitNodeOptions 头注）。
  const savedExit = node?.tailscaleSettings?.exitNode ?? '';
  const exitOpts = exitNodeOptions(peers, savedExit, {
    none: t('ts.exitNone'),
    custom: t('common.customEllipsis'),
    inUse: t('ts.exitInUse'),
    offline: t('ts.exitOffline'),
    notAdvertised: t('ts.exitNotAdvertised'),
  });
  // 前置代理候选：排除自身与 endpoint 类节点（判据对齐生成侧，见 `detour-options.ts`）。
  const detourOpts = endpointDetourOptions(
    effectiveServers,
    node?.id,
    t('node.detourDirect')
  );
  const spec = mainSpec(exitOpts, detourOpts);
  const setField = (k: string, v: FormValue) => {
    setDraft((d) => ({ ...d, [k]: v }));
    setDirty(true);
    if (revealAdvanced) setRevealAdvanced(false);
  };
  const groups = groupTsFields([...spec, ...ADV_SPEC]);

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('ts.discardTitle'),
        message: t('ts.discardMsg'),
        confirmLabel: t('ts.discard'),
        danger: true,
        onConfirm: () => {
          close();
          close();
        },
      },
    });
  };

  const buildSettings = (): TailscaleSettings =>
    buildTsSettings(node?.tailscaleSettings, draft);

  const handleSave = async () => {
    if (!node) return;
    // 非法 CIDR 必须前端拦：后端 sanitize 对非法项是**静默丢弃**，不拦就成了「界面收下了、盘上没有」。
    const badCidr = invalidTsCidrs(draft);
    if (badCidr.length) {
      toast.error(t('ts.errCidr', { list: badCidr.join(', ') }));
      return;
    }
    // control_url 必须前端拦，且必须拦在**保存**这一刻：IP 形式会让 sing-box 在初始化 tailscale
    // endpoint 时直接 panic（见 `domain/control-url.ts` 头注）。后端虽有 fail-closed 兜底（生成配置时
    // 剔掉该节点），但那是**事后**告知——用户那时已经离开本弹窗，看到的只是节点卡置灰。
    // 拦在这里，光标还在这个输入框旁边，改一下就好了。
    const badControl = invalidControlUrl(draft);
    if (badControl) {
      setRevealAdvanced(true);
      toast.error(t('ts.errControlUrl'), t(INVALID_NODE_REASON_KEY[badControl]));
      return;
    }
    setBusy(true);
    try {
      // detour 在顶层，`buildTsSettings` 够不着 —— 单独写回（哨兵 ⇒ 删键）。
      const next = applyDetour({ ...node, tailscaleSettings: buildSettings() }, draft.detour);
      // 配置暂存闸门（与 NodeDialog 同形）。本弹窗改的是**该节点的 tailscaleSettings**，落在
      // `servers` 键上、无远端副作用 ⇒ 走默认腿。（弹窗里从活态回读的只有出口候选列表，不是被写的字段，
      // 故 W-2 不成立；真正有远端效应的是下面的 `handleLogout`，它另走绕过腿。）
      if (editRoute('servers', stagingEnabled) === 'staged') {
        stage({
          id: `server:${node.id}`,
          kind: 'server',
          label: `${t('ts.settingsTitle')} ${node.name}`,
          entityPath: ['servers', node.id],
          nextValue: next,
        });
        close();
        return; // 零 IPC 写、零磁盘写（FR-1）
      }
      await api.server.update(next);
      void loadConfig(true);
      close();
    } catch (e) {
      toast.error(t('common.saveFailed'), e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleLogout = async () => {
    if (!node) return;
    // `block`（ENTITY_ACTION_TABLE）：登出清的是磁盘上的 TS state 目录，盘上没有这个节点就没有对象。
    const split = splitStagedOnly(
      'server.tailscaleLogout',
      [node.id],
      stagedOnly,
      stagedEntries,
      'servers'
    );
    if (split.blocked.length > 0) {
      // 与 NodesScreen:687 的同一条闸门同一形态：这是「还不能做」的提示而非错误，走 info 单参
      // （文案自述完整，不套 title）。
      toast.info(
        t('home.stagedOnlyBlocked')
      );
      return;
    }
    setBusy(true);
    try {
      await api.server.tailscaleLogout(node.id);
      void loadConfig(true);
      close();
    } catch (e) {
      // 登出不是保存 —— 标题取 NodesScreen:696 同一操作已在用的那个键，别套 `common.saveFailed`。
      toast.error(t('nodes.meshTsLogoutFail'), e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      titleId="ts-set-title"
      title={t('ts.settingsTitle')}
      onClose={requestClose}
      icon={<TsSetIcon />}
      className="entry-form-dlg"
      footer={
        <>
          <button
            type="button"
            className="btn ghost"
            onClick={() => void handleLogout()}
            disabled={busy || !node}
            style={{ marginRight: 'auto', color: 'hsl(var(--err))', borderColor: 'hsl(var(--err)/0.3)' }}
          >
            {t('ts.logout')}
          </button>
          {/* 提交中**不锁**「取消」：原型 `:2545` 的 ghost 钮无 disabled，且本仓此前四个弹窗锁、
              两个不锁（NodeDialog/SubDialog）—— 不是与原型的差，是实现自己两套。统一为不锁：
              提交卡住（IPC 无应答）时用户必须还能退出，否则弹窗成了死窗。 */}
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn flow" onClick={() => void handleSave()} disabled={busy || !node}>
            {busy && <span className="spinner spin-inline" style={{ marginRight: 6 }} />}
            {t('common.save')}
          </button>
        </>
      }
    >
      {!node && (
        <div className="mesh-note">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 8v5M12 16h.01" />
          </svg>
          <span>{t('ts.noNode')}</span>
        </div>
      )}

      <FormFields fields={groups.basic} values={draft} onChange={setField} />
      {connected === false && (
        <div className="card-sub form-inline-note">
          {t('ts.exitEmptyHint')}
        </div>
      )}
      <FormSection
        title={t('node.formGroup.routing')}
        fields={groups.routing}
        values={draft}
        onChange={setField}
      />
      <FormSection
        title={t('node.formGroup.advanced')}
        fields={groups.advanced}
        values={draft}
        onChange={setField}
        collapsible
        forceOpen={revealAdvanced}
      />
    </Modal>
  );
}

export function TsSettingsDialog() {
  // 展示面：本弹窗的提交腿走暂存（:202），编辑基准必须同源，否则第二次编辑从盘上的旧值起算。
  const servers = useEffectiveServers();
  const node = servers.find((s) => s.protocol === 'tailscale');
  return <TsSettingsForm key={node?.id ?? 'none'} node={node} />;
}

export default TsSettingsDialog;
