import React, { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatRelativeTimeRounded, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { useCurrentProject, type Deployment, type Environment } from '../lib/project-context';
import { deploymentSource, envState, promoteTargets, sortEnvironments, HEALTH_LABEL, type EnvState } from '../lib/env-state';
import { deploymentPath, logsPath, projectPath } from '../lib/routes';
import { fetchDeploymentContainers } from './logs/api';
import { useToast } from '../components/toast';
import { useDeployActions } from '../components/deploy-actions';
import { Icon } from '../components/icon';
import { SectionHead } from '../components/section-head';
import {
    Button,
    ConfirmDialog,
    Empty,
    EnvironmentColorPicker,
    EnvironmentIcon,
    Field,
    GroupPill,
    Input,
    Modal,
    Panel,
    Status,
    cx,
    statusTone,
} from '../components/r-ui';

interface CurrentUser { is_admin?: boolean }

/** The environment's own URL: where its live deployment is reachable first. */
export function envUrl(state: EnvState): string | null {
    const url = state.live?.all_urls?.[0] ?? state.live?.primary_url ?? null;
    return url && isSafeUrl(url) ? url : null;
}

/** Running/desired replica count of a deployment, e.g. "3/3 running". */
function useReplicaSummary(projectName: string, deployment: Deployment | null): string | null {
    const [summary, setSummary] = useState<string | null>(null);
    const id = deployment?.deployment_id;
    const status = deployment?.status;
    useEffect(() => {
        setSummary(null);
        if (!id) return;
        const controller = new AbortController();
        fetchDeploymentContainers({ projectName, deploymentId: id, signal: controller.signal })
            .then(page => {
                const total = page.containers.length;
                if (total === 0) return;
                const running = page.containers.filter(c => c.state === 'running' && c.health !== 'unhealthy').length;
                setSummary(`${running}/${total} running`);
            })
            .catch(() => { /* replica counts are a nicety; the card stands without them */ });
        return () => controller.abort();
    }, [projectName, id, status]);
    return summary;
}

function EnvironmentCard({ state, projectName, environments, onEdit }: {
    state: EnvState;
    projectName: string;
    environments: Environment[];
    onEdit: (env: Environment) => void;
}) {
    const { env, live, previous, failedAttempt, health, otherActive, latest } = state;
    const { request } = useDeployActions();
    const replicas = useReplicaSummary(projectName, live);
    const url = envUrl(state);
    const targets = promoteTargets(env.name, environments);
    const promoteTo = targets.find(t => t.is_production) ?? targets[0];
    const canPromote = !!live && !env.is_production && !!promoteTo && health !== 'deploying';

    return (
        <Panel className="r-env-card">
            <div className="r-env-card-head">
                <EnvironmentIcon color={env.color} size={13} />
                <span className="name">{env.name}</span>
                {env.is_production && env.name !== 'production' && <span className="r-env-tag prod">production</span>}
                <Status status={HEALTH_LABEL[health]} />
                <span className="right">{replicas ?? (live ? `${live.replicas} replica${live.replicas === 1 ? '' : 's'}` : '')}</span>
            </div>
            <div className="r-env-card-body">
                {url ? (
                    <a className="r-link mono r-env-url" href={url} target="_blank" rel="noopener noreferrer">{stripUrlScheme(url)}</a>
                ) : (
                    <span className="r-env-url muted">{env.primary_deployment_group ? 'Not deployed' : 'No primary deployment group'}</span>
                )}
                <button
                    type="button"
                    className="r-live-block"
                    disabled={!live && !latest}
                    onClick={() => { const d = live ?? latest; if (d) navigate(deploymentPath(projectName, d.deployment_id)); }}
                >
                    <span className="r-section-label">{live ? 'Live deployment' : latest ? 'Latest deployment' : 'Live deployment'}</span>
                    {live || latest ? (
                        <>
                            <span className="line">
                                <span className="mono id">{(live ?? latest)!.deployment_id}</span>
                                <span className="msg">{deploymentSource((live ?? latest)!)}</span>
                            </span>
                            <span className="meta">
                                {(live ?? latest)!.created_by_email} · {formatRelativeTimeRounded((live ?? latest)!.created)}
                                {!live && latest && <> · {latest.status}</>}
                            </span>
                        </>
                    ) : (
                        <span className="msg muted">No deployment yet in group <span className="mono">{state.group}</span></span>
                    )}
                </button>
                {failedAttempt && (
                    <button type="button" className="r-env-warn" onClick={() => navigate(deploymentPath(projectName, failedAttempt.deployment_id))}>
                        <Icon name="alert" size={14} />
                        <span>
                            Deploy <span className="mono">{failedAttempt.deployment_id}</span> {failedAttempt.status === 'Unhealthy' ? 'is unhealthy' : 'failed'}
                            {live && <> · still serving <span className="mono">{live.deployment_id}</span></>}
                        </span>
                    </button>
                )}
                {otherActive.length > 0 && (
                    <div className="r-env-others">
                        <span className="r-section-label">Other active groups</span>
                        {otherActive.map(d => (
                            <button type="button" key={d.deployment_id} className="r-env-other" onClick={() => navigate(deploymentPath(projectName, d.deployment_id))}>
                                <span className={cx('r-dot', statusTone(d.status))} />
                                <GroupPill projectName={projectName} group={d.deployment_group} />
                                <span className="mono id">{d.deployment_id}</span>
                                <span className="age">{formatRelativeTimeRounded(d.created)}</span>
                            </button>
                        ))}
                    </div>
                )}
            </div>
            <div className="r-env-card-foot">
                {canPromote && (
                    <Button variant="primary" icon="promote" onClick={() => request({ kind: 'promote', source: live!, targetEnv: promoteTo!.name })}>
                        Promote to {promoteTo!.name}
                    </Button>
                )}
                {previous && live && (
                    <Button icon="rollback" onClick={() => request({ kind: 'rollback', source: previous })} title={`Roll back to ${previous.deployment_id}`}>
                        Roll back
                    </Button>
                )}
                <span className="icons">
                    <button type="button" className="r-icon-btn" title="Logs" aria-label={`${env.name} logs`}
                        onClick={() => navigate(logsPath(projectName, (live ?? latest)?.deployment_id))} disabled={!live && !latest}>
                        <Icon name="terminal" size={15} />
                    </button>
                    <button type="button" className="r-icon-btn" title="Variables" aria-label={`${env.name} variables`}
                        onClick={() => navigate(`${projectPath(projectName, 'variables')}#env-${encodeURIComponent(env.name)}`)}>
                        <Icon name="key" size={15} />
                    </button>
                    {live && (
                        <button type="button" className="r-icon-btn" title="Redeploy" aria-label={`Redeploy ${env.name}`}
                            onClick={() => request({ kind: 'redeploy', source: live })}>
                            <Icon name="refresh" size={15} />
                        </button>
                    )}
                    <button type="button" className="r-icon-btn" title="Environment settings" aria-label={`${env.name} settings`} onClick={() => onEdit(env)}>
                        <Icon name="gear" size={15} />
                    </button>
                </span>
            </div>
        </Panel>
    );
}

interface EnvForm {
    name: string;
    primary_deployment_group: string;
    is_production: boolean;
    color: string;
    max_deployment_expiration: string;
    min_replicas: string;
    max_replicas: string;
    min_cpu: string;
    max_cpu: string;
    min_memory: string;
    max_memory: string;
}

const EMPTY_FORM: EnvForm = {
    name: '', primary_deployment_group: '', is_production: false, color: 'green',
    max_deployment_expiration: '',
    min_replicas: '', max_replicas: '', min_cpu: '', max_cpu: '', min_memory: '', max_memory: '',
};

interface PlatformConstraints {
    min_replicas?: number; max_replicas?: number;
    min_cpu?: string; max_cpu?: string; min_memory?: string; max_memory?: string;
}

interface DeploymentConstraints {
    min_replicas?: number | null; max_replicas?: number | null;
    min_cpu?: string | null; max_cpu?: string | null; min_memory?: string | null; max_memory?: string | null;
}

export function EnvironmentsSection() {
    const current = useCurrentProject()!;
    const { projectName, project, environments, deployments, reload } = current;
    const platformConstraints = (project?.platform_constraints ?? null) as PlatformConstraints | null;
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [editingEnv, setEditingEnv] = useState<Environment | null>(null);
    const [formData, setFormData] = useState<EnvForm>(EMPTY_FORM);
    const [envToDelete, setEnvToDelete] = useState<Environment | null>(null);
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [currentUser, setCurrentUser] = useState<CurrentUser | null>(null);
    const { showToast } = useToast();

    useEffect(() => {
        api.getMe().then(setCurrentUser).catch(() => {});
    }, []);

    const isAdmin = currentUser?.is_admin ?? false;
    const states = useMemo(
        () => sortEnvironments(environments).map(env => envState(env, deployments)),
        [environments, deployments],
    );

    const set = (patch: Partial<EnvForm>) => setFormData(f => ({ ...f, ...patch }));

    const openCreate = () => {
        setEditingEnv(null);
        setFormData(EMPTY_FORM);
        setIsModalOpen(true);
    };

    const openEdit = (env: Environment) => {
        setEditingEnv(env);
        const c = env.deployment_constraints as DeploymentConstraints | undefined;
        setFormData({
            name: env.name,
            primary_deployment_group: env.primary_deployment_group || '',
            is_production: env.is_production,
            color: env.color || 'green',
            max_deployment_expiration: env.max_deployment_expiration ?? '',
            min_replicas: c?.min_replicas?.toString() ?? '',
            max_replicas: c?.max_replicas?.toString() ?? '',
            min_cpu: c?.min_cpu ?? '',
            max_cpu: c?.max_cpu ?? '',
            min_memory: c?.min_memory ?? '',
            max_memory: c?.max_memory ?? '',
        });
        setIsModalOpen(true);
    };

    const changed = () => {
        reload();
        window.dispatchEvent(new Event('rise:mutation'));
    };

    const handleSave = async () => {
        if (!formData.name) {
            showToast('Environment name is required', 'error');
            return;
        }
        setSaving(true);
        try {
            if (editingEnv) {
                const updates: Record<string, unknown> = {
                    primary_deployment_group: formData.primary_deployment_group || null,
                    is_production: formData.is_production,
                    color: formData.color,
                    max_deployment_expiration: formData.max_deployment_expiration || null,
                };
                if (isAdmin) {
                    const hasAnyConstraint = formData.min_replicas || formData.max_replicas ||
                        formData.min_cpu || formData.max_cpu || formData.min_memory || formData.max_memory;
                    updates.deployment_constraints = {
                        min_replicas: hasAnyConstraint && formData.min_replicas ? parseInt(formData.min_replicas, 10) : null,
                        max_replicas: hasAnyConstraint && formData.max_replicas ? parseInt(formData.max_replicas, 10) : null,
                        min_cpu: (hasAnyConstraint && formData.min_cpu) || null,
                        max_cpu: (hasAnyConstraint && formData.max_cpu) || null,
                        min_memory: (hasAnyConstraint && formData.min_memory) || null,
                        max_memory: (hasAnyConstraint && formData.max_memory) || null,
                    };
                }
                await api.updateEnvironment(projectName, editingEnv.name, updates);
                showToast(`Environment ${editingEnv.name} updated`, 'success');
            } else {
                await api.createEnvironment(projectName, {
                    name: formData.name,
                    primary_deployment_group: formData.primary_deployment_group || null,
                    is_production: formData.is_production,
                    color: formData.color,
                    max_deployment_expiration: formData.max_deployment_expiration || null,
                });
                showToast(`Environment ${formData.name} created`, 'success');
            }
            setIsModalOpen(false);
            changed();
        } catch (err) {
            showToast(`Failed to ${editingEnv ? 'update' : 'create'} environment: ${(err as Error).message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDeleteConfirm = async () => {
        if (!envToDelete) return;
        setDeleting(true);
        try {
            await api.deleteEnvironment(projectName, envToDelete.name);
            showToast(`Environment ${envToDelete.name} deleted`, 'success');
            setEnvToDelete(null);
            changed();
        } catch (err) {
            showToast(`Failed to delete environment: ${(err as Error).message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    const constraintField = (key: keyof EnvForm, label: string, placeholder: string, type?: string) => (
        <Field label={label}>
            <Input type={type} value={formData[key] as string} onChange={e => set({ [key]: e.target.value } as Partial<EnvForm>)} placeholder={placeholder} />
        </Field>
    );

    return (
        <div>
            <SectionHead section="environments" actions={<Button variant="primary" icon="plus" onClick={openCreate}>New environment</Button>} />

            {states.length === 0 ? (
                <Panel>
                    <Empty title="No environments">
                        No environments configured. Create one to define a deployment target.
                    </Empty>
                </Panel>
            ) : (
                <div className="r-env-grid">
                    {states.map(state => (
                        <EnvironmentCard key={state.env.name} state={state} projectName={projectName} environments={environments} onEdit={openEdit} />
                    ))}
                </div>
            )}

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingEnv ? `Environment ${editingEnv.name}` : 'New environment'}
                footer={
                    <>
                        {editingEnv && (
                            <Button
                                variant="ghost"
                                className="r-btn-danger-text r-foot-left"
                                icon="trash"
                                disabled={editingEnv.is_production || saving}
                                title={editingEnv.is_production ? 'The production environment cannot be deleted' : undefined}
                                onClick={() => { setIsModalOpen(false); setEnvToDelete(editingEnv); }}
                            >
                                Delete
                            </Button>
                        )}
                        <Button onClick={() => setIsModalOpen(false)} disabled={saving}>Cancel</Button>
                        <Button variant="primary" onClick={handleSave} loading={saving}>
                            {editingEnv ? 'Save' : 'Create'}
                        </Button>
                    </>
                }
            >
                <Field label="Name">
                    <Input
                        value={formData.name}
                        onChange={e => set({ name: e.target.value.toLowerCase() })}
                        placeholder="staging"
                        disabled={editingEnv !== null}
                    />
                </Field>
                <Field label="Primary deployment group" hint="Its active deployment serves this environment's URL and custom domains. Leave empty if not applicable.">
                    <Input value={formData.primary_deployment_group} onChange={e => set({ primary_deployment_group: e.target.value })} placeholder="default" />
                </Field>
                <Field
                    label="Max expiration for non-primary groups"
                    hint="Deployments into any group other than the primary group expire after at most this long. A missing --expire gets this value; a longer one is clamped. Leave empty for no cap."
                >
                    <Input value={formData.max_deployment_expiration} onChange={e => set({ max_deployment_expiration: e.target.value })} placeholder="7d" />
                </Field>
                <label className="r-check">
                    <input type="checkbox" checked={formData.is_production} onChange={e => set({ is_production: e.target.checked })} />
                    <span>Production</span>
                </label>
                <Field label="Color">
                    <EnvironmentColorPicker value={formData.color} onChange={c => set({ color: c })} />
                </Field>

                {isAdmin && editingEnv && (
                    <div className="r-form-section">
                        <div className="r-section-label">Resource constraints</div>
                        <p className="r-modal-text muted">
                            Bounds for what a deployment can <em>request</em> at deploy time (via <span className="mono">rise.toml</span> or the CLI). These are not Kubernetes requests/limits — they constrain the user's choice. Leave empty to use platform defaults.
                        </p>
                        <div className="r-form-grid">
                            {constraintField('min_replicas', 'Min replicas', platformConstraints?.min_replicas?.toString() ?? '1', 'number')}
                            {constraintField('max_replicas', 'Max replicas', platformConstraints?.max_replicas?.toString() ?? '1', 'number')}
                            {constraintField('min_cpu', 'Min CPU', platformConstraints?.min_cpu ?? '100m')}
                            {constraintField('max_cpu', 'Max CPU', platformConstraints?.max_cpu ?? '2')}
                            {constraintField('min_memory', 'Min memory', platformConstraints?.min_memory ?? '64Mi')}
                            {constraintField('max_memory', 'Max memory', platformConstraints?.max_memory ?? '2Gi')}
                        </div>
                    </div>
                )}
            </Modal>

            <ConfirmDialog
                isOpen={!!envToDelete}
                onClose={() => setEnvToDelete(null)}
                onConfirm={handleDeleteConfirm}
                title={`Delete environment ${envToDelete?.name ?? ''}?`}
                message={`Deletes the environment "${envToDelete?.name}". This cannot be undone.`}
                confirmText="Delete environment"
                confirmTone="danger"
                loading={deleting}
            />
        </div>
    );
}

/** Compact environment rows for the project overview. */
export function EnvironmentRows({ states, projectName }: { states: EnvState[]; projectName: string }): React.ReactElement {
    return (
        <Panel className="r-list">
            {states.map(state => {
                const d = state.live ?? state.latest;
                const url = envUrl(state);
                return (
                    <div
                        key={state.env.name}
                        role="link"
                        tabIndex={0}
                        className="r-list-row r-env-row-v2"
                        onClick={() => navigate(d ? deploymentPath(projectName, d.deployment_id) : projectPath(projectName, 'environments'))}
                        onKeyDown={e => { if (e.key === 'Enter') navigate(d ? deploymentPath(projectName, d.deployment_id) : projectPath(projectName, 'environments')); }}
                    >
                        <span className="env-name"><EnvironmentIcon color={state.env.color} size={12} />{state.env.name}</span>
                        <span className="env-status"><Status status={HEALTH_LABEL[state.health]} /></span>
                        <span className="mono id">{state.live?.deployment_id ?? '—'}</span>
                        <span className="msg">{state.live ? deploymentSource(state.live) : 'Not deployed'}</span>
                        {url && (
                            <a className="r-link mono url" href={url} target="_blank" rel="noopener noreferrer" onClick={e => e.stopPropagation()}>
                                {stripUrlScheme(url)}
                            </a>
                        )}
                    </div>
                );
            })}
        </Panel>
    );
}
