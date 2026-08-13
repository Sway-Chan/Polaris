/**
 * `event:proxyError` 分腿路由单测（对齐 App.tsx 头注「代理错误」小节）。
 *
 * 只测导出的 `handleProxyErrorEvent`（不渲染 App —— vitest `environment: 'node'`，全仓无组件渲染测试）。
 * mock 掉 `./lib/error-handler`（toast）与 `./lib/desktop-notify`（notifyDesktop）——两者是本函数
 * 唯一的可观察副作用出口，其余（React/zustand/AppShell 等）只是模块图谱的一部分，无需关心。
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';

const { toastErrorMock, toastWarningMock, toastInfoMock, notifyDesktopMock } = vi.hoisted(() => ({
  toastErrorMock: vi.fn(),
  toastWarningMock: vi.fn(),
  toastInfoMock: vi.fn(),
  notifyDesktopMock: vi.fn(),
}));
vi.mock('./lib/error-handler', () => ({
  toast: {
    error: toastErrorMock,
    warning: toastWarningMock,
    success: vi.fn(),
    info: toastInfoMock,
  },
}));
vi.mock('./lib/desktop-notify', () => ({
  notifyDesktop: notifyDesktopMock,
  setDesktopNotificationsEnabled: vi.fn(),
}));

/**
 * `<html dir/lang>` 的落点桩。
 *
 * App.tsx 的模块图谱含 `i18n/index.ts`（语言水合腿的调用点），而后者在**模块加载期**就写
 * `document.documentElement`；本仓 vitest 是 node 环境、有意不引 jsdom（`vite.config.ts:74`），
 * 故沿用既有先例（`i18n/language-hydration.test.ts`、`settings/terminal-env-and-fold.test.tsx`）
 * 先立桩、再**动态** import —— 静态 import 会被提升到桩之前，那正是这条腿曾经炸掉的方式。
 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '' },
};

const { handleProxyErrorEvent } = await import('./App');
import { isProxyErrorCode } from './contracts/types';
// 认领闸门用**真实实现**（不 mock）：本组要验的正是「事件腿与认领闸门的合璧」，mock 掉闸门等于
// 把被测的接线换成桩，两边各自绿、合起来仍双报。
import { withProxyStartClaim, resetProxyStartClaim } from './lib/proxy-start-claim';

/** 对齐 App.tsx 里 `t(key, fallback)` 的真实用法（第二参恒为字符串兜底）。 */
const t = vi.fn((key: string, fallback?: unknown) =>
  typeof fallback === 'string' ? fallback : key
);

describe('handleProxyErrorEvent（代理错误分腿）', () => {
  let refreshProxyStatus: () => Promise<void>;

  beforeEach(() => {
    toastErrorMock.mockClear();
    toastWarningMock.mockClear();
    toastInfoMock.mockClear();
    notifyDesktopMock.mockClear();
    t.mockClear();
    refreshProxyStatus = vi.fn(async () => {});
    // 认领是模块级单例，跨用例存活 —— 不复位则前一条用例的宽限尾巴会把后一条吞成静默。
    resetProxyStartClaim();
  });

  it.each(['PROCESS_EXITED', 'AUTO_RESTART_FAILED'])(
    '崩溃腿 %s → 刷连接态 + 断开 toast + 桌面通知',
    (errorCode) => {
      handleProxyErrorEvent({ errorCode }, { t, refreshProxyStatus });
      expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
      expect(toastErrorMock).toHaveBeenCalledWith('home.proxyCrashed');
      expect(toastWarningMock).not.toHaveBeenCalled();
      expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
    }
  );

  // ── 本组的核心不变式：**i18n 键压过后端 message，不是反过来** ────────────────────
  //
  // 后端 `emit_proxy_error(message, error_code)` 两参皆非可选，`message` 是 Rust 侧写死的中文串。
  // 此前六处都写成 `data.message || t(key)` ⇒ `||` 右边永远短路不到，那些 key 是死键，
  // ru/fa 用户在最高频的错误路径上看到中文。下面几条带 message 的用例就是那条回归的哨兵：
  // 只要有人把优先级换回去，断言拿到的就会是中文串而不是键名。
  it.each(['SYSTEM_PROXY_FAILED', 'EXIT_MISMATCH'])(
    '出口误导腿 %s → warning toast 用 i18n 键（**压过**后端中文 message）+ 桌面通知，不刷连接态',
    (errorCode) => {
      handleProxyErrorEvent(
        { errorCode, message: '核在跑但实际出口 ≠ 选中节点' },
        { t, refreshProxyStatus }
      );
      expect(refreshProxyStatus).not.toHaveBeenCalled();
      expect(toastErrorMock).not.toHaveBeenCalled();
      expect(toastWarningMock).toHaveBeenCalledTimes(1);
      expect(toastWarningMock).toHaveBeenCalledWith('home.proxyMisdirected');
      // 后端那句中文一个字都不该出现在 toast 里。
      expect(toastWarningMock.mock.calls[0][0]).not.toContain('核在跑');
      expect(toastWarningMock.mock.calls[0][0]).not.toContain('已断开');
      expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
    }
  );

  it('能力降级腿 RULE_RESOURCES_MISSING → warning toast 用 i18n 键（压过 message）+ 桌面通知', () => {
    handleProxyErrorEvent(
      { errorCode: 'RULE_RESOURCES_MISSING', message: '规则资源 geosite-cn 缺少本地副本' },
      { t, refreshProxyStatus }
    );
    expect(refreshProxyStatus).not.toHaveBeenCalled();
    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastWarningMock).toHaveBeenCalledTimes(1);
    expect(toastWarningMock).toHaveBeenCalledWith('home.ruleResourcesMissing');
    expect(toastWarningMock.mock.calls[0][0]).not.toContain('geosite-cn');
    expect(toastWarningMock.mock.calls[0][0]).not.toContain('已断开');
    expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
  });

  // 无 message 时同样落到**本码自己的**键（含可操作指引），不得复用出口误导腿那条
  // ——两者用户下一步动作不同，串了等于把「去规则资源页下载」这条指引冲掉。
  it('RULE_RESOURCES_MISSING 无 message → 仍是本码专属键 + 专属通知 key', () => {
    handleProxyErrorEvent({ errorCode: 'RULE_RESOURCES_MISSING' }, { t, refreshProxyStatus });
    expect(toastWarningMock).toHaveBeenCalledWith('home.ruleResourcesMissing');
    expect(notifyDesktopMock).toHaveBeenCalledWith(
      'notify.ruleResourcesMissing.title',
      'notify.ruleResourcesMissing.body'
    );
  });

  it("isProxyErrorCode('RULE_RESOURCES_MISSING') === true", () => {
    expect(isProxyErrorCode('RULE_RESOURCES_MISSING')).toBe(true);
  });

  it('STARTUP_FAILED → 不重复报（发起方自己 toast，此处零弹）', () => {
    handleProxyErrorEvent({ errorCode: 'STARTUP_FAILED' }, { t, refreshProxyStatus });
    expect(refreshProxyStatus).not.toHaveBeenCalled();
    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastWarningMock).not.toHaveBeenCalled();
    expect(notifyDesktopMock).not.toHaveBeenCalled();
  });

  it("isProxyErrorCode('EXIT_MISMATCH') === true", () => {
    expect(isProxyErrorCode('EXIT_MISMATCH')).toBe(true);
  });

  // ── TUN 提权门两码 ─────────────────────────────────────────────────────────
  //
  // 这两条腿此前**整条落空**（函数末尾无 else，后端发了码前端直接丢）。发起方常常没人在 await：
  // 托盘切档位 / 启动自动连接 / switchMode 去抖重启都不经 Home 连接按钮 —— 真机反馈「点了没反应」。

  it('HELPER_GATE_ABORTED（用户取消）→ 刷连接态 + 中性 info，不报错、不发桌面通知', () => {
    handleProxyErrorEvent({ errorCode: 'HELPER_GATE_ABORTED' }, { t, refreshProxyStatus });
    // 核未起（终态）→ 必须刷，否则 UI 停在假「已连接」。
    expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
    expect(toastInfoMock).toHaveBeenCalledWith('errors.helperGateAborted');
    // 用户自己点的取消不是错误：红 toast 会把「我不想装」渲染成「出错了」。
    expect(toastErrorMock).not.toHaveBeenCalled();
    expect(toastWarningMock).not.toHaveBeenCalled();
    // 自己点的取消再推一条系统通知 = 噪音。
    expect(notifyDesktopMock).not.toHaveBeenCalled();
  });

  it('HELPER_NOT_INSTALLED（装不上）→ 刷连接态 + error toast 用 i18n 键（压过 message）+ 桌面通知', () => {
    handleProxyErrorEvent(
      { errorCode: 'HELPER_NOT_INSTALLED', message: 'helper 尚未安装' },
      { t, refreshProxyStatus }
    );
    expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
    expect(toastErrorMock).toHaveBeenCalledWith('errors.helperNotInstalledDesc');
    expect(toastInfoMock).not.toHaveBeenCalled();
    // 窗口常已收进托盘（托盘切档位触发），应用内 toast 送不到 → 桌面通知是唯一送达路径。
    expect(notifyDesktopMock).toHaveBeenCalledWith(
      'notify.helperNotInstalled.title',
      'notify.helperNotInstalled.body'
    );
  });

  // 两码**不得合并**：用户下一步动作相反（去装 vs 什么都不做）。合并会把可操作指引冲掉，
  // 或反过来对刚拒绝过的用户再催一遍。
  it('两码各用各的兜底文案与 toast 等级（不串腿）', () => {
    handleProxyErrorEvent({ errorCode: 'HELPER_NOT_INSTALLED' }, { t, refreshProxyStatus });
    const notInstalled = toastErrorMock.mock.calls[0][0];
    toastErrorMock.mockClear();
    handleProxyErrorEvent({ errorCode: 'HELPER_GATE_ABORTED' }, { t, refreshProxyStatus });
    expect(toastInfoMock.mock.calls[0][0]).not.toBe(notInstalled);
    expect(toastErrorMock).not.toHaveBeenCalled();
  });

  it.each(['HELPER_NOT_INSTALLED', 'HELPER_GATE_ABORTED'])(
    'isProxyErrorCode(%s) === true（契约认这两个码，否则前端 err.code 会被判为脏值丢弃）',
    (c) => {
      expect(isProxyErrorCode(c)).toBe(true);
    }
  );

  // ── root 孤儿阻断腿（此前**完全不路由**，本码在托盘/自动连接等无人 await 的入口是静默丢弃） ──

  it('ROOT_ORPHAN_BLOCKED → 刷连接态 + error toast **原样带 pid 的后端 message**（不经 i18n 键）+ 桌面通知', () => {
    handleProxyErrorEvent(
      { errorCode: 'ROOT_ORPHAN_BLOCKED', message: '上次遗留的 sing-box 核（pid [12345]）以管理员权限运行且无法清理，它占用着内核缓存文件，任何模式都无法启动。请安装/修复 Helper 后重试，或手动执行：sudo kill -9 12345' },
      { t, refreshProxyStatus }
    );
    // 核未起（终态）→ 必须刷，否则 UI 停在假「已连接」。
    expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
    // 本码在门的豁免表里判断过后决定不配 i18n 键：message 带着 pid，是用户唯一能据以行动的信息，
    // 键只能给同义反复。断言 toast 里**必须**能看到那个 pid —— 否则用户不知道该 kill 哪一个。
    expect(toastErrorMock).toHaveBeenCalledTimes(1);
    expect(toastErrorMock.mock.calls[0][0]).toContain('12345');
    expect(notifyDesktopMock).toHaveBeenCalledWith(
      'notify.rootOrphanBlocked.title',
      'notify.rootOrphanBlocked.body'
    );
  });

  it("isProxyErrorCode('ROOT_ORPHAN_BLOCKED') === true（否则前端 err.code 会被判为脏值丢弃）", () => {
    expect(isProxyErrorCode('ROOT_ORPHAN_BLOCKED')).toBe(true);
  });

  // ── 与发起方（Home 连接按钮）的去重：认领闸门 ─────────────────────────────
  //
  // 这三码（HELPER 两码 + ROOT_ORPHAN_BLOCKED）是后端**双出口**（emit 事件 + 让 api.proxy.start
  // reject）。发起方在场时两条腿都报 ⇒ 同一次失败弹两遍（NOT_INSTALLED 更是 toast + 桌面通知 +
  // 「去安装」模态三重），违反本文件 §STARTUP_FAILED 的既有约定。但事件腿**不能**整条忽略——
  // 没人 await 的入口（托盘/自动连接/去抖重启）会因此静默。下面两组分别钉住这两个相反方向的失效。

  describe('认领期内（Home 连接按钮已发起）→ 提示让位给 await 腿', () => {
    /** 模拟「本窗口发起的起核失败」：认领包裹内 reject，落定后事件才到达（最坏顺序）。 */
    async function claimFailedStart(): Promise<void> {
      await withProxyStartClaim(() => Promise.reject(new Error('helper 门失败'))).catch(() => {});
    }

    it('HELPER_GATE_ABORTED：事件腿零提示（否则与 Home 的 info toast 双报）', async () => {
      await claimFailedStart();
      handleProxyErrorEvent({ errorCode: 'HELPER_GATE_ABORTED' }, { t, refreshProxyStatus });
      expect(toastInfoMock).not.toHaveBeenCalled();
      expect(toastErrorMock).not.toHaveBeenCalled();
      expect(notifyDesktopMock).not.toHaveBeenCalled();
    });

    it('HELPER_NOT_INSTALLED：事件腿零提示且不发桌面通知（否则 toast+通知+模态三重）', async () => {
      await claimFailedStart();
      handleProxyErrorEvent({ errorCode: 'HELPER_NOT_INSTALLED' }, { t, refreshProxyStatus });
      expect(toastErrorMock).not.toHaveBeenCalled();
      expect(toastInfoMock).not.toHaveBeenCalled();
      // 用户正盯着连接按钮，系统通知在此刻纯属噪音。
      expect(notifyDesktopMock).not.toHaveBeenCalled();
    });

    it.each(['HELPER_GATE_ABORTED', 'HELPER_NOT_INSTALLED', 'ROOT_ORPHAN_BLOCKED'])(
      '%s：认领期内仍必须刷连接态（await 腿并不刷，漏刷则 UI 停在假「已连接」）',
      async (errorCode) => {
        await claimFailedStart();
        handleProxyErrorEvent({ errorCode }, { t, refreshProxyStatus });
        expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
      }
    );

    it('ROOT_ORPHAN_BLOCKED：事件腿零提示且不发桌面通知（否则与 await 腿双报）', async () => {
      await claimFailedStart();
      handleProxyErrorEvent({ errorCode: 'ROOT_ORPHAN_BLOCKED' }, { t, refreshProxyStatus });
      expect(toastErrorMock).not.toHaveBeenCalled();
      expect(notifyDesktopMock).not.toHaveBeenCalled();
    });

    // 认领的射程仅限提权门两码 + ROOT_ORPHAN_BLOCKED。若把认领判定提到函数顶部（或扩到其它码），
    // 一次连接按钮点击就会顺带吞掉这 2s 内任何来源的崩溃/出口误导告警 —— 凭空制造静默。
    it('认领不外溢：认领期内的崩溃腿照常报（PROCESS_EXITED）', async () => {
      await claimFailedStart();
      handleProxyErrorEvent({ errorCode: 'PROCESS_EXITED' }, { t, refreshProxyStatus });
      expect(toastErrorMock).toHaveBeenCalledWith('home.proxyCrashed');
      expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
    });

    it('认领不外溢：认领期内的出口误导腿照常报（EXIT_MISMATCH）', async () => {
      await claimFailedStart();
      handleProxyErrorEvent({ errorCode: 'EXIT_MISMATCH' }, { t, refreshProxyStatus });
      expect(toastWarningMock).toHaveBeenCalledTimes(1);
      expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
    });
  });

  describe('无人认领（托盘 / 启动自动连接 / switchMode 去抖重启）→ 事件腿照常报', () => {
    // 这一组是「修过头」的反向哨兵：认领若恒真（或抑制写死），这三条就会红。
    it('HELPER_GATE_ABORTED：仍出 info toast，不得静默', () => {
      handleProxyErrorEvent({ errorCode: 'HELPER_GATE_ABORTED' }, { t, refreshProxyStatus });
      expect(toastInfoMock).toHaveBeenCalledWith('errors.helperGateAborted');
      expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
    });

    it('HELPER_NOT_INSTALLED：仍出 error toast + 桌面通知，不得静默', () => {
      handleProxyErrorEvent({ errorCode: 'HELPER_NOT_INSTALLED' }, { t, refreshProxyStatus });
      expect(toastErrorMock).toHaveBeenCalledTimes(1);
      expect(notifyDesktopMock).toHaveBeenCalledWith(
        'notify.helperNotInstalled.title',
        'notify.helperNotInstalled.body'
      );
    });

    // 本条正是本次要修的缺陷本身：此前这里没有分腿，托盘/自动连接撞上残留 root 孤儿时用户
    // 什么都看不到（真机反馈的直接成因）。
    it('ROOT_ORPHAN_BLOCKED：仍出 error toast + 桌面通知，不得静默', () => {
      handleProxyErrorEvent(
        { errorCode: 'ROOT_ORPHAN_BLOCKED', message: '残留进程（pid [999]）占用缓存文件' },
        { t, refreshProxyStatus }
      );
      expect(refreshProxyStatus).toHaveBeenCalledTimes(1);
      expect(toastErrorMock).toHaveBeenCalledTimes(1);
      expect(toastErrorMock.mock.calls[0][0]).toContain('999');
      expect(notifyDesktopMock).toHaveBeenCalledWith(
        'notify.rootOrphanBlocked.title',
        'notify.rootOrphanBlocked.body'
      );
    });

    it('认领过期后恢复上报（认领不得长期挂住，否则后续托盘失败被永久吞掉）', async () => {
      vi.useFakeTimers();
      try {
        await withProxyStartClaim(async () => {});
        vi.advanceTimersByTime(2001);
        handleProxyErrorEvent({ errorCode: 'HELPER_NOT_INSTALLED' }, { t, refreshProxyStatus });
      } finally {
        vi.useRealTimers();
      }
      expect(toastErrorMock).toHaveBeenCalledTimes(1);
      expect(notifyDesktopMock).toHaveBeenCalledTimes(1);
    });
  });
});
