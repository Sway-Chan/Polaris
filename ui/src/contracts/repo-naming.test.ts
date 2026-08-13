/**
 * 仓库文案不得出现上游项目名。
 *
 * # 为什么要有这道门
 *
 * 本仓是独立产品，注释、文档、脚本、错误消息里出现另一个项目的名字有两个问题：对外它把本仓
 * 说成某个项目的衍生物；对内它让「溯源指针」和「品牌」混在一起——指针的价值是那个**符号/文件名**
 * （`src/main/services/xxx.ts`），项目名本身不携带信息。
 *
 * 2026-08-13 一次性清了 **2682 处 / 335 个文件**（`FlowZ` → `上游`，路径与脚本另行处理）。
 * 没有这道门的话，下一次从上游移植代码时会连注释一起抄回来，而它不会让任何测试变红。
 *
 * # 判据自身不能含那个词
 *
 * 断言里若直接写出被禁的字面量，这个文件自己就会命中 —— 要么门恒红，要么得给自己开豁免，
 * 而豁免会变成下一个盲区。故 needle **从片段拼出来**，本文件里任何位置都不出现完整串。
 * （同型教训：今天早些时候一条「冻结字面量」的门把期望值与被测常量写在同一文件，
 * 一次全局改名把两边一起改掉，变异不红。）
 */
import { describe, it, expect } from 'vitest';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('../../../', import.meta.url));

/** 被禁的项目名 —— 拼出来，避免本文件自己命中。 */
const FORBIDDEN = ['F', 'low', 'Z'].join('');

function scan(): string[] {
  try {
    // -I 跳二进制；排除构建产物与依赖树。grep 无命中时退出码 1，execFileSync 会抛 —— 那正是期望态。
    const out = execFileSync(
      'grep',
      [
        '-rIn',
        '-i',
        FORBIDDEN,
        '--exclude-dir=target',
        '--exclude-dir=node_modules',
        '--exclude-dir=.git',
        '--exclude-dir=dist',
        '.',
      ],
      { cwd: REPO, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
    );
    return out.split('\n').filter(Boolean);
  } catch (e) {
    const err = e as { status?: number; stdout?: string };
    if (err.status === 1) return []; // grep：无命中
    throw e;
  }
}

describe('仓库文案', () => {
  it('不出现上游项目名（大小写不敏感）', () => {
    const hits = scan().filter((l) => !l.includes('contracts/repo-naming.test.ts'));
    expect(
      hits,
      `仓库里出现了上游项目名（${hits.length} 处）。溯源要留的是符号与文件名，不是项目名 —— ` +
        `把「对齐 <项目名> \`foo\`」写成「对齐上游 \`foo\`」即可：\n${hits.slice(0, 20).join('\n')}`,
    ).toEqual([]);
  });

  it('扫描器自检：它确实能扫到东西（否则上面那条是空过）', () => {
    // 用一个必然存在的词证明 grep 这条通路是活的 —— 不然 grep 参数写错时上面那条会永远绿。
    const out = execFileSync(
      'grep',
      ['-rIl', 'polaris', '--exclude-dir=target', '--exclude-dir=node_modules', '--exclude-dir=.git', '.'],
      { cwd: REPO, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
    );
    expect(out.split('\n').filter(Boolean).length).toBeGreaterThan(20);
  });
});
