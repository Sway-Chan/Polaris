/**
 * 订阅手动拉取 + 三态 toast 的单一真值（NodesScreen 刷新按钮 与 SubDialog 新增后自动拉取共用）。
 *
 * 三态由后端 `updateServers` 的真实返回驱动（契约 §16.3.4）：
 *  - 业务失败（`success:false`）→ 报后端真实 `error`（不用「部分节点解析失败」这类笼统文案替代真值）；
 *  - 无变化（`unchanged`，304/内容等价）→ 中性提示（否则计数 0/0/0 会误显「已更新」）；
 *  - 有变化 → 成功报节点变化数。
 *
 * 抽出成 domain 函数是为了让「新增订阅后拉取」与「列表手动刷新」共用同一份三态语义，防两处分叉。
 */
import type { TFunction } from 'i18next';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';

/** 拉取指定订阅并弹三态 toast。返回后端业务是否成功（异常/失败均已 toast，调用方无需再报）。 */
export async function refreshSubscriptionWithToast(subId: string, t: TFunction): Promise<boolean> {
  try {
    const r = await api.subscription.updateServers(subId);
    if (!r.success) {
      toast.error(t('nodes.subRefreshFail'), r.error);
      return false;
    }
    if (r.unchanged) {
      toast.info(t('nodes.subRefreshUnchanged'));
      return true;
    }
    const changed = r.addedServers + r.updatedServers + r.deletedServers;
    toast.success(
      t('nodes.subRefreshOk', { count: changed })
    );
    return true;
  } catch (err) {
    console.error('[subscription-refresh]', err);
    toast.error(
      t('nodes.subRefreshFail'),
      err instanceof Error ? err.message : undefined
    );
    return false;
  }
}
