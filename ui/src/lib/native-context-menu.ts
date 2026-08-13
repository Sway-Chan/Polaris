/**
 * 禁掉 webview **自带**的系统右键菜单（「重新加载」「检查元素」那类——在成品应用里是穿帮）。
 *
 * ## 判据（三分，无第四种情形）
 *
 *  1. **可编辑文本控件**（文本类 input / textarea / contenteditable）→ **弹系统菜单**。
 *     右键粘贴是订阅 URL 那类长文本的刚需，Ctrl/Cmd+V 替代不了（用户手在鼠标上）。
 *  2. **本仓自绘的两处功能菜单**（连接表行 `ConnectionsScreen.tsx` / 拓扑节点 `ConnectionTopology.tsx`）
 *     → 弹它们自己的。
 *  3. **其它任何地方** → 什么都不弹。
 *
 * ①②不冲突：自绘菜单挂在表行 / 拓扑节点上，那里没有文本控件；文本控件上没有自绘菜单。
 *
 * 边界的实质是「**这里的系统菜单是不是一份文本编辑菜单**」。非文本控件
 * （checkbox / radio / range / color / file / date…）右键出的是**页面菜单**（重新加载 / 检查元素 /
 * 另存为）——正是本模块要消灭的东西。故 input 走**类型白名单**而不是「是 input 就放行」。
 *
 * ## 为什么走前端而不是 Tauri 原生开关
 *
 * 查过了，**没有**原生开关可用（tauri 2.11.5 / wry 0.55.1，本仓 Cargo.lock 锁定版本）：
 *  - `tauri.conf.json` 的 `WindowConfig`（tauri-utils `config.rs`）无任何 context menu 字段。
 *  - wry 只有 `WebViewBuilderExtWindows::with_default_context_menus`——**仅 Windows**
 *    （落到 WebView2 的 `SetAreDefaultContextMenusEnabled`），Linux/WebKitGTK 与 macOS/WKWebView 没有对应项；
 *    且 tauri-runtime-wry 根本没把它接出来，Tauri 侧够不着。
 *  ⇒ 即使接得到也只能盖 1/3 平台，且它是**全禁**（连①一起禁）。`preventDefault()` 三个引擎一致生效，
 *    又能按元素分流，是唯一的全平台落法。
 *
 * ## 为什么不会吃掉自绘菜单
 *
 * React 18 的合成事件委托挂在**根容器**（`#root` / `#tray-root`），不是 document。冒泡序为
 * `tr → … → #root（React handler 在此跑）→ body → html → document`，本监听在最末端，
 * 自绘菜单的 `setMenu(...)` 早已执行完。且 `preventDefault()` **不**是 `stopPropagation()`——
 * 它只否决浏览器默认动作，不阻断任何 handler；那两处自己也调了 `preventDefault()`，
 * 这里再调一次是幂等的空操作。故先后顺序在这件事上其实无关紧要，选冒泡末端只是最不打扰的一档。
 *
 * ## 入口花名册
 *
 * 每个 webview 各是一个 document，**必须各挂一次**（一个窗的监听盖不到别的窗）。本仓四个：
 * 主窗 `main.tsx` / 托盘浮层 `tray/main.tsx` / 更新弹窗 `update-popup/main.ts`，
 * 外加 sing-box 官方面板窗——那是**第三方产物**（`scripts/fetch-dashboard.mjs` 拉的 zip、核 serve，
 * 改不了它的 JS），由 Rust 侧 `commands/misc.rs` 的 `DISABLE_CONTEXT_MENU_SCRIPT` 经
 * `initialization_script` 从外面挂同一条监听、同一套判据。花名册由 `native-context-menu.test.ts`
 * 从 `vite.config.ts` 的入口表**推导**后逐个断言，新增入口不接线即红。
 *
 * @param target 监听宿主，默认 `document`。参数只为在本仓 node 环境的 vitest 里注入假宿主
 *   （全仓无 DOM 测试环境，见 `vite.config.ts` test 段）——生产三个前端入口都按默认调用。
 */
export function disableNativeContextMenu(target: EventTarget = document): void {
  target.addEventListener('contextmenu', (e) => {
    if (isTextEditingTarget(e.target)) return;
    e.preventDefault();
  });
}

/**
 * 会给出**文本编辑菜单**（剪切 / 复制 / 粘贴 / 全选）的 input type 白名单。
 *
 * 取白名单而非黑名单：将来出现的新 type 落在「不放行」那一侧 = 维持全禁，是本需求的安全侧。
 * 这不会误伤未知 type —— 规范规定 `input.type` 的 getter 把不认识的值归一化成 `'text'`，
 * 故凡是被浏览器当文本框渲染的，读出来必在白名单内。
 *
 * 排除项分两类：
 *  - 无自由文本可粘贴：button / checkbox / color / file / hidden / image / radio / range / reset / submit。
 *  - 分段选择器：date / time / month / week / datetime-local —— 值由各段拼成，不是一段可粘贴的文本。
 */
const TEXT_INPUT_TYPES = new Set([
  'text',
  'search',
  'url',
  'tel',
  'email',
  'password',
  'number',
]);

/**
 * 本模块用到的那一小片 DOM 面。
 *
 * 用结构类型而非 `instanceof HTMLInputElement`：本仓 vitest 跑 node 环境、**没有** DOM 全局
 * （`vite.config.ts` test 段刻意不引 jsdom），`instanceof` 会直接 ReferenceError。结构类型让判据
 * 能被真单测喂假节点覆盖，而不是退化成只能靠源码扫描守。
 */
interface CtxNode {
  tagName?: string;
  /** `HTMLInputElement.type`（规范保证已归一化，未知值读出来是 `'text'`）。 */
  type?: string;
  disabled?: boolean;
  isContentEditable?: boolean;
  closest?: (selector: string) => CtxNode | null;
  /** `HTMLLabelElement.control`：该 label 标注的表单控件。 */
  control?: CtxNode | null;
}

/**
 * 右键落点是不是「可编辑文本」面 —— 是则放行系统菜单。
 *
 * ## 落点解析：为什么不是直接判 `e.target`
 *
 * `e.target` 是指针下**最深**的那个元素，未必是控件本身：
 *  - 本仓搜索框是 `<label class="input"><svg 放大镜/><input id="conn-search"/></label>`
 *    （`ConnectionsScreen.tsx`）。点在 label 的 11px 内边距上 → target 是 label；点在放大镜上 →
 *    target 是 `<svg>`。只判 target 会得到「点框里放行、点框边不放行」这种同一个控件两种行为。
 *  - 但 `closest()` **只向上走祖先**，从 label / svg 出发够不到兄弟节点的 `<input>` —— 光换成
 *    `closest` 并不解决上面那条。真正解析得到控件的是 `HTMLLabelElement.control`。
 *
 * 故顺序是：① 自身或祖先命中 input/textarea（点在控件里，最常见）→ ② 落在 label 上则解析它标注的控件
 * → ③ 都不是就用 target 自己（走 contenteditable 分支）。
 *
 * ①优先于②的理由：一个 label 若标注多个控件，`control` 只给第一个；直接命中的那个才是用户点的。
 *
 * ②不是「见 label 就放行」——解析出来的控件照样过类型白名单：
 * `<label><input type="checkbox"/> 开启</label>` 解析到 checkbox，不放行。
 *
 * ## disabled / readonly
 *
 *  - `disabled` → **不放行**：拿不到焦点、选不中文本，浏览器给的本来就是页面菜单。
 *  - `readonly` → **放行**：能选中、能复制、能全选（只是不能粘贴），那仍是一份文本编辑菜单；
 *    而只读框恰恰是「右键复制」最有用的地方（生成出来的 URL / 密钥）。
 *    ⇒ 判据是「这里能不能拿到文本选区」，不是「能不能写入」。故代码里没有 `readonly` 这一维。
 */
export function isTextEditingTarget(node: unknown): boolean {
  const el = node as CtxNode | null;
  if (!el || typeof el.closest !== 'function') return false;
  const host = el.closest('input, textarea') ?? el.closest('label')?.control ?? el;
  const tag = (host.tagName ?? '').toUpperCase();
  if (tag === 'TEXTAREA') return !host.disabled;
  if (tag === 'INPUT') {
    return !host.disabled && TEXT_INPUT_TYPES.has((host.type ?? 'text').toLowerCase());
  }
  // contenteditable 区域内的**后代**上 `isContentEditable` 同样为 true（继承），故点在 <b>/<span> 上也成立。
  return host.isContentEditable === true;
}
