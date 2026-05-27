// @ts-nocheck
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Bar, BarChart, CartesianGrid, Cell, ResponsiveContainer, Tooltip as RechartsTooltip, XAxis, YAxis } from 'recharts';
import { DayPicker } from 'react-day-picker';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatDate, formatISO8601, formatRelativeTimeRounded, formatTimeRemaining, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { usePolling } from '../lib/polling';
import { useToast } from '../components/toast';
import { MonoSortButton, MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { Button as RButton, Combobox, ConfirmDialog, ENV_COLOR_STYLES, Empty, EnvPill, EnvironmentColorDot, GroupPill, KV, KVRow, Modal, Panel, PanelBody, PanelHead, Pill, SearchInput, Segmented, SourceLinkGroup, SourceLinkGroupAction, Status, Tabs } from '../components/r-ui';
import { Icon } from '../components/icon';
import { EnvVarsList } from './resources';
import { EmptyState, ErrorState, LoadingState } from '../components/states';

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

    // Auto-refresh every 5 seconds, paused when the tab is hidden.
    usePolling(loadSummary, 5000);

    useEffect(() => {
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
    }, [projectName]);

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
                                    <Status status={deployment.status} />
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
                                <dd>{deployment.primary_url
                                    ? (isSafeUrl(deployment.primary_url)
                                        ? <a href={deployment.primary_url} target="_blank" rel="noopener noreferrer" className="text-indigo-600 dark:text-indigo-400 hover:text-indigo-700 dark:hover:text-indigo-300">{deployment.primary_url}</a>
                                        : <span className="text-gray-900 dark:text-gray-200">{deployment.primary_url}</span>)
                                    : '-'}</dd>
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
                                    <dd><EnvPill env={deployment.environment} color={deployment.environment_color} /></dd>
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
                confirmTone="danger"
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
    const [statusFilter, setStatusFilter] = useState('active');
    const [search, setSearch] = useState('');
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [deploymentToStop, setDeploymentToStop] = useState(null);
    const [stopping, setStopping] = useState(false);
    const [rollbackDialogOpen, setRollbackDialogOpen] = useState(false);
    const [deploymentToRollback, setDeploymentToRollback] = useState(null);
    const [rollingBack, setRollingBack] = useState(false);
    const [actionStatus, setActionStatus] = useState('');
    const { showToast } = useToast();
    const pageSize = 10;
    // Sort deployments by created desc — the new r-table doesn't expose
    // sort headers, so we keep the default ordering only.
    const sortedDeployments = useMemo(() => {
        return [...deployments].sort((a, b) => {
            const av = a?.created;
            const bv = b?.created;
            if (av == null && bv == null) return 0;
            if (av == null) return 1;
            if (bv == null) return -1;
            return String(bv).localeCompare(String(av), undefined, { numeric: true, sensitivity: 'base' });
        });
    }, [deployments]);

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

    // Auto-refresh every 5 seconds, paused when the tab is hidden.
    usePolling(loadDeployments, 5000);

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
    const envFilteredDeployments = envFilter
        ? sortedDeployments.filter(d => d.environment === envFilter)
        : sortedDeployments;

    // Status filter — "Active" = not in a terminal state; the rest match a
    // specific status; "All" shows everything.
    const matchesStatus = (d) => {
        switch (statusFilter) {
            case 'active': return !isTerminal(d.status);
            case 'healthy': return d.status === 'Healthy';
            case 'unhealthy': return d.status === 'Unhealthy';
            case 'failed': return d.status === 'Failed';
            default: return true;
        }
    };
    const statusFilteredDeployments = envFilteredDeployments.filter(matchesStatus);

    // Client-side text search over the loaded page (deployment id / image / author)
    const q = search.trim().toLowerCase();
    const filteredDeployments = q
        ? statusFilteredDeployments.filter(d => {
            const haystack = `${d.deployment_id || ''} ${d.image || ''} ${d.created_by_email || ''}`.toLowerCase();
            return haystack.includes(q);
        })
        : statusFilteredDeployments;

    const envOptions = [
        { value: '', label: 'All envs' },
        ...environments.map(env => ({ value: env.name, label: env.name })),
    ];

    return (
        <div>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', marginBottom: 14 }}>
                <SearchInput
                    value={search}
                    onChange={setSearch}
                    placeholder="Filter deployments…"
                    style={{ flex: 1, maxWidth: 280 }}
                />
                <Segmented<string>
                    value={statusFilter}
                    options={[
                        { value: 'active', label: 'Active' },
                        { value: 'healthy', label: 'Healthy' },
                        { value: 'unhealthy', label: 'Unhealthy' },
                        { value: 'failed', label: 'Failed' },
                        { value: 'all', label: 'All' },
                    ]}
                    onChange={setStatusFilter}
                />
                {environments.length > 0 && (
                    <Segmented<string>
                        value={envFilter}
                        options={envOptions}
                        onChange={(v) => { setEnvFilter(v); setPage(0); }}
                        capitalize
                    />
                )}
                {deploymentGroups.length > 0 && (
                    <div style={{ width: 200 }}>
                        <Combobox
                            value={groupFilter}
                            onChange={(v) => { setGroupFilter(v); setPage(0); }}
                            options={[
                                { value: '', label: 'All groups' },
                                ...deploymentGroups.map(group => ({ value: group, label: group })),
                            ]}
                            placeholder="All groups"
                        />
                    </div>
                )}
                <div style={{ marginLeft: 'auto', fontSize: 12.5, color: 'var(--text-soft)' }}>
                    {filteredDeployments.length} of {deployments.length}
                </div>
            </div>
            {actionStatus && (
                <div className="r-alert info" style={{ marginBottom: 14, fontSize: 12.5 }}>
                    <Icon name="info" size={14} />
                    <div style={{ flex: 1 }}>{actionStatus}</div>
                </div>
            )}

            <Panel>
                {filteredDeployments.length === 0 ? (
                    <div style={{ padding: 36, textAlign: 'center', color: 'var(--text-muted)' }}>
                        No deployments found.
                    </div>
                ) : (
                    <table className="r-table">
                        <thead>
                            <tr>
                                <th>ID</th>
                                <th>Status</th>
                                <th>Env</th>
                                <th>Group</th>
                                <th>Image</th>
                                <th>Created by</th>
                                <th>Duration</th>
                                <th>Age</th>
                                <th style={{ textAlign: 'right' }}>Actions</th>
                            </tr>
                        </thead>
                        <tbody>
                            {filteredDeployments.map(d => (
                                <tr
                                    key={d.id}
                                    className="click"
                                    onClick={() => navigate(`/deployment/${projectName}/${d.deployment_id}`)}
                                >
                                    <td className="mono" style={{ fontSize: 12.25 }}>{d.deployment_id}</td>
                                    <td><Status status={d.status} /></td>
                                    <td>
                                        {d.environment ? (
                                            <EnvPill env={d.environment} color={d.environment_color} />
                                        ) : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                    </td>
                                    <td>{d.deployment_group ? (() => {
                                        const env = environments.find((e) => e.name === d.environment);
                                        return <GroupPill group={d.deployment_group} primary={!!env && env.primary_deployment_group === d.deployment_group} />;
                                    })() : null}</td>
                                    <td className="mono" style={{ fontSize: 12, color: 'var(--text-muted)' }}>
                                        {d.image ? d.image.split('/').pop() : '—'}
                                    </td>
                                    <td>{d.created_by_email || <span style={{ color: 'var(--text-soft)' }}>—</span>}</td>
                                    <td className="mono" style={{ fontSize: 12.25, color: 'var(--text-muted)' }}>
                                        {d.completed_at ? formatDurationDelta(d.created, d.completed_at) : '—'}
                                    </td>
                                    <td style={{ color: 'var(--text-muted)' }} title={formatISO8601(d.created)}>
                                        {formatRelativeTimeRounded(d.created)}
                                    </td>
                                    <td style={{ textAlign: 'right' }}>
                                        <div className="row-actions" style={{ display: 'inline-flex', gap: 6, justifyContent: 'flex-end' }}>
                                            {isRollbackable(d) && (
                                                <RButton
                                                    size="sm"
                                                    onClick={(e) => {
                                                        e.stopPropagation();
                                                        handleRollbackClick(d);
                                                    }}
                                                >
                                                    {d.is_active ? 'Redeploy' : 'Rollback'}
                                                </RButton>
                                            )}
                                            {!isTerminal(d.status) && (
                                                <RButton
                                                    size="sm"
                                                    variant="danger"
                                                    onClick={(e) => {
                                                        e.stopPropagation();
                                                        handleStopClick(d);
                                                    }}
                                                >
                                                    Stop
                                                </RButton>
                                            )}
                                        </div>
                                    </td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                )}
            </Panel>

            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginTop: 14 }}>
                <RButton
                    size="sm"
                    onClick={() => setPage(p => p - 1)}
                    disabled={page === 0}
                    icon="chevl"
                >
                    Previous
                </RButton>
                <span style={{ fontSize: 12.5, color: 'var(--text-muted)' }}>
                    Page {page + 1} · showing{' '}
                    {filteredDeployments.length === deployments.length
                        ? `${deployments.length} deployment${deployments.length === 1 ? '' : 's'}`
                        : `${filteredDeployments.length} of ${deployments.length} deployment${deployments.length === 1 ? '' : 's'}`}
                </span>
                <RButton
                    size="sm"
                    onClick={() => setPage(p => p + 1)}
                    disabled={!hasMore}
                >
                    Next
                </RButton>
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
                confirmTone="danger"
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
                footer={
                    <>
                        <RButton
                            onClick={() => {
                                setRollbackDialogOpen(false);
                                setDeploymentToRollback(null);
                                setUseSourceEnvVars(false);
                            }}
                            disabled={rollingBack}
                        >
                            Cancel
                        </RButton>
                        <RButton
                            variant="primary"
                            onClick={handleRollbackConfirm}
                            loading={rollingBack}
                            disabled={rollingBack}
                        >
                            {deploymentToRollback?.is_active ? 'Redeploy' : 'Rollback'}
                        </RButton>
                    </>
                }
            >
                <p style={{ fontSize: 13, color: 'var(--text-muted)', margin: 0, lineHeight: 1.6 }}>
                    {deploymentToRollback?.is_active
                        ? `Are you sure you want to redeploy ${deploymentToRollback?.deployment_id}? This will create a new deployment with the same image.`
                        : `Are you sure you want to rollback to deployment ${deploymentToRollback?.deployment_id}? This will create a new deployment with the same image.`}
                </p>

                <div style={{ background: 'var(--surface-2)', border: '1px solid var(--border-faint)', borderRadius: 'var(--radius-sm)', padding: 14 }}>
                    <label style={{ display: 'flex', alignItems: 'flex-start', gap: 10, cursor: 'pointer' }}>
                        <input
                            type="checkbox"
                            checked={useSourceEnvVars}
                            onChange={(e) => setUseSourceEnvVars(e.target.checked)}
                            style={{ marginTop: 2 }}
                        />
                        <div style={{ flex: 1 }}>
                            <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--text)' }}>
                                Use source deployment's environment variables
                            </div>
                            <div style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 4 }}>
                                {useSourceEnvVars
                                    ? 'Will copy environment variables from the source deployment'
                                    : "Will use the current project's environment variables (default)"}
                            </div>
                        </div>
                    </label>
                </div>
            </Modal>
        </div>
    );
}

// Split button used in the logs header: left half toggles live streaming on/off
// (pulsing dot when active), right half independently toggles auto-scroll.
function StreamToggle({ streaming, autoScroll, onToggleStream, onToggleAutoScroll }) {
    return (
        <div className="r-split-btn">
            <button
                type="button"
                className={`r-split-btn-main${streaming ? ' active' : ''}`}
                onClick={onToggleStream}
                aria-pressed={streaming}
                title={streaming ? 'Stop streaming' : 'Stream logs'}
            >
                <span className="dot" />
                <span>{streaming ? 'Live streaming' : 'Stream'}</span>
            </button>
            <button
                type="button"
                className={`r-split-btn-side${autoScroll ? ' active' : ''}`}
                onClick={onToggleAutoScroll}
                aria-pressed={autoScroll}
                aria-label="Toggle auto-scroll"
                title={autoScroll ? 'Auto-scroll on' : 'Auto-scroll off'}
            >
                <Icon name="chevd" size={12} />
            </button>
        </div>
    );
}

const STREAMABLE_LOG_STATUSES = new Set(['Deploying', 'Healthy', 'Unhealthy', 'Cancelling', 'Terminating']);
const LOGGABLE_STATUSES = new Set(['Deploying', 'Healthy', 'Unhealthy', 'Cancelling', 'Terminating', 'Cancelled', 'Stopped', 'Failed', 'Superseded', 'Expired']);

const LOG_RANGE_PRESETS = [
    { value: '15m', label: '15m', minutes: 15 },
    { value: '1h', label: '1h', minutes: 60 },
    { value: '6h', label: '6h', minutes: 360 },
    { value: '24h', label: '24h', minutes: 1440 },
    { value: '7d', label: '7d', minutes: 10080 },
    { value: 'custom', label: 'Custom', minutes: 0 },
];

function presetToMilliseconds(value) {
    const preset = LOG_RANGE_PRESETS.find((option) => option.value === value);
    return preset ? preset.minutes * 60 * 1000 : LOG_RANGE_PRESETS[2].minutes * 60 * 1000;
}

function resolveLogWindow(rangeValue, customStart, customEnd) {
    const now = new Date();
    if (rangeValue === 'custom') {
        if (!customStart) return null;
        const end = customEnd || now;
        if (customStart >= end) return null;
        return { start: customStart, end };
    }
    const durationMs = presetToMilliseconds(rangeValue);
    return { start: new Date(now.getTime() - durationMs), end: now };
}

function formatDateTimeShort(date) {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return '';
    const pad = (n) => String(n).padStart(2, '0');
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function formatTimeHm(date) {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return '00:00';
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function applyTimeOfDay(target, source) {
    if (!(target instanceof Date)) return null;
    const next = new Date(target);
    if (source instanceof Date && !Number.isNaN(source.getTime())) {
        next.setHours(source.getHours(), source.getMinutes(), 0, 0);
    }
    return next;
}

function applyTimeString(date, hhmm) {
    if (!(date instanceof Date)) return date;
    const match = /^(\d{1,2}):(\d{2})$/.exec(hhmm || '');
    if (!match) return date;
    const next = new Date(date);
    next.setHours(Math.min(23, Number(match[1])), Math.min(59, Number(match[2])), 0, 0);
    return next;
}

function DateRangePopover({ start, end, onChange }) {
    const [open, setOpen] = useState(false);
    const rootRef = useRef(null);

    useEffect(() => {
        if (!open) return undefined;
        const onDocMouseDown = (e) => {
            if (rootRef.current && !rootRef.current.contains(e.target)) setOpen(false);
        };
        const onKey = (e) => { if (e.key === 'Escape') setOpen(false); };
        document.addEventListener('mousedown', onDocMouseDown);
        document.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDocMouseDown);
            document.removeEventListener('keydown', onKey);
        };
    }, [open]);

    const label = start && end
        ? `${formatDateTimeShort(start)} → ${formatDateTimeShort(end)}`
        : 'Select range';

    // We drive selection from `onDayClick` rather than the built-in
    // mode="range" `onSelect` so that clicking inside an already-complete
    // range starts a fresh selection (the default behavior tries to extend
    // the existing range instead).
    const handleDayClick = (day) => {
        if (!start || (start && end)) {
            // Fresh first click - either we have no selection, or we already
            // have a complete one and the user wants to start over.
            onChange(applyTimeOfDay(day, start), null);
            return;
        }
        // We have `start` only; this click finishes the range.
        let s = start;
        let e = day;
        if (day.getTime() < start.getTime()) {
            s = day;
            e = start;
        }
        onChange(applyTimeOfDay(s, start), applyTimeOfDay(e, end));
    };

    return (
        <div ref={rootRef} className="r-date-range">
            <button
                type="button"
                className="r-field r-date-range-trigger"
                onClick={() => setOpen((v) => !v)}
            >
                {label}
            </button>
            {open && (
                <div className="r-date-range-popover">
                    <DayPicker
                        mode="range"
                        numberOfMonths={2}
                        selected={{ from: start || undefined, to: end || undefined }}
                        onDayClick={handleDayClick}
                        defaultMonth={start || new Date()}
                    />
                    <div className="r-date-range-times">
                        <label>
                            <span>From</span>
                            <input
                                type="time"
                                value={formatTimeHm(start)}
                                onChange={(e) => onChange(applyTimeString(start, e.target.value), end)}
                                className="r-field"
                            />
                        </label>
                        <label>
                            <span>To</span>
                            <input
                                type="time"
                                value={formatTimeHm(end)}
                                onChange={(e) => onChange(start, applyTimeString(end, e.target.value))}
                                className="r-field"
                            />
                        </label>
                        <RButton size="sm" variant="ghost" onClick={() => onChange(null, null)}>Clear</RButton>
                        <RButton size="sm" onClick={() => setOpen(false)}>Done</RButton>
                    </div>
                </div>
            )}
        </div>
    );
}

function chooseCountStepSeconds(rangeMs) {
    const rangeSeconds = Math.max(60, Math.floor(rangeMs / 1000));
    if (rangeSeconds <= 3600) return 60;
    if (rangeSeconds <= 6 * 3600) return 5 * 60;
    if (rangeSeconds <= 24 * 3600) return 15 * 60;
    if (rangeSeconds <= 7 * 24 * 3600) return 60 * 60;
    return 6 * 60 * 60;
}

function formatRangeLabel(rangeValue, window) {
    if (!window) return 'Select a valid time range';
    if (rangeValue === 'custom') {
        return `${formatDate(window.start.toISOString())} to ${formatDate(window.end.toISOString())}`;
    }
    const preset = LOG_RANGE_PRESETS.find((option) => option.value === rangeValue);
    return preset ? `Last ${preset.label}` : 'Selected range';
}

async function readHttpErrorMessage(response) {
    try {
        const text = await response.text();
        if (text) {
            try {
                const data = JSON.parse(text);
                if (data && typeof data.error === 'string') return data.error;
            } catch {
                /* not JSON */
            }
            if (text.length < 240) return text;
        }
    } catch {
        /* noop */
    }
    return `HTTP ${response.status}: ${response.statusText}`;
}

async function readSseResponse(response, handlers) {
    const reader = response.body?.getReader();
    if (!reader) {
        throw new Error('Log stream response did not include a body');
    }

    const decoder = new TextDecoder();
    let buffer = '';
    let eventType = 'message';

    while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n');
        buffer = lines.pop();

        lines.forEach((line) => {
            if (line.startsWith('event: ')) {
                eventType = line.substring(7);
            } else if (line.startsWith('data: ')) {
                const payload = line.substring(6);
                if (eventType === 'status') {
                    handlers.onStatus?.(payload);
                } else if (payload.trim()) {
                    handlers.onLine?.(payload);
                }
            } else if (line === '') {
                eventType = 'message';
            }
        });
    }
}

const LOG_PAGE_SIZE = 200;
const LOG_STREAM_CAP = 2000;
const LOG_SCROLL_TOP_THRESHOLD = 8;
const LOG_LONG_LINE_THRESHOLD = 120;

function detectLogLevel(line) {
    const l = line.toLowerCase();
    if (/\b(error|err|fatal|panic|exception|failed)\b/.test(l)) return 'error';
    if (/\b(warn|warning)\b/.test(l)) return 'warn';
    return 'info';
}

function parseLogLine(line, seq) {
    // Backend prepends "<RFC3339> " when timestamps=true.
    const sp = line.indexOf(' ');
    const isoCandidate = sp > 0 ? line.slice(0, sp) : '';
    const date = isoCandidate ? new Date(isoCandidate) : null;
    const hasTs = date && !Number.isNaN(date.getTime());
    const timestampMs = hasTs ? date.getTime() : 0;
    const iso = hasTs ? isoCandidate : '';
    const raw = hasTs ? line.slice(sp + 1) : line;
    const parsed = extractLogJson(raw);
    return {
        id: `${timestampMs}-${seq}`,
        timestampMs,
        iso,
        raw,
        level: detectLogLevel(raw),
        isJson: parsed !== null,
        parsed,
    };
}

function formatHms(timestampMs) {
    if (!timestampMs) return '--:--:--.---';
    const d = new Date(timestampMs);
    const pad2 = (n) => String(n).padStart(2, '0');
    const pad3 = (n) => String(n).padStart(3, '0');
    return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}.${pad3(d.getMilliseconds())}`;
}

function extractLogJson(raw) {
    const s = raw.trim();
    if (s.length < 2) return null;
    const candidates = ['{', '['].map((c) => s.indexOf(c)).filter((i) => i >= 0);
    if (candidates.length === 0) return null;
    const idx = Math.min(...candidates);
    try {
        return JSON.parse(s.slice(idx));
    } catch {
        return null;
    }
}

function highlightLogJson(value) {
    const pretty = JSON.stringify(value, null, 2);
    const re = /("(?:\\.|[^"\\])*"\s*:|"(?:\\.|[^"\\])*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|\b(?:true|false|null)\b)/g;
    const out = [];
    let last = 0;
    let key = 0;
    pretty.replace(re, (match, _g1, offset) => {
        if (offset > last) out.push(pretty.slice(last, offset));
        let cls = 'json-num';
        if (match.startsWith('"')) cls = match.endsWith(':') ? 'json-key' : 'json-str';
        else if (match === 'true' || match === 'false') cls = 'json-bool';
        else if (match === 'null') cls = 'json-null';
        out.push(<span key={key++} className={cls}>{match}</span>);
        last = offset + match.length;
        return match;
    });
    if (last < pretty.length) out.push(pretty.slice(last));
    return <pre className="r-logs-json">{out}</pre>;
}

function ExpandedLogLine({ entry }) {
    if (entry.isJson) return highlightLogJson(entry.parsed);
    return <span>{entry.raw}</span>;
}

const LOG_CHART_COLOR_INFO = 'oklch(0.62 0.005 80)';
const LOG_CHART_COLOR_WARN = 'oklch(0.78 0.13 85)';
const LOG_CHART_COLOR_ERROR = 'oklch(0.62 0.16 25)';

function formatTickLabel(ms, rangeMs) {
    const d = new Date(ms);
    const pad = (n) => String(n).padStart(2, '0');
    if (rangeMs <= 24 * 3600 * 1000) {
        return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    }
    return `${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function LogChartTooltip({ active, payload, stepSeconds }) {
    if (!active || !payload || payload.length === 0) return null;
    const b = payload[0].payload;
    const start = new Date(b.ts);
    const end = new Date(b.ts + stepSeconds * 1000);
    return (
        <div className="r-logs-chart-tip">
            <div className="r-logs-chart-tip-title">
                {formatDate(start.toISOString())} – {formatTickLabel(end.getTime(), 0)}
            </div>
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_ERROR }} />
                <span className="lbl">Error</span>
                <span className="val">{b.error || 0}</span>
            </div>
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_WARN }} />
                <span className="lbl">Warn</span>
                <span className="val">{b.warn || 0}</span>
            </div>
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_INFO }} />
                <span className="lbl">Info</span>
                <span className="val">{b.info || 0}</span>
            </div>
            <div className="r-logs-chart-tip-total">
                <span className="lbl">Total</span>
                <span className="val">{b.total || 0}</span>
            </div>
            <div className="r-logs-chart-tip-hint">Click to zoom</div>
        </div>
    );
}

function LogCountsChart({ counts, loading, error, status, rangeLabel, rangeStartMs, rangeEndMs, stepSeconds, onSelectBucket, selectedBucketStartMs, collapsed, onToggleCollapsed }) {
    const data = useMemo(
        () =>
            counts.map((b) => ({
                ...b,
                ts: new Date(b.timestamp).getTime(),
            })),
        [counts],
    );
    const totalSum = useMemo(() => data.reduce((sum, b) => sum + (b.total || 0), 0), [data]);
    const rangeMs = (rangeEndMs || 0) - (rangeStartMs || 0);

    const statusMessage = () => {
        if (!status) return 'No log volume found for the selected range.';
        if (status.reason === 'retention_expired_possible') {
            return status.retention_hint
                ? `No log volume found. Runtime logs are retained for ${status.retention_hint}, so this range may no longer be available.`
                : 'No log volume found. It may have expired based on the log backend retention policy.';
        }
        if (status.reason === 'historical_backend_not_configured') {
            return 'Log volume charts are only available with Loki.';
        }
        if (status.reason === 'deployment_not_ready') {
            return 'Deployment logs are not ready yet.';
        }
        if (status.reason === 'backend_unavailable') {
            return 'The log backend is unavailable.';
        }
        return 'No log volume found for the selected range.';
    };

    const handleBucketClick = (entry) => {
        if (!onSelectBucket || !entry || typeof entry.ts !== 'number') return;
        const start = entry.ts;
        const end = entry.ts + stepSeconds * 1000;
        // Toggle: re-clicking the same bar clears the selection.
        if (selectedBucketStartMs === start) {
            onSelectBucket(null);
        } else {
            onSelectBucket({ startMs: start, endMs: end });
        }
    };

    return (
        <div className="r-logs-chart" style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            <button
                type="button"
                className="r-logs-chart-head"
                onClick={onToggleCollapsed}
                aria-expanded={!collapsed}
            >
                <span className="r-logs-chart-head-toggle">
                    <Icon name={collapsed ? 'chev' : 'chevd'} size={12} />
                </span>
                <span className="r-logs-chart-head-title">Log volume</span>
                <span className="r-logs-chart-head-range">{rangeLabel}</span>
            </button>
            {collapsed ? null : (
            <div className="rounded border border-[var(--border)] bg-[var(--panel)] px-2 py-1">
                {loading ? (
                    <div className="py-6 text-center text-xs text-[var(--text-soft)]">Loading chart…</div>
                ) : error ? (
                    <div className="py-6 text-center text-xs text-[var(--err)]">{error}</div>
                ) : !data.length || totalSum === 0 ? (
                    <div className="py-6 text-center text-xs text-[var(--text-soft)]">{statusMessage()}</div>
                ) : (
                    <ResponsiveContainer width="100%" height={96}>
                        <BarChart
                            data={data}
                            margin={{ top: 4, right: 6, bottom: 4, left: 4 }}
                            barCategoryGap={1}
                        >
                            <CartesianGrid stroke="var(--border)" strokeDasharray="2 3" vertical={false} />
                            <XAxis
                                dataKey="ts"
                                type="number"
                                scale="time"
                                domain={['dataMin', 'dataMax']}
                                tickFormatter={(ms) => formatTickLabel(ms, rangeMs)}
                                tick={{ fontSize: 10, fill: 'var(--text-soft)' }}
                                axisLine={{ stroke: 'var(--border)' }}
                                tickLine={{ stroke: 'var(--border)' }}
                                minTickGap={48}
                            />
                            <YAxis
                                allowDecimals={false}
                                tick={{ fontSize: 10, fill: 'var(--text-soft)' }}
                                axisLine={{ stroke: 'var(--border)' }}
                                tickLine={{ stroke: 'var(--border)' }}
                                width={44}
                            />
                            <RechartsTooltip
                                content={<LogChartTooltip stepSeconds={stepSeconds} />}
                                cursor={{ fill: 'oklch(0.22 0.005 80 / 0.45)' }}
                            />
                            <Bar dataKey="info" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                                {data.map((d) => (
                                    <Cell
                                        key={`info-${d.ts}`}
                                        fill={LOG_CHART_COLOR_INFO}
                                        fillOpacity={selectedBucketStartMs == null || selectedBucketStartMs === d.ts ? 1 : 0.3}
                                    />
                                ))}
                            </Bar>
                            <Bar dataKey="warn" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                                {data.map((d) => (
                                    <Cell
                                        key={`warn-${d.ts}`}
                                        fill={LOG_CHART_COLOR_WARN}
                                        fillOpacity={selectedBucketStartMs == null || selectedBucketStartMs === d.ts ? 1 : 0.3}
                                    />
                                ))}
                            </Bar>
                            <Bar dataKey="error" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                                {data.map((d) => (
                                    <Cell
                                        key={`error-${d.ts}`}
                                        fill={LOG_CHART_COLOR_ERROR}
                                        fillOpacity={selectedBucketStartMs == null || selectedBucketStartMs === d.ts ? 1 : 0.3}
                                    />
                                ))}
                            </Bar>
                        </BarChart>
                    </ResponsiveContainer>
                )}
            </div>
            )}
        </div>
    );
}

// Deployment Logs Component with SSE streaming + infinite scroll
function DeploymentLogs({ projectName, deploymentId, deploymentStatus }) {
    const streamable = STREAMABLE_LOG_STATUSES.has(deploymentStatus);
    const loggable = LOGGABLE_STATUSES.has(deploymentStatus);
    const [entries, setEntries] = useState([]);
    const [streaming, setStreaming] = useState(false);
    const [error, setError] = useState(null);
    const [logStatus, setLogStatus] = useState(null);
    const [counts, setCounts] = useState([]);
    const [countsLoading, setCountsLoading] = useState(false);
    const [countsError, setCountsError] = useState(null);
    const [countsStatus, setCountsStatus] = useState(null);
    const [autoScroll, setAutoScroll] = useState(true);
    const [levelFilter, setLevelFilter] = useState('all');
    const [rangeValue, setRangeValue] = useState('6h');
    const [customStart, setCustomStart] = useState(null);
    const [customEnd, setCustomEnd] = useState(null);
    const [hasMore, setHasMore] = useState(false);
    const [loadingMore, setLoadingMore] = useState(false);
    const [expandedIds, setExpandedIds] = useState(() => new Set());
    const [searchInput, setSearchInput] = useState('');
    const [searchActive, setSearchActive] = useState('');
    const [chartCollapsed, setChartCollapsed] = useState(true);
    // `selectedBucket` narrows the log list to a single chart bucket without
    // collapsing the rangeWindow (which would lose the chart context). The
    // chart still renders the full range; we just override the log query.
    const [selectedBucket, setSelectedBucket] = useState(null);

    const logBoxRef = useRef(null);
    const loadMoreRef = useRef(null);
    const abortControllerRef = useRef(null);
    const countsRequestIdRef = useRef(0);
    const seqRef = useRef(0);
    const oldestLoadedMsRef = useRef(null);
    const loadingMoreRef = useRef(false);
    const ignoreScrollRef = useRef(false);
    const streamGenRef = useRef(0);

    const rangeWindow = useMemo(() => resolveLogWindow(rangeValue, customStart, customEnd), [rangeValue, customStart, customEnd]);
    const rangeLabel = useMemo(() => formatRangeLabel(rangeValue, rangeWindow), [rangeValue, rangeWindow]);
    const rangeStepSeconds = useMemo(() => {
        if (!rangeWindow) return 60;
        return chooseCountStepSeconds(rangeWindow.end.getTime() - rangeWindow.start.getTime());
    }, [rangeWindow]);
    // Effective window used for log queries (loadHistoricalLogs / loadOlder).
    // The chart and counts always use rangeWindow so the user keeps context.
    const logWindow = useMemo(() => {
        if (selectedBucket) {
            return {
                start: new Date(selectedBucket.startMs),
                end: new Date(selectedBucket.endMs),
            };
        }
        return rangeWindow;
    }, [rangeWindow, selectedBucket]);

    const stopStreaming = useCallback(() => {
        if (abortControllerRef.current) {
            abortControllerRef.current.abort();
            abortControllerRef.current = null;
        }
        setStreaming(false);
    }, []);

    const refreshCounts = useCallback(async () => {
        if (!rangeWindow) {
            setCounts([]);
            setCountsStatus(null);
            setCountsError('Select a valid time range');
            return;
        }

        const requestId = countsRequestIdRef.current + 1;
        countsRequestIdRef.current = requestId;
        setCountsLoading(true);
        setCountsError(null);

        const baseUrl = window.API_BASE_URL || '';
        const params = new URLSearchParams({
            start: rangeWindow.start.toISOString(),
            end: rangeWindow.end.toISOString(),
            step_seconds: String(rangeStepSeconds),
        });
        if (levelFilter && levelFilter !== 'all') params.set('level', levelFilter);
        if (searchActive) params.set('search', searchActive);

        try {
            const response = await fetch(
                `${baseUrl}/api/v1/projects/${projectName}/deployments/${deploymentId}/logs/counts?${params.toString()}`,
                {
                    headers: { 'Accept': 'application/json' },
                    credentials: 'include',
                },
            );

            if (!response.ok) {
                throw new Error(await readHttpErrorMessage(response));
            }

            const data = await response.json();
            if (countsRequestIdRef.current !== requestId) return;
            setCounts(data.buckets || []);
            setCountsStatus(data.status || null);
        } catch (err) {
            if (countsRequestIdRef.current !== requestId) return;
            console.error('Failed to load log counts:', err);
            setCounts([]);
            setCountsStatus(null);
            setCountsError(err.message);
        } finally {
            if (countsRequestIdRef.current === requestId) {
                setCountsLoading(false);
            }
        }
    }, [deploymentId, projectName, rangeStepSeconds, rangeWindow, levelFilter, searchActive]);

    // Cancellable SSE fetch. `onLine` receives raw (timestamped) lines.
    const fetchLogFeed = useCallback(async ({ follow, start, end, tail, level, search, onLine, onStatus }) => {
        if (abortControllerRef.current) {
            abortControllerRef.current.abort();
            abortControllerRef.current = null;
        }

        const controller = new AbortController();
        abortControllerRef.current = controller;

        const baseUrl = window.API_BASE_URL || '';
        const params = new URLSearchParams({
            timestamps: 'true',
            tail: String(tail),
        });
        if (follow) {
            params.set('follow', 'true');
        }
        if (start) params.set('start', start);
        if (end) params.set('end', end);
        if (level && level !== 'all') params.set('level', level);
        if (search) params.set('search', search);

        const response = await fetch(
            `${baseUrl}/api/v1/projects/${projectName}/deployments/${deploymentId}/logs?${params.toString()}`,
            {
                headers: { 'Accept': 'text/event-stream' },
                credentials: 'include',
                signal: controller.signal,
            },
        );

        if (!response.ok) {
            throw new Error(await readHttpErrorMessage(response));
        }

        await readSseResponse(response, { onLine, onStatus });
    }, [deploymentId, projectName]);

    const loadHistoricalLogs = useCallback(async () => {
        if (!logWindow) {
            setError('Select a valid time range.');
            setCounts([]);
            setCountsStatus(null);
            setCountsError('Select a valid time range.');
            setCountsLoading(false);
            return;
        }

        setEntries([]);
        setExpandedIds(new Set());
        setError(null);
        setLogStatus(null);
        setStreaming(false);
        setHasMore(false);
        oldestLoadedMsRef.current = null;

        const collected = [];
        let newStatus = null;

        try {
            await fetchLogFeed({
                follow: false,
                start: logWindow.start.toISOString(),
                end: logWindow.end.toISOString(),
                tail: LOG_PAGE_SIZE,
                level: levelFilter,
                search: searchActive,
                onLine: (line) => {
                    collected.push(parseLogLine(line, seqRef.current++));
                },
                onStatus: (payload) => {
                    try {
                        newStatus = JSON.parse(payload);
                    } catch {
                        newStatus = { reason: 'backend_unavailable' };
                    }
                },
            });
            // Backend sorts ascending; reverse so newest is on top.
            const reversed = collected.slice().reverse();
            setEntries(reversed);
            setLogStatus(newStatus);
            if (reversed.length > 0) {
                oldestLoadedMsRef.current = reversed[reversed.length - 1].timestampMs;
            }
            setHasMore(collected.length === LOG_PAGE_SIZE);
        } catch (err) {
            if (err.name === 'AbortError') return;
            console.error('Failed to load logs:', err);
            setError(err.message);
        } finally {
            if (abortControllerRef.current) {
                abortControllerRef.current = null;
            }
        }
    }, [fetchLogFeed, logWindow, levelFilter, searchActive]);

    const loadOlder = useCallback(async () => {
        if (loadingMoreRef.current) return;
        if (!hasMore) return;
        if (!logWindow) return;
        const oldestMs = oldestLoadedMsRef.current;
        if (!oldestMs) return;

        loadingMoreRef.current = true;
        setLoadingMore(true);

        const baseUrl = window.API_BASE_URL || '';
        const params = new URLSearchParams({
            timestamps: 'true',
            tail: String(LOG_PAGE_SIZE),
            start: logWindow.start.toISOString(),
            end: new Date(oldestMs).toISOString(),
        });
        if (levelFilter && levelFilter !== 'all') params.set('level', levelFilter);
        if (searchActive) params.set('search', searchActive);

        try {
            const response = await fetch(
                `${baseUrl}/api/v1/projects/${projectName}/deployments/${deploymentId}/logs?${params.toString()}`,
                {
                    headers: { 'Accept': 'text/event-stream' },
                    credentials: 'include',
                },
            );

            if (!response.ok) {
                throw new Error(await readHttpErrorMessage(response));
            }

            const collected = [];
            await readSseResponse(response, {
                onLine: (line) => {
                    collected.push(parseLogLine(line, seqRef.current++));
                },
            });

            const reversed = collected.slice().reverse();
            setEntries((prev) => {
                const seen = new Set(prev.map((e) => e.id));
                const filtered = reversed.filter((e) => !seen.has(e.id) && e.timestampMs < oldestMs);
                if (filtered.length === 0) return prev;
                return prev.concat(filtered);
            });
            if (reversed.length > 0) {
                oldestLoadedMsRef.current = reversed[reversed.length - 1].timestampMs;
            }
            setHasMore(collected.length === LOG_PAGE_SIZE);
        } catch (err) {
            console.error('Failed to load older logs:', err);
        } finally {
            loadingMoreRef.current = false;
            setLoadingMore(false);
        }
    }, [deploymentId, projectName, hasMore, logWindow, levelFilter, searchActive]);

    const startStreaming = useCallback(async () => {
        const gen = ++streamGenRef.current;
        setEntries([]);
        setExpandedIds(new Set());
        setError(null);
        setLogStatus(null);
        setStreaming(true);
        setHasMore(false);
        oldestLoadedMsRef.current = null;

        try {
            await fetchLogFeed({
                follow: true,
                tail: 1,
                level: levelFilter,
                search: searchActive,
                onLine: (line) => {
                    if (streamGenRef.current !== gen) return;
                    const newEntry = parseLogLine(line, seqRef.current++);
                    setEntries((prev) => {
                        // Loki tail can deliver lines slightly out-of-order; keep
                        // the array sorted descending by timestamp.
                        let i = 0;
                        while (i < prev.length && prev[i].timestampMs > newEntry.timestampMs) i++;
                        if (i < prev.length && prev[i].id === newEntry.id) return prev;
                        const next = prev.slice();
                        next.splice(i, 0, newEntry);
                        if (next.length > LOG_STREAM_CAP) next.length = LOG_STREAM_CAP;
                        return next;
                    });
                    if (newEntry.timestampMs > 0
                        && (oldestLoadedMsRef.current === null
                            || newEntry.timestampMs < oldestLoadedMsRef.current)) {
                        oldestLoadedMsRef.current = newEntry.timestampMs;
                    }
                    setHasMore(true);
                },
                onStatus: (payload) => {
                    if (streamGenRef.current !== gen) return;
                    try {
                        setLogStatus(JSON.parse(payload));
                    } catch {
                        setLogStatus({ reason: 'backend_unavailable' });
                    }
                },
            });
        } catch (err) {
            if (err.name === 'AbortError') return;
            console.error('Failed to start log stream:', err);
            if (streamGenRef.current === gen) setError(err.message);
        } finally {
            // Only the most recent invocation may flip global stream state.
            // Without this, a Refresh during streaming would see the previous
            // call's `finally` block clobber `streaming` back to false.
            if (streamGenRef.current === gen) {
                abortControllerRef.current = null;
                setStreaming(false);
            }
        }
    }, [fetchLogFeed, levelFilter, searchActive]);

    // Refresh the volume chart whenever the time range or filters change. A
    // small debounce smooths out custom datetime-local edits. Skip entirely
    // while the chart is collapsed so we don't query Loki for data nobody is
    // looking at.
    useEffect(() => {
        if (!loggable) return undefined;
        if (!rangeWindow) return undefined;
        if (chartCollapsed) return undefined;
        const handle = window.setTimeout(() => { void refreshCounts(); }, 250);
        return () => window.clearTimeout(handle);
    }, [rangeWindow, loggable, refreshCounts, chartCollapsed]);

    // Reload on deployment identity/status changes, when the user changes the
    // level/search filters, and when the visible time range changes. A preset
    // range (15m, 1h, ...) means "live tail with this much history"; custom
    // implies the user picked specific historical times, so don't open the
    // live tail (otherwise current-time lines keep arriving even though the
    // user is looking at a past window).
    // eslint-disable-next-line react-hooks/exhaustive-deps
    useEffect(() => {
        if (!loggable) {
            return undefined;
        }
        // Selecting a chart bucket narrows the log query to a fixed past
        // window, so always use the historical path then.
        if (streamable && rangeValue !== 'custom' && !selectedBucket) {
            void startStreaming();
        } else {
            void loadHistoricalLogs();
        }
        return () => stopStreaming();
    }, [deploymentId, deploymentStatus, levelFilter, searchActive, rangeValue, customStart, customEnd, selectedBucket]);

    // A bucket selection is tied to a specific chart row; if the range or
    // filters change, the previously-selected bucket may no longer be on
    // screen, so drop it.
    useEffect(() => {
        setSelectedBucket(null);
    }, [rangeWindow, levelFilter, searchActive]);

    // Debounce the search input so we don't reopen the SSE on every keystroke.
    useEffect(() => {
        const handle = window.setTimeout(() => setSearchActive(searchInput.trim()), 350);
        return () => window.clearTimeout(handle);
    }, [searchInput]);

    // Pin scroll to top when new entries arrive at the top and auto-scroll is on.
    useEffect(() => {
        if (!autoScroll) return;
        const box = logBoxRef.current;
        if (!box) return;
        ignoreScrollRef.current = true;
        box.scrollTop = 0;
        // Release the guard on the next frame so any synthetic scroll event from
        // the programmatic write doesn't toggle autoScroll off again.
        const handle = window.requestAnimationFrame(() => {
            ignoreScrollRef.current = false;
        });
        return () => window.cancelAnimationFrame(handle);
    }, [entries, autoScroll]);

    // IntersectionObserver for infinite scroll: when sentinel near the bottom
    // becomes visible, load older logs.
    useEffect(() => {
        const sentinel = loadMoreRef.current;
        if (!sentinel) return undefined;
        if (!hasMore) return undefined;
        const observer = new IntersectionObserver((records) => {
            for (const record of records) {
                if (record.isIntersecting) {
                    void loadOlder();
                    break;
                }
            }
        }, { root: logBoxRef.current, rootMargin: '120px 0px' });
        observer.observe(sentinel);
        return () => observer.disconnect();
    }, [hasMore, loadOlder, entries.length]);

    const isHistoricalUnavailable = countsStatus?.reason === 'historical_backend_not_configured';

    const handleRangeChange = (value) => {
        setRangeValue(value);
        if (value === 'custom' && !customStart && !customEnd) {
            const now = new Date();
            setCustomEnd(now);
            setCustomStart(new Date(now.getTime() - presetToMilliseconds('6h')));
        }
    };

    const handleCustomRangeChange = (newStart, newEnd) => {
        setCustomStart(newStart);
        setCustomEnd(newEnd);
    };

    const handleSelectBucket = useCallback((bucket) => {
        setSelectedBucket(bucket);
    }, []);

    const handleRefresh = useCallback(() => {
        if (streamable) {
            void startStreaming();
        } else {
            void loadHistoricalLogs();
        }
    }, [streamable, startStreaming, loadHistoricalLogs]);

    const handleCopyLogs = async () => {
        try {
            await copyToClipboard(entries.map((e) => e.iso ? `${e.iso} ${e.raw}` : e.raw).join('\n'));
        } catch {
            /* noop */
        }
    };

    const handleScroll = useCallback(() => {
        if (ignoreScrollRef.current) return;
        const box = logBoxRef.current;
        if (!box) return;
        const atTop = box.scrollTop <= LOG_SCROLL_TOP_THRESHOLD;
        setAutoScroll((prev) => (prev === atTop ? prev : atTop));
    }, []);

    const toggleExpanded = useCallback((id) => {
        setExpandedIds((prev) => {
            const next = new Set(prev);
            if (next.has(id)) next.delete(id);
            else next.add(id);
            return next;
        });
    }, []);

    const renderLogStatus = () => {
        if (!logStatus) return null;
        if (logStatus.reason === 'retention_expired_possible') {
            return logStatus.retention_hint
                ? `No logs found. Runtime logs are retained for ${logStatus.retention_hint}, so this deployment's logs may no longer be available.`
                : 'No logs found. They may have expired based on the log backend retention policy.';
        }
        if (logStatus.reason === 'historical_backend_not_configured') {
            return 'No active deployment pod was found and historical logs are not configured.';
        }
        if (logStatus.reason === 'deployment_not_ready') {
            return 'Deployment logs are not ready yet.';
        }
        if (logStatus.reason === 'backend_unavailable') {
            return 'The log backend is unavailable.';
        }
        return 'No logs found.';
    };

    if (!loggable) {
        return null;
    }

    // Backend already applies the level filter, so the entries array contains
    // only matching lines.
    const visibleEntries = entries;

    return (
        <Panel style={{ display: 'flex', flexDirection: 'column' }}>
            <div className="r-panel-head r-logs-head">
                <div className="r-logs-head-left">
                    <div className="r-logs-level">
                        <Combobox
                            value={levelFilter}
                            options={[
                                { value: 'all', label: 'All levels' },
                                { value: 'info', label: 'Info' },
                                { value: 'warn', label: 'Warn' },
                                { value: 'error', label: 'Error' },
                            ]}
                            onChange={setLevelFilter}
                        />
                    </div>
                    <div className="r-logs-search">
                        <Icon name="search" size={12} />
                        <input
                            type="search"
                            placeholder="Search lines…"
                            value={searchInput}
                            onChange={(e) => setSearchInput(e.target.value)}
                            className="r-field"
                            aria-label="Search log lines"
                        />
                    </div>
                </div>
                <div className="r-logs-head-right">
                    <div className="r-logs-range">
                        <Combobox
                            value={rangeValue}
                            options={LOG_RANGE_PRESETS.map((option) => ({
                                value: option.value,
                                label: option.value === 'custom' ? 'Custom range' : `Last ${option.label}`,
                            }))}
                            onChange={handleRangeChange}
                        />
                    </div>
                    {rangeValue === 'custom' && (
                        <DateRangePopover
                            start={customStart}
                            end={customEnd}
                            onChange={handleCustomRangeChange}
                        />
                    )}
                    <RButton size="sm" onClick={handleRefresh}>Refresh</RButton>
                    <RButton size="sm" onClick={handleCopyLogs} disabled={entries.length === 0} title="Copy logs">
                        <Icon name="copy" size={11} />
                    </RButton>
                </div>
            </div>
            <PanelBody style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                {error && (
                    <div className="r-alert err" style={{ fontSize: 12.5 }}>
                        <Icon name="info" size={14} />
                        <div style={{ flex: 1 }}>Error: {error}</div>
                    </div>
                )}
                <LogCountsChart
                    counts={counts}
                    loading={countsLoading}
                    error={countsError}
                    status={countsStatus}
                    rangeLabel={rangeLabel}
                    rangeStartMs={rangeWindow?.start.getTime() || 0}
                    rangeEndMs={rangeWindow?.end.getTime() || 0}
                    stepSeconds={rangeStepSeconds}
                    onSelectBucket={handleSelectBucket}
                    selectedBucketStartMs={selectedBucket?.startMs ?? null}
                    collapsed={chartCollapsed}
                    onToggleCollapsed={() => setChartCollapsed((v) => !v)}
                />
                {isHistoricalUnavailable && (
                    <div className="text-xs text-[var(--text-soft)]">
                        Historical volume charts are not available for this log backend.
                    </div>
                )}
                {selectedBucket && (
                    <div className="r-logs-bucket-banner">
                        <span>
                            Filtered to {formatHms(selectedBucket.startMs)}–{formatHms(selectedBucket.endMs)}
                        </span>
                        <button
                            type="button"
                            className="r-logs-bucket-clear"
                            onClick={() => setSelectedBucket(null)}
                            aria-label="Clear bucket filter"
                        >
                            <Icon name="close" size={11} />
                        </button>
                    </div>
                )}
                <div
                    ref={logBoxRef}
                    className="r-logs"
                    onScroll={handleScroll}
                    style={{ height: 'calc(100vh - 380px)', minHeight: 360, flex: 'none' }}
                >
                    {visibleEntries.length === 0 ? (
                        <div style={{ textAlign: 'center', padding: '32px 0', color: 'oklch(0.55 0.005 80)' }}>
                            {entries.length === 0
                                ? (renderLogStatus() || (streaming ? 'Waiting for logs…' : 'No logs in the selected range. Click Refresh to retry.'))
                                : 'No log lines match this filter.'}
                        </div>
                    ) : (
                        <>
                            {visibleEntries.map((entry) => {
                                const expanded = expandedIds.has(entry.id);
                                const canExpand = entry.isJson || entry.raw.length > LOG_LONG_LINE_THRESHOLD;
                                const actionLabel = expanded
                                    ? (entry.isJson ? 'Collapse JSON' : 'Show less')
                                    : (entry.isJson ? 'Expand JSON' : 'Show more');
                                const classes = `r-logs-row lv-${entry.level}`
                                    + (canExpand ? ' is-expandable' : '')
                                    + (expanded ? ' is-expanded' : '');
                                return (
                                    <div
                                        key={entry.id}
                                        role={canExpand ? 'button' : undefined}
                                        tabIndex={canExpand ? 0 : undefined}
                                        className={classes}
                                        title={entry.iso}
                                        onClick={canExpand ? () => toggleExpanded(entry.id) : undefined}
                                        onKeyDown={canExpand ? (e) => {
                                            if (e.key === 'Enter' || e.key === ' ') {
                                                e.preventDefault();
                                                toggleExpanded(entry.id);
                                            }
                                        } : undefined}
                                    >
                                        <span className="r-logs-ts">{formatHms(entry.timestampMs)}</span>
                                        {expanded ? <ExpandedLogLine entry={entry} /> : <span>{entry.raw}</span>}
                                        {canExpand && (
                                            <button
                                                type="button"
                                                className="r-logs-action"
                                                onClick={(e) => {
                                                    e.stopPropagation();
                                                    toggleExpanded(entry.id);
                                                    e.currentTarget.blur();
                                                }}
                                            >
                                                {actionLabel}
                                            </button>
                                        )}
                                    </div>
                                );
                            })}
                            {hasMore && (
                                <div ref={loadMoreRef} className="r-logs-loader">
                                    {loadingMore ? 'Loading older…' : ' '}
                                </div>
                            )}
                            {!hasMore && entries.length > 0 && !streaming && (
                                <div className="r-logs-loader" style={{ opacity: 0.6 }}>End of range</div>
                            )}
                        </>
                    )}
                </div>
            </PanelBody>
        </Panel>
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
    started_at?: string;
    finished_at?: string;
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

/** Returns the notable reason from a container's last_state (e.g. "OOMKilled").
 *  Falls back to "Exit N" when the previous run terminated with a non-zero exit
 *  code but Kubernetes didn't supply a named reason — so crash-loops with bare
 *  exit codes still get a visible badge without expanding the row. */
function getLastStateReason(container: ContainerStatusInfo): string | undefined {
    const last = container.last_state;
    if (last?.state_type !== 'terminated') return undefined;
    if (last.reason) return last.reason;
    if (last.exit_code !== undefined && last.exit_code !== 0) return `Exit ${last.exit_code}`;
    return undefined;
}

function PodInfoRow({ pod }: { pod: PodInfo }) {
    const [expanded, setExpanded] = useState(false);
    const detailsId = `pod-details-${pod.name.replace(/[^a-zA-Z0-9_-]/g, '-')}`;

    const isGone = pod.terminating || pod.terminated;

    // One badge per unique last_state reason across containers (e.g. OOMKilled).
    // Keep the most-recent finished_at so the pill can show how long ago it happened.
    const lastStateBadges: { reason: string; finishedAt?: string }[] = [];
    if (!isGone && pod.containers) {
        for (const c of pod.containers) {
            const reason = getLastStateReason(c);
            if (!reason) continue;
            const finishedAt = c.last_state?.finished_at;
            const existing = lastStateBadges.find(b => b.reason === reason);
            if (!existing) {
                lastStateBadges.push({ reason, finishedAt });
            } else if (finishedAt && (!existing.finishedAt || Date.parse(finishedAt) > Date.parse(existing.finishedAt))) {
                existing.finishedAt = finishedAt;
            }
        }
    }

    const phaseStatus = {
        Running: 'running',
        Pending: 'pending',
        Failed: 'failed',
        Succeeded: 'succeeded',
        Unknown: 'stopped',
    };

    const displayPhase = pod.terminated ? 'Terminated' : pod.terminating ? 'Terminating' : pod.phase;
    const statusKey = isGone ? 'stopped' : (phaseStatus[pod.phase] || 'stopped');

    return (
        <div
            style={{
                borderBottom: '1px solid var(--border-faint)',
                opacity: isGone ? 0.6 : 1,
            }}
        >
            <button
                type="button"
                onClick={() => setExpanded(!expanded)}
                aria-expanded={expanded}
                aria-controls={detailsId}
                style={{
                    all: 'unset',
                    cursor: 'pointer',
                    display: 'block',
                    width: '100%',
                    padding: '12px 16px',
                    boxSizing: 'border-box',
                }}
            >
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
                    <div style={{ flex: 1, minWidth: 0, display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                        <span className="mono" style={{ fontSize: 12, fontWeight: 600 }}>{pod.name}</span>
                        <span className={`r-status ${statusKey}`}>
                            <span className="dot" />
                            <span>{displayPhase}</span>
                        </span>
                        {lastStateBadges.map(({ reason, finishedAt }) => (
                            <span
                                key={reason}
                                className="r-status bad"
                                title={finishedAt ? formatISO8601(finishedAt) : undefined}
                            >
                                <span className="dot" />
                                <span>
                                    {reason}
                                    {finishedAt && ` · ${formatRelativeTimeRounded(finishedAt)}`}
                                </span>
                            </span>
                        ))}
                    </div>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexShrink: 0 }}>
                        <div style={{ fontSize: 11.5, color: 'var(--text-soft)', whiteSpace: 'nowrap' }}>
                            {pod.containers?.filter(c => c.ready).length || 0}/{pod.containers?.length || 0} ready
                        </div>
                        <Icon name="chev" size={12} className="chev" style={{ transform: expanded ? 'rotate(90deg)' : 'none', transition: 'transform .15s', color: 'var(--text-soft)' }} />
                    </div>
                </div>
            </button>

            {expanded && (
                <div id={detailsId} style={{ padding: '0 16px 14px', background: 'var(--surface-2)' }}>
                    {/* Container statuses */}
                    {pod.containers && pod.containers.length > 0 && (
                        <div style={{ marginBottom: 12, paddingTop: 12 }}>
                            <div style={{ fontSize: 11, fontWeight: 600, color: 'var(--text-soft)', marginBottom: 8, textTransform: 'uppercase', letterSpacing: '0.06em' }}>
                                Containers
                            </div>
                            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(320px, 1fr))', gap: 10 }}>
                                {pod.containers.map((container) => {
                                    const lastStateReason = getLastStateReason(container);
                                    const lastFinishedAt = container.last_state?.finished_at;
                                    const hasLastStateIssue = lastStateReason != null || (
                                        container.last_state?.state_type === 'terminated' &&
                                        container.last_state.exit_code !== undefined &&
                                        container.last_state.exit_code !== 0
                                    );
                                    const runningSince = container.state?.state_type === 'running' ? container.state.started_at : undefined;
                                    return (
                                        <div
                                            key={container.name}
                                            style={{
                                                fontSize: 12,
                                                padding: 10,
                                                background: 'var(--surface)',
                                                border: `1px solid ${hasLastStateIssue ? 'var(--err)' : 'var(--border-faint)'}`,
                                                borderRadius: 'var(--radius-sm)',
                                            }}
                                        >
                                            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4, gap: 8 }}>
                                                <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', minWidth: 0 }}>
                                                    <span className="mono">{container.name}</span>
                                                    {lastStateReason && (
                                                        <span
                                                            className="r-status bad"
                                                            title={lastFinishedAt ? formatISO8601(lastFinishedAt) : undefined}
                                                        >
                                                            <span className="dot" />
                                                            <span>
                                                                {lastStateReason}
                                                                {lastFinishedAt && ` · ${formatRelativeTimeRounded(lastFinishedAt)}`}
                                                            </span>
                                                        </span>
                                                    )}
                                                </div>
                                                <span style={{ color: container.ready ? 'var(--ok)' : 'var(--err)', fontWeight: 500 }}>
                                                    {container.ready ? '✓ Ready' : '✗ Not ready'}
                                                </span>
                                            </div>
                                            {container.restart_count > 0 && (
                                                <div style={{ color: 'var(--warn)' }}>
                                                    Restarts: {container.restart_count}
                                                </div>
                                            )}
                                            {container.state && (
                                                <div style={{ color: 'var(--text-muted)' }}>
                                                    State: {container.state.state_type}
                                                    {container.state.reason && ` (${container.state.reason})`}
                                                </div>
                                            )}
                                            {runningSince && (
                                                <div style={{ color: 'var(--text-muted)' }} title={formatISO8601(runningSince)}>
                                                    Started {formatRelativeTimeRounded(runningSince)}
                                                </div>
                                            )}
                                            {container.state?.message && (
                                                <div className="mono" style={{ marginTop: 4, color: 'var(--err)' }}>
                                                    {container.state.message}
                                                </div>
                                            )}
                                            {container.state?.exit_code !== undefined && (
                                                <div style={{ color: 'var(--text-muted)' }}>
                                                    Exit code: {container.state.exit_code}
                                                </div>
                                            )}
                                            {container.last_state && (
                                                <div style={{ marginTop: 8, paddingTop: 8, borderTop: '1px solid var(--border-faint)' }}>
                                                    <div style={{ fontWeight: 600, marginBottom: 4, color: 'var(--text-soft)', textTransform: 'uppercase', letterSpacing: '0.06em', fontSize: 11 }}>
                                                        Last state
                                                    </div>
                                                    <div style={{ color: hasLastStateIssue ? 'var(--err)' : 'var(--text-muted)' }}>
                                                        {container.last_state.state_type}
                                                        {container.last_state.reason && ` (${container.last_state.reason})`}
                                                    </div>
                                                    {lastFinishedAt && (
                                                        <div style={{ color: 'var(--text-muted)' }} title={formatISO8601(lastFinishedAt)}>
                                                            Ended {formatRelativeTimeRounded(lastFinishedAt)}
                                                        </div>
                                                    )}
                                                    {container.last_state.exit_code !== undefined && (
                                                        <div style={{ color: 'var(--text-muted)' }}>
                                                            Exit code: {container.last_state.exit_code}
                                                        </div>
                                                    )}
                                                    {container.last_state.message && (
                                                        <div className="mono" style={{ marginTop: 4, color: 'var(--err)' }}>
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

                    {/* Pod conditions: one pill per condition (green=True, red=False),
                        with reason/message expanded below for any False condition. */}
                    {pod.conditions && pod.conditions.length > 0 && (
                        <div style={{ marginBottom: 12 }}>
                            <div style={{ fontSize: 11, fontWeight: 600, color: 'var(--text-soft)', marginBottom: 8, textTransform: 'uppercase', letterSpacing: '0.06em' }}>
                                Conditions
                            </div>
                            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                                {pod.conditions.map((condition) => {
                                    const ok = condition.status === 'True';
                                    return (
                                        <span
                                            key={condition.type}
                                            className={`r-status ${ok ? 'ok' : 'bad'}`}
                                            title={condition.message || condition.reason || undefined}
                                        >
                                            <span className="dot" />
                                            <span>
                                                {condition.type}
                                                {!ok && ` (${condition.status})`}
                                            </span>
                                        </span>
                                    );
                                })}
                            </div>
                            {pod.conditions.filter(c => c.status !== 'True' && (c.reason || c.message)).map((condition) => (
                                <div key={`detail-${condition.type}`} style={{ fontSize: 12, color: 'var(--err)', marginTop: 6, paddingLeft: 2 }}>
                                    <span style={{ fontWeight: 600 }}>{condition.type}</span>
                                    {condition.reason && <span> · {condition.reason}</span>}
                                    {condition.message && <div className="mono" style={{ color: 'var(--text-muted)', marginTop: 2 }}>{condition.message}</div>}
                                </div>
                            ))}
                        </div>
                    )}

                    {/* Recent events */}
                    {pod.events && pod.events.length > 0 && (
                        <div>
                            <div style={{ fontSize: 11, fontWeight: 600, color: 'var(--text-soft)', marginBottom: 8, textTransform: 'uppercase', letterSpacing: '0.06em' }}>
                                Recent Events
                            </div>
                            <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                                {pod.events.map((event, idx) => (
                                    <div
                                        key={idx}
                                        className={`r-alert ${event.type === 'Error' ? 'err' : 'warn'}`}
                                        style={{ fontSize: 12, display: 'block' }}
                                    >
                                        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
                                            <span style={{ fontWeight: 600 }}>{event.reason}</span>
                                            <span style={{ color: 'var(--text-muted)' }}>
                                                {event.count > 1 && `${event.count}× `}
                                                {formatRelativeTimeRounded(event.last_timestamp)}
                                            </span>
                                        </div>
                                        <div className="mono">{event.message}</div>
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

// Scoped 1s ticker for the "Updated Xs ago" indicator so re-renders stay
// inside this tiny component instead of triggering the whole pod panel.
function UpdatedAgo({ timestamp }: { timestamp: string }) {
    const [, setTick] = useState(0);
    useEffect(() => {
        const id = setInterval(() => setTick(t => t + 1), 1000);
        return () => clearInterval(id);
    }, []);
    return (
        <span
            style={{ fontSize: 11.5, color: 'var(--text-soft)', whiteSpace: 'nowrap' }}
            title={formatISO8601(timestamp)}
        >
            Updated {formatRelativeTimeRounded(timestamp)}
        </span>
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
    let replicaTone = 'ok';
    if (podStatus.ready_replicas === 0) {
        replicaTone = 'bad';
    } else if (replicasMismatch || hasPodIssues) {
        replicaTone = 'warn';
    }

    const sectionHeading = (label: string) => (
        <div
            className="r-group-bar"
            style={{ fontWeight: 600, color: 'var(--text)', textTransform: 'uppercase', letterSpacing: '0.06em', fontSize: 11 }}
        >
            <span>{label}</span>
        </div>
    );

    return (
        <Panel>
            <PanelHead
                title="Pod status"
                right={
                    <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                        <UpdatedAgo timestamp={podStatus.last_checked} />
                        <span className={`r-status ${replicaTone === 'ok' ? 'running' : replicaTone === 'warn' ? 'warn' : 'failed'}`}>
                            <span className="dot" />
                            <span>{podStatus.ready_replicas}/{podStatus.desired_replicas} ready</span>
                        </span>
                    </div>
                }
            />
            <PanelBody style={{ padding: 0 }}>
                {hasIssues && (
                    <div className="r-alert warn" style={{ margin: 16, fontSize: 12.5 }}>
                        <Icon name="info" size={14} />
                        <div style={{ flex: 1 }}>
                            {activePods.length} active{inactivePods.length > 0 && ` · ${inactivePods.length} previous`} ·
                            {replicasMismatch ? ' replica count below desired' : ' pod-level issues detected'}.
                        </div>
                    </div>
                )}

                {/* Active pods */}
                {activePods.length > 0 && (
                    <>
                        {sectionHeading(`Active (${activePods.length})`)}
                        <div>
                            {activePods.map((pod, idx) => (
                                <PodInfoRow key={pod.name || `pod-${idx}`} pod={pod} />
                            ))}
                        </div>
                    </>
                )}

                {/* Terminating / terminated pods */}
                {inactivePods.length > 0 && (
                    <>
                        {sectionHeading(`Previous (${inactivePods.length})`)}
                        <div>
                            {inactivePods.map((pod, idx) => (
                                <PodInfoRow key={pod.name || `prev-${idx}`} pod={pod} />
                            ))}
                        </div>
                    </>
                )}
            </PanelBody>
        </Panel>
    );
}

// Resolves a stable URL — the active deployment of an environment (via its
// primary deployment group) or of an explicit deployment group — to the
// concrete deployment, then renders the deployment detail.
export function EnvironmentDeploymentView({ projectName, environmentName, groupName }) {
    const [activeDeploymentId, setActiveDeploymentId] = useState(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);

    useEffect(() => {
        let cancelled = false;
        async function resolve() {
            try {
                let group = groupName || null;
                if (!group) {
                    // Environment URL: resolve the environment's primary group.
                    const envs = await api.getProjectEnvironments(projectName);
                    group = (envs || []).find((e) => e.name === environmentName)?.primary_deployment_group || null;
                }
                const deployments = await api.getProjectDeployments(projectName, { limit: 100 });
                const active = deployments.find(
                    (d) => d.environment === environmentName
                        && d.is_active
                        && (!group || (d.deployment_group || 'default') === group)
                );
                if (!cancelled) {
                    setActiveDeploymentId(active ? active.deployment_id : null);
                    setLoading(false);
                }
            } catch (err) {
                if (!cancelled) { setError(err.message); setLoading(false); }
            }
        }
        resolve();
        return () => { cancelled = true; };
    }, [projectName, environmentName, groupName]);

    if (loading) return <LoadingState label="Loading deployment…" />;
    if (error) return <ErrorState message={`Error: ${error}`} />;
    if (!activeDeploymentId) {
        const scope = groupName
            ? <>the <strong>{groupName}</strong> group of <strong>{environmentName}</strong></>
            : <>the <strong>{environmentName}</strong> environment</>;
        return (
            <div>
                <p style={{ color: 'var(--text-muted)', marginBottom: 16 }}>
                    No active deployment in {scope}.
                </p>
                <RButton variant="default" size="sm" onClick={() => navigate(`/project/${projectName}/environments`)}>
                    Back to Environments
                </RButton>
            </div>
        );
    }

    return <DeploymentDetail projectName={projectName} deploymentId={activeDeploymentId} />;
}

export function DeploymentDetail({ projectName, deploymentId }) {
    const [deployment, setDeployment] = useState(null);
    const [environments, setEnvironments] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [rollbackDialogOpen, setRollbackDialogOpen] = useState(false);
    const [rolling, setRolling] = useState(false);
    const [useSourceEnvVars, setUseSourceEnvVars] = useState(false);
    const [stopDialogOpen, setStopDialogOpen] = useState(false);
    const [stopping, setStopping] = useState(false);
    const [detailActionStatus, setDetailActionStatus] = useState('');
    const [activeTab, setActiveTab] = useState('logs');
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

    // Best-effort fetch of environments so we can flag the group pill as
    // primary when the deployment's group matches the env's primary group.
    useEffect(() => {
        let cancelled = false;
        api.getProjectEnvironments(projectName)
            .then((envs) => { if (!cancelled) setEnvironments(Array.isArray(envs) ? envs : []); })
            .catch(() => {});
        return () => { cancelled = true; };
    }, [projectName]);

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

    const podStatus = deployment.controller_metadata?.pod_status;
    // Prefer the backend-deduplicated all_urls (deployment-group URL, env URL,
    // production URL, custom domains). Fall back to the older fields for stale
    // payloads; dedupe so a custom domain set as primary isn't shown twice.
    const allDomains = (deployment.all_urls && deployment.all_urls.length > 0)
        ? deployment.all_urls
        : Array.from(new Set([
              deployment.primary_url,
              ...(deployment.custom_domain_urls || []),
          ].filter(Boolean)));

    const tabs = [
        { id: 'logs', label: 'Logs' },
        { id: 'timeline', label: 'Timeline', count: timeline.length },
        ...(podStatus ? [{ id: 'pods', label: 'Pods', count: podStatus.pods?.length || 0 }] : []),
        ...(deployment.build_logs ? [{ id: 'build', label: 'Build output' }] : []),
        { id: 'env', label: 'Environment' },
    ];

    const buildKv = (
        <Panel>
            <PanelHead title="Deploy" />
            <PanelBody>
                <KV>
                    <KVRow k="Image">
                        <span className="mono" style={{ fontSize: 12, wordBreak: 'break-all' }}>{deployment.image || '-'}</span>
                    </KVRow>
                    {deployment.image_digest && (
                        <KVRow k="Digest">
                            <span className="mono" style={{ fontSize: 12, wordBreak: 'break-all' }}>{deployment.image_digest}</span>
                        </KVRow>
                    )}
                    {deployment.http_port ? (
                        <KVRow k="HTTP port">
                            <span className="mono" style={{ fontSize: 12 }}>{deployment.http_port}</span>
                        </KVRow>
                    ) : null}
                    <KVRow k="Created by">{deployment.created_by_email || '-'}</KVRow>
                    <KVRow k="Started">
                        <span title={formatISO8601(deployment.created)}>{formatRelativeTimeRounded(deployment.created)}</span>
                    </KVRow>
                    <KVRow k="Completed">{deployment.completed_at ? formatDate(deployment.completed_at) : '-'}</KVRow>
                    {deployment.expires_at && (
                        <KVRow k="Expires">{formatTimeRemaining(deployment.expires_at)}</KVRow>
                    )}
                    {(deployment.job_url || deployment.pull_request_url) && (
                        <KVRow k="Source">
                            <SourceLinkGroup jobUrl={deployment.job_url} prUrl={deployment.pull_request_url} />
                        </KVRow>
                    )}
                    {deployment.git_repository_url && (
                        <KVRow k="Repository">
                            {isSafeUrl(deployment.git_repository_url) ? (
                                <a
                                    className="r-link mono"
                                    style={{ fontSize: 12.5, wordBreak: 'break-all' }}
                                    href={deployment.git_repository_url}
                                    target="_blank"
                                    rel="noopener noreferrer"
                                >
                                    {stripUrlScheme(deployment.git_repository_url)}
                                </a>
                            ) : (
                                <span className="mono" style={{ fontSize: 12.5, wordBreak: 'break-all' }}>
                                    {deployment.git_repository_url}
                                </span>
                            )}
                        </KVRow>
                    )}
                </KV>
            </PanelBody>
        </Panel>
    );

    const runtimeKv = (
        <Panel>
            <PanelHead title="Resources" />
            <PanelBody>
                <KV>
                    <KVRow k="Replicas">{deployment.replicas}</KVRow>
                    <KVRow k="CPU">{deployment.cpu}</KVRow>
                    <KVRow k="Memory">{deployment.memory}</KVRow>
                </KV>
            </PanelBody>
        </Panel>
    );

    const routingPanel = (
        <Panel>
            <PanelHead title="Routing" />
            <PanelBody style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                {allDomains.length > 0 ? (
                    allDomains.map((url) => (
                        isSafeUrl(url) ? (
                            <a
                                key={url}
                                href={url}
                                target="_blank"
                                rel="noopener noreferrer"
                                className="r-link mono"
                                style={{ fontSize: 12.5, wordBreak: 'break-all' }}
                            >
                                {url.replace(/^https?:\/\//, '')}
                            </a>
                        ) : (
                            <span
                                key={url}
                                className="mono"
                                style={{ fontSize: 12.5, wordBreak: 'break-all' }}
                            >
                                {url.replace(/^https?:\/\//, '')}
                            </span>
                        )
                    ))
                ) : (
                    <span style={{ fontSize: 12.5, color: 'var(--text-soft)' }}>No domains configured.</span>
                )}
            </PanelBody>
        </Panel>
    );

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
                        <h1 className="r-page-title mono" style={{ fontSize: 20 }}>{deployment.deployment_id}</h1>
                        <Status status={deployment.status} tooltip={`Deployment status: ${deployment.status}`} />
                        {deployment.environment ? (
                            <EnvPill
                                env={deployment.environment}
                                color={deployment.environment_color}
                                tooltip={
                                    <>
                                        <div>Environment: <span className="mono">{deployment.environment}</span></div>
                                        <div>
                                            Deployment Group: <span className="mono">{deployment.deployment_group}</span>
                                            {deployment.deployment_group === 'default' && ' (primary group)'}
                                        </div>
                                    </>
                                }
                            />
                        ) : null}
                        {deployment.deployment_group && (() => {
                            const env = environments.find((e) => e.name === deployment.environment);
                            const isPrimaryGroup = !!env && env.primary_deployment_group === deployment.deployment_group;
                            return (
                                <GroupPill
                                    group={deployment.deployment_group}
                                    primary={isPrimaryGroup}
                                    tooltip={
                                        <>
                                            <div>Deployment group: <span className="mono">{deployment.deployment_group}</span></div>
                                            {isPrimaryGroup && <div>Primary group for the {deployment.environment} environment.</div>}
                                        </>
                                    }
                                />
                            );
                        })()}
                    </div>
                    <div className="r-meta-bar" style={{ marginTop: 8 }}>
                        <span>{projectName}</span>
                        <span className="dot-sep" />
                        <span>by {deployment.created_by_email || 'unknown'}</span>
                        <span className="dot-sep" />
                        <span title={formatISO8601(deployment.created)}>{formatRelativeTimeRounded(deployment.created)}</span>
                        {deployment.completed_at && (
                            <>
                                <span className="dot-sep" />
                                <span>completed {formatDate(deployment.completed_at)}</span>
                            </>
                        )}
                    </div>
                </div>
                <RButton icon="copy" onClick={() => handleCopy(deployment.deployment_id, 'Deployment ID')}>
                    Copy ID
                </RButton>
                {deployment.can_rollback && (
                    <RButton icon="refresh" onClick={handleRollbackClick}>
                        {deployment.is_active ? 'Redeploy' : 'Rollback'}
                    </RButton>
                )}
                {!isTerminal(deployment.status) && (
                    <RButton variant="danger" icon="stop" onClick={() => setStopDialogOpen(true)}>
                        Stop
                    </RButton>
                )}
            </div>

            {detailActionStatus && (
                <div className="r-alert info" style={{ marginBottom: 18, fontSize: 12.5 }}>
                    <Icon name="info" size={14} />
                    <div style={{ flex: 1 }}>{detailActionStatus}</div>
                </div>
            )}

            {deployment.error_message && (
                <div className="r-alert err" style={{ marginBottom: 18, fontSize: 12.5 }}>
                    <Icon name="info" size={14} />
                    <div style={{ flex: 1 }}>Error: {deployment.error_message}</div>
                </div>
            )}

            <Tabs tabs={tabs} active={activeTab} onChange={setActiveTab} />

            {activeTab === 'logs' && (
                <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 320px', gap: 18, alignItems: 'start' }}>
                    <DeploymentLogs projectName={projectName} deploymentId={deploymentId} deploymentStatus={deployment.status} />
                    <div className="r-stack">
                        {buildKv}
                        {runtimeKv}
                        {routingPanel}
                    </div>
                </div>
            )}

            {activeTab === 'timeline' && (
                <Panel>
                    <PanelHead title="Deployment timeline" />
                    <PanelBody style={{ display: 'flex', flexDirection: 'column', gap: 18 }}>
                        {groupedTimeline.length === 0 ? (
                            <Empty>No timeline events recorded.</Empty>
                        ) : (
                            groupedTimeline.map((group) => (
                                <div key={group.phase}>
                                    <div
                                        style={{
                                            fontSize: 11,
                                            fontWeight: 600,
                                            color: 'var(--text-soft)',
                                            textTransform: 'uppercase',
                                            letterSpacing: '0.06em',
                                            marginBottom: 8,
                                        }}
                                    >
                                        {group.phase}
                                    </div>
                                    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
                                        {group.events.map((event, idx) => (
                                            <div
                                                key={`${group.phase}-${idx}`}
                                                style={{
                                                    display: 'flex',
                                                    alignItems: 'baseline',
                                                    gap: 14,
                                                    fontSize: 12.5,
                                                    padding: '6px 0',
                                                }}
                                            >
                                                <span className="mono" style={{ color: 'var(--text-soft)', flex: '0 0 170px' }}>
                                                    {formatDate(event.ts || '')}
                                                </span>
                                                <span style={{ flex: 1 }}>{event.label}</span>
                                                <span className="mono" style={{ color: 'var(--text-muted)' }}>+{event.delta}</span>
                                            </div>
                                        ))}
                                    </div>
                                </div>
                            ))
                        )}
                    </PanelBody>
                </Panel>
            )}

            {activeTab === 'pods' && podStatus && (
                <PodStatusSection podStatus={podStatus} />
            )}

            {activeTab === 'build' && deployment.build_logs && (
                <Panel>
                    <PanelHead title="Build output" />
                    <PanelBody>
                        <div className="r-logs" style={{ maxHeight: 480 }}>
                            {deployment.build_logs.split('\n').map((line, idx) => (
                                <div key={idx} style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>{line}</div>
                            ))}
                        </div>
                    </PanelBody>
                </Panel>
            )}

            {activeTab === 'env' && (
                <Panel>
                    <PanelHead title="Environment variables" sub="Snapshot captured for this deployment" />
                    <PanelBody>
                        <EnvVarsList projectName={projectName} deploymentId={deploymentId} />
                    </PanelBody>
                </Panel>
            )}

            <Modal
                isOpen={rollbackDialogOpen}
                onClose={() => {
                    setRollbackDialogOpen(false);
                    setUseSourceEnvVars(false);
                }}
                title={deployment?.is_active ? 'Redeploy' : 'Rollback to Deployment'}
                footer={
                    <>
                        <RButton
                            onClick={() => {
                                setRollbackDialogOpen(false);
                                setUseSourceEnvVars(false);
                            }}
                            disabled={rolling}
                        >
                            Cancel
                        </RButton>
                        <RButton
                            variant="primary"
                            onClick={handleRollback}
                            loading={rolling}
                            disabled={rolling}
                        >
                            {deployment?.is_active ? 'Redeploy' : 'Rollback'}
                        </RButton>
                    </>
                }
            >
                <p style={{ fontSize: 13, color: 'var(--text-muted)', margin: 0, lineHeight: 1.6 }}>
                    {deployment?.is_active
                        ? `Are you sure you want to redeploy ${deploymentId}? This will create a new deployment with the same image.`
                        : `Are you sure you want to rollback to deployment ${deploymentId}? This will create a new deployment with the same image.`}
                </p>

                <div style={{ background: 'var(--surface-2)', border: '1px solid var(--border-faint)', borderRadius: 'var(--radius-sm)', padding: 14 }}>
                    <label style={{ display: 'flex', alignItems: 'flex-start', gap: 10, cursor: 'pointer' }}>
                        <input
                            type="checkbox"
                            checked={useSourceEnvVars}
                            onChange={(e) => setUseSourceEnvVars(e.target.checked)}
                            style={{ marginTop: 2 }}
                        />
                        <div style={{ flex: 1 }}>
                            <div style={{ fontSize: 13, fontWeight: 500, color: 'var(--text)' }}>
                                Use source deployment's environment variables
                            </div>
                            <div style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 4 }}>
                                {useSourceEnvVars
                                    ? 'Will copy environment variables from the source deployment'
                                    : "Will use the current project's environment variables (default)"}
                            </div>
                        </div>
                    </label>
                </div>
            </Modal>

            <ConfirmDialog
                isOpen={stopDialogOpen}
                onClose={() => setStopDialogOpen(false)}
                onConfirm={handleStopConfirm}
                title="Stop Deployment"
                message={`Are you sure you want to stop deployment ${deploymentId}? Impact: traffic for group "${deployment?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop Deployment"
                confirmTone="danger"
                loading={stopping}
            />
        </section>
    );
}
