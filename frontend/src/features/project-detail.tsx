// @ts-nocheck
import React, { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatISO8601, formatRelativeTimeRounded, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { useToast } from '../components/toast';
import { Button, ConfirmDialog, Empty, EnvPill, KV, KVRow, Modal, Panel, PanelBody, PanelHead, Pill, Status, Tabs, cx } from '../components/r-ui';
import { Icon } from '../components/icon';
import { EnvironmentColorDot } from '../components/ui';
import { LoadingState, ErrorState, EmptyState } from '../components/states';

import { DeploymentsList } from './deployments';
import { DomainsList, EnvironmentsList, EnvVarsList, ExtensionsList, ServiceAccountsList } from './resources';
import { AppUsersList } from './projects';

const TAB_LABELS: Record<string, string> = {
    overview: 'Overview',
    environments: 'Environments',
    deployments: 'Deployments',
    'env-vars': 'Env vars',
    domains: 'Domains',
    'service-accounts': 'Service accounts',
    extensions: 'Extensions',
    access: 'Access',
};

const TAB_IDS = ['overview', 'environments', 'deployments', 'env-vars', 'domains', 'service-accounts', 'extensions', 'access'];

export function ProjectDetail({ projectName, initialTab }: { projectName: string; initialTab?: string }) {
    const [project, setProject] = useState<any>(null);
    const [error, setError] = useState<string | null>(null);
    const [activeTab, setActiveTab] = useState(initialTab || 'overview');
    const [confirmOpen, setConfirmOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const [accessClasses, setAccessClasses] = useState<any[]>([]);
    const [currentUserEmail, setCurrentUserEmail] = useState<string>('');
    const [environments, setEnvironments] = useState<any[]>([]);
    const [tabCounts, setTabCounts] = useState<Record<string, number>>({});
    const { showToast } = useToast();

    useEffect(() => { if (initialTab && TAB_IDS.includes(initialTab)) setActiveTab(initialTab); }, [initialTab]);

    const loadProject = useCallback(async () => {
        try {
            const data = await api.getProject(projectName);
            setProject(data);
        } catch (err: any) {
            setError(err.message);
        }
    }, [projectName]);

    useEffect(() => { loadProject(); }, [loadProject]);
    useEffect(() => {
        api.getAccessClasses().then(d => setAccessClasses(d?.access_classes || [])).catch(() => {});
        api.getMe().then(u => setCurrentUserEmail(u?.email || '')).catch(() => {});
    }, []);

    // Fetch environments + per-tab count badges. Each count is recorded only
    // once its source request resolves, so badges appear progressively.
    useEffect(() => {
        let cancelled = false;
        const record = (key: string, n: number) => {
            if (!cancelled) setTabCounts(c => ({ ...c, [key]: n }));
        };
        api.getProjectEnvironments(projectName)
            .then((d: any[]) => {
                const list = Array.isArray(d) ? d : [];
                if (!cancelled) setEnvironments(list);
                record('environments', list.length);
            })
            .catch(() => {});
        api.getProjectDeployments(projectName, { limit: 100 })
            .then((d: any[]) => record('deployments', Array.isArray(d) ? d.length : 0))
            .catch(() => {});
        api.getProjectDomains(projectName)
            .then((d: any) => record('domains', Array.isArray(d) ? d.length : (d?.domains?.length ?? 0)))
            .catch(() => {});
        api.getProjectServiceAccounts(projectName)
            .then((d: any) => record('service-accounts', Array.isArray(d) ? d.length : (d?.workload_identities?.length ?? 0)))
            .catch(() => {});
        api.getProjectExtensions(projectName)
            .then((d: any) => record('extensions', Array.isArray(d) ? d.length : (d?.extensions?.length ?? 0)))
            .catch(() => {});
        return () => { cancelled = true; };
    }, [projectName]);

    const changeTab = (tab: string) => {
        setActiveTab(tab);
        navigate(`/project/${projectName}/${tab}`);
    };

    const handleDelete = async () => {
        if (!project) return;
        setDeleting(true);
        try {
            await api.deleteProject(project.name);
            showToast(`Project ${project.name} deleted`, 'success');
            navigate('/projects');
        } catch (err: any) {
            showToast(`Failed to delete project: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    const handleCopy = async (value: string, label: string) => {
        if (!value) return;
        try {
            await copyToClipboard(value);
            showToast(`${label} copied`, 'success');
        } catch (err: any) {
            showToast(`Failed to copy ${label.toLowerCase()}: ${err.message}`, 'error');
        }
    };

    if (error) return <ErrorState message={`Failed to load project: ${error}`} onRetry={loadProject} />;
    if (!project) return <LoadingState label="Loading project…" />;

    const ownerType: 'user' | 'team' | null = project.owner?.email ? 'user' : project.owner?.name ? 'team' : null;
    const ownerLabel = project.owner?.email || project.owner?.name || '—';
    const accessLabel = accessClasses.find(a => a.id === project.access_class)?.display_name || project.access_class || '—';

    const tabs = TAB_IDS.map(id => ({ id, label: TAB_LABELS[id], count: tabCounts[id] }));

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8, flexWrap: 'wrap' }}>
                        <h1 className="r-page-title">{project.name}</h1>
                        <Status status={project.status || 'Unknown'} />
                        <Pill kind="accent">{accessLabel}</Pill>
                        {environments.map(env => (
                            <span key={env.name} style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}>
                                <EnvironmentColorDot color={env.color} size="0.55rem" />
                                <EnvPill env={env.name} />
                            </span>
                        ))}
                    </div>
                    <div className="r-meta-bar" style={{ marginTop: 8 }}>
                        {ownerType && (
                            <>
                                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
                                    <Icon name={ownerType === 'team' ? 'users' : 'user'} size={13} />
                                    {ownerType === 'team' ? (
                                        <a className="r-link" onClick={() => navigate(`/team/${ownerLabel}`)}>{ownerLabel}</a>
                                    ) : ownerLabel}
                                </span>
                                <span className="dot-sep" />
                            </>
                        )}
                        {project.primary_url ? (
                            <>
                                <a className="r-link mono" style={{ fontSize: 12.5 }} href={project.primary_url} target="_blank" rel="noopener noreferrer">
                                    {stripUrlScheme(project.primary_url)}
                                </a>
                                <span className="dot-sep" />
                            </>
                        ) : null}
                        {project.created && (
                            <span title={formatISO8601(project.created)}>created {formatRelativeTimeRounded(project.created)}</span>
                        )}
                        {project.deployment_groups?.length > 0 && (
                            <>
                                <span className="dot-sep" />
                                <span>
                                    {project.deployment_groups.length} group{project.deployment_groups.length === 1 ? '' : 's'}
                                    {tabCounts.deployments != null && (
                                        ` · ${tabCounts.deployments} deployment${tabCounts.deployments === 1 ? '' : 's'}`
                                    )}
                                </span>
                            </>
                        )}
                    </div>
                </div>
                {project.primary_url && (
                    <Button icon="ext" onClick={() => window.open(project.primary_url, '_blank', 'noopener,noreferrer')}>Open URL</Button>
                )}
                {project.source_url && isSafeUrl(project.source_url) && (
                    <Button icon="git" onClick={() => window.open(project.source_url, '_blank', 'noopener,noreferrer')}>Repo</Button>
                )}
                <Button variant="danger" icon="trash" onClick={() => setConfirmOpen(true)}>Delete</Button>
            </div>

            <div style={{ marginBottom: 24 }}>
                <Tabs tabs={tabs} active={activeTab} onChange={changeTab} />
            </div>

            <div>
                {activeTab === 'overview' && (
                    <ProjectOverview project={project} projectName={projectName} accessLabel={accessLabel} environments={environments} onCopy={handleCopy} />
                )}
                {activeTab === 'deployments' && <DeploymentsList projectName={projectName} />}
                {activeTab === 'environments' && (
                    <EnvironmentsList projectName={projectName} platformConstraints={project?.platform_constraints} />
                )}
                {activeTab === 'service-accounts' && <ServiceAccountsList projectName={projectName} />}
                {activeTab === 'env-vars' && <EnvVarsList projectName={projectName} />}
                {activeTab === 'domains' && <DomainsList projectName={projectName} defaultUrl={project.default_url} />}
                {activeTab === 'extensions' && <ExtensionsList projectName={projectName} />}
                {activeTab === 'access' && (
                    <AppUsersList
                        projectName={projectName}
                        project={project}
                        accessClasses={accessClasses}
                        currentUserEmail={currentUserEmail}
                        onProjectUpdated={loadProject}
                    />
                )}
            </div>

            <ConfirmDialog
                isOpen={confirmOpen}
                onClose={() => setConfirmOpen(false)}
                onConfirm={handleDelete}
                title={`Delete project ${project.name}?`}
                message={
                    <>
                        <p style={{ marginTop: 0 }}>
                            This removes the project, its deployments, environment variables, service accounts and extensions.
                            This cannot be undone.
                        </p>
                    </>
                }
                confirmText="Delete project"
                requireText={project.name}
                loading={deleting}
            />
        </section>
    );
}

function RecentDeploymentsPanel({ projectName }: { projectName: string }) {
    const [rows, setRows] = useState<any[] | null>(null);

    useEffect(() => {
        let cancelled = false;
        api.getProjectDeployments(projectName, { limit: 6 })
            .then((d: any[]) => { if (!cancelled) setRows(Array.isArray(d) ? d : []); })
            .catch(() => { if (!cancelled) setRows([]); });
        return () => { cancelled = true; };
    }, [projectName]);

    return (
        <Panel>
            <PanelHead title="Recent deployments" sub="Latest deployments across all environments" />
            {rows === null ? (
                <PanelBody><div style={{ color: 'var(--text-muted)', fontSize: 12.5 }}>Loading…</div></PanelBody>
            ) : rows.length === 0 ? (
                <PanelBody><Empty title="No deployments yet" /></PanelBody>
            ) : (
                <table className="r-table">
                    <thead>
                        <tr>
                            <th>ID</th>
                            <th>Status</th>
                            <th>Env</th>
                            <th style={{ textAlign: 'right' }}>Age</th>
                        </tr>
                    </thead>
                    <tbody>
                        {rows.map(d => (
                            <tr
                                key={d.id || d.deployment_id}
                                className="click"
                                onClick={() => navigate(`/deployment/${projectName}/${d.deployment_id}`)}
                            >
                                <td className="mono" style={{ fontSize: 12.25 }}>{d.deployment_id}</td>
                                <td><Status status={d.status || 'Unknown'} /></td>
                                <td>
                                    {d.environment ? (
                                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
                                            <EnvironmentColorDot color={d.environment_color} size="0.5rem" />
                                            {d.environment}
                                        </span>
                                    ) : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                </td>
                                <td style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                    {d.created ? formatRelativeTimeRounded(d.created) : '—'}
                                </td>
                            </tr>
                        ))}
                    </tbody>
                </table>
            )}
        </Panel>
    );
}

// The currently-active deployment in the production environment's default group.
function CurrentDeploymentPanel({ projectName, environments }: { projectName: string; environments: any[] }) {
    const [deployments, setDeployments] = useState<any[] | null>(null);

    useEffect(() => {
        let cancelled = false;
        api.getProjectDeployments(projectName, { limit: 100 })
            .then((d: any[]) => { if (!cancelled) setDeployments(Array.isArray(d) ? d : []); })
            .catch(() => { if (!cancelled) setDeployments([]); });
        return () => { cancelled = true; };
    }, [projectName]);

    const prodEnv = environments.find(e => e.is_production)
        || environments.find(e => e.name === 'production')
        || environments[0];

    let current: any = null;
    if (deployments && prodEnv) {
        const inEnv = deployments.filter(d => d.environment === prodEnv.name && d.is_active);
        const preferredGroup = prodEnv.primary_deployment_group || 'default';
        current = inEnv.find(d => d.deployment_group === preferredGroup) || inEnv[0] || null;
    }

    return (
        <Panel>
            <PanelHead
                title="Current production deployment"
                sub={current ? `${current.deployment_group || 'default'} group` : undefined}
                right={current && (
                    <a className="r-link" style={{ fontSize: 12.5 }} onClick={() => navigate(`/deployment/${projectName}/${current.deployment_id}`)}>
                        View deployment →
                    </a>
                )}
            />
            {deployments === null ? (
                <PanelBody><div style={{ color: 'var(--text-muted)', fontSize: 12.5 }}>Loading…</div></PanelBody>
            ) : !current ? (
                <PanelBody><Empty title="No active production deployment" /></PanelBody>
            ) : (
                <PanelBody style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 20 }}>
                    <div>
                        <div className="r-stat-label">Image</div>
                        <div className="mono" style={{ marginTop: 8, fontSize: 12.5, wordBreak: 'break-all' }}>
                            {current.image ? current.image.split('/').pop() : '—'}
                        </div>
                    </div>
                    <div>
                        <div className="r-stat-label">Status</div>
                        <div style={{ marginTop: 8 }}><Status status={current.status || 'Unknown'} /></div>
                    </div>
                    <div>
                        <div className="r-stat-label">Deployed</div>
                        <div style={{ marginTop: 8, fontSize: 13.5 }}>{current.created ? formatRelativeTimeRounded(current.created) : '—'}</div>
                        {current.created_by_email && (
                            <div style={{ color: 'var(--text-soft)', fontSize: 12 }}>by {current.created_by_email}</div>
                        )}
                    </div>
                    <div>
                        <div className="r-stat-label">Replicas</div>
                        <div style={{ marginTop: 8, fontSize: 13.5 }}>{current.replicas ?? '—'}</div>
                    </div>
                </PanelBody>
            )}
        </Panel>
    );
}

function ProjectOverview({ project, projectName, accessLabel, environments, onCopy }: { project: any; projectName: string; accessLabel: string; environments: any[]; onCopy: (v: string, l: string) => void }) {
    return (
        <div className="r-grid-2-1">
            <div className="r-stack">
                <CurrentDeploymentPanel projectName={projectName} environments={environments} />

                <RecentDeploymentsPanel projectName={projectName} />
            </div>

            <div className="r-stack">
                <Panel>
                    <PanelHead title="About" />
                    <PanelBody>
                        <KV>
                            <KVRow k="Owner">
                                {project.owner?.email || project.owner?.name || '—'}
                            </KVRow>
                            <KVRow k="Access">{accessLabel}</KVRow>
                            {project.primary_url && (
                                <KVRow k="Primary URL">
                                    <a className="r-link mono" style={{ fontSize: 12.5 }} href={project.primary_url} target="_blank" rel="noopener noreferrer">
                                        {stripUrlScheme(project.primary_url)}
                                    </a>
                                </KVRow>
                            )}
                            {project.source_url && isSafeUrl(project.source_url) && (
                                <KVRow k="Source">
                                    <a className="r-link mono" style={{ fontSize: 12.5 }} href={project.source_url} target="_blank" rel="noopener noreferrer">
                                        {stripUrlScheme(project.source_url)}
                                    </a>
                                </KVRow>
                            )}
                            {project.created && (
                                <KVRow k="Created">
                                    <span title={formatISO8601(project.created)}>{formatRelativeTimeRounded(project.created)}</span>
                                </KVRow>
                            )}
                        </KV>
                    </PanelBody>
                </Panel>

                <Panel>
                    <PanelHead title="Environments" sub={`${environments.length} environment${environments.length === 1 ? '' : 's'}`} />
                    <PanelBody>
                        {environments.length === 0 ? (
                            <Empty title="No environments" />
                        ) : (
                            <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                                {environments.map(env => (
                                    <div key={env.name} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 10 }}>
                                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, fontSize: 13.5, fontWeight: 500 }}>
                                            <EnvironmentColorDot color={env.color} size="0.6rem" />
                                            {env.name}
                                            {env.is_production && <Pill kind="env-prod">production</Pill>}
                                        </span>
                                        {env.primary_deployment_group && (
                                            <span style={{ fontSize: 12, color: 'var(--text-soft)' }}>
                                                <span className="mono">{env.primary_deployment_group}</span> group
                                            </span>
                                        )}
                                    </div>
                                ))}
                            </div>
                        )}
                    </PanelBody>
                </Panel>
            </div>
        </div>
    );
}
