/**
 * DialogHost —— 弹窗唯一挂载点（挂 AppShell 内，§1.3）。
 *
 * 按 store.stack 顺序渲染每个弹窗组件（各自包 <Modal> 原语）。原生 dialog 按 showModal 顺序叠 top-layer，
 * store 栈镜像之 —— 数组顺序 = 叠放次序（末尾最顶层）。用 index 作 key：栈只做 push/pop，
 * 保留项 index 稳定 → 不触发无谓重挂。
 *
 * **本文件在 Phase A 冻结**：所有 kind 的 case + import 一次到位，各批（D2–D5）只覆写自己那个
 * *Dialog 组件文件（单一 owner），不改本文件 → 并行零写冲突。switch 的 default 走 never 兜底：
 * 新增 DialogDesc.kind 未补 case → 编译期报错（穷尽性）。组件与 DialogDesc 解耦：payload 经此处
 * narrow 后以 props 下传，故各组件定义自己的 props 接口、不 import DialogDesc（避免循环耦合）。
 */

import { Fragment } from 'react';
import { useDialogStore, type DialogDesc } from './dialog-store';
import { ResUrlDialog } from './ResUrlDialog';
import { ConfirmDialog } from './ConfirmDialog';
import { NodeDialog } from './NodeDialog';
import { ImportDialog } from './ImportDialog';
import { SubDialog } from './SubDialog';
import { WarpDialog } from './WarpDialog';
import { TsLoginDialog } from './TsLoginDialog';
import { TsSettingsDialog } from './TsSettingsDialog';
import { WgDialog } from './WgDialog';
import { RuleDialog } from './RuleDialog';
import { ProcPickDialog } from './ProcPickDialog';
import { RulePickDialog } from './RulePickDialog';
import { AppAddDialog } from './AppAddDialog';
import { ResCatalogDialog } from './ResCatalogDialog';
import { BackupImportDialog } from './BackupImportDialog';
import { MeshJoinDialog } from './MeshJoinDialog';

function renderDialog(desc: DialogDesc) {
  switch (desc.kind) {
    case 'res-url':
      return <ResUrlDialog />;
    case 'confirm':
      return <ConfirmDialog payload={desc.payload} />;
    case 'node':
      return <NodeDialog serverId={desc.serverId} initialProto={desc.initialProto} />;
    case 'import':
      return <ImportDialog />;
    case 'sub':
      return <SubDialog subId={desc.subId} focus={desc.focus} onAdded={desc.onAdded} />;
    case 'warp':
      return <WarpDialog edit={desc.edit} />;
    case 'ts-login':
      return <TsLoginDialog />;
    case 'ts-settings':
      return <TsSettingsDialog />;
    case 'wg':
      return <WgDialog serverId={desc.serverId} />;
    case 'mesh-join':
      return (
        <MeshJoinDialog
          onTsLogout={desc.onTsLogout}
          onWarpReregister={desc.onWarpReregister}
          onWarpDeregister={desc.onWarpDeregister}
        />
      );
    case 'rule':
      return <RuleDialog ruleId={desc.ruleId} preset={desc.preset} />;
    case 'proc-pick':
      return <ProcPickDialog onPick={desc.onPick} />;
    case 'rule-pick':
      return <RulePickDialog subject={desc.subject} onPick={desc.onPick} />;
    case 'app-add':
      return <AppAddDialog />;
    case 'res-catalog':
      return <ResCatalogDialog />;
    case 'backup-import':
      return <BackupImportDialog />;
    default: {
      // 穷尽性守卫：新增 kind 未补 case → 编译期报错。
      const _exhaustive: never = desc;
      return _exhaustive;
    }
  }
}

export function DialogHost() {
  const stack = useDialogStore((s) => s.stack);
  return (
    <>
      {stack.map((desc, i) => (
        <Fragment key={i}>{renderDialog(desc)}</Fragment>
      ))}
    </>
  );
}

export default DialogHost;
