/**
 * `useRuleDelete` —— 删一条自定义规则的**唯一**执行腿。
 *
 * # 为什么是一个共用 hook，而不是两处各写一遍
 *
 * 删规则今天有两个语境：**列表里删一条**（`RulesScreen` 的行内垃圾桶，原型 :4097 `rule-del`）与
 * **编辑到一半决定不要了**（`RuleDialog` footer 左侧，原型 :4095 `rule-del-dlg`）。两个入口、
 * 一条腿：暂存分流（`splitStagedOnly('rule.delete', …)` → 撤销条目 / 暂存一条 `nextValue: null` /
 * 直落盘）是**同一件事**，复制一份的代价不是重复几行，而是两份会漂 —— 而漂出来的症状是
 * 「暂存态下从列表删会写盘、从弹窗删不会」这种没有任何门会抓、真机也极难复现的不一致。
 *
 * ⇒ `api.rules.delete` 全仓**只此一个**调用点（`entity-action-wiring.test.ts` 的登记表钉住这件事：
 * 该行的 `file` 就指向本文件）。
 *
 * # 为什么 staged-only 差集在本 hook 内部算，而不是由调用方传进来
 *
 * 差集的两个入参（展示面 `useEffectiveRules()` + 操作面磁盘镜像 `s.rules`）是这条腿**自己的**判据，
 * 不是调用方的关切。更要紧的是接线守卫的判据面 = 「文件里出现 `useEffectiveRules`」：把差集当参数
 * 收进来，本文件就落到判据面**之外**，那个 `api.rules.delete` 调用点会从 `entity-action-wiring`
 * 的对账里整个消失 —— 抽函数就成了绕开计数的手段。自持读点让它留在灯下。
 *
 * # 错误处理不收进来
 *
 * 抛出去，由调用点自己接：弹窗要把消息落在 `.err-line` 上（不关窗、让用户改），列表要 toast。
 * 收进来只会变成又一层要绕开的抽象（同 `confirm-twice.ts` 头注的那条理由）。
 * 成功后的收尾动作同理留给调用点（弹窗要 `close()`，列表什么都不用做 —— 行自己会消失）。
 */

import { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import type { Rule } from '@/contracts/types';
import { api } from '@/ipc';
import { useAppStore, useEffectiveRules } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { editRoute, splitStagedOnly, stagedOnlyIds } from '@/lib/staged-config';

/** 删一条规则；失败抛出（调用点自己决定怎么报）。 */
export type DeleteRuleFn = (rule: Rule) => Promise<void>;

export function useRuleDelete(): DeleteRuleFn {
  const { t } = useTranslation();
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  /** 撤销腿的入参（`ENTITY_ACTION_TABLE` 的 `revert` 策略）。总开关关着时两者恒空 ⇒ 走直落盘那条路径。 */
  const stagedEntries = useStagedConfigStore((s) => s.entries);
  const revertStaged = useStagedConfigStore((s) => s.revert);
  /** staged-only 差集的两个入参（展示面 effective + 操作面磁盘镜像），判据与节点卡同一函数。 */
  const effectiveRules = useEffectiveRules();
  const diskRules = useAppStore((s) => s.rules);
  const stagedOnlyRuleIds = useMemo(
    () => stagedOnlyIds(effectiveRules, diskRules),
    [effectiveRules, diskRules]
  );

  return useCallback(
    async (rule: Rule) => {
      // 配置暂存闸门（与 NodeDialog 同形）。`customRules` 是 UserConfig 字段（Class B）且删规则
      // 无任何不可逆副作用（不同于删节点 —— 那会触发 WARP 远端注销 / 清 TS state）⇒ 走默认腿。
      // 集合实体的「删除」= `nextValue: null`（见 `staged-config.ts` 的 `upsertById`）。
      // `revert`（ENTITY_ACTION_TABLE）先判：删一条**还没保存的新规则** = 撤销那条条目本身。
      // 不走下面那条「暂存一条 nextValue: null 的删除条目」——盘上根本没有它，重放是空操作，
      // 却会在条上多留一条「删除规则 X」，用户看到一条指向不存在实体的待保存项。
      const split = splitStagedOnly(
        'rule.delete',
        [rule.id],
        stagedOnlyRuleIds,
        stagedEntries,
        'customRules'
      );
      for (const entryId of split.revertEntryIds) revertStaged(entryId);
      if (split.backend.length === 0) return;
      if (editRoute('customRules', stagingEnabled) === 'staged') {
        stage({
          id: `rule:${rule.id}`,
          kind: 'rule',
          label: `${t('rules.deleteRule')} ${rule.remarks || rule.type}`,
          entityPath: ['customRules', rule.id],
          nextValue: null,
        });
        return; // 零 IPC 写、零磁盘写（FR-1）
      }
      await api.rules.delete(rule.id);
      // 写后端即刷 store，否则列表里被删的那条还在（store.rules 只由 loadConfig/saveConfig 写）。
      void loadConfig(true);
    },
    [t, loadConfig, stagingEnabled, stage, stagedEntries, revertStaged, stagedOnlyRuleIds]
  );
}
