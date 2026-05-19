// @ts-nocheck
import React, { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatISO8601, formatRelativeTimeRounded, isSafeUrl } from '../lib/utils';
import { useToast } from '../components/toast';
import { Button, ConfirmDialog, KV, KVRow, Modal, Panel, PanelBody, PanelHead, Pill, Status, Tabs, cx } from '../components/r-ui';
import { Icon } from '../components/icon';
import { LoadingState, ErrorState, EmptyState } from '../components/states';

import { ActiveDeploymentsSummary, DeploymentsList } from './deployments';
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

    const tabs = TAB_IDS.map(id => ({ id, label: TAB_LABELS[id] }));

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8, flexWrap: 'wrap' }}>
                        <h1 className="r-page-title">{project.name}</h1>
                        <Status status={project.status || 'Unknown'} />
                        <Pill kind="accent">{accessLabel}</Pill>
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
                                    {project.primary_url}
                                </a>
                                <span className="dot-sep" />
                            </>
                        ) : null}
                        {project.created && (
                            <span title={formatISO8601(project.created)}>created {formatRelativeTimeRounded(project.created)}</span>
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
                    <ProjectOverview project={project} projectName={projectName} accessLabel={accessLabel} onCopy={handleCopy} />
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

function ProjectOverview({ project, projectName, accessLabel, onCopy }: { project: any; projectName: string; accessLabel: string; onCopy: (v: string, l: string) => void }) {
    return (
        <div className="r-grid-2-1">
            <div className="r-stack">
                <Panel>
                    <PanelHead title="Active deployments" sub="Current healthy deployment per group, per environment" />
                    <PanelBody>
                        <ActiveDeploymentsSummary projectName={projectName} />
                    </PanelBody>
                </Panel>
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
                                        {project.primary_url}
                                    </a>
                                </KVRow>
                            )}
                            {project.source_url && isSafeUrl(project.source_url) && (
                                <KVRow k="Source">
                                    <a className="r-link mono" style={{ fontSize: 12.5 }} href={project.source_url} target="_blank" rel="noopener noreferrer">
                                        {project.source_url}
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
            </div>
        </div>
    );
}
