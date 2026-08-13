/**
 * 条件草稿的门 —— 类型切换清空、勾选/文本单一真值、进程池投影、测试匹配组合。
 *
 * 最要紧的是**第一组**：切类型必须清空。它不是「顺手清一下更干净」，是 `(类型, 值)` 的原子性
 * （每个类型自带解析 + 变换 + 分区，见 `setCondTypeAt` 头注）。这条一旦被谁改成「同族保留」，
 * 症状是**静默的**：规则照存不误，只是从此匹配面变了（`域名 → 域名后缀` 多命中全部子域名、
 * `→ 域名正则` 里的 `.` 变通配）。没有门就没有任何东西会说话。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  computeTestMatch,
  invalidCondValues,
  matchRuleValueOptions,
  offPoolSelectedOptions,
  processPoolOptions,
  selectedValueSet,
  setCondTypeAt,
  sortRuleValueOptions,
  toggleCondValueAt,
  type Cond,
  type RuleValueOption,
} from './rule-cond';
import { RULE_TYPE_IDS } from '@/domain/rules';

/** RuleDialog 源码（去注释）—— 接线门共用。注释里逐字引用了这些标识符，不去掉就是拿注释当证据。 */
const dialogSrc = readFileSync(fileURLToPath(new URL('./RuleDialog.tsx', import.meta.url)), 'utf8')
  .replace(/\/\*[\s\S]*?\*\//g, ' ')
  .replace(/(^|[^:])\/\/.*$/gm, '$1');

describe('类型切换：一律清空（脏数据在结构上消掉，不是靠加清空逻辑打补丁）', () => {
  it('自由输入类型：文本清空', () => {
    // 变异靶：`{ t: tp, v: '' }` 改回 `{ ...c, t: tp }` → 本条转红。
    const before: Cond[] = [{ t: 'domain', v: 'example.com, a.b' }];
    expect(setCondTypeAt(before, 0, 'domainSuffix')).toEqual([{ t: 'domainSuffix', v: '' }]);
  });

  it('可枚举类型：勾选态随之归零（勾选态就是值本身派生的）', () => {
    const before: Cond[] = [{ t: 'ruleSet', v: 'res:builtin:geosite-cn, res:geosite-youtube' }];
    const after = setCondTypeAt(before, 0, 'geosite');
    expect(after[0].v).toBe('');
    expect(selectedValueSet(after[0].v).size).toBe(0);
  });

  it('`domainRegex` 永不接受任何「带过来的」值（`.` 从字面点变通配的转义陷阱）', () => {
    for (const from of RULE_TYPE_IDS) {
      if (from === 'domainRegex') continue;
      const after = setCondTypeAt([{ t: from, v: 'example.com' }], 0, 'domainRegex');
      expect(after[0].v, `${from} → domainRegex 把值带过来了`).toBe('');
    }
  });

  it('15×15 全组合：只要类型真的变了，值必空', () => {
    // 「同族可携」这类想法只要有人加,必然是按某个子集豁免 —— 全组合断言把每个子集都堵住。
    for (const from of RULE_TYPE_IDS) {
      for (const to of RULE_TYPE_IDS) {
        if (from === to) continue;
        expect(setCondTypeAt([{ t: from, v: 'x' }], 0, to)[0].v, `${from} → ${to} 未清空`).toBe('');
      }
    }
  });

  it('切到**同一个**类型是 no-op（Csel 重选当前项也会触发 onChange，不挡就把已填内容清光）', () => {
    const before: Cond[] = [{ t: 'domain', v: 'example.com' }];
    expect(setCondTypeAt(before, 0, 'domain')).toEqual(before);
  });

  it('只动被切的那一条，兄弟条件不受影响', () => {
    const before: Cond[] = [
      { t: 'domain', v: 'a.com' },
      { t: 'port', v: '443' },
    ];
    expect(setCondTypeAt(before, 1, 'sourcePort')).toEqual([
      { t: 'domain', v: 'a.com' },
      { t: 'sourcePort', v: '' },
    ]);
  });
});

describe('勾选与手填共用同一份值（结构上不可能失同步）', () => {
  it('勾上 = 追加；再点 = 移除', () => {
    let conds: Cond[] = [{ t: 'geosite', v: '' }];
    conds = toggleCondValueAt(conds, 0, 'youtube');
    expect(conds[0].v).toBe('youtube');
    conds = toggleCondValueAt(conds, 0, 'netflix');
    expect(conds[0].v).toBe('youtube, netflix');
    conds = toggleCondValueAt(conds, 0, 'youtube');
    expect(conds[0].v).toBe('netflix');
  });

  it('取消与勾选态同口径（都按小写比）—— 否则会出现「显示为勾上、点一下取消不掉」', () => {
    // 变异靶：把 toggle 的比较改成大小写敏感 → 本条转红（selectedValueSet 恒小写）。
    const conds = toggleCondValueAt([{ t: 'geosite', v: 'YouTube' }], 0, 'youtube');
    expect(conds[0].v).toBe('');
    expect(selectedValueSet('YouTube').has('youtube')).toBe(true);
  });

  it('手填进去的值同样算勾上（两个入口一份真值）', () => {
    expect(selectedValueSet(' res:geosite-x ,\n res:geosite-y ')).toEqual(
      new Set(['res:geosite-x', 'res:geosite-y'])
    );
  });
});

describe('processPoolOptions —— proc-path 必须剔掉无 path 的进程', () => {
  const PROCS = [
    { name: 'Telegram', path: '/Applications/Telegram.app/Contents/MacOS/Telegram', count: 1 },
    { name: 'kworker/0:1', count: 3 }, // Linux 内核线程：无 exe（本机实测 356 里 272 条如此）
    { name: 'chrome.exe', count: 12 }, // Windows tasklist：恒无路径
  ];

  it('proc-name：值取进程名；存不下去的（名字含路径分隔符的内核线程）不上架', () => {
    expect(processPoolOptions('processName', PROCS).map((o) => o.value)).toEqual([
      'Telegram',
      'chrome.exe',
    ]);
  });

  it('proc-path：无 path 的整条剔掉（回落成进程名会产出过不了校验的值）', () => {
    // 变异靶：把 `p.path` 换成 `p.path ?? p.name` → 本条转红。那正是改前的行为：
    // Windows 上勾一个进程会往 processPath 里填一个纯文件名，保存时才被 Rust 拒。
    expect(processPoolOptions('processPath', PROCS).map((o) => o.value)).toEqual([
      '/Applications/Telegram.app/Contents/MacOS/Telegram',
    ]);
  });

  it('检索语料含 name 与 path 两者（描述符 searchFields）', () => {
    const opts = processPoolOptions('processName', PROCS);
    expect(matchRuleValueOptions(opts, 'Applications').map((o) => o.value)).toEqual(['Telegram']);
    expect(matchRuleValueOptions(opts, 'CHROME').map((o) => o.value)).toEqual(['chrome.exe']);
  });
});

describe('提交前逐值校验（此前渲染端零校验，全靠后端 RULE_INVALID 往返）', () => {
  it('合法值一个都不报', () => {
    expect(
      invalidCondValues([
        { t: 'domain', v: 'www.google.com' },
        { t: 'ipCidr', v: '8.8.8.8/32,\n10.0.0.0/8' },
        { t: 'ruleSet', v: 'res:builtin:geosite-cn' },
      ])
    ).toEqual([]);
  });

  it('逐值报，按条件类型带出来（用户要照着改，只说「不合法」没用）', () => {
    // 变异靶：把 invalidCondValues 改成「一条条件里有一个合法值就放行」→ 本条转红。
    // `10.0.0.0/40` 是真实会让 sing-box 启动 FATAL 的形态（掩码 >32）。
    expect(invalidCondValues([{ t: 'ipCidr', v: '8.8.8.8, 10.0.0.0/40, 300.1.1.1' }])).toEqual([
      { type: 'ipCidr', value: '10.0.0.0/40' },
      { type: 'ipCidr', value: '300.1.1.1' },
    ]);
  });

  it('规则集的裸 tag 会被拦下（生成端只认 res: 前缀，其余静默剪枝）', () => {
    // 这是 2026-07-30 真机反馈那条：照旧 placeholder 填的裸 tag 保存「成功」，运行时静默不生效。
    expect(invalidCondValues([{ t: 'ruleSet', v: 'category-ads-all' }])).toEqual([
      { type: 'ruleSet', value: 'category-ads-all' },
    ]);
  });

  it('候选面已被同一个判据过滤过 ⇒ 勾得出来的必然存得下去', () => {
    // 变异靶：把两个 poolOptions 里的 `!validateRuleValue(...)` 去掉 → 本条转红
    //（`kworker/0:1` 含 `/`，过不了 processName 的判据）。
    const opts = processPoolOptions('processName', [
      { name: 'kworker/0:1', count: 3 },
      { name: 'Telegram', count: 1 },
    ]);
    expect(opts.map((o) => o.value)).toEqual(['Telegram']);
    expect(invalidCondValues([{ t: 'processName', v: opts.map((o) => o.value).join(', ') }])).toEqual([]);
  });
});

describe('接线：提交腿真的消费了这两个函数', () => {
  const src = dialogSrc;

  it('自检：读到了 RuleDialog 源码', () => {
    expect(src.length).toBeGreaterThan(5000);
  });

  it('`validateRule` 决定能不能提交、`invalidCondValues` 负责说清哪个值不对', () => {
    // 变异靶：删掉 `if (!validateRule(draft))` 那条腿 → 转红。纯函数单测管不到「弹窗到底调没调」。
    expect(src, '提交腿不再走聚合校验 —— 又退回「全靠后端返 RULE_INVALID」').toMatch(
      /if\s*\(\s*!\s*validateRule\s*\(/
    );
    expect(src, '不再说明哪个值不对（只报「校验未通过」用户无从下手）').toContain(
      'invalidCondValues('
    );
  });
});

describe('computeTestMatch —— 轴不匹配算「不适用」，不算未命中', () => {
  it('域名条件 + IP 输入 ⇒ untestable（不是 miss）', () => {
    expect(computeTestMatch([{ t: 'domain', v: 'a.com' }], 'or', '1.2.3.4')).toBe('untestable');
  });

  it('AND 里混一个不适用的条件，不会把整条拖成 miss', () => {
    // 变异靶：把「轴不匹配 → continue」改成 push(false) → 本条转红。
    // 那正是「一条『域名 + 端口』的 AND 规则永远测不出命中」的形态。
    const conds: Cond[] = [
      { t: 'domainSuffix', v: 'google.com' },
      { t: 'port', v: '443' },
    ];
    expect(computeTestMatch(conds, 'and', 'www.google.com')).toBe('hit');
  });

  it('空输入 = empty；无值的条件不参与', () => {
    expect(computeTestMatch([{ t: 'domain', v: 'a.com' }], 'or', '  ')).toBe('empty');
    expect(computeTestMatch([{ t: 'domain', v: '' }], 'or', 'a.com')).toBe('untestable');
  });

  it('逐类型语义来自描述符：suffix 命中子域、keyword 命中子串、regex 用原串', () => {
    expect(computeTestMatch([{ t: 'domainSuffix', v: 'google.com' }], 'or', 'a.b.google.com')).toBe('hit');
    expect(computeTestMatch([{ t: 'domain', v: 'google.com' }], 'or', 'a.google.com')).toBe('miss');
    expect(computeTestMatch([{ t: 'domainKeyword', v: 'goog' }], 'or', 'x.google.com')).toBe('hit');
    expect(computeTestMatch([{ t: 'domainRegex', v: '^stun\\..+' }], 'or', 'stun.l.google.com')).toBe('hit');
  });

  it('IP 轴：ipCidr 走前缀启发式、geoip 恒命中（客户端无 GeoIP 库）', () => {
    expect(computeTestMatch([{ t: 'ipCidr', v: '8.8.0.0/16' }], 'or', '8.8.4.4')).toBe('hit');
    expect(computeTestMatch([{ t: 'ipCidr', v: '10.0.0.0/8' }], 'or', '8.8.4.4')).toBe('miss');
    expect(computeTestMatch([{ t: 'geoip', v: 'cn' }], 'or', '1.2.3.4')).toBe('hit');
  });
});

// ─────────────────────────────────────────────────────────────────────────────
// 候选区：排序（已选优先，且用快照）+ 池外已选可见 —— 陈先生 2026-07-30 那批
// ─────────────────────────────────────────────────────────────────────────────

/** geo 池形态的候选（有 `group`）。 */
const opt = (value: string, group?: 'builtin' | 'external'): RuleValueOption => ({
  value,
  label: value,
  group,
  search: [value.toLowerCase()],
});

describe('候选排序：已选(快照) > 内置 > 名称，且排序键**是快照不是实时勾选态**', () => {
  const GEO = [
    opt('zeta', 'external'),
    opt('alpha', 'external'),
    opt('mike', 'builtin'),
    opt('beta', 'builtin'),
  ];

  it('三级键逐级生效（已选跨组顶到最前；同为已选/未选时内置在前；再同则名称）', () => {
    // 变异靶：删掉 `selRank` 那一级 → 本条转红（zeta 会掉到最后）。
    expect(sortRuleValueOptions(GEO, new Set(['zeta'])).map((o) => o.value)).toEqual([
      'zeta', // 已选（虽是 external、名称最末）
      'beta',
      'mike', // 内置组内按名称
      'alpha', // 外置组内按名称
    ]);
  });

  it('「内置优先」那一级不得被名称吞掉（变异「只按名称排」即红）', () => {
    expect(sortRuleValueOptions(GEO, new Set()).map((o) => o.value)).toEqual([
      'beta',
      'mike',
      'alpha',
      'zeta',
    ]);
  });

  it('进程池（无 group）自动退化成「已选 > 名称」，不需要按池分叉', () => {
    const procs = [opt('zsh'), opt('Chrome'), opt('ssh')];
    expect(sortRuleValueOptions(procs, new Set(['zsh'])).map((o) => o.value)).toEqual([
      'zsh',
      'Chrome',
      'ssh',
    ]);
  });

  it('比对按小写（候选写作 GeoSite-CN、快照里是小写 ⇒ 仍算已选）', () => {
    const list = [opt('aaa'), opt('GeoSite-CN')];
    expect(sortRuleValueOptions(list, new Set(['geosite-cn']))[0].value).toBe('GeoSite-CN');
  });

  it('**核心**：排序只认传进来的那个集合 —— 「实时勾选态」与快照不同时，顺序跟快照走', () => {
    // 这是本设计最容易退化的一条。退化后的症状是「勾一个跳一个」（列表在手底下乱动）。
    // 纯函数层面把它钉死：函数只有一个集合参数、自己不读任何实时来源 ⇒ 想退化只能改调用点，
    // 调用点由下面那组接线门守。
    const snapshot = new Set(['alpha']);
    const liveAfterUserClicked = new Set(['alpha', 'zeta']); // 用户又勾了 zeta
    expect(sortRuleValueOptions(GEO, snapshot).map((o) => o.value)).toEqual([
      'alpha',
      'beta',
      'mike',
      'zeta',
    ]);
    // 同一份输入，若拿实时态去排，zeta 会立刻跳到第二位 —— 两个结果必须不同，
    // 否则本用例是在一个「怎么排都一样」的样本上恒绿。
    expect(sortRuleValueOptions(GEO, liveAfterUserClicked).map((o) => o.value)).not.toEqual(
      sortRuleValueOptions(GEO, snapshot).map((o) => o.value)
    );
  });

  it('不原地改输入数组（候选池投影是 useMemo 的缓存值，就地 sort 会污染缓存）', () => {
    const input = [opt('zeta'), opt('alpha')];
    sortRuleValueOptions(input, new Set());
    expect(input.map((o) => o.value)).toEqual(['zeta', 'alpha']);
  });
});

describe('接线：排序键取「快照」state，且快照只在打开 / 切类型两处重建', () => {
  const src = dialogSrc;

  it('自检：读到了 RuleDialog 源码', () => {
    expect(src.length).toBeGreaterThan(5000);
  });

  it('① 全文件只排一次序，且那一次传的是快照 state，不是实时 `selected`', () => {
    // 变异靶：把排序挪到渲染处、拿 `selected`（= `selectedValueSet(c.v)`，实时）当排序键 → 转红。
    const calls = src.match(/sortRuleValueOptions\(/g) ?? [];
    expect(calls.length, '排序出现在不止一处 —— 另一处很可能拿的是实时勾选态').toBe(1);
    const callSite = src.match(/sortRuleValueOptions\([\s\S]{0,120}/)?.[0] ?? '';
    expect(callSite, '排序键不再取快照 state').toMatch(/poolSnap/);
    expect(callSite, '排序键取了实时勾选态 ⇒ 勾一个跳一个').not.toMatch(
      /selectedValueSet|\bselected\b/
    );
  });

  it('② 候选投影的依赖里不得有 `conds`（有就等于每敲一个字符 / 每勾一次都重排）', () => {
    const deps = src.match(/const poolOptions = useMemo\([\s\S]*?\}, \[([^\]]*)\]\);/)?.[1] ?? '';
    expect(deps.length, '没扫到 poolOptions 的依赖数组 —— 门在空集上恒绿').toBeGreaterThan(5);
    expect(deps, '候选投影依赖了 conds ⇒ 排序变实时').not.toMatch(/\bconds\b/);
    expect(deps, '候选投影没依赖快照 ⇒ 切类型后排序不刷新').toMatch(/\bpoolSnap\b/);
  });

  it('③ 快照的写入点**有且只有一处**，且在切类型腿里（勾选腿里重建 = 实时排序的等价形态）', () => {
    // 变异靶：在 `toggleCondValue` 里补一句 `setPoolSnap(...)` → 本条转红。
    // 那是最像「顺手保持同步」的一次改动，而它恰好把快照策略整个作废。
    const writes = src.match(/setPoolSnap\(/g) ?? [];
    expect(writes.length, '快照写入点不止一处').toBe(1);
    const setTypeLeg = src.match(/const setCondType = [\s\S]*?\n {2}\};/)?.[0] ?? '';
    expect(setTypeLeg.length, '没扫到 setCondType —— 门在空集上恒绿').toBeGreaterThan(60);
    expect(setTypeLeg, '唯一那处写入不在「切类型」腿里').toContain('setPoolSnap(');
  });
});

describe('池外已选：「已选但候选池里没有」的值必须在勾选区露面', () => {
  const POOL = [opt('geosite-cn', 'builtin'), opt('GeoSite-YouTube', 'external')];

  it('池内的值不重复列出，池外的值原样保留大小写并打 offPool 标', () => {
    // 变异靶：把 `inPool.has(lv)` 那道过滤去掉 → 池内的值会重复出一份，本条转红。
    const out = offPoolSelectedOptions('geosite-cn, MyOwnTag, res:x', POOL, 'hint');
    expect(out.map((o) => o.value)).toEqual(['MyOwnTag', 'res:x']);
    expect(out.every((o) => o.offPool === true)).toBe(true);
    expect(out[0].hint).toBe('hint');
  });

  it('比对按小写（与 selectedValueSet / toggle 同口径，否则会「显示为池外、点一下取消不掉」）', () => {
    expect(offPoolSelectedOptions('geosite-youtube', POOL, 'h')).toEqual([]);
  });

  it('同一个值写两遍只出一个 chip（key 冲突 + 视觉重复）', () => {
    expect(offPoolSelectedOptions('tagx, TAGX', POOL, 'h').map((o) => o.value)).toEqual(['tagx']);
  });

  it('检索语料带上自己（勾选区的搜索框对它同样生效）', () => {
    const out = offPoolSelectedOptions('MyOwnTag', POOL, 'h');
    expect(matchRuleValueOptions(out, 'ownt').map((o) => o.value)).toEqual(['MyOwnTag']);
  });

  it('空值 / 池为空的边界（池空时全部已选值都是池外的 —— 清单加载失败那一态）', () => {
    expect(offPoolSelectedOptions('', POOL, 'h')).toEqual([]);
    expect(offPoolSelectedOptions('a, b', [], 'h').map((o) => o.value)).toEqual(['a', 'b']);
  });
});

describe('接线：勾选区渲染的是「池外已选 + 池内」，且手填腿默认折叠', () => {
  const src = dialogSrc;

  it('① 勾选区的入参真的并进了池外已选（变异「只渲染候选池」即红）', () => {
    expect(src, '池外已选的投影函数没有生产调用点').toContain('offPoolSelectedOptions(');
    const shownDef = src.match(/const shownOpts\s*=([\s\S]*?);\n/)?.[1] ?? '';
    expect(shownDef.length, '没扫到 shownOpts 的定义 —— 门在空集上恒绿').toBeGreaterThan(10);
    expect(
      shownDef,
      '勾选区又退回「只渲染候选池」—— 规则里手填的 / 引用了未下载 tag 的值会既看不见也删不掉'
    ).toMatch(/offPool/);
    expect(src, '算出来了却没传给勾选区').toContain('options={shownOpts}');
  });

  it('② 池外 chip 必须被显式标注（`off-pool` 类 —— 光并进去而不区分等于谎称它在池里）', () => {
    expect(src).toMatch(/o\.offPool && 'off-pool'/);
    const css = readFileSync(fileURLToPath(new URL('../../styles/index.css', import.meta.url)), 'utf8');
    expect(css, '组件发了 off-pool 类但覆盖层没有对应规则 = 死类，视觉照旧没区别').toMatch(
      /\.tagchip\.off-pool/
    );
  });

  it('③ 手填腿默认折叠（`<Fold>` 不带 `defaultOpen`），但只对**有候选区**的类型折', () => {
    // 2026-08-10：裸 `<details className="fld-fold cond-manual">` 收编进受控 `<Fold>`（「展开即露出」批次）。
    // 断言的**诉求没变**，只是「默认折叠」的表达从「details 不带 open」变成「Fold 不带 defaultOpen」。
    // 变异靶：给那个 Fold 加 `defaultOpen` → 转红（「避免误修改」的诉求作废）。
    // 反向的靶同样要挡：把 `kind==='free'` 的 textarea 也折进去 → 第二条转红
    //（那些类型的 textarea 是唯一入口，折起来整个条件行就是一片空白）。
    const det = src.match(/<Fold className="cond-manual"[^>]*>/)?.[0] ?? '';
    expect(det.length, '没扫到手填腿的折叠框').toBeGreaterThan(20);
    expect(det, '手填腿默认展开了 —— 「文本框隐藏、避免误修改」的诉求作废').not.toMatch(
      /\bdefaultOpen\b/
    );
    expect(src, '折叠不再以「有候选区」为条件（自由输入类型会被折成一片空白）').toMatch(
      /\{all \? \(\s*<Fold className="cond-manual"/
    );
  });

  it('④ 候选清单**加载中**不判池外（不挡就会把每个已选值都标成「本地暂无」再全翻回去）', () => {
    // 加载中 `all` 恒空 ⇒ 这条规则里每一个已选值都会被判成池外，等清单到了再整批翻回去，
    // 是一次秒级、内容完全相反的闪烁。既有同题口径见 `ruleSetMissing`（清单未到位时恒空）。
    // 加载**失败**则相反：清单永远不会来，此刻池外 chip 是唯一入口 ⇒ 必须露面（只换提示词）。
    const def = src.match(/const offPool = [\s\S]*?;\n/)?.[0] ?? '';
    expect(def.length, '没扫到 offPool 的定义 —— 门在空集上恒绿').toBeGreaterThan(20);
    expect(def, '加载中也判池外 ⇒ 一次内容完全相反的秒级闪烁').toMatch(/!poolLoading/);
    expect(src, '加载失败时也把池外值藏了 —— 那时它们是唯一入口').not.toMatch(
      /const offPool = [^;]*poolFailed/
    );
  });
});
