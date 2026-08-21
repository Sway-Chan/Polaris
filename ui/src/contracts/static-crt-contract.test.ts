/**
 * PKG-3 防回潮门：Windows 出包必须静态链接 CRT，且出包腿必须真的在断言它。
 *
 * # 背景（2026-08-19 真机实测的缺陷）
 *
 * 零 VC++ 运行库的干净机器上首装：`polaris.exe` 缺 `MSVcp140` 起不来、helper 缺
 * `VCRUNTIME140` 同病——「干净机器装完即起」的验收闭包被打破。
 * 修法 = `.cargo/config.toml` 给 msvc 目标开 `+crt-static`（所有自家 exe 不再依赖系统
 * VC++ 运行库；helper 是 SYSTEM 服务、无法靠 app-local DLL 兜底，静态链接是唯一双侧治法）。
 *
 * # 这门钉什么（源扫描；行为收据在 CI 出包腿）
 *
 * - `.cargo/config.toml` 的 msvc 目标段与 `target-feature=+crt-static` 在场——删掉/改坏
 *   即红（本地门牙；远端真门是 package.yml 的逐字节断言步）。
 * - `package.yml` 的断言步在场且形态完整：步骤名、两个产物路径、`grep -aqi` 的 **i**
 *   （大小写不敏感——实测导入名混合大小写，`MSVcp140` 形态；漏 i = 恒绿假门）、两个
 *   DLL 针。拆空 run 块留步骤名（本仓经典假绿形态）同样红。
 * - ci.yml 的 clippy 步**保留** `RUSTFLAGS: "-D warnings"` env（step 级）：它是全 workflow
 *   唯一被 env 覆盖而保持动态的步；Build/Test 无 env、windows 腿上会静态编（提前验证，见
 *   .cargo/config.toml 头注）。把这行 env 删了不会坏任何东西，但会静默抹掉这层分层事实。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));
const read = (rel: string): string => readFileSync(join(REPO_ROOT, rel), 'utf8');

describe('PKG-3：Windows 出包静态 CRT（配置 + 断言步 + 刻意的分层）', () => {
  it('.cargo/config.toml 给 msvc 目标开 +crt-static', () => {
    const cfg = read('.cargo/config.toml');
    expect(cfg.includes('[target.x86_64-pc-windows-msvc]'), 'msvc 目标段被删/改名').toBe(true);
    expect(
      cfg.includes('target-feature=+crt-static'),
      'crt-static flag 被改坏——干净机器 MSVCP140 起不来将回潮',
    ).toBe(true);
  });

  it('package.yml 的 CRT 断言步在场且不可拆空', () => {
    const wf = read('.github/workflows/package.yml');
    const stepName = 'Assert statically linked CRT (Windows, PKG-3)';
    expect(wf.includes(stepName), '断言步整步消失').toBe(true);
    // 钉 if 条件：改成永假即静默废掉 CI 门而其余针照绿（复审指出的洞 ①）。
    expect(
      wf.includes(`- name: ${stepName}\n        if: runner.os == 'Windows'`),
      '断言步的 Windows if 条件被改/删——步会被静默跳过',
    ).toBe(true);
    // 切出断言步本体（步骤名 → 下一个 `- name:`），exe 路径针必须落在**本步 run 块内**：
    // 全文级 includes 会被 portable 步的同名路径喂成恒绿空针（复审 Med-3）。
    const at = wf.indexOf(`- name: ${stepName}`);
    const next = wf.indexOf('- name:', at + 1);
    const step = wf.slice(at, next === -1 ? undefined : next);
    expect(
      step.includes('grep -aqi'),
      '断言必须大小写不敏感（-i）：实测导入名混合大小写，漏 i 是恒绿假门',
    ).toBe(true);
    for (const needle of ['msvcp140', 'vcruntime140']) {
      expect(step.includes(needle), `断言步丢了 ${needle} 针`).toBe(true);
    }
    for (const exe of ['target/release/polaris.exe', 'resources/win/polaris-helper.exe']) {
      expect(step.includes(exe), `断言步不再检查 ${exe}（产物路径变了门没跟？）`).toBe(true);
    }
  });

  it('ci.yml 的 clippy 步 RUSTFLAGS env 仍在（全 workflow 唯一动态 CRT 的步；Build/Test 静态编=提前验证）', () => {
    const ci = read('.github/workflows/ci.yml');
    expect(ci.includes('RUSTFLAGS: "-D warnings"'), '分层依据被删——见 .cargo/config.toml 头注').toBe(
      true,
    );
  });
});
