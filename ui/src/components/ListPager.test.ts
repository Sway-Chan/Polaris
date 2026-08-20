import { describe, expect, it } from 'vitest';
import { pageWindow } from './ListPager';

describe('pageWindow', () => {
  it('空结果保持一个零长度页面', () => {
    expect(pageWindow(0, 99, 50)).toEqual({ page: 0, pageCount: 1, start: 0, end: 0 });
  });

  it('分页只投影视窗，不丢掉末页余数', () => {
    expect(pageWindow(121, 1, 50)).toEqual({ page: 1, pageCount: 3, start: 50, end: 100 });
    expect(pageWindow(121, 2, 50)).toEqual({ page: 2, pageCount: 3, start: 100, end: 121 });
  });

  it('页码越界时夹取到仍存在的页面', () => {
    expect(pageWindow(121, -3, 50).page).toBe(0);
    expect(pageWindow(121, 30, 50).page).toBe(2);
  });

  it('拒绝无效页面容量', () => {
    expect(() => pageWindow(10, 0, 0)).toThrow(RangeError);
    expect(() => pageWindow(10, 0, 2.5)).toThrow(RangeError);
  });
});
