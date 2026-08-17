/**
 * CI 工具链钉扎的守门测试。
 *
 * 2026-08-17 起 protoc / NASM 换成「固定版本 + 固定 URL + sha256」的自装内联步，当时常量在
 * `ci.yml` 与 `package.yml` 各一份，本文件用「两份逐字对拍」防漂。
 * 2026-08-18（CI-4）起 protoc 收敛到 `scripts/fetch-protoc.mjs`：常量只有一处，「两份对拍」
 * 这个失效模式对 protoc 消失，protoc 对拍部分退休，改为守「workflow 真的在调用脚本」与
 * 「脚本自己的钉扎表有牙」。NASM 仍内联两处，对拍照旧。
 *
 * # 判据
 * - protoc（workflow 侧）：两个文件都有真的 `run: node scripts/fetch-protoc.mjs`——
 *   只 grep 步骤名的话，把 run 块掏空、名字留着照样绿；
 * - protoc（脚本侧）：`ASSETS` 表恰 4 条、每条带 64 位 sha256；版本常量唯一；
 *   校验走 `createHash` 且比对不符会抛；`PROTOC_EXPECT` 仍经 `GITHUB_ENV` 导出
 *   （断言步的期望值来源，丢了它断言步拿不到期望）；
 * - NASM：两文件 `NASM_VERSION` 与 sha256 一致，且条数固定——两边一起删空也是「相等」，
 *   固定条数让删空转红；
 * - 两文件 NASM 步的 sha256 是真在校验（`sha256sum -c`）而不是摆着看；装配步不靠
 *   `command -v` 探测选工具；装完仍有 PATH 断言步（`[ "$got" = "$PROTOC_EXPECT" ]`）。
 *
 * # 这门抓不到什么（别当成「工具链钉扎都验过了」）
 * - **sha256 对不对**：只保证两处相同 / 表内形齐。两处一起写错，本门全绿；
 *   Linux 腿真正会红的是脚本里的 createHash 比对，win/mac 腿只有 CI 真跑才知道。
 * - **版本选得对不对**：选 35.1 的依据在 `scripts/fetch-protoc.mjs` 头注（一次性实测结论）。
 * - **URL 还在不在**：资产被上游删除只有真下载才知道。
 * - pnpm 在 **win/mac 打包腿**上的行为（CI-2 统一到 10 后，首个全矩阵之前的残余风险，登记在案）。
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));

const WORKFLOWS = ['ci.yml', 'package.yml'] as const;

/** protoc 官方 release 覆盖的平台数（`process.platform-process.arch` → 资产名）。 */
const PROTOC_PLATFORM_ROWS = 4;

function read(workflow: string): string {
  return readFileSync(join(REPO_ROOT, '.github/workflows', workflow), 'utf8');
}

/** 取 `NAME: '<value>'`（workflow step 的 env 钉扎）。 */
function envPin(src: string, name: string): string[] {
  return [...src.matchAll(new RegExp(`^\\s*${name}: '([^']+)'$`, 'gm'))].map((m) => m[1]);
}

/** 取独立成行的 `sha256='<64 位十六进制>'`（NASM 那份）。 */
function standaloneShaPins(src: string): string[] {
  return [...src.matchAll(/^\s*sha256='([0-9a-f]{64})'$/gm)].map((m) => m[1]).sort();
}

describe('CI 工具链钉扎守门', () => {
  const sources = Object.fromEntries(WORKFLOWS.map((w) => [w, read(w)])) as Record<
    (typeof WORKFLOWS)[number],
    string
  >;
  // CI-4 后 protoc 常量的唯一真值点；断言读的是源码文本（与其它 workflow 断言同款手法）。
  const fetchProtoc = readFileSync(join(REPO_ROOT, 'scripts/fetch-protoc.mjs'), 'utf8');

  it.each(WORKFLOWS)('%s 通过 scripts/fetch-protoc.mjs 装 protoc', (workflow) => {
    expect(
      /^ *run: node scripts\/fetch-protoc\.mjs\s*$/m.test(sources[workflow]),
      `${workflow} 里没有 \`run: node scripts/fetch-protoc.mjs\` —— ` +
        'protoc 装配被改回内联或掏空了；常量唯一真值在脚本里，别在 workflow 里再抄一份'
    ).toBe(true);
  });

  it('fetch-protoc.mjs 的钉扎表恰四平台且每条带 sha256，版本常量唯一', () => {
    const rows = [...fetchProtoc.matchAll(/asset: '([^']+)',\s*sha256: '([0-9a-f]{64})'/g)].map(
      (m) => `${m[1]}=${m[2]}`
    );
    expect(
      rows,
      `ASSETS 表应有 ${PROTOC_PLATFORM_ROWS} 条（linux/win/darwin-arm64/darwin-x64）——` +
        '少了某平台，那条腿的 runner 会在装配步当场红（fail-loud）；表被删空时本断言也必须红'
    ).toHaveLength(PROTOC_PLATFORM_ROWS);
    expect(new Set(rows).size, 'ASSETS 表里有重复条目').toBe(rows.length);
    const versions = [...fetchProtoc.matchAll(/^const PROTOC_VERSION = '([^']+)'/gm)].map(
      (m) => m[1]
    );
    expect(versions, 'PROTOC_VERSION 常量应恰有一处').toHaveLength(1);
  });

  it('fetch-protoc.mjs 的校验与 CI 装配线还在（钉执行形，防被注释/import/log 行喂饱）', () => {
    // 三条都钉「执行形」而不是裸词：CI-4 复审实证过裸词的喂饱路径——`createHash` 被 import 行
    // 喂饱、`PROTOC_EXPECT=libprotoc` 被 console.log 行喂饱、`GITHUB_PATH` 被注释喂饱、
    // `sha256 不符` 词在 throw 被改成 console.error 后仍绿（那等于校验静默放行，供应链牙全失效）。
    expect(
      fetchProtoc.includes('throw new Error(`sha256 不符'),
      'sha256 不符的处置必须是 throw（经 catch 转 exit 1）—— 换成 console.error 就成了静默放行'
    ).toBe(true);
    expect(
      fetchProtoc.includes("createHash('sha256')"),
      '脚本丢了 sha256 校验调用 —— 常量还在、校验没了，钉扎退化成装饰'
    ).toBe(true);
    expect(
      fetchProtoc.includes('appendFileSync(process.env.GITHUB_ENV, `PROTOC_EXPECT='),
      'PROTOC_EXPECT 不再经 GITHUB_ENV 真写入 —— 断言步拿不到期望值，安装步可静默 no-op'
    ).toBe(true);
    expect(
      fetchProtoc.includes('appendFileSync(process.env.GITHUB_PATH'),
      'CI 的 PATH 注册线没了 —— 装了但断言步解析不到；此处防整段被误删'
    ).toBe(true);
  });

  it('pnpm 版本两处一致且都已精确钉扎', () => {
    // CI-2（2026-08-18）：package.yml 从 9 统一到 10 之后，pnpm 钉扎就有了对拍对象（此前 ui.yml
    // 独一份，无从对拍）。两个 workflow 的安装机制不同（ui.yml 为 `cache: pnpm` 装在 setup-node
    // 之前、package.yml 装在其后），但**版本必须同**——分家 = 门腿与出包腿的 node_modules
    // 又不是同一套安装语义（pnpm 9 跑生命周期脚本、10 默认不跑），「门验过的」再度≠「出包用的」。
    const ui = envPin(read('ui.yml'), 'PNPM_VERSION');
    const pkg = envPin(sources['package.yml'], 'PNPM_VERSION');
    expect(ui, 'ui.yml 里没有精确钉扎的 PNPM_VERSION —— pnpm 又浮动了？').toHaveLength(1);
    expect(pkg, 'package.yml 里没有精确钉扎的 PNPM_VERSION —— 出包腿工具链不可复现').toHaveLength(1);
    expect(
      pkg[0],
      'ui 门与出包腿的 pnpm 版本不同：两侧装出来的 node_modules 不是一回事（见 CI-2）'
    ).toBe(ui[0]);
    // 版本同还不够，install 形态也要同（复审 Low 的补刀）：版本一致而一侧掏掉
    // `--frozen-lockfile` / 换安装器，lockfile 就能静默漂移——同一个坑的另一半。
    // 钉整段执行形而不是步骤名：掏空 run 块、名字留着照样绿是本文件记载过的失效形态。
    expect(
      read('ui.yml').includes('pnpm install --frozen-lockfile'),
      'ui.yml 的 install 丢了 --frozen-lockfile —— 门腿可以装出 lockfile 之外的依赖'
    ).toBe(true);
    expect(
      sources['package.yml'].includes('pnpm --dir ui install --frozen-lockfile'),
      'package.yml 的 install 丢了 --frozen-lockfile —— 出包腿可以装出门验过之外的依赖'
    ).toBe(true);
  });

  it('NASM 版本与 sha256 两处一致', () => {
    const ciVer = envPin(sources['ci.yml'], 'NASM_VERSION');
    const pkgVer = envPin(sources['package.yml'], 'NASM_VERSION');
    expect(ciVer).toHaveLength(1);
    expect(pkgVer).toHaveLength(1);
    expect(pkgVer[0], 'ci.yml 与 package.yml 的 NASM 版本不同').toBe(ciVer[0]);

    const ciSha = standaloneShaPins(sources['ci.yml']);
    const pkgSha = standaloneShaPins(sources['package.yml']);
    expect(ciSha, 'ci.yml 应恰有 1 条独立 sha256（NASM）').toHaveLength(1);
    expect(pkgSha, 'package.yml 应恰有 1 条独立 sha256（NASM）').toHaveLength(1);
    expect(pkgSha, 'ci.yml 与 package.yml 的 NASM sha256 漂移了').toEqual(ciSha);
  });

  it.each(WORKFLOWS)('%s 里 NASM 的 sha256 是真在校验，不是摆着看', (workflow) => {
    const src = sources[workflow];
    expect(
      src.includes('sha256sum -c -'),
      `${workflow} 里找不到 sha256sum 校验命令 —— NASM 常量还在，校验没了`
    ).toBe(true);
  });

  // `scripts/lib/extract-zip.mjs` 头注那条 🔴：解压器/校验器**按平台写死，不写「先试 A 失败退 B」**
  // —— 静默 fallback 会让「A 其实不在」永远不被观测到，换 runner 镜像时原样复发且报错点已漂走。
  // 这两步第一版正是写成了 `command -v` 探测（2026-08-17 复审时改掉），本条守它不回潮。
  // 只钉这三个**探测**形态：断言步里的 `command -v protoc` / `command -v nasm` 是**报告**用哪个
  // 二进制、不做分支，属于要保留的东西，故不在射程内。
  it.each(WORKFLOWS)('%s 的装配步不靠 `command -v` 探测选工具', (workflow) => {
    const src = sources[workflow];
    for (const probe of ['command -v unzip', 'command -v 7z', 'command -v sha256sum']) {
      expect(
        src.includes(probe),
        `${workflow} 出现了 \`${probe}\` —— 工具选择又变成了静默 fallback，` +
          `见 scripts/lib/extract-zip.mjs 头注那条 🔴：判据要按平台写死并把实际用的工具打进日志`
      ).toBe(false);
    }
  });

  it.each(WORKFLOWS)('%s 装完仍断言 PATH 上解析到的就是钉扎的那份', (workflow) => {
    const src = sources[workflow];
    // 匹配断言里**真正的比较**，不是步骤名：只 grep 步骤名的话，把 run 块掏空、名字留着照样绿。
    expect(
      src.includes('[ "$got" = "$PROTOC_EXPECT" ]'),
      `${workflow} 丢了 protoc 的 PATH 断言 —— 安装步变成静默 no-op 也没人知道`
    ).toBe(true);
  });
});
