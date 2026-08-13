/**
 * 数据驱动表单基建 —— FieldSpec 描述符 + 单一 FieldRenderer（§1.4 表单层）。
 *
 * **可复用**：节点表单（D2，8 协议）+ 组网表单（D3）+ 规则表单（D4）共用此层，勿节点化。
 * 消灭 上游 `server-config-dialog.tsx:332-501` 那种「15 个 {proto==='X' && <XForm/>} 同构分支」——
 * 一张数据表（ND_SPEC/规则表）+ 一个渲染器 map 出字段，新协议/新字段只加表项不复制 JSX。
 *
 * 淬火机制（polaris-node-form-hardening-requirements.md，逐条落为结构而非自觉）：
 *  - **R2 单点 number 渲染**：`parseNumberField` 是全库唯一的 number 解析实现 —— 空串 → `undefined`
 *    （允许退格删空重录）、非空十进制解析、异常 → `undefined`（**绝不硬塞 0**）。所有 number 字段
 *    （port / keepalive / mtu / alterId / up·down 带宽 …）都走它，「15 个手写 `parseInt(x)||0`」时代
 *    的重犯面整类消失。
 *  - **R1 无 radix/RHF**：select 走 D1 `<Csel>`（受控、无懒挂 Portal、无伪 onValueChange），
 *    reset-race 根因整类不存在（配合 NodeDialog 的 `key` 重挂 + 同步初始化）。
 *
 * i18n：label/hint 存 i18n key，`zh`/`hintZh` 是**可选**的 zh 缺省，渲染走 `t(key, zhDefault)`
 * （对齐既有屏 `t('nodes.add','添加')` 的「键 + zh 默认」惯例）。
 *
 * **缺省是过渡态、不是终态**：给了 zh 缺省，「locale 里译错一条」与「locale 里漏一条」在界面上
 * 长得一模一样（都显示中文），5 个语种会被一起压回中文而没有任何门会红 —— `node.field.*` 那 95 条
 * 就是这么在 locale 铺齐前一直被 zh 挡住的。故键落地后应删掉缺省：`ND_SPEC` 已于 2026-08-07 删光
 * （`node-spec.ts` 文件头），`WgDialog`/`WarpDialog`/`TsSettingsDialog`/`RuleDialog` 尚有 36 个键
 * 没进 locale、仍靠缺省渲染中文，是同型存量债。守这两侧的是 `i18n/i18n-coverage.test.ts` 的 G5：
 * 「键在 en-US 里 **或** 同对象仍留着 zh 缺省」，两者皆无 ⇒ 红。
 *
 * select 选项文案多为专有名词（TCP/xtls-rprx-vision/…）直接字面量，不入 i18n。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Csel, type CselOption } from './Csel';

/** 表单草稿值域：文本/数值/开关/未填。number 空 = undefined（R2）。 */
export type FormValue = string | number | boolean | undefined;

/**
 * select 的一个选项：`[值, 文案]`，可选第三位 = **不可选**（省略 ⇒ 可选）。
 *
 * 为什么是「可选第三元素」而不是改成 `{value,label,disabled}` 对象：`Csel` 早就支持
 * `disabled`（点击拦截 `Csel.tsx:172`、键盘跳过 `:241`、样式与 `aria-disabled` `:315/318`），
 * 唯一断点就是本层把选项拍平成了二元组、表达不出禁用。而二元组字面量对
 * `[string, string, boolean?]` 天然可赋值 ⇒ 全仓 22 处 select 调用点的选项字面量**一处都没改**
 * （实测 `tsc --noEmit` 全绿），要禁用的那一处多写一位即可。改成对象则要么全量改写、
 * 要么两种形状并存。
 */
export type SelectOption = readonly [value: string, label: string, disabled?: boolean];

/** 表单草稿：键（FieldSpec.k）→ 值。协议特定字段的扁平袋，protoCodec 在此与 ServerConfig 往返。 */
export type FormValues = Record<string, FormValue>;

/** 字段描述符公共部分。`when` = 显隐谓词（通用：节点的 tls/reality 条件、规则的类型条件都用它）。 */
interface FieldBase {
  /** 草稿键（= protoCodec 读写的键）。 */
  k: string;
  /** 标签 i18n key。 */
  label: string;
  /**
   * 标签 zh 缺省（key 缺失时的渲染文案）—— **可选**。
   *
   * 曾是必填。改成可选是为了让「键已五语齐备」的表把它删干净（`ND_SPEC`），因为留着就是第二个
   * 真值源且看不出谁在生效（见文件头）。**类型放宽丢掉的那份保证由门接管**：
   * `i18n/i18n-coverage.test.ts` 的 G5b 要求每个 FieldSpec 的键要么在 en-US 里、要么仍有 zh 缺省，
   * 两者皆无 ⇒ 红（i18next 对缺失键返回**键本身**，界面上会出现一串点分标识符）。
   */
  zh?: string;
  /** 显隐谓词（返回 false = 该字段在当前草稿下隐藏）。缺省恒显。 */
  when?: (values: FormValues) => boolean;
  /** 「可选」徽标。 */
  opt?: boolean;
  /**
   * 控件下方的说明行（`.fld-hint`）—— **所有字段类型都有**（2026-08-07 从 select/switch 两支提上来）。
   *
   * 加这一支最初是因为「选了会怎样」有时**不能只靠标签表达**：endpoint 的前置代理是实例——
   * WireGuard 的握手走 UDP，前置代理不支持 UDP 转发就**静默不通**（不回落直连，见
   * `crates/config-engine/src/singbox/endpoint.rs` 的实测），而 Tailscale 那侧只需 TCP。
   * 两句话不同、都必须出现在控件旁，否则用户只能靠试。
   *
   * 提到 `FieldBase` 是因为 text/textarea 也有同样的需求，而此前它们**没有 hint 位**，于是说明
   * 只能塞进标签：`node.field.h2Host` = 「HTTP/2 Host（逗号分隔，留空回落 SNI/节点地址）」。
   * 这不是排版偏好问题 —— 标签是控件的**名字**，`styles/text-fit.test.ts` 给 `.fld-l` 定的 2 行预算
   * 正是这条判据的具象（占到第 3 行就说明它其实是一句说明），而这四条恰恰是把预算刚好用满的那批。
   * 「不为两个字段去扩 union」的旧结论（`node-spec.ts` h2 段的原注释）在字段涨到 4 条、且行数预算
   * 把它顶出来之后不再成立。
   */
  hint?: string;
  /** 说明行的 zh 缺省 —— 同 `zh`，可选且是过渡态，见文件头。 */
  hintZh?: string;
}

/**
 * 字段描述符（discriminated union，§1.4）。渲染器按 `t` 穷尽 switch。
 * 新增字段类型 → 加一支 union + 一个 case（never 兜底保证补全）。
 */
export type FieldSpec =
  | (FieldBase & { t: 'text'; ph?: string; mono?: boolean; secret?: boolean })
  | (FieldBase & { t: 'number'; ph?: string; mono?: boolean })
  | (FieldBase & { t: 'textarea'; ph?: string; mono?: boolean; rows?: number; secret?: boolean })
  | (FieldBase & { t: 'select'; options: readonly SelectOption[] })
  | (FieldBase & {
      t: 'switch';
      /**
       * 禁用态 —— **静态布尔，不是谓词**，由构表处算好传进来（同 `SelectOption` 第三位 `disabled`
       * 那条既定形态）。
       *
       * 为什么不做成 `when` 那样的谓词：`when` 是**调用方** filter 掉的
       * （`spec.filter(f => !f.when || f.when(draft))`），而 `FieldRenderer` 只收到单个 spec，
       * 拿不到整份草稿。要谓词就得再给渲染器传一个 `values` prop —— 那样「某个调用点忘了传」会把
       * 禁用**静默退化成可用**，而这个开关禁用与否是阻断级的（见 WarpDialog 的 `advSpec`）。
       * 静态值没有这条退化路径：表里写了就是写了。
       */
      disabled?: boolean;
      /**
       * 禁用时**取代** `hint` 的说明（讲「为什么不能开」）。缺省 ⇒ 仍显示 `hint`。
       * 之所以是取代而非追加：`hint` 描述的是开启后的行为，而那件事在禁用场景下结构上永远不会发生，
       * 照显等于对着一个拨不动的开关解释它拨动后会怎样。
       */
      disabledHint?: string;
      disabledHintZh?: string;
    });

/**
 * number 字段解析 —— **全库唯一实现点（R2）**。
 *  - 空串（含纯空白）→ `undefined`：允许退格删空重录，不被压成 0；
 *  - 非空 → 十进制解析；解析失败（NaN/Infinity）→ `undefined`，**绝不硬塞 0**。
 * 抽为纯函数供 NumberField 分支与 NodeDialog 的 port 字段共用（单一逻辑），并入 vitest。
 */
export function parseNumberField(raw: string): number | undefined {
  const s = raw.trim();
  if (s === '') return undefined;
  const n = Number(s);
  return Number.isFinite(n) ? n : undefined;
}

/**
 * select 选项元组 → `Csel` 选项对象 —— **抽成函数是为了让它可测**。
 *
 * 本仓 vitest 是 `environment:'node'`（无 jsdom），`FieldRenderer` 渲染不了 ⇒ 这条映射若内联在
 * 组件里，「少映一个字段」不会被任何门发现：类型不红（少写属性是合法的）、build 不红、
 * 渲染测不了。`disabled` 正是这么一路从 `Csel`（早就支持）断在这一层的。抽出来即可直测。
 *
 * **未知当前值保留（`current`）**：选项集是**前端选的展示档位**，而磁盘上的值域由 sing-box/后端拥有
 * 且更宽（ss `method`、uTLS `fingerprint`、`vmessSecurity` 等都是开放集，Rust 侧就是 `String`）。
 * 存量/订阅节点的值落在表外时，若照直渲染，下拉是空选中态，用户**一碰就被迫改成表内某档**——
 * 静默改坏一个本来能用的节点，且没有撤销入口。故当前值不在表里就把它并入首位（值即文案，同
 * 上游 ss-form 的 `sortedMethods.unshift(field.value)`）。放在这一层而不是给 ss 打补丁：
 * `fp` / `sec` / `enc` / `cc` / `obfs` … 每个 select 都是同一类风险，逐个补必然漏。
 * 空串不并入 —— 它是「未设置」而非未知取值，且多张表里 `''` 本身就是合法首项（flow=none / bbr=默认）。
 */
export function toCselOptions(options: readonly SelectOption[], current?: FormValue): CselOption[] {
  const opts = options.map(([value, label, disabled]) => ({ value, label, disabled }));
  if (typeof current === 'string' && current !== '' && !opts.some((o) => o.value === current)) {
    opts.unshift({ value: current, label: current, disabled: undefined });
  }
  return opts;
}

export interface FieldRendererProps {
  spec: FieldSpec;
  value: FormValue;
  onChange: (value: FormValue) => void;
}

/**
 * 唯一字段渲染器。按 spec.t 分派；enum → `<Csel>`（非原生 select、非 radix），number → 单点 R2 分支。
 * 类名对齐原型（`.fld`/`.fld-l`/`.input`/`.swt-row` 等）→ 样式复用 components.css 无需新写。
 */
export function FieldRenderer({ spec, value, onChange }: FieldRendererProps) {
  const { t } = useTranslation();
  const [secretVisible, setSecretVisible] = useState(false);
  /**
   * `t(key, zh?)` —— zh 缺省可选之后必须分两条调用路径，**不能写成 `t(key, zh)` 直接把 undefined 递进去**：
   * i18next 的 `t` 是重载并集，第二参为 `undefined` 时会选中 `options?: TOptions` 那一支，`tsc` 直接报
   * TS2345（`string` 不可赋给 `$Dictionary`）。两条路径的运行期语义相同（键缺失都返回**键本身**，
   * 实测 23.16.8），差别只在类型层。
   */
  const tr = (key: string, zh?: string) => (zh === undefined ? t(key) : t(key, zh));
  const label = tr(spec.label, spec.zh);
  const fid = `nd-f-${spec.k}`;

  if (spec.t === 'switch') {
    const on = value === true;
    const off = spec.disabled === true;
    const hintKey = off && spec.disabledHint ? spec.disabledHint : spec.hint;
    const hintZh = off && spec.disabledHint ? spec.disabledHintZh : spec.hintZh;
    return (
      <div className="fld swt-row">
        <div className="swt-tx">
          <b>{label}</b>
          {/* 传 `hintZh` 原样（可能是 undefined），**不写 `?? ''`**：实测 i18next 23.16.8 对
              `t(key, '')` 返回**空串** —— 键漏了会渲染出一个空的 `.fld-hint`，谁都看不出少了什么；
              而 `t(key, undefined)` 返回**键本身**，一眼可见。缺省删光之后这两者的差别才浮出来。 */}
          {hintKey && <div className="fld-hint">{tr(hintKey, hintZh)}</div>}
        </div>
        {/* 原生 `disabled`（而非 `aria-disabled` + 自行拦截）：它连点击事件都不派发 ⇒ `onChange`
            结构上不可达，而不是「拦得住就好」。禁用的开关是「结构上永远不能开」的语义载体，
            这一层不能只做视觉。视觉见 index.css 的 `.swt:disabled`。 */}
        <button
          type="button"
          role="switch"
          aria-checked={on}
          aria-label={label}
          className={`swt${on ? ' on' : ''}`}
          disabled={off}
          onClick={() => onChange(!on)}
        />
      </div>
    );
  }

  const labelEl = (
    <label className="fld-l" htmlFor={fid}>
      <span>{label}</span>
      {spec.opt && <span className="fld-opt"> {t('common.optional', '可选')}</span>}
    </label>
  );
  /**
   * 说明行 —— 非 switch 的四个分支**共用同一个元素**（switch 有自己那份：它要在 `.swt-tx` 里、
   * 且禁用时用 `disabledHint` 取代）。抽成一个常量而不是每支各写一遍 `{spec.hint && …}`：
   * `hint` 提到 `FieldBase` 之后每支都得渲染，漏一支的下场是**填了 hint 却不显示**——
   * 类型不红、build 不红、本仓 vitest 无 jsdom 渲染不了，没有任何门抓得到（同 `toCselOptions` 那条理由）。
   */
  const hintEl = spec.hint ? <div className="fld-hint">{tr(spec.hint, spec.hintZh)}</div> : null;

  if (spec.t === 'select') {
    // 传 value：当前值落在选项集外时并入选项，避免「一碰下拉就被迫改值」（见 toCselOptions 注释）。
    const opts = toCselOptions(spec.options, value);
    return (
      <div className="fld">
        {labelEl}
        <Csel
          id={fid}
          ariaLabel={label}
          value={typeof value === 'string' ? value : ''}
          onChange={(v) => onChange(v)}
          options={opts}
        />
        {hintEl}
      </div>
    );
  }

  if (spec.t === 'number') {
    return (
      <div className="fld">
        {labelEl}
        <input
          id={fid}
          className={`input${spec.mono ? ' mono' : ''}`}
          inputMode="numeric"
          value={value === undefined || value === null ? '' : String(value)}
          onChange={(e) => onChange(parseNumberField(e.target.value))}
          placeholder={spec.ph ?? '—'}
        />
        {hintEl}
      </div>
    );
  }

  if (spec.t === 'textarea') {
    const textarea = (
      <textarea
        id={fid}
        className={`input${spec.mono ? ' mono' : ''}${spec.secret && !secretVisible ? ' secret-masked' : ''}`}
        rows={spec.rows ?? 4}
        value={typeof value === 'string' ? value : ''}
        onChange={(e) => onChange(e.target.value)}
        placeholder={spec.ph}
      />
    );
    return (
      <div className="fld">
        {labelEl}
        {spec.secret ? (
          <div className="secret-field">
            {textarea}
            <button
              type="button"
              className="secret-toggle"
              aria-label={secretVisible ? t('common.hideSecret', '隐藏敏感内容') : t('common.showSecret', '显示敏感内容')}
              aria-pressed={secretVisible}
              onClick={() => setSecretVisible((visible) => !visible)}
            >
              {secretVisible ? '◉' : '◎'}
            </button>
          </div>
        ) : textarea}
        {hintEl}
      </div>
    );
  }

  // text（默认）
  const input = (
    <input
      id={fid}
      type={spec.secret && !secretVisible ? 'password' : 'text'}
      className={`input${spec.mono ? ' mono' : ''}`}
      value={typeof value === 'string' ? value : ''}
      onChange={(e) => onChange(e.target.value)}
      placeholder={spec.ph ?? '—'}
    />
  );
  return (
    <div className="fld">
      {labelEl}
      {spec.secret ? (
        <div className="secret-field">
          {input}
          <button
            type="button"
            className="secret-toggle"
            aria-label={secretVisible ? t('common.hideSecret', '隐藏敏感内容') : t('common.showSecret', '显示敏感内容')}
            aria-pressed={secretVisible}
            onClick={() => setSecretVisible((visible) => !visible)}
          >
            {secretVisible ? '◉' : '◎'}
          </button>
        </div>
      ) : input}
      {hintEl}
    </div>
  );
}

/**
 * 从 FieldSpec 列表构造初始草稿（新增态默认）：select→首选项、switch→false、number→undefined、text→''。
 * fromConfig 在此之上覆盖存量值（编辑态），保证每个键都有合法默认、缺省不漏键。
 */
export function draftFromSpecs(specs: readonly FieldSpec[]): FormValues {
  const d: FormValues = {};
  for (const f of specs) {
    if (f.t === 'select') d[f.k] = f.options[0]?.[0] ?? '';
    else if (f.t === 'switch') d[f.k] = false;
    else if (f.t === 'number') d[f.k] = undefined;
    else d[f.k] = '';
  }
  return d;
}

export default FieldRenderer;
