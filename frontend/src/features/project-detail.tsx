// @ts-nocheck
import React, { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatISO8601, formatRelativeTimeRounded, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { useToast } from '../components/toast';
import { Alert, Button, ConfirmDialog, Empty, EnvPill, GroupPill, KV, KVRow, OwnerLabel, Panel, PanelBody, PanelHead, Pill, SourceLinkGroup, Status, Tooltip } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState, EmptyState } from '../components/states';

import { DeploymentsList } from './deployments';
import { ProjectStatusPill } from '../components/project-table';
import { DomainsList, EnvironmentsList, EnvVarsList, ExtensionsList } from './resources';
import { AppUsersList } from './projects';
import { useQuickstartTemplates } from './quickstart-templates';
import { useCurrentProject, type Project } from '../lib/project-context';
import { PROJECT_SECTIONS, type ProjectSection } from '../lib/routes';

export function ProjectDetail({ section }: { section: ProjectSection }) {
    const current = useCurrentProject()!;
    const { projectName, project, environments, error, reload } = current;
    const [accessClasses, setAccessClasses] = useState<any[]>([]);
    const [currentUserEmail, setCurrentUserEmail] = useState<string>('');

    useEffect(() => {
        api.getAccessClasses().then(d => setAccessClasses(d?.access_classes || [])).catch(() => {});
        api.getMe().then(u => setCurrentUserEmail(u?.email || '')).catch(() => {});
    }, []);

    if (error) return <ErrorState message={`Failed to load project: ${error}`} onRetry={reload} />;
    if (!project) return <LoadingState label="Loading project…" />;

    const accessClass = accessClasses.find(a => a.id === project.access_class);
    const accessLabel = accessClass?.display_name || project.access_class || '—';
    const info = PROJECT_SECTIONS.find(s => s.id === section)!;

    return (
        <section>
            {section === 'overview' ? (
                <ProjectHeader project={project} accessLabel={accessLabel} accessDescription={accessClass?.description || ''} />
            ) : (
                <div className="r-page-head">
                    <div className="title-stack">
                        <h1 className="r-page-title">{info.label}</h1>
                        {info.sub && <div className="r-page-sub">{info.sub}</div>}
                    </div>
                </div>
            )}

            {section === 'overview' && (
                <ProjectOverview project={project} projectName={projectName} accessLabel={accessLabel} environments={environments} onUpdated={reload} />
            )}
            {section === 'deployments' && <DeploymentsList projectName={projectName} />}
            {section === 'environments' && (
                <EnvironmentsList projectName={projectName} platformConstraints={project?.platform_constraints} />
            )}
            {section === 'variables' && <EnvVarsList projectName={projectName} />}
            {section === 'domains' && <DomainsList projectName={projectName} defaultUrl={project.default_url} />}
            {section === 'extensions' && <ExtensionsList projectName={projectName} />}
            {section === 'access' && (
                <div className="r-stack">
                    <AppUsersList
                        projectName={projectName}
                        project={project}
                        accessClasses={accessClasses}
                        currentUserEmail={currentUserEmail}
                        onProjectUpdated={reload}
                    />
                    <DangerZone project={project} />
                </div>
            )}
        </section>
    );
}

function ProjectHeader({ project, accessLabel, accessDescription }: { project: Project; accessLabel: string; accessDescription: string }) {
    // The source repository is either explicitly configured on the project, or
    // resolved by the backend from the active/most recent deployment metadata.
    const sourceUrl: string | null = project.source_url || project.resolved_source_url || null;
    const sourceUrlInherited = !project.source_url && !!project.resolved_source_url;

    return (
        <div className="r-page-head">
            <div className="title-stack">
                <div className="r-title-row">
                    <h1 className="r-page-title">{project.name}</h1>
                    <ProjectStatusPill project={project} tooltip={`Lifecycle status: ${project.status || 'Unknown'}`} />
                </div>
                <div className="r-meta-bar r-meta-wrap">
                    {project.primary_url && isSafeUrl(project.primary_url) && (
                        <a className="r-link mono" href={project.primary_url} target="_blank" rel="noopener noreferrer">
                            {stripUrlScheme(project.primary_url)}
                        </a>
                    )}
                    {sourceUrl && isSafeUrl(sourceUrl) && (
                        <a
                            className="r-meta-link"
                            href={sourceUrl}
                            target="_blank"
                            rel="noopener noreferrer"
                            title={sourceUrlInherited ? 'Source URL resolved from deployment metadata' : undefined}
                        >
                            <Icon name="git" size={13} />
                            {stripUrlScheme(sourceUrl).replace(/^(github\.com|gitlab\.com)\//, '')}
                        </a>
                    )}
                    <OwnerLabel owner={project.owner} link />
                    <Tooltip content={accessDescription ? <><div><strong>{accessLabel}</strong></div><div>{accessDescription}</div></> : `Access class: ${accessLabel}`}>
                        <span className="r-meta-link"><Icon name="lock" size={13} />{accessLabel}</span>
                    </Tooltip>
                    {project.created && (
                        <span title={formatISO8601(project.created)}>created {formatRelativeTimeRounded(project.created)}</span>
                    )}
                </div>
            </div>
        </div>
    );
}

function DangerZone({ project }: { project: Project }) {
    const [confirmOpen, setConfirmOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const { showToast } = useToast();

    const handleDelete = async () => {
        setDeleting(true);
        try {
            await api.deleteProject(project.name);
            showToast(`Project ${project.name} deleted`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            navigate('/projects');
        } catch (err: any) {
            showToast(`Failed to delete project: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    return (
        <Panel className="r-danger-zone">
            <PanelHead
                title="Delete project"
                sub="Removes the project, its deployments, environment variables, service accounts and extensions."
                right={<Button variant="danger" icon="trash" onClick={() => setConfirmOpen(true)}>Delete project</Button>}
            />
            <ConfirmDialog
                isOpen={confirmOpen}
                onClose={() => setConfirmOpen(false)}
                onConfirm={handleDelete}
                title={`Delete project ${project.name}?`}
                message={
                    <p style={{ marginTop: 0 }}>
                        This removes the project, its deployments, environment variables, service accounts and extensions.
                        This cannot be undone.
                    </p>
                }
                confirmText="Delete project"
                requireText={project.name}
                loading={deleting}
            />
        </Panel>
    );
}

// The deployment currently serving each environment's groups.
function ActiveDeploymentsPanel({ projectName, environments }: { projectName: string; environments: any[] }) {
    const [rows, setRows] = useState<any[] | null>(null);

    const load = useCallback(() => {
        let cancelled = false;
        api.getProjectDeployments(projectName, { active: true, limit: 100 })
            .then((d: any[]) => { if (!cancelled) setRows(Array.isArray(d) ? d : []); })
            .catch(() => { if (!cancelled) setRows([]); });
        return () => { cancelled = true; };
    }, [projectName]);

    useEffect(() => {
        const cleanup = load();
        // Refresh whenever a mutation elsewhere in the app might have produced
        // a new deployment (e.g. the "Redeploy from template" button).
        const onMutation = () => { load(); };
        window.addEventListener('rise:mutation', onMutation);
        return () => {
            cleanup?.();
            window.removeEventListener('rise:mutation', onMutation);
        };
    }, [load]);

    return (
        <Panel>
            <PanelHead
                title="Active deployments"
                sub="Currently serving across all environments and groups"
                right={
                    <a
                        className="r-link"
                        style={{ fontSize: 12.5 }}
                        href={`/project/${projectName}/deployments`}
                        onClick={(e) => { e.preventDefault(); navigate(`/project/${projectName}/deployments`); }}
                    >
                        All deployments →
                    </a>
                }
            />
            {rows === null ? (
                <PanelBody><div style={{ color: 'var(--text-muted)', fontSize: 12.5 }}>Loading…</div></PanelBody>
            ) : rows.length === 0 ? (
                <PanelBody><Empty title="No active deployments" /></PanelBody>
            ) : (
                <table className="r-table">
                    <thead>
                        <tr>
                            <th>ID</th>
                            <th>Status</th>
                            <th>Env</th>
                            <th>Group</th>
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
                                        <EnvPill projectName={projectName} env={d.environment} color={d.environment_color} />
                                    ) : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                </td>
                                <td>
                                    {d.deployment_group ? (() => {
                                        const env = (environments || []).find((e: any) => e.name === d.environment);
                                        return (
                                            <GroupPill
                                                projectName={projectName}
                                                group={d.deployment_group}
                                                primary={!!env && env.primary_deployment_group === d.deployment_group}
                                            />
                                        );
                                    })() : <span style={{ color: 'var(--text-soft)' }}>—</span>}
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
        api.getProjectDeployments(projectName, { active: true, limit: 100 })
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
        <Panel onClick={current ? () => navigate(`/deployment/${projectName}/${current.deployment_id}`) : undefined}>
            <PanelHead
                title="Current production deployment"
                sub={current ? `${current.deployment_group || 'default'} group` : undefined}
                right={current && (
                    <span
                        className="r-link"
                        style={{ fontSize: 12.5 }}
                    >
                        View deployment →
                    </span>
                )}
            />
            {deployments === null ? (
                <PanelBody><div style={{ color: 'var(--text-muted)', fontSize: 12.5 }}>Loading…</div></PanelBody>
            ) : !current ? (
                <PanelBody><Empty title="No active production deployment" /></PanelBody>
            ) : (
                <PanelBody style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 20 }}>
                    <div>
                        <div className="r-stat-label">Source</div>
                        <div style={{ marginTop: 8 }} onClick={(e) => {
                            if ((e.target as HTMLElement).closest('a, button')) e.stopPropagation();
                        }}>
                            {(current.pull_request_url || current.job_url) ? (
                                <SourceLinkGroup
                                    jobUrl={current.job_url}
                                    prUrl={current.pull_request_url}
                                />
                            ) : current.git_repository_url && isSafeUrl(current.git_repository_url) ? (
                                <a
                                    className="r-link mono"
                                    style={{ fontSize: 12.5 }}
                                    href={current.git_repository_url}
                                    target="_blank"
                                    rel="noopener noreferrer"
                                >
                                    {stripUrlScheme(current.git_repository_url)}
                                </a>
                            ) : (
                                <span style={{ color: 'var(--text-soft)', fontSize: 12.5 }}>—</span>
                            )}
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

// Right-column panel surfacing the quickstart template a project was created
// from. The "Redeploy from template" button is enabled whenever the project's
// catalog entry still exists — clicking it always pulls the catalog image
// fresh, which is useful both when the catalog has moved ahead of the deployed
// version and when the image uses a floating tag (e.g. `:latest`) that the
// drift detector can't observe. When the catalog `image:tag` differs from the
// stored one, the panel surfaces an "Update available" badge and promotes the
// button to the primary variant so the action stands out.
function ProjectTemplatePanel({ project, projectName, onUpdated }: { project: any; projectName: string; onUpdated: () => void }) {
    const { templates } = useQuickstartTemplates();
    const { showToast } = useToast();
    const [updating, setUpdating] = useState(false);
    if (!project.template) return null;

    const stored = project.template;
    const catalog = templates?.find((t: any) => t.id === stored.id) || null;
    const updateAvailable = !!catalog && catalog.image !== stored.image;

    const handleUpdate = async () => {
        if (!catalog) return;
        setUpdating(true);
        try {
            await api.createDeploymentFromImage(projectName, catalog.image, catalog.http_port);
            await api.updateProjectTemplateImage(projectName, catalog.image);
            showToast(`Redeploying ${catalog.display_name} with ${catalog.image}…`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            onUpdated();
        } catch (err: any) {
            showToast(`Failed to redeploy from template: ${err.message}`, 'error');
        } finally {
            setUpdating(false);
        }
    };

    const actionTooltip = catalog && (
        <div>
            <div>
                Redeploy with <code className="mono">{catalog.image}</code>.
            </div>
            <div style={{ marginTop: 6, fontStyle: 'italic', color: 'var(--text-soft)' }}>
                {updateAvailable
                    ? 'The catalog has moved ahead of the version currently deployed.'
                    : 'The image digest will be refreshed to whatever the tag currently resolves to — useful when the catalog uses a floating tag (e.g. :latest).'}
            </div>
        </div>
    );

    return (
        <Panel>
            <PanelHead
                title="Template"
                right={
                    catalog && (
                        <Tooltip content={actionTooltip} focusable>
                            <Button
                                icon="refresh"
                                variant={updateAvailable ? 'primary' : undefined}
                                loading={updating}
                                onClick={handleUpdate}
                            >
                                {updateAvailable ? 'Upgrade' : 'Redeploy'}
                            </Button>
                        </Tooltip>
                    )
                }
            />
            <PanelBody>
                <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 12 }}>
                    {catalog?.icon_url && (
                        <img src={catalog.icon_url} alt="" width={32} height={32} style={{ borderRadius: 6 }} />
                    )}
                    <div style={{ minWidth: 0, flex: 1 }}>
                        <div style={{ fontWeight: 500, fontSize: 13.5 }}>
                            {catalog?.display_name || stored.id}
                        </div>
                        {catalog?.tagline && (
                            <div style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>{catalog.tagline}</div>
                        )}
                    </div>
                </div>
                <KV>
                    <KVRow k="Deployed">
                        <code className="mono" style={{ fontSize: 12 }}>{stored.image}</code>
                    </KVRow>
                    {catalog && (
                        <KVRow k="Catalog">
                            <code className="mono" style={{ fontSize: 12 }}>{catalog.image}</code>
                        </KVRow>
                    )}
                </KV>
                {!catalog && (
                    <div style={{ marginTop: 10 }}>
                        <Alert tone="info" icon="info">
                            This template is no longer in the catalog. Redeploy is unavailable.
                        </Alert>
                    </div>
                )}
            </PanelBody>
        </Panel>
    );
}

function ProjectOverview({ project, projectName, accessLabel, environments, onUpdated }: { project: any; projectName: string; accessLabel: string; environments: any[]; onUpdated: () => void }) {
    const sourceUrl: string | null = project.source_url || project.resolved_source_url || null;
    const sourceUrlInherited = !project.source_url && !!project.resolved_source_url;
    return (
        <div className="r-grid-2-1">
            <div className="r-stack">
                <CurrentDeploymentPanel projectName={projectName} environments={environments} />

                <ActiveDeploymentsPanel projectName={projectName} environments={environments} />
            </div>

            <div className="r-stack">
                <ProjectTemplatePanel project={project} projectName={projectName} onUpdated={onUpdated} />

                <Panel>
                    <PanelHead title="About" />
                    <PanelBody>
                        <KV>
                            <KVRow k="Owner">
                                {project.owner?.email || project.owner?.name || '—'}
                            </KVRow>
                            <KVRow k="Access">{accessLabel}</KVRow>
                            {project.primary_url && isSafeUrl(project.primary_url) && (
                                <KVRow k="Primary URL">
                                    <a className="r-link mono" style={{ fontSize: 12.5 }} href={project.primary_url} target="_blank" rel="noopener noreferrer">
                                        {stripUrlScheme(project.primary_url)}
                                    </a>
                                </KVRow>
                            )}
                            {sourceUrl && isSafeUrl(sourceUrl) && (
                                <KVRow k="Source">
                                    <span style={{ display: 'inline-flex', alignItems: 'baseline', gap: 6, flexWrap: 'wrap' }}>
                                        <a className="r-link mono" style={{ fontSize: 12.5 }} href={sourceUrl} target="_blank" rel="noopener noreferrer">
                                            {stripUrlScheme(sourceUrl)}
                                        </a>
                                        {sourceUrlInherited && (
                                            <span
                                                style={{ fontSize: 11, color: 'var(--text-soft)' }}
                                                title="Resolved from deployment metadata; not explicitly configured on the project"
                                            >
                                                from deployment
                                            </span>
                                        )}
                                    </span>
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
                                {environments.map(env => {
                                    const target = `/project/${projectName}/environment/${env.name}`;
                                    return (
                                        <div
                                            key={env.name}
                                            className="r-env-row"
                                            role="link"
                                            tabIndex={0}
                                            aria-label={`Open ${env.name} environment`}
                                            onClick={() => navigate(target)}
                                            onKeyDown={(e) => {
                                                if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); navigate(target); }
                                            }}
                                        >
                                            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                                                <EnvPill projectName={projectName} env={env.name} color={env.color} />
                                                {env.is_production && env.name !== 'production' && (
                                                    <Pill kind="env-prod">production</Pill>
                                                )}
                                            </span>
                                            {env.primary_deployment_group && (
                                                <span style={{ fontSize: 12, color: 'var(--text-soft)' }}>
                                                    <span className="mono">{env.primary_deployment_group}</span> group
                                                </span>
                                            )}
                                        </div>
                                    );
                                })}
                            </div>
                        )}
                    </PanelBody>
                </Panel>
            </div>
        </div>
    );
}
