/**
 * 弹窗层 toast 门 —— 守「提交级错误改右下角 toast」这一批的**根因**，而不是它当时的文案。
 *
 * # 这批改动到底在修什么（复发方向就是门要挡的方向）
 *
 * `.dlg` 是 flex 列：`.dlg-head`(flex:none) + `.dlg-body`(**flex:1 + overflow-y:auto，弹窗内唯一
 * 滚动容器**) + `.dlg-foot`(flex:none)。提交级错误条渲染在 `Modal` 的 `children` 末尾 ⇒ 落在滚动区内。
 * 用户点的是 footer 里的「保存」，视口停在弹窗底部，错误却在**滚动内容的最底部** ⇒ 要滚到底才看得见。
 * 规则弹窗涨到最坏 ~3050px 后这条被真机反馈抓出来（2026-07-30）。
 *
 * # 为什么必须 portal 进 dialog 子树（不是 z-index 能解的）
 *
 * `showModal()` 把 `<dialog>` 提到 **top-layer**；按 CSS 规范 top-layer 元素**及其 `::backdrop`**
 * 渲染在所有普通内容之上，**z-index 对其无效**。`#toast-stack` 原本挂 `.win` 内 = 普通流
 * ⇒ 弹窗一开 toast 必被遮罩压住。双引擎实测（2026-07-30）：
 *   · WebKitGTK 4.1（= Tauri 在 Linux 用的引擎，亦是 macOS WKWebView 的同系代理）
 *   · Chromium（= Windows WebView2 的代理）
 * 两边一致：toast 停在 `.win` 时 `document.elementFromPoint(toast 中心)` 返回 `<dialog>`（它的
 * `::backdrop`）而不是 toast；Chromium 截图上该采样点是 `rgb(130,5,135)` —— 纯品红 `rgb(255,0,255)`
 * 被 50% 遮罩压暗的值。portal 进 dialog 子树后两边都返回 toast 本身、像素为纯品红。
 * 解法与 csel 菜单同源（`design/polaris-dialog-layer-and-governance.md:12`）；
 * **不走 Popover API**（同文档 :51，macOS floor 13.0 = WebKit 16.0 < 所需 Safari 17）。
 *
 * # 射程与不射程（如实记账）
 *
 * 本仓 vitest 是 `environment:'node'`、无 jsdom（`vite.config.ts:76`，有意为之）⇒
 *  · **能测**：`dialog-top-layer` 的栈语义（纯逻辑，下面 describe 一真跑到底）；
 *  · **测不到**：portal 真的挂进了 dialog、`position:fixed` 真的相对视口、遮罩真的没盖住 ——
 *    这三条属渲染/层叠语义，node 环境里不可观测，靠上面那次双引擎实测 + 真机验收，
 *    源码侧只能钉住「接线还在」（下面 describe 二/三，正则级，挡得住整段被删、挡不住写反）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { readdirSync } from 'node:fs';
import { useDialogTopLayerStore } from './dialog-top-layer';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const stripComments = (src: string) =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

/** 假 dialog：本层只关心「哪个元素在栈顶」，不碰任何 DOM 面。 */
const fakeDialog = (tag: string) => ({ tag }) as unknown as HTMLDialogElement;

describe('dialog-top-layer 栈语义：toast 必须跟到最顶层弹窗', () => {
  const reset = () => useDialogTopLayerStore.setState({ els: [] });
  const top = () => {
    const { els } = useDialogTopLayerStore.getState();
    return els[els.length - 1] ?? null;
  };

  it('无弹窗时栈空 —— toast 回落挂 .win', () => {
    reset();
    expect(top()).toBeNull();
  });

  it('嵌套：后 showModal 的那个是顶层（proc-pick 从规则弹窗内打开）', () => {
    reset();
    const rule = fakeDialog('rule');
    const pick = fakeDialog('proc-pick');
    const { register } = useDialogTopLayerStore.getState();
    register(rule);
    expect(top()).toBe(rule);
    register(pick);
    // 顶层必须跟到最内层那个；停在 rule 上 = toast 被 proc-pick 的 backdrop 压住（原缺陷的嵌套版）。
    expect(top()).toBe(pick);
  });

  it('顶层关闭后回落到下面那层，而不是直接清空', () => {
    reset();
    const rule = fakeDialog('rule');
    const pick = fakeDialog('proc-pick');
    const { register, unregister } = useDialogTopLayerStore.getState();
    register(rule);
    register(pick);
    unregister(pick);
    // 回落到 rule。若实现写成「注销即清栈」，规则弹窗还开着而 toast 已经挂回 .win ⇒ 又被压住。
    expect(top()).toBe(rule);
    unregister(rule);
    expect(top()).toBeNull();
  });

  it('注销中间层不动顶层（关闭次序不必与打开次序对称）', () => {
    reset();
    const a = fakeDialog('a');
    const b = fakeDialog('b');
    const c = fakeDialog('c');
    const { register, unregister } = useDialogTopLayerStore.getState();
    register(a);
    register(b);
    register(c);
    unregister(b);
    expect(top()).toBe(c);
    expect(useDialogTopLayerStore.getState().els).toEqual([a, c]);
  });

  it('重复 register 幂等 —— StrictMode 双跑不得把同一个弹窗压进去两次', () => {
    reset();
    const d = fakeDialog('d');
    const { register, unregister } = useDialogTopLayerStore.getState();
    register(d);
    register(d);
    expect(useDialogTopLayerStore.getState().els).toEqual([d]);
    // 非幂等的话这次 unregister 之后栈里还剩一个**已卸载**的 dialog：它已不在文档里，
    // portal 挂上去 = toast 整个不显示（比被压住更糟，因为连痕迹都没有）。
    unregister(d);
    expect(top()).toBeNull();
  });
});

describe('接线还在：toast 挂载点随弹窗栈走', () => {
  const toaster = stripComments(read('../layout/Toaster.tsx'));
  const modal = stripComments(read('./Modal.tsx'));
  const layer = stripComments(read('./dialog-top-layer.ts'));

  it('Toaster 订阅顶层弹窗并据此 portal（不是恒挂 .win）', () => {
    expect(toaster).toMatch(/useTopDialogEl\(\)/);
    expect(toaster, 'toast 又变回恒挂 .win —— 弹窗一开就被 ::backdrop 压住').toMatch(
      /createPortal\(\s*stack\s*,\s*topDialog\s*\)/,
    );
  });

  it('#toast-stack 内联定位必须是 fixed —— absolute 会相对那 460px 的弹窗定位', () => {
    /* 实测（两引擎一致）：portal 进 dialog 子树后写 absolute，toast 落在**弹窗内部**右下角
       （925×740 视口下 right=669 而非 901）。fixed 才仍相对视口。 */
    const rule = toaster.match(/id="toast-stack"[\s\S]{0,400}?\}\}/);
    expect(rule, '#toast-stack 的内联 style 不见了').not.toBeNull();
    expect(rule![0]).toMatch(/position:\s*'fixed'/);
    expect(rule![0], "absolute 会让 toast 掉进弹窗里").not.toMatch(/position:\s*'absolute'/);
  });

  it('hook 取的是栈尾（最顶层），不是栈首', () => {
    expect(layer).toMatch(/s\.els\[s\.els\.length - 1\]\s*\?\?\s*null/);
  });

  it('Modal 在 showModal 之后登记、卸载时注销（成对，否则栈会漏）', () => {
    const openIdx = modal.indexOf('dialog.showModal()');
    const regIdx = modal.indexOf('register(dialog)');
    expect(openIdx, 'showModal 不见了').toBeGreaterThan(-1);
    expect(regIdx, 'Modal 不再登记进 top-layer 栈 —— topDialog 恒 null，toast 退回被压住').toBeGreaterThan(-1);
    // 顺序有意义：先提层再登记，登记的语义才是「这个元素现在在 top-layer 里」。
    expect(regIdx).toBeGreaterThan(openIdx);
    expect(modal, 'unregister 缺席 —— 弹窗关了栈里还留着已卸载节点，toast 会挂到空处').toMatch(
      /unregister\(dialog\)/,
    );
  });
});

describe('.dlg-err 白名单：提交级错误条不得回到 children 末尾', () => {
  /*
   * 白名单而非计数：新增一处 `.dlg-err` 就转红，逼作者显式判一次「这是上下文级还是提交级」。
   * 上下文级（紧贴出错控件、不需要滚动就能看见）留在原地是**对的**，故不是「一律禁用」。
   */
  const ALLOWED: Record<string, string> = {
    'TsLoginDialog.tsx': '上下文级：紧贴 Auth Key / 登录方式那块控件，距 </Modal> 48 行，不在滚动尾部',
    'SubDialog.tsx': '上下文级：订阅预检结果，与 .mesh-success 二选一，贴着「预检」按钮',
  };

  const dir = fileURLToPath(new URL('.', import.meta.url));
  // 排除 `*.test.tsx`：本门扫的是**弹窗组件**，测试文件里出现 `dlg-err` 是断言字符串、不是渲染。
  // （不排的话，`field-switch-disabled.test.tsx` 这类同目录测试也会各生成一条恒绿用例，
  //  且一旦它的断言里写了 `dlg-err`，报错文案会变成「请把测试文件加进白名单」这种胡话。）
  const files = readdirSync(dir).filter((f) => f.endsWith('.tsx') && !f.endsWith('.test.tsx'));

  it('前提校验：确实扫到了一批弹窗组件（目录改名后本门不得空转恒绿）', () => {
    expect(files.length).toBeGreaterThan(10);
  });

  for (const f of files) {
    it(`${f} 的 .dlg-err 用法在白名单内`, () => {
      const src = stripComments(read(`./${f}`));
      const uses = src.includes('dlg-err');
      if (!uses) {
        expect(
          ALLOWED[f],
          `${f} 已无 .dlg-err —— 请把它从白名单里删掉（白名单不许留过期条目）`,
        ).toBeUndefined();
        return;
      }
      expect(
        ALLOWED[f],
        `${f} 新增了 .dlg-err。若是**提交级**（渲染在 children 末尾），它会落进 .dlg-body 的滚动区、` +
          `点保存时看不见 —— 改用 toast.error(标题, 详情)。若确是上下文级，把它连同理由加进本白名单。`,
      ).toBeDefined();
    });
  }
});
