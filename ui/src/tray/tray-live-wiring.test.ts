/**
 * 托盘浮层「活性」接线守卫 —— A1 / A3 / A5 / A6 的**接线形态**锁。
 *
 * 为什么必须是源码结构守卫：这一批修的缺陷全都不在算法里，而在**调用点**。
 *  - A3：`labels.ts` 曾是模块级 `const LANG = resolveLang()`，解析逻辑一直是对的，
 *    但浮层窗常驻不重载 ⇒ 那个 const 的生命周期 = 整个进程 ⇒ 改语言必须重启。
 *    逻辑单测无论怎么写都全绿，缺陷照旧。
 *  - A5：`speedTestableIds` 一直存在且有单测，浮层就是不调它，自己只测 `selectedId` 一个节点。
 *  - A6：`notifyDesktop` 存在、`desktopNotifications` 开关存在，浮层就是不同步、不发。
 *  - A1：两个入口整个不存在。
 * 都是「谓词/出口在，调用点没用」——沿用本仓既有守卫模式（`nodes-speedtest-wiring.test.ts`、
 * `store/latency-wiring-invariants.test.ts`、`i18n/locale-parity.test.ts`）。
 *
 * 守的是**形态**不是措辞：断言的都是「哪条腿调了哪个函数 / 传了什么参数」这类结构事实。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const read = (rel: string): string =>
  readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');

/**
 * 去注释后的源码 —— 所有断言都跑在它上面。两个方向都必要：本仓注释习惯逐字引用「被替换掉的旧形态」
 * （这里就有 `const LANG = resolveLang()`、`running ? 'ok' : 'idle'`），扫原文会被自己的说明文字误伤；
 * 反过来，只在注释里提一句 `speedTestableIds` 就能让 `toContain` 变绿 —— 那是假绿。
 * `[^:]` 前瞻避免把 `https://` 当行注释切掉。
 */
function code(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');
}

const MENU_RAW = read('./TrayMenu.tsx');
const LABELS_RAW = read('./labels.ts');
/** 语言的**活性状态**自 2026-07-31 起住在共享的 aux i18n 里（浮层与更新弹窗同一份），故一并扫。 */
const AUX_RAW = read('../i18n/auxiliary.ts');
const MENU = code(MENU_RAW);
const LABELS = code(LABELS_RAW);
const AUX = code(AUX_RAW);

describe('守卫自检：扫到的确实是源码（防读空文件恒绿）', () => {
  it('三个源文件非空且是托盘/aux 的文件', () => {
    expect(MENU_RAW.length).toBeGreaterThan(1000);
    expect(LABELS_RAW.length).toBeGreaterThan(200);
    expect(AUX_RAW.length).toBeGreaterThan(200);
    expect(MENU).toContain('export default function TrayMenu');
    // 形态无关（`export function t(` / `export const t =` 都算）：断的是「浮层有取文案的出口」。
    expect(LABELS).toMatch(/export\s+(?:const|function)\s+t\b/);
    expect(AUX).toContain('export function createAuxI18n');
  });

  it('去注释后仍是可断言的代码（防 code() 把源码整段吃掉 → 负向断言恒绿）', () => {
    expect(MENU.length).toBeGreaterThan(MENU_RAW.length / 3);
    expect(LABELS.length).toBeGreaterThan(LABELS_RAW.length / 3);
    expect(MENU).not.toContain('原型 `.tray-menu` L2905-2963 移植');
  });
});

describe('A1：托盘缺的两项入口真实存在且真实接线', () => {
  // 通道名自 2026-07-29 起一律取自 `IPC_CHANNELS`（G11 收编托盘那 7 条命令），故下面按常量名断言。
  // 值与 Rust 函数名是否仍相等由 G11 的跨语言腿守，本文件只管「这条腿接了没」。
  it('「打开设置」走 tray_show_main 并带白名单目标屏参数', () => {
    // 光有按钮不算：没有 screen 参数它就只是第二个「打开主窗口」。
    expect(MENU).toMatch(
      /invoke\(\s*IPC_CHANNELS\.TRAY_SHOW_MAIN\s*,\s*\{\s*screen:\s*'settings'\s*\}/,
    );
  });

  it('「检查更新」走后端共享命令，而不是前端另拼一条链', () => {
    expect(MENU).toContain('invoke<boolean>(IPC_CHANNELS.TRAY_CHECK_UPDATE)');
    // 前端自己拼 update_popup_show 就得自备 currentVersion，会与启动期自动检查读出不同值。
    expect(MENU, '不得在浮层里另起一条弹窗链').not.toContain('update_popup_show');
  });

  it('检查更新有互斥闸（连点会并发发多次出站请求）', () => {
    expect(MENU).toMatch(/if\s*\(checking\)\s*return;/);
    expect(MENU).toContain('disabled={checking}');
  });

  it('检查失败不得显示成「已是最新」（B5 反伪造）', () => {
    // 结构判据：catch 分支里出现的必须是失败文案，不能是 up-to-date 文案。
    // 文案 2026-07-31 起走 i18n 键（浮层接入 5 语种），故按**键**断言而非按英文字面量。
    const start = MENU.indexOf('const checkUpdate');
    expect(start).toBeGreaterThan(-1);
    const body = MENU.slice(start, MENU.indexOf('\n  };', start));
    const catchIdx = body.indexOf('} catch {');
    expect(catchIdx).toBeGreaterThan(-1);
    const catchBody = body.slice(catchIdx);
    expect(catchBody).toContain('tray.updateCheckFailed');
    expect(catchBody, 'catch 腿里不得出现「已是最新」').not.toContain('tray.upToDate');
  });
});

describe('A3：浮层语言必须 live（浮层窗常驻不重载）', () => {
  it('labels 导出刷新出口，且语言是可刷新的模块状态、不是模块级 const', () => {
    expect(LABELS).toMatch(/export\s+(?:const|function)\s+refreshTrayLang\b/);
    // 被修掉的原形态：一次求值、终生不变。
    expect(LABELS, '语言不得退回模块级 const（改语言要重启才生效）').not.toMatch(
      /const\s+LANG\s*=\s*resolveLang\(\)/
    );
    // 活性状态住在 aux i18n 里：`let lang` + `refresh()` 重新赋值。两条都要，缺一即回归成快照。
    expect(AUX).toMatch(/let\s+lang\s*=\s*resolveAuxLanguage\(\)/);
    expect(AUX).toMatch(/refresh\(\)\s*\{[\s\S]{0,200}lang\s*=\s*resolveAuxLanguage\(\)/);
  });

  it('t() 读的是当前语言变量（而不是被闭包捕获的快照）', () => {
    // `bundles[lang][...]` —— 每次调用现读 `lang`。若有人改成在 createAuxI18n 里先
    // `const b = bundles[lang]` 再闭包捕获，语言切换就再也传不进来，本条转红。
    expect(AUX).toMatch(/bundles\[lang\]\[/);
    expect(AUX, 'bundle 不得在建实例时被闭包捕获成快照').not.toMatch(
      /const\s+\w+\s*=\s*bundles\[lang\]\s*;/
    );
  });

  it('浮层语言解析每次重读 localStorage（不缓存 choice）', () => {
    // 主窗改语言只写 localStorage + 后端 config；浮层若缓存了 choice，refresh() 就成了空转。
    expect(AUX).toMatch(/function\s+resolveAuxLanguage\(\)[\s\S]{0,400}localStorage\.getItem\(/);
  });

  it('浮层在配置变更与获焦两处都重解析语言', () => {
    // 只挂一处不够：configChanged 可能在浮层隐藏期间投递，获焦腿是兜底。
    const hits = MENU.match(/refreshTrayLang\(\)/g) ?? [];
    expect(hits.length, '至少 hydrate + onFocus 两处').toBeGreaterThanOrEqual(2);
    expect(MENU).toContain('setLang(refreshTrayLang())');
  });
});

describe('A5：托盘测速 = 全量，且与首页/节点页同一条过滤线', () => {
  it('目标集走 speedTestableIds，并带 path-aware 的 mainCorePool 位', () => {
    expect(MENU).toMatch(/speedTestableIds\(servers,\s*\{\s*mainCorePool:\s*running\s*\}\)/);
  });

  it('不得退回「只测当前选中节点」', () => {
    // 这正是被修的原形态：`const target = servers.find(s => s.id === selectedId)` → speedTest([target.id])。
    expect(MENU).not.toMatch(/speedTest\(\s*target\s*\?/);
    expect(MENU).not.toMatch(/api\.server\.speedTest\(\s*\)/);
  });

  it('空集不发请求，且给出提示（不空跑）', () => {
    const start = MENU.indexOf('const onSpeedTest');
    const body = MENU.slice(start, MENU.indexOf('\n  };', start));
    expect(body).toMatch(/ids\.length\s*===\s*0/);
    // 空集腿必须在 return 之前落一条 notice —— 静默 return 会让用户读作「按钮坏了」。
    const guard = body.slice(body.indexOf('ids.length === 0'));
    expect(guard.slice(0, guard.indexOf('}'))).toContain('setNotice');
  });
});

describe('A6：托盘桌面通知（开关同步 + FakeIP 纠正告知）', () => {
  it('hydrate 把 desktopNotifications 开关同步进本窗 JS 堆', () => {
    // 托盘窗与主窗不共享模块实例：App.tsx 那次同步只作用于主窗，缺这行则浮层通知无视用户的关闭设置。
    expect(MENU).toContain('setDesktopNotificationsEnabled(cfg.desktopNotifications)');
  });

  it('切接管方式触发 FakeIP 自动启用时真的发通知（不是又一次 TODO）', () => {
    const start = MENU.indexOf('const setTakeover');
    const body = MENU.slice(start, MENU.indexOf('\n  };', start));
    expect(body).toContain('applyFakeIpTunEntry');
    expect(body).toMatch(/if\s*\(corrected\)/);
    expect(body).toContain('notifyDesktop(');
  });
});

describe('A2：浮层状态点走多态折算，不再是内联二元三目', () => {
  it('状态点类由 TRAY_TONE_DOT_CLASS 决定', () => {
    expect(MENU).toContain('TRAY_TONE_DOT_CLASS[tone]');
    expect(MENU).toMatch(/const tone = trayStatusTone\(\{/);
    // 被修的原形态。
    expect(MENU, '不得退回 ok/idle 二态').not.toMatch(/dot\s+\$\{running\s*\?\s*'ok'\s*:\s*'idle'\}/);
  });

  it('errored 的真值来自 ProxyStatus 快照，不靠自建 latch', () => {
    // 存的是**码本体**（降级判定要区分 SYSTEM_PROXY_FAILED 与其它码），布尔在折算处现算。
    expect(MENU).toContain('setErrorCode(status.errorCode)');
    expect(MENU).toMatch(/errored:\s*!!errorCode/);
  });
});

describe('A2b：systemProxy 降级态跨窗一致（2026-07-28 复审 MED #4）', () => {
  // 被守的缺陷：浮层只折 running/starting/errored 且 running 压过一切 ⇒ OS 代理被手改时，
  // 主窗状态栏亮琥珀「未生效」、托盘同一时刻显绿点「已连接」。同一台机器上两个窗说相反的话。
  it('降级判定复用主窗那个纯函数，不在浮层重写一套', () => {
    expect(MENU).toMatch(/degraded:\s*[\s\S]{0,40}deriveTakeoverConnState\(\{/);
    expect(MENU).toMatch(/=== 'proxy-degraded'/);
    expect(MENU).toContain("from '@/components/screens/home/connection-state'");
  });

  it('活态**取一发**（走共享取数出口），不在浮层自建常驻轮询', () => {
    expect(MENU).toContain('fetchSystemProxyLive()');
    expect(MENU, '浮层不得挂常驻轮询驱动').not.toContain('useSystemProxyLivePolling');
    // 后端查询语句仍只许出现在共享 store 一处（system-proxy-live-wiring.test.ts 的 T1）。
    expect(MENU).not.toMatch(/\.getSystemProxyStatus\s*\(/);
  });

  it('适用范围闸门与主窗共用同一个谓词（不适用时不发查询、且丢回 unknown）', () => {
    expect(MENU).toMatch(/if \(isSystemProxyLiveApplicable\(status\.running, cfg\.proxyModeType\)\)/);
    expect(MENU).toMatch(/setSystemProxyLive\('unknown'\)/);
  });
});
