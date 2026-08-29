import type { LogEntry, LogStatus, LogWindow } from './types';
import type { LogLinePayload } from './sse';

/**
 * Severity-ascending order, so filter menus and the stacked chart present
 * levels in a sensible reading order. Anything absent (forward compat: Loki
 * adds a value) sorts after the known ones.
 */
const LEVEL_SEVERITY_ORDER: Record<string, number> = {
    trace: 0,
    debug: 1,
    info: 2,
    notice: 3,
    warn: 4,
    error: 5,
    critical: 6,
    fatal: 7,
    unknown: 8,
};

/** Levels whose message text is tinted. Everything quieter reads as body text. */
export const LOUD_LEVELS = new Set(['error', 'critical', 'fatal']);

/** Levels that earn a tick on the heat track. */
export const NOTABLE_LEVELS = new Set(['warn', 'error', 'critical', 'fatal']);

export function levelSeverityRank(level: string): number {
    const rank = LEVEL_SEVERITY_ORDER[level];
    return rank === undefined ? 99 : rank;
}

export function levelLabel(level: string): string {
    if (!level) return '';
    return level.charAt(0).toUpperCase() + level.slice(1);
}

export function orderedLevels(levels: string[]): string[] {
    return [...levels].sort((a, b) => levelSeverityRank(a) - levelSeverityRank(b));
}

export function orderedLevelOptions(levels: string[]): { value: string; label: string }[] {
    return orderedLevels(levels).map((value) => ({ value, label: levelLabel(value) }));
}

export const LOG_RANGE_PRESETS = [
    { value: '15m', label: '15m', minutes: 15 },
    { value: '1h', label: '1h', minutes: 60 },
    { value: '6h', label: '6h', minutes: 360 },
    { value: '24h', label: '24h', minutes: 1440 },
    { value: '7d', label: '7d', minutes: 10080 },
    { value: 'custom', label: 'Custom', minutes: 0 },
] as const;

export function presetToMilliseconds(value: string): number {
    const preset = LOG_RANGE_PRESETS.find((option) => option.value === value);
    return preset ? preset.minutes * 60 * 1000 : 6 * 60 * 60 * 1000;
}

/**
 * Resolve the visible window. For presets, `anchorEnd` (a terminal
 * deployment's stop time) replaces wall-clock now, so "Last 6h" means the six
 * hours leading up to the deployment's end rather than an empty window.
 */
export function resolveLogWindow(
    rangeValue: string,
    customStart: Date | null,
    customEnd: Date | null,
    anchorEnd: Date | null,
): LogWindow | null {
    if (rangeValue === 'custom') {
        if (!customStart) return null;
        const end = customEnd || new Date();
        if (customStart >= end) return null;
        return { start: customStart, end };
    }
    const end = anchorEnd || new Date();
    return { start: new Date(end.getTime() - presetToMilliseconds(rangeValue)), end };
}

/** Bucket width for the volume chart, chosen to keep bar counts reasonable. */
export function chooseCountStepSeconds(rangeMs: number): number {
    const rangeSeconds = Math.max(60, Math.floor(rangeMs / 1000));
    if (rangeSeconds <= 3600) return 60;
    if (rangeSeconds <= 6 * 3600) return 5 * 60;
    if (rangeSeconds <= 24 * 3600) return 15 * 60;
    if (rangeSeconds <= 7 * 24 * 3600) return 60 * 60;
    return 6 * 60 * 60;
}

const pad2 = (n: number) => String(n).padStart(2, '0');
const pad3 = (n: number) => String(n).padStart(3, '0');

/** `HH:MM:SS.mmm` in local time. */
export function formatHms(timestampMs: number): string {
    if (!timestampMs) return '--:--:--.---';
    const d = new Date(timestampMs);
    return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}.${pad3(d.getMilliseconds())}`;
}

/** `MMM DD` — prefixed to the time when the window spans more than one day. */
export function formatDay(timestampMs: number): string {
    if (!timestampMs) return '';
    return new Date(timestampMs).toLocaleDateString(undefined, { month: 'short', day: '2-digit' });
}

export function formatDateTimeShort(date: Date | null): string {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return '';
    return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`
        + ` ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

/** True when a window covers more than one local calendar day. */
export function windowSpansDays(window: LogWindow | null): boolean {
    if (!window) return false;
    return window.start.toDateString() !== window.end.toDateString();
}

/** Find the first `{`/`[` and parse from there. Returns null when it isn't JSON. */
export function extractLogJson(raw: string): unknown {
    const s = raw.trim();
    if (s.length < 2) return null;
    const candidates = ['{', '['].map((c) => s.indexOf(c)).filter((i) => i >= 0);
    if (candidates.length === 0) return null;
    try {
        return JSON.parse(s.slice(Math.min(...candidates)));
    } catch {
        return null;
    }
}

/**
 * Turn one wire payload into a `LogEntry`.
 *
 * The backend prepends the RFC3339 timestamp and a space when `timestamps=true`,
 * so the timestamp has to come back out of the text. When the server supplies
 * structured attribution those fields are carried through unchanged.
 */
export function parseLogLine(payload: LogLinePayload, seq: number): LogEntry {
    const line = payload.line;
    const sp = line.indexOf(' ');
    const isoCandidate = sp > 0 ? line.slice(0, sp) : '';
    const date = isoCandidate ? new Date(isoCandidate) : null;
    const hasTs = date !== null && !Number.isNaN(date.getTime());
    const timestampMs = hasTs ? date.getTime() : 0;
    const raw = hasTs ? line.slice(sp + 1) : line;
    const parsed = extractLogJson(raw);
    const level = payload.level && payload.level.trim() ? payload.level.trim() : 'unknown';
    return {
        id: `${timestampMs}-${seq}`,
        timestampMs,
        iso: hasTs ? isoCandidate : '',
        raw,
        level,
        isJson: parsed !== null,
        parsed,
        container: payload.container,
        replica: payload.replica,
        stream: payload.stream,
    };
}

/**
 * Identity for deduplicating paginated pages against what is already loaded.
 * `id` cannot serve: it embeds a monotonic sequence number, so rows from a new
 * page never collide with rows already in state.
 */
export function entryKey(entry: LogEntry): string {
    return `${entry.timestampMs} ${entry.raw}`;
}

/** Human-readable text for a typed empty state. */
export function describeLogStatus(status: LogStatus | null): string | null {
    if (!status) return null;
    switch (status.reason) {
        case 'retention_expired_possible':
            return status.retention_hint
                ? `No logs found. Runtime logs are retained for ${status.retention_hint}, so this deployment's logs may no longer be available.`
                : 'No logs found. They may have expired based on the log backend retention policy.';
        case 'historical_backend_not_configured':
            return 'No active deployment pod was found and historical logs are not configured.';
        case 'deployment_not_ready':
            return 'Deployment logs are not ready yet.';
        case 'backend_unavailable':
            return 'The log backend is unavailable.';
        default:
            return 'No logs found.';
    }
}
