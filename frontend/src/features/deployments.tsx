import { Fragment, lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { navigate, useQueryParam } from '../lib/navigation';
import { copyToClipboard, formatDate, formatISO8601, formatRelativeTimeRounded, formatTimeRemaining, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { usePolling } from '../lib/polling';
import { useToast } from '../components/toast';
import { MonoSortButton, MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { Button as RButton, Combobox, ConfirmDialog, ENV_COLOR_STYLES, Empty, EnvPill, EnvironmentColorDot, GroupPill, KV, KVRow, Modal, Panel, PanelBody, PanelHead, Pill, SearchInput, Segmented, SourceLinkGroup, SourceLinkGroupAction, Status, Tabs } from '../components/r-ui';
import { Icon } from '../components/icon';
import { EnvVarsList } from './resources';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { LogConsole } from './logs/log-console';
import { ContainerStatusPanel } from './logs/container-status';
import { EventTimeline } from './logs/event-timeline';
import { fetchDeploymentEvents, type DeploymentEvent } from './logs/api';

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

// ── CPU / memory parsing helpers for the multi-container resource breakdown ──
// CPU is stored as the same string K8s accepts (`"500m"`, `"1"`, `"1.5"`).
// Memory is the same — IEC suffixes (Ki/Mi/Gi/...) plus the SI suffixes K/M/G.
// A value may also be a `request-limit` range (`"128m-1"`, `"256Mi-1Gi"`),
// mirroring the backend's `split_request_limit` in
// `src/server/deployment/quantity.rs`: a bare value means request == limit, a
// `req-lim` value carries both sides.
// Aggregation goes through (millicores, bytes), then re-formats for display so
// "500m × 2 replicas + 1 × 3 replicas" prints as "4" (not "4000m") and "256Mi
// × 4 + 1Gi" prints as a single readable value.

// Split a fixed-or-range value into its `[request, limit]` sides. A bare value
// yields request == limit; a single `-` separates the two. Like the Rust
// `split_request_limit`, real CPU/memory quantities never contain a `-`, so
// splitting on it is safe (an exotic `"1e-3"` would split into unparseable
// halves and fall through to 0, which is acceptable for a display total).
function splitRequestLimit(s: string): [string, string] {
    const parts = s.split('-');
    if (parts.length === 2) return [parts[0].trim(), parts[1].trim()];
    return [s, s];
}

function parseCpuToMillicores(s: string | null | undefined): number {
    if (s == null) return 0;
    const t = String(s).trim();
    if (!t) return 0;
    if (t.endsWith('m')) {
        const v = parseFloat(t.slice(0, -1));
        return Number.isFinite(v) ? v : 0;
    }
    const v = parseFloat(t);
    return Number.isFinite(v) ? v * 1000 : 0;
}

function formatMillicoresAsCpu(milli: number): string {
    if (!Number.isFinite(milli) || milli <= 0) return '0';
    if (milli % 1000 === 0) return String(milli / 1000);
    return `${Math.round(milli)}m`;
}

const MEM_UNIT_FACTORS = {
    Ki: 1024,
    Mi: 1024 ** 2,
    Gi: 1024 ** 3,
    Ti: 1024 ** 4,
    Pi: 1024 ** 5,
    Ei: 1024 ** 6,
    K: 1e3,
    M: 1e6,
    G: 1e9,
    T: 1e12,
    P: 1e15,
    E: 1e18,
};

function parseMemoryToBytes(s: string | null | undefined): number {
    if (s == null) return 0;
    const t = String(s).trim();
    if (!t) return 0;
    const m = t.match(/^(\d+(?:\.\d+)?)\s*(Ki|Mi|Gi|Ti|Pi|Ei|K|M|G|T|P|E)?$/);
    if (!m) return 0;
    const v = parseFloat(m[1]);
    if (!Number.isFinite(v)) return 0;
    const factor = m[2] ? MEM_UNIT_FACTORS[m[2]] : 1;
    return v * factor;
}

function formatBytesAsMemory(bytes: number): string {
    if (!Number.isFinite(bytes) || bytes <= 0) return '0';
    // Render in the largest IEC unit that divides the total exactly, so the
    // breakdown stays precise (no rounding) regardless of mixed inputs. Memory
    // specs are practically always Mi/Gi-aligned, so this is Gi or Mi in almost
    // every case; Ki (then raw bytes) is the exact fallback for a rarer
    // finer-grained total — never the lossy decimal-Gi the previous code used.
    const Gi = 1024 ** 3;
    const Mi = 1024 ** 2;
    const Ki = 1024;
    if (bytes % Gi === 0) return `${bytes / Gi}Gi`;
    if (bytes % Mi === 0) return `${bytes / Mi}Mi`;
    if (bytes % Ki === 0) return `${bytes / Ki}Ki`;
    return `${bytes}`;
}

/**
 * Sum (replicas × per-container cpu/memory) across the deployment's containers.
 *
 * Per-container `cpu`/`memory` may be a `request-limit` range. The "Resources"
 * panel and breakdown header render this as the deployment's resource footprint
 * (heading is just "CPU" / "Memory"), so we aggregate the *limit* side — the
 * ceiling the deployment can consume. A fixed value has request == limit, so
 * this is unchanged for the common case.
 */
function aggregateContainerResources(
    containers: Array<{ replicas?: number | string | null; cpu?: string | null; memory?: string | null }>,
): { replicas: number; cpu: string; memory: string } {
    let replicas = 0;
    let cpuMilli = 0;
    let memBytes = 0;
    for (const c of containers) {
        const r = Number(c.replicas) || 0;
        replicas += r;
        const [, cpuLimit] = splitRequestLimit(String(c.cpu ?? ''));
        const [, memLimit] = splitRequestLimit(String(c.memory ?? ''));
        cpuMilli += parseCpuToMillicores(cpuLimit) * r;
        memBytes += parseMemoryToBytes(memLimit) * r;
    }
    return {
        replicas,
        cpu: formatMillicoresAsCpu(cpuMilli),
        memory: formatBytesAsMemory(memBytes),
    };
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
            const params: { limit: number; offset: number; group?: string } = {
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


/**
 * Shows the latest backend resource representation for each container.
 *
 * This is keyed by the event contract rather than a backend name: any backend
 * that reports `type: resource_adjusted` gets the same notice. The event log is
 * the source of this fact because the deployment row retains the requested
 * resources and controller metadata is private bookkeeping.
 */
function ResourceAdjustmentNotice({
    projectName,
    deploymentId,
    deploymentStatus,
}: {
    projectName: string;
    deploymentId: string;
    deploymentStatus: string;
}) {
    const [events, setEvents] = useState<DeploymentEvent[]>([]);

    useEffect(() => {
        const controller = new AbortController();
        void fetchDeploymentEvents({
            projectName,
            deploymentId,
            kinds: ['backend_event'],
            minSeverity: 'all',
            limit: 500,
            signal: controller.signal,
        })
            .then((page) => setEvents(page.events))
            .catch((error) => {
                if (error instanceof Error && error.name === 'AbortError') return;
                setEvents([]);
            });
        return () => controller.abort();
    }, [projectName, deploymentId, deploymentStatus]);

    const latestByContainer = new Map<string, DeploymentEvent>();
    for (const event of events) {
        if (event.attributes?.type !== 'resource_adjusted') continue;
        const container = typeof event.attributes.container === 'string'
            ? event.attributes.container
            : event.subject || 'deployment';
        if (!latestByContainer.has(container)) latestByContainer.set(container, event);
    }

    if (latestByContainer.size === 0) return null;

    return (
        <div className="r-alert warn" style={{ marginBottom: 16, fontSize: 12.5 }}>
            <Icon name="info" size={14} />
            <div style={{ flex: 1 }}>
                <div style={{ fontWeight: 600, marginBottom: 4 }}>Resources adjusted by the backend</div>
                {[...latestByContainer.entries()].map(([container, event]) => (
                    <div key={`${container}-${event.id}`}>
                        <span className="mono">{container}</span>{': '}
                        CPU <span className="mono">{attributeText(event, 'requested_cpu')}</span>
                        {' → '}
                        <span className="mono">{attributeText(event, 'resolved_cpu_units')} units</span>
                        {', memory '}
                        <span className="mono">{attributeText(event, 'requested_memory')}</span>
                        {' → '}
                        <span className="mono">{attributeText(event, 'resolved_memory_mib')} MiB</span>
                    </div>
                ))}
            </div>
        </div>
    );
}

function attributeText(event: DeploymentEvent, key: string): string {
    const value = event.attributes?.[key];
    return value === undefined || value === null ? '-' : String(value);
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



/**
 * The controller's own bookkeeping, rendered verbatim.
 *
 * Each deployment backend writes whatever it needs to track convergence, and
 * the shape is its business alone — it changes when the controller changes, with
 * no compatibility promise. This is deliberately a JSON dump rather than a
 * parsed view: parsing it here would turn an internal note into an interface,
 * and the interface for what happened to a deployment is its event log.
 */
function ControllerMetadataSection({ metadata }: { metadata: unknown }) {
    return (
        <Panel>
            <PanelHead
                title="Controller metadata"
                sub="Internal bookkeeping — shape is not stable"
            />
            <PanelBody>
                <pre className="r-ctrl-meta">{JSON.stringify(metadata, null, 2)}</pre>
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
    // Pushed, not replaced: switching tabs is a move the Back button undoes.
    const [tabParam, setTabParam] = useQueryParam('tab', { history: 'push' });
    const [breakdownOpen, setBreakdownOpen] = useState(false);
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

    // Opaque by contract: whatever the controller is tracking, shown verbatim.
    // Nothing here reads into it — see `ControllerMetadataSection`.
    const controllerMetadata = deployment.controller_metadata;
    const hasControllerMetadata =
        !!controllerMetadata && Object.keys(controllerMetadata).length > 0;
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
        // No count: the timeline's length is only known once the event log
        // is fetched, and the tab should not block on that to render.
        { id: 'timeline', label: 'Timeline' },
        // No count: the number of replicas is only known once the snapshot is
        // fetched, and the tab should not block on that to render.
        { id: 'containers', label: 'Containers' },
        ...(hasControllerMetadata ? [{ id: 'controller', label: 'Controller' }] : []),
        ...(deployment.build_logs ? [{ id: 'build', label: 'Build output' }] : []),
        { id: 'variables', label: 'Variables' },
    ];

    // A `?tab=` naming a tab this deployment has no data for — a build-output
    // link to a deployment with no build logs — falls back rather than showing
    // an empty page. Writing `null` for the default keeps a plain link clean.
    const activeTab = tabs.some((t) => t.id === tabParam) ? (tabParam as string) : 'logs';
    const setActiveTab = (id: string) => setTabParam(id === 'logs' ? null : id);

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

    const containers = Array.isArray(deployment.containers) ? deployment.containers : null;
    const isMultiContainer = !!containers && containers.length > 0;
    // Populates the log console's container filter without a second request.
    const containerNames = (containers || []).map((c) => c.name).filter(Boolean);
    const totals = isMultiContainer
        ? aggregateContainerResources(containers)
        : { replicas: deployment.replicas, cpu: deployment.cpu, memory: deployment.memory };

    const runtimeKv = (
        <Panel>
            <PanelHead
                title="Resources"
                right={
                    isMultiContainer ? (
                        <RButton size="sm" onClick={() => setBreakdownOpen(true)}>
                            Breakdown
                        </RButton>
                    ) : null
                }
            />
            <PanelBody>
                <ResourceAdjustmentNotice
                    projectName={projectName}
                    deploymentId={deploymentId}
                    deploymentStatus={deployment.status}
                />
                <KV>
                    <KVRow k="Replicas">
                        {totals.replicas}
                        {isMultiContainer && (
                            <span style={{ color: 'var(--text-soft)', marginLeft: 6 }}>
                                across {containers.length} container{containers.length === 1 ? '' : 's'}
                            </span>
                        )}
                    </KVRow>
                    <KVRow k="CPU">{totals.cpu}</KVRow>
                    <KVRow k="Memory">{totals.memory}</KVRow>
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
                <LogConsole
                    projectName={projectName}
                    deploymentId={deploymentId}
                    deploymentStatus={deployment.status}
                    deploymentCompletedAt={deployment.completed_at}
                    deploymentCreated={deployment.created}
                    containers={containerNames}
                    lead={
                        <a
                            className="r-logc-expand"
                            href={`/deployment/${projectName}/${deploymentId}/logs`}
                            onClick={(e) => {
                                if (e.metaKey || e.ctrlKey || e.shiftKey) return;
                                e.preventDefault();
                                navigate(`/deployment/${projectName}/${deploymentId}/logs`);
                            }}
                        >
                            <Icon name="ext" size={12} />
                            Full screen
                        </a>
                    }
                    details={(
                        <>
                            {buildKv}
                            {runtimeKv}
                            {routingPanel}
                        </>
                    )}
                />
            )}

            {activeTab === 'containers' && (
                <ContainerStatusPanel
                    projectName={projectName}
                    deploymentId={deploymentId}
                    deploymentStatus={deployment.status}
                />
            )}

            {activeTab === 'timeline' && (
                <EventTimeline
                    projectName={projectName}
                    deploymentId={deploymentId}
                    deploymentStatus={deployment.status}
                />
            )}

            {activeTab === 'controller' && hasControllerMetadata && (
                <ControllerMetadataSection metadata={controllerMetadata} />
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

            {activeTab === 'variables' && (
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

            <Modal
                isOpen={breakdownOpen}
                onClose={() => setBreakdownOpen(false)}
                title="Resource breakdown"
                sub={`${containers ? containers.length : 0} container${containers && containers.length === 1 ? '' : 's'} · totals: ${totals.replicas} replicas, ${totals.cpu} CPU, ${totals.memory} memory`}
                width="wide"
                footer={
                    <RButton onClick={() => setBreakdownOpen(false)}>Close</RButton>
                }
            >
                {containers && containers.length > 0 ? (
                    <MonoTableFrame>
                        <MonoTable>
                            <MonoTableHead>
                                <MonoTableRow>
                                    <MonoTh>Container</MonoTh>
                                    <MonoTh style={{ textAlign: 'right' }}>Replicas</MonoTh>
                                    <MonoTh style={{ textAlign: 'right' }}>CPU</MonoTh>
                                    <MonoTh style={{ textAlign: 'right' }}>Memory</MonoTh>
                                    <MonoTh style={{ textAlign: 'right' }}>HTTP port</MonoTh>
                                </MonoTableRow>
                            </MonoTableHead>
                            <MonoTableBody>
                                {containers.map((c) => (
                                    <MonoTableRow key={c.name}>
                                        <MonoTd>
                                            <span className="mono">{c.name}</span>
                                        </MonoTd>
                                        <MonoTd style={{ textAlign: 'right' }}>{c.replicas ?? '-'}</MonoTd>
                                        <MonoTd style={{ textAlign: 'right' }}>{c.cpu || '-'}</MonoTd>
                                        <MonoTd style={{ textAlign: 'right' }}>{c.memory || '-'}</MonoTd>
                                        <MonoTd style={{ textAlign: 'right' }}>{c.port ?? '-'}</MonoTd>
                                    </MonoTableRow>
                                ))}
                            </MonoTableBody>
                        </MonoTable>
                    </MonoTableFrame>
                ) : (
                    <Empty>No container breakdown available.</Empty>
                )}
            </Modal>
        </section>
    );
}
