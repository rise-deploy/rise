import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate, useQueryParam } from '../lib/navigation';
import { Button, Combobox, Empty, Panel, SearchInput, Segmented } from '../components/r-ui';
import { ProjectTable, type AccessClassOption, type ProjectRow } from '../components/project-table';
import { LoadingState, ErrorState } from '../components/states';
import { effectiveStatus } from '../lib/project-status';
import { CommandHint } from '../components/command-hint';

export function ProjectsList() {
    const [projects, setProjects] = useState<ProjectRow[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [searchParam, setSearchParam] = useQueryParam('q');
    const search = searchParam || '';
    const setSearch = (v: string) => setSearchParam(v || null);
    const [statusParam, setStatusParam] = useQueryParam('status');
    const statusFilter = statusParam || 'all';
    const setStatusFilter = (v: string) => setStatusParam(v === 'all' ? null : v);
    const [accessFilter, setAccessFilter] = useState('all');
    const [accessClasses, setAccessClasses] = useState<AccessClassOption[]>([]);

    const loadProjects = useCallback(async () => {
        try {
            const data = await api.getProjects();
            setProjects(data);
            setError(null);
        } catch (err) {
            setError(err instanceof Error ? err.message : 'Unable to load projects');
        }
    }, []);

    useEffect(() => { loadProjects(); }, [loadProjects]);

    useEffect(() => {
        api.getAccessClasses().then(d => setAccessClasses(d?.access_classes || [])).catch(() => {});
    }, []);

    const filtered = useMemo(() => {
        if (!projects) return [];
        return projects.filter(p => {
            const s = effectiveStatus(p).toLowerCase();
            // Filter buckets cover every ProjectStatus the backend can emit
            // (Running, Stopped, Deploying, Failed, Deleting, Terminated) so
            // no project is silently hidden by a non-"all" selection.
            const matchesStatus =
                statusFilter === 'all' ||
                (statusFilter === 'healthy' && s === 'running') ||
                (statusFilter === 'deploying' && s === 'deploying') ||
                (statusFilter === 'failed' && (s === 'failed' || s === 'deleting')) ||
                (statusFilter === 'stopped' && (s === 'stopped' || s === 'terminated'));
            if (!matchesStatus) return false;
            if (accessFilter !== 'all' && p.access_class !== accessFilter) return false;
            if (search) {
                const q = search.toLowerCase();
                const haystack = `${p.name} ${p.owner?.email || ''} ${p.owner?.name || ''}`.toLowerCase();
                if (!haystack.includes(q)) return false;
            }
            return true;
        });
    }, [projects, search, statusFilter, accessFilter]);

    if (error) return <ErrorState message={`Failed to load projects: ${error}`} onRetry={loadProjects} />;
    if (!projects) return <LoadingState label="Loading projects…" layout="table" />;

    const owners = new Set(projects.map(p => p.owner?.name ? `team:${p.owner.name}` : null).filter(Boolean));
    const teamCount = owners.size;
    const counts = {
        healthy: projects.filter(p => effectiveStatus(p) === 'Running').length,
        // Failed rollouts (including a failed deploy the previous deployment
        // outlived) and deletes that may be stalled need attention.
        unhealthy: projects.filter(p => {
            const s = effectiveStatus(p).toLowerCase();
            return s === 'failed' || s === 'deleting';
        }).length,
        deploying: projects.filter(p => effectiveStatus(p) === 'Deploying').length,
        // Stopped or fully terminated projects — present but not running.
        inactive: projects.filter(p => {
            const s = effectiveStatus(p).toLowerCase();
            return s === 'stopped' || s === 'terminated';
        }).length,
    };

    return (
        <section className="r-projects-page">
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">Projects</h1>
                    <div className="r-page-sub">
                        {projects.length} project{projects.length === 1 ? '' : 's'} across {teamCount} team{teamCount === 1 ? '' : 's'}
                    </div>
                </div>
                <Button variant="primary" icon="plus" onClick={() => navigate('/projects/new')}>
                    New project
                </Button>
            </div>

            {projects.length === 0 ? (
                <Panel>
                    <Empty title="Your first app starts here">
                        <p>Create a project, then deploy it from your terminal.</p>
                        <Button variant="primary" icon="plus" onClick={() => navigate('/projects/new')}>Create a project</Button>
                        <CommandHint command="rise project create my-app" label="Or use the CLI" />
                    </Empty>
                </Panel>
            ) : (
                <>
                    <div className="r-projects-toolbar">
                        <SearchInput value={search} onChange={setSearch} placeholder="Filter by name or owner" />
                        <Segmented<string>
                            value={statusFilter}
                            options={[
                                { value: 'all', label: `All ${projects.length}` },
                                { value: 'healthy', label: `Healthy ${counts.healthy}` },
                                { value: 'deploying', label: `Deploying ${counts.deploying}` },
                                { value: 'failed', label: `Failed ${counts.unhealthy}` },
                                { value: 'stopped', label: `Stopped ${counts.inactive}` },
                            ]}
                            onChange={setStatusFilter}
                        />
                        {accessClasses.length > 0 && (
                            <div className="r-projects-access">
                                <Combobox
                                    value={accessFilter}
                                    onChange={setAccessFilter}
                                    options={[
                                        { value: 'all', label: 'Access: all' },
                                        ...accessClasses.map(ac => ({ value: ac.id, label: `Access: ${ac.display_name}` })),
                                    ]}
                                    placeholder="Access: all"
                                />
                            </div>
                        )}
                    </div>

                    <ProjectTable
                        projects={filtered}
                        accessClasses={accessClasses}
                        onRowClick={(p) => navigate(`/project/${p.name}`)}
                        emptyText="No projects match these filters."
                    />
                    {filtered.length === 0 && (search || statusFilter !== 'all' || accessFilter !== 'all') && (
                        <div className="r-projects-clear">
                            <button type="button" className="r-link-btn" onClick={() => { setSearch(''); setStatusFilter('all'); setAccessFilter('all'); }}>Clear filters</button>
                        </div>
                    )}
                    <CommandHint />
                </>
            )}
        </section>
    );
}
