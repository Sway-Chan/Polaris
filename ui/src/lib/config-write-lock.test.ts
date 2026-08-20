import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { describe, it, expect, beforeEach } from 'vitest';
import { withConfigWriteLock, resetConfigWriteLock } from './config-write-lock';

beforeEach(() => resetConfigWriteLock());

describe('withConfigWriteLock —— 串行语义', () => {
  // 变异对照：把实现改成 `return run()`（不排队）→ 本条转红（两段会交错成 a1 b1 a2 b2）。
  it('后一次必须等前一次**落定**才进临界区（不是交错）', async () => {
    const log: string[] = [];
    const task = (tag: string) => async () => {
      log.push(`${tag}-enter`);
      await Promise.resolve();
      await Promise.resolve();
      log.push(`${tag}-exit`);
    };
    await Promise.all([withConfigWriteLock(task('a')), withConfigWriteLock(task('b'))]);
    expect(log).toEqual(['a-enter', 'a-exit', 'b-enter', 'b-exit']);
  });

  // 变异对照：把 `tail.then(run, run)` 改成 `tail.then(run)` → 前一次失败后队列永久堵死 → 本条超时/转红。
  // 一次保存失败把后续保存全堵住，比并发冲突更糟：用户再也存不进任何东西，且没有任何提示。
  it('前一次失败不得堵死后一次', async () => {
    const ran: string[] = [];
    const boom = withConfigWriteLock(async () => {
      ran.push('boom');
      throw new Error('第一次挂了');
    });
    const after = withConfigWriteLock(async () => {
      ran.push('after');
      return 'ok';
    });
    await expect(boom).rejects.toThrow('第一次挂了');
    await expect(after).resolves.toBe('ok');
    expect(ran).toEqual(['boom', 'after']);
  });

  // 成败必须原样透传：闸门只管顺序，不得吞掉调用方要判的结果。
  it('返回值与异常原样透传给调用方', async () => {
    await expect(withConfigWriteLock(async () => 42)).resolves.toBe(42);
    await expect(withConfigWriteLock(async () => Promise.reject(new Error('x')))).rejects.toThrow('x');
  });
});

/**
 * **接线守卫**：闸门只有在「主窗里每一次 `api.config.save` 都在它里面」时才成立。
 * 漏一处就重新打开 `performSave` 的 `get()`→`save()` 窗口 —— 那正是「盘存好了、条说失败了」的成因。
 *
 * 用 `git grep` 扫源码而非依赖人工记忆：新增一处未入队的全量写立刻转红。
 * 托盘（`ui/src/tray/`）**刻意排除**：它是另一个 webview，模块级队列跨不过去，那边的写冲突由
 * 后端 `baseVersion` 检出机制负责（见闸门头注的射程边界）。
 */
describe('接线：主窗里的全量配置写必须都在闸门内', () => {
  const SRC = new URL('..', import.meta.url).pathname; // ui/src/

  /** 递归收集 ui/src 下的源码文件（去掉测试）。不含子进程、不依赖 git 索引（新文件也扫得到）。 */
  function sources(dir = SRC, out: string[] = []): string[] {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) sources(p, out);
      else if (/\.(ts|tsx)$/.test(e.name) && !e.name.includes('.test.')) out.push(p);
    }
    return out;
  }

  /**
   * 去注释后再扫 —— **必须**：本文件与闸门自身的文档注释里都逐字写着这些调用，只扫原文的话
   * 「把真代码改坏、只留注释」会照样绿（无牙），而新增一处文档提及又会误红。同
   * `ipc-channel-bypass-wiring.test.ts` 的 `code()` 与 Rust 侧 `method_body` 的同款纪律。
   */
  const stripComments = (s: string): string =>
    s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^[ \t]*\/\/.*$/gm, '');

  /** 命中 `needle` 的文件（相对 ui/src 的路径），排除 `skip` 前缀。 */
  const filesWith = (needle: string, skip: readonly string[] = []): string[] =>
    sources()
      .filter((p) => stripComments(readFileSync(p, 'utf8')).includes(needle))
      .map((p) => p.slice(SRC.length))
      .filter((rel) => !skip.some((s) => rel.startsWith(s)))
      .sort();

  // 变异对照：在主窗任何地方新加一处裸 `api.config.save(...)`（不入队）→ 本条转红。
  // 托盘刻意排除：另一个 webview，模块级队列跨不过去（见闸门头注射程边界）。
  it('全量写只出现在两个已入队的位置', () => {
    expect(filesWith('api.config.save(', ['tray/'])).toEqual([
      'store/app-store.ts',
      'store/staged-config-store.ts',
    ]);
  });

  // 变异对照：把任一处的 `withConfigWriteLock(...)` 拆掉、只留裸调用 → 对应断言转红。
  it('三处都真的裹在 withConfigWriteLock 里', () => {
    const appStore = readFileSync(join(SRC, 'store/app-store.ts'), 'utf8');
    expect(appStore).toContain(
      'withConfigWriteLock(() => api.config.save(config))'
    );
    expect(appStore).toContain(
      'withConfigWriteLock(() => api.server.switch(serverId))'
    );
    // performSave 是薄壳：整个函数体（**含开头读 entries**）都在临界区内。
    expect(readFileSync(join(SRC, 'store/staged-config-store.ts'), 'utf8')).toContain(
      'withConfigWriteLock(() => performSaveLocked(set, get, conflictResolved))'
    );
  });

  // 不可嵌套（临界区内再入队 = 自锁）。当前三个使用点互不调用；新增使用点必须先读头注那条。
  it('闸门只有三个生产使用点', () => {
    const appStore = readFileSync(join(SRC, 'store/app-store.ts'), 'utf8');
    const stagedStore = readFileSync(join(SRC, 'store/staged-config-store.ts'), 'utf8');
    expect((appStore.match(/withConfigWriteLock\(\(\) =>/g) ?? []).length).toBe(2);
    expect((stagedStore.match(/withConfigWriteLock\(\(\) =>/g) ?? []).length).toBe(1);
    expect(filesWith('withConfigWriteLock(() =>', ['config-write-lock.ts'])).toEqual([
      'store/app-store.ts',
      'store/staged-config-store.ts',
    ]);
  });
});
