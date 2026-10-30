import React, { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { Button, EnvTag, OwnerLabel, Panel, Status, cx, statusTone } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState } from '../components/states';
import { formatRelativeTimeRounded, stripUrlScheme } from '../lib/utils';
import { byNewest } from '../lib/env-state';
import { attentionFor, deploymentAttentionKey, projectAttentionKey, projectNeedsAttention } from '../lib/home-attention';
import { useAttentionDismissals } from '../lib/attention-dismissals';
import { usePolling } from '../lib/polling';
import type { Deployment, Project } from '../lib/project-context';
import { useRecentProjects } from '../lib/recent';
import { deploymentPath, projectPath } from '../lib/routes';
import { QuickstartPanel } from './quickstart-templates';
import { effectiveStatus, statusLabel } from '../lib/project-status';
import { DeploymentSource } from '../components/deployment-source';

interface HomeProps {
    user: { email?: string; id?: string } | null;
}

interface TeamSummary { id: string; name: string }

function timeOfDayGreeting() {
    const h = new Date().getHours();
    if (h < 5) return 'Good evening';
    if (h < 12) return 'Good morning';
    if (h < 18) return 'Good afternoon';
    return 'Good evening';
}

/** How many projects Home reads deployments for (one request each). */
const FAN_OUT = 8;

/**
 * Deployments of the projects you're most likely to care about: the ones you
 * opened recently, then the most recently updated. There is no cross-project
 * deployments endpoint, so this reads each project's recent page.
 */
function useRecentDeployments(projects: Project[] | null, recent: string[]) {
    const [byProject, setByProject] = useState<Record<string, Deployment[]> | null>(null);
    const names = useMemo(() => {
        if (!projects) return [];
        const known = new Set(projects.map(p => p.name));
        const byUpdate = [...projects].sort((a, b) => (b.updated || '').localeCompare(a.updated || '')).map(p => p.name);
        return Array.from(new Set([...recent.filter(n => known.has(n)), ...byUpdate])).slice(0, FAN_OUT);
    }, [projects, recent]);
    const key = names.join(',');

    useEffect(() => {
        if (!projects) return;
        let cancelled = false;
        Promise.all(names.map(name =>
            api.getProjectDeployments(name, { limit: 10 })
                .then((d: Deployment[]) => [name, Array.isArray(d) ? d : []] as const)
                .catch(() => [name, [] as Deployment[]] as const),
        )).then(entries => { if (!cancelled) setByProject(Object.fromEntries(entries)); });
        return () => { cancelled = true; };
        // eslint-disable-next-line react-hooks/exhaustive-deps -- `key` captures `names`
    }, [key, projects]);

    return byProject;
}

export function Home({ user }: HomeProps) {
    const [projects, setProjects] = useState<Project[] | null>(null);
    const [teams, setTeams] = useState<TeamSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const recent = useRecentProjects();
    const { dismissed, dismiss, restore } = useAttentionDismissals(user?.id || user?.email || 'anonymous');
    const [now, setNow] = useState(Date.now);
    usePolling(() => setNow(Date.now()), 60000);
    const deployments = useRecentDeployments(projects, recent);

    useEffect(() => {
        const load = () => Promise.all([api.getProjects(), api.getTeams()])
            .then(([ps, ts]) => { setProjects(ps || []); setTeams(ts || []); })
            .catch((err: Error) => setError(err.message));
        load();
        window.addEventListener('rise:mutation', load);
        return () => window.removeEventListener('rise:mutation', load);
    }, []);

    if (error) return <ErrorState message={`Failed to load workspace: ${error}`} />;
    if (!projects || !teams) return <LoadingState label="Loading workspace…" layout="workspace" />;

    const byName = new Map(projects.map(p => [p.name, p]));
    const healthy = projects.filter(p => effectiveStatus(p) === 'Running').length;
    const candidates = deployments
        ? Object.entries(deployments).flatMap(([name, ds]) => attentionFor(name, ds, now))
            .sort((a, b) => (a.kind === b.kind ? byNewest(a.deployment, b.deployment) : a.kind === 'failed' ? -1 : 1))
        : [];
    const covered = new Set(candidates.map(a => a.project));
    const projectCandidates = projects.filter(p => projectNeedsAttention(p) && !covered.has(p.name));
    const attention = candidates.filter(a => !dismissed.has(deploymentAttentionKey(a.project, a.deployment)));
    const failedProjects = projectCandidates.filter(p => !dismissed.has(projectAttentionKey(p)));
    const hasDismissed = candidates.length > attention.length || projectCandidates.length > failedProjects.length;
    const needAttention = new Set([...attention.map(a => a.project), ...failedProjects.map(p => p.name)]).size;

    const recentDeployments = deployments
        ? Object.entries(deployments).flatMap(([name, ds]) => ds.map(d => ({ project: name, d })))
            .sort((a, b) => byNewest(a.d, b.d)).slice(0, 8)
        : null;

    const viewed = (recent.length > 0 ? recent : [...projects].sort((a, b) => (b.updated || '').localeCompare(a.updated || '')).map(p => p.name))
        .map(n => byName.get(n)).filter((p): p is Project => !!p).slice(0, 4);

    const firstName = (user?.email || '').split('@')[0].split('.')[0];
    const greeting = `${timeOfDayGreeting()}${firstName ? ', ' + firstName.charAt(0).toUpperCase() + firstName.slice(1) : ''}`;

    return (
        <section className="r-home">
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">{greeting}</h1>
                    <div className="r-page-sub">
                        {projects.length} project{projects.length === 1 ? '' : 's'} · {healthy} healthy · {needAttention} need{needAttention === 1 ? 's' : ''} attention
                    </div>
                </div>
                <Button variant="primary" icon="plus" onClick={() => navigate('/projects/new')}>New project</Button>
            </div>

            {(attention.length > 0 || failedProjects.length > 0 || hasDismissed) && (
                <div className="r-home-block">
                    <div className="r-label-row">
                        <span className="r-section-label" title="Failed attempts from the last 7 days, plus ongoing unhealthy deployments and work in progress.">Needs attention</span>
                        {hasDismissed && <Button size="sm" variant="ghost" onClick={restore}>Restore dismissed</Button>}
                    </div>
                    {hasDismissed && attention.length === 0 && failedProjects.length === 0 && <div className="r-list-empty">All current notices dismissed in this browser.</div>}
                    <div className="r-attention-grid">
                        {attention.map(a => (
                            <div key={deploymentAttentionKey(a.project, a.deployment)} className="r-attention">
                                <a className="r-attention-link" href={deploymentPath(a.project, a.deployment.deployment_id)}
                                    onClick={e => { if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return; e.preventDefault(); navigate(deploymentPath(a.project, a.deployment.deployment_id)); }}>
                                    <span className={cx('tile', a.kind)}><Icon name={a.kind === 'failed' ? 'alert' : 'refresh'} size={15} /></span>
                                    <div className="body">
                                        <div className="title">{a.project} {a.deployment.environment && <EnvTag env={a.deployment.environment} color={a.deployment.environment_color} />}</div>
                                        <div className="sub">
                                            {a.kind === 'failed'
                                                ? <>Deploy <span className="mono">{a.deployment.deployment_id}</span> {a.deployment.status === 'Unhealthy' ? 'is unhealthy' : 'failed'}.{a.serving && <> Still serving <span className="mono">{a.serving.deployment_id}</span>.</>}</>
                                                : <>Deploy <span className="mono">{a.deployment.deployment_id}</span> is {a.deployment.status.toLowerCase()}.</>}
                                        </div>
                                        <div className="meta">
                                            <span>Group: <span className="mono">{a.deployment.deployment_group || 'default'}</span></span>
                                            <span aria-hidden="true">·</span>
                                            <time dateTime={a.deployment.created} title={new Date(a.deployment.created).toLocaleString()}>Created {formatRelativeTimeRounded(a.deployment.created)}</time>
                                        </div>
                                    </div>
                                    <span className="cta">{a.kind === 'failed' ? 'Diagnose →' : 'Watch →'}</span>
                                </a>
                                <Button size="sm" variant="ghost" icon="close" aria-label={`Dismiss ${a.project} deployment ${a.deployment.deployment_id}`}
                                    title="Dismiss this notice for your account in this browser" onClick={() => dismiss(deploymentAttentionKey(a.project, a.deployment))} />
                            </div>
                        ))}
                        {failedProjects.map(p => (
                            <div key={projectAttentionKey(p)} className="r-attention">
                                <a className="r-attention-link" href={projectPath(p.name)}
                                    onClick={e => { if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return; e.preventDefault(); navigate(projectPath(p.name)); }}>
                                    <span className="tile failed"><Icon name="alert" size={15} /></span>
                                    <div className="body">
                                        <div className="title">{p.name}</div>
                                        <div className="sub">Project status: {p.status}.</div>
                                    </div>
                                    <span className="cta">Open →</span>
                                </a>
                                <Button size="sm" variant="ghost" icon="close" aria-label={`Dismiss ${p.name} project notice`}
                                    title="Dismiss this notice for your account in this browser" onClick={() => dismiss(projectAttentionKey(p))} />
                            </div>
                        ))}
                    </div>
                </div>
            )}

            <div className="r-home-cols">
                <div>
                    <div className="r-label-row"><span className="r-section-label">Recent deployments</span></div>
                    <Panel className="r-list">
                        {recentDeployments === null ? (
                            <div className="r-list-empty">Loading…</div>
                        ) : recentDeployments.length === 0 ? (
                            <div className="r-list-empty">No deployments yet.</div>
                        ) : recentDeployments.map(({ project, d }) => (
                            <div key={`${project}/${d.deployment_id}`} role="link" tabIndex={0} className="r-list-row r-dep-row"
                                onClick={() => navigate(deploymentPath(project, d.deployment_id))}
                                onKeyDown={e => { if (e.key === 'Enter') navigate(deploymentPath(project, d.deployment_id)); }}>
                                <span className={cx('r-dot', statusTone(d.status))} title={d.status} />
                                <span className="proj" title={project}>{project}</span>
                                <span className="r-dep-env">{d.environment && <EnvTag env={d.environment} color={d.environment_color} />}</span>
                                <span className="msg"><DeploymentSource d={d} projectName={project} /></span>
                                <span className="age">{formatRelativeTimeRounded(d.created)}</span>
                            </div>
                        ))}
                    </Panel>
                </div>
                <div>
                    <div className="r-label-row">
                        <span className="r-section-label">{recent.length > 0 ? 'Recently viewed' : 'Recently updated'}</span>
                        <a className="r-link" href="/projects" onClick={e => { e.preventDefault(); navigate('/projects'); }}>All projects →</a>
                    </div>
                    {viewed.length === 0 ? (
                        <Panel><div className="r-list-empty">
                            No projects yet. Create one, or follow the <a className="r-link" href="/docs/user-guide/getting-started/">Getting Started</a> guide.
                        </div></Panel>
                    ) : (
                        <div className="r-project-cards">
                            {viewed.map(p => (
                                <div key={p.name} className="r-project-card" role="link" tabIndex={0}
                                    onClick={() => navigate(projectPath(p.name))}
                                    onKeyDown={e => { if (e.key === 'Enter') navigate(projectPath(p.name)); }}>
                                    <div className="top">
                                        <span className="name">{p.name}</span>
                                        <Status status={effectiveStatus(p)} label={statusLabel(p)} />
                                    </div>
                                    {p.primary_url && <div className="url mono">{stripUrlScheme(p.primary_url)}</div>}
                                    <div className="meta">
                                        <OwnerLabel owner={p.owner} />
                                        <span>· {formatRelativeTimeRounded(p.updated || p.created)}</span>
                                    </div>
                                </div>
                            ))}
                        </div>
                    )}
                    <div style={{ marginTop: 20 }}>
                        <QuickstartPanel />
                    </div>
                </div>
            </div>
        </section>
    );
}
