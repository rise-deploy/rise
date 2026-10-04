import React, { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatISO8601, formatRelativeTimeRounded, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { useToast } from '../components/toast';
import { Alert, Button, ConfirmDialog, EnvTag, KV, KVRow, OwnerLabel, Panel, PanelBody, PanelHead, Tooltip, cx, statusTone } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState } from '../components/states';
import { DeploymentsList } from './deployments';
import { ProjectStatusPill } from '../components/project-table';
import { DomainsList, ExtensionsList } from './resources';
import { VariablesSection } from './variables';
import { EnvironmentRows, EnvironmentsSection } from './environments';
import { AppUsersList } from './projects';
import { useQuickstartTemplates } from './quickstart-templates';
import { useCurrentProject, type Project } from '../lib/project-context';
import { useDeployActions } from '../components/deploy-actions';
import { byNewest, deploymentSource, envState, productionEnv, sortEnvironments } from '../lib/env-state';
import { deploymentPath, projectPath, type ProjectSection } from '../lib/routes';
import { SectionHead } from '../components/section-head';

// Sections that render their own head because it carries actions.
const SELF_HEADED = new Set<ProjectSection>(['environments', 'variables', 'domains', 'extensions']);

interface AccessClass { id: string; display_name?: string; description?: string }

export function ProjectDetail({ section }: { section: ProjectSection }) {
    const current = useCurrentProject()!;
    const { projectName, project, error, reload } = current;
    const [accessClasses, setAccessClasses] = useState<AccessClass[]>([]);
    const [currentUserEmail, setCurrentUserEmail] = useState<string>('');

    useEffect(() => {
        api.getAccessClasses().then((d: { access_classes?: AccessClass[] }) => setAccessClasses(d?.access_classes || [])).catch(() => {});
        api.getMe().then((u: { email?: string }) => setCurrentUserEmail(u?.email || '')).catch(() => {});
    }, []);

    if (error) return <ErrorState message={`Failed to load project: ${error}`} onRetry={reload} />;
    if (!project) return <LoadingState label="Loading project…" />;

    const accessClass = accessClasses.find(a => a.id === project.access_class);
    const accessLabel = accessClass?.display_name || project.access_class || '—';

    return (
        <section>
            {section === 'overview' && (
                <ProjectHeader project={project} accessLabel={accessLabel} accessDescription={accessClass?.description || ''} />
            )}
            {section !== 'overview' && !SELF_HEADED.has(section) && <SectionHead section={section} />}

            {section === 'overview' && <ProjectOverview project={project} onUpdated={reload} />}
            {section === 'deployments' && <DeploymentsList projectName={projectName} />}
            {section === 'environments' && <EnvironmentsSection />}
            {section === 'variables' && <VariablesSection />}
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
            <RedeployProductionButton />
        </div>
    );
}

function RedeployProductionButton() {
    const current = useCurrentProject()!;
    const { request } = useDeployActions();
    const prod = productionEnv(current.environments);
    const live = prod ? envState(prod, current.deployments).live : null;
    if (!prod || !live) return null;
    return (
        <Button variant="primary" icon="refresh" onClick={() => request({ kind: 'redeploy', source: live })}>
            Redeploy {prod.name}
        </Button>
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
        } catch (err) {
            showToast(`Failed to delete project: ${(err as Error).message}`, 'error');
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

// Surfaces the quickstart template a project was created from.
// The "Redeploy from template" button is enabled whenever the project's
// catalog entry still exists — clicking it always pulls the catalog image
// fresh, which is useful both when the catalog has moved ahead of the deployed
// version and when the image uses a floating tag (e.g. `:latest`) that the
// drift detector can't observe.
function ProjectTemplatePanel({ project, projectName, onUpdated }: { project: Project; projectName: string; onUpdated: () => void }) {
    const { templates } = useQuickstartTemplates();
    const { showToast } = useToast();
    const [updating, setUpdating] = useState(false);
    const prodEnvName = productionEnv(useCurrentProject()?.environments ?? [])?.name ?? null;
    if (!project.template) return null;

    const stored = project.template;
    const catalog = templates?.find(t => t.id === stored.id) || null;
    const updateAvailable = !!catalog && catalog.image !== stored.image;

    const handleUpdate = async () => {
        if (!catalog) return;
        setUpdating(true);
        try {
            await api.createDeploymentFromImage(projectName, catalog.image, catalog.http_port, prodEnvName);
            await api.updateProjectTemplateImage(projectName, catalog.image);
            showToast(`Redeploying ${catalog.display_name} with ${catalog.image}…`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            onUpdated();
        } catch (err) {
            showToast(`Failed to redeploy from template: ${(err as Error).message}`, 'error');
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


function SectionLabelRow({ label, href, link }: { label: string; href?: string; link?: string }) {
    return (
        <div className="r-label-row">
            <span className="r-section-label">{label}</span>
            {href && link && (
                <a className="r-link" href={href} onClick={(e) => { if (e.metaKey || e.ctrlKey) return; e.preventDefault(); navigate(href); }}>
                    {link}
                </a>
            )}
        </div>
    );
}

function ProjectOverview({ project, onUpdated }: { project: Project; onUpdated: () => void }) {
    const { projectName, environments, deployments } = useCurrentProject()!;
    const states = useMemo(
        () => sortEnvironments(environments).map(env => envState(env, deployments)),
        [environments, deployments],
    );
    const recent = useMemo(() => [...deployments].sort(byNewest).slice(0, 5), [deployments]);
    const issues = states.filter(s => s.failedAttempt);

    return (
        <div className="r-stack">
            {issues.map(s => {
                const failed = s.failedAttempt!;
                return (
                    <div key={s.env.name} className="r-issue">
                        <Icon name="alert" size={16} />
                        <div className="text">
                            The last {s.env.name} deploy (<span className="mono">{failed.deployment_id}</span>) {failed.status === 'Unhealthy' ? 'is unhealthy' : 'failed'}
                            {failed.error_message ? <>: {failed.error_message}</> : '.'}
                            {s.live && <> <span className="mono">{s.live.deployment_id}</span> is still serving traffic.</>}
                        </div>
                        <Button onClick={() => navigate(deploymentPath(projectName, failed.deployment_id))}>Diagnose →</Button>
                    </div>
                );
            })}

            <div>
                <SectionLabelRow label="Environments" href={projectPath(projectName, 'environments')} link="Manage →" />
                {states.length === 0
                    ? <Panel><PanelBody><span className="muted">No environments.</span></PanelBody></Panel>
                    : <EnvironmentRows states={states} projectName={projectName} />}
            </div>

            <div>
                <SectionLabelRow label="Recent deployments" href={projectPath(projectName, 'deployments')} link="View all →" />
                <Panel className="r-list">
                    {recent.length === 0 ? (
                        <div className="r-list-empty">No deployments yet. Deploy with <span className="mono">rise deploy</span>.</div>
                    ) : recent.map(d => (
                        <div
                            key={d.deployment_id}
                            role="link"
                            tabIndex={0}
                            className="r-list-row r-dep-row"
                            onClick={() => navigate(deploymentPath(projectName, d.deployment_id))}
                            onKeyDown={e => { if (e.key === 'Enter') navigate(deploymentPath(projectName, d.deployment_id)); }}
                        >
                            <span className={cx('r-dot', statusTone(d.status))} title={d.status} />
                            <span className="mono id">{d.deployment_id}</span>
                            {d.environment && <EnvTag env={d.environment} />}
                            <span className="msg">{deploymentSource(d)}</span>
                            <span className="age">{formatRelativeTimeRounded(d.created)}</span>
                        </div>
                    ))}
                </Panel>
            </div>

            <ProjectTemplatePanel project={project} projectName={projectName} onUpdated={onUpdated} />
        </div>
    );
}
