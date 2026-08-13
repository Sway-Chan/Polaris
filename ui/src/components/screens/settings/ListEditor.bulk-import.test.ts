/**
 * ListEditor 批量导入 —— 守「控件名不副实」这个缺陷的根因。
 *
 * 旧实现是 `onChange([...value,'','',''])`：按钮叫「批量导入」，做的却是追加三个空框，
 * 没有任何粘贴/拆分/去重。本门断言的是**导入语义真的存在**（拆分 + 去重 + 上限），
 * 一旦有人把实现退回「追加空行」，这里的每一条都会红。
 */
import { describe, it, expect } from 'vitest';
import { parseBulkEntries } from './ListEditor';

describe('parseBulkEntries —— 真批量导入的解析与合并', () => {
  it('换行与逗号都是分隔符，条目去空白', () => {
    expect(parseBulkEntries('10.0.0.0/8\n 192.168.0.0/16 ,172.16.0.0/12', [])).toEqual([
      '10.0.0.0/8',
      '192.168.0.0/16',
      '172.16.0.0/12',
    ]);
  });

  it('空行/纯空白/连续分隔符一律丢弃（不再产生空框——正是旧实现的全部行为）', () => {
    expect(parseBulkEntries('a\n\n , \n,,b\n', [])).toEqual(['a', 'b']);
    // 反向锚点：粘贴纯空白必须什么都不加，而不是"加几个空的"。
    expect(parseBulkEntries('\n , \n', ['x'])).toEqual(['x']);
  });

  it('追加而非替换：既有条目原样保留在前', () => {
    expect(parseBulkEntries('c', ['a', 'b'])).toEqual(['a', 'b', 'c']);
  });

  it('与既有条目去重，大小写不敏感（域名/DoH URL 场景）', () => {
    expect(parseBulkEntries('Example.COM\nnew.com', ['example.com'])).toEqual([
      'example.com',
      'new.com',
    ]);
  });

  it('批内互相去重（同一次粘贴里出现两遍只进一条）', () => {
    expect(parseBulkEntries('a\nA\na', [])).toEqual(['a']);
  });

  it('既有的空行不吃掉粘贴内容的第一条（去重基准剔空串）', () => {
    // 用户点过「添加」留下一个空框后再粘贴 —— 空框保留，粘贴内容照常进。
    expect(parseBulkEntries('a\nb', [''])).toEqual(['', 'a', 'b']);
  });

  it('尊重 max 上限：达到即停，不报错也不越界（对齐「添加」按钮的 atCap 语义）', () => {
    expect(parseBulkEntries('a\nb\nc', [], 2)).toEqual(['a', 'b']);
    expect(parseBulkEntries('a\nb', ['x'], 1)).toEqual(['x']);
    // max 未给 = 无上限。
    expect(parseBulkEntries('a\nb\nc', []).length).toBe(3);
  });

  it('不修改入参数组（调用方拿到的是新列表，旧引用不被就地改写）', () => {
    const existing = ['a'];
    const out = parseBulkEntries('b', existing);
    expect(existing).toEqual(['a']);
    expect(out).not.toBe(existing);
  });
});
