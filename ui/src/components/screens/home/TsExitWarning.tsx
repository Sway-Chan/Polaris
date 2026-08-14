/**
 * 首页「Tailscale 出口名不副实」行内警示（§H，迁移自 上游 `home/ts-exit-warning.tsx`）。
 * 挂在「出口节点」下拉正下方：选中 TS 当全局出口但出不了公网（未认证 / 未选出口设备 / 出口设备离线 /
 * 未广告出口）时，给一行 warning 注脚 + 一个直达下一步动作的链接。判定单一真值 = `deriveTsExitWarning`。
 * none → 渲染 null。纯 renderer 视图态、零 IPC / config-gen / 重启。
 *
 * 接入动机（本仓 dead-code 修）：`deriveTsExitWarning` 已定义 + locale 齐全，但全仓零 .tsx 调用 → TS 当出口未配
 * exit_node 时公网静默走直连而无任何提示（safety）。
 *
 * `needs-auth` 分支为本仓新增（**上游 无此形态**：其 `backendState` 刻意不入 renderer store，
 * `use-native-events.ts:307`「backendState 仅本地驱动登录 toast（不入 store）」⇒ 上游同样没有
 * 「设为出口却从未认证」的持久提示，只有一次性 toast）。真机依据见 `domain/tailscale-exit-warning.ts` 顶注。
 */
import { useTranslation } from 'react-i18next';
import { useAppStore, useEffectiveConfig } from '@/store/app-store';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { deriveTsExitWarning } from '@/domain/tailscale-exit-warning';
import { api } from '@/ipc/api-client';

export function TsExitWarning() {
  const { t } = useTranslation();
  const config = useEffectiveConfig();
  const servers = useAppStore((s) => s.servers);
  const selectedServerId = useAppStore((s) => s.selectedServerId);
  const proxyRunning = useAppStore((s) => !!s.proxyStatus?.running);
  const openDialog = useDialogStore((s) => s.open);

  const selectedServer = servers.find((x) => x.id === selectedServerId);
  const tsId =
    selectedServer?.protocol?.toLowerCase() === 'tailscale' ? selectedServer.id : undefined;
  const loggedIn = useAppStore((s) => (tsId ? !!s.tailscaleLoginStates[tsId] : false));
  const status = useAppStore((s) => (tsId ? s.tailscaleStatuses[tsId] : undefined));
  const authUrl = useAppStore((s) => (tsId ? s.tailscaleAuthUrls[tsId] : undefined));

  const warning = deriveTsExitWarning({
    selectedServer,
    loggedIn,
    proxyModeDirect: (config?.proxyMode || 'smart').toLowerCase() === 'direct',
    proxyRunning,
    status,
  });
  if (warning === 'none') return null;

  // 「选择出口设备」→ 打开 TS 设置弹窗（内含出口设备选择，单例弹窗自查现有节点）。
  const goPickExitNode = () => openDialog({ kind: 'ts-settings' });
  // 「去登录」→ 优先直接开控制面给的这一份 authURL（末帧带；App.tsx 的全局兜底只在 URL **首次**到达
  // 那一刻自动开过一次浏览器，用户错过就没有第二次）；没有 URL 时退回登录弹窗。
  const goAuth = () => {
    const url = status?.authURL || authUrl;
    if (url) void api.system.openExternal(url);
    else openDialog({ kind: 'ts-login' });
  };

  const text =
    warning === 'needs-auth'
      ? t('home.tsExitNeedsAuthWarn')
      : warning === 'no-exit-device'
        ? t('home.tsExitNoDeviceWarn')
        : warning === 'exit-device-not-advertised'
          ? t('home.tsExitNotAdvertisedWarn')
          : t('home.tsExitDeviceOfflineWarn');

  // 动作跟着根因走：未认证 → 去登录；其余三条都是「出口设备选错/失效」→ 去挑设备。
  const isAuth = warning === 'needs-auth';
  const actionLabel = isAuth
    ? t('home.tsExitGoAuth')
    : t('home.tsExitPickDevice');

  return (
    <div style={{ display: 'flex', alignItems: 'flex-start', gap: 6, marginTop: 8 }} role="status">
      <svg
        viewBox="0 0 24 24"
        width={14}
        height={14}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.9}
        style={{ color: 'hsl(var(--warn))', flex: 'none', marginTop: 1 }}
        aria-hidden
      >
        <path d="M12 3.2L21 19H3z" />
        <path d="M12 10v4M12 17h.01" />
      </svg>
      <p style={{ margin: 0, fontSize: 12, lineHeight: 1.5, color: 'hsl(var(--fg-dim))', minWidth: 0 }}>
        {text}
        <button
          type="button"
          onClick={isAuth ? goAuth : goPickExitNode}
          style={{
            marginInlineStart: 6,
            border: 0,
            background: 'none',
            padding: 0,
            cursor: 'pointer',
            color: 'hsl(var(--warn))',
            textDecoration: 'underline',
            textUnderlineOffset: 2,
            font: 'inherit',
          }}
        >
          {actionLabel}
        </button>
      </p>
    </div>
  );
}

export default TsExitWarning;
