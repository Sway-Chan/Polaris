/**
 * 切换出口节点 —— **全仓唯一**的切换腿（首页出口选单 / 节点页卡片共用）。
 *
 * 原先只活在 `HomeScreen.onPickNode` 里。节点页补「点卡片切换」时把它提出来，而不是照抄一份：
 * 这条腿里有三处非平凡语义，抄漏任何一处都会造出「一个入口切得对、另一个切得不对」——
 * 本仓在连接按钮上已经吃过一次这种分叉（见 `HomeScreen` 的 `onConnectToggle` 注释）。
 *
 * 三处语义（逐条移自原实现，未改）：
 *  1. **先判后切**：选中「待入池/待生效」节点会让它由「未引用」变「被引用」⇒ 恒立即整核重启 ⇒
 *     待应用差集瞬态清空。切完再读差集恒为空、判不出来，故必须在 `switchServer` 之前取。
 *  2. **差集走 pull 而非 store 快照**：`store.pendingChanges` 由 push 事件喂，可能滞后一拍；
 *     这里要的是「切换前那一瞬」的真值。拉取失败按「不重启」降级 —— 提示是锦上添花，
 *     切换才是用户意图，不能让一次失败的预判挡住主动作。
 *  3. **节点名在 await 前定格**：`switchServer` 成功后会重渲染，闭包里的 `servers` 仍是本次调用的快照，
 *     但取名字这一步若放在 await 之后读的就是旧数组 —— 定格是为了让 toast 报对名字。
 */
import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { useAppStore } from '@/store/app-store';
import { willRestartOnSelect } from '@/components/screens/home/pending-select-hint';

export function useSwitchNode(): (id: string) => Promise<void> {
  const { t } = useTranslation();
  const servers = useAppStore((s) => s.servers);
  const switchServer = useAppStore((s) => s.switchServer);

  return useCallback(
    async (id: string) => {
      const pending = await api.proxy.getPendingChanges().catch(() => null);
      const willRestart = pending ? willRestartOnSelect(pending, id) : false;
      const nodeName = servers.find((s) => s.id === id)?.name ?? '';
      try {
        await switchServer(id);
        // 原型 setNode :4439 每次切换 notify('name · 已切换','ok')。willRestart 分支改发重启提示
        // （它自身已确认切换发生 + 解释待应用条为何消失），二者互斥，避免一次切换弹两条。
        if (willRestart) toast.info(t('home.selectPendingNodeRestartHint'));
        else
          toast.success(
            t('home.switchedToast', { node: nodeName })
          );
      } catch (err) {
        console.error('[switch-node] switch server failed:', err);
        toast.error(t('home.switchError'));
      }
    },
    [switchServer, servers, t]
  );
}
