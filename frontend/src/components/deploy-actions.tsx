import React, { createContext, useCallback, useContext, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { isApiError, type ApiError } from '../lib/api-error';
import { navigate } from '../lib/navigation';
import { useCurrentProject, type Deployment } from '../lib/project-context';
import { deploymentSource, envState, promoteTargetsFlagged, shortDeploymentId } from '../lib/env-state';
import { formatRelativeTimeRounded } from '../lib/utils';
import { Alert, Button, Modal, Select, cx } from './r-ui';
import { useToast } from './toast';
import { VarChanges } from './var-changes';

interface BarrierDetails {
    candidate_version?: number | null;
    environment_version?: number | null;
    barriers?: { version: number; reason: string }[];
}

/** The server's preview of a deployment made from another one (`GET …/plan`). */
interface CreateFromPlan {
    pinned_image?: string | null;
    migration?: { status?: 'ok' | 'unknown' | 'crossed' } & BarrierDetails;
    adjustments?: { container: string; field: string; from: string; to: string; reason: string }[];
    warnings?: string[];
}

function BarrierList({ details: d }: { details: BarrierDetails }) {
    return (
        <>
            {d.candidate_version != null && d.environment_version != null && (
                <div>This deployment is at migration version {d.candidate_version}; the environment has run version {d.environment_version}.</div>
            )}
            <ul className="r-barrier-list">
                {(d.barriers ?? []).map(b => <li key={b.version}><span className="mono">v{b.version}</span> — {b.reason}</li>)}
            </ul>
        </>
    );
}

function PlanSummary({ plan }: { plan: CreateFromPlan }) {
    const m = plan.migration;
    const barriers = m?.barriers ?? [];
    const rows = [
        plan.pinned_image && <li key="img">Image pinned to <span className="mono">{plan.pinned_image}</span></li>,
        m?.status === 'crossed' && <li key="mig">Crosses migration barrier{barriers.length === 1 ? '' : 's'} {barriers.map(b => `v${b.version}`).join(', ')}: the environment has run version {m.environment_version}, this deployment is at {m.candidate_version}.</li>,
        m?.status === 'unknown' && <li key="mig">Migration version unknown: it can't be checked against recorded barriers.</li>,
        ...(plan.adjustments ?? []).map(a => <li key={`${a.container}-${a.field}`}><span className="mono">{a.container}</span> {a.field} {a.from} → {a.to} ({a.reason})</li>),
        ...(plan.warnings ?? []).map(w => <li key={`w-${w}`}>{w}</li>),
    ].filter(Boolean);
    if (rows.length === 0) return null;
    return (
        <div className="r-var-changes r-plan">
            <div className="r-section-label">Plan</div>
            <ul>{rows}</ul>
        </div>
    );
}

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
    const [isAdmin, setIsAdmin] = useState(false);
    useEffect(() => {
        api.getMe().then((u: { is_admin?: boolean } | null) => setIsAdmin(!!u?.is_admin)).catch(() => {});
    }, []);
    // Admins may bypass an environment's promotion source, so they see gated targets too, flagged.
    const flagged = kind === 'promote' ? promoteTargetsFlagged(sourceEnv, project.environments, isAdmin) : [];
    const targets = flagged.map(t => t.env);
    const [target, setTarget] = useState(req.targetEnv || targets[0]?.name || '');
    const targetGated = flagged.find(t => t.env.name === target)?.gated ?? false;
    const [useSourceEnvVars, setUseSourceEnvVars] = useState(false);
    const [busy, setBusy] = useState(false);
    // A refusal the dialog explains instead of toasting.
    const [blocked, setBlocked] = useState<ApiError | null>(null);
    const [plan, setPlan] = useState<CreateFromPlan | null>(null);

    const envName = kind === 'promote' ? target : sourceEnv;
    const env = project.environments.find(e => e.name === envName);

    useEffect(() => {
        setBlocked(null);
        setPlan(null);
        if (kind === 'promote' && !target) return;
        let cancelled = false;
        // The preview is advisory: an older server without the endpoint just shows no plan.
        api.getDeploymentPlan(project.projectName, source.deployment_id, envName || null)
            .then((p: CreateFromPlan) => { if (!cancelled) setPlan(p); })
            .catch(() => {});
        return () => { cancelled = true; };
    }, [project.projectName, source.deployment_id, kind, target, envName]);
    // What the affected environment serves before the action.
    const current = env
        ? (kind === 'promote' ? envState(env, project.deployments).live : project.deployments.find(d =>
            d.is_active && d.environment === sourceEnv && d.deployment_group === source.deployment_group) ?? null)
        : null;

    // Redeploying a deployment that failed retries an image that already
    // failed once; say so, and what keeps serving meanwhile.
    const retrying = kind === 'redeploy' && (source.status === 'Failed' || source.status === 'Unhealthy');

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
    } else if (retrying) {
        title = `Retry ${shortDeploymentId(source.deployment_id)} in ${envName || source.deployment_group}?`;
        body = <>Deploys the image of <span className="mono">{source.deployment_id}</span> again, without a rebuild.</>;
        confirm = 'Retry';
    } else {
        title = `Redeploy ${envName || source.deployment_group}?`;
        body = <>Starts a new deployment of the same image. Use this to apply variable changes — no rebuild.</>;
        confirm = 'Redeploy';
    }

    const run = async (forceMigrationBarrier = false) => {
        setBusy(true);
        setBlocked(null);
        try {
            const res: { deployment_id?: string; warnings?: string[] } | null = await api.createDeploymentFrom(
                project.projectName,
                source.deployment_id,
                kind === 'promote' ? false : useSourceEnvVars,
                kind === 'promote' ? { environment: target } : null,
                { force_migration_barrier: forceMigrationBarrier },
            );
            const verb = kind === 'promote' ? `Promoting ${source.deployment_id} to ${target}`
                : kind === 'rollback' ? `Rolling ${envName} back to ${source.deployment_id}`
                    : `Redeploying ${project.projectName} · ${envName}`;
            const warnings = res?.warnings ?? [];
            showToast(warnings.length ? <>{verb}{warnings.map(w => <div key={w}>{w}</div>)}</> : verb, warnings.length ? 'info' : 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            onClose();
            if (res?.deployment_id) navigate(`/deployment/${project.projectName}/${res.deployment_id}`);
        } catch (err) {
            setBusy(false);
            if (isApiError(err) && (err.code === 'migration_barrier' || err.code === 'promotion_source_required')) {
                setBlocked(err);
                return;
            }
            showToast(`Failed to ${kind}: ${(err as Error).message}`, 'error');
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
                    {blocked?.code === 'migration_barrier' ? (
                        <Button variant="primary" icon="alert" onClick={() => run(true)} loading={busy}>
                            Deploy anyway
                        </Button>
                    ) : (
                        <Button
                            variant="primary"
                            icon={kind === 'rollback' ? 'rollback' : kind === 'promote' ? 'promote' : 'refresh'}
                            onClick={() => run()}
                            loading={busy}
                            disabled={(kind === 'promote' && !target) || blocked?.code === 'promotion_source_required'}
                        >
                            {confirm}
                        </Button>
                    )}
                </>
            }
        >
            <p className="r-modal-text">{body}</p>
            {blocked?.code === 'promotion_source_required' && <Alert tone="err" icon="alert">{blocked.message}</Alert>}
            {blocked?.code === 'migration_barrier' && (
                <Alert tone="err" icon="alert">
                    <strong>{blocked.message}</strong>
                    <BarrierList details={(blocked.details ?? {}) as BarrierDetails} />
                    <div>Deploying anyway runs this image against a database schema it may not support.</div>
                </Alert>
            )}
            {kind === 'promote' && targetGated && (
                <Alert tone="warn" icon="alert">
                    {target} only accepts promotions from {project.environments.find(e => e.name === target)?.promotion_source_environment}. As an administrator you can promote anyway; the bypass is recorded.
                </Alert>
            )}
            {retrying && (
                <div className="r-retry-note" role="note">
                    <strong>This image failed{source.error_message ? ':' : '.'}</strong>
                    {source.error_message && <> {source.error_message}</>}
                    <div>
                        {current && current.deployment_id !== source.deployment_id
                            ? <><span className="mono">{current.deployment_id}</span> keeps serving until the new deployment is healthy. </>
                            : null}
                        Retry when the cause was outside the image (a dependency, a variable you have since changed); otherwise fix the code and deploy again.
                    </div>
                </div>
            )}
            {kind === 'promote' && targets.length > 1 && (
                <div>
                    <label className="r-field-label">Target environment</label>
                    <Select value={target} onChange={setTarget} options={flagged.map(({ env: t, gated }) => ({
                        value: t.name,
                        label: t.name,
                        hint: gated ? `only from ${t.promotion_source_environment}` : t.is_production ? 'production' : undefined,
                    }))} />
                </div>
            )}
            {kind === 'promote' && targets.length === 0 && (
                <p className="r-modal-text muted">No other environment with a primary deployment group accepts promotions from {sourceEnv || 'this environment'}.</p>
            )}
            <div className="r-diff">
                <DeploymentSummary d={current} label="Now" tone="now" />
                <span className="r-diff-arrow" aria-hidden>→</span>
                <DeploymentSummary d={source} label="After" tone="after" />
            </div>
            {plan && <PlanSummary plan={plan} />}
            {useSourceEnvVars && (
                <p className="r-var-changes-note">Uses the variables of <span className="mono">{source.deployment_id}</span> unchanged.</p>
            )}
            {!useSourceEnvVars && (kind !== 'promote' || target) && (
                <VarChanges
                    projectName={project.projectName}
                    sourceId={source.deployment_id}
                    environment={envName || null}
                />
            )}
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
