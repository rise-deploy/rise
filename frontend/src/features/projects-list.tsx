// @ts-nocheck
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { useToast } from '../components/toast';
import { Button, Combobox, Field, Input, Modal, Panel, SearchInput, Segmented, Status, Stat, StatGrid, cx } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState, EmptyState } from '../components/states';
import { formatRelativeTimeRounded } from '../lib/utils';

interface Project {
    id?: string;
    name: string;
    status?: string;
    primary_url?: string;
    access_class?: string;
    owner?: { email?: string; name?: string; id?: string };
    created?: string;
    updated_at?: string;
}

export function ProjectsList({ openCreate = false }: { openCreate?: boolean }) {
    const [projects, setProjects] = useState<Project[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [search, setSearch] = useState('');
    const [statusFilter, setStatusFilter] = useState('all');
    const [accessFilter, setAccessFilter] = useState('all');
    const [modalOpen, setModalOpen] = useState(false);
    const [accessClasses, setAccessClasses] = useState<any[]>([]);
    const [teams, setTeams] = useState<any[]>([]);
    const [currentUser, setCurrentUser] = useState<any>(null);
    const [formData, setFormData] = useState({ name: '', access_class: 'public', owner: 'self' });
    const [saving, setSaving] = useState(false);
    const { showToast } = useToast();

    const loadProjects = useCallback(async () => {
        try {
            const data = await api.getProjects();
            setProjects(data);
        } catch (err: any) {
            setError(err.message);
        }
    }, []);

    useEffect(() => { loadProjects(); }, [loadProjects]);

    useEffect(() => {
        api.getTeams().then(setTeams).catch(() => {});
        api.getMe().then(setCurrentUser).catch(() => {});
        api.getAccessClasses().then(d => setAccessClasses(d?.access_classes || [])).catch(() => {});
    }, []);

    useEffect(() => {
        if (openCreate) {
            setFormData({ name: '', access_class: 'public', owner: 'self' });
            setModalOpen(true);
            window.history.replaceState({}, '', window.location.pathname);
        }
    }, [openCreate]);

    const filtered = useMemo(() => {
        if (!projects) return [];
        return projects.filter(p => {
            const s = (p.status || '').toLowerCase();
            const matchesStatus =
                statusFilter === 'all' ||
                (statusFilter === 'healthy' && ['healthy', 'running'].includes(s)) ||
                (statusFilter === 'unhealthy' && ['unhealthy', 'failed'].includes(s)) ||
                (statusFilter === 'deploying' && ['deploying', 'building', 'pushing', 'pending'].includes(s));
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
    if (!projects) return <LoadingState label="Loading projects…" />;

    const handleCreate = async () => {
        if (!formData.name) { showToast('Project name is required', 'error'); return; }
        if (!/^[a-z0-9-]+$/.test(formData.name)) {
            showToast('Project name must contain only lowercase letters, numbers, and hyphens', 'error');
            return;
        }
        if (!currentUser) { showToast('Unable to determine current user', 'error'); return; }
        setSaving(true);
        try {
            const owner = formData.owner === 'self' ? { user: currentUser.id } : { team: formData.owner };
            await api.createProject(formData.name, formData.access_class, owner);
            showToast(`Project ${formData.name} created`, 'success');
            setModalOpen(false);
            loadProjects();
        } catch (err: any) {
            showToast(`Failed to create project: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const counts = {
        healthy: projects.filter(p => ['healthy', 'running'].includes((p.status || '').toLowerCase())).length,
        unhealthy: projects.filter(p => ['unhealthy', 'failed'].includes((p.status || '').toLowerCase())).length,
        deploying: projects.filter(p => ['deploying', 'building', 'pushing', 'pending'].includes((p.status || '').toLowerCase())).length,
    };

    const ownerInfo = (p: Project) => {
        if (p.owner?.email) return { type: 'user' as const, label: p.owner.email };
        if (p.owner?.name) return { type: 'team' as const, label: p.owner.name };
        return null;
    };

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">Projects</h1>
                    <div className="r-page-sub">
                        {projects.length} project{projects.length === 1 ? '' : 's'} · {counts.healthy} healthy
                        {counts.unhealthy ? `, ${counts.unhealthy} need attention` : ''}
                        {counts.deploying ? `, ${counts.deploying} deploying` : ''}
                    </div>
                </div>
                <Button onClick={loadProjects} icon="refresh">Refresh</Button>
                <Button variant="primary" icon="plus" onClick={() => { setFormData({ name: '', access_class: 'public', owner: 'self' }); setModalOpen(true); }}>
                    New project
                </Button>
            </div>

            <StatGrid cols={3}>
                <Stat label="Healthy" value={counts.healthy} unit={` / ${projects.length}`} delta={counts.unhealthy ? `${counts.unhealthy} need attention` : 'all probes passing'} deltaTone={counts.unhealthy ? 'down' : undefined} />
                <Stat label="Deploying" value={counts.deploying} delta={counts.deploying ? 'builds or rollouts in progress' : 'no active deploys'} />
                <Stat label="Access classes" value={accessClasses.length || '—'} delta="configured for this workspace" />
            </StatGrid>

            <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap', marginBottom: 16 }}>
                <SearchInput value={search} onChange={setSearch} placeholder="Filter projects…" style={{ flex: 1, maxWidth: 360 }} />
                <Segmented<string>
                    value={statusFilter}
                    options={[{ value: 'all', label: 'All' }, { value: 'healthy', label: 'Healthy' }, { value: 'deploying', label: 'Deploying' }, { value: 'unhealthy', label: 'Unhealthy' }]}
                    onChange={setStatusFilter}
                />
                {accessClasses.length > 0 && (
                    <div style={{ width: 220 }}>
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

            <Panel>
                {filtered.length === 0 ? (
                    <div style={{ padding: 36, textAlign: 'center', color: 'var(--text-muted)' }}>
                        No projects match your filters.
                    </div>
                ) : (
                    <table className="r-table">
                        <thead>
                            <tr>
                                <th>Project</th>
                                <th>Status</th>
                                <th>Primary URL</th>
                                <th>Owner</th>
                                <th>Access</th>
                                <th style={{ textAlign: 'right' }}>Updated</th>
                            </tr>
                        </thead>
                        <tbody>
                            {filtered.map(p => {
                                const owner = ownerInfo(p);
                                return (
                                    <tr key={p.id || p.name} className="click" onClick={() => navigate(`/project/${p.name}`)}>
                                        <td style={{ maxWidth: 280 }}>
                                            <div style={{ fontWeight: 500, fontSize: 13.5 }}>{p.name}</div>
                                        </td>
                                        <td><Status status={p.status || 'Unknown'} /></td>
                                        <td style={{ maxWidth: 300 }}>
                                            {p.primary_url ? (
                                                <a
                                                    className="r-link mono"
                                                    style={{ fontSize: 12.5, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', display: 'inline-block', maxWidth: '100%' }}
                                                    href={p.primary_url}
                                                    target="_blank"
                                                    rel="noopener noreferrer"
                                                    onClick={e => e.stopPropagation()}
                                                >
                                                    {p.primary_url}
                                                </a>
                                            ) : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                        </td>
                                        <td>
                                            {owner ? (
                                                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
                                                    <Icon name={owner.type === 'team' ? 'users' : 'user'} size={13} />
                                                    {owner.label}
                                                </span>
                                            ) : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                        </td>
                                        <td>
                                            <span className="r-pill">
                                                {accessClasses.find(a => a.id === p.access_class)?.display_name || p.access_class || '—'}
                                            </span>
                                        </td>
                                        <td style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                            {p.updated_at || p.created ? formatRelativeTimeRounded(p.updated_at || p.created) : '—'}
                                        </td>
                                    </tr>
                                );
                            })}
                        </tbody>
                    </table>
                )}
            </Panel>

            <Modal
                isOpen={modalOpen}
                onClose={() => setModalOpen(false)}
                title="Create a new project"
                sub="A project bundles deployments, environments, env vars, domains and access."
                footer={
                    <>
                        <Button onClick={() => setModalOpen(false)} disabled={saving}>Cancel</Button>
                        <Button variant="primary" loading={saving} onClick={handleCreate}>Create project</Button>
                    </>
                }
            >
                <Field label="Project name" hint="Only lowercase letters, numbers, and hyphens.">
                    <Input
                        placeholder="my-service"
                        value={formData.name}
                        onChange={e => setFormData({ ...formData, name: e.target.value.toLowerCase() })}
                        autoFocus
                    />
                </Field>
                <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 14 }}>
                    <Field
                        label="Access class"
                        hint={accessClasses.find(a => a.id === formData.access_class)?.description}
                    >
                        <Combobox
                            value={formData.access_class}
                            onChange={(v) => setFormData({ ...formData, access_class: v })}
                            options={accessClasses.map(ac => ({ value: ac.id, label: ac.display_name, hint: ac.description }))}
                            placeholder="Select access class"
                        />
                    </Field>
                    <Field label="Owner">
                        <Combobox
                            value={formData.owner}
                            onChange={(v) => setFormData({ ...formData, owner: v })}
                            options={[
                                { value: 'self', label: `Self (${currentUser?.email || 'me'})`, keywords: 'me self' },
                                ...teams.map(t => ({ value: t.id, label: `team:${t.name}`, keywords: t.name })),
                            ]}
                            placeholder="Select owner"
                        />
                    </Field>
                </div>
            </Modal>
        </section>
    );
}
