import { describe, it, expect } from 'vitest';
import { willRestartOnSelect } from './pending-select-hint';

describe('willRestartOnSelect', () => {
  it('选中「待入池」(added) 节点 → 将重启', () => {
    expect(willRestartOnSelect({ added: ['a'], modified: [], removed: [], restartDeferred: false }, 'a')).toBe(true);
  });

  it('选中「待生效」(modified) 节点 → 将重启', () => {
    expect(willRestartOnSelect({ added: [], modified: ['b'], removed: [], restartDeferred: false }, 'b')).toBe(true);
  });

  it('不在差集内的节点 → 不重启', () => {
    expect(willRestartOnSelect({ added: ['a'], modified: ['b'], removed: [], restartDeferred: false }, 'c')).toBe(false);
  });

  // `removed` 里的节点已不在 config.servers → 出口选单列不出它，本函数不会被拿它的 id 调用。
  // 钉住「不看 removed」这个刻意的取舍：并进来是死代码，且会暗示「可以选一个已删节点」。
  it('removed 内的节点 → 不参与判定（恒 false）', () => {
    expect(willRestartOnSelect({ added: [], modified: [], removed: ['gone'], restartDeferred: false }, 'gone')).toBe(
      false
    );
  });

  // 核未运行时差集恒空 → 不得误报
  it('空差集 → 恒 false', () => {
    expect(willRestartOnSelect({ added: [], modified: [], removed: [], restartDeferred: false }, 'a')).toBe(false);
  });

  // 核未运行时 getPendingChanges 可能返回缺字段的畸形对象 → 不得抛错，按「不重启」降级
  it('pending 缺字段（畸形对象）→ 不抛错，恒 false', () => {
    expect(
      willRestartOnSelect({} as unknown as Parameters<typeof willRestartOnSelect>[0], 'a')
    ).toBe(false);
  });
});
