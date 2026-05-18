// @ts-nocheck
import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatISO8601, formatRelativeTimeRounded, isSafeUrl } from '../lib/utils';
import { useToast } from '../components/toast';
import { AutocompleteInput, Button, ConfirmDialog, FormField, Modal, ModalActions, ModalSection, MonoNotice, SegmentedRadioGroup } from '../components/ui';
import { ProjectTable } from '../components/project-table';
import { ActiveDeploymentsSummary, DeploymentDetail, DeploymentsList } from './deployments';
import { DomainsList, EnvironmentsList, EnvVarsList, ExtensionDetailPage, ExtensionsList, ServiceAccountsList } from './resources';
import { MonoTable, MonoTableBody, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { useRowKeyboardNavigation, useSortableData } from '../lib/table';


// Projects List Component
export function ProjectsList({ openCreate = false }) {
    const [projects, setProjects] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [formData, setFormData] = useState({ name: '', access_class: 'public', owner: 'self' });
    const [teams, setTeams] = useState([]);
    const [currentUser, setCurrentUser] = useState(null);
    const [accessClasses, setAccessClasses] = useState([]);
    const [saving, setSaving] = useState(false);
    const [actionStatus, setActionStatus] = useState('');
    const { showToast } = useToast();
    const { sortedItems: sortedProjects, sortKey, sortDirection, requestSort } = useSortableData(projects, 'name');
    const { activeIndex, setActiveIndex, onKeyDown } = useRowKeyboardNavigation(
        (idx) => {
            const project = sortedProjects[idx];
            if (project) navigate(`/project/${project.name}`);
        },
        sortedProjects.length
    );

    const loadProjects = useCallback(async () => {
        try {
            const data = await api.getProjects();
            setProjects(data);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        loadProjects();
    }, [loadProjects]);

    useEffect(() => {
        async function loadTeams() {
            try {
                const data = await api.getTeams();
                setTeams(data);
            } catch (err) {
                console.error('Failed to load teams:', err);
            }
        }
        loadTeams();
    }, []);

    useEffect(() => {
        async function loadCurrentUser() {
            try {
                const user = await api.getMe();
                setCurrentUser(user);
            } catch (err) {
                console.error('Failed to load current user:', err);
            }
        }
        loadCurrentUser();
    }, []);

    useEffect(() => {
        async function loadAccessClasses() {
            try {
                const data = await api.getAccessClasses();
                setAccessClasses(data.access_classes || []);
            } catch (err) {
                console.error('Failed to load access classes:', err);
            }
        }
        loadAccessClasses();
    }, []);

    const handleCreateClick = () => {
        setFormData({ name: '', access_class: 'public', owner: 'self' });
        setIsModalOpen(true);
    };

    useEffect(() => {
        if (!openCreate) return;
        handleCreateClick();
        window.history.replaceState({}, '', window.location.pathname);
    }, [openCreate]);

    const handleCreate = async () => {
        if (!formData.name) {
            showToast('Project name is required', 'error');
            return;
        }

        // Validate project name (lowercase alphanumeric and hyphens)
        if (!/^[a-z0-9-]+$/.test(formData.name)) {
            showToast('Project name must contain only lowercase letters, numbers, and hyphens', 'error');
            return;
        }

        if (!currentUser) {
            showToast('Unable to determine current user', 'error');
            return;
        }

        setSaving(true);
        setActionStatus(`Creating project ${formData.name}...`);
        try {
            // Format owner correctly for the API
            let owner;
            if (formData.owner === 'self') {
                owner = { user: currentUser.id };
            } else {
                // formData.owner is the team ID
                owner = { team: formData.owner };
            }

            await api.createProject(formData.name, formData.access_class, owner);
            showToast(`Project ${formData.name} created successfully`, 'success');
            setActionStatus(`Created project ${formData.name}.`);
            setIsModalOpen(false);
            loadProjects();
        } catch (err) {
            showToast(`Failed to create project: ${err.message}`, 'error');
            setActionStatus(`Failed to create project ${formData.name}.`);
        } finally {
            setSaving(false);
        }
    };

    if (loading) return <LoadingState label="Loading projects..." />;
    if (error) return <ErrorState message={`Error loading projects: ${error}`} onRetry={loadProjects} />;

    return (
        <section>
            <div className="flex justify-between items-center mb-6">
                <MonoNotice tone="muted">
                    New to Rise? Follow the{' '}
                    <a href="/docs/user-guide/getting-started/" className="underline">
                        Getting Started
                    </a>{' '}
                    guide to deploy your first app.
                </MonoNotice>
                <Button variant="primary" size="sm" onClick={handleCreateClick} className="ml-4 shrink-0">
                    Create Project
                </Button>
            </div>
            {actionStatus && <p className="mono-inline-status mb-3">{actionStatus}</p>}
            <ProjectTable
                projects={sortedProjects}
                sortKey={sortKey}
                sortDirection={sortDirection}
                requestSort={requestSort}
                onRowClick={(project) => navigate(`/project/${project.name}`)}
                onKeyDown={onKeyDown}
                activeIndex={activeIndex}
                setActiveIndex={setActiveIndex}
                emptyMessage="No projects."
            />

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title="Create Project"
            >
                <ModalSection>
                    <FormField
                        label="Project Name"
                        id="project-name"
                        value={formData.name}
                        onChange={(e) => setFormData({ ...formData, name: e.target.value.toLowerCase() })}
                        placeholder="my-awesome-app"
                        required
                    />
                    <p className="text-sm text-gray-600 dark:text-gray-500 -mt-2">
                        Only lowercase letters, numbers, and hyphens allowed
                    </p>

                    <FormField
                        label="Access Class"
                        id="project-access-class"
                        type="select"
                        value={formData.access_class}
                        onChange={(e) => setFormData({ ...formData, access_class: e.target.value })}
                        required
                    >
                        {accessClasses.map(ac => (
                            <option key={ac.id} value={ac.id} title={ac.description}>
                                {ac.display_name}
                            </option>
                        ))}
                    </FormField>
                    {accessClasses.find(ac => ac.id === formData.access_class) && (
                        <p className="text-sm text-gray-600 dark:text-gray-500 -mt-2">
                            {accessClasses.find(ac => ac.id === formData.access_class).description}
                        </p>
                    )}

                    <FormField
                        label="Owner"
                        id="project-owner"
                        type="select"
                        value={formData.owner}
                        onChange={(e) => setFormData({ ...formData, owner: e.target.value })}
                        required
                    >
                        <option value="self">Self</option>
                        {teams.map(team => (
                            <option key={team.id} value={team.id}>team:{team.name}</option>
                        ))}
                    </FormField>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => setIsModalOpen(false)}
                            disabled={saving}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleCreate}
                            loading={saving}
                        >
                            Create
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>
        </section>
    );
}

// Project Detail Component
export function ProjectDetail({ projectName, initialTab }) {
    const [project, setProject] = useState(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [activeTab, setActiveTab] = useState(initialTab || 'overview');
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const [accessClasses, setAccessClasses] = useState([]);
    const [editingOwner, setEditingOwner] = useState(false);
    const [ownerType, setOwnerType] = useState('user');
    const [ownerUserEmail, setOwnerUserEmail] = useState('');
    const [ownerTeamId, setOwnerTeamId] = useState('');
    const [teams, setTeams] = useState([]);
    const [currentUserEmail, setCurrentUserEmail] = useState('');
    const [updatingOwner, setUpdatingOwner] = useState(false);
    const [detailActionStatus, setDetailActionStatus] = useState('');
    const { showToast } = useToast();
    const handleCopy = useCallback(async (value, label) => {
        if (!value || value === '-') return;

        try {
            await copyToClipboard(value);
            showToast(`${label} copied`, 'success');
        } catch (err) {
            showToast(`Failed to copy ${label.toLowerCase()}: ${err.message}`, 'error');
        }
    }, [showToast]);

    const getStatusTone = (status) => {
        const statusTones = {
            Healthy: 'ok',
            Running: 'ok',
            Deploying: 'warn',
            Pending: 'warn',
            Building: 'warn',
            Pushing: 'warn',
            Pushed: 'warn',
            Unhealthy: 'bad',
            Failed: 'bad',
            Stopped: 'muted',
            Cancelled: 'muted',
            Superseded: 'muted',
            Expired: 'muted',
            Terminating: 'muted',
        };
        return statusTones[status] || 'muted';
    };

    const getOwnerInfo = (projectData) => {
        if (!projectData?.owner) return null;
        const owner = projectData.owner;
        if (owner.email) return { type: 'user', label: owner.email, userId: owner.id || null };
        if (owner.name) return { type: 'team', label: owner.name, teamId: owner.id || null };
        return null;
    };

    const loadProject = useCallback(async () => {
        try {
            const data = await api.getProject(projectName);
            setProject(data);
        } catch (err) {
            setError(err.message);
        } finally {
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadProject();
    }, [loadProject]);

    useEffect(() => {
        async function loadAccessClasses() {
            try {
                const data = await api.getAccessClasses();
                setAccessClasses(data.access_classes || []);
            } catch (err) {
                console.error('Failed to load access classes:', err);
            }
        }
        loadAccessClasses();
    }, []);

    useEffect(() => {
        async function loadTeams() {
            try {
                const data = await api.getTeams();
                setTeams(data || []);
            } catch (err) {
                console.error('Failed to load teams:', err);
            }
        }
        loadTeams();
    }, []);

    useEffect(() => {
        async function loadCurrentUser() {
            try {
                const user = await api.getMe();
                setCurrentUserEmail(user?.email || '');
            } catch (err) {
                console.error('Failed to load current user:', err);
            }
        }
        loadCurrentUser();
    }, []);

    // Update activeTab when initialTab changes (e.g., browser back/forward)
    useEffect(() => {
        if (initialTab) {
            setActiveTab(initialTab);
        }
    }, [initialTab]);

    // Helper function to change tab and update URL
    const changeTab = (tab) => {
        setActiveTab(tab);
        navigate(`/project/${projectName}/${tab}`);
    };

    const handleDeleteClick = () => {
        setConfirmDialogOpen(true);
    };

    const handleDeleteConfirm = async () => {
        if (!project) return;

        setDeleting(true);
        setDetailActionStatus(`Deleting project ${project.name}...`);
        try {
            await api.deleteProject(project.name);
            showToast(`Project ${project.name} deleted successfully`, 'success');
            setDetailActionStatus(`Deleted project ${project.name}.`);
            setConfirmDialogOpen(false);
            // Redirect to projects list
            navigate('/projects');
        } catch (err) {
            showToast(`Failed to delete project: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to delete project ${project.name}.`);
        } finally {
            setDeleting(false);
        }
    };

    const handleEditOwner = () => {
        const currentOwner = getOwnerInfo(project);
        const initialType = currentOwner?.type || 'user';
        setOwnerType(initialType);
        setOwnerUserEmail(initialType === 'user' ? (currentOwner?.label || '') : '');
        if (initialType === 'team' && teams.length > 0) {
            const matchingTeam = teams.find((t) => t.name === currentOwner?.label || t.id === currentOwner?.teamId);
            setOwnerTeamId(matchingTeam?.id || currentOwner?.teamId || '');
        } else {
            setOwnerTeamId('');
        }
        setEditingOwner(true);
    };

    const handleCancelEditOwner = () => {
        setEditingOwner(false);
        setOwnerType('user');
        setOwnerUserEmail('');
        setOwnerTeamId('');
    };

    const handleSaveOwner = async () => {
        if (!project) return;

        if (ownerType === 'user' && !ownerUserEmail.trim()) {
            showToast('User email is required', 'error');
            return;
        }
        if (ownerType === 'team' && !ownerTeamId) {
            showToast('Team is required', 'error');
            return;
        }

        setUpdatingOwner(true);
        setDetailActionStatus(`Transferring ownership for ${project.name}...`);

        try {
            let ownerPayload;
            if (ownerType === 'user') {
                ownerPayload = { user: ownerUserEmail.trim() };
            } else {
                ownerPayload = { team: ownerTeamId };
            }

            await api.updateProject(project.name, { owner: ownerPayload });
            const updatedProject = await api.getProject(projectName);
            setProject(updatedProject);
            showToast('Project owner updated', 'success');
            setDetailActionStatus(`Ownership transferred for ${project.name}.`);
            handleCancelEditOwner();
        } catch (err) {
            showToast(`Failed to update owner: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to transfer ownership for ${project.name}.`);
        } finally {
            setUpdatingOwner(false);
        }
    };

    if (loading) return <LoadingState label="Loading project..." />;
    if (error) return <ErrorState message={`Error loading project: ${error}`} onRetry={loadProject} />;
    if (!project) return <EmptyState message="Project not found." />;

    const ownerInfo = getOwnerInfo(project);
    const appUsers = project.app_users || [];
    const appTeams = project.app_teams || [];
    const owner = project.owner || null;
    const ownerAccessUserEmail = owner?.email ? owner.email.trim().toLowerCase() : null;
    const ownerAccessTeamName = owner?.name ? owner.name.trim().toLowerCase() : null;
    const ownerAccessTeamId = owner?.id || null;

    const userCount = (() => {
        if (!ownerAccessUserEmail) return appUsers.length;
        const ownerAlreadyIncluded = appUsers.some((u) => (u.email || '').trim().toLowerCase() === ownerAccessUserEmail);
        return ownerAlreadyIncluded ? appUsers.length : appUsers.length + 1;
    })();

    const teamCount = (() => {
        if (!ownerAccessTeamName) return appTeams.length;
        const ownerAlreadyIncluded = appTeams.some((t) => {
            if (ownerAccessTeamId && t.id) return t.id === ownerAccessTeamId;
            return (t.name || '').trim().toLowerCase() === ownerAccessTeamName;
        });
        return ownerAlreadyIncluded ? appTeams.length : appTeams.length + 1;
    })();

    return (
        <section>
            <div className="flex justify-end items-start mb-4">
                <Button
                    variant="danger"
                    size="sm"
                    onClick={handleDeleteClick}
                >
                    Delete Project
                </Button>
            </div>

            {detailActionStatus && <p className="mono-inline-status mb-4">{detailActionStatus}</p>}

            <div className="mono-status-strip mono-status-strip-normalcase mb-6">
                <div className={`mono-status-card mono-status-card-${getStatusTone(project.status)}`}>
                    <span>status</span>
                    <strong>{project.status}</strong>
                </div>
                <div>
                    <span>owner</span>
                    <strong className="mono-copyable-value">
                        <span className="inline-flex items-center gap-2">
                            {ownerInfo?.type === 'user' && (
                                <span
                                    className="w-3 h-3 svg-mask inline-block"
                                    aria-hidden="true"
                                    style={{
                                        maskImage: 'url(/assets/user.svg)',
                                        WebkitMaskImage: 'url(/assets/user.svg)',
                                    }}
                                />
                            )}
                            {ownerInfo?.type === 'team' && (
                                <svg className="w-3 h-3" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true">
                                    <path d="M7 10a3 3 0 1 0-3-3 3 3 0 0 0 3 3Zm6 0a3 3 0 1 0-3-3 3 3 0 0 0 3 3ZM1.5 16.5a5.5 5.5 0 0 1 11 0v.5h-11Zm12 0a5.5 5.5 0 0 1 5-5.48 5.53 5.53 0 0 1 .5.02V17h-5.5Z" />
                                </svg>
                            )}
                            {ownerInfo?.type === 'team' ? (
                                <button
                                    type="button"
                                    className="underline"
                                    onClick={() => navigate(`/team/${ownerInfo.label}`)}
                                >
                                    {ownerInfo.label}
                                </button>
                            ) : (
                                <span>{ownerInfo?.label || '-'}</span>
                            )}
                        </span>
                        <button
                            type="button"
                            className="mono-copy-button"
                            title="Transfer ownership"
                            aria-label="Transfer ownership"
                            onClick={handleEditOwner}
                        >
                            <svg className="w-3 h-3" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                                <path strokeLinecap="round" strokeLinejoin="round" d="M12 20h9" />
                                <path strokeLinecap="round" strokeLinejoin="round" d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z" />
                            </svg>
                        </button>
                    </strong>
                </div>
                <div
                    className="cursor-pointer"
                    onClick={() => changeTab('access')}
                    title="Edit access settings"
                >
                    <span>access</span>
                    <strong className="inline-flex items-center gap-3">
                        <span>{accessClasses.find(ac => ac.id === project.access_class)?.display_name || project.access_class}</span>
                        <span className="text-gray-400">|</span>
                        <span className="inline-flex items-center gap-1">
                            <svg className="w-3 h-3" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true">
                                <path d="M7 10a3 3 0 1 0-3-3 3 3 0 0 0 3 3Zm6 0a3 3 0 1 0-3-3 3 3 0 0 0 3 3ZM1.5 16.5a5.5 5.5 0 0 1 11 0v.5h-11Zm12 0a5.5 5.5 0 0 1 5-5.48 5.53 5.53 0 0 1 .5.02V17h-5.5Z" />
                            </svg>
                            {teamCount}
                        </span>
                        <span className="inline-flex items-center gap-1">
                            <span
                                className="w-3 h-3 svg-mask inline-block"
                                aria-hidden="true"
                                style={{
                                    maskImage: 'url(/assets/user.svg)',
                                    WebkitMaskImage: 'url(/assets/user.svg)',
                                }}
                            />
                            {userCount}
                        </span>
                    </strong>
                </div>
                <div>
                    <span>created</span>
                    <strong className="mono-copyable-value" title={formatISO8601(project.created)}>
                        <span>{formatRelativeTimeRounded(project.created)}</span>
                        <button
                            type="button"
                            className="mono-copy-button"
                            title="Copy created timestamp (ISO8601)"
                            aria-label="Copy created timestamp (ISO8601)"
                            onClick={() => handleCopy(formatISO8601(project.created), 'Created timestamp')}
                        >
                            <span
                                className="mono-copy-icon svg-mask"
                                style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                            />
                        </button>
                    </strong>
                </div>
                <div
                    className="cursor-pointer"
                    onClick={() => changeTab('domains')}
                    title="View domains"
                >
                    <span>primary_url</span>
                    <strong className="mono-copyable-value">
                        <span>
                            {project.primary_url ? (
                                <a href={project.primary_url} target="_blank" rel="noopener noreferrer" className="underline"
                                    onClick={(e) => e.stopPropagation()}
                                >
                                    {project.primary_url}
                                </a>
                            ) : '-'}
                        </span>
                        {project.primary_url && (
                            <button
                                type="button"
                                className="mono-copy-button"
                                title="Copy primary URL"
                                aria-label="Copy primary URL"
                                onClick={(e) => { e.stopPropagation(); handleCopy(project.primary_url, 'Primary URL'); }}
                            >
                                <span
                                    className="mono-copy-icon svg-mask"
                                    style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                                />
                            </button>
                        )}
                    </strong>
                </div>
                {project.source_url && (
                    <div>
                        <span>source</span>
                        <strong className="mono-copyable-value">
                            {isSafeUrl(project.source_url) ? (
                                <a href={project.source_url} target="_blank" rel="noopener noreferrer" className="underline">
                                    {project.source_url}
                                </a>
                            ) : (
                                <span>{project.source_url}</span>
                            )}
                            <button
                                type="button"
                                className="mono-copy-button"
                                title="Copy source URL"
                                aria-label="Copy source URL"
                                onClick={() => handleCopy(project.source_url, 'Source URL')}
                            >
                                <span
                                    className="mono-copy-icon svg-mask"
                                    style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                                />
                            </button>
                        </strong>
                    </div>
                )}
            </div>

            <div className="mono-tabbar mb-6">
                <div className="flex flex-wrap gap-2">
                    <button
                        className={`mono-tab-button ${activeTab === 'overview' ? 'active' : ''}`}
                        onClick={() => changeTab('overview')}
                    >
                        Overview
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'environments' ? 'active' : ''}`}
                        onClick={() => changeTab('environments')}
                    >
                        Environments
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'deployments' ? 'active' : ''}`}
                        onClick={() => changeTab('deployments')}
                    >
                        Deployments
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'env-vars' ? 'active' : ''}`}
                        onClick={() => changeTab('env-vars')}
                    >
                        Environment Variables
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'domains' ? 'active' : ''}`}
                        onClick={() => changeTab('domains')}
                    >
                        Domains
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'access' ? 'active' : ''}`}
                        onClick={() => changeTab('access')}
                    >
                        Access
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'service-accounts' ? 'active' : ''}`}
                        onClick={() => changeTab('service-accounts')}
                    >
                        Service Accounts
                    </button>
                    <button
                        className={`mono-tab-button ${activeTab === 'extensions' ? 'active' : ''}`}
                        onClick={() => changeTab('extensions')}
                    >
                        Extensions
                    </button>
                </div>
            </div>

            <div className="mono-tab-panel">
                {activeTab === 'overview' && (
                    <div>
                        <h3 className="text-xl font-bold mb-4">Active Deployments</h3>
                        <ActiveDeploymentsSummary projectName={projectName} />
                    </div>
                )}
                {activeTab === 'deployments' && (
                    <div>
                        <DeploymentsList projectName={projectName} />
                    </div>
                )}
                {activeTab === 'environments' && (
                    <div>
                        <EnvironmentsList projectName={projectName} platformConstraints={project?.platform_constraints} />
                    </div>
                )}
                {activeTab === 'service-accounts' && (
                    <div>
                        <ServiceAccountsList projectName={projectName} />
                    </div>
                )}
                {activeTab === 'env-vars' && (
                    <div>
                        <EnvVarsList projectName={projectName} />
                    </div>
                )}
                {activeTab === 'domains' && (
                    <div>
                        <DomainsList projectName={projectName} defaultUrl={project.default_url} />
                    </div>
                )}
                {activeTab === 'extensions' && (
                    <div>
                        <ExtensionsList projectName={projectName} />
                    </div>
                )}
                {activeTab === 'access' && (
                    <div>
                        <AppUsersList
                            projectName={projectName}
                            project={project}
                            accessClasses={accessClasses}
                            currentUserEmail={currentUserEmail}
                            onProjectUpdated={loadProject}
                        />
                    </div>
                )}
            </div>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => setConfirmDialogOpen(false)}
                onConfirm={handleDeleteConfirm}
                title="Delete Project"
                message={`Delete project "${project.name}"? Impact: this removes associated deployments, service accounts, and environment variables.`}
                confirmText="Delete Project"
                variant="danger"
                requireConfirmation={true}
                confirmationText={project.name}
                loading={deleting}
            />

            <Modal
                isOpen={editingOwner}
                onClose={handleCancelEditOwner}
                title="Transfer Project Ownership"
                maxWidth="max-w-lg"
            >
                <ModalSection>
                    <SegmentedRadioGroup
                        label="Owner Type"
                        name="owner-type"
                        value={ownerType}
                        onChange={setOwnerType}
                        options={[
                            { value: 'user', label: 'USER' },
                            { value: 'team', label: 'TEAM' },
                        ]}
                    />

                    {ownerType === 'user' ? (
                        <div className="form-field">
                            <label htmlFor="project-owner-user-email" className="mono-label">
                                Owner User Email
                                <span className="text-red-300 ml-1">*</span>
                            </label>
                            <AutocompleteInput
                                type="email"
                                id="project-owner-user-email"
                                value={ownerUserEmail}
                                onChange={setOwnerUserEmail}
                                options={currentUserEmail ? [currentUserEmail] : []}
                                placeholder="owner@example.com"
                                onEnter={handleSaveOwner}
                            />
                        </div>
                    ) : (
                        <div>
                            <label className="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
                                Owner Team
                            </label>
                            <Combobox
                                value={ownerTeamId}
                                onChange={setOwnerTeamId}
                                options={teams.map(t => ({ value: t.id, label: t.name }))}
                                placeholder="Search teams..."
                            />
                        </div>
                    )}

                    <ModalActions>
                        <Button variant="secondary" onClick={handleCancelEditOwner} disabled={updatingOwner}>
                            Cancel
                        </Button>
                        <Button variant="primary" onClick={handleSaveOwner} loading={updatingOwner}>
                            Transfer Ownership
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>
        </section>
    );
}

// Combobox component for team selection with autocomplete
function Combobox({ value, onChange, options, placeholder = '', disabled = false, loading = false }) {
    const [isOpen, setIsOpen] = useState(false);
    const [inputText, setInputText] = useState('');
    const ref = useRef(null);

    const selectedOption = options.find(opt => opt.value === value);

    const filteredOptions = options.filter(opt =>
        opt.label.toLowerCase().includes(inputText.toLowerCase())
    );

    useEffect(() => {
        function handleClickOutside(e) {
            if (ref.current && !ref.current.contains(e.target)) {
                setIsOpen(false);
            }
        }
        document.addEventListener('mousedown', handleClickOutside);
        return () => document.removeEventListener('mousedown', handleClickOutside);
    }, []);

    // Sync input text when value changes externally (e.g. selection or reset)
    useEffect(() => {
        if (selectedOption) {
            setInputText(selectedOption.label);
        } else if (!value) {
            setInputText('');
        }
    }, [value, selectedOption]);

    return (
        <div ref={ref} className="relative">
            <input
                type="text"
                className="mono-input w-full"
                placeholder={placeholder}
                value={inputText}
                onChange={(e) => {
                    setInputText(e.target.value);
                    // Clear the selected value when the user types freely
                    if (value) onChange('');
                    if (!isOpen) setIsOpen(true);
                }}
                onFocus={() => {
                    setIsOpen(true);
                }}
                disabled={disabled || loading}
            />
            {isOpen && (
                <div className="absolute z-10 w-full mt-1 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded shadow-lg max-h-48 overflow-y-auto">
                    {loading ? (
                        <div className="px-3 py-2 text-sm text-gray-500">Loading...</div>
                    ) : filteredOptions.length === 0 ? (
                        <div className="px-3 py-2 text-sm text-gray-500">No matches</div>
                    ) : (
                        filteredOptions.map(opt => (
                            <button
                                key={opt.value}
                                type="button"
                                className={`w-full text-left px-3 py-2 text-sm hover:bg-gray-100 dark:hover:bg-gray-700 ${
                                    opt.value === value ? 'bg-gray-100 dark:bg-gray-700' : ''
                                }`}
                                onClick={() => {
                                    onChange(opt.value);
                                    setInputText(opt.label);
                                    setIsOpen(false);
                                }}
                            >
                                {opt.label}
                            </button>
                        ))
                    )}
                </div>
            )}
        </div>
    );
}

// Access tab component - manages access class and app-level user/team access
function AppUsersList({ projectName, project, accessClasses, currentUserEmail, onProjectUpdated }) {
    const [newUserEmail, setNewUserEmail] = useState('');
    const [selectedTeamId, setSelectedTeamId] = useState('');
    const [teams, setTeams] = useState([]);
    const { showToast } = useToast();

    const appUsers = project?.app_users || [];
    const appTeams = project?.app_teams || [];
    const owner = project?.owner || null;

    const ownerUserEmail = owner?.email ? owner.email.trim() : null;
    const ownerTeamName = owner?.name ? owner.name.trim() : null;
    const ownerTeamId = owner?.id || null;

    const displayedUsers = (() => {
        if (!ownerUserEmail) return appUsers;
        const ownerEmailLower = ownerUserEmail.toLowerCase();
        const nonOwnerUsers = appUsers.filter((u) => (u.email || '').toLowerCase() !== ownerEmailLower);
        return [{ id: owner?.id || `owner-user-${ownerUserEmail}`, email: ownerUserEmail, isOwnerFixed: true }, ...nonOwnerUsers];
    })();

    const displayedTeams = (() => {
        if (!ownerTeamName) return appTeams;

        const nonOwnerTeams = appTeams.filter((t) => {
            if (ownerTeamId && t.id) return t.id !== ownerTeamId;
            return (t.name || '').toLowerCase() !== ownerTeamName.toLowerCase();
        });

        return [{ id: ownerTeamId || `owner-team-${ownerTeamName}`, name: ownerTeamName, isOwnerFixed: true }, ...nonOwnerTeams];
    })();

    useEffect(() => {
        async function loadTeams() {
            try {
                const data = await api.getTeams();
                setTeams(data || []);
            } catch (err) {
                console.error('Failed to load teams:', err);
            }
        }
        loadTeams();
    }, []);

    const handleChangeAccessClass = async (newAccessClass) => {
        if (!project || !newAccessClass || newAccessClass === project.access_class) return;

        try {
            await api.updateProject(projectName, { access_class: newAccessClass });
            const ac = accessClasses.find(a => a.id === newAccessClass);
            showToast(`Access class updated to ${ac ? ac.display_name : newAccessClass}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to update access class: ${err.message}`, 'error');
        }
    };

    const handleAddUser = async () => {
        if (!newUserEmail.trim()) {
            showToast('User email is required', 'error');
            return;
        }

        try {
            const currentUserEmails = appUsers.map(u => u.email);
            await api.updateProject(projectName, {
                app_users: [...currentUserEmails, newUserEmail.trim()]
            });
            showToast(`Added app user ${newUserEmail}`, 'success');
            setNewUserEmail('');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app user: ${err.message}`, 'error');
        }
    };

    const handleRemoveUser = async (email) => {
        try {
            const updatedEmails = appUsers.filter(u => u.email !== email).map(u => u.email);
            await api.updateProject(projectName, { app_users: updatedEmails });
            showToast(`Removed app user ${email}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to remove app user: ${err.message}`, 'error');
        }
    };

    const handleAddTeam = async () => {
        if (!selectedTeamId) {
            showToast('Team selection is required', 'error');
            return;
        }

        const selectedTeam = teams.find(t => t.id === selectedTeamId);
        try {
            const currentTeamIds = appTeams.map(t => t.id);
            await api.updateProject(projectName, {
                app_teams: [...currentTeamIds, selectedTeamId]
            });
            showToast(`Added app team ${selectedTeam?.name || ''}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app team: ${err.message}`, 'error');
        }
    };

    const handleRemoveTeam = async (teamId, teamName) => {
        try {
            const updatedTeamIds = appTeams.filter(t => t.id !== teamId).map(t => t.id);
            await api.updateProject(projectName, { app_teams: updatedTeamIds });
            showToast(`Removed app team ${teamName}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to remove app team: ${err.message}`, 'error');
        }
    };

    return (
        <div>
            <div className="mb-6 flex items-center gap-3">
                <SegmentedRadioGroup
                    label="Access Class"
                    name="access-class"
                    value={project?.access_class}
                    onChange={handleChangeAccessClass}
                    options={accessClasses.map(ac => ({ value: ac.id, label: ac.display_name }))}
                />
                {accessClasses.find(ac => ac.id === project?.access_class)?.description && (
                    <span className="text-sm text-gray-400 self-end mb-1">
                        {accessClasses.find(ac => ac.id === project?.access_class).description}
                    </span>
                )}
            </div>
            <p className="text-sm text-gray-500 mb-4">
                The project owner always has access and is shown as a fixed entry.
            </p>

            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                <div>
                    <div className="flex justify-between items-center mb-4">
                        <h4 className="text-lg font-bold">Users ({displayedUsers.length})</h4>
                    </div>
                    {displayedUsers.length > 0 ? (
                        <MonoTableFrame className="mb-4">
                            <MonoTable>
                                <MonoTableHead>
                                    <tr>
                                        <MonoTh className="px-6 py-3 text-left">Email</MonoTh>
                                        <MonoTh className="px-6 py-3 text-right">Actions</MonoTh>
                                    </tr>
                                </MonoTableHead>
                                <MonoTableBody>
                                    {displayedUsers.map(user => (
                                        <MonoTableRow key={user.id} interactive className="transition-colors">
                                            <MonoTd className="px-6 py-4 text-sm text-gray-900 dark:text-gray-200">
                                                <span className="inline-flex items-center gap-2">
                                                    <span>{user.email}</span>
                                                    {user.isOwnerFixed && (
                                                        <span className="px-2 py-0.5 rounded text-xs border border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-300">
                                                            Owner
                                                        </span>
                                                    )}
                                                </span>
                                            </MonoTd>
                                            <MonoTd className="px-6 py-4">
                                                <div className="flex justify-end">
                                                    {user.isOwnerFixed ? (
                                                        <span className="text-xs text-gray-500">Always has access</span>
                                                    ) : (
                                                        <Button
                                                            variant="danger"
                                                            size="sm"
                                                            onClick={() => handleRemoveUser(user.email)}
                                                        >
                                                            Remove
                                                        </Button>
                                                    )}
                                                </div>
                                            </MonoTd>
                                        </MonoTableRow>
                                    ))}
                                </MonoTableBody>
                            </MonoTable>
                        </MonoTableFrame>
                    ) : (
                        <p className="text-gray-600 dark:text-gray-400 mb-4">No users</p>
                    )}
                    <div className="flex justify-end">
                        <div className="flex gap-2">
                            <AutocompleteInput
                                type="email"
                                value={newUserEmail}
                                onChange={setNewUserEmail}
                                options={currentUserEmail ? [currentUserEmail] : []}
                                placeholder="user@example.com"
                                className="w-64"
                                onEnter={handleAddUser}
                            />
                            <Button variant="primary" size="sm" onClick={handleAddUser}>
                                Add
                            </Button>
                        </div>
                    </div>
                </div>

                <div>
                    <div className="flex justify-between items-center mb-4">
                        <h4 className="text-lg font-bold">Teams ({displayedTeams.length})</h4>
                    </div>
                    {displayedTeams.length > 0 ? (
                        <MonoTableFrame className="mb-4">
                            <MonoTable>
                                <MonoTableHead>
                                    <tr>
                                        <MonoTh className="px-6 py-3 text-left">Name</MonoTh>
                                        <MonoTh className="px-6 py-3 text-right">Actions</MonoTh>
                                    </tr>
                                </MonoTableHead>
                                <MonoTableBody>
                                    {displayedTeams.map(team => (
                                        <MonoTableRow key={team.id} interactive className="transition-colors">
                                            <MonoTd className="px-6 py-4 text-sm">
                                                <span className="inline-flex items-center gap-2">
                                                    <button
                                                        type="button"
                                                        className="text-gray-900 dark:text-gray-200 underline"
                                                        onClick={() => navigate(`/team/${team.name}`)}
                                                    >
                                                        {team.name}
                                                    </button>
                                                    {team.isOwnerFixed && (
                                                        <span className="px-2 py-0.5 rounded text-xs border border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-300">
                                                            Owner
                                                        </span>
                                                    )}
                                                </span>
                                            </MonoTd>
                                            <MonoTd className="px-6 py-4">
                                                <div className="flex justify-end">
                                                    {team.isOwnerFixed ? (
                                                        <span className="text-xs text-gray-500">Always has access</span>
                                                    ) : (
                                                        <Button
                                                            variant="danger"
                                                            size="sm"
                                                            onClick={() => handleRemoveTeam(team.id, team.name)}
                                                        >
                                                            Remove
                                                        </Button>
                                                    )}
                                                </div>
                                            </MonoTd>
                                        </MonoTableRow>
                                    ))}
                                </MonoTableBody>
                            </MonoTable>
                        </MonoTableFrame>
                    ) : (
                        <p className="text-gray-600 dark:text-gray-400 mb-4">No teams</p>
                    )}
                    <div className="flex justify-end">
                        <div className="flex gap-2 items-center">
                            <div className="w-64">
                                <Combobox
                                    value={selectedTeamId}
                                    onChange={setSelectedTeamId}
                                    options={teams.map(t => ({ value: t.id, label: t.name }))}
                                    placeholder="Search teams..."
                                    loading={teams.length === 0}
                                />
                            </div>
                            <Button variant="primary" size="sm" onClick={handleAddTeam} disabled={teams.length === 0}>
                                Add
                            </Button>
                        </div>
                    </div>
                </div>
            </div>
        </div>
    );
}
