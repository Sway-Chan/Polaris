/**
 * `HostRuleMenuItems` —— 「给这个主机名建规则」的两个右键菜单项 + 追加写入的**唯一**执行腿。
 *
 * # 为什么是一个共用组件，而不是两处各写一遍
 *
 * 这两项今天有两个语境：**连接拓扑图**右键一个域名节点（`ConnectionTopology`）与**连接页**右键一行
 * （`ConnectionsScreen`）。两个入口、一条腿 —— 追加写入的暂存分流（`editRoute('customRules', …)`
 * → 暂存条目 / 直落盘）与「已有规则先命中时把『加入已有』排到前面」这条排序判据都是**同一件事**。
 * 复制一份的代价不是重复几行，而是两份会漂，而漂出来的症状（「拓扑那边排序对、连接页那边不对」
 * 或「一边进暂存一边直接写盘」）没有任何门会抓、真机也极难复现。同 `lib/use-rule-delete.ts` 的先例。
 *
 * ⇒ 追加腿的 `api.rules.update` 全仓**只此一个**调用点（`entity-action-wiring.test.ts` /
 * `config-write-wiring.test.ts` 两张登记表钉住这件事）。
 *
 * # 排序按事实自适应
 *
 * 若该域名已被某条**启用**规则命中，「加入已有规则…」排到「加入自定义规则」之上，并把命中的那条
 * 显示在菜单里。判据 = `commands/rules.rs` 的 `rules_add` 恒 `arr.push`（新规则落列表末尾）+ 路由
 * 语义「先匹配先生效」：那种情况下**新建的那条压根不生效**，默认动作不该指向一条无效路径。
 *
 * ⚠️ 但这只是**提示不是门**：`analyzeDomainCoverage` 用的是客户端启发式匹配（权威匹配器在 sing-box
 * 内核），它会误报也会漏报。所以它只改**排序**与一行说明，绝不阻断「新建」——
 * `dialogs/rule-append.test.ts` 第 ③ 组有一道门钉住「本文件去注释后不得出现任何 `disabled`」，
 * 防止日后有人把它升格成闸门（该变异已实跑，转红）。
 */

import { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { editRoute } from '@/lib/staged-config';
import { useAppStore, useEffectiveRules } from '@/store/app-store';
import { useNavStore } from '@/store/nav-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { ruleTypeNameKey } from '@/domain/rules';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import {
  analyzeDomainCoverage,
  appendValueToRule,
  type RuleAppendTarget,
} from '@/components/dialogs/rule-append';

function PlusIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M12 5v14M5 12h14" />
    </svg>
  );
}
function MergeIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M6 3v6a4 4 0 004 4h8M14 9l4 4-4 4" />
    </svg>
  );
}

export function HostRuleMenuItems({ host, onDone }: { host: string; onDone: () => void }) {
  const { t } = useTranslation();
  const navigate = useNavStore((s) => s.navigate);
  const openDialog = useDialogStore((s) => s.open);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  /* 展示面取 effective：暂存中新建/编辑过的规则也参与「谁先命中」的判断与追加目标枚举，
     否则用户刚加的那条在这里看不见，菜单会给出与列表相矛盾的结论。 */
  const rules = useEffectiveRules();

  const coverage = useMemo(() => analyzeDomainCoverage(rules, host), [rules, host]);
  const covering = useMemo(
    () => (coverage.firstId ? (rules.find((r) => r.id === coverage.firstId) ?? null) : null),
    [rules, coverage.firstId]
  );
  const coveringName = covering
    ? covering.remarks?.trim() || t(ruleTypeNameKey(covering.type))
    : '';

  /** 追加到已有规则 —— 全仓唯一的 `api.rules.update` 追加调用点。 */
  const append = useCallback(
    async (target: RuleAppendTarget) => {
      const base = rules.find((r) => r.id === target.ruleId) ?? null;
      const next = base ? appendValueToRule(base, target, host) : null;
      if (!next) {
        // `null` 有两种由来：**已包含**（成功的无事可做）与**目标漂移**（选中后规则被别处改了）。
        // 前者不该报错吓人，后者不该假装成功。
        if (target.block === 'contains') toast.success(t('home.domainAlreadyInRule', { domain: host }));
        else toast.error(t('rules.appendFail'));
        return;
      }
      const label = next.remarks?.trim() || t(ruleTypeNameKey(next.type));
      try {
        // 配置暂存闸门（与 NodeDialog 同形）：`customRules` Class B，写的是**整条** Rule
        // ⇒ 天然满足重放要求的「幂等整体替换」。
        if (editRoute('customRules', stagingEnabled) === 'staged') {
          stage({
            id: `rule:${next.id}`,
            kind: 'rule',
            label: `${t('rules.editTitle')} ${label}`,
            entityPath: ['customRules', next.id],
            nextValue: next,
          });
        } else {
          await api.rules.update(next);
          void loadConfig(true); // 不刷则规则列表看不到追加结果
        }
        toast.success(
          t('rules.appendDone', {
            domain: host,
            rule: label,
          })
        );
      } catch {
        toast.error(t('rules.appendFail'));
      }
    },
    [rules, host, t, stagingEnabled, stage, loadConfig]
  );

  const pickExisting = (
    <button
      key="pick"
      type="button"
      className="ctx-i"
      onClick={() => {
        onDone();
        openDialog({ kind: 'rule-pick', domain: host, onPick: (tg) => void append(tg) });
      }}
      data-tip={covering ? t('home.domainAlreadyInRule', { domain: host }) : undefined}
    >
      <MergeIcon />
      {t('home.addToExistingRule')}
      {covering && <span className="ctx-note">{coveringName}</span>}
    </button>
  );

  const createNew = (
    <button
      key="new"
      type="button"
      className="ctx-i"
      onClick={() => {
        // 打开完整规则弹窗并预填首条件（原型 dnodeMenu :3802 route + openRuleDialog）。
        onDone();
        navigate('rules');
        openDialog({ kind: 'rule', presetDomain: host });
      }}
    >
      <PlusIcon />
      {t('home.addToRule')} <b>{host}</b>
    </button>
  );

  return <>{covering ? [pickExisting, createNew] : [createNew, pickExisting]}</>;
}

export default HostRuleMenuItems;
