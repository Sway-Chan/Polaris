/**
 * TsLoginDialog —— Tailscale 交互登录弹窗（原型 #ts-dialog :2776；tsSetMode :4855）。
 *
 * 两 pane（seg2）：浏览器登录 / Auth Key。两者提交都走 `api.server.tailscaleLogin(server)`——
 * 该命令**已是真实现**（server.rs:436 调 mesh().start_tailscale_login，非 stub），resolve 三态：
 *  - `{started:true}`：瞬态登录核已起。authKey 模式免交互，提交即完成，直接关弹窗；browser 模式的
 *    登录 URL **不在这个 resolve 值里**——后端订阅瞬态核自己的管理 API STATUS 流，帧里 `authURL`
 *    非空时才异步经 `EVENT_TAILSCALE_AUTH_URL`（`onTailscaleAuth`）回填，故此时不关弹窗，切到
 *    「等待/展示登录地址」态。（URL 来源已从「扫核 stdout 日志行」改为 gRPC 字段，理由见
 *    `src-tauri/src/runtime/tailscale_login_core.rs` 模块头。）
 *  - `{started:false, reason:'inMainCore'}`：双写守卫拦下——该出口已在主核里跑，不需要（也不会）
 *    再起瞬态核；toast 告知原因，不能像早前实现那样把这当成功静默关掉（用户会以为登录已发起）。
 *  - reject（IpcError，`TAILSCALE_LOGIN_FAILED` / `TAILSCALE_LOGIN_BAD_SERVER`）：内联错误、
 *    弹窗保持打开（同 ResUrlDialog 对下载失败态的处置范式）。
 *
 * 「先落盘节点、再登录」是硬前置：后端按 `server.id` 分键存登录产物，没落盘的节点 = 登录成果无主。
 * 提交计划（建/更新/免写 + id 从哪来）在 `ts-login-server.ts`，其顶注有完整因果。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useAppStore } from '@/store/app-store';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';
import { planTsLoginSubmit } from './ts-login-server';
import { InfoIcon } from '@/components/InfoIcon';

/**
 * 「等登录地址」的放弃时限，对齐后端瞬态登录核的超时臂
 * （src-tauri/src/runtime/tailscale_login_core.rs:54 `DEFAULT_LOGIN_TIMEOUT = 300s`）。
 *
 * 为何不取更短的值：到点之前后端的核仍**合法在跑**（STATUS 帧的 `authURL` 还没变成非空），
 * 此时报失败是 UI 谎报后端状态；到点之后核必已被后端杀掉，「等不到地址了」才是事实。
 * 嫌久的用户可以直接取消——取消现在会真的杀核（见下方 loginCancel 接线），不再是空按钮。
 */
const TS_LOGIN_TIMEOUT_MS = 300_000;

function TsIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
    </svg>
  );
}

export function TsLoginDialog() {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);
  const servers = useAppStore((s) => s.servers);
  const loadConfig = useAppStore((s) => s.loadConfig);
  const setTailscaleAuthUrl = useAppStore((s) => s.setTailscaleAuthUrl);
  const setTailscaleLoginInitiated = useAppStore((s) => s.setTailscaleLoginInitiated);

  const existingTs = servers.find((s) => s.protocol === 'tailscale');

  const [mode, setMode] = useState<'browser' | 'authkey'>('browser');
  const [authKey, setAuthKey] = useState('');
  const [errKey, setErrKey] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [dirty, setDirty] = useState(false);
  // browser 模式本次登录的 server.id（非空 = 核已起、正在等/已拿到登录地址）。authKey 模式不设：
  // 它没有「等地址」阶段，设了会让收尾逻辑把免交互登录核误杀。
  const [pendingServerId, setPendingServerId] = useState<string | null>(null);
  const [loginTimedOut, setLoginTimedOut] = useState(false);
  // authUrl 的真值在 store（卡片角标也要读），弹窗只按本次登录的 id 取。
  const authUrl = useAppStore((s) => (pendingServerId ? s.tailscaleAuthUrls[pendingServerId] ?? null : null));
  const awaitingUrl = pendingServerId !== null && !authUrl && !loginTimedOut;

  // 订阅登录 URL 事件 → 落 store（按 serverId 分键，故此处无需过滤「是不是本次提交」；展示侧按
  // pendingServerId 取即可）。onTailscaleAuth 挂在 proxyApi（events 命名空间与 proxy 生命周期事件同源）。
  useEffect(() => {
    return api.proxy.onTailscaleAuth((data) => {
      if (data.serverId) setTailscaleAuthUrl(data.serverId, data.url);
    });
  }, [setTailscaleAuthUrl]);

  // 等地址超时兜底：后端超时杀核时**不发任何事件**，没有这一臂「正在获取登录页地址…」会永转。
  useEffect(() => {
    if (!pendingServerId || authUrl) return;
    const timer = setTimeout(() => {
      setLoginTimedOut(true);
      setTailscaleLoginInitiated(pendingServerId, false);
      // 后端超时臂到点也会自杀核；这里仍显式取消一次（幂等，commands/server.rs:470），消掉两侧计时
      // 起点不一致（后端从 spawn 起算、这里从 resolve 起算）留下的残留窗口。
      void api.server.tailscaleLoginCancel(pendingServerId);
    }, TS_LOGIN_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [pendingServerId, authUrl, setTailscaleLoginInitiated]);

  // 弹窗收尾（关闭/卸载/重新提交）：清「登录在飞」标记；**仅当登录地址还没到**才杀瞬态核——
  // 那是用户在「等地址」阶段离开，明确等于放弃，否则核要空跑到后端 300s 超时才被回收。
  // 地址已经拿到就不杀：此时这个核正是完成登录的那一方，用户是按提示「浏览器授权后关闭本窗口」离开的，
  // 杀核会打断正在进行的授权。收场交给后端：授权一旦完成，其 STATUS 流会报 backendState=Running，
  // 后端当即收核（不再干等满 300s）；用户一直不去授权则仍由超时臂兜底。
  useEffect(() => {
    if (!pendingServerId) return;
    return () => {
      const s = useAppStore.getState();
      s.setTailscaleLoginInitiated(pendingServerId, false);
      if (!s.tailscaleAuthUrls[pendingServerId]) {
        void api.server.tailscaleLoginCancel(pendingServerId);
      }
    };
  }, [pendingServerId]);

  // 切换登录方式 = 放弃当前这次登录：清掉在飞标记与超时态。置空 pendingServerId 会触发上面的收尾
  // effect（该杀核的杀核），无需在此重复。
  const discardPendingLogin = () => {
    setLoginTimedOut(false);
    setPendingServerId(null);
  };

  const requestClose = () => {
    if (!dirty) {
      close();
      return;
    }
    open({
      kind: 'confirm',
      payload: {
        title: t('ts.loginDiscardTitle'),
        message: t('ts.loginDiscardMsg'),
        confirmLabel: t('ts.loginDiscard'),
        danger: true,
        onConfirm: () => {
          close();
          close();
        },
      },
    });
  };

  const handleSubmit = async () => {
    if (mode === 'authkey' && !authKey.trim()) {
      setErrKey(true);
      return;
    }
    const { server, persist } = planTsLoginSubmit(existingTs, mode, authKey, () =>
      crypto.randomUUID()
    );

    setSubmitting(true);
    setLoginTimedOut(false);
    // 重新提交 → 先撤掉上一轮的在飞标记与陈旧 URL（陈旧 URL 会让「等地址」瞬间误判成已拿到）。
    setPendingServerId(null);
    setTailscaleAuthUrl(server.id, null);
    try {
      // 登录**之前**先落盘节点：后端按 server.id 分键存登录 state，节点不在 config 里就等于登录成果
      // 无主（config 里永远不会出现 Tailscale 节点、state 落在对不上号的键下）。authKey 一并写进
      // tailscaleSettings，否则「已提交」只是句空话——key 没进任何持久配置。
      if (persist === 'add') await api.server.add(server);
      else if (persist === 'update') await api.server.update(server);
      if (persist !== 'none') await loadConfig(true);

      const res = await api.server.tailscaleLogin(server);
      if (!res.started) {
        // 未真正起核（如 inMainCore：该出口已在主核里跑，登录态由主核自身维护）——不能当成功静默关闭，
        // 否则用户会以为登录已发起。toast 告知原因，弹窗留给用户自行关闭。
        toast.info(
          res.reason === 'inMainCore'
            ? t('ts.loginInMainCore')
            : t('ts.loginAlreadyActive'),
        );
        return;
      }
      if (mode === 'authkey') {
        // Auth Key 免交互：核起来即完成，没有登录 URL 可等（预授权无需 stdout 的 Waiting for
        // authentication 行）。key 已随上面的 add/update 落盘，这句「已提交」才名副其实。
        toast.success(t('ts.loginStarted'));
        close();
        return;
      }
      // browser 模式：核已起 → 进「等地址」态。当前后端从不在 resolve 值里带 authUrl（异步经
      // EVENT_TAILSCALE_AUTH_URL 回填，见顶部订阅）——这里仍防御性地读一遍 res.authUrl（IPC 类型
      // 声明了该字段），避免未来后端改为同步下发时本组件仍卡在「等待」态不放。
      setTailscaleLoginInitiated(server.id, true);
      if (res.authUrl) setTailscaleAuthUrl(server.id, res.authUrl);
      setPendingServerId(server.id);
    } catch (e) {
      toast.error(t('ts.loginFailed'), e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      titleId="ts-dlg-title"
      title={t('ts.loginTitle')}
      onClose={requestClose}
      icon={<TsIcon />}
      className="entry-form-dlg"
      footer={
        <>
          {/* 提交中**不锁**「取消」：原型 `:2545` 的 ghost 钮无 disabled，且本仓此前四个弹窗锁、
              两个不锁（NodeDialog/SubDialog）—— 不是与原型的差，是实现自己两套。统一为不锁：
              提交卡住（IPC 无应答）时用户必须还能退出，否则弹窗成了死窗。 */}
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn flow" onClick={() => void handleSubmit()} disabled={submitting}>
            {submitting && <span className="spinner spin-inline" style={{ marginRight: 6 }} />}
            {mode === 'browser' ? t('ts.openLogin') : t('ts.signIn')}
          </button>
        </>
      }
    >
      <div className="fld">
        <label className="fld-l">{t('ts.method')}</label>
        <div className="seg2" role="group" aria-label={t('ts.method')} style={{ display: 'flex' }}>
          <button
            type="button"
            style={{ flex: 1 }}
            className={mode === 'browser' ? 'on' : ''}
            onClick={() => {
              setMode('browser');
              discardPendingLogin();
              setDirty(true);
            }}
          >
            {t('ts.browserLogin')}
          </button>
          <button
            type="button"
            style={{ flex: 1 }}
            className={mode === 'authkey' ? 'on' : ''}
            onClick={() => {
              setMode('authkey');
              discardPendingLogin();
              setDirty(true);
            }}
          >
            {t('ts.authKey')}
          </button>
        </div>
      </div>

      {mode === 'browser' ? (
        authUrl ? (
          <div className="fld">
            <label className="fld-l" htmlFor="ts-auth-url">
              {t('ts.authUrlLabel')}
            </label>
            <div style={{ display: 'flex', gap: 8 }}>
              <input
                id="ts-auth-url"
                className="input mono"
                value={authUrl}
                readOnly
                style={{ flex: 1 }}
                onFocus={(e) => e.currentTarget.select()}
              />
              <button
                type="button"
                className="btn ghost sm"
                onClick={() => {
                  void navigator.clipboard.writeText(authUrl);
                  toast.success(t('ts.authUrlCopied'));
                }}
              >
                {t('common.copy')}
              </button>
              <button
                type="button"
                className="btn ghost sm"
                onClick={() => void api.system.openExternal(authUrl)}
              >
                {t('ts.openUrl')}
              </button>
            </div>
            <div className="card-sub" style={{ marginTop: 6 }}>
              {t('ts.authUrlHint')}
            </div>
          </div>
        ) : loginTimedOut ? (
          <div className="fld">
            <div className="dlg-err">
              {t('ts.awaitingUrlTimeout')}
            </div>
            <button
              type="button"
              className="btn ghost sm"
              style={{ marginTop: 8 }}
              onClick={() => void handleSubmit()}
              disabled={submitting}
            >
              {t('ts.retryLogin')}
            </button>
          </div>
        ) : awaitingUrl ? (
          <div className="card-sub" style={{ lineHeight: 1.6, display: 'flex', alignItems: 'center', gap: 8 }}>
            <span className="spinner spin-inline" />
            {t('ts.awaitingUrl')}
          </div>
        ) : (
          <div className="card-sub" style={{ lineHeight: 1.6 }}>
            {t('ts.browserHint')}
          </div>
        )
      ) : (
        <>
          <div className="fld">
            <label className="fld-l fld-l-info" htmlFor="ts-authkey">
              <span>{t('ts.authKeyLabel')}</span>
              <InfoIcon tip={t('ts.authkeyHint')} />
            </label>
            <input
              id="ts-authkey"
              className="input mono"
              value={authKey}
              onChange={(e) => {
                setAuthKey(e.target.value);
                setErrKey(false);
                setDirty(true);
              }}
              placeholder="tskey-auth-xxxxCNTRL-xxxxxxxxxxxxxxxxxxxx"
            />
            {errKey && <div className="err-line">{t('ts.errKey')}</div>}
          </div>
        </>
      )}
    </Modal>
  );
}

export default TsLoginDialog;
