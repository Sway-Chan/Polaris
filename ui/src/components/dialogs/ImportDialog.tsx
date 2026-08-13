/**
 * ImportDialog —— 手动导入弹窗（粘贴文本 / 本地文件，原型 #add-dialog :2488-2510）。
 *
 * 流程（**两步**）：粘贴分享链接 / 订阅 URL / Base64 / Clash → 客户端轻量识别（复用
 * `domain/protocol-url-schemes.ts` 的 `isSupportedShareUrl`，不重写）→ 点「解析」走真后端
 * `local_import_parse`（离线解析，src-tauri/src/commands/subscription.rs）→ **停在结果预览**
 * （计数 + 告警 + 节点清单）→ 点「导入 N 项」才 `server.addBulk` 批量入库。
 *
 * 拆成两步是因为后端一直在返回 `stats` 与 `warnings`，而此前这里把它们**整个丢掉**：
 * 「4 个因缺字段没进来」「6 个协议本内核不认、已透传成 custom」这些后端算得完全正确的事实，
 * 用户一次都看不到，点完只知道「成功了」。
 *
 * **文件选择两条路径**：
 *  - 点击拖放区 → `local_import_pick_file`（tauri-plugin-dialog 原生选择器 + 读内容回传，取消静默，
 *    超限/读失败提示）；拖拽 → 客户端 `FileReader`（webview 放行时可用）。两者殊途同归到 `content`。
 *  - `local_import_parse` 0 节点 / 不可识别 → 后端 throw → 前端 catch 显错，弹窗保持打开。
 *
 * URL 识别为订阅（checklist R7-5/6）：单行裸订阅 URL → 非阻断提示引导「添加订阅」（`sub` 弹窗为 D3，
 * union kind 已存在，open 即可，落地后渲染）。
 *
 * props 签名由 stub 冻结：`ImportDialog()`（无 props，恒新增态）。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import type { ImportParseResult } from '@/contracts/types';
import { isSupportedShareUrl } from '@/domain/protocol-url-schemes';
import { admitMeshSingletons } from '@/domain/endpoint-routes';
import { useAppStore, useEffectiveServers } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute } from '@/lib/staged-config';
import { toast } from '@/lib/error-handler';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

type Source = 'share' | 'file';

/** 客户端轻量识别（预览用；权威解析在 Rust local_import_parse）。 */
interface Detection {
  kind: 'links' | 'base64' | 'clash' | 'subscription' | 'unknown' | 'empty';
  linkCount: number;
}

const BASE64_RE = /^[A-Za-z0-9+/=\s]+$/;

function detect(text: string): Detection {
  const trimmed = text.trim();
  if (!trimmed) return { kind: 'empty', linkCount: 0 };

  const lines = trimmed.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const links = lines.filter((l) => isSupportedShareUrl(l));
  if (links.length > 0) return { kind: 'links', linkCount: links.length };

  // 单行裸订阅 URL（http(s)、无内联凭据 @、无 #name 片段）→ 疑似订阅
  if (
    lines.length === 1 &&
    /^https?:\/\/\S+$/i.test(trimmed) &&
    !trimmed.includes('@') &&
    !trimmed.includes('#')
  ) {
    return { kind: 'subscription', linkCount: 0 };
  }

  if (/(^|\n)\s*proxies\s*:/.test(trimmed)) return { kind: 'clash', linkCount: 0 };
  if (!trimmed.includes('\n') && BASE64_RE.test(trimmed) && trimmed.length >= 24) {
    return { kind: 'base64', linkCount: 0 };
  }
  return { kind: 'unknown', linkCount: 0 };
}

export function ImportDialog() {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  // 展示面：单例槽判据必须含暂存节点 —— 否则暂存了一个 WARP 还能再导入第二个，重放后配置非法。
  const servers = useEffectiveServers();
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  const loadConfig = useAppStore((s) => s.loadConfig);

  const [source, setSource] = useState<Source>('share');
  const [content, setContent] = useState('');
  const [dragOver, setDragOver] = useState(false);
  const [fileHint, setFileHint] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [clip, setClip] = useState<string | null>(null);
  /** 解析结果预览。`null` = 还没解析（或内容已改动，旧结果作废）。 */
  const [preview, setPreview] = useState<ImportParseResult | null>(null);

  const det = detect(content);

  // 剪贴板 best-effort 探测（原型 clip-hint :2494）：有分享链接则提示一键填入。失败静默。
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const txt = await navigator.clipboard.readText();
        if (alive && txt && isSupportedShareUrl(txt.trim())) setClip(txt.trim());
      } catch {
        /* 剪贴板不可读（权限/焦点）→ 忽略，非阻断 */
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const onContentChange = (v: string) => {
    setContent(v);
    // 内容一改，上一次的解析结果就不再描述它 —— 留着会让用户对着旧预览按「确认导入」。
    setPreview(null);
  };

  const readFiles = (files: FileList | null) => {
    const file = files?.[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      const text = typeof reader.result === 'string' ? reader.result : '';
      setContent(text);
      setSource('share');
      setFileHint(null);
    };
    reader.onerror = () => setFileHint(t('import.fileReadFail', '文件读取失败'));
    reader.readAsText(file);
  };

  const pickFile = async () => {
    setFileHint(null);
    try {
      const r = await api.localImport.pickFile();
      if (r.canceled) return; // 用户取消 → 静默（对齐 上游）
      if (r.error === 'too_large') {
        setFileHint(t('import.fileTooLarge', '文件过大（最大 10 MB）'));
        return;
      }
      if (r.error || r.content == null) {
        setFileHint(t('import.fileReadFail', '文件读取失败'));
        return;
      }
      setContent(r.content);
      setSource('share');
    } catch (e) {
      setFileHint(e instanceof Error ? e.message : String(e));
    }
  };

  const requestClose = () => {
    if (content.trim()) {
      open({
        kind: 'confirm',
        payload: {
          title: t('import.discardTitle', '放弃导入？'),
          message: t('import.discardMsg', '已粘贴的内容将不会保存。'),
          confirmLabel: t('node.discard', '放弃'),
          danger: true,
          onConfirm: () => {
            close();
            close();
          },
        },
      });
    } else {
      close();
    }
  };

  /**
   * 第一步：解析并**停在预览**，不入库。
   *
   * 为什么要拆成两步：后端一直在返回 `stats`（imported / unsupported / skipped / failed）与
   * `warnings`，而此前这个弹窗把它们**整个丢掉**，只在「撞上单例槽」时报一句。于是
   * 「37 个节点里 4 个因缺字段被丢了」「6 个协议本内核不认、已透传成 custom」这些**后端算得
   * 完全正确**的事实，用户一次都看不到 —— 点完导入只知道「成功了」。
   *
   * 文案不新造：预览阶段这批键（`parse` / `parsed` / `nodesTitle` / `unsupportedBadge` /
   * `unsupportedTip` / `skippedTitle` / `submitN {{count}}` / `resultUnsupported`）五语齐备、
   * **本来就在设计里**，只是移植时整套躺进了一个零消费点的 `localImport.*` 命名空间。已并入 `import.*`。
   */
  const handleParse = async () => {
    const text = content.trim();
    if (!text) {
      toast.error(t('import.errEmpty', '请粘贴分享链接 / Base64 / 配置内容'));
      return;
    }
    setSubmitting(true);
    try {
      const result = await api.localImport.parse(text); // 0 节点 / 不可识别 → throw
      if (!result.nodes.length) {
        toast.error(t('import.errNoNodes', '未识别到任何可用节点'));
        return;
      }
      setPreview(result);
    } catch (e) {
      toast.error(t('import.failed', '导入失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  /** 第二步：把预览里那批节点真正入库。 */
  const handleImport = async () => {
    const result = preview;
    if (!result) return;
    setSubmitting(true);
    try {
      // 组网单例硬闸门·**逐条**（`admitMeshSingletons` 让准入者即刻占槽，同一批里的第二个 WARP
      // 也拦得住）。整批拒绝是错的：一条撞槽的节点不该连累同批其余正常节点，故过滤 + 如实报跳过数。
      // 今天 net-stack 的三个解析器（clash/singbox/xray）都跳过 wireguard，本闸门恒不命中；但
      // `detect_format` 已为 sing-box `endpoints[]`（wireguard/tailscale）留了分支，那条一旦接通，
      // 外部内容就能直接灌进 `server:addBulk`——闸门必须先于它就位。
      const { admitted, rejected } = admitMeshSingletons(result.nodes, servers);
      if (!admitted.length) {
        toast.error(
          t('nodes.importSingletonAllSkipped', '识别到的节点都撞上了已占用的 WARP / Tailscale 单例槽，未导入任何节点')
        );
        return;
      }
      // 配置暂存闸门（与 NodeDialog 同形）。解析已经在 `localImport.parse` 里做完，`addBulk` 本身
      // 是**纯 `servers` 写**、无副作用 ⇒ 走默认腿。批量导入 ⇒ 逐节点一条条目：整批一条的话
      // 「逐条撤销」就退化成「要么全留要么全撤」，而这正是导入最需要挑挑拣拣的场景。
      const staged = editRoute('servers', stagingEnabled) === 'staged';
      if (staged) {
        for (const node of admitted) {
          // 同 NodeDialog：后端只在落盘那一刻补 id，而条目现在就需要稳定的实体寻址键。
          const entityId = node.id ? node.id : crypto.randomUUID();
          stage({
            id: `server:${entityId}`,
            kind: 'server',
            label: `${t('import.title', '导入节点')} ${node.name}`,
            entityPath: ['servers', entityId],
            nextValue: { ...node, id: entityId },
          });
        }
      } else {
        await api.server.addBulk(admitted);
        // 写后端即刷 store（同 NodeDialog/WgDialog/SubDialog/TsSettingsDialog）：`store.servers`
        // 只由 loadConfig/saveConfig 写。后端 `server_add_bulk` 确实发了 `broadcast_config_changed`、
        // `App.tsx:433` 也订阅着它兜底，但那是事件往返的**慢路径** —— 弹窗已经关掉、回执也已经
        // 弹出，节点网格却还空着。快路径与其余四个写节点的弹窗对齐，不在这里单开一种时序。
        void loadConfig(true);
      }
      // 回执。此前**只有**撞单例槽那一条腿会说话，而那条腿今天恒不命中（三个解析器都跳过
      // wireguard，见上方闸门注释）⇒ 实际形态是「导入完全静默」：弹窗一关，用户手上唯一的
      // 计数是刚刚消失的那颗按钮。
      // staged 与直落盘必须分开说：前者一个字节都没落盘，说「已导入」会让用户跳过条上的「保存」。
      // `rejected>0 && staged` 这一格今天不可达（闸门恒不命中），不为它再铺一对键；`endpoints[]`
      // 那条解析腿一旦接通，这里要连同上面的闸门一起改。
      if (rejected.length > 0) {
        toast.info(
          t('nodes.importSingletonSkipped', {
            defaultValue: '已导入 {{count}} 个；{{skipped}} 个因 WARP / Tailscale 单例槽已占用被跳过',
            count: admitted.length,
            skipped: rejected.length,
          })
        );
      } else {
        toast.success(
          staged
            ? t('nodes.importStagedOk', {
                defaultValue: '{{count}} 个节点已加入待保存',
                count: admitted.length,
              })
            : t('nodes.importOk', { defaultValue: '已导入 {{count}} 个节点', count: admitted.length })
        );
      }
      close();
    } catch (e) {
      toast.error(t('import.failed', '导入失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const previewText = (): string => {
    switch (det.kind) {
      case 'links':
        return t('import.detLinks', { defaultValue: '识别到 {{n}} 条分享链接', n: det.linkCount });
      case 'base64':
        return t('import.detBase64', '识别到 Base64 内容（提交后端解码）');
      case 'clash':
        return t('import.detClash', '识别到 Clash 配置');
      case 'subscription':
        return t('import.detSub', '这看起来是订阅链接');
      case 'unknown':
        return t('import.detUnknown', '格式待后端解析');
      default:
        return '';
    }
  };

  return (
    <Modal
      titleId="add-dlg-title"
      title={t('import.title', '导入节点')}
      onClose={requestClose}
      className="entry-form-dlg"
      icon={
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 15V4M8 8l4-4 4 4M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3" />
        </svg>
      }
      footer={
        <>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void (preview ? handleImport() : handleParse())}
            disabled={submitting}
          >
            {submitting && preview
              ? t('import.importing', '导入中…')
              : preview
                ? t('import.submitN', {
                    defaultValue: '导入 {{count}} 项',
                    count: preview.nodes.length,
                  })
                : t('import.parse', '解析')}
          </button>
        </>
      }
    >
      {clip && (
        <div className="clip-hint show">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <rect x="9" y="9" width="11" height="11" rx="2" />
            <path d="M5 15V5a2 2 0 012-2h10" />
          </svg>
          <span>{t('import.clipDetected', '检测到剪贴板里有分享链接')}</span>
          <button
            type="button"
            className="btn flow sm"
            onClick={() => {
              setContent(clip);
              setSource('share');
              setClip(null);
            }}
          >
            {t('import.submit', '导入')}
          </button>
        </div>
      )}

      {/* 来源切换 */}
      <div className="fld">
        <label className="fld-l">{t('import.source', '来源')}</label>
        <div className="seg2" style={{ display: 'flex' }}>
          <button
            type="button"
            style={{ flex: 1 }}
            className={source === 'share' ? 'on' : ''}
            onClick={() => setSource('share')}
          >
            {t('import.pasteText', '粘贴文本')}
          </button>
          <button
            type="button"
            style={{ flex: 1 }}
            className={source === 'file' ? 'on' : ''}
            onClick={() => setSource('file')}
          >
            {t('import.localFile', '本地文件')}
          </button>
        </div>
      </div>

      {source === 'share' ? (
        <>
          <div className="fld">
            <label className="fld-l" htmlFor="share-input">
              {t('import.pasteLabel', '粘贴分享链接 / 订阅 URL / Base64')}
            </label>
            <textarea
              id="share-input"
              className="input mono"
              rows={5}
              value={content}
              onChange={(e) => onContentChange(e.target.value)}
              placeholder={t('import.textPlaceholder')}
            />
          </div>

          {det.kind !== 'empty' && (
            <div className="parse-list">
              <div className={`pl-row ${det.kind === 'unknown' ? 'parse-bad' : 'parse-ok'}`}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M5 12l5 5 9-11" />
                </svg>
                <span>{previewText()}</span>
              </div>
            </div>
          )}

          {det.kind === 'subscription' && (
            <div className="card-sub">
              {t('import.subHint', '订阅链接建议用「添加订阅」以支持自动更新。')}{' '}
              <button
                type="button"
                className="btn ghost sm"
                onClick={() => open({ kind: 'sub' })}
              >
                {t('import.gotoSub', '添加订阅')}
              </button>
            </div>
          )}

          <div className="card-sub">
            {t('import.landHint', '导入的节点归入「自建节点」。若需可自动更新的列表，请用「添加订阅」。')}
          </div>
        </>
      ) : (
        <>
          <div
            className={`dz${dragOver ? ' drag' : ''}`}
            role="button"
            tabIndex={0}
            onClick={() => void pickFile()}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                void pickFile();
              }
            }}
            onDragOver={(e) => {
              e.preventDefault();
              setDragOver(true);
            }}
            onDragLeave={() => setDragOver(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragOver(false);
              readFiles(e.dataTransfer.files);
            }}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 15V4M8 8l4-4 4 4" />
              <path d="M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3" />
            </svg>
            <div>{t('import.dropHere', '拖拽配置文件到此，或点击选择')}</div>
            <div style={{ fontSize: 10.5, marginTop: 4 }}>.conf · .yaml · .json · .txt</div>
          </div>
          {fileHint && <div className="card-sub">{fileHint}</div>}
        </>
      )}

      {/* 解析结果预览。
       *
       * 文案全部取自**既有译文**（`parse` / `nodesTitle` / `unsupportedBadge` / `unsupportedTip` /
       * `skippedTitle` / `submitN {{count}}` / `resultUnsupported`，五语齐备）：这个预览阶段本来
       * 就在设计里，只是 Polaris 移植时没落地，整套躺在零消费点的 `localImport.*` 里。
       * 不另起一套新键：那等于把同一件事的文案写第二份，且要重新铺 5 个语种。 */}
      {preview && (
        <div className="imp-preview">
          <div className="imp-stats">
            <span className="imp-stat ok">
              {t('import.parsed', {
                defaultValue: '已解析 {{nodes}} 个节点、{{subs}} 个订阅',
                nodes: preview.nodes.length,
                subs: preview.subscriptions.length,
              })}
            </span>
            {/* 三个「不是全都顺利」的计数**只在非零时出现** —— 恒显示会让「一切正常」和
                「丢了 4 个」在视觉上长得一样，用户扫一眼分不出来。 */}
            {preview.stats.unsupported > 0 && (
              <span className="imp-stat warn" data-tip={t('import.unsupportedTip', '')}>
                {t('import.resultUnsupported', {
                  defaultValue: '{{count}} 个节点的协议当前内核不支持，导入后置灰',
                  count: preview.stats.unsupported,
                })}
              </span>
            )}
            {preview.stats.skipped + preview.stats.failed > 0 && (
              <span className="imp-stat bad">
                {t('import.skippedTitle', {
                  defaultValue: '跳过 {{count}} 项（协议不支持或解析失败）',
                  // `skipped`（协议不支持）与 `failed`（缺字段/解析异常）在用户眼里是同一件事
                  //「这条没进来」；既有文案本就把两者合并表述，故合并计数而不是另造一个键。
                  count: preview.stats.skipped + preview.stats.failed,
                })}
              </span>
            )}
          </div>

          {preview.warnings.length > 0 && (
            <ul className="imp-warn">
              {preview.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}

          <div className="card-sub" style={{ marginBottom: 5 }}>
            {t('import.nodesTitle', '待导入节点')}
          </div>
          <ul className="imp-list">
            {preview.nodes.map((n, i) => (
              <li key={n.id || `${n.name}-${i}`}>
                <span className="imp-name" data-tip={n.name}>
                  {n.name}
                </span>
                {/* `unsupported` 的后端判据就是 `protocol === 'custom'`
                    （commands/subscription.rs 按此派生计数），故此处同判据、不另立一份。 */}
                {n.protocol === 'custom' ? (
                  <span className="imp-badge" data-tip={t('import.unsupportedTip', '')}>
                    {t('import.unsupportedBadge', '不支持')}
                  </span>
                ) : (
                  <span className="imp-proto">{n.protocol}</span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </Modal>
  );
}

export default ImportDialog;
