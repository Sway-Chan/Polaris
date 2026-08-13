import type { Rule } from '@/contracts/types';

/**
 * 规则「复制」的载荷构造（G5，原型 `enhanceRuleRow` :4771 / :4096「造一条同条件新规则」）。
 *
 * 抽成纯函数是因为它有两条会静默出错的不变式，而调用点（`RulesScreen.handleDuplicate`）在 node
 * 环境下测不了：
 *  - **`id` 必须消失**：带着原 id 走 `rules.add` 会撞进后端的「已存在」腿（或更糟：覆盖原规则）；
 *  - **`remarks` 必须可分辨**：不加后缀就是两行完全同形的规则，用户分不出哪条是新的、
 *    也就无从下手改它 —— 而复制的意义正是「以它为底改出一个变体」。
 */
export function duplicateRulePayload(rule: Rule, suffix: string): Omit<Rule, 'id'> {
  // 解构剔除而非 `delete`：前者在类型上保证「将来给 Rule 加字段会自动跟着复制」，
  // 后者只会把新字段静默留在原对象里、复制出一条缺字段的规则。
  const { id: _id, ...rest } = rule;
  return {
    ...rest,
    remarks: rest.remarks ? `${rest.remarks} (${suffix})` : suffix,
  };
}
