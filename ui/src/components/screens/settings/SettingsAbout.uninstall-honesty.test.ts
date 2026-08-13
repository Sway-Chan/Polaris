/**
 * 卸载入口「不骗人」守卫 —— 锁死 **后端接线状态 ⟺ 前端可用性、文案与结果呈现** 这条对应关系。
 *
 * 背景：用户 2026-07-21 连报两次「设置›关于 的卸载坏了」。真相是**故意未接线**，缺陷在于描述文案
 * 写着「移除提权助手、用户数据与内核」、读起来像可用功能，禁用理由只藏在 hover tooltip、点击零反馈
 * ⇒ 用户读成「坏了」而非「未实现」。当时的守卫把「未接线 ⇒ 必须 disabled + 明写未实现」钉死，
 * 并在注释里预告「真接线那天本测会红，这正是期望行为」。
 *
 * **2026-07-28 后端已真接线**（`app_uninstall_all` → `runtime::uninstall::run_uninstall`），
 * 本文件按当初写好的剧本翻转极性。现在要挡的是**新的三个方向**的回退：
 *  1. 后端退回未接线却把按钮留着 → 点了必然报错（旧洞的镜像）；
 *  2. 确认文案缩水 → 破坏性操作不说清「删什么 / 不可恢复」，用户在不知情下点掉全部数据；
 *  3. 结果被糊成一句「已卸载」 → 四类目标里 Windows 便携版 / Linux 包管理器安装是**真删不掉**的
 *     （后端返 `unsupported`），糊起来就退回「名不副实」那个形态 —— 当初正是判它「比诚实地不做更糟」。
 *
 * 断言面仍刻意分两处取材，各扬其长：
 *  - **结构事实**取自 `.tsx`（`disabled` 形态、消费的 i18n key、是否真的逐项渲染）；
 *  - **文案事实**取自 locale JSON（**无注释**，不存在「注释里的字被误判成正文」的假阳性；
 *    对应的 `.tsx` 注释里就写着「用户数据与应用本体」，直接扫源码必然误报）。
 */

import { describe, it, expect } from 'vitest';

/** 仓内绝对定位：本文件 → settings 目录 → …… → 仓根。集中一处，路径漂了所有断言一起红。 */
async function repoPaths() {
  const path = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const settingsDir = path.dirname(fileURLToPath(import.meta.url));
  // settings → screens → components → src → ui → <repo root>
  const repoRoot = path.resolve(settingsDir, '../../../../..');
  return {
    path,
    aboutTsx: path.join(settingsDir, 'SettingsAbout.tsx'),
    updaterRs: path.join(repoRoot, 'src-tauri/src/commands/updater.rs'),
    localesDir: path.join(repoRoot, 'ui/src/i18n/locales'),
  };
}

const LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;

/** 裸 JSX 布尔属性 `disabled`（整行只有它）。`disabled={...}` 与注释里提到的 disabled 都不匹配。 */
const BARE_DISABLED = /^\s*disabled\s*$/m;

/** 后端是否又退回了「未接线」占位实现。 */
function stillUnwired(rs: string): boolean {
  const at = rs.indexOf('pub async fn app_uninstall_all');
  expect(at, '锚点消失：app_uninstall_all 被改名或移走，本守卫已失去判据').toBeGreaterThan(-1);
  const body = rs.slice(at);
  const end = body.indexOf('\n}\n');
  expect(end, '找不到 app_uninstall_all 的右花括号，取材面已失效').toBeGreaterThan(-1);
  return body.slice(0, end).includes('CORE_SWAP_NOT_WIRED');
}

describe('卸载入口：后端接线状态 ⟺ 前端可用性、文案与结果呈现', () => {
  it('取材面自检：三处源文件都真的读到了非空内容', async () => {
    // 没有这一条，任何一处路径漂走都会让下面的断言在空字符串上「恰好通过」= 假绿。
    const fs = await import('node:fs');
    const p = await repoPaths();
    for (const f of [p.aboutTsx, p.updaterRs]) {
      expect(fs.existsSync(f), `取材文件不存在：${f}`).toBe(true);
      expect(fs.readFileSync(f, 'utf8').length).toBeGreaterThan(500);
    }
    for (const loc of LOCALES) {
      expect(fs.existsSync(p.path.join(p.localesDir, `${loc}.json`))).toBe(true);
    }
  });

  it('后端已接线 ⇒ 按钮必须解禁，且仍随「卸载进行中」禁用', async () => {
    const fs = await import('node:fs');
    const p = await repoPaths();
    const rs = fs.readFileSync(p.updaterRs, 'utf8');
    const tsx = fs.readFileSync(p.aboutTsx, 'utf8');

    if (stillUnwired(rs)) {
      // 后端回退到未接线：按钮必须一并退回 disabled（解禁一个必然失败的按钮 = 旧洞复发）。
      expect(
        BARE_DISABLED.test(tsx),
        '后端返 CORE_SWAP_NOT_WIRED，卸载按钮必须保持裸 disabled',
      ).toBe(true);
      return;
    }

    expect(BARE_DISABLED.test(tsx), '后端已接线，按钮不得再是恒 disabled').toBe(false);
    expect(
      tsx.includes('disabled={uninstalling}'),
      '按钮必须在卸载进行中禁用 —— 否则用户可重入，第二次会撞上已被删掉的配置目录',
    ).toBe(true);
    expect(
      rs.includes('run_uninstall'),
      '后端必须真的走编排函数，而不是又变成一个占位返回',
    ).toBe(true);
  });

  it('描述文案改回描述性 uninstallDesc，且「尚未实现」那套键已全语种删除', async () => {
    const fs = await import('node:fs');
    const p = await repoPaths();
    const tsx = fs.readFileSync(p.aboutTsx, 'utf8');

    expect(
      tsx.includes('settings.about.uninstallDesc'),
      '功能已可用 ⇒ 描述必须说明它会做什么，而不是继续说「尚未实现」',
    ).toBe(true);

    for (const loc of LOCALES) {
      const about = JSON.parse(
        fs.readFileSync(p.path.join(p.localesDir, `${loc}.json`), 'utf8'),
      ).settings.about;
      expect(about.uninstallDesc, `${loc} 缺 uninstallDesc`).toBeTruthy();
      expect(about.uninstallTitle, `${loc} 缺 uninstallTitle`).toBeTruthy();
      // 这两句现在是谎话（功能已实现），留着早晚有人再把它渲染出来。
      expect(about.uninstallUnavailable, `${loc} 仍留着「尚未实现」文案`).toBeUndefined();
      expect(about.uninstallDisabledHint, `${loc} 仍留着禁用提示`).toBeUndefined();
    }
  });

  /**
   * **2026-07-29 载体迁移，门槛不变**：确认形态从自绘弹窗（两段 prompt `uninstallConfirm1/2`）
   * 改成原型的原地二次点击（`useConfirmTwice`，对齐 prototype :5185）后，那两段 prompt 已无承载它们
   * 的 DOM、键也随之删除。「六类目标 + 不可恢复」这条**红线原样保留**，只是改由**常驻**的
   * `uninstallDesc`（渲染在按钮左侧，不点也看得见）承载 —— 相比藏在弹窗里，触达更早而非更晚。
   * 按钮的确认态文案（`uninstallConfirmAgain`）另行钉死必须明说不可逆。
   */
  it('常驻描述必须说清「删什么」与「不可恢复」——破坏性操作不许含糊', async () => {
    const fs = await import('node:fs');
    const p = await repoPaths();
    const about = JSON.parse(
      fs.readFileSync(p.path.join(p.localesDir, 'zh-CN.json'), 'utf8'),
    ).settings.about;

    const desc: string = about.uninstallDesc;
    expect(desc).toBeTruthy();
    // 每加一个删除目标就必须在这里加一项 —— 「偏好域」是 2026-07-31 补的第七项
    // （macOS `~/Library/Preferences/<id>.plist`，app_language 写的 AppleLanguages 住在里面）。
    for (const what of ['开机自启', '提权助手', '内核', '用户配置', '更新包', '偏好', '应用本体']) {
      expect(desc, `描述漏了「${what}」—— 删了却没说，就是不知情同意`).toContain(what);
    }
    expect(desc, '必须明说不可恢复').toMatch(/不可恢复|无法复原|不可撤销/);
    // 已改原地二次点击 ⇒ 不许再宣称「双重确认」（说了两道闸、实际一道 = 反向的不诚实）。
    expect(desc, '确认形态已是原地二次点击，描述不得再说「双重确认」').not.toContain('双重确认');

    // 确认态按钮文案：这是用户按下不可逆动作前最后看到的一行字，必须自带「不可逆」语气。
    expect(about.uninstallConfirmAgain, 'zh-CN 缺 uninstallConfirmAgain').toBeTruthy();
    expect(about.uninstallConfirmAgain as string).toMatch(/不可逆|不可恢复|无法复原|不可撤销/);

    // 五语种都得有这两句，缺了会 fallback 到硬编码中文默认值（对非中文用户等于没提示）。
    for (const loc of LOCALES) {
      const a = JSON.parse(
        fs.readFileSync(p.path.join(p.localesDir, `${loc}.json`), 'utf8'),
      ).settings.about;
      expect(a.uninstallDesc, `${loc} 缺 uninstallDesc`).toBeTruthy();
      expect(a.uninstallConfirmAgain, `${loc} 缺 uninstallConfirmAgain`).toBeTruthy();
      // 旧的弹窗两段 prompt 已退役：留着早晚有人把它们再渲染成第二套确认形态。
      expect(a.uninstallConfirm1, `${loc} 仍留着已退役的 uninstallConfirm1`).toBeUndefined();
      expect(a.uninstallConfirm2, `${loc} 仍留着已退役的 uninstallConfirm2`).toBeUndefined();
    }
  });

  it('结果必须逐项呈现：五种结果态齐全，「未执行」也要显示，成功只挂在 complete 上', async () => {
    const fs = await import('node:fs');
    const p = await repoPaths();
    const tsx = fs.readFileSync(p.aboutTsx, 'utf8');

    // 真的遍历后端返回的每一步（而不是只挑成功的显示）。
    expect(
      tsx.includes('report.steps.map('),
      '结果没有逐项渲染 —— 一句「已卸载」正是这个功能当初被判「名不副实」的形态',
    ).toBe(true);

    // 五种结果态一个都不能少：少了 unsupported/notAttempted，Windows 便携版与 fail-fast
    // 中止的那几项就会在 UI 上凭空消失。
    for (const kind of ['done', 'skipped', 'unsupported', 'failed', 'notAttempted']) {
      expect(tsx, `结果态 ${kind} 没有对应文案映射`).toContain(kind);
    }

    // 「没抛异常」不等于卸载成功：外层信封恒 success:true，真值在 verdict。
    expect(
      tsx.includes("r.verdict === 'complete'"),
      '必须按 verdict 判成败，否则半成品会被显示成「已卸载」',
    ).toBe(true);

    for (const loc of LOCALES) {
      const about = JSON.parse(
        fs.readFileSync(p.path.join(p.localesDir, `${loc}.json`), 'utf8'),
      ).settings.about;
      for (const k of [
        'uninstallStepStopCore',
        'uninstallStepAutostart',
        'uninstallStepCacheDir',
        'uninstallStepPreferences',
        'uninstallStepHelper',
        'uninstallStepUserConfig',
        'uninstallStepAppBundle',
        'uninstallKindDone',
        'uninstallKindSkipped',
        'uninstallKindUnsupported',
        'uninstallKindFailed',
        'uninstallKindNotAttempted',
        'uninstallVerdictComplete',
        'uninstallVerdictIncomplete',
        'uninstallVerdictFailed',
      ]) {
        expect(about[k], `${loc} 缺 ${k}`).toBeTruthy();
      }
    }
  });

  it('守卫本身有牙：裸 disabled 检测不得把 disabled={...} 或注释里的字误判', () => {
    expect(BARE_DISABLED.test('  <Button\n    disabled\n  >')).toBe(true);
    expect(BARE_DISABLED.test('  <Button\n    disabled={uninstalling}\n  >')).toBe(false);
    expect(BARE_DISABLED.test('  // 把 disabled 改回 disabled={uninstalling}\n')).toBe(false);
    expect(BARE_DISABLED.test('  按钮状态（disabled）与文案一致\n')).toBe(false);
  });
});
