import React, { createContext, useCallback, useContext, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { useCurrentProject, type Deployment } from '../lib/project-context';
import { deploymentSource, envState, promoteTargets } from '../lib/env-state';
import { formatRelativeTimeRounded } from '../lib/utils';
import { Button, Modal, Select, cx } from './r-ui';
import { useToast } from './toast';

// Redeploy, roll back and promote all create a new deployment from an existing
// one's image — no rebuild. They differ only in where the new deployment lands
// and which variables it gets, so one dialog serves all three.

export type DeployActionKind = 'redeploy' | 'rollback' | 'promote';

export interface DeployActionRequest {
    kind: DeployActionKind;
    /** The deployment whose image is deployed. */
    source: Deployment;
    /** Promote only: the initially selected target environment. */
    targetEnv?: string;
}

interface DeployActionsValue {
    request: (req: DeployActionRequest) => void;
}

const DeployActionsContext = createContext<DeployActionsValue | null>(null);

export function DeployActionsProvider({ children }: { children: React.ReactNode }) {
    const [pending, setPending] = useState<DeployActionRequest | null>(null);
    const request = useCallback((req: DeployActionRequest) => setPending(req), []);
    return (
        <DeployActionsContext.Provider value={{ request }}>
            {children}
            {pending && <DeployActionDialog req={pending} onClose={() => setPending(null)} />}
        </DeployActionsContext.Provider>
    );
}

export function useDeployActions(): DeployActionsValue {
    const ctx = useContext(DeployActionsContext);
    if (!ctx) throw new Error('useDeployActions must be used within DeployActionsProvider');
    return ctx;
}

function DeploymentSummary({ d, label, tone }: { d: Deployment | null; label: string; tone: 'now' | 'after' }) {
    return (
        <div className={cx('r-diff-cell', tone)}>
            <div className="r-diff-label">{label}</div>
            {d ? (
                <>
                    <div className="mono r-diff-id">{d.deployment_id}</div>
                    <div className="r-diff-msg">{deploymentSource(d)}</div>
                    <div className="r-diff-meta">{d.created_by_email} · {formatRelativeTimeRounded(d.created)}</div>
                </>
            ) : (
                <div className="r-diff-msg">Nothing deployed</div>
            )}
        </div>
    );
}

function DeployActionDialog({ req, onClose }: { req: DeployActionRequest; onClose: () => void }) {
    const project = useCurrentProject()!;
    const { showToast } = useToast();
    const { kind, source } = req;
    const sourceEnv = source.environment || '';
    const targets = kind === 'promote' ? promoteTargets(sourceEnv, project.environments) : [];
    const [target, setTarget] = useState(req.targetEnv || targets[0]?.name || '');
    const [useSourceEnvVars, setUseSourceEnvVars] = useState(false);
    const [busy, setBusy] = useState(false);

    const envName = kind === 'promote' ? target : sourceEnv;
    const env = project.environments.find(e => e.name === envName);
    // What the affected environment serves before the action.
    const current = env
        ? (kind === 'promote' ? envState(env, project.deployments).live : project.deployments.find(d =>
            d.is_active && d.environment === sourceEnv && d.deployment_group === source.deployment_group) ?? null)
        : null;

    let title: string;
    let body: React.ReactNode;
    let confirm: string;
    if (kind === 'rollback') {
        title = `Roll back ${envName || source.deployment_group}?`;
        body = <>{project.projectName} {envName} switches to the image of <span className="mono">{source.deployment_id}</span>. No rebuild — the image is reused.</>;
        confirm = 'Roll back';
    } else if (kind === 'promote') {
        title = target ? `Promote to ${target}?` : 'Promote';
        body = <>Ships the exact image running in {sourceEnv} to {target || 'another environment'}. Variables from {target || 'the target'} are used.</>;
        confirm = 'Promote';
    } else {
        title = `Redeploy ${envName || source.deployment_group}?`;
        body = <>Starts a new deployment of the same image. Use this to apply variable changes — no rebuild.</>;
        confirm = 'Redeploy';
    }

    const run = async () => {
        setBusy(true);
        try {
            const res = await api.createDeploymentFrom(
                project.projectName,
                source.deployment_id,
                kind === 'promote' ? false : useSourceEnvVars,
                kind === 'promote' ? { environment: target } : null,
            );
            const verb = kind === 'promote' ? `Promoting ${source.deployment_id} to ${target}`
                : kind === 'rollback' ? `Rolling ${envName} back to ${source.deployment_id}`
                    : `Redeploying ${project.projectName} · ${envName}`;
            showToast(verb, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            onClose();
            if (res?.deployment_id) navigate(`/deployment/${project.projectName}/${res.deployment_id}`);
        } catch (err) {
            showToast(`Failed to ${kind}: ${(err as Error).message}`, 'error');
            setBusy(false);
        }
    };

    return (
        <Modal
            isOpen
            onClose={onClose}
            title={title}
            footer={
                <>
                    <Button onClick={onClose} disabled={busy}>Cancel</Button>
                    <Button
                        variant={kind === 'rollback' ? 'danger' : 'primary'}
                        icon={kind === 'rollback' ? 'rollback' : kind === 'promote' ? 'promote' : 'refresh'}
                        onClick={run}
                        loading={busy}
                        disabled={kind === 'promote' && !target}
                    >
                        {confirm}
                    </Button>
                </>
            }
        >
            <p className="r-modal-text">{body}</p>
            {kind === 'promote' && targets.length > 1 && (
                <div>
                    <label className="r-field-label">Target environment</label>
                    <Select value={target} onChange={setTarget} options={targets.map(t => ({ value: t.name, label: t.name, hint: t.is_production ? 'production' : undefined }))} />
                </div>
            )}
            {kind === 'promote' && targets.length === 0 && (
                <p className="r-modal-text muted">No other environment has a primary deployment group to promote into.</p>
            )}
            <div className="r-diff">
                <DeploymentSummary d={current} label="Now" tone="now" />
                <span className="r-diff-arrow" aria-hidden>→</span>
                <DeploymentSummary d={source} label="After" tone="after" />
            </div>
            {kind !== 'promote' && (
                <label className="r-check">
                    <input type="checkbox" checked={useSourceEnvVars} onChange={e => setUseSourceEnvVars(e.target.checked)} />
                    <span>
                        Use the variables of <span className="mono">{source.deployment_id}</span>
                        <span className="hint">Otherwise the environment's current variables are used.</span>
                    </span>
                </label>
            )}
        </Modal>
    );
}
