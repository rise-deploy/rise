import React, { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { Button, EnvTag, OwnerLabel, Panel, Status, cx, statusTone } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState } from '../components/states';
import { formatRelativeTimeRounded, stripUrlScheme } from '../lib/utils';
import { byNewest, deploymentSource, isInProgress } from '../lib/env-state';
import type { Deployment, Project } from '../lib/project-context';
import { useRecentProjects } from '../lib/recent';
import { deploymentPath, projectPath } from '../lib/routes';
import { QuickstartPanel } from './quickstart-templates';

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

interface Attention {
    project: string;
    kind: 'failed' | 'deploying';
    deployment: Deployment;
    serving: Deployment | null;
}

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

/** Per environment+group: a deploy newer than what's serving that failed, or one in flight. */
function attentionFor(project: string, deployments: Deployment[]): Attention[] {
    const out: Attention[] = [];
    const groups = new Map<string, Deployment[]>();
    for (const d of deployments) {
        const k = `${d.environment}/${d.deployment_group}`;
        groups.set(k, [...(groups.get(k) ?? []), d]);
    }
    for (const ds of groups.values()) {
        const sorted = [...ds].sort(byNewest);
        const latest = sorted[0];
        const serving = sorted.find(d => d.is_active) ?? null;
        if (latest.status === 'Failed' && serving !== latest) out.push({ project, kind: 'failed', deployment: latest, serving });
        else if (latest.status === 'Unhealthy') out.push({ project, kind: 'failed', deployment: latest, serving: null });
        else if (isInProgress(latest.status)) out.push({ project, kind: 'deploying', deployment: latest, serving });
    }
    return out;
}

export function Home({ user }: HomeProps) {
    const [projects, setProjects] = useState<Project[] | null>(null);
    const [teams, setTeams] = useState<TeamSummary[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const recent = useRecentProjects();
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
    if (!projects || !teams) return <LoadingState label="Loading workspace…" />;

    const byName = new Map(projects.map(p => [p.name, p]));
    const healthy = projects.filter(p => p.status === 'Running').length;
    const attention = deployments
        ? Object.entries(deployments).flatMap(([name, ds]) => attentionFor(name, ds))
            .sort((a, b) => (a.kind === b.kind ? byNewest(a.deployment, b.deployment) : a.kind === 'failed' ? -1 : 1))
        : [];
    // Failed projects whose deployments weren't read still need a card.
    const covered = new Set(attention.map(a => a.project));
    const failedProjects = projects.filter(p => (p.status === 'Failed' || p.status === 'Deleting') && !covered.has(p.name));
    const needAttention = new Set([...attention.filter(a => a.kind === 'failed').map(a => a.project), ...failedProjects.map(p => p.name)]).size;

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

            {(attention.length > 0 || failedProjects.length > 0) && (
                <div className="r-home-block">
                    <div className="r-label-row"><span className="r-section-label">Needs attention</span></div>
                    <div className="r-attention-grid">
                        {attention.map(a => (
                            <div key={a.deployment.deployment_id} className="r-attention" role="link" tabIndex={0}
                                onClick={() => navigate(deploymentPath(a.project, a.deployment.deployment_id))}
                                onKeyDown={e => { if (e.key === 'Enter') navigate(deploymentPath(a.project, a.deployment.deployment_id)); }}>
                                <span className={cx('tile', a.kind)}><Icon name={a.kind === 'failed' ? 'alert' : 'refresh'} size={15} /></span>
                                <div className="body">
                                    <div className="title">{a.project} {a.deployment.environment && <EnvTag env={a.deployment.environment} color={a.deployment.environment_color} />}</div>
                                    <div className="sub">
                                        {a.kind === 'failed'
                                            ? <>Deploy <span className="mono">{a.deployment.deployment_id}</span> {a.deployment.status === 'Unhealthy' ? 'is unhealthy' : 'failed'}.{a.serving && <> Still serving <span className="mono">{a.serving.deployment_id}</span>.</>}</>
                                            : <>Deploy <span className="mono">{a.deployment.deployment_id}</span> is {a.deployment.status.toLowerCase()}.</>}
                                    </div>
                                </div>
                                <span className="cta">{a.kind === 'failed' ? 'Diagnose →' : 'Watch →'}</span>
                            </div>
                        ))}
                        {failedProjects.map(p => (
                            <div key={p.name} className="r-attention" role="link" tabIndex={0}
                                onClick={() => navigate(projectPath(p.name))}
                                onKeyDown={e => { if (e.key === 'Enter') navigate(projectPath(p.name)); }}>
                                <span className="tile failed"><Icon name="alert" size={15} /></span>
                                <div className="body">
                                    <div className="title">{p.name}</div>
                                    <div className="sub">Project status: {p.status}.</div>
                                </div>
                                <span className="cta">Open →</span>
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
                                <span className="proj">{project}</span>
                                {d.environment && <EnvTag env={d.environment} color={d.environment_color} />}
                                <span className="msg">{deploymentSource(d)}</span>
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
                                        <Status status={p.status || 'Unknown'} />
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
