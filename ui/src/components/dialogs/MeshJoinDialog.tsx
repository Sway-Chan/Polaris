import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import type { ServerConfig } from '@/contracts/types';
import { useEffectiveServers } from '@/store/app-store';
import { findWarpNode } from '@/domain/warp';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

interface MeshJoinDialogProps {
  onTsLogout: (node: ServerConfig) => void;
  onWarpReregister: (node: ServerConfig) => void;
  onWarpDeregister: (node: ServerConfig) => void;
}

function JoinIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M9 15l6-6M8 8a3 3 0 10-3 3M16 16a3 3 0 103 3" />
    </svg>
  );
}

function Choice({
  title,
  description,
  icon,
  onClick,
  actions,
}: {
  title: string;
  description: string;
  icon: ReactNode;
  onClick: () => void;
  actions?: ReactNode;
}) {
  return (
    <div className="mesh-choice">
      <button type="button" className="mesh-col clickable" onClick={onClick}>
        <span className="mesh-ic">{icon}</span>
        <span className="mesh-tx">
          <span className="mesh-col-h"><b>{title}</b></span>
          <span className="mesh-col-sub">{description}</span>
        </span>
        <svg className="mesh-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2}>
          <path d="M9 6l6 6-6 6" />
        </svg>
      </button>
      {actions && <div className="mesh-choice-actions">{actions}</div>}
    </div>
  );
}

export function MeshJoinDialog({ onTsLogout, onWarpReregister, onWarpDeregister }: MeshJoinDialogProps) {
  const { t } = useTranslation();
  const servers = useEffectiveServers();
  const open = useDialogStore((state) => state.open);
  const close = useDialogStore((state) => state.close);
  const tsNode = servers.find((server) => server.protocol === 'tailscale');
  const warpNode = findWarpNode(servers);

  const go = (next: Parameters<typeof open>[0]) => {
    close();
    open(next);
  };
  const action = (run: () => void) => {
    close();
    run();
  };
  const shield = (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z" />
    </svg>
  );

  return (
    <Modal
      titleId="mesh-join-title"
      title={t('meshJoin.title')}
      icon={<JoinIcon />}
      onClose={close}
      className="access-picker-dlg"
      footer={
        <button type="button" className="btn ghost" onClick={close}>
          {t('common.cancel')}
        </button>
      }
    >
      <p className="card-sub mesh-join-intro">
        {t('meshJoin.intro')}
      </p>

      <div className="field-lbl"><span>{t('meshJoin.managed')}</span></div>
      <div className="mesh-grid mesh-choice-grid">
        <Choice
          title="Cloudflare WARP"
          description={warpNode
            ? t('meshJoin.warpConfigured')
            : t('meshJoin.warpNew')}
          icon={<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}><path d="M13 2L4 14h6l-1 8 9-12h-6z" /></svg>}
          onClick={() => go({ kind: 'warp', edit: !!warpNode })}
          actions={warpNode && (
            <>
              <button type="button" className="btn ghost sm" onClick={() => action(() => onWarpReregister(warpNode))}>
                {t('meshJoin.reregister')}
              </button>
              <button type="button" className="btn ghost sm danger-text" onClick={() => action(() => onWarpDeregister(warpNode))}>
                {t('meshJoin.deregister')}
              </button>
            </>
          )}
        />
        <Choice
          title="Tailscale"
          description={tsNode
            ? t('meshJoin.tsConfigured')
            : t('meshJoin.tsNew')}
          icon={<JoinIcon />}
          onClick={() => go({ kind: tsNode ? 'ts-settings' : 'ts-login' })}
          actions={tsNode && (
            <>
              <button type="button" className="btn ghost sm" onClick={() => go({ kind: 'ts-login' })}>
                {t('meshJoin.switchAccount')}
              </button>
              <button type="button" className="btn ghost sm danger-text" onClick={() => action(() => onTsLogout(tsNode))}>
                {t('meshJoin.logout')}
              </button>
            </>
          )}
        />
      </div>

      <div className="field-lbl"><span>{t('meshJoin.tunnels')}</span></div>
      <div className="mesh-grid mesh-choice-grid">
        <Choice title="OpenConnect" description={t('meshJoin.oc')} icon={shield} onClick={() => go({ kind: 'node', initialProto: 'openconnect' })} />
        <Choice title="OpenVPN" description={t('meshJoin.ovpn')} icon={shield} onClick={() => go({ kind: 'node', initialProto: 'openvpn-client' })} />
        <Choice title="WireGuard" description={t('meshJoin.wg')} icon={shield} onClick={() => go({ kind: 'wg' })} />
      </div>
      <div className="fld-hint mesh-join-hint">
        {t('meshJoin.routesHint')}
      </div>
    </Modal>
  );
}

export default MeshJoinDialog;
