import React, { Fragment, lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { navigate, useQueryParam } from '../lib/navigation';
import { copyToClipboard, formatDate, formatISO8601, formatRelativeTimeRounded, formatTimeRemaining, isSafeUrl, stripUrlScheme } from '../lib/utils';
import { usePolling } from '../lib/polling';
import { useToast } from '../components/toast';
import { MonoSortButton, MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { Button as RButton, Combobox, ConfirmDialog, ENV_COLOR_STYLES, Empty, EnvPill, EnvTag, EnvironmentColorDot, GroupPill, KV, KVRow, Modal, Panel, PanelBody, PanelHead, Pill, SearchInput, Segmented, SourceLinkGroup, SourceLinkGroupAction, Status, Tabs } from '../components/r-ui';
import { Icon } from '../components/icon';
import { EnvVarsList } from './resources';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { LogConsole } from './logs/log-console';
import { ContainerStatusPanel } from './logs/container-status';
import { EventTimeline } from './logs/event-timeline';
import { fetchDeploymentEvents, type DeploymentEvent } from './logs/api';
import { useCurrentProject, type Deployment } from '../lib/project-context';
import { useDeployActions } from '../components/deploy-actions';
import { byNewest, deploymentSource, isInProgress, isTerminal, promoteTargets, sortEnvironments } from '../lib/env-state';
import { DeploymentMetaStrip, FailurePanel, RolloutPanel, rolloutEnd, rolloutSteps, useDeploymentEvents } from './deployment-diagnosis';
import { useIsMobile } from '../lib/use-media';

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


export function DeploymentsList({ projectName }: { projectName: string }) {
    const [deployments, setDeployments] = useState<Deployment[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [pageParam, setPageParam] = useQueryParam('page');
    const page = Math.max(0, Number.parseInt(pageParam || '0', 10) || 0);
    const setPage = (value: number) => setPageParam(value > 0 ? String(value) : null);
    const [hasMore, setHasMore] = useState(true);
    const [groupParam, setGroupFilter] = useQueryParam('group');
    const groupFilter = groupParam || '';
    const [deploymentGroups, setDeploymentGroups] = useState<string[]>([]);
    const project = useCurrentProject();
    const environments = useMemo(() => sortEnvironments(project?.environments ?? []), [project?.environments]);
    const [envParam, setEnvFilter] = useQueryParam('env');
    const envFilter = envParam || '';
    const [statusParam, setStatusParam] = useQueryParam('status');
    const statusFilter = statusParam || 'all';
    const setStatusFilter = (v: string) => setStatusParam(v === 'all' ? null : v);
    const [searchParam, setSearch] = useQueryParam('search');
    const search = searchParam || '';
    const [deploymentToStop, setDeploymentToStop] = useState<Deployment | null>(null);
    const [stopping, setStopping] = useState(false);
    const { request } = useDeployActions();
    const { showToast } = useToast();
    const isMobile = useIsMobile();
    const pageSize = 20;

    useEffect(() => {
        api.getDeploymentGroups(projectName)
            .then((groups: string[]) => setDeploymentGroups(groups || []))
            .catch((err: unknown) => console.error('Failed to load deployment groups:', err));
    }, [projectName]);

    const loadDeployments = useCallback(async () => {
        try {
            const params: { limit: number; offset: number; group?: string } = {
                limit: pageSize,
                offset: page * pageSize,
            };
            if (groupFilter) params.group = groupFilter;
            const data: Deployment[] = await api.getProjectDeployments(projectName, params);
            setDeployments(data);
            setHasMore(data.length >= pageSize);
            setError(null);
        } catch (err) {
            setError((err as Error).message);
        } finally {
            setLoading(false);
        }
    }, [projectName, page, groupFilter]);

    useEffect(() => { loadDeployments(); }, [loadDeployments]);
    useEffect(() => {
        window.addEventListener('rise:mutation', loadDeployments);
        return () => window.removeEventListener('rise:mutation', loadDeployments);
    }, [loadDeployments]);

    // Auto-refresh every 5 seconds, paused when the tab is hidden.
    usePolling(loadDeployments, 5000);

    const sorted = useMemo(() => [...deployments].sort(byNewest), [deployments]);

    // Per environment+group: is something live there? A superseded deployment
    // can only be rolled back to when its group still has a live deployment.
    const liveIn = useMemo(() => {
        const set = new Set<string>();
        for (const d of project?.deployments ?? []) if (d.is_active) set.add(`${d.environment}/${d.deployment_group}`);
        for (const d of deployments) if (d.is_active) set.add(`${d.environment}/${d.deployment_group}`);
        return set;
    }, [project?.deployments, deployments]);

    const handleStopConfirm = async () => {
        if (!deploymentToStop) return;
        setStopping(true);
        try {
            await api.stopDeployment(projectName, deploymentToStop.deployment_id);
            showToast(`Stopping ${deploymentToStop.deployment_id}`, 'success');
            setDeploymentToStop(null);
            window.dispatchEvent(new Event('rise:mutation'));
        } catch (err) {
            showToast(`Failed to stop deployment: ${(err as Error).message}`, 'error');
        } finally {
            setStopping(false);
        }
    };

    if (loading && deployments.length === 0) return <LoadingState label="Loading deployments..." />;
    if (error && deployments.length === 0) return <ErrorState message={`Error loading deployments: ${error}`} onRetry={loadDeployments} />;

    const matchesStatus = (d: Deployment) => {
        switch (statusFilter) {
            case 'active': return !isTerminal(d.status);
            case 'healthy': return d.status === 'Healthy';
            case 'unhealthy': return d.status === 'Unhealthy';
            case 'failed': return d.status === 'Failed';
            default: return true;
        }
    };
    const q = search.trim().toLowerCase();
    const filtered = sorted
        .filter(d => !envFilter || d.environment === envFilter)
        .filter(matchesStatus)
        .filter(d => !q || `${d.deployment_id} ${d.image || ''} ${d.created_by_email || ''} ${d.deployment_group}`.toLowerCase().includes(q));

    const actionFor = (d: Deployment): React.ReactNode => {
        if (!d.can_rollback) return null;
        if (d.is_active) {
            return (
                <button type="button" className="r-row-action" onClick={(e) => { e.stopPropagation(); request({ kind: 'redeploy', source: d }); }}>
                    Redeploy
                </button>
            );
        }
        if (d.status === 'Superseded' && liveIn.has(`${d.environment}/${d.deployment_group}`)) {
            return (
                <button type="button" className="r-row-action" onClick={(e) => { e.stopPropagation(); request({ kind: 'rollback', source: d }); }}>
                    Roll back here
                </button>
            );
        }
        return null;
    };

    const stopFor = (d: Deployment): React.ReactNode => isTerminal(d.status) ? null : (
        <button type="button" className="r-icon-btn r-row-stop" title="Stop deployment" aria-label={`Stop ${d.deployment_id}`}
            onClick={(e) => { e.stopPropagation(); setDeploymentToStop(d); }}>
            <Icon name="stop" size={12} />
        </button>
    );

    const groupFor = (d: Deployment): React.ReactNode => {
        const env = environments.find(e => e.name === d.environment);
        const primary = !!env && (env.primary_deployment_group || 'default') === d.deployment_group;
        return primary ? null : <GroupPill projectName={projectName} group={d.deployment_group} />;
    };

    const open = (d: Deployment) => navigate(`/deployment/${projectName}/${d.deployment_id}`);

    return (
        <div>
            <div className="r-toolbar">
                <SearchInput value={search} onChange={setSearch} placeholder="Filter by id, image or author" style={{ flex: '1 1 220px', maxWidth: 320 }} />
                <Segmented<string>
                    value={statusFilter}
                    options={[
                        { value: 'all', label: 'All' },
                        { value: 'active', label: 'Active' },
                        { value: 'healthy', label: 'Healthy' },
                        { value: 'unhealthy', label: 'Unhealthy' },
                        { value: 'failed', label: 'Failed' },
                    ]}
                    onChange={setStatusFilter}
                />
                {environments.length > 1 && (
                    <Segmented<string>
                        value={envFilter}
                        options={[{ value: '', label: 'All envs' }, ...environments.map(env => ({ value: env.name, label: env.name }))]}
                        onChange={(v) => { setEnvFilter(v || null); setPage(0); }}
                    />
                )}
                {deploymentGroups.length > 1 && (
                    <div style={{ width: 180 }}>
                        <Combobox
                            value={groupFilter}
                            onChange={(v) => { setGroupFilter(v || null); setPage(0); }}
                            options={[{ value: '', label: 'All groups' }, ...deploymentGroups.map(group => ({ value: group, label: group }))]}
                            placeholder="All groups"
                        />
                    </div>
                )}
            </div>

            {filtered.length === 0 ? (
                <Panel><div className="r-list-empty">No deployments match these filters.</div></Panel>
            ) : isMobile ? (
                <div className="r-cards">
                    {filtered.map(d => (
                        <div key={d.deployment_id} className="r-card" role="link" tabIndex={0} onClick={() => open(d)}
                            onKeyDown={e => { if (e.key === 'Enter') open(d); }}>
                            <div className="r-card-row">
                                <Status status={d.status} />
                                {d.environment && <EnvTag env={d.environment} />}
                                {groupFor(d)}
                                <span className="age">{formatRelativeTimeRounded(d.created)}</span>
                            </div>
                            <div className="r-card-title">{deploymentSource(d)}</div>
                            <div className="r-card-meta"><span className="mono">{d.deployment_id}</span> · {d.created_by_email}</div>
                            {(actionFor(d) || stopFor(d)) && <div className="r-card-actions">{actionFor(d)}{stopFor(d)}</div>}
                        </div>
                    ))}
                </div>
            ) : (
                <Panel>
                    <table className="r-table r-dep-table">
                        <thead>
                            <tr>
                                <th style={{ width: 116 }}>Status</th>
                                <th style={{ width: 150 }}>ID</th>
                                <th>Source</th>
                                <th style={{ width: 140 }}>Environment</th>
                                <th style={{ width: 110 }}>Age</th>
                                <th style={{ width: 150 }} />
                            </tr>
                        </thead>
                        <tbody>
                            {filtered.map(d => (
                                <tr key={d.deployment_id} className="click" onClick={() => open(d)}>
                                    <td><Status status={d.status} /></td>
                                    <td className="mono r-nowrap" style={{ fontSize: 12 }}>{d.deployment_id}</td>
                                    <td>
                                        <div className="r-cell-main">{deploymentSource(d)}</div>
                                        <div className="r-cell-sub">
                                            {[d.created_by_email, durationNote(d)].filter(Boolean).join(' · ')}
                                            {d.expires_at && <span title={formatISO8601(d.expires_at)}> · expires in {formatTimeRemaining(d.expires_at)}</span>}
                                        </div>
                                    </td>
                                    <td>
                                        <span className="r-cell-tags">
                                            {d.environment ? <EnvTag env={d.environment} /> : <span className="muted">—</span>}
                                            {groupFor(d)}
                                        </span>
                                    </td>
                                    <td className="muted r-nowrap" title={formatISO8601(d.created)}>{formatRelativeTimeRounded(d.created)}</td>
                                    <td style={{ textAlign: 'right' }}>
                                        <span className="r-row-actions">{actionFor(d)}{stopFor(d)}</span>
                                    </td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                </Panel>
            )}

            {(page > 0 || hasMore) && (
                <div className="r-pager">
                    <RButton size="sm" onClick={() => setPage(page - 1)} disabled={page === 0} icon="chevl">Newer</RButton>
                    <span>Page {page + 1}</span>
                    <RButton size="sm" onClick={() => setPage(page + 1)} disabled={!hasMore}>Older</RButton>
                </div>
            )}

            <ConfirmDialog
                isOpen={!!deploymentToStop}
                onClose={() => setDeploymentToStop(null)}
                onConfirm={handleStopConfirm}
                title={`Stop ${deploymentToStop?.deployment_id ?? 'deployment'}?`}
                message={`Traffic for group "${deploymentToStop?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop deployment"
                confirmTone="danger"
                loading={stopping}
            />
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

/**
 * How long a deployment took, or has been going: a finished rollout's
 * duration, the elapsed time of one in progress, and nothing for one that is
 * simply live (time since start isn't a duration).
 */
function durationNote(d: Deployment): string {
    if (d.completed_at) return formatDurationDelta(d.created, d.completed_at);
    if (isInProgress(d.status)) return `${formatDurationDelta(d.created, new Date().toISOString())} so far`;
    return '';
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
    const [stopDialogOpen, setStopDialogOpen] = useState(false);
    const [stopping, setStopping] = useState(false);
    const project = useCurrentProject();
    const { request } = useDeployActions();
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

    const handleStopConfirm = async () => {
        setStopping(true);
        try {
            await api.stopDeployment(projectName, deploymentId);
            showToast(`Stopping ${deploymentId}`, 'success');
            setStopDialogOpen(false);
            loadDeployment();
            window.dispatchEvent(new Event('rise:mutation'));
        } catch (err) {
            showToast(`Failed to stop deployment: ${err.message}`, 'error');
        } finally {
            setStopping(false);
        }
    };

    useEffect(() => {
        loadDeployment();
        window.addEventListener('rise:mutation', loadDeployment);
        return () => window.removeEventListener('rise:mutation', loadDeployment);
    }, [loadDeployment]);

    const events = useDeploymentEvents(projectName, deploymentId, deployment?.status ?? '');

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
        { id: 'overview', label: 'Overview' },
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
    const activeTab = tabs.some((t) => t.id === tabParam) ? (tabParam as string) : 'overview';
    const setActiveTab = (id: string) => setTabParam(id === 'overview' ? null : id);

    // What can be done from here depends on where the deployment is in its life.
    const groupLive = (project?.deployments ?? []).find(d =>
        d.is_active && d.environment === deployment.environment && d.deployment_group === deployment.deployment_group) ?? null;
    const promoteTo = deployment.is_active && deployment.environment
        ? promoteTargets(deployment.environment, project?.environments ?? []).find(e => e.is_production)
            ?? promoteTargets(deployment.environment, project?.environments ?? [])[0]
        : null;
    const currentEnv = (project?.environments ?? []).find(e => e.name === deployment.environment);
    let primaryAction: React.ReactNode = null;
    const secondaryActions: React.ReactNode[] = [];
    if (deployment.can_rollback) {
        if (deployment.status === 'Failed' || deployment.status === 'Unhealthy') {
            primaryAction = <RButton variant="primary" icon="refresh" onClick={() => request({ kind: 'redeploy', source: deployment })}>Retry deploy</RButton>;
        } else if (deployment.is_active) {
            if (promoteTo && !currentEnv?.is_production) {
                primaryAction = <RButton variant="primary" icon="promote" onClick={() => request({ kind: 'promote', source: deployment, targetEnv: promoteTo.name })}>Promote to {promoteTo.name}</RButton>;
            }
            secondaryActions.push(<RButton key="redeploy" icon="refresh" onClick={() => request({ kind: 'redeploy', source: deployment })}>Redeploy</RButton>);
        } else if (deployment.status === 'Superseded' && groupLive) {
            primaryAction = <RButton variant="primary" icon="rollback" onClick={() => request({ kind: 'rollback', source: deployment })}>Roll back to this</RButton>;
        } else if (isTerminal(deployment.status)) {
            secondaryActions.push(<RButton key="redeploy" icon="refresh" onClick={() => request({ kind: 'redeploy', source: deployment })}>Deploy again</RButton>);
        }
    }
    const rollout = events ? rolloutSteps(deployment, events) : null;
    const showFailure = deployment.status === 'Failed' || deployment.status === 'Unhealthy';

    const deploymentPanel = (
        <Panel>
            <PanelHead title="Deployment" />
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
                    {[
                        { label: 'Repository', url: deployment.git_repository_url },
                        {
                            label: /\/merge_requests(?:\/|$)/.test(deployment.pull_request_url || '') ? 'Merge request' : 'Pull request',
                            url: deployment.pull_request_url,
                        },
                        { label: 'CI job', url: deployment.job_url },
                    ].filter(source => source.url).map(source => (
                        <KVRow key={source.label} k={source.label}>
                            {isSafeUrl(source.url) ? (
                                <a
                                    className="r-link mono r-deployment-source-link"
                                    href={source.url}
                                    target="_blank"
                                    rel="noopener noreferrer"
                                >
                                    <span>{stripUrlScheme(source.url)}</span>
                                    <Icon name="ext" size={12} />
                                </a>
                            ) : (
                                <span className="mono" style={{ fontSize: 12.5, wordBreak: 'break-all' }}>
                                    {source.url}
                                </span>
                            )}
                        </KVRow>
                    ))}
                </KV>
            </PanelBody>
        </Panel>
    );

    const lifecyclePanel = (
        <Panel>
            <PanelHead
                title="Lifecycle"
                right={<RButton size="sm" onClick={() => setActiveTab('timeline')}>View timeline</RButton>}
            />
            <PanelBody>
                <KV>
                    <KVRow k="Active deployment">
                        {deployment.is_active ? 'Yes — active for this deployment group' : 'No'}
                    </KVRow>
                    <KVRow k="Started">
                        <span title={formatISO8601(deployment.created)}>{formatDate(deployment.created)}</span>
                    </KVRow>
                    {deployment.updated && (
                        <KVRow k="Last updated">
                            <span title={formatISO8601(deployment.updated)}>{formatRelativeTimeRounded(deployment.updated)}</span>
                        </KVRow>
                    )}
                    {deployment.completed_at && (
                        <KVRow k="Completed">{formatDate(deployment.completed_at)}</KVRow>
                    )}
                    <KVRow k="Expires">
                        {deployment.expires_at ? (
                            <span title={formatISO8601(deployment.expires_at)}>{formatTimeRemaining(deployment.expires_at)}</span>
                        ) : 'No expiration'}
                    </KVRow>
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
                sub="Configured allocation"
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
            <PanelHead
                title="Routing"
                sub={!deployment.is_active && allDomains.length > 0
                    ? 'These addresses may serve another deployment in this group.'
                    : undefined}
            />
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
                    <div className="r-title-row">
                        <h1 className="r-page-title mono r-dep-title">{deployment.deployment_id}</h1>
                        <Status status={deployment.status} tooltip={`Deployment status: ${deployment.status}`} />
                        {deployment.environment && <EnvTag env={deployment.environment} color={deployment.environment_color} />}
                        {deployment.deployment_group && (() => {
                            const env = environments.find((e) => e.name === deployment.environment);
                            const isPrimaryGroup = !!env && env.primary_deployment_group === deployment.deployment_group;
                            return (
                                <GroupPill
                                    projectName={projectName}
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
                    <div className="r-dep-subtitle">{deploymentSource(deployment)}</div>
                </div>
                <div className="r-page-actions">
                    {primaryAction}
                    {secondaryActions}
                    <RButton icon="terminal" onClick={() => setActiveTab('logs')}>Logs</RButton>
                    {!isTerminal(deployment.status) && (
                        <RButton variant="danger" icon="stop" onClick={() => setStopDialogOpen(true)}>Stop</RButton>
                    )}
                    <button type="button" className="r-icon-btn" title="Copy ID" aria-label="Copy deployment ID"
                        onClick={() => handleCopy(deployment.deployment_id, 'Deployment ID')}>
                        <Icon name="copy" size={14} />
                    </button>
                </div>
            </div>

            <Tabs tabs={tabs} active={activeTab} onChange={setActiveTab} />

            {activeTab === 'overview' && (
                <div className="r-stack r-dep-diagnosis">
                    <DeploymentMetaStrip deployment={deployment} endedAt={rolloutEnd(deployment, rollout?.steps ?? null)} />
                    {showFailure && (
                        <FailurePanel
                            projectName={projectName}
                            deployment={deployment}
                            failedAt={rollout?.failedAt ?? null}
                            stillServing={groupLive && groupLive.deployment_id !== deployment.deployment_id ? groupLive : null}
                            onOpenLogs={() => setActiveTab('logs')}
                        />
                    )}
                    {rollout && <RolloutPanel steps={rollout.steps} />}
                </div>
            )}

            {activeTab === 'overview' && (
                <div className="r-grid-2-1 r-deployment-overview">
                    <div className="r-stack">
                        {deploymentPanel}
                        {lifecyclePanel}
                    </div>
                    <div className="r-stack">
                        {routingPanel}
                        {runtimeKv}
                    </div>
                </div>
            )}

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

            <ConfirmDialog
                isOpen={stopDialogOpen}
                onClose={() => setStopDialogOpen(false)}
                onConfirm={handleStopConfirm}
                title={`Stop ${deploymentId}?`}
                message={`Traffic for group "${deployment?.deployment_group || 'default'}" may terminate.`}
                confirmText="Stop deployment"
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
