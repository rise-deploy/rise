// @ts-nocheck
import { Fragment, useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatDate, formatISO8601, formatRelativeTimeRounded, formatTimeRemaining } from '../lib/utils';
import { useToast } from '../components/toast';
import { Button, ConfirmDialog, ENV_COLOR_STYLES, EnvironmentColorDot, Modal, ModalActions, ModalSection, MonoStatusPill, SourceLinkGroup, SourceLinkGroupAction, StatusBadge } from '../components/ui';
import { MonoSortButton, MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { EnvVarsList } from './resources';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { useRowKeyboardNavigation, useSortableData } from '../lib/table';

const STATUS_TONES = {
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

function getStatusTone(status) {
    return STATUS_TONES[status] || 'muted';
}


export function ActiveDeploymentsSummary({ projectName }) {
    const [activeDeployments, setActiveDeployments] = useState({});
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [deploymentToStop, setDeploymentToStop] = useState(null);
    const [stopping, setStopping] = useState(false);
    const [environments, setEnvironments] = useState([]);
    const { showToast } = useToast();

    const isTerminal = (status) => {
        return ['Cancelled', 'Stopped', 'Superseded', 'Failed', 'Expired'].includes(status);
    };

    const loadSummary = useCallback(async () => {
        try {
            const deployments = await api.getProjectDeployments(projectName, { limit: 100 });

            // Group deployments by deployment group
            const grouped = deployments.reduce((acc, d) => {
                const group = d.deployment_group || 'default';
                if (!acc[group]) {
                    acc[group] = {
                        active: null,
                        progressing: []
                    };
                }

                // Track the active deployment (is_active === true)
                if (d.is_active) {
                    acc[group].active = d;
                }

                // Track progressing (non-terminal) deployments
                if (!isTerminal(d.status)) {
                    acc[group].progressing.push(d);
                }

                return acc;
            }, {});

            // Filter to only include groups that have an active deployment or progressing deployments
            const filtered = {};
            Object.keys(grouped).forEach(group => {
                const groupData = grouped[group];
                // Always include default group if it has an active deployment
                // Include other groups if they have active OR progressing deployments
                if (groupData.active || (group !== 'default' && groupData.progressing.length > 0)) {
                    filtered[group] = groupData;
                }
            });

            setActiveDeployments(filtered);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadSummary();
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
        const interval = setInterval(loadSummary, 5000);
        return () => clearInterval(interval);
    }, [loadSummary]);

    if (loading) return <LoadingState label="Loading active deployments..." />;
    if (error) return <ErrorState message={`Error loading active deployments: ${error}`} onRetry={loadSummary} />;

    const handleStopClick = (deployment) => {
        setDeploymentToStop(deployment);
        setConfirmDialogOpen(true);
    };

    const handleStopConfirm = async () => {
        if (!deploymentToStop) return;

        setStopping(true);
        try {
            await api.stopDeployment(projectName, deploymentToStop.deployment_id);
            showToast(`Deployment ${deploymentToStop.deployment_id} stopped successfully`, 'success');
            setConfirmDialogOpen(false);
            setDeploymentToStop(null);
            loadSummary(); // Refresh the list
        } catch (err) {
            showToast(`Failed to stop deployment: ${err.message}`, 'error');
        } finally {
            setStopping(false);
        }
    };

    const groups = Object.keys(activeDeployments);
    if (groups.length === 0) return <EmptyState message="No active deployments." />;

    // Build environment lookup by name
    const envMap = {};
    for (const env of environments) {
        envMap[env.name] = env;
    }

    // Sort groups: production primary first, then production other, then other env primary, then rest
    const sortedGroups = groups.sort((a, b) => {
        const dA = activeDeployments[a].active;
        const dB = activeDeployments[b].active;
        const envA = dA?.environment ? envMap[dA.environment] : null;
        const envB = dB?.environment ? envMap[dB.environment] : null;
        const prodA = envA?.is_production || false;
        const prodB = envB?.is_production || false;
        const primaryA = envA?.primary_deployment_group === a;
        const primaryB = envB?.primary_deployment_group === b;

        // Production before non-production
        if (prodA !== prodB) return prodA ? -1 : 1;
        // Primary before non-primary
        if (primaryA !== primaryB) return primaryA ? -1 : 1;
        // Both same tier: alphabetical by group name
        return a.localeCompare(b);
    });

    return (
        <>
            <div className="mono-active-deployments-grid grid gap-4 md:grid-cols-2">
                {sortedGroups.map(group => {
                    const groupData = activeDeployments[group];
                    const deployment = groupData.active;

                    // Skip if no active deployment (shouldn't happen due to filtering, but be safe)
                    if (!deployment) {
                        return null;
                    }

                    const canStop = !isTerminal(deployment.status);
                    // Count other progressing deployments (exclude the active one)
                    const otherProgressing = groupData.progressing.filter(d => d.deployment_id !== deployment.deployment_id).length;

                    const envColor = deployment.environment_color
                        ? (ENV_COLOR_STYLES[deployment.environment_color] || ENV_COLOR_STYLES.gray).color
                        : null;

                    return (
                        <div
                            key={group}
                            className={`mono-active-deployment-card mono-status-card mono-status-card-${getStatusTone(deployment.status)} border border-gray-200 dark:border-gray-800 p-6`}
                            style={envColor ? { borderTop: `3px solid ${envColor}` } : undefined}
                            onClick={() => navigate(`/deployment/${projectName}/${deployment.deployment_id}`)}
                            onKeyDown={(e) => {
                                if (e.key === 'Enter' || e.key === ' ') {
                                    e.preventDefault();
                                    navigate(`/deployment/${projectName}/${deployment.deployment_id}`);
                                }
                            }}
                            role="link"
                            tabIndex={0}
                            aria-label={`View deployment ${deployment.deployment_id}`}
                        >
                            <div className="flex justify-between items-center mb-4">
                                <h5 className="text-lg font-semibold">{group}</h5>
                                <div className="flex items-center gap-3">
                                    <StatusBadge status={deployment.status} />
                                    <SourceLinkGroup jobUrl={deployment.job_url} prUrl={deployment.pull_request_url} onClick={(e) => e.stopPropagation()}>
                                        {canStop && (
                                            <SourceLinkGroupAction
                                                variant="danger"
                                                onClick={(e) => { e.stopPropagation(); handleStopClick(deployment); }}
                                            >
                                                Stop
                                            </SourceLinkGroupAction>
                                        )}
                                    </SourceLinkGroup>
                                </div>
                            </div>
                        <dl className="grid grid-cols-2 gap-4 text-sm">
                            <div>
                                <dt className="text-gray-600 dark:text-gray-400">Deployment ID</dt>
                                <dd className="font-mono text-gray-900 dark:text-gray-200">{deployment.deployment_id}</dd>
                            </div>
                            <div>
                                <dt className="text-gray-600 dark:text-gray-400">Image</dt>
                                <dd className="font-mono text-gray-900 dark:text-gray-200 text-xs">{deployment.image ? deployment.image.split('/').pop() : '-'}</dd>
                            </div>
                            <div>
                                <dt className="text-gray-600 dark:text-gray-400">URL</dt>
                                <dd>{deployment.primary_url ? <a href={deployment.primary_url} target="_blank" rel="noopener noreferrer" className="text-indigo-600 dark:text-indigo-400 hover:text-indigo-700 dark:hover:text-indigo-300">{deployment.primary_url}</a> : '-'}</dd>
                            </div>
                            <div>
                                <dt className="text-gray-600 dark:text-gray-400">Created</dt>
                                <dd className="text-gray-900 dark:text-gray-200" title={formatISO8601(deployment.created)}>
                                    {formatRelativeTimeRounded(deployment.created)}
                                </dd>
                            </div>
                            {deployment.environment && (
                                <div>
                                    <dt className="text-gray-600 dark:text-gray-400">Environment</dt>
                                    <dd className="text-gray-900 dark:text-gray-200 flex items-center gap-2"><EnvironmentColorDot color={deployment.environment_color} />{deployment.environment}</dd>
                                </div>
                            )}
                            {deployment.expires_at && (
                                <div>
                                    <dt className="text-gray-600 dark:text-gray-400">Expires</dt>
                                    <dd className="text-gray-900 dark:text-gray-200">
                                        {formatTimeRemaining(deployment.expires_at)}
                                        <span className="text-gray-600 dark:text-gray-500 text-xs ml-2">({formatDate(deployment.expires_at)})</span>
                                    </dd>
                                </div>
                            )}
                        </dl>
                        <div className="mt-4 pt-4 border-t border-gray-200 dark:border-gray-800 flex items-center justify-end">
                            {otherProgressing > 0 && (
                                <span className="text-sm text-gray-600 dark:text-gray-500">
                                    +{otherProgressing} other{otherProgressing === 1 ? '' : 's'} progressing
                                </span>
                            )}
                        </div>
                    </div>
                );
            })}
            </div>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setDeploymentToStop(null);
                }}
                onConfirm={handleStopConfirm}
                title="Stop Deployment"
                message={`Are you sure you want to stop deployment ${deploymentToStop?.deployment_id}? Impact: traffic for group "${deploymentToStop?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop Deployment"
                variant="danger"
                loading={stopping}
            />
        </>
    );
}

// Deployments List Component (with pagination)
export function DeploymentsList({ projectName }) {
    const [deployments, setDeployments] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [page, setPage] = useState(0);
    const [hasMore, setHasMore] = useState(true);
    const [groupFilter, setGroupFilter] = useState('');
    const [deploymentGroups, setDeploymentGroups] = useState([]);
    const [environments, setEnvironments] = useState([]);
    const [envFilter, setEnvFilter] = useState('');
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [deploymentToStop, setDeploymentToStop] = useState(null);
    const [stopping, setStopping] = useState(false);
    const [rollbackDialogOpen, setRollbackDialogOpen] = useState(false);
    const [deploymentToRollback, setDeploymentToRollback] = useState(null);
    const [rollingBack, setRollingBack] = useState(false);
    const [actionStatus, setActionStatus] = useState('');
    const { showToast } = useToast();
    const pageSize = 10;
    const { sortedItems: sortedDeployments, sortKey, sortDirection, requestSort } = useSortableData(deployments, 'created', 'desc');
    const { activeIndex, setActiveIndex, onKeyDown } = useRowKeyboardNavigation(
        (idx) => {
            const deployment = sortedDeployments[idx];
            if (deployment) navigate(`/deployment/${projectName}/${deployment.deployment_id}`);
        },
        sortedDeployments.length
    );

    // Load deployment groups and environments
    useEffect(() => {
        async function loadGroups() {
            try {
                const groups = await api.getDeploymentGroups(projectName);
                setDeploymentGroups(groups);
            } catch (err) {
                console.error('Failed to load deployment groups:', err);
            }
        }
        loadGroups();
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
    }, [projectName]);

    const loadDeployments = useCallback(async () => {
        try {
            const params = {
                limit: pageSize,
                offset: page * pageSize,
            };
            if (groupFilter) params.group = groupFilter;

            const data = await api.getProjectDeployments(projectName, params);
            setDeployments(data);
            setHasMore(data.length >= pageSize);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName, page, groupFilter]);

    useEffect(() => {
        loadDeployments();
        const interval = setInterval(loadDeployments, 5000);
        return () => clearInterval(interval);
    }, [loadDeployments]);

    const handleGroupChange = (e) => {
        setGroupFilter(e.target.value);
        setPage(0);
    };

    const handleEnvFilterChange = (e) => {
        setEnvFilter(e.target.value);
        setPage(0);
    };

    const isTerminal = (status) => {
        return ['Cancelled', 'Stopped', 'Superseded', 'Failed', 'Expired'].includes(status);
    };

    const isRollbackable = (deployment) => {
        return Boolean(deployment?.can_rollback);
    };

    const handleStopClick = (deployment) => {
        setDeploymentToStop(deployment);
        setConfirmDialogOpen(true);
    };

    const handleStopConfirm = async () => {
        if (!deploymentToStop) return;

        setStopping(true);
        setActionStatus(`Stopping deployment ${deploymentToStop.deployment_id}...`);
        try {
            await api.stopDeployment(projectName, deploymentToStop.deployment_id);
            showToast(`Deployment ${deploymentToStop.deployment_id} stopped successfully`, 'success');
            setActionStatus(`Stopped deployment ${deploymentToStop.deployment_id}.`);
            setConfirmDialogOpen(false);
            setDeploymentToStop(null);
            loadDeployments();
        } catch (err) {
            showToast(`Failed to stop deployment: ${err.message}`, 'error');
            setActionStatus(`Failed to stop deployment ${deploymentToStop.deployment_id}.`);
        } finally {
            setStopping(false);
        }
    };

    const handleRollbackClick = (deployment) => {
        setDeploymentToRollback(deployment);
        setRollbackDialogOpen(true);
    };

    const [useSourceEnvVars, setUseSourceEnvVars] = useState(false);

    const handleRollbackConfirm = async () => {
        if (!deploymentToRollback) return;

        setRollingBack(true);
        setActionStatus(`${deploymentToRollback.is_active ? 'Redeploying' : 'Rolling back'} deployment ${deploymentToRollback.deployment_id}...`);
        try {
            const response = await api.createDeploymentFrom(projectName, deploymentToRollback.deployment_id, useSourceEnvVars);
            showToast(`${deploymentToRollback.is_active ? 'Redeploy' : 'Rollback'} successful! New deployment: ${response.deployment_id}`, 'success');
            setActionStatus(`${deploymentToRollback.is_active ? 'Redeployed' : 'Rolled back'} to new deployment ${response.deployment_id}.`);
            setRollbackDialogOpen(false);
            setDeploymentToRollback(null);
            setUseSourceEnvVars(false); // Reset checkbox
            loadDeployments();
        } catch (err) {
            showToast(`Failed to ${deploymentToRollback.is_active ? 'redeploy' : 'rollback'} deployment: ${err.message}`, 'error');
            setActionStatus(`Failed to ${deploymentToRollback.is_active ? 'redeploy' : 'rollback'} deployment ${deploymentToRollback.deployment_id}.`);
        } finally {
            setRollingBack(false);
        }
    };

    if (loading && deployments.length === 0) return <LoadingState label="Loading deployments..." />;
    if (error) return <ErrorState message={`Error loading deployments: ${error}`} onRetry={loadDeployments} />;

    // Client-side environment filter
    const filteredDeployments = envFilter
        ? sortedDeployments.filter(d => d.environment === envFilter)
        : sortedDeployments;

    // Find the most recent deployment in the default group (only non-terminal)
    const mostRecentDefault = filteredDeployments.find(d => d.deployment_group === 'default' && !isTerminal(d.status));

    return (
        <div>
            <div className="mb-4 flex items-center gap-4">
                <label htmlFor="deployment-group-filter" className="flex items-center gap-2">
                    <span className="text-sm text-gray-600 dark:text-gray-400 whitespace-nowrap">Group:</span>
                    <select
                        id="deployment-group-filter"
                        value={groupFilter}
                        onChange={handleGroupChange}
                        className="mono-select text-sm"
                    >
                        <option value="">All groups</option>
                        {deploymentGroups.map(group => (
                            <option key={group} value={group}>{group}</option>
                        ))}
                    </select>
                </label>
                {environments.length > 0 && (
                    <label htmlFor="deployment-env-filter" className="flex items-center gap-2">
                        <span className="text-sm text-gray-600 dark:text-gray-400 whitespace-nowrap">Environment:</span>
                        <select
                            id="deployment-env-filter"
                            value={envFilter}
                            onChange={handleEnvFilterChange}
                            className="mono-select text-sm"
                        >
                            <option value="">All environments</option>
                            {environments.map(env => (
                                <option key={env.name} value={env.name}>{env.name}</option>
                            ))}
                        </select>
                    </label>
                )}
            </div>
            {actionStatus && <p className="mono-inline-status mb-3">{actionStatus}</p>}

            <MonoTableFrame>
                <MonoTable className="mono-sticky-table mono-table--sticky" onKeyDown={onKeyDown}>
                    <MonoTableHead>
                        <tr>
                            <MonoTh stickyCol className="px-6 py-3 text-left">
                                <MonoSortButton label="ID" active={sortKey === 'deployment_id'} direction={sortDirection} onClick={() => requestSort('deployment_id')} />
                            </MonoTh>
                            <MonoTh className="px-6 py-3 text-left">
                                <MonoSortButton label="Status" active={sortKey === 'status'} direction={sortDirection} onClick={() => requestSort('status')} />
                            </MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Created by</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Image</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Group</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Environment</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">URL</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Expires</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">
                                <MonoSortButton label="Created" active={sortKey === 'created'} direction={sortDirection} onClick={() => requestSort('created')} />
                            </MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Actions</MonoTh>
                        </tr>
                    </MonoTableHead>
                    <MonoTableBody>
                        {filteredDeployments.length === 0 ? (
                            <MonoTableEmptyRow colSpan={10}>
                                <EmptyState message="No deployments found." />
                            </MonoTableEmptyRow>
                        ) : (
                            filteredDeployments.map((d, idx) => {
                                    const isHighlighted = mostRecentDefault && d.id === mostRecentDefault.id;
                                    return (
                                    <MonoTableRow
                                        key={d.id}
                                        onClick={() => navigate(`/deployment/${projectName}/${d.deployment_id}`)}
                                        onFocus={() => setActiveIndex(idx)}
                                        tabIndex={0}
                                        aria-label={`Deployment ${d.deployment_id}`}
                                        interactive
                                        active={activeIndex === idx}
                                        highlight={Boolean(isHighlighted)}
                                        className="transition-colors"
                                    >
                                        <MonoTd stickyCol mono className="px-6 py-4 whitespace-nowrap text-sm text-gray-900 dark:text-gray-200">{d.deployment_id}</MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm"><StatusBadge status={d.status} /></MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">{d.created_by_email || '-'}</MonoTd>
                                        <MonoTd mono className="px-6 py-4 whitespace-nowrap text-xs text-gray-700 dark:text-gray-300">{d.image ? d.image.split('/').pop() : '-'}</MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">{d.deployment_group}</MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">{d.environment ? <span className="inline-flex items-center gap-2"><EnvironmentColorDot color={d.environment_color} />{d.environment}</span> : '-'}</MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm">
                                            {d.primary_url ? (
                                                <a
                                                    href={d.primary_url}
                                                    target="_blank"
                                                    rel="noopener noreferrer"
                                                    className="text-indigo-600 dark:text-indigo-400 hover:text-indigo-700 dark:hover:text-indigo-300"
                                                    onClick={(e) => e.stopPropagation()}
                                                >
                                                    Link
                                                </a>
                                            ) : '-'}
                                        </MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">
                                            {d.expires_at ? (
                                                <span>
                                                    {formatTimeRemaining(d.expires_at)}
                                                    <br />
                                                    <span className="text-gray-600 dark:text-gray-500 text-xs">({formatDate(d.expires_at)})</span>
                                                </span>
                                            ) : '-'}
                                        </MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300" title={formatISO8601(d.created)}>
                                            {formatRelativeTimeRounded(d.created)}
                                        </MonoTd>
                                        <MonoTd className="px-6 py-4 whitespace-nowrap text-sm">
                                            <div className="mono-table-action-slot">
                                                {isRollbackable(d) && (
                                                    <Button
                                                        variant="primary"
                                                        size="sm"
                                                        onClick={(e) => {
                                                            e.stopPropagation();
                                                            handleRollbackClick(d);
                                                        }}
                                                    >
                                                        {d.is_active ? 'Redeploy' : 'Rollback'}
                                                    </Button>
                                                )}
                                                {!isTerminal(d.status) && (
                                                    <Button
                                                        variant="danger"
                                                        size="sm"
                                                        onClick={(e) => {
                                                            e.stopPropagation();
                                                            handleStopClick(d);
                                                        }}
                                                    >
                                                        Stop
                                                    </Button>
                                                )}
                                            </div>
                                        </MonoTd>
                                    </MonoTableRow>
                                    );
                                })
                        )}
                    </MonoTableBody>
                </MonoTable>
            </MonoTableFrame>

            <div className="mt-4 flex justify-between items-center">
                <button
                    onClick={() => setPage(p => p - 1)}
                    disabled={page === 0}
                    className="bg-gray-700 hover:bg-gray-600 disabled:bg-gray-100 dark:bg-gray-800 disabled:text-gray-600 text-white px-4 py-2 rounded text-sm transition-colors"
                >
                    Previous
                </button>
                <span className="text-sm text-gray-600 dark:text-gray-400">
                    Page {page + 1} (showing {deployments.length} deployments)
                </span>
                <button
                    onClick={() => setPage(p => p + 1)}
                    disabled={!hasMore}
                    className="bg-gray-700 hover:bg-gray-600 disabled:bg-gray-100 dark:bg-gray-800 disabled:text-gray-600 text-white px-4 py-2 rounded text-sm transition-colors"
                >
                    Next
                </button>
            </div>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setDeploymentToStop(null);
                }}
                onConfirm={handleStopConfirm}
                title="Stop Deployment"
                message={`Are you sure you want to stop deployment ${deploymentToStop?.deployment_id}? Impact: traffic for group "${deploymentToStop?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop Deployment"
                variant="danger"
                loading={stopping}
            />

            <Modal
                isOpen={rollbackDialogOpen}
                onClose={() => {
                    setRollbackDialogOpen(false);
                    setDeploymentToRollback(null);
                    setUseSourceEnvVars(false);
                }}
                title={deploymentToRollback?.is_active ? 'Redeploy' : 'Rollback to Deployment'}
            >
                <ModalSection>
                    <p className="text-gray-700 dark:text-gray-300">
                        {deploymentToRollback?.is_active
                            ? `Are you sure you want to redeploy ${deploymentToRollback?.deployment_id}? This will create a new deployment with the same image.`
                            : `Are you sure you want to rollback to deployment ${deploymentToRollback?.deployment_id}? This will create a new deployment with the same image.`}
                    </p>
                    
                    <div className="bg-gray-50 dark:bg-gray-800 p-4 rounded-lg">
                        <label className="flex items-start gap-3 cursor-pointer">
                            <input
                                type="checkbox"
                                checked={useSourceEnvVars}
                                onChange={(e) => setUseSourceEnvVars(e.target.checked)}
                                className="mt-1 w-4 h-4 text-indigo-600 border-gray-300 rounded focus:ring-indigo-500"
                            />
                            <div className="flex-1">
                                <div className="text-sm font-medium text-gray-900 dark:text-gray-100">
                                    Use source deployment's environment variables
                                </div>
                                <div className="text-xs text-gray-600 dark:text-gray-400 mt-1">
                                    {useSourceEnvVars 
                                        ? "Will copy environment variables from the source deployment" 
                                        : "Will use the current project's environment variables (default)"}
                                </div>
                            </div>
                        </label>
                    </div>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => {
                                setRollbackDialogOpen(false);
                                setDeploymentToRollback(null);
                                setUseSourceEnvVars(false);
                            }}
                            disabled={rollingBack}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleRollbackConfirm}
                            loading={rollingBack}
                            disabled={rollingBack}
                        >
                            {deploymentToRollback?.is_active ? 'Redeploy' : 'Rollback'}
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>
        </div>
    );
}

// Deployment Logs Component with SSE streaming
function DeploymentLogs({ projectName, deploymentId, deploymentStatus }) {
    const [logs, setLogs] = useState([]);
    const [streaming, setStreaming] = useState(false);
    const [error, setError] = useState(null);
    const [autoScroll, setAutoScroll] = useState(true);
    const [tailLines, setTailLines] = useState(1000);
    const [tailInputValue, setTailInputValue] = useState('1000');
    const logsEndRef = useRef(null);
    const abortControllerRef = useRef(null);

    const isLoggable = (status) => {
        // Can view logs for deployments that are running or have run
        return ['Deploying', 'Healthy', 'Unhealthy', 'Stopped', 'Failed', 'Superseded'].includes(status);
    };

    const scrollToBottom = () => {
        if (autoScroll && logsEndRef.current) {
            logsEndRef.current.scrollIntoView({ behavior: 'smooth' });
        }
    };

    useEffect(() => {
        scrollToBottom();
    }, [logs]);

    const startStreaming = useCallback(() => {
        // Stop any existing stream first
        if (abortControllerRef.current) {
            abortControllerRef.current.abort();
            abortControllerRef.current = null;
        }

        // Clear existing logs when starting a new stream
        setLogs([]);
        setError(null);
        setStreaming(true);

        const baseUrl = window.API_BASE_URL || '';
        const url = `${baseUrl}/api/v1/projects/${projectName}/deployments/${deploymentId}/logs?follow=true&tail=${tailLines}`;

        // Create new AbortController for this stream
        const abortController = new AbortController();
        abortControllerRef.current = abortController;

        // Use fetch for SSE with cookies
        fetch(url, {
            headers: {
                'Accept': 'text/event-stream',
            },
            credentials: 'include',  // Include cookies (rise_jwt)
            signal: abortController.signal,
        })
        .then(response => {
            if (!response.ok) {
                throw new Error(`HTTP ${response.status}: ${response.statusText}`);
            }

            const reader = response.body.getReader();
            const decoder = new TextDecoder();
            let buffer = '';

            const processStream = () => {
                reader.read().then(({ done, value }) => {
                    if (done) {
                        setStreaming(false);
                        return;
                    }

                    buffer += decoder.decode(value, { stream: true });
                    const lines = buffer.split('\n');
                    buffer = lines.pop(); // Keep incomplete line in buffer

                    lines.forEach(line => {
                        if (line.startsWith('data: ')) {
                            const logLine = line.substring(6); // Remove 'data: ' prefix
                            if (logLine.trim()) {
                                setLogs(prevLogs => [...prevLogs, logLine]);
                            }
                        }
                    });

                    processStream();
                }).catch(err => {
                    // Ignore abort errors
                    if (err.name === 'AbortError') {
                        return;
                    }
                    console.error('Stream error:', err);
                    setError(err.message);
                    setStreaming(false);
                });
            };

            processStream();
        })
        .catch(err => {
            // Ignore abort errors
            if (err.name === 'AbortError') {
                return;
            }
            console.error('Failed to start log stream:', err);
            setError(err.message);
            setStreaming(false);
        });
    }, [projectName, deploymentId, tailLines]);

    const stopStreaming = useCallback(() => {
        if (abortControllerRef.current) {
            abortControllerRef.current.abort();
            abortControllerRef.current = null;
        }
        setStreaming(false);
    }, []);

    const loadInitialLogs = useCallback(async () => {
        const baseUrl = window.API_BASE_URL || '';
        const url = `${baseUrl}/api/v1/projects/${projectName}/deployments/${deploymentId}/logs?tail=${tailLines}`;

        try {
            const response = await fetch(url, {
                headers: {
                    'Accept': 'text/event-stream',
                },
                credentials: 'include',  // Include cookies (rise_jwt)
            });

            if (!response.ok) {
                throw new Error(`HTTP ${response.status}: ${response.statusText}`);
            }

            const reader = response.body.getReader();
            const decoder = new TextDecoder();
            let buffer = '';
            const newLogs = [];

            while (true) {
                const { done, value } = await reader.read();
                if (done) break;

                buffer += decoder.decode(value, { stream: true });
                const lines = buffer.split('\n');
                buffer = lines.pop();

                lines.forEach(line => {
                    if (line.startsWith('data: ')) {
                        const logLine = line.substring(6);
                        if (logLine.trim()) {
                            newLogs.push(logLine);
                        }
                    }
                });
            }

            setLogs(newLogs);
        } catch (err) {
            console.error('Failed to load logs:', err);
            setError(err.message);
        }
    }, [projectName, deploymentId, tailLines]);

    const clearLogs = () => {
        setLogs([]);
    };

    const handleTailLinesChange = (e) => {
        setTailInputValue(e.target.value);
    };

    const handleTailLinesBlur = () => {
        const newTail = parseInt(tailInputValue, 10);
        if (!isNaN(newTail) && newTail > 0) {
            setTailLines(newTail);
        } else {
            // Reset to current value if invalid
            setTailInputValue(tailLines.toString());
        }
    };

    const handleTailLinesKeyPress = (e) => {
        if (e.key === 'Enter') {
            e.target.blur(); // Trigger blur which will handle the update
        }
    };

    // Effect to restart streaming when tailLines changes and we're currently streaming
    useEffect(() => {
        if (streaming) {
            console.log('Tail lines changed to', tailLines, ', restarting stream...');
            startStreaming();
        }
    }, [tailLines]); // Only depend on tailLines, not streaming or startStreaming to avoid loops

    useEffect(() => {
        return () => {
            stopStreaming();
        };
    }, [stopStreaming]);

    if (!isLoggable(deploymentStatus)) {
        return null;
    }

    return (
        <div className="mb-6">
            <div className="flex justify-between items-center mb-3">
                <h3 className="text-xl font-bold">Runtime Logs</h3>
                <div className="flex gap-2 items-center">
                    <label className="flex items-center gap-2 text-sm text-gray-600 dark:text-gray-400">
                        <span>Tail lines:</span>
                        <input
                            type="number"
                            value={tailInputValue}
                            onChange={handleTailLinesChange}
                            onBlur={handleTailLinesBlur}
                            onKeyPress={handleTailLinesKeyPress}
                            min="1"
                            className="w-20 bg-gray-100 dark:bg-gray-800 border border-gray-600 rounded px-2 py-1 text-sm text-gray-900 dark:text-gray-100 focus:outline-none focus:border-indigo-500"
                        />
                    </label>
                    <label className="flex items-center gap-2 text-sm text-gray-600 dark:text-gray-400">
                        <input
                            type="checkbox"
                            checked={autoScroll}
                            onChange={(e) => setAutoScroll(e.target.checked)}
                            className="rounded border-gray-600 bg-gray-100 dark:bg-gray-800 text-indigo-600 focus:ring-indigo-500"
                        />
                        Auto-scroll
                    </label>
                    <Button
                        variant="secondary"
                        size="sm"
                        onClick={clearLogs}
                        disabled={logs.length === 0}
                    >
                        Clear
                    </Button>
                    {!streaming ? (
                        <>
                            <Button
                                variant="secondary"
                                size="sm"
                                onClick={loadInitialLogs}
                            >
                                Load Logs
                            </Button>
                            <Button
                                variant="primary"
                                size="sm"
                                onClick={startStreaming}
                            >
                                Follow Logs
                            </Button>
                        </>
                    ) : (
                        <Button
                            variant="secondary"
                            size="sm"
                            onClick={stopStreaming}
                        >
                            Stop
                        </Button>
                    )}
                </div>
            </div>

            {error && (
                <div className="mb-3 p-3 bg-red-900/20 border border-red-800 rounded text-red-600 dark:text-red-400 text-sm">
                    Error: {error}
                </div>
            )}

            <div className="bg-gray-950 border border-gray-200 dark:border-gray-800 rounded-lg overflow-hidden">
                <div
                    className="p-4 overflow-y-auto font-mono text-xs text-gray-700 dark:text-gray-300"
                    style={{ height: '400px' }}
                >
                    {logs.length === 0 ? (
                        <div className="text-gray-600 dark:text-gray-500 text-center py-8">
                            {streaming ? 'Waiting for logs...' : 'No logs yet. Click "Load Logs" or "Follow Logs" to view.'}
                        </div>
                    ) : (
                        <>
                            {logs.map((log, idx) => (
                                <div key={idx} className="whitespace-pre-wrap break-all">
                                    {log}
                                </div>
                            ))}
                            <div ref={logsEndRef} />
                        </>
                    )}
                </div>
            </div>

            {streaming && (
                <div className="mt-2 flex items-center gap-2 text-sm text-gray-600 dark:text-gray-400">
                    <div className="w-2 h-2 bg-green-500 rounded-full animate-pulse"></div>
                    Live streaming logs...
                </div>
            )}
        </div>
    );
}

function getPhaseForEvent(event: string) {
    const e = event.toLowerCase();
    if (e.includes('build')) return 'build';
    if (e.includes('push') || e.includes('image')) return 'push';
    if (e.includes('rollout') || e.includes('deploy')) return 'rollout';
    if (e.includes('health') || e.includes('ready') || e.includes('active')) return 'health';
    return 'other';
}

function formatDurationDelta(fromTs?: string | null, toTs?: string | null) {
    if (!fromTs || !toTs) return '--';
    const from = new Date(fromTs).getTime();
    const to = new Date(toTs).getTime();
    if (Number.isNaN(from) || Number.isNaN(to) || to < from) return '--';
    const seconds = Math.floor((to - from) / 1000);
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    const rem = seconds % 60;
    if (minutes < 60) return `${minutes}m ${rem}s`;
    const hours = Math.floor(minutes / 60);
    const mins = minutes % 60;
    return `${hours}h ${mins}m`;
}

function buildDeploymentTimeline(deployment: any) {
    const events: Array<{ label: string; ts: string | null; phase: string }> = [
        { label: 'Deployment requested', ts: deployment.created || null, phase: 'build' },
        { label: 'Image prepared', ts: deployment.created || null, phase: 'push' },
        { label: 'Rollout started', ts: deployment.created || null, phase: 'rollout' },
    ];

    if (deployment.completed_at) {
        events.push({
            label: deployment.status === 'Failed' ? 'Deployment failed' : 'Deployment completed',
            ts: deployment.completed_at,
            phase: deployment.status === 'Failed' ? 'rollout' : 'health',
        });
    }

    const healthLastCheck = deployment.controller_metadata?.health?.last_check || null;
    if (healthLastCheck) {
        events.push({
            label: deployment.controller_metadata?.health?.healthy ? 'Health check healthy' : 'Health check degraded',
            ts: healthLastCheck,
            phase: 'health',
        });
    }

    const statusEventTime = deployment.completed_at || deployment.created || null;
    events.push({
        label: `Current status: ${deployment.status}`,
        ts: statusEventTime,
        phase: getPhaseForEvent(deployment.status || ''),
    });

    const sorted = events
        .filter((e) => e.ts)
        .sort((a, b) => new Date(a.ts || '').getTime() - new Date(b.ts || '').getTime());

    return sorted.map((event, index) => {
        const prev = index > 0 ? sorted[index - 1] : null;
        return {
            ...event,
            delta: prev ? formatDurationDelta(prev.ts, event.ts) : '--',
        };
    });
}

// TypeScript interfaces matching Rust backend structs
interface PodEvent {
    type: string;
    reason: string;
    message: string;
    count: number;
    last_timestamp: string;
}

interface ContainerState {
    state_type: 'waiting' | 'running' | 'terminated';
    reason?: string;
    message?: string;
    exit_code?: number;
}

interface ContainerStatusInfo {
    name: string;
    ready: boolean;
    restart_count: number;
    state?: ContainerState;
    last_state?: ContainerState;
}

interface PodCondition {
    type: string;
    status: string;
    reason?: string;
    message?: string;
}

interface PodInfo {
    name: string;
    phase: 'Pending' | 'Running' | 'Succeeded' | 'Failed' | 'Unknown';
    terminating?: boolean;
    terminated?: boolean;
    conditions?: PodCondition[];
    containers?: ContainerStatusInfo[];
    events?: PodEvent[];
}

interface PodStatus {
    desired_replicas: number;
    ready_replicas: number;
    current_replicas: number;
    pods: PodInfo[];
    last_checked: string;
}

/** Returns the notable reason from a container's last_state (e.g. "OOMKilled"), or undefined. */
function getLastStateReason(container: ContainerStatusInfo): string | undefined {
    if (container.last_state?.state_type === 'terminated') {
        return container.last_state.reason ?? undefined;
    }
    return undefined;
}

function PodInfoRow({ pod }: { pod: PodInfo }) {
    const [expanded, setExpanded] = useState(false);
    const detailsId = `pod-details-${pod.name.replace(/[^a-zA-Z0-9_-]/g, '-')}`;

    const isGone = pod.terminating || pod.terminated;

    // Collect unique notable last_state reasons across all containers (e.g. OOMKilled)
    const lastStateReasons: string[] = [];
    if (!isGone && pod.containers) {
        for (const c of pod.containers) {
            const reason = getLastStateReason(c);
            if (reason && !lastStateReasons.includes(reason)) {
                lastStateReasons.push(reason);
            }
        }
    }

    // Terminating/terminated pods: suppress issue indicators (not-ready is expected)
    const hasIssues = !isGone && (
                      pod.events?.length > 0 ||
                      pod.containers?.some(c =>
                          !c.ready ||
                          c.restart_count > 0 ||
                          (c.last_state?.state_type === 'terminated' && (c.last_state.reason || (c.last_state.exit_code !== undefined && c.last_state.exit_code !== 0)))
                      ) ||
                      pod.conditions?.some(c => c.status === 'False'));

    const phaseTone = {
        Running: '#b7ffce',
        Pending: '#ffe3a8',
        Failed: '#ffc0c0',
        Succeeded: '#b7ffce',
        Unknown: '#888',
    };

    const displayPhase = pod.terminated ? 'Terminated' : pod.terminating ? 'Terminating' : pod.phase;
    const displayColor = isGone ? '#888' : (phaseTone[pod.phase] || '#888');

    // Use appropriate border color based on issues
    const borderColor = hasIssues ? '#7d4b4b' : 'var(--mono-line)';

    return (
        <div className="border-b" style={{ borderColor, opacity: isGone ? 0.5 : 1 }}>
            <button
                type="button"
                className="w-full p-3 text-left cursor-pointer"
                onClick={() => setExpanded(!expanded)}
                aria-expanded={expanded}
                aria-controls={detailsId}
            >
                <div className="flex items-center justify-between gap-2">
                    <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2 mb-1">
                            <span className="text-xs font-mono font-semibold" style={{ color: '#e8e8e8' }}>
                                {pod.name}
                            </span>
                            <span className="text-xs px-1.5 py-0.5" style={{
                                color: displayColor,
                                border: `1px solid ${displayColor}`,
                                background: 'rgba(0, 0, 0, 0.3)'
                            }}>
                                {displayPhase}
                            </span>
                            {hasIssues && (
                                <span className="text-xs" style={{ color: '#ffc0c0' }}>
                                    ⚠
                                </span>
                            )}
                            {lastStateReasons.map((reason) => (
                                <MonoStatusPill key={reason} tone="bad" uppercase={false}>
                                    {reason}
                                </MonoStatusPill>
                            ))}
                        </div>
                        <div className="text-xs" style={{ color: 'var(--mono-muted)' }}>
                            {pod.containers?.length || 0} container(s) •{' '}
                            {pod.containers?.filter(c => c.ready).length || 0} ready
                        </div>
                    </div>
                    <span className="text-xs" style={{ color: 'var(--mono-muted)' }} aria-hidden="true">
                        {expanded ? '▼' : '▶'}
                    </span>
                </div>
            </button>

            {expanded && (
                <div id={detailsId} className="p-3 pt-0" style={{ background: '#0a0a0a' }}>
                    {/* Container statuses */}
                    {pod.containers && pod.containers.length > 0 && (
                        <div className="mb-3">
                            <h6 className="text-xs font-semibold mb-2" style={{ color: 'var(--mono-muted)' }}>
                                Containers
                            </h6>
                            <div className="space-y-2">
                                {pod.containers.map((container, idx) => {
                                    const lastStateReason = getLastStateReason(container);
                                    const hasLastStateIssue = lastStateReason != null || (
                                        container.last_state?.state_type === 'terminated' &&
                                        container.last_state.exit_code !== undefined &&
                                        container.last_state.exit_code !== 0
                                    );
                                    const containerBorderColor = hasLastStateIssue ? '#7d4b4b' : 'var(--mono-line)';
                                    return (
                                        <div key={idx} className="text-xs p-2" style={{ background: '#0f0f0f', border: `1px solid ${containerBorderColor}` }}>
                                            <div className="flex items-center justify-between mb-1">
                                                <div className="flex items-center gap-2 flex-wrap">
                                                    <span className="font-mono" style={{ color: '#e8e8e8' }}>{container.name}</span>
                                                    {lastStateReason && (
                                                        <MonoStatusPill tone="bad" uppercase={false}>
                                                            {lastStateReason}
                                                        </MonoStatusPill>
                                                    )}
                                                </div>
                                                <span style={{ color: container.ready ? '#b7ffce' : '#ffc0c0' }}>
                                                    {container.ready ? '✓ Ready' : '✗ Not ready'}
                                                </span>
                                            </div>
                                            {container.restart_count > 0 && (
                                                <div style={{ color: 'var(--mono-warn)' }}>
                                                    Restarts: {container.restart_count}
                                                </div>
                                            )}
                                            {container.state && (
                                                <div style={{ color: 'var(--mono-muted)' }}>
                                                    State: {container.state.state_type}
                                                    {container.state.reason && ` (${container.state.reason})`}
                                                </div>
                                            )}
                                            {container.state?.message && (
                                                <div className="mt-1 font-mono" style={{ color: '#ffc0c0' }}>
                                                    {container.state.message}
                                                </div>
                                            )}
                                            {container.state?.exit_code !== undefined && (
                                                <div style={{ color: 'var(--mono-muted)' }}>
                                                    Exit code: {container.state.exit_code}
                                                </div>
                                            )}
                                            {container.last_state && (
                                                <div className="mt-2 pt-2" style={{ borderTop: '1px solid var(--mono-line)' }}>
                                                    <div className="font-semibold mb-1" style={{ color: 'var(--mono-muted)' }}>
                                                        Last state
                                                    </div>
                                                    <div style={{ color: hasLastStateIssue ? '#ffc0c0' : 'var(--mono-muted)' }}>
                                                        {container.last_state.state_type}
                                                        {container.last_state.reason && ` (${container.last_state.reason})`}
                                                    </div>
                                                    {container.last_state.exit_code !== undefined && (
                                                        <div style={{ color: 'var(--mono-muted)' }}>
                                                            Exit code: {container.last_state.exit_code}
                                                        </div>
                                                    )}
                                                    {container.last_state.message && (
                                                        <div className="font-mono" style={{ color: '#ffc0c0' }}>
                                                            {container.last_state.message}
                                                        </div>
                                                    )}
                                                </div>
                                            )}
                                        </div>
                                    );
                                })}
                            </div>
                        </div>
                    )}

                    {/* Pod conditions */}
                    {pod.conditions && pod.conditions.length > 0 && (
                        <div className="mb-3">
                            <h6 className="text-xs font-semibold mb-2" style={{ color: 'var(--mono-muted)' }}>
                                Conditions
                            </h6>
                            <div className="space-y-1">
                                {pod.conditions.map((condition, idx) => (
                                    <div key={idx} className="text-xs flex items-center justify-between">
                                        <span style={{ color: '#e8e8e8' }}>{condition.type}</span>
                                        <span style={{ color: condition.status === 'True' ? '#b7ffce' : '#ffc0c0' }}>
                                            {condition.status}
                                        </span>
                                    </div>
                                ))}
                            </div>
                        </div>
                    )}

                    {/* Recent events */}
                    {pod.events && pod.events.length > 0 && (
                        <div>
                            <h6 className="text-xs font-semibold mb-2" style={{ color: 'var(--mono-muted)' }}>
                                Recent Events
                            </h6>
                            <div className="space-y-2">
                                {pod.events.map((event, idx) => (
                                    <div key={idx} className="text-xs p-2" style={{
                                        background: event.type === 'Error' ? 'rgba(125, 75, 75, 0.24)' : 'rgba(139, 112, 57, 0.22)',
                                        border: `1px solid ${event.type === 'Error' ? '#7d4b4b' : '#7b6333'}`
                                    }}>
                                        <div className="flex items-center justify-between mb-1">
                                            <span className="font-semibold" style={{ color: event.type === 'Error' ? '#ffc0c0' : 'var(--mono-warn)' }}>
                                                {event.reason}
                                            </span>
                                            <span style={{ color: 'var(--mono-muted)' }}>
                                                {event.count > 1 && `${event.count}× `}
                                                {formatRelativeTimeRounded(event.last_timestamp)}
                                            </span>
                                        </div>
                                        <div className="font-mono" style={{ color: '#e8e8e8' }}>
                                            {event.message}
                                        </div>
                                    </div>
                                ))}
                            </div>
                        </div>
                    )}
                </div>
            )}
        </div>
    );
}

function PodStatusSection({ podStatus }: { podStatus: PodStatus }) {
    const activePods = podStatus.pods?.filter(p => !p.terminating && !p.terminated) || [];
    const inactivePods = podStatus.pods?.filter(p => p.terminating || p.terminated) || [];

    const replicasMismatch = podStatus.ready_replicas < podStatus.desired_replicas;
    const hasPodIssues =
        activePods.some(
            (p) =>
                (p.containers && p.containers.some(c => c.restart_count > 0)) ||
                (p.events && p.events.length > 0)
        );

    const hasIssues = replicasMismatch || hasPodIssues;

    // Determine tone based on replica counts and pod-level issues
    let tone = 'ok';
    if (podStatus.ready_replicas === 0) {
        tone = 'bad';
    } else if (replicasMismatch || hasPodIssues) {
        tone = 'warn';
    }

    const toneColors = {
        ok: { color: '#b7ffce', borderColor: '#2e6c44', background: 'rgba(44, 105, 66, 0.2)' },
        warn: { color: '#ffe3a8', borderColor: '#7b6333', background: 'rgba(139, 112, 57, 0.22)' },
        bad: { color: '#ffc0c0', borderColor: '#7d4b4b', background: 'rgba(125, 75, 75, 0.24)' },
    };

    const borderColors = {
        ok: '#2e6c44',
        warn: '#7b6333',
        bad: '#7d4b4b',
    };

    const headerColors = {
        ok: '#b7ffce',
        warn: '#ffe3a8',
        bad: '#ffc0c0',
    };

    return (
        <div className="mb-6">
            <h4 className="text-sm font-semibold mb-2" style={{ color: 'var(--mono-muted)' }}>
                Pod Status
            </h4>

            {/* Replica summary */}
            <div className="mono-inline-status mb-3" style={toneColors[tone]}>
                <div className="flex items-center justify-between">
                    <span>Pods: {podStatus.ready_replicas}/{podStatus.desired_replicas} ready</span>
                    <span className="text-xs" style={{ color: 'var(--mono-muted)' }}>
                        {activePods.length} active{inactivePods.length > 0 && ` (+${inactivePods.length} previous)`}
                    </span>
                </div>
            </div>

            {/* Active pods */}
            {activePods.length > 0 && (
                <div className="border border-solid" style={{ borderColor: borderColors[tone], background: tone === 'ok' ? '#0a1210' : '#1a1212' }}>
                    <div className="p-3" style={{ borderBottom: `1px solid ${borderColors[tone]}` }}>
                        <h5 className="text-xs font-semibold" style={{ color: headerColors[tone] }}>
                            Active ({activePods.length})
                        </h5>
                    </div>
                    <div>
                        {activePods.map((pod, idx) => (
                            <PodInfoRow key={pod.name || `pod-${idx}`} pod={pod} />
                        ))}
                    </div>
                </div>
            )}

            {/* Terminating / terminated pods */}
            {inactivePods.length > 0 && (
                <div className="border border-solid mt-3" style={{ borderColor: 'var(--mono-line)', background: '#0f0f0f' }}>
                    <div className="p-3" style={{ borderBottom: '1px solid var(--mono-line)' }}>
                        <h5 className="text-xs font-semibold" style={{ color: 'var(--mono-muted)' }}>
                            Previous ({inactivePods.length})
                        </h5>
                    </div>
                    <div>
                        {inactivePods.map((pod, idx) => (
                            <PodInfoRow key={pod.name || `prev-${idx}`} pod={pod} />
                        ))}
                    </div>
                </div>
            )}

            <p className="text-xs mt-2" style={{ color: 'var(--mono-muted)' }}>
                Last checked: {formatRelativeTimeRounded(podStatus.last_checked)}
            </p>
        </div>
    );
}

export function EnvironmentDeploymentView({ projectName, environmentName }) {
    const [activeDeploymentId, setActiveDeploymentId] = useState(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);

    useEffect(() => {
        async function findActiveDeployment() {
            try {
                const deployments = await api.getProjectDeployments(projectName, { limit: 100 });
                const active = deployments.find(
                    (d) => d.environment === environmentName && d.is_active
                );
                setActiveDeploymentId(active ? active.deployment_id : null);
                setLoading(false);
            } catch (err) {
                setError(err.message);
                setLoading(false);
            }
        }
        findActiveDeployment();
    }, [projectName, environmentName]);

    if (loading) return <LoadingState label="Loading environment deployment..." />;
    if (error) return <ErrorState message={`Error: ${error}`} />;
    if (!activeDeploymentId) {
        return (
            <div>
                <p className="text-gray-600 dark:text-gray-400 mb-4">
                    No active deployment in the <strong>{environmentName}</strong> environment.
                </p>
                <Button variant="secondary" size="sm" onClick={() => navigate(`/project/${projectName}/environments`)}>
                    Back to Environments
                </Button>
            </div>
        );
    }

    return <DeploymentDetail projectName={projectName} deploymentId={activeDeploymentId} />;
}

export function DeploymentDetail({ projectName, deploymentId }) {
    const [deployment, setDeployment] = useState(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [rollbackDialogOpen, setRollbackDialogOpen] = useState(false);
    const [rolling, setRolling] = useState(false);
    const [useSourceEnvVars, setUseSourceEnvVars] = useState(false);
    const [stopDialogOpen, setStopDialogOpen] = useState(false);
    const [stopping, setStopping] = useState(false);
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

    const isTerminal = (status) => {
        return ['Cancelled', 'Stopped', 'Superseded', 'Failed', 'Expired'].includes(status);
    };

    const loadDeployment = useCallback(async () => {
        try {
            const data = await api.getDeployment(projectName, deploymentId);
            setDeployment(data);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName, deploymentId]);

    const handleRollbackClick = () => {
        setRollbackDialogOpen(true);
    };

    const handleStopConfirm = async () => {
        setStopping(true);
        setDetailActionStatus(`Stopping deployment ${deploymentId}...`);
        try {
            await api.stopDeployment(projectName, deploymentId);
            showToast(`Deployment ${deploymentId} stopped successfully`, 'success');
            setDetailActionStatus(`Stopped deployment ${deploymentId}.`);
            setStopDialogOpen(false);
            loadDeployment();
        } catch (err) {
            showToast(`Failed to stop deployment: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to stop deployment ${deploymentId}.`);
        } finally {
            setStopping(false);
        }
    };

    const handleRollback = async () => {
        setRolling(true);
        setDetailActionStatus(`${deployment.is_active ? 'Redeploying' : 'Rolling back'} deployment ${deploymentId}...`);
        try {
            const response = await api.createDeploymentFrom(projectName, deploymentId, useSourceEnvVars);
            showToast(`${deployment.is_active ? 'Redeploy' : 'Rollback'} successful! New deployment: ${response.deployment_id}`, 'success');
            setDetailActionStatus(`${deployment.is_active ? 'Redeployed' : 'Rolled back'} to deployment ${response.deployment_id}.`);
            setRollbackDialogOpen(false);
            setUseSourceEnvVars(false); // Reset checkbox
            // Redirect to project page to see the new deployment
            navigate(`/project/${projectName}`);
        } catch (err) {
            showToast(`Failed to ${deployment.is_active ? 'redeploy' : 'rollback'} deployment: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to ${deployment.is_active ? 'redeploy' : 'rollback'} deployment ${deploymentId}.`);
        } finally {
            setRolling(false);
        }
    };

    useEffect(() => {
        loadDeployment();
    }, [loadDeployment]);

    // Auto-refresh only if deployment is not in a terminal state
    useEffect(() => {
        if (deployment && !isTerminal(deployment.status)) {
            const interval = setInterval(loadDeployment, 5000);
            return () => clearInterval(interval);
        }
    }, [deployment?.status, loadDeployment]);

    if (loading) return <LoadingState label="Loading deployment..." />;
    if (error) return <ErrorState message={`Error loading deployment: ${error}`} onRetry={loadDeployment} />;
    if (!deployment) return <EmptyState message="Deployment not found." />;

    const timeline = buildDeploymentTimeline(deployment);
    const phases = ['build', 'push', 'rollout', 'health', 'other'];
    const groupedTimeline = phases
        .map((phase) => ({ phase, events: timeline.filter((e) => e.phase === phase) }))
        .filter((group) => group.events.length > 0);

    return (
        <section>
            <div className="flex justify-end items-center gap-2 mb-4">
                {deployment.can_rollback && (
                    <Button
                        variant="secondary"
                        size="sm"
                        onClick={handleRollbackClick}
                    >
                        {deployment.is_active ? 'Redeploy' : 'Rollback'}
                    </Button>
                )}
                {!isTerminal(deployment.status) && (
                    <Button
                        variant="danger"
                        size="sm"
                        onClick={() => setStopDialogOpen(true)}
                    >
                        Stop
                    </Button>
                )}
            </div>

            {detailActionStatus && <p className="mono-inline-status mb-4">{detailActionStatus}</p>}

            <div className="mono-status-strip mono-status-strip-normalcase mb-6">
                <div className={`mono-status-card mono-status-card-${getStatusTone(deployment.status)}`}>
                    <span>status</span>
                    <strong>{deployment.status}</strong>
                </div>
                <div>
                    <span>deployment</span>
                    <strong className="mono-copyable-value">
                        <span>{deployment.deployment_id}</span>
                        <button
                            type="button"
                            className="mono-copy-button"
                            title="Copy deployment ID"
                            aria-label="Copy deployment ID"
                            onClick={() => handleCopy(deployment.deployment_id, 'Deployment ID')}
                        >
                            <span
                                className="mono-copy-icon svg-mask"
                                style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                            />
                        </button>
                    </strong>
                </div>
                <div><span>group</span><strong>{deployment.deployment_group}</strong></div>
                {deployment.environment && (
                    <div><span>environment</span><strong className="inline-flex items-center gap-2"><EnvironmentColorDot color={deployment.environment_color} />{deployment.environment}</strong></div>
                )}
                <div>
                    <span>created</span>
                    <strong className="mono-copyable-value" title={formatISO8601(deployment.created)}>
                        <span>{formatRelativeTimeRounded(deployment.created)}</span>
                        <button
                            type="button"
                            className="mono-copy-button"
                            title="Copy created timestamp (ISO8601)"
                            aria-label="Copy created timestamp (ISO8601)"
                            onClick={() => handleCopy(formatISO8601(deployment.created), 'Created timestamp')}
                        >
                            <span
                                className="mono-copy-icon svg-mask"
                                style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                            />
                        </button>
                    </strong>
                </div>
                <div><span>project</span><strong>{projectName}</strong></div>
                <div><span>created_by</span><strong>{deployment.created_by_email || '-'}</strong></div>
                <div>
                    <span>image</span>
                    <strong className="mono-copyable-value">
                        <span>{deployment.image || '-'}</span>
                        {deployment.image && (
                            <button
                                type="button"
                                className="mono-copy-button"
                                title="Copy image"
                                aria-label="Copy image"
                                onClick={() => handleCopy(deployment.image, 'Image')}
                            >
                                <span
                                    className="mono-copy-icon svg-mask"
                                    style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                                />
                            </button>
                        )}
                    </strong>
                </div>
                <div><span>digest</span><strong>{deployment.image_digest || '-'}</strong></div>
                <div>
                    <span>primary_url</span>
                    <strong className="mono-copyable-value">
                        <span>
                            {deployment.primary_url ? (
                                <a href={deployment.primary_url} target="_blank" rel="noopener noreferrer" className="underline">
                                    {deployment.primary_url}
                                </a>
                            ) : '-'}
                        </span>
                        {deployment.primary_url && (
                            <button
                                type="button"
                                className="mono-copy-button"
                                title="Copy primary URL"
                                aria-label="Copy primary URL"
                                onClick={() => handleCopy(deployment.primary_url, 'Primary URL')}
                            >
                                <span
                                    className="mono-copy-icon svg-mask"
                                    style={{ maskImage: 'url(/assets/copy.svg)', WebkitMaskImage: 'url(/assets/copy.svg)' }}
                                />
                            </button>
                        )}
                    </strong>
                </div>
                <div>
                    <span>custom_urls</span>
                    <strong>
                        {deployment.custom_domain_urls && deployment.custom_domain_urls.length > 0
                            ? deployment.custom_domain_urls.map((url, idx) => (
                                <Fragment key={url}>
                                    {idx > 0 ? ', ' : ''}
                                    <a href={url} target="_blank" rel="noopener noreferrer" className="underline">
                                        {url}
                                    </a>
                                </Fragment>
                            ))
                            : '-'}
                    </strong>
                </div>
                {(deployment.job_url || deployment.pull_request_url) && (
                    <div>
                        <span>source</span>
                        <strong>
                            <SourceLinkGroup jobUrl={deployment.job_url} prUrl={deployment.pull_request_url} />
                        </strong>
                    </div>
                )}
                <div><span>completed</span><strong>{deployment.completed_at ? formatDate(deployment.completed_at) : '-'}</strong></div>
                <div><span>expires</span><strong>{deployment.expires_at ? formatTimeRemaining(deployment.expires_at) : '-'}</strong></div>
                <div><span>replicas</span><strong>{deployment.replicas}</strong></div>
                <div><span>cpu</span><strong>{deployment.cpu}</strong></div>
                <div><span>memory</span><strong>{deployment.memory}</strong></div>
            </div>

            {deployment.error_message && (
                <div className="mono-inline-status mb-6" style={{ color: '#ffc0c0', borderColor: '#7d4b4b', background: '#1a1212' }}>
                    Error: {deployment.error_message}
                </div>
            )}

            {deployment.controller_metadata?.pod_status && (
                <PodStatusSection podStatus={deployment.controller_metadata.pod_status} />
            )}

            {deployment.build_logs && (
                <details className="mb-6">
                    <summary className="cursor-pointer text-indigo-600 dark:text-indigo-400 hover:text-indigo-700 dark:hover:text-indigo-300 font-semibold">Build Logs</summary>
                    <pre className="mt-2 bg-gray-950 border border-gray-200 dark:border-gray-800 rounded p-4 overflow-x-auto text-xs">
                        <code className="text-gray-700 dark:text-gray-300">{deployment.build_logs}</code>
                    </pre>
                </details>
            )}

            <div className="bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-800 rounded-lg p-6 mb-6">
                <h3 className="text-xl font-bold mb-4">Deployment Timeline</h3>
                <div className="space-y-4">
                    {groupedTimeline.map((group) => (
                        <div key={group.phase} className="mono-timeline-group">
                            <h4 className="mono-timeline-phase">{group.phase}</h4>
                            <div className="mono-timeline-list">
                                {group.events.map((event, idx) => (
                                    <div key={`${group.phase}-${idx}`} className="mono-timeline-item">
                                        <span>{formatDate(event.ts || '')}</span>
                                        <span>{event.label}</span>
                                        <span>+{event.delta}</span>
                                    </div>
                                ))}
                            </div>
                        </div>
                    ))}
                </div>
            </div>

            <DeploymentLogs projectName={projectName} deploymentId={deploymentId} deploymentStatus={deployment.status} />

            <h3 className="text-xl font-bold mb-4">Environment Variables</h3>
            <EnvVarsList projectName={projectName} deploymentId={deploymentId} />

            <Modal
                isOpen={rollbackDialogOpen}
                onClose={() => {
                    setRollbackDialogOpen(false);
                    setUseSourceEnvVars(false);
                }}
                title={deployment?.is_active ? 'Redeploy' : 'Rollback to Deployment'}
            >
                <ModalSection>
                    <p className="text-gray-700 dark:text-gray-300">
                        {deployment?.is_active
                            ? `Are you sure you want to redeploy ${deploymentId}? This will create a new deployment with the same image.`
                            : `Are you sure you want to rollback to deployment ${deploymentId}? This will create a new deployment with the same image.`}
                    </p>
                    
                    <div className="bg-gray-50 dark:bg-gray-800 p-4 rounded-lg">
                        <label className="flex items-start gap-3 cursor-pointer">
                            <input
                                type="checkbox"
                                checked={useSourceEnvVars}
                                onChange={(e) => setUseSourceEnvVars(e.target.checked)}
                                className="mt-1 w-4 h-4 text-indigo-600 border-gray-300 rounded focus:ring-indigo-500"
                            />
                            <div className="flex-1">
                                <div className="text-sm font-medium text-gray-900 dark:text-gray-100">
                                    Use source deployment's environment variables
                                </div>
                                <div className="text-xs text-gray-600 dark:text-gray-400 mt-1">
                                    {useSourceEnvVars 
                                        ? "Will copy environment variables from the source deployment" 
                                        : "Will use the current project's environment variables (default)"}
                                </div>
                            </div>
                        </label>
                    </div>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => {
                                setRollbackDialogOpen(false);
                                setUseSourceEnvVars(false);
                            }}
                            disabled={rolling}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleRollback}
                            loading={rolling}
                            disabled={rolling}
                        >
                            {deployment?.is_active ? 'Redeploy' : 'Rollback'}
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>

            <ConfirmDialog
                isOpen={stopDialogOpen}
                onClose={() => setStopDialogOpen(false)}
                onConfirm={handleStopConfirm}
                title="Stop Deployment"
                message={`Are you sure you want to stop deployment ${deploymentId}? Impact: traffic for group "${deployment?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop Deployment"
                variant="danger"
                loading={stopping}
            />
        </section>
    );
}
