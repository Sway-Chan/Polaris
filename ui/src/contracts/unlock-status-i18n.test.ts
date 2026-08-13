/**
 * 解锁检测状态文案的五语种完整性门。
 *
 * # 缺陷原型（2026-08-11 用户反馈）
 *
 * `UnlockBadge` 的 tooltip 与 aria-label 此前直接拼 `status` —— 那是**机器枚举**
 * （`idle`/`checking`/`ok`/`partial`/`blocked`/`restricted`/`timeout`）。于是无论界面切到哪种语言，
 * 悬停看到的恒为英文枚举名。这类缺陷不会让任何测试变红：字符串拼得出来、渲染也正常，
 * 只是**内容是给机器看的**。
 *
 * # 判据取自类型，不取自「当时补了哪几个键」
 *
 * 枚举源是 `contracts/unlock-detection.ts` 的 `UnlockStatus` 联合类型。本门从**源码解析**它，
 * 再要求每个成员在**五个** locale 里都有键。写死一份状态清单等于把「将来新增状态值」这件事
 * 留成盲区 —— 那正是本缺陷的同型。
 *
 * 反向也锁：locale 里多出来的键同样红，否则删掉一个状态值后残留的文案会一直躺在五个文件里。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ru', 'fa'] as const;

/** 从 `UnlockStatus = 'a' | 'b' | …` 的源码里解析成员集合。 */
function unlockStatusMembers(): string[] {
  const src = readFileSync(fileURLToPath(new URL('./unlock-detection.ts', import.meta.url)), 'utf8');
  const m = src.match(/export type UnlockStatus\s*=\s*([^;]+);/);
  if (!m) throw new Error('解析不到 UnlockStatus —— 类型改名/改形了，先确认再动本门');
  return [...m[1].matchAll(/'([^']+)'/g)].map((x) => x[1]);
}

function localeStatusKeys(loc: string): Record<string, string> {
  const raw = readFileSync(
    fileURLToPath(new URL(`../i18n/locales/${loc}.json`, import.meta.url)),
    'utf8',
  );
  const data = JSON.parse(raw) as { home?: { unlockStatus?: Record<string, string> } };
  return data.home?.unlockStatus ?? {};
}

describe('解锁状态文案 —— 五语种完整性', () => {
  const members = unlockStatusMembers();

  it('解析得到的状态成员数量合理（解析器本身没瞎）', () => {
    expect(members.length).toBeGreaterThanOrEqual(5);
    expect(members).toContain('ok');
  });

  for (const loc of LOCALES) {
    it(`${loc}：每个 UnlockStatus 成员都有文案，且没有多余键`, () => {
      const keys = localeStatusKeys(loc);
      expect(Object.keys(keys).sort(), `${loc} 的 home.unlockStatus 与 UnlockStatus 成员不一致`).toEqual(
        [...members].sort(),
      );
      for (const [k, v] of Object.entries(keys)) {
        expect(v.trim(), `${loc}.home.unlockStatus.${k} 是空串 —— 悬停会显示一段空白`).not.toBe('');
      }
    });
  }

  it('徽章渲染真的走了 i18n（拼回裸 status 即红）', () => {
    const src = readFileSync(
      fileURLToPath(new URL('../components/screens/home/HomeScreen.tsx', import.meta.url)),
      'utf8',
    );
    expect(src, 'UnlockBadge 没走 t(home.unlockStatus.*)').toContain(
      't(`home.unlockStatus.${status}`)',
    );
    // 变异靶：把 statusText 换回 status → 下面两条转红。
    expect(src).toContain('aria-label={`${svc.name}: ${statusText}');
    expect(src).toContain('data-tip={`${svc.name} · ${statusText}');
  });
});
