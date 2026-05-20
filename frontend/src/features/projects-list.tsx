// @ts-nocheck
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { useToast } from '../components/toast';
import { Button, Combobox, Field, Input, Modal, SearchInput, Segmented, Stat, StatGrid } from '../components/r-ui';
import { ProjectTable } from '../components/project-table';
import { LoadingState, ErrorState } from '../components/states';

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

            <ProjectTable
                projects={filtered}
                accessClasses={accessClasses}
                onRowClick={(p) => navigate(`/project/${p.name}`)}
                emptyText="No projects match your filters."
            />

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
