import { describe, it, expect } from 'vitest';
import { APP_CATEGORY_ALL, compareLabel, sortAppCategories } from './apppolicy-logic';

/** 六个真实类目 + 「全部」，故意乱序传入（防「输入本就有序」的假绿）。 */
const CATS = [
  { key: 'video', label: '视频' },
  { key: 'ai', label: 'AI' },
  { key: APP_CATEGORY_ALL, label: '全部' },
  { key: 'game', label: '游戏' },
  { key: 'social', label: '社交' },
  { key: 'tools', label: '工具' },
];

const labels = (cats: { label: string }[]) => cats.map((c) => c.label);

describe('compareLabel', () => {
  it('中文按拼音序，而非拉丁 locale 下的码位序', () => {
    // 社交(shejiao) < 游戏(youxi)；同一对在 en-US 整理下（汉字退化成码位序）恰好反号 —— 两边都断言
    // 才钉得住「locale 真的被传进去用了」：若实现吞掉 locale，两条必有一条转红。
    expect(compareLabel('社交', '游戏', 'zh-CN')).toBeLessThan(0);
    expect(compareLabel('社交', '游戏', 'en-US')).toBeGreaterThan(0);
  });

  it('英文按字母序', () => {
    expect(compareLabel('Games', 'Video', 'en-US')).toBeLessThan(0);
    expect(compareLabel('Video', 'Games', 'en-US')).toBeGreaterThan(0);
  });

  it('同名返回 0（相等判定，不靠符号猜）', () => {
    expect(compareLabel('工具', '工具', 'zh-CN')).toBe(0);
  });
});

describe('sortAppCategories', () => {
  it('zh-CN：全部恒首，其余按拼音（汉字整理把拉丁名 AI 排在汉字之后）', () => {
    expect(labels(sortAppCategories(CATS, 'zh-CN'))).toEqual([
      '全部',
      '工具',
      '社交',
      '视频',
      '游戏',
      'AI',
    ]);
  });

  it('en-US：全部（All）恒首，其余按字母序', () => {
    const en = [
      { key: 'video', label: 'Video' },
      { key: 'ai', label: 'AI' },
      { key: APP_CATEGORY_ALL, label: 'All' },
      { key: 'game', label: 'Games' },
      { key: 'social', label: 'Social' },
      { key: 'tools', label: 'Tools' },
    ];
    expect(labels(sortAppCategories(en, 'en-US'))).toEqual([
      'All',
      'AI',
      'Games',
      'Social',
      'Tools',
      'Video',
    ]);
  });

  it('「全部」的置首不靠它的标签碰巧排在前面：改成排序上最靠后的标签也仍在首位', () => {
    // 拼音序下 '龟' 排在全部真实类目之后；若置首逻辑被删成纯 label 排序，这条立刻转红。
    const cats = CATS.map((c) => (c.key === APP_CATEGORY_ALL ? { ...c, label: '龟' } : c));
    expect(labels(sortAppCategories(cats, 'zh-CN'))[0]).toBe('龟');
  });

  it('切语言重排：同一份 key 序列在 zh-CN 与 en-US 下不同（排序确实吃 locale）', () => {
    const keys = (l: string) => sortAppCategories(CATS, l).map((c) => c.key);
    expect(keys('zh-CN')).not.toEqual(keys('en-US'));
  });

  it('不就地改传入数组（调用方传的是模块级常量表）', () => {
    const input = [...CATS];
    sortAppCategories(input, 'zh-CN');
    expect(labels(input)).toEqual(labels(CATS));
  });

  it('不增不减，key 集合恒等', () => {
    const sorted = sortAppCategories(CATS, 'zh-CN');
    expect(sorted).toHaveLength(CATS.length);
    expect(new Set(sorted.map((c) => c.key))).toEqual(new Set(CATS.map((c) => c.key)));
  });
});
