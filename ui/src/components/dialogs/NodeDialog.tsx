/**
 * NodeDialog —— 节点添加/编辑弹窗（8 协议动态表单，原型 #node-dialog :2513-2547）。
 *
 * 数据驱动：ND_SPEC（node-spec.ts）+ FieldRenderer（field-spec.tsx）+ protoCodec 对称层（proto-codec.ts）。
 *
 * 淬火（polaris-node-form-hardening-requirements.md，逐条结构化）：
 *  - **R1 reset-race 根因整类消失**：无 radix/RHF。外层按 `key={serverId ?? 'new'}` 重挂内层 <NodeForm>——
 *    编辑另一节点 = 重新挂载；表单 state 用 `useState(初始化器)` **同步初始化**（挂载即带正确值），
 *    **代码库中不存在「挂载后 reset()」路径**。Csel 受控无懒挂 Portal，不产生伪 change。
 *  - **R2**：所有 number 走 FieldRenderer 单点分支；port 走同一 `parseNumberField`（空→undefined 非 0）。
 *  - **R3/R4**：fromConfig 边界归一（大小写/别名），见 proto-codec.ts。
 *  - **R5**：toConfig 以 base 起底保全非模型字段；对称性 + 往返单测见 proto-codec.test.ts。
 *
 * props 签名由 stub 冻结：`NodeDialog({ serverId })`（undefined=新增，defined=按 id 取 ServerConfig 预填）。
 * 提交走真后端：新增 `api.server.add` / 编辑 `api.server.update`（src-tauri/src/commands/server.rs）。
 *
 * **配置暂存灰度入口（P3）**：`handleSubmit` 里经 `editRoute('servers', …)` 分流——总开关开且未命中
 * 豁免/绕过时，提交只产生一条 staged 条目（零 IPC 写、零磁盘写）；否则走上面那条今天的直落盘腿。
 * 开关默认关，故当前行为与接入前逐字节相同。判定只此一处，不得在别处再写第二个 if。
 * 脏态取消 → 嵌套 confirm（放弃更改，复用 D1 ConfirmDialog）。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { useAppStore, useEffectiveServers } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute } from '@/lib/staged-config';
import { api } from '@/ipc';
import type { ServerConfig } from '@/contracts/types';
import { Modal } from './Modal';
import { Csel, type CselGroup, type CselOption } from './Csel';
import { useDialogStore } from './dialog-store';
import {
  FieldRenderer,
  parseNumberField,
  draftFromSpecs,
  type FieldSpec,
  type FormValue,
  type FormValues,
} from './field-spec';
import {
  ND_SPEC,
  PROTO_OPTIONS,
  protosInGroup,
  protoGroupsForNodeForm,
  defaultPortPlaceholder,
  allFields,
  describeProbeResult,
  type NodeProto,
  type NodeFieldGroupId,
  type ProbeDisplay,
} from './node-spec';
import { protoCodec } from './proto-codec';
import { blockedByMeshSingleton } from '@/domain/mesh-singleton-guard';
import { Fold } from '@/components/Fold';
import { revealOnToggle } from '@/components/reveal';
import { vpnDraftError } from './vpn-form-layout';

const NODE_PROTOS = new Set<string>(PROTO_OPTIONS.map(([p]) => p));
function isNodeProto(p: string): p is NodeProto {
  return NODE_PROTOS.has(p);
}

function NodeIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <rect x="3" y="4" width="18" height="7" rx="1.5" />
      <rect x="3" y="13" width="18" height="7" rx="1.5" />
      <path d="M7 7.5h.01M7 16.5h.01" />
    </svg>
  );
}

interface NodeFormProps {
  base?: ServerConfig;
  isEdit: boolean;
  servers: ServerConfig[];
  initialProto?: NodeProto;
}

type VpnTab = NodeFieldGroupId;

function NodeForm({ base, isEdit, servers, initialProto }: NodeFormProps) {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);

  // 同步初始化（R1）：挂载即带正确值，绝不挂载后 reset。
  const initProto: NodeProto =
    base && isNodeProto(base.protocol) ? base.protocol : initialProto ?? 'vless';
  const [proto, setProto] = useState<NodeProto>(initProto);
  const [name, setName] = useState(base?.name ?? '');
  const [address, setAddress] = useState(base?.address ?? '');
  const [portStr, setPortStr] = useState(base?.port != null ? String(base.port) : '443');
  const [detour, setDetour] = useState(base?.detour ?? '');
  const [draft, setDraft] = useState<FormValues>(() =>
    base && isNodeProto(base.protocol)
      ? protoCodec[base.protocol].fromConfig(base)
      : draftFromSpecs(allFields(initProto)),
  );

  const [dirty, setDirty] = useState(false);
  const [errName, setErrName] = useState(false);
  const [errAddr, setErrAddr] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [vpnTab, setVpnTab] = useState<VpnTab>('basic');

  // C10：custom 协议内核兼容性 probe（`kernel:probeOutbound`）——套 `SubDialog.previewing`/
  // `previewMsg` 同一形态，非本弹窗首创。`probeResult` 只在协议为 custom 时被读，但状态放这里
  // （而非拆子组件）与本文件其余「协议特定但状态挂在 NodeForm 顶层」的既有写法（如 `detour`）一致。
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<ProbeDisplay | null>(null);

  const setField = (k: string, v: FormValue) => {
    setDraft((d) => ({ ...d, [k]: v }));
    setDirty(true);
    // 编辑过 JSON 后，上一次探测结果已经对不上新文本——清掉比留着一条可能早已过期的「支持/不支持」
    // 更诚实。只在 outbound 字段上生效：其它协议的字段编辑与 probe 结果无关。
    if (k === 'outbound') setProbeResult(null);
  };

  const changeProto = (next: NodeProto) => {
    setProto(next);
    // 换协议：公共字段（名/址/端口/detour）保留在各自 state；协议特定字段重置为新协议默认。
    setDraft(draftFromSpecs(allFields(next)));
    setDirty(true);
    setProbeResult(null); // 换出 custom 协议后旧探测结果同样失效。
    setVpnTab('basic');
  };

  /**
   * 测内核兼容性：JSON 语法先在本地校验（invoke 前置守卫——非法 JSON 传不过 `serde_json::Value`
   * 反序列化，与其让 Tauri 底层抛一个用户看不懂的 IPC 反序列化错误，不如本地先拦、给一句能懂的提示）；
   * 语法过了才真调后端 `sing-box check`。结果统一经 [`describeProbeResult`] 映射成展示态。
   */
  const runCompatProbe = async () => {
    const raw = typeof draft.outbound === 'string' ? draft.outbound : '';
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      setProbeResult({ kind: 'invalidJson' });
      return;
    }
    setProbing(true);
    setProbeResult(null);
    try {
      const r = await api.proxy.probeOutbound(parsed, draft.isEndpoint === true);
      setProbeResult(describeProbeResult(r));
    } catch (e) {
      // IPC 本身失败（核路径解析异常等，非「不兼容」判定）：与其它弹窗的兜底一致，直出错误文案。
      setProbeResult({
        kind: 'unsupported',
        message: e instanceof Error ? e.message : String(e),
        raw: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setProbing(false);
    }
  };

  const visible = (specs: FieldSpec[]): FieldSpec[] =>
    specs.filter((f) => !f.when || f.when(draft));

  // 分组下拉（`Csel` 原生支持 `CselGroup[]`）。分组与组内顺序的判据见 node-spec 的 `PROTO_GROUP_ORDER`。
  const protoLabel = new Map<string, string>(PROTO_OPTIONS.map(([v, l]) => [v, l]));
  const protoGroups: CselGroup[] = protoGroupsForNodeForm(isEdit, initialProto).map((g) => ({
    // 单参 `t`：`node-spec.ts` 2026-08-07 起不留中文缺省（151 条 zh/hintZh 一次删净），组头照同一口径。
    label: t(`node.protoGroup.${g}`),
    options: protosInGroup(g).map((v) => ({ value: v, label: protoLabel.get(v) ?? v })),
  })).filter((g) => g.options.length > 0);

  // 前置代理 detour：direct + 其它节点（排除自身）。
  const detourOpts: CselOption[] = [
    { value: 'direct', label: t('node.detourDirect', '直连（不串联）') },
    ...servers
      .filter((s) => s.id !== base?.id)
      .map((s) => ({ value: s.id, label: s.name })),
  ];

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('node.discardTitle', '放弃更改？'),
        message: t('node.discardMsg', '已填写的内容将不会保存。'),
        confirmLabel: t('node.discard', '放弃'),
        danger: true,
        onConfirm: () => {
          close(); // pop confirm
          close(); // pop 本弹窗
        },
      },
    });
  };

  const handleSubmit = async () => {
    const nameEmpty = !name.trim();
    const port = parseNumberField(portStr);
    const addrEmpty = !address.trim() || port === undefined;
    setErrName(nameEmpty);
    setErrAddr(addrEmpty);
    if (nameEmpty || addrEmpty) return;

    const vpnError = vpnDraftError(proto, draft);
    if (vpnError) {
      setVpnTab(vpnError.tab);
      toast.error(
        vpnError.key === 'json'
          ? t('node.vpnJsonInvalid', '扩展配置必须是合法的 JSON 对象')
          : t('node.vpnRequired', '请补全当前协议的必填认证字段')
      );
      return;
    }

    setSubmitting(true);
    try {
      const meta: ServerConfig = {
        id: base?.id ?? '',
        name: name.trim(),
        protocol: proto,
        address: address.trim(),
        port: port as number,
      };
      if (detour) meta.detour = detour;
      if (base) {
        meta.createdAt = base.createdAt;
        meta.subscriptionId = base.subscriptionId;
        meta.providerName = base.providerName;
      }
      // 同协议编辑 → base 起底保全非模型字段（R5）；改型/新增 → 干净 base 防旧协议残留。
      const codecBase =
        base && base.protocol === proto ? { ...base, ...meta } : meta;
      const full = protoCodec[proto].toConfig(draft, codecBase);

      // 组网单例硬闸门（与 WgDialog / ImportDialog / 克隆同一真值）。
      // **今天在本弹窗恒不命中**：`PROTO_OPTIONS`（node-spec.ts:36）不含 wireguard/tailscale，
      // 故 `full.protocol` 取不到这两个值。留着不是防御性冗余，而是把「凡直调 server:add 者皆过闸」
      // 这条不变量钉在**每一条**腿上——协议表增补一行就会让本腿变成活腿，届时没人会想起来补闸门。
      if (blockedByMeshSingleton(full, servers, t, base?.id)) return; // submitting 由下方 finally 复位

      // 配置暂存灰度入口（P3 只接这一个）。`editRoute` 是**唯一**闸门：总开关关 / W-0 豁免 /
      // W-1·2·3 绕过任一命中都返 'direct'，走下面那条与今天逐字节相同的直落盘腿。
      // 节点是实体边界最清晰的一族：表单提交的就是**整个** ServerConfig，天然满足重放所要求的
      // 「幂等整体替换」，不需要为暂存改造表单。
      if (editRoute('servers', stagingEnabled) === 'staged') {
        // 新增时前端自铸 id：后端 `ensure_server_id` 只在落盘那一刻补 id，而暂存条目现在就需要一个
        // 稳定的实体寻址键（同一节点重复编辑要覆盖同一条）。带 id 提交后端照收（`ensure_server_id`
        // 见 id 非空即放行）。
        const entityId = full.id !== '' ? full.id : crypto.randomUUID();
        stage({
          id: `server:${entityId}`,
          kind: 'server',
          label: `${isEdit ? t('node.editTitle', '编辑节点') : t('node.addTitle', '添加节点')} ${meta.name}`,
          entityPath: ['servers', entityId],
          nextValue: { ...full, id: entityId },
        });
        close();
        return; // 零 IPC 写、零磁盘写（FR-1）
      }

      if (isEdit && base) {
        await api.server.update(full);
      } else {
        const { id: _id, ...rest } = full;
        await api.server.add(rest);
      }
      // 写后端即刷 store（同 WgDialog/TsSettingsDialog/SubDialog/WarpDialog）：store.servers 只由
      // loadConfig/saveConfig 写，不刷则节点网格、规则弹窗「目标出站」下拉、detour 下拉都看不到本次改动。
      void loadConfig(true);
      close();
    } catch (e) {
      toast.error(t('common.saveFailed', '保存失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const adv = ND_SPEC[proto].adv;
  const groups = ND_SPEC[proto].groups;

  const detourField = (
    <div className="fld">
      <label className="fld-l" htmlFor="nd-detour">
        {t('node.chainVia', '经由')}
      </label>
      <Csel
        id="nd-detour"
        ariaLabel={t('node.chainVia', '经由')}
        value={detour || 'direct'}
        onChange={(v) => {
          setDetour(v === 'direct' ? '' : v);
          setDirty(true);
        }}
        options={detourOpts}
      />
      <div className="fld-hint">
        {t('node.chainHint', '先经另一节点再出站（链式代理）')}
      </div>
    </div>
  );

  return (
    <Modal
      titleId="nd-title"
      title={
        isEdit
          ? t('node.editTitle', '编辑节点')
          : initialProto
            ? t('meshJoin.title', '添加接入')
            : t('node.addTitle', '添加节点')
      }
      onClose={requestClose}
      icon={<NodeIcon />}
      className="entry-form-dlg"
      footer={
        <>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void handleSubmit()}
            disabled={submitting}
          >
            {isEdit ? t('common.save', '保存') : t('node.add', '添加')}
          </button>
        </>
      }
    >
      {/* 协议 */}
      <div className="fld">
        <label className="fld-l" htmlFor="nd-proto">
          {t('node.protocol', '协议')}
        </label>
        <Csel
          id="nd-proto"
          ariaLabel={t('node.protocol', '协议')}
          value={proto}
          onChange={(v) => changeProto(v as NodeProto)}
          options={protoGroups}
        />
      </div>

      {/* 备注名 */}
      <div className="fld">
        <label className="fld-l" htmlFor="nd-name">
          <span>{t('node.label', '备注名')}</span> <span className="req-star">*</span>
        </label>
        <input
          id="nd-name"
          className="input"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setDirty(true);
            setErrName(false);
          }}
          placeholder={t('node.labelPh', '香港 · 01')}
        />
        {errName && <div className="err-line">{t('node.errName', '请填写备注名')}</div>}
      </div>

      {/* 地址 / 端口 */}
      <div className="fld">
        <label className="fld-l">
          <span>{t('node.serverPort', '地址 / 端口')}</span> <span className="req-star">*</span>
        </label>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 96px', gap: 10 }}>
          <input
            className="input"
            value={address}
            onChange={(e) => {
              setAddress(e.target.value);
              setDirty(true);
              setErrAddr(false);
            }}
            placeholder="example.com"
            aria-label={t('node.server', '服务器地址')}
          />
          <input
            className="input mono"
            inputMode="numeric"
            value={portStr}
            onChange={(e) => {
              // R2：port 复用 parseNumberField 的空→undefined 语义（此处存原始串以允许退格删空，提交时校验）。
              const raw = e.target.value;
              if (raw === '' || parseNumberField(raw) !== undefined) {
                setPortStr(raw);
                setDirty(true);
                setErrAddr(false);
              }
            }}
            placeholder={defaultPortPlaceholder(proto)}
            aria-label={t('node.port', '端口')}
          />
        </div>
        {errAddr && (
          <div className="err-line">{t('node.errAddr', '请填写服务器地址与端口')}</div>
        )}
      </div>

      {/* 协议凭据（inline） */}
      {visible(ND_SPEC[proto].cred).map((f) => (
        <FieldRenderer key={f.k} spec={f} value={draft[f.k]} onChange={(v) => setField(f.k, v)} />
      ))}

      {/* Endpoint VPN 字段多且语义横跨认证/路由/兼容性，使用页签分层；不把它们继续塞进组网页或
          一个含义不准确的“传输 / 安全”折叠段。公共名称与地址仍固定在上方，切页不会丢上下文。 */}
      {groups && (
        <>
          <div className="sub-tabs form-tabs" role="tablist" aria-label={t('node.formGroup.aria', '配置分组')}>
            {groups.map((group) => (
              <button
                key={group.id}
                type="button"
                role="tab"
                className={vpnTab === group.id ? 'on' : ''}
                aria-selected={vpnTab === group.id}
                onClick={() => setVpnTab(group.id)}
              >
                {t(`node.formGroup.${group.id}`)}
              </button>
            ))}
          </div>
          <div role="tabpanel" className="form-tab-panel">
            {visible(groups.find((group) => group.id === vpnTab)?.fields ?? []).map((f) => (
              <FieldRenderer key={f.k} spec={f} value={draft[f.k]} onChange={(v) => setField(f.k, v)} />
            ))}
            {vpnTab === 'advanced' && detourField}
          </div>
        </>
      )}

      {/* C10：custom 协议内核兼容性 probe——只有 custom 协议的原始 JSON 才需要问「内核认不认识这个
          outbound」，其余 12 协议走本仓自建的 outbound builder，协议合法性由表单本身的必填/枚举
          约束兜底，没有「核认不认识」这一档风险。 */}
      {proto === 'custom' && (
        <div className="fld">
          <button
            type="button"
            className="btn ghost sm"
            onClick={() => void runCompatProbe()}
            disabled={probing || !(typeof draft.outbound === 'string' && draft.outbound.trim())}
          >
            {probing
              ? t('node.customProbe.testing', '检测中…')
              : t('node.customProbe.test', '测试内核兼容性')}
          </button>
          {probeResult?.kind === 'invalidJson' && (
            <div className="err-line">
              {t('node.customProbe.invalidJson', '不是合法 JSON，无法检测')}
            </div>
          )}
          {probeResult?.kind === 'supported' && (
            <div className="fld-hint">{t('node.customProbe.supported', '内核支持该协议')}</div>
          )}
          {probeResult?.kind === 'indeterminate' && (
            <div className="fld-hint">
              {t('node.customProbe.indeterminate', '内核不可用或超时，无法判定兼容性')}
            </div>
          )}
          {probeResult?.kind === 'unsupported' && (
            <>
              <div className="err-line">
                {probeResult.keyPath
                  ? t('node.customProbe.errorWithPath', '键 {{path}}：{{message}}', {
                      // 插值变量名保持 `path`（locale 里是 `{{path}}`）；源字段叫 `keyPath` 的理由见 node-spec.ts。
                      path: probeResult.keyPath,
                      message: probeResult.message,
                    })
                  : t('node.customProbe.error', '校验失败：{{message}}', {
                      message: probeResult.message,
                    })}
              </div>
              {/* 结构化提取失败或用户想看全貌时的兜底——原始诊断原样保留，不因为解析出了 path/message
                  就丢弃（解析器本身也可能挑错行/挑错 keypath，全貌是最后一道校验）。 */}
              <details className="fld-fold" onToggle={revealOnToggle}>
                <summary>{t('node.customProbe.rawOutput', '原始输出')}</summary>
                <pre
                  className="mono"
                  style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all', margin: 0 }}
                >
                  {probeResult.raw}
                </pre>
              </details>
            </>
          )}
        </div>
      )}

      {/* 传输 / 安全（折叠） */}
      {adv.length > 0 && (
        <Fold defaultOpen title={t('node.transportSecurity', '传输 / 安全')}>
          {visible(adv).map((f) => (
          <FieldRenderer key={f.k} spec={f} value={draft[f.k]} onChange={(v) => setField(f.k, v)} />
          ))}
        </Fold>
      )}

      {/* 前置代理 / detour（折叠） */}
      {!groups && <Fold title={t('node.frontProxy', '前置代理 / detour')}>{detourField}</Fold>}
    </Modal>
  );
}

export function NodeDialog({ serverId, initialProto }: { serverId?: string; initialProto?: NodeProto }) {
  // 展示面：编辑基准 + 单例槽判据。读盘的话「改完再打开」看到的是改前的旧值。
  const servers = useEffectiveServers();
  const base = serverId ? servers.find((s) => s.id === serverId) : undefined;
  // R1：key 绑定 serverId —— 切换编辑目标 = 重挂 = 同步重新初始化，杜绝挂载后 reset。
  return (
    <NodeForm
      key={`${serverId ?? 'new'}:${initialProto ?? ''}`}
      base={base}
      isEdit={base != null}
      servers={servers}
      initialProto={initialProto}
    />
  );
}

export default NodeDialog;
