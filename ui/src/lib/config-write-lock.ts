/**
 * 配置写串行化闸门 —— **同一 webview 内**对 `config.json` 的读改写必须一个接一个。
 *
 * # 为什么需要它（真缺陷，非防御性编程）
 *
 * `staged-config-store.performSave` 是一段 read-modify-write：`api.config.get()` 取盘现值算出
 * `diskVersion` → 重放 staged → `api.config.save(merged, true, diskVersion)`。两个 `await` 之间
 * **没有互斥**，于是两次交错的保存会各自读到**同一个** `diskVersion`：
 *
 *  1. A 读盘（v1）→ B 读盘（v1）→ A 写盘成功（盘变 v2）→ B 带着 v1 提交；
 *  2. 后端 `config_save_core` 的乐观并发闸判 `v1 != v2` ⇒ B 拿 `conflict`、**一个字节都不写**；
 *  3. 前端把 `conflict` 落成 `saveStatus:'saveFailed'`。
 *
 * 净效果是最坏的一种：**盘已经存好了，条却说「保存失败」，而 staged 已被 A 清空 ⇒ 没有任何东西
 * 可重试**。用户会去重试一个并不存在的失败。
 *
 * `saveStatus:'saving'` 挡不住这件事 —— 它**从来没有被当作闸门读过**。唯一像门的是
 * `composeBarView` 把三颗按钮置 `disabled`，那是**渲染层**的防误点，覆盖不到
 * `resolveConflict` / `applyNow` / 任何直接 `useStagedConfigStore.getState().save()`，
 * 也依赖 React 重渲在两次点击之间落地。
 *
 * # 为什么是「串行」而不是「拒绝并发」
 *
 * 丢弃第二次保存 = 静默吞掉用户的编辑（NFR-1 明令禁止）。串行则让第二次在第一次**落定之后**
 * 才进临界区，于是它自己那次 `api.config.get()` 读到的就是**已经更新的**盘 —— 「链式：每次用
 * 上一次的版本」由此自动成立，不需要手工把版本传下去（多一条传递就多一处会忘记同步的真值）。
 *
 * 附带效果：临界区里读 store 的 `entries` 也变成串行的。A 成功后会把 `entries` 清空，
 * 于是排在后面的 B 读到空条目 ⇒ 走 `performSave` 的「没有 staged ⇒ 不是失败」早退。
 * **这正是「写盘成功却报失败」被根除的机制**：重复保存退化成一次无害的 no-op，而不是一次假失败。
 *
 * # 射程边界（勿扩大，也勿指望它管到别处）
 *
 * - **只管本 webview**。托盘浮层是**另一个** JS 上下文（`ui/src/tray/`，独立 entry），模块级
 *   promise 链跨不过去。跨 webview / 跨进程的写冲突由后端的 `baseVersion` 乐观并发闸负责检出
 *   —— 那是**检测**机制，不是预防机制，两者分工不同，别拿本闸门去顶它。
 * - **不可嵌套**：在临界区内再调一次 `withConfigWriteLock` 会自锁（后者排在前者之后，而前者
 *   在等后者）。当前三个使用点（`performSave` / `app-store.saveConfig` / `app-store.switchServer`）互不调用，由
 *   `config_write_lock_has_no_nested_callers` 钉住。
 */

/** 队尾：已落定的前一次写。只保留「落定」这一位，成败与返回值都被吞掉。 */
let tail: Promise<unknown> = Promise.resolve();

/**
 * 把一次配置读改写排进队列，返回它自己的结果（成败原样透传给调用方）。
 *
 * `tail.then(run, run)`：前一次**失败也要放行**下一次 —— 一次保存失败把后续保存永久堵死，
 * 比并发冲突更糟（用户再也存不进任何东西，且没有任何提示）。
 */
export function withConfigWriteLock<T>(run: () => Promise<T>): Promise<T> {
  const mine = tail.then(run, run);
  // 队尾只需要「落定了」，故两侧都吞掉；不吞会在链上留下 unhandled rejection。
  tail = mine.then(
    () => undefined,
    () => undefined
  );
  return mine;
}

/** 仅供单测复位模块级队列（模块单例跨用例存活，不复位会串味）。生产代码不调。 */
export function resetConfigWriteLock(): void {
  tail = Promise.resolve();
}
