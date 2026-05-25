// @ts-nocheck
import React, { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { Button, EnvPill, Panel, PanelBody, PanelHead, Stat, StatGrid, Status, cx, colorFor, Avatar } from '../components/r-ui';
import { LoadingState, ErrorState } from '../components/states';
import { formatRelativeTimeRounded } from '../lib/utils';

interface HomeProps {
    user: { email?: string; id?: string } | null;
}

function timeOfDayGreeting() {
    const h = new Date().getHours();
    if (h < 5) return 'Good evening';
    if (h < 12) return 'Good morning';
    if (h < 18) return 'Good afternoon';
    return 'Good evening';
}

export function Home({ user }: HomeProps) {
    const [projects, setProjects] = useState<any[] | null>(null);
    const [teams, setTeams] = useState<any[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        Promise.all([api.getProjects(), api.getTeams()])
            .then(([ps, ts]) => { setProjects(ps || []); setTeams(ts || []); })
            .catch(err => setError(err.message));
    }, []);

    if (error) return <ErrorState message={`Failed to load workspace: ${error}`} />;
    if (!projects || !teams) return <LoadingState label="Loading workspace…" />;

    const counts = {
        healthy: projects.filter(p => (p.status || '').toLowerCase() === 'running').length,
        // "unhealthy" surfaces anything operator attention should care about:
        // failed rollouts and projects currently being deleted (deletes can stall).
        unhealthy: projects.filter(p => {
            const s = (p.status || '').toLowerCase();
            return s === 'failed' || s === 'deleting';
        }).length,
        deploying: projects.filter(p => (p.status || '').toLowerCase() === 'deploying').length,
        // Stopped/Terminated projects exist but aren't running and aren't unhealthy;
        // surface them as their own stat so the totals reconcile.
        inactive: projects.filter(p => {
            const s = (p.status || '').toLowerCase();
            return s === 'stopped' || s === 'terminated';
        }).length,
    };

    const recent = [...projects]
        .sort((a, b) => new Date(b.updated || b.created || 0).getTime() - new Date(a.updated || a.created || 0).getTime())
        .slice(0, 5);

    const firstName = (user?.email || '').split('@')[0].split('.')[0];
    const greeting = `${timeOfDayGreeting()}${firstName ? ', ' + firstName.charAt(0).toUpperCase() + firstName.slice(1) : ''}.`;

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">{greeting}</h1>
                    <div className="r-page-sub">
                        {projects.length} project{projects.length === 1 ? '' : 's'} across {teams.length} team{teams.length === 1 ? '' : 's'}.
                    </div>
                </div>
                <Button variant="primary" icon="arrow" onClick={() => navigate('/projects')}>All projects</Button>
            </div>

            <StatGrid cols={3}>
                <Stat
                    label="Healthy"
                    value={counts.healthy}
                    unit={` / ${projects.length}`}
                    delta={
                        counts.unhealthy > 0
                            ? `${counts.unhealthy} need attention`
                            : counts.inactive > 0
                                ? `${counts.inactive} stopped or terminated`
                                : 'all probes passing'
                    }
                    deltaTone={counts.unhealthy > 0 ? 'down' : undefined}
                />
                <Stat
                    label="In progress"
                    value={counts.deploying}
                    delta={counts.deploying ? 'deploys building or rolling out' : 'no active deploys'}
                />
                <Stat
                    label="Teams"
                    value={teams.length}
                    delta={`${teams.reduce((n, t) => n + (t.members?.length || 0), 0)} members total`}
                />
            </StatGrid>

            <div className="r-grid-2-1" style={{ alignItems: 'start' }}>
                <Panel>
                    <PanelHead title="Recent projects" sub="Most recently updated across the workspace" />
                    {recent.length === 0 ? (
                        <div className="r-panel-body" style={{ color: 'var(--text-muted)', fontSize: 13 }}>
                            No projects yet. Create one to get started.
                        </div>
                    ) : (
                        <table className="r-table">
                            <thead>
                                <tr>
                                    <th>Project</th>
                                    <th>Status</th>
                                    <th>Owner</th>
                                    <th>Access</th>
                                    <th style={{ textAlign: 'right' }}>Updated</th>
                                </tr>
                            </thead>
                            <tbody>
                                {recent.map(p => (
                                    <tr
                                        key={p.id || p.name}
                                        className="click"
                                        onClick={() => navigate(`/project/${p.name}`)}
                                    >
                                        <td>
                                            <a
                                                className="r-link"
                                                href={`/project/${p.name}`}
                                                style={{ fontWeight: 500, fontSize: 13.5 }}
                                                onClick={(e) => { e.preventDefault(); navigate(`/project/${p.name}`); }}
                                            >
                                                {p.name}
                                            </a>
                                            {p.primary_url && (
                                                <div className="mono" style={{ fontSize: 11.5, color: 'var(--text-soft)', marginTop: 2, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: 280 }}>
                                                    {p.primary_url}
                                                </div>
                                            )}
                                        </td>
                                        <td><Status status={p.status || 'Unknown'} /></td>
                                        <td style={{ color: 'var(--text-muted)' }}>
                                            {p.owner?.email || p.owner?.name || '—'}
                                        </td>
                                        <td style={{ color: 'var(--text-muted)' }}>{p.access_class || '—'}</td>
                                        <td style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                            {p.updated || p.created ? formatRelativeTimeRounded(p.updated || p.created) : '—'}
                                        </td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    )}
                </Panel>

                <div className="r-stack">
                    <Panel>
                        <PanelHead title="Your teams" />
                        <div style={{ padding: 6 }}>
                            {teams.slice(0, 5).map(t => (
                                <div
                                    key={t.id || t.name}
                                    onClick={() => navigate(`/team/${t.name}`)}
                                    tabIndex={0}
                                    role="button"
                                    aria-label={`Open team ${t.name}`}
                                    onKeyDown={(e) => {
                                        if (e.key === 'Enter' || e.key === ' ') {
                                            e.preventDefault();
                                            navigate(`/team/${t.name}`);
                                        }
                                    }}
                                    style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '8px 12px', borderRadius: 5, cursor: 'pointer' }}
                                    onMouseEnter={e => (e.currentTarget.style.background = 'var(--hover)')}
                                    onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                                >
                                    <Avatar label={t.name} color={colorFor(t.name)} size={28} />
                                    <div style={{ flex: 1, minWidth: 0 }}>
                                        <div style={{ fontWeight: 500, fontSize: 13 }}>{t.name}</div>
                                        <div style={{ color: 'var(--text-soft)', fontSize: 11.5 }}>
                                            {t.members?.length || 0} member{(t.members?.length || 0) === 1 ? '' : 's'}
                                        </div>
                                    </div>
                                </div>
                            ))}
                            {teams.length === 0 && (
                                <div style={{ padding: 16, color: 'var(--text-soft)', fontSize: 13, textAlign: 'center' }}>No teams yet.</div>
                            )}
                        </div>
                    </Panel>

                    <Panel>
                        <PanelHead title="Getting started" />
                        <PanelBody>
                            <p style={{ margin: 0, fontSize: 13, color: 'var(--text-muted)' }}>
                                New to Rise? Follow the{' '}
                                <a className="r-link" href="/docs/user-guide/getting-started/">Getting Started</a>
                                {' '}guide to deploy your first app.
                            </p>
                        </PanelBody>
                    </Panel>
                </div>
            </div>
        </section>
    );
}
