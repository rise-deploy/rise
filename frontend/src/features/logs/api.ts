import { CONFIG } from '../../lib/config';
import { readSseResponse, type SseHandlers } from './sse';
import type { LogVolumeResponse, LogsCapabilities } from './types';

/**
 * Log endpoints. These don't go through `lib/api.ts`'s `request()` helper: the
 * stream is SSE (it needs the raw `Response` body reader and an abort signal)
 * and the export is a plain body, neither of which that JSON helper can carry.
 */

function deploymentBase(projectName: string, deploymentId: string): string {
    return `${CONFIG.backendUrl}/api/v1/projects/${encodeURIComponent(projectName)}`
        + `/deployments/${encodeURIComponent(deploymentId)}`;
}

/** Pull the most useful message out of a failed response. */
export async function readHttpErrorMessage(response: Response): Promise<string> {
    try {
        const text = await response.text();
        if (text) {
            try {
                const data = JSON.parse(text) as { error?: unknown };
                if (typeof data?.error === 'string') return data.error;
            } catch {
                /* not JSON */
            }
            if (text.length < 240) return text;
        }
    } catch {
        /* body already consumed or unreadable */
    }
    return `HTTP ${response.status}: ${response.statusText}`;
}

const CAPABILITIES_FALLBACK: LogsCapabilities = {
    levels: [],
    supports_volume: false,
    backend: null,
    max_tail: null,
};

/**
 * Server capabilities for the configured runtime log backend. Drives the level
 * filter options, the chart palette, and the tail ceiling.
 *
 * Each field is extracted with its own type guard rather than spread-merged, so
 * a backend that returns `levels: "all"` or `supports_volume: "yes"` can't leak
 * off-type values into Combobox options or a visibility check.
 */
export async function getLogsCapabilities(): Promise<LogsCapabilities> {
    let payload: unknown;
    try {
        const response = await fetch(`${CONFIG.backendUrl}/api/v1/logs/capabilities`, {
            headers: { Accept: 'application/json' },
            credentials: 'include',
        });
        if (!response.ok) throw new Error(await readHttpErrorMessage(response));
        payload = await response.json();
    } catch (err) {
        console.warn('getLogsCapabilities request failed; using safe default:', err);
        return CAPABILITIES_FALLBACK;
    }
    if (!payload || typeof payload !== 'object') {
        console.warn('getLogsCapabilities returned a non-object payload; using safe default:', payload);
        return CAPABILITIES_FALLBACK;
    }
    const record = payload as Record<string, unknown>;
    return {
        levels: Array.isArray(record.levels)
            ? record.levels.filter((l): l is string => typeof l === 'string')
            : [],
        supports_volume: typeof record.supports_volume === 'boolean' ? record.supports_volume : false,
        backend: typeof record.backend === 'string' ? record.backend : null,
        max_tail: typeof record.max_tail === 'number' && Number.isFinite(record.max_tail)
            ? record.max_tail
            : null,
    };
}

/** Filters shared by the stream, volume and export endpoints. */
export interface LogFilters {
    levels?: string[];
    search?: string;
    containers?: string[];
}

function applyFilters(params: URLSearchParams, filters: LogFilters): void {
    for (const level of filters.levels ?? []) params.append('level', level);
    for (const container of filters.containers ?? []) params.append('container', container);
    if (filters.search) params.set('search', filters.search);
}

/**
 * The server rejected our continuation token — it was minted for a different
 * filter set, or its backend moved on. Callers drop the cursor and stop paging
 * rather than surfacing this as a stream error.
 */
export class StaleCursorError extends Error {
    constructor(message: string) {
        super(message);
        this.name = 'StaleCursorError';
    }
}

function isCursorRejection(message: string): boolean {
    return message.toLowerCase().includes('cursor');
}

export interface LogStreamRequest extends LogFilters {
    projectName: string;
    deploymentId: string;
    follow?: boolean;
    /** RFC3339. */
    start?: string;
    end?: string;
    tail: number;
    /**
     * Opaque continuation token from a `page_complete` / `backlog_complete` /
     * `cursor` event. The server rejects a cursor sent alongside `follow` or
     * any time-range parameter, so passing one here suppresses them.
     */
    cursor?: string;
    signal: AbortSignal;
}

/**
 * Open the SSE log feed and pump it into `handlers` until the stream ends or
 * `signal` aborts. Resolves when the server closes the stream.
 */
export async function streamLogs(request: LogStreamRequest, handlers: SseHandlers): Promise<void> {
    const params = new URLSearchParams({ timestamps: 'true', tail: String(request.tail) });
    if (request.cursor) {
        // The cursor already encodes the window and the filters it was minted
        // for; sending either again is a 400. Enforced here so no call site can
        // construct an invalid request.
        params.set('cursor', request.cursor);
    } else {
        if (request.follow) params.set('follow', 'true');
        if (request.start) params.set('start', request.start);
        if (request.end) params.set('end', request.end);
    }
    applyFilters(params, request);

    const response = await fetch(
        `${deploymentBase(request.projectName, request.deploymentId)}/logs?${params}`,
        {
            headers: { Accept: 'text/event-stream' },
            credentials: 'include',
            signal: request.signal,
        },
    );
    if (!response.ok) {
        const message = await readHttpErrorMessage(response);
        throw isCursorRejection(message) ? new StaleCursorError(message) : new Error(message);
    }
    await readSseResponse(response, handlers);
}

export interface LogVolumeRequest extends LogFilters {
    projectName: string;
    deploymentId: string;
    /** RFC3339. */
    start: string;
    end: string;
    stepSeconds: number;
    signal: AbortSignal;
}

export async function fetchLogVolume(request: LogVolumeRequest): Promise<LogVolumeResponse> {
    const params = new URLSearchParams({
        start: request.start,
        end: request.end,
        step_seconds: String(request.stepSeconds),
    });
    applyFilters(params, request);

    const response = await fetch(
        `${deploymentBase(request.projectName, request.deploymentId)}/logs/volume?${params}`,
        {
            headers: { Accept: 'application/json' },
            credentials: 'include',
            signal: request.signal,
        },
    );
    if (!response.ok) throw new Error(await readHttpErrorMessage(response));
    return (await response.json()) as LogVolumeResponse;
}

/** One row of the deployment event log. */
export interface DeploymentEvent {
    id: number;
    deployment_id: string;
    /** When it happened, according to the event source. What the rail plots. */
    occurred_at: string;
    /** When Rise recorded it. What pages are cut on. */
    recorded_at: string;
    kind: string;
    severity: string;
    source: string;
    /** What inside the deployment this is about — a container replica — or
     *  `null` for the deployment itself. */
    subject: string | null;
    message: string | null;
    attributes: Record<string, unknown>;
}

export interface DeploymentEventPage {
    events: DeploymentEvent[];
    next_cursor?: string;
}

/**
 * Read a page of the deployment event log.
 *
 * The log is a history, so it records what a current-state view cannot: a
 * deployment that went healthy, unhealthy and healthy again contributes three
 * moves here where its status shows one.
 */
export async function fetchDeploymentEvents(request: {
    projectName: string;
    deploymentId: string;
    limit?: number;
    cursor?: string;
    kinds?: string[];
    /** `all` includes `debug`; omitted means `info` and above. */
    minSeverity?: string;
    signal?: AbortSignal;
}): Promise<DeploymentEventPage> {
    const params = new URLSearchParams();
    if (request.limit) params.set('limit', String(request.limit));
    if (request.cursor) params.set('cursor', request.cursor);
    if (request.minSeverity) params.set('min_severity', request.minSeverity);
    for (const kind of request.kinds ?? []) params.append('kind', kind);

    const response = await fetch(
        `${deploymentBase(request.projectName, request.deploymentId)}/events?${params}`,
        {
            headers: { Accept: 'application/json' },
            credentials: 'include',
            signal: request.signal,
        },
    );
    if (!response.ok) throw new Error(await readHttpErrorMessage(response));
    return (await response.json()) as DeploymentEventPage;
}

/** One replica, as the deployment's backend last saw it. */
export interface ContainerStatus {
    /** The backend's stable handle — `web[0]`, a pod name, an ECS task id. The
     *  same string an event carries as its `subject`. */
    subject: string;
    container: string;
    /** Present only where the backend has a stable replica ordinal. */
    replica?: number;
    state: 'pending' | 'running' | 'exited' | 'unknown';
    started_at?: string;
    finished_at?: string;
    exit_code?: number;
    /** Absent on ECS, which replaces tasks rather than restarting containers. */
    restart_count?: number;
    health?: string;
    reason?: string;
    image?: string;
}

export interface ContainerStatusPage {
    version: number;
    containers: ContainerStatus[];
}

/**
 * Read the current state of a deployment's replicas.
 *
 * A snapshot, where the event log is a history. The two share a vocabulary: a
 * container's `subject` here is the `subject` on its events.
 */
export async function fetchDeploymentContainers(request: {
    projectName: string;
    deploymentId: string;
    signal?: AbortSignal;
}): Promise<ContainerStatusPage> {
    const { projectName, deploymentId, signal } = request;
    const url = `${deploymentBase(projectName, deploymentId)}/containers`;
    const response = await fetch(url, { credentials: 'include', signal });
    if (!response.ok) {
        throw new Error(`Failed to load containers (${response.status})`);
    }
    return response.json();
}
