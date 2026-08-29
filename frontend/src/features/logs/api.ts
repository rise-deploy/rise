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

export interface LogStreamRequest extends LogFilters {
    projectName: string;
    deploymentId: string;
    follow?: boolean;
    /** RFC3339. */
    start?: string;
    end?: string;
    tail: number;
    /** Backends without an end-time filter use this to page past loaded lines. */
    skipRecent?: number;
    signal: AbortSignal;
}

/**
 * Open the SSE log feed and pump it into `handlers` until the stream ends or
 * `signal` aborts. Resolves when the server closes the stream.
 */
export async function streamLogs(request: LogStreamRequest, handlers: SseHandlers): Promise<void> {
    const params = new URLSearchParams({ timestamps: 'true', tail: String(request.tail) });
    if (request.follow) params.set('follow', 'true');
    if (request.start) params.set('start', request.start);
    if (request.end) params.set('end', request.end);
    if (request.skipRecent && request.skipRecent > 0) {
        params.set('skip_recent', String(request.skipRecent));
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
    if (!response.ok) throw new Error(await readHttpErrorMessage(response));
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
