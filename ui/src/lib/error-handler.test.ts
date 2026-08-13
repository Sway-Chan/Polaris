/**
 * `proxyErrorCategory` 单测：后端错误码 → ErrorCategory 的分类映射。
 *
 * 只覆盖「核在跑但能力/流量降级」这族非终态码——它们最容易漏映射：新增码若忘了进 switch，
 * 会静默落到 `default: null`，调用方回落到中文字符串匹配 fallback，看着不报错、实则分类失效。
 * 本函数是纯映射、无副作用出口，唯一的 mock 是 `../i18n`——它在模块加载期就摸 `document`
 * （`applyDocumentDirection`），而 vitest 跑在 `environment: 'node'` 下没有 DOM。
 */
import { describe, it, expect, vi } from 'vitest';

vi.mock('../i18n', () => ({ default: { t: (k: string) => k } }));

import { proxyErrorCategory, ErrorCategory } from './error-handler';
import { ProxyErrorCode } from '../contracts/types';

describe('proxyErrorCategory（错误码 → 分类）', () => {
  it.each([
    ProxyErrorCode.SYSTEM_PROXY_FAILED,
    ProxyErrorCode.EXIT_MISMATCH,
    ProxyErrorCode.RULE_RESOURCES_MISSING,
  ])('核在跑的降级码 %s → System（不得落 null）', (code) => {
    expect(proxyErrorCategory(code)).toBe(ErrorCategory.System);
  });

  it('未知码 / 非法值 → null（调用方据此回落字符串匹配）', () => {
    expect(proxyErrorCategory(ProxyErrorCode.UNKNOWN)).toBeNull();
    expect(proxyErrorCategory('ENOENT')).toBeNull();
    expect(proxyErrorCategory(undefined)).toBeNull();
  });
});
