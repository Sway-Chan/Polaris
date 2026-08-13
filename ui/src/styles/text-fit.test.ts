/**
 * 文本装得下吗 —— i18n 文案 × 受限容器的几何门（2026-07-31 真机：「俄语容易超出导航栏边框」）。
 *
 * # 缺陷长什么样（阳性对照，本门必须抓到它）
 *
 * `.side{width:148px}` + `.nav-item{padding:8px 9px; gap:10px; font-size:13px; white-space:nowrap}`
 * + `.nav-item svg{width:17px; flex:none}`，且**全链路没有任何 overflow/text-overflow**
 * ⇒ 标签既不换行也不截断，长文案直接画到侧栏外面。ru 的 `sidebar.appPolicy` =
 * 「Политика приложений」在 13px 下约 124–172px（随字体族），可用宽只有 83px。
 *
 * # 为什么是「算」不是「渲染量」
 *
 * 本仓 vitest 是 `environment:'node'`（vite.config.ts），无 jsdom / 无 CSSOM / 无 Canvas
 * ⇒ 真实排版在这一层根本不可观测。故用**字符宽度模型**估算，并对模型本身做阳性/阴性自校（见 ①②）。
 *
 * # 宽度模型的系数从哪来（不是拍脑袋）
 *
 * 字体栈是 `--sans: -apple-system, "Segoe UI Variable", "Segoe UI", "PingFang SC",
 * "Hiragino Sans GB", "Microsoft YaHei", system-ui, sans-serif`（tokens.css / prototype.css），
 * 真机落到 SF Pro（mac）/ Segoe UI（win）/ 系统 sans（linux）。系数取自**离线实测的字体 hmtx 表**：
 * DejaVu Sans Regular+Bold、Liberation Sans Regular+Bold（Arial 度量兼容）、Lato、
 * Noto Sans CJK、Noto Sans Arabic Regular+Bold —— 每个字符桶的系数 = 桶内成员在这批字体上的
 * **advance 最大值**（含 Bold），故对单字符是严格上界，对整串亦然。
 * DejaVu Sans 本身就是这套栈的宽端（西里尔比 Segoe UI 宽约 30%），⇒ 模型相对真机再多一层余量。
 *
 * **自校实测（10281 条 = 5 locale 全量 × 逐条比对上述字体的真实 advance 之和）**：
 * 模型 < 实测的条数 **0**（0.00%），中位过估 +10.5%，p95 +61.5%。方向恒为「偏宽」。
 * 波斯语的比对基准用 Presentation Forms-B 做了**连写还原**（见下）。
 *
 * # 波斯语（RTL + 连写）怎么处理，误差朝哪边
 *
 * - **RTL 不影响宽度**：行内推进方向变了，advance 之和不变 ⇒ 模型按 LTR 累加即可。
 * - **连写（initial/medial/final/isolated 四形）**：模型对整个 Arabic 块用**单一系数 0.84**
 *   （= 上述字体 Arabic 区 advance 均值的上界）。离线用 Presentation Forms-B 码位还原了真实
 *   连写形宽度做对照（例：「تست」连写 2.24em，模型 2.52em；「سیاست برنامه‌ها」连写 6.69–7.53em，
 *   模型 11.28em）—— 连写形普遍**窄于**独立形，故模型恒偏宽。
 * - **这是近似**：不建模 kerning、不建模 لا 之类的强制连字、不建模 Persian 专用字体
 *   （Vazir/IRANSans）的度量差异。误差方向 = 过估，不会漏报。
 * - 组合符号（فتحه/کسره 等 U+064B–U+065F、U+0670）与 ZWNJ/ZWJ/方向标记按 0 宽计（正确）。
 *
 * # 覆盖了什么
 *
 * 三个界面里**有硬宽度上限**的文本容器（可用宽 = 容器宽 − padding − 同行 flex:none 兄弟 − gap，
 * 全部从 CSS 现场解析，不写死数字）：
 *   主窗   S1 主侧栏 nav 标签 / S2 设置侧栏 nav 标签 / S3 两侧栏的 nav-group 组头
 *          S4 连接表定宽列表头（c-dest / c-rule / c-chain / c-rate）
 *          S9 首页右列 seg2 三档（接管方式 / 分流策略）—— 见下方「S9 是一类形态」
 *          S11 节点弹窗（`.dlg` 定宽 460px）的表单字段：`.fld-l` 标签 / `.fld-hint` 提示 /
 *              `.swt-row` 开关行的标签与提示 —— **可换行、无 ellipsis、无 overflow-wrap**，
 *              两条判据同时生效，见「S11 判据为什么是两条」
 *          S12 导入弹窗的解析结果预览（同一个 `.dlg` 定宽）：`.imp-stat` 三颗计数 pill /
 *              `.card-sub` 清单小标题 / `.imp-badge`「不支持」徽标 —— 徽标那条判的不是溢出
 *              （它 `flex:none` 不收缩、名称轨带 ellipsis 会替它让位），而是「别把名称轨压到
 *              看不清」，见该段
 *   托盘   S6 浮层菜单行（带右侧勾/箭头/延迟的更窄那档）/ S7 浮层状态副标题
 *          S7b 浮层状态标题 / S9 浮层组头（10px uppercase）/ S10 浮层一次性提示条
 *   更新窗 S8 380px 弹窗的按钮行（**可换行**，行数有预算，见该段）
 * 语种：**五种全测** en-US / ru / fa / zh-CN / zh-TW —— 托盘与更新弹窗 2026-07-31 接入 i18n 后
 * 与主窗同口径，不再有「只测 zh/en」的例外。
 *
 * # S9 是一类形态：「基类无约束、变体有约束」
 *
 * S1–S8 的判据是「容器有硬宽度上限」。S9 起加第二条判据：**基类本身没有宽度约束（`inline-flex` /
 * `max-content`），但某个变体选择器给它加了「均分或撑满」**——`.seg2` 是 `inline-flex`（= max-content，
 * 天然装得下），可 `.seg-wrap .seg2{width:100%}` + `.seg-wrap .seg2 button{flex:1}` 把它钉成容器宽并
 * 要求三档均分。只看基类会把它误判成「无上限、不用测」（本门 2026-07-31 初版就是这么漏的）。
 *
 * 全仓按此判据扫过一遍（`width:100%` 落在 inline-* / max-content 基类上、`minmax(0,…)` 网格轨道、
 * `flex:1`+`min-width:0` 的 flex 项），结论逐条记在下方「射程之外」的 h.。
 *
 * # S11 判据为什么是两条（一条硬、一条棘轮）
 *
 * `.fld-l` / `.fld-hint` 都是**可折行的块**（无 `white-space:nowrap`）、**无** `text-overflow:ellipsis`、
 * **无** `overflow-wrap:anywhere`。三条合起来决定了它跟 S1（nowrap 画到框外）和 S8（定死窗高裁按钮）
 * 都不同形，判据必须分成两条：
 *
 *  1. **硬判据 —— 不可断长串宽度**（`maxLineWidth > avail`）。没有 `overflow-wrap` ⇒ 一个比字段还宽的
 *     不可断单元（长西里尔词 / `Sec-WebSocket-Protocol` 式 token / URL）真的横向溢出。溢出的下场不是
 *     「画到窗外」（`.dlg{overflow:hidden}` 拦住了），而是 **`.dlg-body` 长出横向滚动条** —— 它写了
 *     `overflow-y:auto`，而 CSS Overflow §3 规定「一轴非 visible 时另一轴的 visible 计算成 auto」
 *     ⇒ 整张表单要左右拖才能读完一个标签。这是真破版，恒判红。
 *  2. **棘轮判据 —— 行数预算**（`lines > maxLines`）。`.dlg` 是 `max-height:calc(100vh - 40px)`、
 *     `.dlg-body` 是 `overflow-y:auto` ⇒ **纵向没有硬上限，超高只是变高、要滚动，不会被裁掉**。
 *     故这条**不是**「顶出可视区」那种量级（那是 S8 更新弹窗的形态：`popup_height_for` 定死窗高 +
 *     `body{overflow:hidden}`，第三行按钮真的点不到）。它的作用是棘轮：预算钉在今天最长的那一格，
 *     再长一句就转红。别把它读成「超了会坏」。
 *
 * 顺带：本节是第一个「可折行 + 无 overflow-wrap + 有大量中文语料」的容器，于是第一次踩到排版模型
 * 里那个「只按空白切词」的近似 —— 详见 `layout()` 的注释（该近似已就地修掉，方向是去掉**假红**）。
 *
 * # 没覆盖什么（射程自曝，别把绿当成「全都装得下」）
 *
 *  a. **运行期才知道长度的用户数据**：节点名 / 订阅名 / 域名 / 进程名 / 规则值 / 版本号 /
 *     速率数字。这些容器（`.nd-name`/`.conn-host`/`.tb-name`/`.tray-node-name`/`.cat-nm` …）
 *     本来就带 `text-overflow:ellipsis`，属于「截断」而非「画到框外」，且没有静态真值可测。
 *  b. **运行期拼接的文案**：带 `{{n}}`/`{{count}}` 插值的键按模板长度测，实际值更长；
 *     `defaultValue` 内联兜底文案不测。
 *  c. ~~托盘浮层只有中/英两态~~ —— **已消除**（2026-07-31）。`tray/labels.ts` 改走
 *     `i18n/auxiliary.ts` 的键查找，文案住进 `locales/*.json` 的 `tray.*`，五语种齐备；本门随之五语全测。
 *  d. ~~更新弹窗完全没有 i18n~~ —— **已消除**（2026-07-31）。文案住进 `updatePopup.*`，五语种齐备。
 *     两条旧例外正是 `i18n/i18n-coverage.test.ts` 那道门要防复发的东西：**「某个界面整个漏在
 *     i18n 体系外」此前没有任何门管**，于是它能一路绿到真机。
 *  e. **未进注册表的受限容器**：`.app-pol`(max-width:112px)、`.ctx-note`(120px)、
 *     `.res-row` 定宽列、`.lock-field`(280px)、`.node-menu`(360px)、`.proto-grid` 等
 *     —— 它们要么已带 ellipsis（信息损失但不破版），要么渲染的是用户数据。
 *     ⚠️ 这条**从来没有涵盖过弹窗表单的 `.fld` 一族**（`.fld-l` / `.fld-hint` / `.swt-row`）：
 *     它们不带 ellipsis、渲染的也不是用户数据，只是**一直没被列进来**——不是判过不进门，是没看见。
 *     2026-08-06 起节点弹窗那份进门（S11），其余弹窗仍在门外，逐条见下面的 i.。
 *  f. **纵向**：只算宽度与行数，不算像素高度。换行后行数增加导致的**纵向**溢出，只有在容器有硬高度
 *     上限时才是「被裁掉」（S8 更新弹窗是这一类）；S10 / S11 的行数预算是**棘轮**不是墙，见各自段注释。
 *  i. **S11 只覆盖 `node.field.*`（ND_SPEC 的 99 个键），节点弹窗里下面这些仍在门外**：
 *     · `NodeDialog.tsx` **内联**的那批（`node.protocol` / `node.label` / `node.serverPort` /
 *       `node.chainVia` / `node.chainHint` / `node.transportSecurity` / `node.frontProxy` /
 *       `node.customProbe.*`）—— 它们不在 ND_SPEC 里，是 JSX 里手写的，要进门得再加一层
 *       regex + 手维护槽位表（同 `TRAY_SLOT` 的形态）。它们与已测字段**同槽同宽**，风险同类。
 *     · **其余弹窗的同款 `.fld` 一族**：`SubDialog` / `WarpDialog` / `WgDialog` / `TsSettingsDialog` /
 *       `RuleDialog` / `ImportDialog` / `AppAddDialog` …。容器几何一模一样（都是 `.dlg` 460px），
 *       缺的只是各自的「键 → 槽」映射。本批刻意只做节点弹窗，别把 S11 的绿读成「所有弹窗都装得下」。
 *     · **`select` 的选项文案与 placeholder**：`TCP` / `xtls-rprx-vision` / `obfs=http;obfs-host=…`
 *       这类专有名词是字面量、不入 i18n（`field-spec.tsx` 文件头有说明），且下拉宽度锁触发器宽、
 *       超长走 ellipsis（components.css 的 CSEL 段），属 a. 那一类。
 *     · ~~**`common.optional`「可选」徽标在 fa/ru 缺失**~~ —— **已补齐**（2026-08-07）。本门按
 *       **真实回落链**量它（locale 里有就用 locale 的、没有就用代码里的 zh 默认），此前 `fa`/`ru`
 *       两个语种根本没有这个键 ⇒ 那两种语言的用户在 52 个 `opt:true` 字段上看到的都是中文「可选」。
 *       补上之后徽标从 2 全角（≈20px）变成 ru「Необязательно」≈110px / fa「اختیاری」，标签行宽了一大截：
 *       实测 `echConfig`/`hostKeyAlgorithms`/`idleCheck`/`kexAlgorithm`/`privateKeyPassphrase` 等
 *       多条 ru/fa 标签由 1 行涨到 2 行（仍在预算内），**没有一条越预算** —— 本门是这件事的验收面。
 *  g. **字体真值**：真机字体不在这批离线字体里（SF Pro / Segoe UI 均未安装），模型是它们的
 *     保守上界，不是它们本身。
 *  h. **「基类无约束、变体有约束」全仓扫描里判为不进门的**（2026-07-31，逐条判过，不是没看）：
 *     · `.brand-svg`（`width:100%` 落在 inline-flex 基类上，components.css:253 / index.css:460）
 *       —— 承载的是 SVG，没有文本。
 *     · `.top-grid .top-card-h .seg2`（index.css:1399，同族的第二个 seg2）—— 三档文案是**数字**
 *       （topN = 5/10/20，ConnectionsScreen.tsx:922），与语种无关；且它是 `flex:none`（保 max-content，
 *       压不动），同行的标题才是让宽的那一侧且自带 ellipsis。
 *     · `.term-row code`（`flex:1;min-width:0` + nowrap + **无** ellipsis，components.css:1129）
 *       —— 但它带 `overflow-x:auto`，是**可横向滚动**而非画到框外；且内容是运行期拼的命令行。
 *     · `#s-nodes.nodes-list-view .nd-pills`（`flex:1;min-width:0`，screens.css:295，内含 nowrap 的
 *       `.pill`）—— `.nd-pills{flex-wrap:wrap}`（screens.css:85），pill 会换行不会溢出。
 *     · `.cc-cols` 左列 / `.res-row` / `.cat-item` 的 `minmax(0,…)` 轨道 —— 里面的文本节点
 *       （`.exit-name`/`.exit-addr`/`.cat-nm`/资源名）全是运行期用户数据，且已带 ellipsis。
 *     · `.aad-nc .sel{width:100%}`、`.aad-res-bar/.cond-head 的 .search-box{flex:1}` —— 原生
 *       `<select>` / `<input>`，超长由控件自身裁切，不会画到框外。
 *     · `.row2` / `.field-grid` 的 `1fr 1fr`、`.mesh-grid`/`.node-grid`/`.app-wall`/`.cidr-list`/
 *       `.aad-ico-grid`/`.proto-grid` 的 `repeat(auto-fill,minmax(N,1fr))` —— 轨道有 `minmax` 下限
 *       兜底，且里面的文本要么可换行（无 nowrap）要么是带 ellipsis 的用户数据。
 *     · `.unlock-field .unlock-row{flex:1}`、`.top-bar-row/.ut-prog/.res-prog 的 .bar{flex:1}`、
 *       `#s-home > .topo{flex:1 1 auto}` —— 纵向 flex 容器 / 进度条 / SVG，无 nowrap 文本。
 *     · `.field-lbl`（S9 的同排兄弟，静态 i18n）—— **没有 nowrap**，超宽只会折行不会画到框外；
 *       且同行挂着运行期条件元素 `ReverseRoutingBadge`（reverseMesh 开时才出现），要进门就得给
 *       运行期状态建模，属 a./b. 之外。实测余量：最窄容器下可用 191.7px，最长项 ru
 *       「Маршрутизация」11.5px 下 135.6px，余 56px。
 *
 * # 读不到就报错，不跳过
 *
 * 所有 CSS 值与组件里的键都是**解析出来的**：解析不到 → `throw`（不是 `it.skip`）。
 * 改窄某个容器、加一条导航项、加一个语种、加一条长翻译，都会让本门转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
// S11 的字段清单**直接取真值源**，不 regex 扒 `node-spec.ts` 的文本：ND_SPEC 是导出的纯数据表
// （对 `field-spec` 只有 `import type`，不牵进 React ⇒ node 环境可直接 import），regex 反而会在
// `...F_TRANSPORT` 这类展开处漏字段而**静默少测**。同 `homeSegGroups()` 用 regex 的理由相反：
// 那批常量住在 .tsx 组件文件里、拿不到；这张表拿得到。
import { ND_SPEC, type NodeProto } from '../components/dialogs/node-spec';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const stripComments = (src: string) =>
  src.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '));

// ════════════════════════════════════════════════════════════════════════════════════════
// ① 字符宽度模型
// ════════════════════════════════════════════════════════════════════════════════════════

/**
 * 字宽 ÷ font-size。每档 = 档内成员在参考字体集上的 advance **最大值**（见文件头「系数从哪来」）。
 * 参考字体集：DejaVu Sans R/B、Liberation Sans R/B、Lato、Noto Sans CJK、Noto Sans Arabic R/B。
 */
export const COEF = {
  space: 0.36, // U+0020/A0/2007/2009/202F —— max 0.348 (DejaVu Bold)
  latThin: 0.4, // i j l I ' . , : ;      —— max 0.400 (DejaVu Bold ':')
  latNarrow: 0.57, // f r t J ! | ( ) [ ] - / \ * "  —— max 0.556 (Liberation Bold 'J')
  latLower: 0.73, // 其余小写拉丁             —— max 0.716 (DejaVu Bold 'b/d/g/p/q')
  latUpper: 0.86, // 其余大写拉丁             —— max 0.850 (DejaVu Bold 'O/Q')
  latWide: 1.1, // m w M W @ % & # + = < > { } ~ ^ $ ? _ – — … —— max 1.103 (DejaVu Bold 'W')
  digit: 0.72, // 0-9                     —— max 0.696 (DejaVu Bold)
  cyrLower: 0.83, // 其余西里尔小写            —— max 0.820 (DejaVu Bold 'м')
  cyrUpper: 0.95, // 其余西里尔大写            —— max 0.940 (DejaVu Bold 'Ъ')
  cyrWide: 1.33, // щ ш ж ф ю ы Щ Ш Ж Ю Ы М Ф —— max 1.326 (DejaVu Bold 'Щ')
  arabic: 0.84, // 阿拉伯/波斯字母（连写前的保守上界，见文件头）—— DejaVu Bold 区均值 0.807
  arabicDigit: 0.64, // ۰-۹ / ٠-٩            —— max 0.639 (Noto Sans Arabic Bold)
  cjk: 1.0, // CJK/假名/谚文/全角        —— 恒等宽 1.000 (Noto Sans CJK)
  zero: 0, // ZWNJ/ZWJ/LRM/RLM/BOM/组合符号
  unknown: 1.1, // 未归档字符：按最宽档兜底（宁可过估）
} as const;

const LAT_THIN = new Set([...`ijlI'.,:;`]);
const LAT_NARROW = new Set([...`frtJ!|()[]-/\\*"‘’“”`]);
const LAT_WIDE = new Set([...'mwMW@%&#+=<>{}~^$?_–—…']);
const CYR_WIDE = new Set([...'щшжфюыЩШЖЮЫМФ']);

export function charEm(ch: string): number {
  const cp = ch.codePointAt(0)!;
  if (cp === 0x20 || cp === 0xa0 || cp === 0x2007 || cp === 0x2009 || cp === 0x202f)
    return COEF.space;
  if (
    cp === 0x200b ||
    cp === 0x200c ||
    cp === 0x200d ||
    cp === 0x200e ||
    cp === 0x200f ||
    cp === 0x061c ||
    cp === 0xfeff ||
    (cp >= 0x0300 && cp <= 0x036f) ||
    (cp >= 0x064b && cp <= 0x065f) ||
    cp === 0x0670
  )
    return COEF.zero;
  if (cp >= 0x30 && cp <= 0x39) return COEF.digit;
  if ((cp >= 0x06f0 && cp <= 0x06f9) || (cp >= 0x0660 && cp <= 0x0669)) return COEF.arabicDigit;
  if (
    (cp >= 0x0600 && cp <= 0x06ff) ||
    (cp >= 0x0750 && cp <= 0x077f) ||
    (cp >= 0xfb50 && cp <= 0xfdff) ||
    (cp >= 0xfe70 && cp <= 0xfeff)
  )
    return COEF.arabic;
  if ((cp >= 0x0400 && cp <= 0x04ff) || (cp >= 0x0500 && cp <= 0x052f)) {
    if (CYR_WIDE.has(ch)) return COEF.cyrWide;
    return (cp >= 0x0410 && cp <= 0x042f) || (cp >= 0x0400 && cp <= 0x040f)
      ? COEF.cyrUpper
      : COEF.cyrLower;
  }
  if (
    (cp >= 0x4e00 && cp <= 0x9fff) ||
    (cp >= 0x3400 && cp <= 0x4dbf) ||
    (cp >= 0x3000 && cp <= 0x303f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x3040 && cp <= 0x30ff) ||
    (cp >= 0xac00 && cp <= 0xd7af)
  )
    return COEF.cjk;
  if (LAT_THIN.has(ch)) return COEF.latThin;
  if (LAT_NARROW.has(ch)) return COEF.latNarrow;
  if (LAT_WIDE.has(ch)) return COEF.latWide;
  if (cp < 0x250) {
    if (ch >= 'a' && ch <= 'z') return COEF.latLower;
    if (ch >= 'A' && ch <= 'Z') return COEF.latUpper;
    if (cp >= 0xc0)
      return ch === ch.toUpperCase() && ch !== ch.toLowerCase() ? COEF.latUpper : COEF.latLower;
    return COEF.latLower;
  }
  return COEF.unknown;
}

export const textEm = (s: string) => [...s].reduce((t, ch) => t + charEm(ch), 0);

interface TypeSpec {
  fontSize: number;
  /** CSS `letter-spacing`，单位 em（px 值需先除 font-size 后传入）。 */
  letterSpacingEm?: number;
  /** CSS `text-transform:uppercase`。大写字母更宽，必须先转换再测。 */
  uppercase?: boolean;
}
export function textPx(text: string, { fontSize, letterSpacingEm = 0, uppercase = false }: TypeSpec) {
  const s = uppercase ? text.toUpperCase() : text;
  // CSS 的 letter-spacing 在**每个**字符后加一份（含末字符），故乘字符数而非 n-1。
  return (textEm(s) + letterSpacingEm * [...s].length) * fontSize;
}

/**
 * 「不可断单元」判定：相邻两个 CJK 表意文字 / 假名 / 谚文之间有断行机会（UAX #14 class ID，
 * `word-break:normal` 下浏览器默认就在这里断）。**标点两侧一律不算**（`（`『，』`）`… 有禁则），
 * 故本集合刻意排除 U+3000–303F、U+FF00–FF60 —— 少给断点 = 偏保守 = 只会高估行数，不会漏报。
 */
const cjkBreakable = (cp: number) =>
  (cp >= 0x4e00 && cp <= 0x9fff) || // CJK 统一表意
  (cp >= 0x3400 && cp <= 0x4dbf) || // 扩展 A
  (cp >= 0x3040 && cp <= 0x30fa) || // 平/片假名（排除 U+30FB「・」）
  (cp >= 0x30fc && cp <= 0x30ff) ||
  (cp >= 0xac00 && cp <= 0xd7af); // 谚文音节

/**
 * 把一行文本切成**不可断单元**：空白处切（空白被吃掉，`space=true` 表示该单元前有一个空格），
 * 相邻 CJK 之间切（不吃字符，`space=false`）。
 */
function breakUnits(s: string): { text: string; space: boolean }[] {
  const out: { text: string; space: boolean }[] = [];
  const chars = [...s];
  let cur = '';
  let space = false;
  const flush = (nextSpace: boolean) => {
    if (cur) out.push({ text: cur, space });
    cur = '';
    space = nextSpace;
  };
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i];
    if (/[\s​]/u.test(ch)) {
      flush(true);
      continue;
    }
    cur += ch;
    const nx = chars[i + 1];
    if (nx && cjkBreakable(ch.codePointAt(0)!) && cjkBreakable(nx.codePointAt(0)!)) flush(false);
  }
  flush(false);
  return out;
}

/**
 * 贪心排版，返回占几行。
 *
 * - `nowrap` ⇒ 恒 1 行（整串宽 > 可用宽即溢出，由调用方判）。
 * - `breakAnywhere=false` ⇒ 只在**断行机会**处断（空白 + 相邻 CJK 之间，见 `breakUnits`）；
 *   某个不可断单元本身超宽时该行照样溢出（返回的行宽会 > avail，由调用方按 `maxLineWidth` 判溢出）。
 * - `breakAnywhere=true`（CSS `overflow-wrap:anywhere`）⇒ 超宽单元在内部断，永不溢出。
 * - `tailPx` = 行尾还挂着一段**固定宽的 inline 内容**（S11 的 `.fld-opt`「可选」徽标：字号与标签
 *   不同 ⇒ 塞不进同一个 `TypeSpec` 里量）。放不下就跟浏览器一样把它挤到下一行。
 *
 * ── CJK 断点是 2026-08-06 补的，方向是**去掉假红** ────────────────────────────────────────
 * 旧版只按空白切词，于是一整句无空格中文 = **一个不可断的词**，其宽度必然 > 任何窄容器
 * ⇒ 对「不可断单元超宽」那条判据是**误报**方向。S1–S10 一直没踩到：它们要么带
 * `overflow-wrap:anywhere`（词内可断，误差被吸收），要么文案短。S11 的 `.fld-hint` 两条都不占，
 * 首次把它暴露成假红（zh-TW `node.field.muxPadHint` 整句算 357.0px vs 可用 346px，而真机在每两个
 * 汉字之间都能断，实测 5 行、最宽行 343.0px）。
 * 加断点只会让行数与最宽行**单调变小**（贪心排版下断点集变大 ⇒ 每行填得不会更少），
 * 故对 S1–S10 只可能更松，不可能把已绿的判红 —— 不是放宽预算，是修掉模型的高估。
 * 仍然保守：不在 `-` / `/` / 标点前后断（真实浏览器会），故真实行数 ≤ 本函数。
 */
function layout(
  s: string,
  avail: number,
  type: TypeSpec,
  opts: { wrap: boolean; breakAnywhere: boolean; tailPx?: number },
): { lines: number; maxLineWidth: number } {
  const w = (t: string) => textPx(t, type);
  const tail = opts.tailPx ?? 0;
  if (!opts.wrap) return { lines: 1, maxLineWidth: w(s) + tail };
  const spaceW = w(' ');
  let lines = 1;
  let cur = 0;
  let maxLine = 0;
  const push = (width: number) => {
    maxLine = Math.max(maxLine, width);
  };
  for (const unit of breakUnits(s)) {
    const ww = w(unit.text);
    const sep = unit.space && cur > 0 ? spaceW : 0;
    if (cur > 0 && cur + sep + ww > avail) {
      push(cur);
      lines++;
      cur = 0;
    }
    if (ww <= avail || !opts.breakAnywhere) {
      cur = cur === 0 ? ww : cur + sep + ww;
      continue;
    }
    // 单元内断：逐字符填满一行再换（`overflow-wrap:anywhere`）。
    for (const ch of unit.text) {
      const cw = w(ch);
      if (cur > 0 && cur + cw > avail) {
        push(cur);
        lines++;
        cur = 0;
      }
      cur += cw;
    }
  }
  if (tail > 0) {
    if (cur > 0 && cur + tail > avail) {
      push(cur);
      lines++;
      cur = 0;
    }
    cur += tail;
  }
  push(cur);
  return { lines, maxLineWidth: maxLine };
}

// ════════════════════════════════════════════════════════════════════════════════════════
// ② CSS 几何解析 —— 读不到即 throw
// ════════════════════════════════════════════════════════════════════════════════════════

/** @import 顺序（index.css:14-16）：components → screens → prototype，index.css 自身规则在最后。 */
const CSS_FILES = [
  './components.css',
  './screens.css',
  './prototype.css',
  './index.css',
  '../tray/tray-overlay.css',
] as const;
type CssFile = (typeof CSS_FILES)[number];

const SRC = new Map<string, string>(CSS_FILES.map((f) => [f, stripComments(read(f))]));

interface Rule {
  sel: string;
  body: string;
}
const rulesOf = (file: CssFile): Rule[] => {
  const css = SRC.get(file)!;
  return [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({
    // 选择器只取最后一个 `;` 之后：`@import '…';` / `@tailwind x;` 会被 `[^{}]+` 吞进来。
    sel: m[1].split(';').pop()!.trim().replace(/\s+/g, ' '),
    body: m[2].replace(/\s+/g, ' ').trim(),
  }));
};
const ALL: { file: CssFile; sel: string; body: string }[] = CSS_FILES.flatMap((f) =>
  rulesOf(f).map((r) => ({ file: f, ...r })),
);

/** 选择器逐字匹配（含逗号分组内的一项）。 */
const selMatches = (sel: string, want: string) =>
  sel.split(',').some((s) => s.trim().replace(/\s+/g, ' ') === want);

/** 取 `file` 里 `selector` 规则的 `prop` 声明。找不到 → throw（不静默跳过）。 */
function decl(file: CssFile, selector: string, prop: string): string {
  for (const r of rulesOf(file)) {
    if (!selMatches(r.sel, selector)) continue;
    const m = r.body.match(new RegExp(`(?:^|;)\\s*${prop}\\s*:\\s*([^;]+)`));
    if (m) return m[1].trim();
  }
  throw new Error(`CSS 里读不到 ${file} \`${selector}\` 的 \`${prop}\` —— 选择器被改名/删除？`);
}

const px = (v: string): number => {
  if (/^-?0(\.0+)?$/.test(v.trim())) return 0; // CSS 允许 0 省单位（`padding:0 10px 12px`）
  const m = v.match(/(-?[\d.]+)px/);
  if (!m) throw new Error(`不是 px 值：\`${v}\``);
  return parseFloat(m[1]);
};
/** `padding: a b c d` 简写 → 左右内边距之和。 */
function padX(shorthand: string): number {
  const parts = shorthand.trim().split(/\s+/).map(px);
  if (parts.length === 1) return parts[0] * 2;
  return parts[1] * 2; // 2/3/4 值写法里第 2 个都是左右
}
/**
 * 间距阶梯 `--sp-N` 的真值（`prototype.css` 的 `:root`，见 index.css 段注释：prototype.css 是最后一个
 * @import，它自带一整套同选择器令牌声明 ⇒ tokens.css 那份不是生效的那份）。
 * 缺一档即 throw：本门里 `padding:var(--sp-4)` 这类简写全靠它解出 px。
 */
const SPACING: ReadonlyMap<string, number> = (() => {
  const m = new Map<string, number>();
  for (const { sel, body } of rulesOf('./prototype.css')) {
    if (sel !== ':root') continue;
    for (const d of body.split(';')) {
      const mm = d.match(/(--sp-\d+)\s*:\s*([\d.]+)px/);
      if (mm) m.set(mm[1], parseFloat(mm[2]));
    }
  }
  if (m.size < 7)
    throw new Error(`prototype.css 的 :root 里只解析到 ${m.size} 档 --sp-* —— 间距阶梯被改名/搬走？`);
  return m;
})();
/** 把值里的 `var(--sp-N)` 换成 px 字面量；解析不到的 var 直接 throw（不静默留原样）。 */
const resolveSp = (v: string): string =>
  v.replace(/var\(\s*(--sp-\d+)\s*\)/g, (_, k: string) => {
    const n = SPACING.get(k);
    if (n === undefined) throw new Error(`\`${k}\` 不在 prototype.css 的间距阶梯里`);
    return `${n}px`;
  });

/** `letter-spacing` → em。`.1em` 直接取；`normal` = 0。 */
function lsEm(v: string, fontSize: number): number {
  if (/normal/.test(v)) return 0;
  const em = v.match(/(-?[\d.]+)em/);
  if (em) return parseFloat(em[1]);
  return px(v) / fontSize;
}

/**
 * 解析「这段文本会不会换行」：按 @import 顺序扫全部文件，取**最后一条**命中的
 * `white-space` 声明（同特异性后者胜；覆盖层写在 index.css 全部 @import 之后）。
 * `selectors` 按「从祖先继承 → 自身」排列。
 */
function wraps(selectors: string[]): boolean {
  let last: string | undefined;
  for (const { sel, body } of ALL) {
    if (!selectors.some((s) => selMatches(sel, s))) continue;
    const m = body.match(/(?:^|;)\s*white-space\s*:\s*([^;]+)/);
    if (m) last = m[1].trim();
  }
  if (last === undefined) return true; // 无声明 = 初始值 normal = 换行
  return !/nowrap|pre(?!-line|-wrap)/.test(last);
}
/** 该文本链路上有没有 `text-overflow:ellipsis`（有 = 截断而非画到框外）。 */
function clips(selectors: string[]): boolean {
  return ALL.some(
    (r) => selectors.some((s) => selMatches(r.sel, s)) && /text-overflow\s*:\s*ellipsis/.test(r.body),
  );
}
/** 该文本链路上有没有 `overflow-wrap:anywhere|break-word`（有 = 超宽单词可在词内断，永不溢出）。 */
function breaksAnywhere(selectors: string[]): boolean {
  return ALL.some(
    (r) =>
      selectors.some((s) => selMatches(r.sel, s)) &&
      /(?:overflow-wrap|word-break)\s*:\s*(anywhere|break-word|break-all)/.test(r.body),
  );
}

// ════════════════════════════════════════════════════════════════════════════════════════
// ③ i18n 语料 + 容器↔键 映射（都从源码解析，解析不到即 throw）
// ════════════════════════════════════════════════════════════════════════════════════════

/** 语种**从目录列出**，不写死清单 —— 新增一个 locale 文件即自动进门（写死就等于新语种默认免检）。 */
const LOCALES: string[] = readdirSync(fileURLToPath(new URL('../i18n/locales', import.meta.url)))
  .filter((f) => f.endsWith('.json'))
  .map((f) => f.replace(/\.json$/, ''))
  .sort();
type Locale = string;

const flatten = (o: unknown, p = '', out: Record<string, string> = {}) => {
  for (const [k, v] of Object.entries(o as Record<string, unknown>)) {
    const kk = p ? `${p}.${k}` : k;
    if (typeof v === 'string') out[kk] = v;
    else if (v && typeof v === 'object') flatten(v, kk, out);
  }
  return out;
};
/**
 * 语料 = 主分区 + 辅助 webview 分区（`locales/auxiliary/`，托盘与更新弹窗的 `tray.*` / `updatePopup.*`；
 * 分区理由见 `i18n/locale-parity.test.ts` 里那段注释——打包上分家，量文案时合成一份）。
 * `aux/` 读不到即 `read()` 抛错，不静默跳过（跳过 = 托盘/弹窗文案又一次不进门）。
 */
const DICT: Record<Locale, Record<string, string>> = Object.fromEntries(
  LOCALES.map((l) => [
    l,
    {
      ...flatten(JSON.parse(read(`../i18n/locales/${l}.json`))),
      ...flatten(JSON.parse(read(`../i18n/locales/auxiliary/${l}.json`))),
    },
  ]),
) as Record<Locale, Record<string, string>>;
if (LOCALES.length < 5) throw new Error(`只列出 ${LOCALES.length} 个 locale —— locales 目录读错了？`);

const src = (rel: string) => read(rel);
function grepAll(text: string, re: RegExp, what: string): RegExpMatchArray[] {
  const hits = [...text.matchAll(re)];
  if (hits.length === 0) throw new Error(`源码里一条 ${what} 都没解析到 —— 渲染方式变了？正则失效？`);
  return hits;
}

/** 主侧栏 nav 标签键（Sidebar.tsx 的 `labelKey: 'sidebar.x'` + 贴底 settings + 折叠/展开 aria）。 */
function sidebarNavKeys(): string[] {
  const s = src('../components/layout/Sidebar.tsx');
  const keys = grepAll(s, /labelKey:\s*'([\w.]+)'/g, 'Sidebar labelKey').map((m) => m[1]);
  // 贴底「设置」不走 NAV_DEF 表，是内联 `t('sidebar.settings')`（两处：data-tip + span）。
  const inline = grepAll(s, /t\('(sidebar\.settings)'\)/g, 'Sidebar 贴底 settings 键').map((m) => m[1]);
  return [...new Set([...keys, ...inline])];
}
/** 设置侧栏 nav 标签键（SettingsSidebar.tsx 的 `label: t('settings.nav.x')` + 贴底 back）。 */
function settingsNavKeys(): string[] {
  const s = src('../components/screens/settings/SettingsSidebar.tsx');
  const items = grepAll(s, /label:\s*t\('(settings\.nav\.[\w]+)'\)/g, 'SettingsSidebar label').map(
    (m) => m[1],
  );
  const back = grepAll(s, /t\('(settings\.nav\.back)'\)/g, 'SettingsSidebar back 键').map((m) => m[1]);
  return [...new Set([...items, ...back])];
}
/** 两个侧栏的 `.nav-group` 组头键。 */
function navGroupKeys(): string[] {
  const a = grepAll(
    src('../components/layout/Sidebar.tsx'),
    /t\('(sidebar\.group\.[\w]+)'\)/g,
    'Sidebar nav-group',
  ).map((m) => m[1]);
  const b = grepAll(
    src('../components/screens/settings/SettingsSidebar.tsx'),
    /header:\s*t\('(settings\.nav\.[\w]+)'\)/g,
    'SettingsSidebar nav-group',
  ).map((m) => m[1]);
  return [...new Set([...a, ...b])];
}
/** 连接表定宽列的表头：`thSortable('k', t('connections.colX'), 'c-y')` → [键, 列 class]。 */
function connTableFixedCols(): { key: string; cls: string }[] {
  const hits = grepAll(
    src('../components/screens/connections/ConnectionsScreen.tsx'),
    /thSortable\(\s*'[\w]+'\s*,\s*t\('([\w.]+)'\)\s*,\s*'(c-[\w-]+)'\s*\)/g,
    'ConnectionsScreen thSortable 定宽列',
  );
  return hits.map((m) => ({ key: m[1], cls: m[2] }));
}
/**
 * 托盘浮层用到的全部文案键（`t('tray.x')` + `MODES`/`TAKEOVERS` 表里的 `k: 'tray.x'`）。
 *
 * 2026-07-31 前这里扫的是 `t('中文','English')` 双语字面量。那个形态有个**结构性的漏**：正则是单行的，
 * 而 `t(\n '长中文',\n '长英文'\n)` 这种跨行写法扫不到 —— FakeIP 自动启用那条 30 字提示就一直在门外。
 * 改成扫键之后跨行与否都无所谓（键短，恒在一行内）。
 */
function trayKeys(): string[] {
  const s = src('../tray/TrayMenu.tsx');
  const inline = grepAll(s, /\bt\('(tray\.[\w]+)'\)/g, "TrayMenu t('tray.x') 键").map((m) => m[1]);
  const tables = grepAll(s, /\bk:\s*'(tray\.[\w]+)'/g, 'TrayMenu MODES/TAKEOVERS 表键').map((m) => m[1]);
  return [...new Set([...inline, ...tables])];
}
/** 更新弹窗按钮行：按 `.row` 收集按钮的 i18n 键（`updatePopup.*`）。 */
function updatePopupButtonRows(): { phase: string; keys: string[] }[] {
  const s = src('../update-popup/main.ts');
  const rows = grepAll(s, /<div class="row(?: between)?">([\s\S]*?)<\/div>/g, 'update-popup 按钮行');
  return rows.map((m, i) => ({
    phase: `row#${i + 1}`,
    keys: [...m[1].matchAll(/<button[^>]*>\$\{esc\(t\('(updatePopup\.[\w]+)'\)\)\}<\/button>/g)].map(
      (b) => b[1],
    ),
  }));
}

// ════════════════════════════════════════════════════════════════════════════════════════
// ④ 受限容器注册表 —— 可用宽全部现算，算式写在每个 `avail` 上
// ════════════════════════════════════════════════════════════════════════════════════════

/** `.side` / `.nav-item` / `.nav-group` 在 components.css 与 prototype.css 各存一份，必须同值。 */
const SIDEBAR_DUP = ['./components.css', './prototype.css'] as const;
function dupAgreed(selector: string, prop: string): string {
  const vals = SIDEBAR_DUP.map((f) => decl(f, selector, prop));
  if (new Set(vals).size !== 1)
    throw new Error(
      `\`${selector}\` 的 \`${prop}\` 在 components.css / prototype.css 不同值（${vals.join(' vs ')}）` +
        ` —— 本仓经典坑：两份重复定义只改了一份，后 @import 的 prototype.css 生效。`,
    );
  return vals[0];
}

/** 侧栏（主 + 设置共用 `.side`）几何。 */
const sideW = px(dupAgreed('.side', 'width')); // 148
const sidePadX = padX(dupAgreed('.side', 'padding')); // 10*2 = 20
const sideInner = sideW - sidePadX; // `*{box-sizing:border-box}`(prototype.css:63) ⇒ padding 吃在 148 内

const navPadX = padX(dupAgreed('.nav-item', 'padding')); // 9*2 = 18
const navGap = px(dupAgreed('.nav-item', 'gap')); // 10
const navIcon = px(dupAgreed('.nav-item svg', 'width')); // 17
const navFont = px(dupAgreed('.nav-item', 'font-size')); // 13
/** S1/S2 可用宽 = 148 − 10×2 − 9×2 − 17(icon) − 10(gap) */
const NAV_LABEL_AVAIL = sideInner - navPadX - navIcon - navGap;

const grpPadX = padX(dupAgreed('.nav-group', 'padding')); // 9*2 = 18
const grpFont = px(dupAgreed('.nav-group', 'font-size')); // 10
const grpLs = lsEm(dupAgreed('.nav-group', 'letter-spacing'), grpFont); // .1em
const grpUpper = /uppercase/.test(dupAgreed('.nav-group', 'text-transform'));
/** S3 可用宽 = 148 − 10×2 − 9×2 */
const NAV_GROUP_AVAIL = sideInner - grpPadX;

/** 连接表：th 定宽列（index.css 的 `.conn-table .c-*`）。 */
const thPadX = padX(decl('./components.css', '.conn-table th', 'padding')); // 12*2 = 24
const thFont = px(decl('./components.css', '.conn-table th', 'font-size')); // 10.5
const thLs = lsEm(decl('./components.css', '.conn-table th', 'letter-spacing'), thFont); // .05em
const thUpper = /uppercase/.test(decl('./components.css', '.conn-table th', 'text-transform'));
const sortArFont = px(decl('./components.css', '.conn-table th .sort-ar', 'font-size')); // 9
const sortArMl = px(decl('./components.css', '.conn-table th .sort-ar', 'margin-left')); // 3
/** 排序箭头 `▲` 常驻 DOM（未排序时只是 opacity:0，仍占宽）。 */
const SORT_AR_W = textPx('▲', { fontSize: sortArFont }) + sortArMl;
/** 列的宽度上限：跨全部 CSS 文件找 `.conn-table .c-x` 的 width / max-width；没有上限返回 null。 */
function colCapPx(cls: string): number | null {
  let cap: number | null = null;
  for (const r of ALL) {
    if (!selMatches(r.sel, `.conn-table .${cls}`) && !selMatches(r.sel, `.${cls}`)) continue;
    const m = r.body.match(/(?:^|;)\s*(?:max-)?width\s*:\s*([^;]+)/);
    if (m) cap = px(m[1].trim());
  }
  return cap;
}

/** 托盘浮层：窗宽由 Rust `TRAY_WIDTH` 定，卡片靠 margin 收进来。 */
const TRAY_WINDOW_W = (() => {
  const rs = readFileSync(
    fileURLToPath(new URL('../../../src-tauri/src/tray.rs', import.meta.url)),
    'utf8',
  );
  const m = rs.match(/const TRAY_WIDTH:\s*f64\s*=\s*([\d.]+)/);
  if (!m) throw new Error('src-tauri/src/tray.rs 里读不到 TRAY_WIDTH —— 常量被改名？');
  return parseFloat(m[1]);
})();
const trayCardMarginX = padX(decl('../tray/tray-overlay.css', '.tray-menu', 'margin')); // 11*2
const trayCardPadX = padX(decl('./components.css', '.tray-menu', 'padding')); // 6*2
const trayInner = TRAY_WINDOW_W - trayCardMarginX - trayCardPadX;
const trayIPadX = padX(decl('../tray/tray-overlay.css', '.tray-menu .tray-i', 'padding')); // 11*2
const trayIGap = px(decl('./components.css', '.tray-i', 'gap')); // 10
const trayIcon = px(decl('./components.css', '.tray-i>svg:first-child', 'width')); // 16
const trayFont = px(decl('./components.css', '.tray-i', 'font-size')); // 12.5
const trayChev = px(decl('./components.css', '.tray-i .tray-chev', 'width')); // 15
/** S5 = 268 − 11×2 − 6×2 − 11×2 − 16(icon) − 10(gap) */
const TRAY_ROW_AVAIL = trayInner - trayIPadX - trayIcon - trayIGap;
/** S6 = S5 − 15(右侧勾/箭头) − 10(gap) */
const TRAY_ROW_TRAILING_AVAIL = TRAY_ROW_AVAIL - trayChev - trayIGap;

const trayStPadX = padX(decl('../tray/tray-overlay.css', '.tray-menu .tray-status', 'padding'));
const trayStGap = px(decl('./components.css', '.tray-status', 'gap')); // 9
const trayStMk = px(decl('./components.css', '.tray-status .ts-mk', 'width')); // 26
const trayStSubFont = px(decl('./components.css', '.tray-status div', 'font-size')); // 10.5
const trayStTitleFont = px(decl('./components.css', '.tray-status b', 'font-size')); // 12.5
/** S7 / S7b = 卡内宽 − 11×2 − 26(logo 磁贴) − 9(gap)。`.ts-tx{flex:1;min-width:0}` ⇒ 标题与副标题同槽。 */
const TRAY_STATUS_SUB_AVAIL = trayInner - trayStPadX - trayStMk - trayStGap;

const trayGrpPadX = padX(decl('./components.css', '.tray-group-h', 'padding')); // 11*2
const trayGrpFont = px(decl('./components.css', '.tray-group-h', 'font-size')); // 10
const trayGrpLs = lsEm(decl('./components.css', '.tray-group-h', 'letter-spacing'), trayGrpFont); // .06em
const trayGrpUpper = /uppercase/.test(decl('./components.css', '.tray-group-h', 'text-transform'));
/** S9 = 卡内宽 − 11×2 */
const TRAY_GROUP_AVAIL = trayInner - trayGrpPadX;

const trayNoteMarginX = padX(decl('../tray/tray-overlay.css', '.tray-menu .tray-note', 'margin')); // 8*2
const trayNotePadX = padX(decl('../tray/tray-overlay.css', '.tray-menu .tray-note', 'padding')); // 8*2
const trayNoteFont = px(decl('../tray/tray-overlay.css', '.tray-menu .tray-note', 'font-size')); // 11
/** S10 = 卡内宽 − 8×2(margin) − 8×2(padding)。带 `word-break:break-word` ⇒ 横向不可能溢出，只钉行数。 */
const TRAY_NOTE_AVAIL = trayInner - trayNoteMarginX - trayNotePadX;

/** 更新弹窗：窗宽由 Rust `POPUP_WIDTH` 定。 */
const POPUP_W = (() => {
  const rs = readFileSync(
    fileURLToPath(new URL('../../../crates/updater/src/popup.rs', import.meta.url)),
    'utf8',
  );
  const m = rs.match(/pub const POPUP_WIDTH:\s*u32\s*=\s*(\d+)/);
  if (!m) throw new Error('crates/updater/src/popup.rs 里读不到 POPUP_WIDTH —— 常量被改名？');
  return parseFloat(m[1]);
})();
const popupCss = stripComments(read('../update-popup/style.css'));
const popupDecl = (selector: string, prop: string) => {
  for (const m of popupCss.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const sel = m[1].split(';').pop()!.trim().replace(/\s+/g, ' ');
    if (!selMatches(sel, selector)) continue;
    const d = m[2].replace(/\s+/g, ' ').match(new RegExp(`(?:^|;)\\s*${prop}\\s*:\\s*([^;]+)`));
    if (d) return d[1].trim();
  }
  throw new Error(`update-popup/style.css 里读不到 \`${selector}\` 的 \`${prop}\``);
};
const popupCardPadX = padX(popupDecl('.card', 'padding')); // 16*2
const popupBtnPadX = padX(popupDecl('.btn', 'padding')); // 12*2
const popupBtnBorder = 2; // `.btn{border:1px solid}` 左右各 1px（border-box 下仍占内容宽）
const popupRowGap = px(popupDecl('.row', 'gap')); // 8
const popupFont = px(popupDecl('body', 'font').match(/([\d.]+px)/)![1]); // `font: 13px/1.5 …`
/** S8 = 380 − 16×2 */
const POPUP_ROW_AVAIL = POPUP_W - popupCardPadX;

// ── S9 首页右列 seg2：几何链 + `:lang()` 名单 ──────────────────────────────────────────────
//
// 判「装不装得下」用的是**三档 min-content 之和**，不是「3 × 最宽档」：`.seg-wrap .seg2 button{flex:1}`
// = `flex:1 1 0%`，先按均分派 1/3，但 flex 项的 `min-width` 初始值是 `auto` ⇒ 自动最小尺寸 = min-content，
// 而按钮是 `white-space:nowrap` ⇒ 超过 1/3 的那颗冻在自己的 min-content 上、剩余空间再分给没冻的。
// 只有总和超了才真的溢出（整颗按钮越过轨道边框往外画，非文字越过按钮）。
// headless Chromium 逐字复核过这条算式与下面整条几何链（详见 index.css 的 S9 段注释）。

/** 窗口最小宽（`tauri.conf.json`，同时也是默认宽）—— 最小容器宽由它与 `.side` 现算，不写死。 */
const TAURI_MIN_WIDTH = (() => {
  const conf = JSON.parse(read('../../../src-tauri/tauri.conf.json')) as {
    app?: { windows?: { label?: string; minWidth?: number }[] };
  };
  const w = conf.app?.windows?.find((x) => x.label === 'main');
  if (!w || typeof w.minWidth !== 'number')
    throw new Error('tauri.conf.json 里读不到 main 窗口的 minWidth —— 窗口配置结构变了？');
  return w.minWidth;
})();
/** `.main` 是 `container:mainc/inline-size`，其 inline-size = 窗口宽 − `.side`（`flex:none`）。 */
const CONTAINER_MIN = TAURI_MIN_WIDTH - sideW;

const screenPadX = padX(resolveSp(dupAgreed('.screen', 'padding')));
const cardBorderX = px(dupAgreed('.card', 'border').split(/\s+/)[0]) * 2;
const connCardPadX = padX(resolveSp(dupAgreed('.conn-card', 'padding')));
/** `.cc-cols{grid-template-columns:minmax(0,3.4fr) minmax(0,1.7fr); gap:0}` → 右列占比。 */
const CC_RIGHT_SHARE = (() => {
  const v = dupAgreed('.cc-cols', 'grid-template-columns');
  const fr = [...v.matchAll(/([\d.]+)fr/g)].map((m) => parseFloat(m[1]));
  if (fr.length !== 2) throw new Error(`.cc-cols 不再是两条 fr 轨道：\`${v}\` —— 右列占比得重新推`);
  if (px(dupAgreed('.cc-cols', 'gap')) !== 0) throw new Error('.cc-cols 有了 gap —— 右列宽算式要加一项');
  return fr[1] / (fr[0] + fr[1]);
})();
const ccRightPadLeft = px(resolveSp(dupAgreed('.cc-col.right', 'padding-left')));
const segPadX = padX(dupAgreed('.seg2', 'padding'));
const segBorderX = px(dupAgreed('.seg2', 'border').split(/\s+/)[0]) * 2;
const segGap = px(dupAgreed('.seg2', 'gap'));
const segBtnPadX = padX(dupAgreed('.seg2 button', 'padding'));
const segBtnFont = px(dupAgreed('.seg2 button', 'font-size'));

/** 容器宽 → `.seg2` 轨道外宽（`*{box-sizing:border-box}` ⇒ padding/border 都吃在各自宽度内）。 */
const trackFromContainer = (c: number) =>
  (c - screenPadX - cardBorderX - connCardPadX) * CC_RIGHT_SHARE - ccRightPadLeft;
/** 反函数：轨道外宽 → 所需容器宽。 */
const containerFromTrack = (t: number) =>
  (t + ccRightPadLeft) / CC_RIGHT_SHARE + screenPadX + cardBorderX + connCardPadX;
/** 一组 n 档横排所需的轨道外宽 = Σ(文字 + 按钮内边距) + 档间 gap + 轨道 padding/border。 */
const trackNeededFor = (labels: string[]) =>
  labels.reduce((t, s) => t + textPx(s, { fontSize: segBtnFont }) + segBtnPadX, 0) +
  segGap * (labels.length - 1) +
  segPadX +
  segBorderX;

/** 首页右列两组 seg2 的 i18n 键（`INTERCEPT_OPTS` / `ROUTING_OPTS` 的 labelKey，唯一真值源）。 */
function homeSegGroups(): { group: string; keys: string[] }[] {
  const s = src('../components/screens/home/HomeScreen.tsx');
  return ['INTERCEPT_OPTS', 'ROUTING_OPTS'].map((group) => {
    const m = s.match(new RegExp(`const ${group}[^=]*=\\s*\\[([\\s\\S]*?)\\];`));
    if (!m) throw new Error(`HomeScreen.tsx 里读不到 ${group} —— 常量被改名/换写法？`);
    const keys = grepAll(m[1], /labelKey:\s*'([\w.]+)'/g, `${group} 的 labelKey`).map((x) => x[1]);
    if (keys.length < 3) throw new Error(`${group} 只解析到 ${keys.length} 档 —— 正则失效？`);
    return { group, keys };
  });
}

/**
 * 解析 index.css 里的转纵向规则：`@container mainc (max-width:Npx){ :lang(X) …{flex-direction:column} }`。
 * 用花括号计数扫 at-rule 体（`rulesOf` 的扁平正则丢掉 at-rule 条件，拿不到阈值）。
 */
function segStackRules(): { lang: string; maxWidth: number }[] {
  const css = SRC.get('./index.css')!;
  const out: { lang: string; maxWidth: number }[] = [];
  const at = /@container\s+mainc\s*\(\s*max-width\s*:\s*([\d.]+)px\s*\)\s*\{/g;
  for (let m = at.exec(css); m; m = at.exec(css)) {
    let depth = 1;
    let i = at.lastIndex;
    for (; i < css.length && depth > 0; i++) {
      if (css[i] === '{') depth++;
      else if (css[i] === '}') depth--;
    }
    const body = css.slice(at.lastIndex, i - 1);
    for (const r of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      const sel = r[1].trim().replace(/\s+/g, ' ');
      if (!/\.seg-wrap\s+\.seg2$/.test(sel)) continue;
      if (!/flex-direction\s*:\s*column/.test(r[2]))
        throw new Error(`\`${sel}\` 命中 seg2 却没有 flex-direction:column —— 修法被改？`);
      const lang = sel.match(/:lang\(\s*([\w-]+)\s*\)/)?.[1];
      if (!lang) throw new Error(`seg2 转纵向规则 \`${sel}\` 没有 :lang() —— 语言无关阈值会误伤 zh，见段注释`);
      out.push({ lang, maxWidth: parseFloat(m[1]) });
    }
  }
  return out;
}
/** CSS 的语言范围匹配：`:lang(en)` 命中 `en-US`（Selectors 3 §6.6.3）。 */
const langMatches = (tag: string, locale: string) =>
  locale.toLowerCase() === tag.toLowerCase() ||
  locale.toLowerCase().startsWith(`${tag.toLowerCase()}-`);

// ── S11 节点弹窗表单字段（`.fld` 一族）：几何链 ──────────────────────────────────────────
//
// 弹窗定宽是本节能立门的前提：`.dlg{width:min(460px, calc(100vw - 40px))}`，而主窗最小宽 925
// ⇒ calc 支恒 ≥ 885px，永远不是较小的那个 ⇒ **460px 与窗口大小无关**。若哪天改成随窗口变，
// 下面 `DLG_W` 的解析就会 throw，而不是拿一个错的常数继续发绿。
const DLG_W = (() => {
  const v = decl('./components.css', '.dlg', 'width');
  const m = v.match(/min\(\s*([\d.]+)px\s*,\s*calc\(\s*100vw\s*-\s*([\d.]+)px\s*\)\s*\)/);
  if (!m)
    throw new Error(
      `.dlg 的 width 不再是 \`min(Npx, calc(100vw - Mpx))\`：\`${v}\` —— 弹窗定宽这个前提没了，S11 的可用宽要重推`,
    );
  const [fixed, inset] = [parseFloat(m[1]), parseFloat(m[2])];
  if (TAURI_MIN_WIDTH - inset <= fixed)
    throw new Error(
      `窗口最小宽 ${TAURI_MIN_WIDTH}px 下 calc 支 = ${TAURI_MIN_WIDTH - inset}px ≤ ${fixed}px` +
        ` ⇒ 弹窗在小窗下不再定宽，S11 得改成按最小窗口宽算`,
    );
  return fixed;
})();
const dlgBorderX = px(decl('./components.css', '.dlg', 'border').split(/\s+/)[0]) * 2; // 1*2
const dlgBodyPadX = padX(dupAgreed('.dlg-body', 'padding')); // 18*2 = 36
/** S11 顶层槽（`.dlg-body` 直接子项的 `.fld`）= 460 − 1×2 − 18×2 = 422 */
const FLD_AVAIL = DLG_W - dlgBorderX - dlgBodyPadX;

const foldBorderX = px(dupAgreed('.fld-fold', 'border').split(/\s+/)[0]) * 2; // 1*2
const foldPadX = padX(dupAgreed('.fld-fold-body', 'padding')); // 13*2 = 26
/** S11 折叠槽（`.fld-fold-body` 里的 `.fld`，即「传输 / 安全」与「前置代理」两段）= 422 − 1×2 − 13×2 = 394 */
const FOLD_AVAIL = FLD_AVAIL - foldBorderX - foldPadX;

const swtRowGap = px(dupAgreed('.swt-row', 'gap')); // 12
const swtW = px(decl('./prototype.css', '.swt', 'width')); // 36
/**
 * 开关行的文本列：`.swt-row{display:flex}` 里 `.swt-tx{flex:1;min-width:0}` 是**唯一** grow 项、
 * flex-basis 0；`.swt` 定宽 36 且此处自由空间为正（不发生收缩）⇒ 文本列 = 容器 − gap − 36。
 */
const swtTextAvail = (container: number) => container - swtRowGap - swtW;

const fldLabelFont = px(dupAgreed('.fld-l', 'font-size')); // 11.5
const fldHintFont = px(dupAgreed('.fld-hint', 'font-size')); // 10.5
const swtLabelFont = px(dupAgreed('.swt-row .swt-tx b', 'font-size')); // 12.5
const fldOptFont = px(decl('./components.css', '.fld-l > .fld-opt', 'font-size')); // 10

/** `.fld-l` / `.fld-hint` 的选择器链（`wraps`/`clips`/`breaksAnywhere` 用）。 */
const FLD_LABEL_SELS = ['.fld-l', '.swt-row .swt-tx b'];
const FLD_HINT_SELS = ['.fld-hint'];

/**
 * `.fld-opt`「可选」徽标 —— 它**贴在标签行尾**，同一个 `.fld-l` 块内、10px 的 inline span
 * （`field-spec.tsx` 的 `{spec.opt && <span className="fld-opt"> {t('common.optional','可选')}</span>}`，
 * span 内前置一个空格即两者之间的间隙）。52 个字段带 `opt:true`，不建模就是把标签行**系统性少算一截**。
 *
 * 取值走**真实回落链**，而 i18next 的真实回落链是 `loc → fallbackLng('en-US') → defaultValue`
 * —— `defaultValue` 排在**最后**，只有键在整条语言链上都缺席才轮得到它。实测（i18next 23.16.8，
 * `lng:'ru'` + `fallbackLng:'en-US'`，en-US 有该键、ru 无）：
 * ```
 * t('common.optional', '可选') === 'Optional'   // ← fallback 赢过 defaultValue
 * t('nope.key',        '可选') === '可选'        // ← 整条链都没有才用 defaultValue
 * ```
 * 所以 2026-08-07 补译前，`fa`/`ru` 用户在 52 个 `opt` 字段上看到的是**英文 `Optional`**，
 * 不是中文「可选」——`en-US` 一直有这个键。（这一点先前写反过：#80 的原标题「显示中文」是错的，
 * 而错的模型一旦写进 `optTailPx`，本门就会按中文「可选」那个**窄一半**的宽度放行。）
 *
 * 补齐后 ru 是「Необязательно」（≈110px，比英文 `Optional` 还宽一截），本门当场按新宽度
 * 重量了全部 52 个 `opt` 字段的标签行。
 *
 * ⇒ 下面 `optTailPx` 的三级回落**必须与 i18next 同序**。哪天有人把它简化成
 * 「locale 没有就用 zh 默认」，本门就会在「某语种漏了这个键」时按窄宽度静默放行，
 * 而用户实际看到的是更宽的英文。`OPT_ZH_DEFAULT` 只是链尾兜底，不是 fallback 的替身。
 */
const OPT_ZH_DEFAULT = (() => {
  const s = src('../components/dialogs/field-spec.tsx');
  const m = s.match(/t\(\s*'common\.optional'\s*,\s*'([^']+)'\s*\)/);
  if (!m)
    throw new Error(
      "field-spec.tsx 里读不到 `t('common.optional', …)` 的 zh 默认 —— 「可选」徽标的渲染方式变了？",
    );
  return m[1];
})();
/**
 * i18next 的解析顺序：当前语种 → `fallbackLng` → `defaultValue`。三级缺一不可，见上方注释。
 * 拆成显式取值函数是为了让「缺 locale」这条支路**可被单独喂进去测**——五份 locale 现在都有
 * 这个键，直接测 `optTail('ru')` 永远走不到后两级，那样的断言没有信息量。
 */
const optTailOf = (own: string | undefined, en: string | undefined) => own ?? en ?? OPT_ZH_DEFAULT;
const optTail = (loc: Locale) =>
  optTailOf(DICT[loc]['common.optional'], DICT['en-US']['common.optional']);
const optTailPx = (loc: Locale) => textPx(` ${optTail(loc)}`, { fontSize: fldOptFont });

/** S11 的一个测点：某个 i18n 键渲染在哪个槽里（同一个键跨协议复用时只留一份）。 */
type FldSlot = 'LABEL' | 'HINT' | 'SWT_LABEL' | 'SWT_HINT';
interface FldPoint {
  key: string;
  slot: FldSlot;
  /** `cred` = 弹窗顶层（422px）；`adv` = 折叠段内（394px）。 */
  section: 'cred' | 'adv';
  /** 该键至少在一处带「可选」徽标 ⇒ 按带徽标量（取最坏）。 */
  opt: boolean;
}
/**
 * 遍历 ND_SPEC 的协议字段，把传统 {cred, adv} 与长表单的 groups 一起摊成「键 → 槽」。
 * groups 在 620px 弹窗的 tab panel 内，比传统 cred 槽更宽；这里仍按 cred 的 422px 保守量，
 * 这样无需为少数协议另开一套更松、也更容易漂移的预算。
 *
 * 槽由 `FieldSpec.t` 决定，与 `FieldRenderer` 的分支一一对应：
 *  - `switch` → 标签渲染成 `.swt-tx b`（12.5px）、hint/disabledHint 渲染成 `.swt-tx` 内的 `.fld-hint`，
 *    两者都在被开关挤窄的那一列里（容器 − 12 − 36）；**且 switch 分支不渲染 `.fld-opt`**（无徽标）。
 *  - 其余 → 标签是 `.fld-l`（11.5px，带可选徽标）、hint 渲染成控件下方占整幅宽的 `.fld-hint`。
 *    **hint 不再只有 `select` 有**：2026-08-07 `hint` 提到了 `FieldBase`（text/number/textarea 也能有），
 *    这里的判据同步从 `f.t === 'select' && f.hint` 放宽成 `f.hint` —— 否则新加的 text hint
 *    会**静默不进门**（正是 `.fld` 一族当初整族漏测的同款形态）。
 * 同一个键在多个协议里出现时取**最窄的槽**（`adv` 比 `cred` 窄）与「任一处 opt 即 opt」，即最坏情形。
 */
function nodeFieldPoints(): FldPoint[] {
  const byId = new Map<string, FldPoint>();
  const put = (key: string, slot: FldSlot, section: 'cred' | 'adv', opt: boolean) => {
    const id = `${key}|${slot}`;
    const prev = byId.get(id);
    if (!prev) byId.set(id, { key, slot, section, opt });
    else {
      if (section === 'adv') prev.section = 'adv';
      prev.opt ||= opt;
    }
  };
  let n = 0;
  for (const proto of Object.keys(ND_SPEC) as NodeProto[]) {
    for (const section of ['cred', 'adv'] as const)
      for (const f of ND_SPEC[proto][section]) {
        n++;
        if (f.t === 'switch') {
          put(f.label, 'SWT_LABEL', section, false);
          if (f.hint) put(f.hint, 'SWT_HINT', section, false);
          if (f.disabledHint) put(f.disabledHint, 'SWT_HINT', section, false);
        } else {
          put(f.label, 'LABEL', section, f.opt === true);
          if (f.hint) put(f.hint, 'HINT', section, false);
        }
      }
    for (const group of ND_SPEC[proto].groups ?? [])
      for (const f of group.fields) {
        n++;
        if (f.t === 'switch') {
          put(f.label, 'SWT_LABEL', 'cred', false);
          if (f.hint) put(f.hint, 'SWT_HINT', 'cred', false);
          if (f.disabledHint) put(f.disabledHint, 'SWT_HINT', 'cred', false);
        } else {
          put(f.label, 'LABEL', 'cred', f.opt === true);
          if (f.hint) put(f.hint, 'HINT', 'cred', false);
        }
      }
  }
  if (n < 100) throw new Error(`ND_SPEC 只走到 ${n} 个字段实例 —— 表结构变了？`);
  return [...byId.values()];
}

/**
 * S11 行数预算 —— **纵向没有硬上限**（`.dlg{max-height:calc(100vh-40px)}` + `.dlg-body{overflow-y:auto}`），
 * 超预算 = 「这一格变得很高、要多滚一段」，**不是**「被裁掉、点不到」（那是 S8 的形态）。
 * 所以这四个数是**棘轮**，不是物理墙；它们钉在今天最长的那一格上，让「再长一句」自曝。
 *
 * 逐档怎么定的（模型口径，模型偏宽 ⇒ 真机行数只会更少）：
 *  - `LABEL` / `SWT_LABEL` = **2**：这是个**设计判据**不是实测跟随 —— 标签是控件的名字，占到第 3 行
 *    就说明它其实是一句说明、该拆进 `.fld-hint`。今天实测最差正好 2 行
 *    （`.fld-l` 15/325 个测点 2 行、`.swt-tx b` 1/60 个测点 2 行），故它同时也是紧的。
 *  - `HINT` = **8**：控件下方占整幅宽的说明行（394px）。2026-08-07 起 `hint` 提到了 `FieldBase`，
 *    这一档不再只覆盖 `select`，text/number/textarea 的说明也走同一个槽；今天最差仍是
 *    ru `spoofMethodHint` 8 行（新加的四条 h2/secretKeys 说明均未逼近该档）。
 *  - `SWT_HINT` = **12**：开关行的说明（346px 宽，被开关挤窄 48px），今天最差 ru `tlsFragmentHint` 12 行。
 *    12 行 × (10.5px × line-height 1.5) ≈ 189px，加上标题行 ≈ 210px —— 在最小窗口（740px 高，
 *    `.dlg` ≤ 700px，头尾固定约 124px ⇒ 正文可视约 576px）里占三分之一强。**难看，但不破版**；
 *    要真收，得改译文而不是改容器（`.dlg` 宽度已由 2026-07-30 裁定「不加宽」，见 index.css:1137 段）。
 */
const FLD_MAX_LINES: Record<FldSlot, number> = {
  LABEL: 2,
  SWT_LABEL: 2,
  HINT: 8,
  SWT_HINT: 12,
};

// ── 溢出计算 ────────────────────────────────────────────────────────────────────────────
interface Over {
  where: string;
  loc: string;
  key: string;
  text: string;
  need: number;
  avail: number;
  lines: number;
  budget: number;
}
interface Box {
  where: string;
  avail: number;
  type: TypeSpec;
  wrap: boolean;
  breakAnywhere: boolean;
  /** 允许占几行。`nowrap` 容器恒为 1。 */
  maxLines: number;
  /** 行尾固定宽的 inline 附加物（S11 的「可选」徽标）。随 locale 变 ⇒ 调用点 spread 覆盖。 */
  tailPx?: number;
}
function check(bucket: Over[], box: Box, loc: string, key: string, text: string) {
  const { lines, maxLineWidth } = layout(text, box.avail, box.type, box);
  const over = maxLineWidth > box.avail + 0.001 || lines > box.maxLines;
  if (over)
    bucket.push({
      where: box.where,
      loc,
      key,
      text,
      need: maxLineWidth,
      avail: box.avail,
      lines,
      budget: box.maxLines,
    });
}
const fmt = (o: Over[]) =>
  o
    .map(
      (x) =>
        `  ${x.where} | ${x.loc} | ${x.key} | "${x.text}" | 最宽行 ${x.need.toFixed(1)}px / 可用 ${x.avail.toFixed(1)}px` +
        ` | ${x.lines} 行 / 预算 ${x.budget} 行`,
    )
    .join('\n');

// ════════════════════════════════════════════════════════════════════════════════════════
// ⑤ 门
// ════════════════════════════════════════════════════════════════════════════════════════

/**
 * 对照组用的**标定时几何**：`.side` 148px 那一版算出来的 83px 可用宽。
 * 写死是刻意的 —— ① 只考模型，不考几何；几何变了该由 ②/③ 报，不该把模型自校一起带红。
 */
const CALIB_NAV_AVAIL = 83;

describe('① 模型自校：已知溢出必须被算出来，已知不溢出不得被误判', () => {
  it('阳性对照 —— ru `sidebar.appPolicy` 在 83px 槽里必须被算成溢出', () => {
    const text = DICT.ru['sidebar.appPolicy'];
    expect(text).toBe('Политика приложений'); // 语料变了就该重新对照，不许静默跟着变
    const w = textPx(text, { fontSize: 13 });
    // 真机实测区间 124px(Segoe UI 估) – 172.0px(DejaVu Sans Bold 实测)；模型必须落在其上。
    expect(w).toBeGreaterThan(172);
    expect(w).toBeGreaterThan(CALIB_NAV_AVAIL);
  });

  it('阴性对照 —— 短标签不得被误判成溢出', () => {
    for (const [loc, key, realMax] of [
      ['ru', 'sidebar.server', 38.9], // "Узлы" DejaVu Bold 实测 38.9px
      ['ru', 'sidebar.rules', 62.4], // "Правила"
      ['en-US', 'sidebar.home', 34.4], // "Home"
      ['zh-CN', 'sidebar.appPolicy', 52.0], // "应用分流" = 4 全角 × 13px
    ] as const) {
      const w = textPx(DICT[loc][key], { fontSize: 13 });
      expect(w, `${loc}/${key} 被模型算成 ${w.toFixed(1)}px，超了 83px 槽 = 误报`).toBeLessThanOrEqual(
        CALIB_NAV_AVAIL,
      );
      expect(w, `${loc}/${key} 模型 ${w.toFixed(1)}px < 实测 ${realMax}px = 模型漏报`).toBeGreaterThanOrEqual(
        realMax,
      );
    }
  });

  it('分档系数必须逐档单调有序，且 CJK 恒为 1em', () => {
    expect(COEF.latThin).toBeLessThan(COEF.latNarrow);
    expect(COEF.latNarrow).toBeLessThan(COEF.latLower);
    expect(COEF.latLower).toBeLessThan(COEF.latUpper);
    expect(COEF.latUpper).toBeLessThan(COEF.latWide);
    expect(COEF.cyrLower).toBeLessThan(COEF.cyrUpper);
    expect(COEF.cyrUpper).toBeLessThan(COEF.cyrWide);
    expect(textEm('应用分流')).toBe(4);
    expect(textEm('‌‍‎')).toBe(0); // ZWNJ/ZWJ/LRM 零宽
  });
});

describe('② 几何：受限容器的可用宽必须从 CSS 现场算出（不是写死的常数）', () => {
  it('侧栏 nav 标签可用宽 = side.width − side.padX − navItem.padX − icon − gap', () => {
    // 钉的是**原型基线快照**：这几个数变了不等于错，但必须有人重新过一遍下面的行数预算
    // （NAV_MAX_LINES / NAV_GROUP_MAX_LINES 是按 83px / 110px 标定的）。
    expect(sideW, '.side 宽变了 → 重新标定 NAV_MAX_LINES').toBe(148);
    expect(NAV_LABEL_AVAIL).toBeCloseTo(148 - 20 - 18 - 17 - 10, 5); // 83
    expect(NAV_GROUP_AVAIL).toBeCloseTo(148 - 20 - 18, 5); // 110
  });
  it('托盘行可用宽由 Rust TRAY_WIDTH 推导，更新窗由 POPUP_WIDTH 推导', () => {
    expect(TRAY_WINDOW_W).toBe(268);
    expect(POPUP_W).toBe(380);
    expect(TRAY_ROW_AVAIL).toBeCloseTo(268 - 22 - 12 - 22 - 16 - 10, 5); // 186
    expect(POPUP_ROW_AVAIL).toBeCloseTo(380 - 32, 5); // 348
  });
});

/**
 * 行数预算 —— 换行不是免费的：折成一根柱子的导航项照样是缺陷，只是缺陷换了形状。
 *
 * **预算是「模型口径」的行数**，模型偏宽 ⇒ 真机行数只会更少。逐项实测（13px，可用宽 83px）：
 *  - nav 标签 4 行：最长项 ru `sidebar.appPolicy` = "Политика"(模型 87.9px / DejaVu Bold 实测 72.7px)
 *    + "приложений"(模型 114.4 / 实测 94.8) ⇒ 模型 4 行、**最宽真实字体 DejaVu Sans Bold 3 行**、
 *    Segoe UI / SF Pro 2 行。预算按模型给，因为门手里只有模型这一把尺。
 *    仍然有牙：再长一截（模型 > 4×83 = 332px）就转红。
 *  - 组头 2 行：最长项 ru `sidebar.group.routing` = "МАРШРУТИЗАЦИЯ" 模型 144px / 可用 110px = 2 行。
 */
const NAV_MAX_LINES = 4;
const NAV_GROUP_MAX_LINES = 2;

const NAV_LABEL_SELS = ['.nav-item', '.side .nav-item > span:not(.cnt)'];
const NAV_GROUP_SELS = ['.nav-group', '.side .nav-group'];

const navBox = (where: string): Box => ({
  where,
  avail: NAV_LABEL_AVAIL,
  type: { fontSize: navFont },
  wrap: wraps(NAV_LABEL_SELS),
  breakAnywhere: breaksAnywhere(NAV_LABEL_SELS),
  maxLines: wraps(NAV_LABEL_SELS) ? NAV_MAX_LINES : 1,
});

describe('③ 主窗侧栏（主 + 设置）：nav 标签与组头必须装得下', () => {
  it('主侧栏 nav 标签 × 5 语种', () => {
    const keys = sidebarNavKeys();
    expect(keys.length).toBeGreaterThanOrEqual(8);
    const box = navBox('S1 主侧栏');
    const over: Over[] = [];
    for (const loc of LOCALES)
      for (const k of keys) {
        const text = DICT[loc][k];
        if (!text) throw new Error(`${loc} 缺键 ${k}（locale-parity 该先红）`);
        check(over, box, loc, k, text);
      }
    expect(over.length, `主侧栏 nav 标签溢出：\n${fmt(over)}`).toBe(0);
  });

  it('设置侧栏 nav 标签 × 5 语种', () => {
    const keys = settingsNavKeys();
    expect(keys.length).toBeGreaterThanOrEqual(7);
    const box = navBox('S2 设置侧栏');
    const over: Over[] = [];
    for (const loc of LOCALES)
      for (const k of keys) {
        const text = DICT[loc][k];
        if (!text) throw new Error(`${loc} 缺键 ${k}`);
        check(over, box, loc, k, text);
      }
    expect(over.length, `设置侧栏 nav 标签溢出：\n${fmt(over)}`).toBe(0);
  });

  it('两侧栏组头 .nav-group × 5 语种（10px + .1em 字距 + uppercase）', () => {
    const keys = navGroupKeys();
    expect(keys.length).toBeGreaterThanOrEqual(4);
    const wrap = wraps(NAV_GROUP_SELS);
    const box: Box = {
      where: 'S3 组头',
      avail: NAV_GROUP_AVAIL,
      type: { fontSize: grpFont, letterSpacingEm: grpLs, uppercase: grpUpper },
      wrap,
      breakAnywhere: breaksAnywhere(NAV_GROUP_SELS),
      maxLines: wrap ? NAV_GROUP_MAX_LINES : 1,
    };
    const over: Over[] = [];
    for (const loc of LOCALES)
      for (const k of keys) {
        const text = DICT[loc][k];
        if (!text) throw new Error(`${loc} 缺键 ${k}`);
        check(over, box, loc, k, text);
      }
    expect(over.length, `nav-group 组头溢出：\n${fmt(over)}`).toBe(0);
  });

  it('修法必须仍在位：两处标签都得允许折行 + 允许词内断（否则回归成「画到侧栏外」）', () => {
    expect(wraps(NAV_LABEL_SELS), 'nav 标签又变回 nowrap 了').toBe(true);
    expect(breaksAnywhere(NAV_LABEL_SELS), 'nav 标签缺 overflow-wrap ⇒ 超宽单词仍会溢出').toBe(true);
    expect(wraps(NAV_GROUP_SELS), 'nav-group 又变回 nowrap 了').toBe(true);
    expect(breaksAnywhere(NAV_GROUP_SELS), 'nav-group 缺 overflow-wrap').toBe(true);
  });
});

describe('④ 主窗连接表：定宽列的表头必须装得下（th 无 ellipsis，撑破即列宽失效）', () => {
  it('定宽列表头 × 5 语种', () => {
    const cols = connTableFixedCols();
    expect(cols.length).toBeGreaterThanOrEqual(4);
    const sels = ['.conn-table th, .conn-table td', '.conn-table th'];
    const wrap = wraps(sels);
    const over: Over[] = [];
    let gated = 0;
    for (const { key, cls } of cols) {
      const cap = colCapPx(cls);
      // width:1px 的列（c-close / c-type）是 auto 表布局的「缩到内容宽」写法，不是宽度上限 ⇒ 不设门。
      if (cap === null || cap <= 2) continue;
      gated++;
      const box: Box = {
        where: `S4 ${cls}(${cap}px)`,
        avail: cap - thPadX - SORT_AR_W,
        type: { fontSize: thFont, letterSpacingEm: thLs, uppercase: thUpper },
        wrap,
        breakAnywhere: breaksAnywhere(sels),
        maxLines: wrap ? 2 : 1,
      };
      for (const loc of LOCALES) {
        const text = DICT[loc][key];
        if (!text) throw new Error(`${loc} 缺键 ${key}`);
        check(over, box, loc, key, text);
      }
    }
    expect(gated, '一个定宽列都没识别到 —— index.css 的 .conn-table .c-* 定宽段被删了？').toBeGreaterThanOrEqual(4);
    expect(over.length, `连接表定宽列表头溢出：\n${fmt(over)}`).toBe(0);
  });
});

/**
 * 托盘键 → 它实际渲染在哪个受限容器。
 *
 * 为什么要这张表：同一批键渲染在**四种**不同几何的槽里（12.5px 菜单行 / 10px uppercase 组头 /
 * 10.5px nowrap 副标题 / 11px 提示条）。旧版本把所有标签一律按菜单行量，对组头与提示条是**误报口径**，
 * 对副标题又是**漏报口径**（副标题字号更小但 nowrap 单行，约束方向完全不同）。
 *
 * 未登记的键落 `ROW`（最保守的那档：带右侧勾/箭头时只剩 161px）。下面那条穷尽性断言保证
 * **新增 `tray.*` 键必须在这里表态**——否则转红，不会有键悄悄溜进「反正默认能过」的缝里。
 */
const TRAY_SLOT: Record<string, 'ROW' | 'GROUP_H' | 'STATUS_TITLE' | 'STATUS_SUB' | 'NOTE'> = {
  // `.tray-group-h`（10px + .06em 字距 + uppercase）
  'tray.nodesRecent': 'GROUP_H',
  'tray.nodes': 'GROUP_H',
  'tray.groupMode': 'GROUP_H',
  'tray.groupTakeover': 'GROUP_H',
  // `.tray-status b`（12.5px，与副标题同槽宽，可折行）
  'tray.statusConnected': 'STATUS_TITLE',
  'tray.statusProxyInactive': 'STATUS_TITLE',
  'tray.statusConnecting': 'STATUS_TITLE',
  'tray.statusError': 'STATUS_TITLE',
  'tray.statusDisconnected': 'STATUS_TITLE',
  // `.tray-status div`（10.5px nowrap+ellipsis）—— `nodeName` 那一格的三个非用户数据取值。
  // `modeDirect` / `blocked` 同时也是菜单行，两个槽都要过（见下方 slotsOf）。
  'tray.noNode': 'STATUS_SUB',
  // `.tray-note`（11px，break-word，高度自适应）
  'tray.fakeIpAutoEnabled': 'NOTE',
  'tray.noTestableNodes': 'NOTE',
  'tray.checkingUpdate': 'NOTE',
  'tray.upToDate': 'NOTE',
  'tray.updateCheckFailed': 'NOTE',
};
/** 两栖键：既是菜单行、又会出现在状态卡副标题里。 */
const TRAY_DUAL_SLOT = new Set(['tray.modeDirect', 'tray.blocked']);

describe('⑤ 托盘浮层（独立 webview，五语种）', () => {
  const rowSels = ['.tray-i', '.tray-menu .tray-i'];
  const subSels = ['.tray-status div'];
  const grpSels = ['.tray-group-h', '.tray-menu .tray-group-h'];
  const noteSels = ['.tray-note', '.tray-menu .tray-note'];
  const rowWrap = wraps(rowSels);
  const BOXES: Record<string, Box> = {
    ROW: {
      where: 'S6 托盘行',
      avail: TRAY_ROW_TRAILING_AVAIL,
      type: { fontSize: trayFont },
      wrap: rowWrap,
      breakAnywhere: breaksAnywhere(rowSels),
      maxLines: rowWrap ? 2 : 1,
    },
    GROUP_H: {
      where: 'S9 托盘组头',
      avail: TRAY_GROUP_AVAIL,
      type: { fontSize: trayGrpFont, letterSpacingEm: trayGrpLs, uppercase: trayGrpUpper },
      wrap: wraps(grpSels),
      breakAnywhere: breaksAnywhere(grpSels),
      maxLines: wraps(grpSels) ? 2 : 1,
    },
    STATUS_TITLE: {
      where: 'S7b 托盘状态标题',
      avail: TRAY_STATUS_SUB_AVAIL,
      type: { fontSize: trayStTitleFont },
      wrap: true,
      breakAnywhere: false,
      maxLines: 2,
    },
    STATUS_SUB: {
      where: 'S7 托盘状态副标题',
      avail: TRAY_STATUS_SUB_AVAIL,
      type: { fontSize: trayStSubFont },
      wrap: wraps(subSels),
      breakAnywhere: breaksAnywhere(subSels),
      maxLines: 1,
    },
    NOTE: {
      where: 'S10 托盘提示条',
      avail: TRAY_NOTE_AVAIL,
      type: { fontSize: trayNoteFont },
      wrap: wraps(noteSels),
      breakAnywhere: breaksAnywhere(noteSels),
      /*
       * 提示条**横向不可能溢出**（`word-break:break-word`），故这里钉的是**纵向行数**。
       *
       * 预算 6 怎么来的（不是「调到能过为止」）：浮层窗高由前端量完回报、Rust 侧
       * `tray_resize` 夹在 **[80, 720]**（`src-tauri/src/tray.rs`），建窗初始高 420。
       * ⇒ 提示条最多可用 ≈ 720 − 420 = 300px；11px 字号 × 1.45 行高 ≈ 16px/行 ⇒ 物理上限 ≈ 18 行。
       * 6 行 ≈ 96px，远在上限内，同时**卡在当前最长译文那一格**（ru 模型口径 6 行、fa 5 行、
       * en-US 4 行、zh 2 行）⇒ 再长一句就转红，仍是棘轮。
       *
       * 注意本条是 2026-07-31 **新增**的覆盖：旧版本这条提示压根没进门（旧解析器只认单行
       * `t('zh','en')`，而 FakeIP 这条是跨行写的），不是「把 4 放宽成 6」。
       */
      maxLines: 6,
    },
  };
  const slotsOf = (key: string): Box[] =>
    TRAY_DUAL_SLOT.has(key)
      ? [BOXES.ROW, BOXES.STATUS_SUB]
      : [BOXES[TRAY_SLOT[key] ?? 'ROW']];

  it('每个 tray.* 键都被消费、且都在槽位表里表过态（防新键从缝里溜走）', () => {
    const used = trayKeys();
    expect(used.length, '托盘键解析数量异常偏低 —— 渲染方式变了？').toBeGreaterThanOrEqual(30);
    const declared = Object.keys(DICT['en-US'])
      .filter((k) => k.startsWith('tray.'))
      .sort();
    // 双向对差：locale 里有、代码没消费 = 死键；代码用了、locale 没有 = 漏译（locale-parity 也会红）。
    expect([...used].sort(), 'tray.* 键集与 locale 不一致').toEqual(declared);
    const unslotted = declared.filter(
      (k) => !(k in TRAY_SLOT) && !TRAY_DUAL_SLOT.has(k),
    );
    // 未登记的走 ROW 默认档是允许的，但必须是**有意**的：这里只断言默认档确实量过（下面那条 it 覆盖）。
    expect(unslotted.every((k) => slotsOf(k).length > 0)).toBe(true);
  });

  it('全部 tray.* 文案 × 5 语种 × 各自的槽必须装得下', () => {
    const over: Over[] = [];
    for (const key of trayKeys())
      for (const loc of LOCALES) {
        const text = DICT[loc][key];
        if (!text) throw new Error(`${loc} 缺键 ${key}（locale-parity 该先红）`);
        for (const box of slotsOf(key)) check(over, box, loc, key, text);
      }
    expect(over.length, `托盘文案溢出：\n${fmt(over)}`).toBe(0);
  });

  it('托盘状态副标题的 ellipsis 是刻意的（长节点名走截断而非画到卡外）', () => {
    expect(clips(subSels), '.tray-status div 的 ellipsis 没了 ⇒ 长文案会画到卡外').toBe(true);
  });
});

/**
 * ⑦ 首页右列 seg2 —— 「基类无约束、变体有约束」那一类（见文件头「S9 是一类形态」）。
 *
 * 本节同时是 `:lang()` 名单的**自维护门**：名单不由人记，由「各语种横排所需容器宽 vs 最小容器宽」
 * 逐条推出来。加第六个语种、改一条翻译、改这条几何链上的任何一个数，都会在这里转红并直接给出该填的阈值。
 */
describe('⑦ 首页右列 seg2（.cc-col.right .seg-wrap .seg2）：横排装不下的语种必须在 :lang() 名单里', () => {
  const groups = homeSegGroups();
  /** 各语种「横排所需最小容器宽」。 */
  const needOf = (loc: Locale) =>
    Math.max(
      ...groups.map(({ keys }) =>
        containerFromTrack(
          trackNeededFor(
            keys.map((k) => {
              const t = DICT[loc][k];
              if (!t) throw new Error(`${loc} 缺键 ${k}（locale-parity 该先红）`);
              return t;
            }),
          ),
        ),
      ),
    );

  it('几何链必须从 CSS / tauri.conf.json 现场解出（这些数变了，下面的阈值全部要重推）', () => {
    expect(groups.length).toBe(2);
    expect(groups.every((g) => g.keys.length === 3)).toBe(true);
    expect(TAURI_MIN_WIDTH, 'tauri.conf.json 的 minWidth 变了 → 重新过一遍 :lang() 名单').toBe(925);
    expect(CONTAINER_MIN).toBe(925 - 148); // 777
    expect(CC_RIGHT_SHARE).toBeCloseTo(1 / 3, 6); // 1.7 / (3.4+1.7)
    expect(screenPadX + cardBorderX + connCardPadX).toBe(48 + 2 + 32); // 82
    expect(trackFromContainer(CONTAINER_MIN)).toBeCloseTo(211.667, 2);
  });

  it('修法的前提仍在位：轨道被钉成 100%、三档均分、按钮 nowrap 且无 ellipsis', () => {
    // 这四条中任何一条没了，「溢出」的形态就变了（例如加了 ellipsis 就成了截断），
    // 本节的算式与三个阈值都得重新推 —— 故让它们的消失直接转红，而不是继续用旧算式发绿。
    expect(dupAgreed('.seg-wrap .seg2', 'width')).toBe('100%');
    expect(dupAgreed('.seg-wrap .seg2 button', 'flex')).toBe('1');
    expect(wraps(['.seg2 button', '.seg-wrap .seg2 button']), '.seg2 button 不再是 nowrap').toBe(false);
    expect(clips(['.seg2 button', '.seg-wrap .seg2 button']), '.seg2 button 有了 ellipsis').toBe(false);
  });

  it('横排装不下的语种必须登记，且阈值不得低于其所需容器宽（阳性方向）', () => {
    const rules = segStackRules();
    const bad: string[] = [];
    for (const loc of LOCALES) {
      const need = needOf(loc);
      if (need <= CONTAINER_MIN) continue; // 任何窗口都放得下 → 无需登记
      const hit = rules.filter((r) => langMatches(r.lang, loc));
      if (hit.length === 0) {
        bad.push(
          `  ${loc}：横排需容器 ${need.toFixed(1)}px > 最小容器 ${CONTAINER_MIN}px，但 index.css 的 :lang() 名单里没有它` +
            ` —— 补 \`@container mainc (max-width:${Math.ceil(need)}px){ :lang(<tag>) .cc-col.right .seg-wrap .seg2{flex-direction:column} }\``,
        );
        continue;
      }
      // 阈值 T 生效于 C ≤ T ⇒ 横排只发生在 C > T ⇒ 需 T ≥ 所需宽。多条命中取最松的那条判。
      const loosest = Math.min(...hit.map((r) => r.maxWidth));
      if (loosest + 1e-6 < need)
        bad.push(
          `  ${loc}：阈值 ${loosest}px < 所需 ${need.toFixed(1)}px ⇒ 容器在 (${loosest}, ${need.toFixed(1)}] 之间会横排且溢出` +
            ` —— 阈值至少要 ${Math.ceil(need)}px`,
        );
    }
    expect(bad.length, `seg2 :lang() 名单不完整/阈值过小：\n${bad.join('\n')}`).toBe(0);
  });

  it('名单里不得有多余语种，也不得有对不上任何 locale 的标签（反向）', () => {
    const rules = segStackRules();
    expect(rules.length, 'index.css 里一条 seg2 转纵向规则都没解析到 —— 修法被删？').toBeGreaterThan(0);
    const bad: string[] = [];
    for (const r of rules) {
      const matched = LOCALES.filter((l) => langMatches(r.lang, l));
      if (matched.length === 0) {
        bad.push(`  :lang(${r.lang}) 对不上任何 locale 文件 —— 语种被删/改名后没清理？`);
        continue;
      }
      const worst = Math.max(...matched.map(needOf));
      if (worst <= CONTAINER_MIN)
        bad.push(
          `  :lang(${r.lang}) 已不需要：它命中的 ${matched.join('/')} 横排只需 ${worst.toFixed(1)}px ≤ 最小容器 ${CONTAINER_MIN}px` +
            `（翻译改短了？）—— 删掉这条，让该语种回到原型的横排基线`,
        );
      else if (r.maxWidth > worst + 200)
        bad.push(
          `  :lang(${r.lang}) 阈值 ${r.maxWidth}px 比所需 ${worst.toFixed(1)}px 宽出 ${(r.maxWidth - worst).toFixed(0)}px` +
            ` —— 白白在放得下的窗口里也堆叠（右列会长高），收到 ${Math.ceil(worst)}px 附近`,
        );
    }
    expect(bad.length, `seg2 :lang() 名单有冗余：\n${bad.join('\n')}`).toBe(0);
  });

  it('兜底档必须真的兜得住：纵向堆叠后单档独占整条轨道，每个标签都要装得下', () => {
    // 堆叠是本修法的唯一退路。若某个标签连整条轨道都放不下，堆叠也救不了它（那就得回到改文案/改布局）。
    const avail = trackFromContainer(CONTAINER_MIN) - segBorderX - segPadX - segBtnPadX;
    const over: string[] = [];
    for (const loc of LOCALES)
      for (const { keys } of groups)
        for (const k of keys) {
          const w = textPx(DICT[loc][k], { fontSize: segBtnFont });
          if (w > avail)
            over.push(`  ${loc}/${k} "${DICT[loc][k]}" 需 ${w.toFixed(1)}px / 纵向可用 ${avail.toFixed(1)}px`);
        }
    expect(over.length, `纵向堆叠后仍装不下：\n${over.join('\n')}`).toBe(0);
  });
});

/**
 * 更新弹窗按钮行的行数预算 —— 为什么是 2。
 *
 * 窗高由 Rust `popup_height_for` 按 phase 定死（remind 184 / error 152，1:1 对齐上游，不动）。
 * remind 卡内纵向账（`style.css` 现值）：padding 14×2 + 标题 14×1.5 + 副标题 12×1.5 + 三道 gap 8
 * + 按钮行 (13×1.5 + 6×2 + 1×2) ≈ 124.5px ⇒ 184 − 124.5 = **59.5px 余量**。
 * 一行按钮 = 33.5px；第二行再要 33.5 + 8(gap) = 41.5px < 59.5 ⇒ 装得下。
 * **第三行**再要 41.5px，累计 83px > 59.5px ⇒ 会被 `body{overflow:hidden}` 裁掉，用户点不到按钮。
 * 故 2 行是硬预算，不是拍脑袋。
 *
 * ⚠️ **本段原先的算式是错的（2026-08-05 订正）**：它写「第二行的 41.5px 由 `.notes` 让出（
 * `min-height:0` + 截断区）」，并为此在下面立了一条断言 `.notes` 的 `min-height` 的门。
 * 但 `.notes` **从来没有被渲染过** —— `UpdatePopupState.notes` 自建档（`2076e86`）起从未被任何
 * 代码赋值（全历史 `notes: Some` 与 `.notes =` 两种形式均零命中），且带 `skip_serializing_if`
 * ⇒ 它压根没进过 DOM。那条「让位」机制一次都没运行过，那道门守的是一个不存在的元素。
 * 真实情况反而更宽松：`.notes` 不占位 ⇒ 余量是完整的 59.5px，第二行本来就装得下。
 * 该字段与它的 CSS 已随 #311 一并删除，本段改为直接对着**窗高减内容**这个真账记。
 */
const POPUP_ROW_MAX_LINES = 2;

describe('⑥ 更新弹窗（380px 独立窗，五语种，`.row` 可换行）', () => {
  it('`.row` 必须允许换行（否则俄语按钮直接画到窗外）', () => {
    // 这是上面那笔容器修法的锁：`flex-wrap` 被删回 nowrap 时本条转红。
    expect(popupDecl('.row', 'flex-wrap')).toBe('wrap');
    // 原先这里还断言 `.notes{min-height:0}`「为换行让位」——已删：那个元素从不渲染，
    // 门守的是不存在的东西（理由见上方 POPUP_ROW_MAX_LINES 的订正段）。
    // 2 行预算的**真**门在下面那条按宽度算行数的用例，不在这里。
  });

  it('单个按钮不得超过卡片内宽（换行也救不了的那种撑破）', () => {
    const bad: string[] = [];
    for (const { phase, keys } of updatePopupButtonRows())
      for (const key of keys)
        for (const loc of LOCALES) {
          const text = DICT[loc][key];
          if (!text) throw new Error(`${loc} 缺键 ${key}`);
          const w = textPx(text, { fontSize: popupFont }) + popupBtnPadX + popupBtnBorder;
          if (w > POPUP_ROW_AVAIL)
            bad.push(`  ${phase} ${loc} ${key} "${text}" 需 ${w.toFixed(1)}px / 可用 ${POPUP_ROW_AVAIL.toFixed(1)}px`);
        }
    expect(bad.length, `更新弹窗单个按钮撑破卡宽：\n${bad.join('\n')}`).toBe(0);
  });

  it(`同一 .row 的按钮换行后不得超过 ${POPUP_ROW_MAX_LINES} 行（第 3 行会被固定窗高裁掉）`, () => {
    const rows = updatePopupButtonRows();
    expect(rows.length, '一个按钮行都没解析到 —— render() 的渲染方式变了？').toBeGreaterThanOrEqual(3);
    expect(
      rows.reduce((n, r) => n + r.keys.length, 0),
      '按钮键解析数量异常偏低',
    ).toBeGreaterThanOrEqual(8);
    const bad: string[] = [];
    for (const { phase, keys } of rows) {
      if (keys.length === 0) continue;
      for (const loc of LOCALES) {
        const ws = keys.map(
          (k) => textPx(DICT[loc][k], { fontSize: popupFont }) + popupBtnPadX + popupBtnBorder,
        );
        // flex-wrap 的贪心装箱：装不下就起新行。
        let lines = 1;
        let cur = 0;
        for (const w of ws) {
          const next = cur === 0 ? w : cur + popupRowGap + w;
          if (next > POPUP_ROW_AVAIL) {
            lines++;
            cur = w;
          } else cur = next;
        }
        if (lines > POPUP_ROW_MAX_LINES)
          bad.push(
            `  ${phase} ${loc} [${keys.map((k) => DICT[loc][k]).join(' / ')}] ${lines} 行 / 预算 ${POPUP_ROW_MAX_LINES}`,
          );
      }
    }
    expect(bad.length, `更新弹窗按钮行数超预算：\n${bad.join('\n')}`).toBe(0);
  });
});

/**
 * ⑧ 节点弹窗表单字段（S11）—— `0b0c186` 铺进来的 95 个 `node.field.*` 键 × 5 语种 = 475 条译文；
 * 2026-08-07 拆标签又添 4 条 hint（`h2Host/h2Method/h2Headers/secretKeys` 的括号说明搬出标签），
 * 现为 99 键 × 5 = 495 条。
 *
 * 为什么之前不在门里：这批文案 2026-08-06 之前**根本不存在**（`node-spec.ts` 走
 * `t(key, zhDefault)`，locale 里只有 `noParrot`/`noParrotHint` 两条，其余 93 键在任何语种下都回落
 * 到同一句中文）—— 中文短，量不量都绿。补齐五语之后最长的一条 ru hint 已到 240 字符，
 * 「装不装得下」这才第一次成为一个真问题，而文件头「覆盖了什么」那张表里从来没有 `.fld` 一族
 * （e. 条列的是 `.app-pol`/`.ctx-note`/`.lock-field`/`.node-menu` 那批，**没提过节点弹窗**）
 * ⇒ 它不是「判过不进门」，是**一直没被看见**。
 */
describe('⑧ 节点弹窗表单字段（.dlg 460px 定宽，五语种，`.fld-l` / `.fld-hint` 可折行）', () => {
  const labelWrap = wraps(FLD_LABEL_SELS);
  const hintWrap = wraps(FLD_HINT_SELS);
  const BOXES: Record<FldSlot, (section: 'cred' | 'adv') => Box> = {
    LABEL: (section) => ({
      where: `S11 ${section} 字段标签`,
      avail: section === 'cred' ? FLD_AVAIL : FOLD_AVAIL,
      type: { fontSize: fldLabelFont },
      wrap: labelWrap,
      breakAnywhere: breaksAnywhere(FLD_LABEL_SELS),
      maxLines: labelWrap ? FLD_MAX_LINES.LABEL : 1,
    }),
    HINT: (section) => ({
      where: `S11 ${section} 下拉说明`,
      avail: section === 'cred' ? FLD_AVAIL : FOLD_AVAIL,
      type: { fontSize: fldHintFont },
      wrap: hintWrap,
      breakAnywhere: breaksAnywhere(FLD_HINT_SELS),
      maxLines: hintWrap ? FLD_MAX_LINES.HINT : 1,
    }),
    SWT_LABEL: (section) => ({
      where: `S11 ${section} 开关标签`,
      avail: swtTextAvail(section === 'cred' ? FLD_AVAIL : FOLD_AVAIL),
      type: { fontSize: swtLabelFont },
      wrap: labelWrap,
      breakAnywhere: breaksAnywhere(FLD_LABEL_SELS),
      maxLines: labelWrap ? FLD_MAX_LINES.SWT_LABEL : 1,
    }),
    SWT_HINT: (section) => ({
      where: `S11 ${section} 开关说明`,
      avail: swtTextAvail(section === 'cred' ? FLD_AVAIL : FOLD_AVAIL),
      type: { fontSize: fldHintFont },
      wrap: hintWrap,
      breakAnywhere: breaksAnywhere(FLD_HINT_SELS),
      maxLines: hintWrap ? FLD_MAX_LINES.SWT_HINT : 1,
    }),
  };

  it('几何链必须从 CSS / tauri.conf.json 现场解出（这些数变了，下面的行数预算全部要重推）', () => {
    expect(DLG_W, '.dlg 宽变了 → 重新标定 FLD_MAX_LINES').toBe(460);
    expect(FLD_AVAIL).toBeCloseTo(460 - 2 - 36, 5); // 422
    expect(FOLD_AVAIL).toBeCloseTo(422 - 2 - 26, 5); // 394
    expect(swtTextAvail(FOLD_AVAIL)).toBeCloseTo(394 - 12 - 36, 5); // 346
    expect([fldLabelFont, fldHintFont, swtLabelFont, fldOptFont]).toEqual([11.5, 10.5, 12.5, 10]);
    // `.fld-opt` 在 prototype.css 另存一份同名规则（:933），两份必须同值 —— 本仓经典坑。
    expect(px(decl('./prototype.css', '.fld-opt', 'font-size'))).toBe(fldOptFont);
  });

  it('修法前提仍在位：可折行、无 ellipsis、无 overflow-wrap ⇒ 两条判据都必须活着', () => {
    expect(labelWrap, '.fld-l / .swt-tx b 变成了 nowrap ⇒ 判据要改回「单行宽度」那一类').toBe(true);
    expect(hintWrap, '.fld-hint 变成了 nowrap ⇒ 判据要改回「单行宽度」那一类').toBe(true);
    expect(clips(FLD_LABEL_SELS), '字段标签有了 ellipsis —— 标签被截断正是这类控件最要避免的事').toBe(
      false,
    );
    expect(clips(FLD_HINT_SELS), '.fld-hint 有了 ellipsis ⇒ 说明会被截断').toBe(false);
    // 加 `overflow-wrap:anywhere` 本身是改进（长串不再顶出横向滚动条），但那会让「硬判据」整条失效，
    // 本节就只剩棘轮了 —— 故让它的出现也转红，逼人来这里改口径，而不是悄悄弱化一道门。
    expect(
      breaksAnywhere(FLD_HINT_SELS),
      '.fld-hint 多了 overflow-wrap/word-break ⇒ 硬判据（不可断长串宽度）失效，请重写本节口径',
    ).toBe(false);
  });

  it('「可选」徽标按 i18next 的三级回落取值，缺 locale 时用 en-US 而不是代码里的 zh 缺省', () => {
    // 为什么不直接断言 `optTail('ru')`：五份 locale 现在都有这个键，那条断言永远停在第一级，
    // 对「后两级排序对不对」零信息量。故把两级缺席态显式喂进去。
    const en = DICT['en-US']['common.optional'];
    expect(en, 'en-US 丢了 common.optional ⇒ 下面整条回落链的前提没了').toBeTruthy();
    expect(optTailOf(undefined, en), '缺 locale 时必须回落到 en-US —— i18next 里 fallbackLng 排在 defaultValue 之前').toBe(
      en,
    );
    expect(optTailOf(undefined, undefined), '整条语言链都没有该键时才轮到代码里的 zh 缺省').toBe(
      OPT_ZH_DEFAULT,
    );
    expect(OPT_ZH_DEFAULT, '兜底值与 en-US 撞了 ⇒ 上面两条断言退化成同一条，失去区分力').not.toBe(en);
  });

  it('每个 node.field.* 键都被 ND_SPEC 消费、也都被本门量过（双向对差，防死键与漏测）', () => {
    const points = nodeFieldPoints();
    expect(points.length, 'ND_SPEC 摊出来的测点异常偏低 —— 表结构变了？').toBeGreaterThanOrEqual(90);
    const used = [...new Set(points.map((p) => p.key))].sort();
    const declared = Object.keys(DICT['en-US'])
      .filter((k) => k.startsWith('node.field.'))
      .sort();
    // locale 里有、ND_SPEC 没消费 = 死键；ND_SPEC 用了、locale 没有 = 漏译（i18n 门也该先红）。
    expect(used, 'node.field.* 键集与 locale 对不上').toEqual(declared);
    expect(declared.length).toBeGreaterThanOrEqual(99);
  });

  it('全部 node.field.* 文案 × 5 语种 × 各自的槽必须装得下', () => {
    const over: Over[] = [];
    let n = 0;
    for (const p of nodeFieldPoints()) {
      const box = BOXES[p.slot](p.section);
      for (const loc of LOCALES) {
        const text = DICT[loc][p.key];
        if (!text) throw new Error(`${loc} 缺键 ${p.key}（locale-parity 该先红）`);
        n++;
        check(over, { ...box, tailPx: p.opt ? optTailPx(loc) : 0 }, loc, p.key, text);
      }
    }
    expect(n, '测点总数异常偏低 —— 语种或字段少了一批？').toBeGreaterThanOrEqual(495);
    expect(over.length, `节点弹窗表单字段溢出：\n${fmt(over)}`).toBe(0);
  });

  /**
   * 阳性对照 —— 一道加进来却永远不会红的门比没有更坏。这里对**两条判据各造一个已知缺陷**，
   * 断言 `check()` 都抓得到，且报文里带得出槽/键/超出量。用合成串而不是改真译文：
   * 改真译文的对照做完就得还原，还原漏了就变成静默篡改语料。
   */
  it('阳性对照：两条判据各造一个已知缺陷，门必须都抓到', () => {
    const box = BOXES.SWT_HINT('adv'); // 最窄的那档：346px
    const bucket: Over[] = [];

    // ① 硬判据：一个比字段还宽的**不可断**单元（长 token / URL 那一类）。
    const longToken = 'X'.repeat(60);
    check(bucket, box, 'ru', 'node.field.__probeWidth', longToken);
    // ② 棘轮判据：可正常折行、但行数超预算。
    const longSentence = 'слово '.repeat(400);
    check(bucket, box, 'ru', 'node.field.__probeLines', longSentence);

    expect(bucket.length, '合成的两个已知缺陷竟然没被抓到 —— 本门是装饰').toBe(2);
    const [w, l] = bucket;
    expect(w.key).toBe('node.field.__probeWidth');
    expect(w.need).toBeGreaterThan(box.avail); // 最宽行确实超了可用宽
    expect(l.key).toBe('node.field.__probeLines');
    expect(l.lines).toBeGreaterThan(FLD_MAX_LINES.SWT_HINT);
    // 报文必须指名槽 / 语种 / 键 / 超出量，否则红了也没法定位。
    const msg = fmt(bucket);
    expect(msg).toMatch(/S11 adv 开关说明/);
    expect(msg).toMatch(/\| ru \|/);
    expect(msg).toMatch(/node\.field\.__probeWidth/);
    expect(msg).toMatch(/最宽行 [\d.]+px \/ 可用 346\.0px/);
    expect(msg).toMatch(/\d+ 行 \/ 预算 12 行/);
  });

  /**
   * 阴性对照 —— 模型必须**认识 CJK 断点**，否则一整句无空格中文会被当成一个不可断的词、
   * 在任何窄容器里都判红（假红）。这是 2026-08-06 补 `breakUnits()` 的那个缺陷的定桩。
   */
  it('阴性对照：无空格中文长句必须能折行，不得被算成「不可断长串」', () => {
    const zh = DICT['zh-TW']['node.field.muxPadHint'];
    expect(zh).toContain('隨機填充'); // 语料换了就该重新对照，不许静默跟着变
    const box = BOXES.SWT_HINT('adv');
    const { lines, maxLineWidth } = layout(zh, box.avail, box.type, box);
    expect(lines, '中文长句在 346px 里必然要折行').toBeGreaterThan(1);
    expect(
      maxLineWidth,
      `整句被当成一个不可断的词了（${maxLineWidth.toFixed(1)}px > ${box.avail}px）—— breakUnits 的 CJK 断点没生效`,
    ).toBeLessThanOrEqual(box.avail);
  });
});

// ── ⑨ 导入弹窗解析结果预览（`.imp-*`）：几何链 ────────────────────────────────────────────
//
// `.imp-preview` 是 `.dlg-body` 的直接子项 ⇒ 与 `.fld` 同一个顶层槽（422px）。
// 它自己只有 `border-top` 与 `padding-top`，横向不再收窄。
const impStatFont = px(decl('./index.css', '.imp-stat', 'font-size')); // 10.5
const impStatPadX = padX(decl('./index.css', '.imp-stat', 'padding')); // 7*2 = 14
/** 计数 pill 的文本可用宽 = 422 − 7×2。`.imp-stats{flex-wrap:wrap}` ⇒ 一个 pill 最宽就是整幅。 */
const IMP_STAT_AVAIL = FLD_AVAIL - impStatPadX;

const impLiPadX = padX(decl('./index.css', '.imp-list > li', 'padding')); // 9*2 = 18
const impLiGap = px(decl('./index.css', '.imp-list > li', 'gap')); // 8
/** 清单行的内容宽 = 422 − 9×2。（`.imp-list` 只有 1px 边框，忽略不计入会让判据更严，故计入。） */
const impListBorderX = px(decl('./index.css', '.imp-list', 'border').split(/\s+/)[0]) * 2; // 1*2
const IMP_ROW_AVAIL = FLD_AVAIL - impListBorderX - impLiPadX;

const impBadgeFont = px(decl('./index.css', '.imp-badge', 'font-size')); // 10
const impBadgePadX = padX(decl('./index.css', '.imp-badge', 'padding')); // 6*2 = 12

const cardSubFont = px(decl('./components.css', '.card-sub', 'font-size')); // 11.5

/**
 * 名称轨的最小宽 —— **棘轮，钉在今天最差的那一格上**，不是拍出来的设计余量。
 *
 * `.imp-name` 带 ellipsis，徽标再宽也只会把名字截得更短、不会破版；所以这里没有物理墙可解，
 * 只有「够不够认出是哪条节点」这个可用性问题。既然没有物理墙，就不能拍一个宽松的数
 * —— 那样门永远不会红（第一版拍了 180，把 ru 的徽标换成整句 `Протокол не поддерживается`
 * 仍留 262px，照样全绿，等于没这道门）。
 *
 * 故取今天五语里最差的一格：ru `Не поддерживается` 占 154.6px ⇒ 名称轨剩 **239.4px**。
 * 钉在 239 = 「徽标再宽一格就自曝」，与 `FLD_MAX_LINES` 同一套语义。
 * 真要放宽，先回答「为什么这一行的主体可以更窄」，再动这个数。
 */
const IMP_NAME_MIN = 239;

// ════════════════════════════════════════════════════════════════════════════════════════

/**
 * ⑨ 导入弹窗的解析结果预览。
 *
 * # 为什么补这一节
 *
 * 这块界面（`feat(import)` 那批）落地时**没进本门的注册表** —— 而「注册表里没有 = 门看不见」
 * 正是本文件开头列的那类假绿：`.fld` 一族当初也是这么漏了整整一族的（S11 的由来）。
 * 五个语种的宽度一次都没被量过。
 *
 * # 量什么、不量什么
 *
 *  · `.imp-stat` 三颗计数 pill（已解析 / 不支持 / 已跳过）—— 整幅宽，可折行；
 *  · `.card-sub` 那行「将导入节点」小标题 —— 整幅宽，可折行；
 *  · `.imp-badge`「不支持」徽标 —— `flex:none` ⇒ 基础尺寸取 max-content 且**不收缩**，恒单行；
 *    它不会溢出（名称轨带 ellipsis 会替它让位），故判据换成「别把名称轨压到看不清」。
 *
 * **不量** `.imp-warn` 里的告警条目：那是后端 `ClashParseResult` 生成的运行期文本，
 * 不是 locale 里的键，本门的语料源里没有它。（它有 `line-height:1.55` 且可折行，纵向无上限。）
 * **不量** `.imp-name` / `.imp-proto`：前者是用户数据 + ellipsis，后者是协议标识符、非文案。
 *
 * # 插值按模板长度测（比实际更宽，故保守）
 *
 * 与本文件既有口径一致（射程自曝 b）。这里额外成立的是：`{{count}}` 这个模板串本身
 * 就比它会被替换成的三四位数字更宽（10.5px 下模板约 40px、`9999` 约 24px），
 * 故「按模板测」对这三条计数文案是**偏严**的一侧，不需要另造样例值。
 */
describe('⑨ 导入弹窗解析结果预览（.imp-*，.dlg 460px 定宽，五语种）', () => {
  /** 槽 → [键, 盒]。键必须真的还被 ImportDialog 消费（见下方自检）。 */
  const IMP_STAT_KEYS = ['import.parsed', 'import.resultUnsupported', 'import.skippedTitle'];
  const IMP_TITLE_KEY = 'import.nodesTitle';
  const IMP_BADGE_KEY = 'import.unsupportedBadge';

  const statWrap = wraps(['.imp-stats', '.imp-stat']);
  const titleWrap = wraps(['.card-sub']);

  const STAT_BOX: Box = {
    where: '⑨ 计数 pill（.imp-stat）',
    avail: IMP_STAT_AVAIL,
    type: { fontSize: impStatFont },
    wrap: statWrap,
    breakAnywhere: breaksAnywhere(['.imp-stats', '.imp-stat']),
    // 棘轮，钉在今天最差：en/ru/fa 的 `resultUnsupported` 与 `skippedTitle` **已经是 2 行**
    // （最差 397.5/408px）。2 不是「pill 应该占两行」的设计主张，是现状；第 3 行必须自曝。
    maxLines: statWrap ? 2 : 1,
  };
  const TITLE_BOX: Box = {
    where: '⑨ 清单小标题（.card-sub）',
    avail: FLD_AVAIL,
    type: { fontSize: cardSubFont },
    wrap: titleWrap,
    breakAnywhere: breaksAnywhere(['.card-sub']),
    // 五语今天全是 1 行（最差 ru 149/422px，余量一倍多）。给 2 行就等于给了一整行的免检额度。
    maxLines: 1,
  };

  it('几何链从 CSS 现场解出，且 `.card-sub` 的两份副本同值（本仓经典坑）', () => {
    expect(IMP_STAT_AVAIL).toBeCloseTo(422 - 14, 5);
    expect(IMP_ROW_AVAIL).toBeCloseTo(422 - 2 - 18, 5);
    expect([impStatFont, impBadgeFont, cardSubFont]).toEqual([10.5, 10, 11.5]);
    expect(px(decl('./prototype.css', '.card-sub', 'font-size')), '.card-sub 两份副本分叉').toBe(
      cardSubFont,
    );
  });

  it('本节量的键仍真的被 ImportDialog 消费（改名/删控件后本节不得继续量死键）', () => {
    const dlg = src('../components/dialogs/ImportDialog.tsx');
    for (const k of [...IMP_STAT_KEYS, IMP_TITLE_KEY, IMP_BADGE_KEY]) {
      expect(dlg.includes(`t('${k}'`), `ImportDialog 已不再消费 ${k} —— 本节的注册表要跟着改`).toBe(
        true,
      );
    }
    // 语料侧同理：键不在 locale 里的话下面 `DICT[loc][key]` 会是 undefined 而静默跳过。
    for (const k of [...IMP_STAT_KEYS, IMP_TITLE_KEY, IMP_BADGE_KEY]) {
      for (const loc of LOCALES) {
        expect(DICT[loc][k], `${loc} 缺键 ${k}（i18n 门该先红）`).toBeTruthy();
      }
    }
  });

  it('计数 pill 与清单小标题 × 5 语种必须装得下', () => {
    const over: Over[] = [];
    let n = 0;
    for (const loc of LOCALES) {
      for (const key of IMP_STAT_KEYS) {
        n++;
        check(over, STAT_BOX, loc, key, DICT[loc][key]);
      }
      n++;
      check(over, TITLE_BOX, loc, IMP_TITLE_KEY, DICT[loc][IMP_TITLE_KEY]);
    }
    expect(n, '测点数异常偏低 —— 语种或键少了？').toBe(LOCALES.length * 4);
    expect(over.length, `导入结果预览溢出：\n${fmt(over)}`).toBe(0);
  });

  it('「不支持」徽标不得把节点名压到看不清（徽标不收缩，名字才是这一行的主体）', () => {
    const tight: string[] = [];
    for (const loc of LOCALES) {
      const badgeW = textPx(DICT[loc][IMP_BADGE_KEY], { fontSize: impBadgeFont }) + impBadgePadX;
      const nameW = IMP_ROW_AVAIL - impLiGap - badgeW;
      if (nameW < IMP_NAME_MIN)
        tight.push(
          `  ${loc} | ${IMP_BADGE_KEY}="${DICT[loc][IMP_BADGE_KEY]}" | 徽标 ${badgeW.toFixed(1)}px` +
            ` ⇒ 名称轨只剩 ${nameW.toFixed(1)}px（下限 ${IMP_NAME_MIN}px）`,
        );
    }
    expect(tight.sort(), '徽标过宽，节点名会被 ellipsis 吃掉大半').toEqual([]);
  });


  /** 阳性对照 —— 两条判据各造一个已知缺陷，门必须都抓到。用合成串，不动真语料。 */
  it('阳性对照：过长的计数文案与过宽的徽标都必须被抓到', () => {
    const bucket: Over[] = [];
    // ① 折行超预算（pill 折成 3 行 = 那一栏在视觉上不再是个「标签」）。
    check(bucket, STAT_BOX, 'ru', 'import.__probeLines', 'слово '.repeat(120));
    // ② 不可断长串超宽。
    check(bucket, STAT_BOX, 'ru', 'import.__probeWidth', 'X'.repeat(200));
    expect(bucket.length, '合成的两个已知缺陷竟然没被抓到 —— 本节是装饰').toBe(2);
    expect(fmt(bucket)).toMatch(/⑨ 计数 pill/);
    expect(fmt(bucket)).toMatch(/import\.__probeWidth/);

    // ③ 徽标那条的阳性对照：把徽标从一个词换成一句话（这正是翻译最容易发生的事），
    //    名称轨必须掉到下限以下。第一版的 180px 下限对这句话仍然全绿，故连这条对照一起收紧。
    const fatBadge =
      textPx('Protocol not supported by the current core', { fontSize: impBadgeFont }) + impBadgePadX;
    expect(IMP_ROW_AVAIL - impLiGap - fatBadge).toBeLessThan(IMP_NAME_MIN);
    // 阴性对照：今天真在用的那个最差值必须**恰好**过关（下限不是随手挑的一个宽松数）。
    const worst = textPx('Не поддерживается', { fontSize: impBadgeFont }) + impBadgePadX;
    const worstName = IMP_ROW_AVAIL - impLiGap - worst;
    expect(worstName).toBeGreaterThanOrEqual(IMP_NAME_MIN);
    expect(worstName - IMP_NAME_MIN, '下限已不再贴着今天最差值 —— 语料变了就把它重新钉一次').toBeLessThan(2);
  });
});
