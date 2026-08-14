/**
 * 设置页四清单折叠态 + 终端代理平台分支的断言门。
 *
 * # 这些测试为什么能算数（而不是又一层复刻）
 *
 * 组件**直接消费** `settings-logic` 的导出（`localProxyPort` / `splitTerminalEnvByPlatform`），
 * 且下面的渲染断言走 `react-dom/server` 的 `renderToStaticMarkup` **真渲染真组件**（本仓 vitest
 * 是 node 环境、无 jsdom/testing-library，此法是既有先例：`ReverseRoutingBadge.test.tsx:1-10`）。
 * 故「渲染出的 HTML 里有什么」= 用户真会看到什么，不是平行复刻出来的假绿。
 *
 * # 顶部那坨 globalThis.document 桩是什么
 *
 * `SettingsNetwork` 经 `@/lib/error-handler` → `@/i18n` 链路在**模块加载期**就要读 `document`
 * （`i18n/index.ts:81` 写 `<html dir/lang>`），`Csel` 还要 `createPortal(_, document.body)`。
 * node 环境没有 document，故给一个最小桩。**`data-os` 可写**正是本文件要的能力：平台分支的唯一
 * 输入就是 `<html data-os>`（AppShell.tsx:78 写入），把它做成可控变量，平台分支才有可能被真断言，
 * 而不是靠源码 grep 猜。
 *
 * # 明确不在本门射程（如实标注，不假装覆盖）
 *
 *  · **折叠态在 config 重渲后不被弹回** —— 这是 React state 跨重渲的存活行为，SSR 单次渲染观测不到。
 *    本门只锁「受控三件套（useState + open={open} + onToggle）在源码里同时存在」这一**结构前提**
 *    （见最后一个 describe），真行为标为真机门。
 *  · 折叠段在设置页的间距 / 视觉（`.fld-fold-body` 的 padding 是按 dialog 上下文调的）—— 真机门。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server';
import type { UserConfig } from '@/contracts/types';

/** t() 桩：返回 `key` 或 `key#值1,值2` —— 断言落在键 + 插值实参上，与具体语种文案解耦。 */
vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({
    t: (key: string, opts?: unknown) =>
      opts && typeof opts === 'object'
        ? `${key}#${Object.values(opts as Record<string, unknown>).join(',')}`
        : key,
  }),
}));

/** `<html data-os>` 的可控值 —— 平台分支的唯一输入。 */
const dataOs: { value: string | null } = { value: 'mac' };
(globalThis as unknown as { document: unknown }).document = {
  documentElement: {
    dir: '',
    lang: '',
    getAttribute: (name: string) => (name === 'data-os' ? dataOs.value : null),
    setAttribute: () => {},
  },
  // Csel 的 createPortal 目标；react-dom 只校验 nodeType===1。
  body: { nodeType: 1 },
};

const SettingsNetwork = (await import('./SettingsNetwork')).default;
const SettingsDns = (await import('./SettingsDns')).default;
const SettingsTun = (await import('./SettingsTun')).default;
const { Fold } = await import('@/components/Fold');
const {
  localProxyPort,
  DEFAULT_MIXED_PORT,
  terminalEnvGroups,
  splitTerminalEnvByPlatform,
  showsUnixPersistenceTip,
} = await import('./settings-logic');

const noop = async () => {};
const render = (cfg: Partial<UserConfig>, Screen: typeof SettingsNetwork) =>
  renderToStaticMarkup(<Screen config={cfg as UserConfig} update={noop} />);

/** 取出某个 `<details id="...">` 的 summary 文本（计数徽章断言用）。 */
function summaryOf(html: string, id: string): string {
  const m = html.match(new RegExp(`<details id="${id}"[^>]*>(.*?)</summary>`, 's'));
  expect(m, `未渲染 <details id="${id}">`).not.toBeNull();
  return m![1];
}

/** 取出某个 `<details id="...">` 的整段（含 body），用于「清单确实被折叠体包住」的断言。 */
function detailsOf(html: string, id: string): string {
  const start = html.indexOf(`<details id="${id}"`);
  expect(start, `未渲染 <details id="${id}">`).toBeGreaterThan(-1);
  return html.slice(start, html.indexOf('</details>', start));
}

// ---------------------------------------------------------------------------
// 1. 端口回退链 —— 与 Rust `local_proxy_port` 同口径
// ---------------------------------------------------------------------------

describe('localProxyPort —— 逐条对齐 crates/config-engine/.../proxy_ports.rs:22-34', () => {
  it('mixedPort > 0 优先', () => {
    expect(localProxyPort({ mixedPort: 7891, httpPort: 2080 })).toBe(7891);
  });

  it('mixedPort 未设 → 回退 httpPort（旧配置迁移前的常见形态；`?? 7890` 写法丢的就是这条）', () => {
    expect(localProxyPort({ httpPort: 2080 })).toBe(2080);
  });

  it('mixedPort = 0 → 视为未设、回退 httpPort（`??` 只挡 null/undefined，挡不住 0）', () => {
    expect(localProxyPort({ mixedPort: 0, httpPort: 1087 })).toBe(1087);
  });

  it('两者皆无 → 默认 7890', () => {
    expect(localProxyPort({})).toBe(DEFAULT_MIXED_PORT);
    expect(DEFAULT_MIXED_PORT).toBe(7890);
  });

  it('mixedPort=0 且 httpPort=0 → 默认（两级 >0 守卫都要在）', () => {
    expect(localProxyPort({ mixedPort: 0, httpPort: 0 })).toBe(7890);
  });
});

describe('跨语言漂移门 —— Rust 侧回退链改了，这里必须转红', () => {
  const rust = readFileSync(
    fileURLToPath(new URL('../../../../../crates/config-engine/src/user_config/proxy_ports.rs', import.meta.url)),
    'utf8',
  );

  it('前提自检：真读到了那个 Rust 文件（防路径写错后 0 断言恒绿）', () => {
    expect(rust).toContain('pub fn local_proxy_port');
  });

  it('默认端口常量两侧一致', () => {
    const m = rust.match(/pub const DEFAULT_MIXED_PORT:\s*u16\s*=\s*(\d+)/);
    expect(m, 'Rust 侧 DEFAULT_MIXED_PORT 常量不见了').not.toBeNull();
    expect(Number(m![1])).toBe(DEFAULT_MIXED_PORT);
  });

  it('回退链仍是 mixed>0 → http>0 → 默认（顺序与两级守卫都在）', () => {
    const body = rust.slice(rust.indexOf('pub fn local_proxy_port'));
    const fn = body.slice(0, body.indexOf('\n}'));
    const mixedAt = fn.indexOf('mixed_port()');
    const httpAt = fn.indexOf('http_port()');
    expect(mixedAt, 'mixed_port 分支不见了').toBeGreaterThan(-1);
    expect(httpAt, 'http_port 回退腿不见了 —— UI 侧的同款回退必须一并删').toBeGreaterThan(mixedAt);
    expect(fn).toContain('DEFAULT_MIXED_PORT');
    // 两级 `p > 0` 守卫：少一个就意味着 0 端口会被当成有效值透传。
    expect(fn.match(/p\s*>\s*0/g)?.length ?? 0).toBe(2);
  });
});

// ---------------------------------------------------------------------------
// 2. 终端环境变量：平台分支 + 端口 + no_proxy
// ---------------------------------------------------------------------------

describe('terminalEnvGroups —— 三套 shell 语法各自成立', () => {
  const groups = terminalEnvGroups(7891);
  const by = (id: string) => groups.find((g) => g.id === id)!;

  it('三组齐全（Unix / CMD / PowerShell），顺序固定', () => {
    expect(groups.map((g) => g.id)).toEqual(['unix', 'win-cmd', 'win-powershell']);
  });

  it('Unix 用 export，无引号', () => {
    expect(by('unix').lines[0]).toBe('export http_proxy=http://127.0.0.1:7891');
  });

  it('Windows CMD 用 set（`export` 在 cmd 里根本不是命令）', () => {
    expect(by('win-cmd').lines[0]).toBe('set http_proxy=http://127.0.0.1:7891');
    expect(by('win-cmd').lines.every((l) => l.startsWith('set '))).toBe(true);
  });

  it('PowerShell 用 $env: 且值必须带引号（裸值遇 : 会被解析成别的东西）', () => {
    expect(by('win-powershell').lines[0]).toBe('$env:http_proxy="http://127.0.0.1:7891"');
    expect(by('win-powershell').lines.every((l) => /^\$env:\w+=".*"$/.test(l))).toBe(true);
  });

  it('每组四条：http / https / all(socks5) / no_proxy', () => {
    for (const g of groups) {
      expect(g.lines).toHaveLength(4);
      expect(g.lines.join('\n')).toContain('socks5://127.0.0.1:7891');
      expect(g.lines.join('\n')).toContain('no_proxy');
    }
  });

  it('端口来自入参 —— 写死 7890 会转红', () => {
    expect(terminalEnvGroups(1080)[0].lines.join('\n')).toContain(':1080');
    expect(terminalEnvGroups(1080)[0].lines.join('\n')).not.toContain('7890');
  });
});

describe('splitTerminalEnvByPlatform —— 当前平台优先 + 其余可展开', () => {
  it('mac：当前只给 Unix，其余两组归折叠', () => {
    const s = splitTerminalEnvByPlatform('mac', 7890);
    expect(s.current.map((g) => g.id)).toEqual(['unix']);
    expect(s.others.map((g) => g.id)).toEqual(['win-cmd', 'win-powershell']);
  });

  it('lin：同 mac', () => {
    expect(splitTerminalEnvByPlatform('lin', 7890).current.map((g) => g.id)).toEqual(['unix']);
  });

  it('win：CMD 与 PowerShell **都**算当前平台（同机两种 shell 都常用，不能只给一种）', () => {
    const s = splitTerminalEnvByPlatform('win', 7890);
    expect(s.current.map((g) => g.id)).toEqual(['win-cmd', 'win-powershell']);
    expect(s.others.map((g) => g.id)).toEqual(['unix']);
  });

  it('平台取不到 → 全部归 current（宁可多显示，也不能把唯一能用的那套藏进折叠）', () => {
    const s = splitTerminalEnvByPlatform(undefined, 7890);
    expect(s.current).toHaveLength(3);
    expect(s.others).toEqual([]);
  });

  it('任何平台下三组都不丢失（current + others 恒为全集）', () => {
    for (const p of ['mac', 'win', 'lin', undefined] as const) {
      const s = splitTerminalEnvByPlatform(p, 7890);
      expect([...s.current, ...s.others].map((g) => g.id).sort()).toEqual(
        ['unix', 'win-cmd', 'win-powershell'].sort(),
      );
    }
  });
});

describe('showsUnixPersistenceTip —— ~/.bashrc 提示不给 Windows 用户', () => {
  it('mac / lin / 未知 → 显示', () => {
    expect(showsUnixPersistenceTip('mac')).toBe(true);
    expect(showsUnixPersistenceTip('lin')).toBe(true);
    expect(showsUnixPersistenceTip(undefined)).toBe(true);
  });

  it('win → 不显示（CMD 要 setx、PowerShell 要 profile，照抄 bashrc 就是同类缺陷换地方复发）', () => {
    expect(showsUnixPersistenceTip('win')).toBe(false);
  });
});

describe('SettingsNetwork 终端代理段（真渲染）', () => {
  it('mac：常驻区只有 export，`set` / `$env:` 全在折叠里', () => {
    dataOs.value = 'mac';
    const html = render({ mixedPort: 7890 }, SettingsNetwork);
    const others = detailsOf(html, 'fold-env-others');
    const outside = html.replace(others, '');
    expect(outside).toContain('export http_proxy=');
    expect(outside).not.toContain('set http_proxy=');
    expect(outside).not.toContain('$env:http_proxy=');
    expect(others).toContain('set http_proxy=');
    expect(others).toContain('$env:http_proxy=');
  });

  it('win：常驻区同时给 CMD 与 PowerShell，export 归折叠', () => {
    dataOs.value = 'win';
    const html = render({ mixedPort: 7890 }, SettingsNetwork);
    const others = detailsOf(html, 'fold-env-others');
    const outside = html.replace(others, '');
    expect(outside).toContain('set http_proxy=');
    expect(outside).toContain('$env:http_proxy=');
    expect(outside).not.toContain('export http_proxy=');
    expect(others).toContain('export http_proxy=');
  });

  it('lin：与 mac 同形（Unix 常驻）', () => {
    dataOs.value = 'lin';
    const html = render({ mixedPort: 7890 }, SettingsNetwork);
    expect(html.replace(detailsOf(html, 'fold-env-others'), '')).toContain('export http_proxy=');
  });

  it('data-os 缺失：三套全部常驻，不渲染「其余平台」折叠', () => {
    dataOs.value = null;
    const html = render({ mixedPort: 7890 }, SettingsNetwork);
    expect(html).not.toContain('fold-env-others');
    expect(html).toContain('export http_proxy=');
    expect(html).toContain('set http_proxy=');
    expect(html).toContain('$env:http_proxy=');
  });

  it('端口取真实配置：mixedPort=7891 → 命令行与说明文字都是 7891，全文无 7890', () => {
    dataOs.value = 'mac';
    const html = render({ mixedPort: 7891 }, SettingsNetwork);
    expect(html).toContain('export http_proxy=http://127.0.0.1:7891');
    expect(html).toContain('settings.advanced.tipHttpPort#7891');
    expect(html).not.toContain('7890');
  });

  it('端口回退链在 UI 上真的生效：只设 httpPort=2080 → 命令行是 2080（此前 UI 会显 7890）', () => {
    dataOs.value = 'mac';
    const html = render({ httpPort: 2080 }, SettingsNetwork);
    expect(html).toContain('export http_proxy=http://127.0.0.1:2080');
    expect(html).not.toContain('7890');
  });

  it('mixedPort=0 → 回退 httpPort，UI 不会渲染出 :0 这种连不上的端口', () => {
    dataOs.value = 'mac';
    const html = render({ mixedPort: 0, httpPort: 1087 }, SettingsNetwork);
    expect(html).toContain('export http_proxy=http://127.0.0.1:1087');
    expect(html).not.toContain('127.0.0.1:0');
  });

  it('no_proxy 保持独立于旁路清单（刻意不联动，理由见 settings-logic.ts::NO_PROXY_VALUE）', () => {
    dataOs.value = 'mac';
    const html = render(
      { mixedPort: 7890, bypassLANList: ['10.0.0.0/8', '*.corp.example'] },
      SettingsNetwork,
    );
    expect(html).toContain('export no_proxy=localhost,127.0.0.1,::1');
    // 旁路清单条目（CIDR / 通配域名）多数 CLI 的 no_proxy 解析器不认，倒进去只会静默失效。
    expect(html).not.toContain('no_proxy=10.0.0.0/8');
    expect(html).not.toContain('*.corp.example,');
  });

  it('tipPermanent 按平台门控；tipDisable 恒显（此前是零消费者的死键）', () => {
    dataOs.value = 'mac';
    expect(render({}, SettingsNetwork)).toContain('settings.advanced.tipPermanent');
    dataOs.value = 'win';
    const win = render({}, SettingsNetwork);
    expect(win).not.toContain('settings.advanced.tipPermanent');
    expect(win).toContain('settings.advanced.tipDisable');
  });
});

// ---------------------------------------------------------------------------
// 2b. 端口输入：草稿 + onBlur 提交（不再每键落盘）
// ---------------------------------------------------------------------------

describe('端口输入不再逐键落盘（结构 + 渲染）', () => {
  const netSrc = readFileSync(fileURLToPath(new URL('./SettingsNetwork.tsx', import.meta.url)), 'utf8');

  it('前提自检：真读到了 SettingsNetwork 源码', () => {
    expect(netSrc).toContain('export default function SettingsNetwork');
  });

  /**
   * 取某个 `id="<inputId>"` 的 `<TextInput …/>` 里 `onChange={…}` 处理器的**函数体**。
   *
   * 为什么要真的切出函数体，而不是用「全文里不出现 `update({ mixedPort:` 」这种负向正则：
   * 后者可绕 —— `const p = { mixedPort: n }; update(p);` 逐键落盘照样全绿（2026-07-28 复审 LOW #9）。
   * 断言对象必须是「这个 onChange 处理器里有没有任何 `update(` 调用」，与写法无关。
   */
  function onChangeBodyOf(inputId: string): string {
    const at = netSrc.indexOf(`id="${inputId}"`);
    expect(at, `找不到 <TextInput id="${inputId}">`).toBeGreaterThan(-1);
    // 从 id 往后到本元素结束（`/>`）为止，再截出 onChange={...} 的花括号块。
    const el = netSrc.slice(at, netSrc.indexOf('/>', at));
    const start = el.indexOf('onChange={');
    expect(start, `${inputId} 没有 onChange`).toBeGreaterThan(-1);
    let depth = 0;
    for (let i = start + 'onChange='.length; i < el.length; i++) {
      if (el[i] === '{') depth++;
      else if (el[i] === '}' && --depth === 0) return el.slice(start, i + 1);
    }
    throw new Error(`${inputId} 的 onChange 块没有配平的 }`);
  }

  it('两个端口的 onChange 处理器体内**没有任何 update( 调用**（不止是没有对象字面量写法）', () => {
    // 代理运行中每键落盘 = 每个字符触发一次整核重启评估，且中间态是 `7`/`78`/`789` 这类非法端口。
    // 唯一的落盘入口必须是 onBlur → commitPort。
    for (const id of ['mixed-port-input', 'control-port-input']) {
      const body = onChangeBodyOf(id);
      expect(body, `${id} 的 onChange 里出现了 update( —— 逐键落盘回归`).not.toMatch(/\bupdate\s*\(/);
      // 前提自检：切出来的确实是那个处理器体（草稿 setter 必在里面），而不是空串恒绿。
      expect(body).toMatch(/set\w*PortDraft\(/);
    }
    expect(netSrc).toContain("onBlur={() => commitPort('mixedPort'");
    expect(netSrc).toContain("onBlur={() => commitPort('controlPort'");
  });

  it('commitPort 的合法性判定**只**来自 normalizePortInput（不并行复刻一套范围判定）', () => {
    // 原写法是 `not.toMatch(/<\s*1024|>\s*65535/)` —— 任何无关的数字比较（分页、超时、字节数…）
    // 都会被误杀，而真正的复刻只要换个常量名就绕过去了（2026-07-28 复审 LOW #9）。
    // 改成正向断言**调用形态**：raw 进 normalizePortInput，其返回值就是落盘值。
    const body = netSrc.slice(
      netSrc.indexOf('function commitPort('),
      netSrc.indexOf('\n  }', netSrc.indexOf('function commitPort(')),
    );
    expect(body, 'commitPort 不见了').toContain('normalizePortInput');
    // ① 入参是用户原文 `raw`（不是先自己清洗过一遍的东西）。
    expect(body).toMatch(/normalizePortInput\(\s*raw\s*,/);
    // ② 返回值就是判定结果：null → 标红不落盘；非 null → 落盘的就是它。
    expect(body).toMatch(/const next = normalizePortInput\(/);
    expect(body).toMatch(/if \(next === null\)/);
    expect(body).toMatch(/update\(\{\s*\[key\]:\s*next\s*\}\)/);
    // ③ 函数体内不得再出现第二个范围判定的来源（复刻式判定会让 settings-logic 的单测变成假绿）。
    //    只在 commitPort 体内扫，不再全文扫 —— 全文扫正是那条误伤面过宽的旧断言。
    expect(body).not.toMatch(/\d{3,5}\s*(<=?|>=?)|(<=?|>=?)\s*\d{3,5}/);
  });

  it('外部配置变更的回填守卫在（seededRef）—— 少了它用户正打字时会被静默覆盖', () => {
    expect(netSrc).toContain('seededRef');
  });

  it('渲染：混合端口输入取真实配置值，Enter 触发提交', () => {
    dataOs.value = 'mac';
    const html = render({ mixedPort: 7891 }, SettingsNetwork);
    expect(html).toContain('id="mixed-port-input"');
    expect(html).toContain('value="7891"');
    // type="number" 会在非法输入时把 value 报成空串 → 用户敲错的字符当场消失、标红无从指向。
    expect(html).not.toMatch(/id="mixed-port-input"[^>]*type="number"/);
  });

  it('渲染：管理面板关闭时不渲染控制端口输入（开启才有这一行）', () => {
    dataOs.value = 'mac';
    expect(render({}, SettingsNetwork)).not.toContain('id="control-port-input"');
    expect(render({ singboxDashboard: true }, SettingsNetwork)).toContain('id="control-port-input"');
  });

  it('渲染：控制端口按 controlApiPort 口径回显（0 → 9090，而非显示 0）', () => {
    dataOs.value = 'mac';
    const html = render({ singboxDashboard: true, controlPort: 0 }, SettingsNetwork);
    expect(html).toMatch(/id="control-port-input"[^>]*value="9090"/);
  });
});

// ---------------------------------------------------------------------------
// 2c. 「更新与测速」归位到网络页（契约指定）
// ---------------------------------------------------------------------------

describe('更新与测速两行在网络页，通用页无残留', () => {
  it('网络页渲染出这两行', () => {
    dataOs.value = 'mac';
    const html = render({}, SettingsNetwork);
    expect(html).toContain('settings.network.updateAndSpeedTest');
    expect(html).toContain('id="main-session-via-proxy-swt"');
    expect(html).toContain('id="speed-test-url-input"');
  });

  it('通用页源码里已无 speedTestUrl / mainSessionViaProxy 残留（含孤儿 import）', () => {
    const full = readFileSync(fileURLToPath(new URL('./SettingsGeneral.tsx', import.meta.url)), 'utf8');
    // 只扫首个 import 之后的正文：文件头注释里那句「已归位到 SettingsNetwork」是给读代码的人留的
    // 交接说明，不是残留代码。
    const src = full.slice(full.indexOf('\nimport '));
    expect(src).not.toContain('speedTestUrl');
    expect(src).not.toContain('mainSessionViaProxy');
    expect(src).not.toContain('useRef'); // 唯一用途是测速端点的种子守卫，已随之搬走
  });
});

// ---------------------------------------------------------------------------
// 2d. shellPlatformFromDataOs 单一副本
// ---------------------------------------------------------------------------

describe('shellPlatformFromDataOs 只剩 settings-logic 一份实现', () => {
  it('三个屏都不再自带一份（各写一份 = 平台判定口径会各自漂移）', () => {
    for (const f of ['SettingsNetwork.tsx', 'SettingsDisplay.tsx', 'SettingsTun.tsx']) {
      const s = readFileSync(fileURLToPath(new URL(`./${f}`, import.meta.url)), 'utf8');
      expect(s, `${f} 仍自带一份 shellPlatformFromDataOs`).not.toContain(
        'function shellPlatformFromDataOs',
      );
      expect(s, `${f} 未从 settings-logic 引用`).toContain('shellPlatformFromDataOs');
    }
  });

  it('搬家后平台分支行为不变（mac/win 的终端命令分组仍按平台切）', () => {
    dataOs.value = 'win';
    expect(render({}, SettingsNetwork).replace(/[\s\S]*fold-env-others/, '')).toContain(
      'export http_proxy=',
    );
  });
});

// ---------------------------------------------------------------------------
// 2e. MAC / 邻居短名的内联校验（生成期静默丢弃 → UI 当场标出）
// ---------------------------------------------------------------------------

describe('TUN 局域网网关：非法条目内联标红', () => {
  const tunBase = { mtu: 9000, stack: 'auto' as const, autoRoute: true, strictRoute: true };
  const tunCfg = (extra: Record<string, unknown>) =>
    ({ proxyModeType: 'tun', tunConfig: { ...tunBase, ...extra } }) as Partial<UserConfig>;

  it('MAC 清单含非法条目 → 渲染 macInvalid 错误行（builder/inbounds.rs 会静默丢弃它们）', () => {
    dataOs.value = 'lin';
    const html = render(
      tunCfg({ macFilterMode: 'include', macFilterList: ['00:11:22:33:44:55', 'zz:11'] }),
      SettingsTun,
    );
    expect(html).toContain('settings.advanced.macInvalid');
  });

  it('MAC 全合法（三种写法）→ 不报错', () => {
    dataOs.value = 'lin';
    const html = render(
      tunCfg({
        macFilterMode: 'include',
        macFilterList: ['00:11:22:33:44:55', '00-11-22-33-44-55', '0011.2233.4455'],
      }),
      SettingsTun,
    );
    expect(html).not.toContain('settings.advanced.macInvalid');
  });

  it('空行不报错（ListEditor「添加」后的编辑中间态）', () => {
    dataOs.value = 'lin';
    const html = render(tunCfg({ macFilterMode: 'exclude', macFilterList: ['', '  '] }), SettingsTun);
    expect(html).not.toContain('settings.advanced.macInvalid');
  });

  it('邻居短名含非法后缀 → 渲染 neighborDomainInvalid 错误行', () => {
    dataOs.value = 'lin';
    const html = render(tunCfg({ neighborDomains: ['.lan', 'bad suffix'] }), SettingsTun);
    expect(html).toContain('settings.advanced.neighborDomainInvalid');
  });

  it('邻居短名合法（带/不带前导点、多标签）→ 不报错', () => {
    dataOs.value = 'lin';
    const html = render(tunCfg({ neighborDomains: ['.lan', 'home', '.home.arpa'] }), SettingsTun);
    expect(html).not.toContain('settings.advanced.neighborDomainInvalid');
  });

  it('两条新键在五份 locale 都齐（locale-parity 棘轮之外的显式确认）', () => {
    for (const loc of ['en-US', 'zh-CN', 'zh-TW', 'ru', 'fa']) {
      const data = JSON.parse(
        readFileSync(fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)), 'utf8'),
      ) as { settings: { advanced: Record<string, unknown> } };
      for (const key of ['macInvalid', 'neighborDomainInvalid']) {
        expect(typeof data.settings.advanced[key], `${loc} 缺 settings.advanced.${key}`).toBe('string');
      }
    }
  });
});

// ---------------------------------------------------------------------------
// 3. 四个清单的折叠态
// ---------------------------------------------------------------------------

describe('Fold 原语（直接渲染）', () => {
  it('默认折叠：不带 open 属性', () => {
    const html = renderToStaticMarkup(<Fold title="T">x</Fold>);
    expect(html).toContain('<details class="fld-fold">');
    expect(html).not.toContain('open=');
  });

  it('defaultOpen → 渲染 open', () => {
    const html = renderToStaticMarkup(
      <Fold title="T" defaultOpen>
        x
      </Fold>,
    );
    expect(html).toContain('open=""');
  });

  it('count=0 **照样**渲染徽章 —— 不渲染才会让人以为功能没了', () => {
    const html = renderToStaticMarkup(
      <Fold title="T" count={0}>
        x
      </Fold>,
    );
    expect(html).toContain('common.itemCount#0');
  });

  it('count 省略 → 无徽章（如「其余平台」这类非清单折叠）', () => {
    const html = renderToStaticMarkup(<Fold title="T">x</Fold>);
    expect(html).not.toContain('fld-fold-c');
  });

  it('徽章数字来自入参，不是常数', () => {
    expect(
      renderToStaticMarkup(
        <Fold title="T" count={17}>
          x
        </Fold>,
      ),
    ).toContain('common.itemCount#17');
  });

  it('标题带 .fld-fold-t（flex:1）—— 少了它计数徽章会飘到 summary 中间', () => {
    expect(renderToStaticMarkup(<Fold title="T">x</Fold>)).toContain('class="fld-fold-t"');
  });

  it('静态说明经标题旁信息提示渲染，不占用折叠内容区', () => {
    const html = renderToStaticMarkup(
      <Fold title="T" tip="Help text">
        x
      </Fold>,
    );
    expect(html).toContain('class="info-i fld-fold-info"');
    expect(html).toContain('data-tip="Help text"');
  });
});

describe('四处清单确实折叠了，且计数跟着真实清单走', () => {
  interface FoldCase {
    name: string;
    foldId: string;
    listId: string;
    screen: typeof SettingsNetwork;
    cfg: Partial<UserConfig>;
    /** 该 cfg 下清单的真实长度 —— 徽章必须等于它，不能是常数。 */
    n: number;
    /** 清空到 0 条的 cfg（验计数不是常数）。 */
    empty: Partial<UserConfig>;
  }

  const CASES: FoldCase[] = [
    {
      name: '#1 旁路列表',
      foldId: 'fold-bypass',
      listId: 'le-bypass',
      screen: SettingsNetwork,
      cfg: { bypassLANList: ['a', 'b', 'c', 'd', 'e'] },
      n: 5,
      empty: { bypassLANList: [] },
    },
    {
      name: '#2 不使用 FakeIP 的域名',
      foldId: 'fold-fakeip-filter',
      listId: 'fakeip-filter-list',
      screen: SettingsDns,
      cfg: { fakeIpFilterList: ['a.com', 'b.com'] },
      n: 2,
      empty: { fakeIpFilterList: [] },
    },
    {
      name: '#3 不走隧道的网段',
      foldId: 'fold-route-exclude',
      listId: 'cidr-list',
      screen: SettingsTun,
      cfg: { bypassLANList: ['a', 'b', 'c', 'd'] },
      n: 4,
      empty: { bypassLANList: [] },
    },
    {
      name: '#4 排除连入来源网段',
      foldId: 'fold-inbound-exclude',
      listId: 'inbound-cidr-list',
      screen: SettingsTun,
      cfg: {
        tunConfig: {
          mtu: 9000,
          stack: 'auto',
          autoRoute: true,
          strictRoute: true,
          inboundExcludeCidrs: ['x', 'y', 'z'],
        },
      },
      n: 3,
      empty: {
        tunConfig: { mtu: 9000, stack: 'auto', autoRoute: true, strictRoute: true, inboundExcludeCidrs: [] },
      },
    },
  ];

  for (const c of CASES) {
    it(`${c.name}：默认折叠 + 计数 = ${c.n} + 编辑器在折叠体内`, () => {
      dataOs.value = 'mac';
      const html = render(c.cfg, c.screen);
      const det = detailsOf(html, c.foldId);
      // 默认折叠：details 开标签里不得有 open。
      expect(det.slice(0, det.indexOf('>'))).not.toContain('open');
      expect(summaryOf(html, c.foldId)).toContain(`common.itemCount#${c.n}`);
      // 编辑器必须在折叠体里 —— 在外面就等于没折叠。
      expect(det).toContain(`id="${c.listId}"`);
    });

    it(`${c.name}：计数不是常数（清空到 0 时随之变 0，且徽章仍在）`, () => {
      dataOs.value = 'mac';
      const html = render(c.empty, c.screen);
      expect(summaryOf(html, c.foldId)).toContain('common.itemCount#0');
    });
  }

  it('#1/#3 折叠体在「绕过局域网」门控内侧：总开关关掉时整个折叠消失，而非留一个空壳标题', () => {
    dataOs.value = 'mac';
    const net = render({ bypassLAN: false, bypassLANList: ['a'] }, SettingsNetwork);
    expect(net).not.toContain('fold-bypass');
    expect(net).not.toContain('id="le-bypass"');
    const tun = render({ bypassLAN: false, bypassLANList: ['a'] }, SettingsTun);
    expect(tun).not.toContain('fold-route-exclude');
    expect(tun).toContain('cidr-bypass-off-note'); // 换成了「已关闭」提示
  });

  /**
   * 两段文案已走 i18n（本文件的 t() 桩返回键名），故断言拆成两半：
   *  · 结构半 —— 折叠体里确实渲染了那两个键（提示没被顺手删掉）；
   *  · 措辞半 —— 键在**五个语种**里都真的点名了对面那一页。
   * 只断言键名会让「键还在、话已改成别的意思」照样过；只断言中文则与 t() 桩打架。
   */
  it('#1/#3 同源在 UI 上明说（别让用户以为是两份互不相干的清单）', () => {
    dataOs.value = 'mac';
    expect(detailsOf(render({}, SettingsNetwork), 'fold-bypass')).toContain(
      'settings.network.sharedListBold',
    );
    expect(detailsOf(render({}, SettingsTun), 'fold-route-exclude')).toContain(
      'settings.tun.sharedListBold',
    );

    // 中文侧逐字对，其余语种只要求「点了对面那一页的名字」（各语种页名不同，不能拿中文比）。
    const zh = JSON.parse(
      readFileSync(fileURLToPath(new URL('../../../i18n/locales/zh-CN.json', import.meta.url)), 'utf8'),
    ) as { settings: { network: Record<string, string>; tun: Record<string, string> } };
    expect(zh.settings.network.sharedListBold).toContain('TUN · 排除网段');
    expect(zh.settings.tun.sharedListBold).toContain('网络 · 旁路列表');
    for (const loc of ['en-US', 'zh-CN', 'zh-TW', 'ru', 'fa']) {
      const data = JSON.parse(
        readFileSync(fileURLToPath(new URL(`../../../i18n/locales/${loc}.json`, import.meta.url)), 'utf8'),
      ) as { settings: { network: Record<string, string>; tun: Record<string, string> } };
      expect(data.settings.network.sharedListBold, `${loc} 的旁路列表同源提示没点名 TUN`).toContain(
        'TUN',
      );
      expect(data.settings.tun.sharedListBold, `${loc} 的排除网段同源提示是空的`).toBeTruthy();
    }
  });
});

// ---------------------------------------------------------------------------
// 4. 受控结构前提（真行为是真机门，见文件头）
// ---------------------------------------------------------------------------

describe('Fold 必须是受控 details —— 结构前提', () => {
  // 2026-08-10：Fold 从 settings 私有原语提到 `components/Fold.tsx` 共享层 —— dialogs 那 6 处折叠
  // 进入射程后，若让 dialogs 反向 import screens/settings，就是为省一次移动把层级依赖拧反。
  const src = readFileSync(fileURLToPath(new URL('../../Fold.tsx', import.meta.url)), 'utf8');
  const foldSrc = (() => {
    const start = src.indexOf('export function Fold(');
    expect(start, 'Fold 组件不见了').toBeGreaterThan(-1);
    const next = src.indexOf('\nexport function ', start + 1);
    return src.slice(start, next === -1 ? undefined : next);
  })();

  it('open 由 useState 持有并回绑到 details（裸 <details> 的 open 只是 DOM 自身状态，重渲会被写回）', () => {
    expect(foldSrc).toMatch(/useState\(/);
    expect(foldSrc).toMatch(/open=\{open\}/);
  });

  it('onToggle 把 DOM 态同步回 state —— 少了它折叠一次后 state 与 DOM 永久分叉', () => {
    expect(foldSrc).toMatch(/onToggle=\{/);
    expect(foldSrc).toMatch(/setOpen\(/);
  });

  it('设置页与 dialogs 的 .fld-fold 全部走 Fold，没有一处退回裸 <details>', () => {
    // 射程从 3 个设置页扩到 dialogs：2026-08-10 之前那 6 处 dialog 折叠是裸 <details>，
    // 老注释自陈「都在 dialog 里、生命周期短，不在本批射程」——用户反馈后进了射程。
    // 唯一豁免：NodeDialog 的「原始输出」那处 summary 无 chevron，转 Fold 会凭空多一个箭头
    // （视觉变更），故保留原形只挂 onToggle —— 由 reveal.test.ts 的全仓门保证它不是漏网的。
    const settings = ['SettingsNetwork.tsx', 'SettingsDns.tsx', 'SettingsTun.tsx'].map(
      (f) => [f, readFileSync(fileURLToPath(new URL(`./${f}`, import.meta.url)), 'utf8')] as const,
    );
    const dialogs = ['SubDialog.tsx', 'TsSettingsDialog.tsx', 'WarpDialog.tsx', 'RuleDialog.tsx'].map(
      (f) =>
        [f, readFileSync(fileURLToPath(new URL(`../../dialogs/${f}`, import.meta.url)), 'utf8')] as const,
    );
    for (const [f, s] of [...settings, ...dialogs]) {
      expect(s, `${f} 里出现了裸 <details className="fld-fold">`).not.toContain(
        '<details className="fld-fold"',
      );
    }
  });
});
