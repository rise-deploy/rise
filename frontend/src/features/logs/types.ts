// TypeScript mirrors of the log types the backend serialises. The Rust
// definitions live in `src/server/deployment/logs.rs`; keep the two in sync.

/** `LogStatusReason` — why a range came back without the lines you expected. */
export type LogStatusReason =
    | 'no_logs_found'
    | 'retention_expired_possible'
    | 'historical_backend_not_configured'
    | 'backend_unavailable'
    | 'deployment_not_ready';

/** `LogStatus` — a typed empty/degraded state rather than a bare empty list. */
export interface LogStatus {
    reason: LogStatusReason;
    message?: string;
    retention_hint?: string;
}

/** `LogVolumeBucket` — one bar of the volume chart. `by_level` is sparse. */
export interface LogVolumeBucket {
    /** Right edge of the bucket, RFC3339. */
    timestamp: string;
    total: number;
    by_level: Record<string, number>;
}

/** `LogVolumeResponse`. */
export interface LogVolumeResponse {
    status?: LogStatus | null;
    start_time: string;
    end_time: string;
    step_seconds: number;
    buckets: LogVolumeBucket[];
}

/**
 * `LogsCapabilities` — server-scoped, from `GET /api/v1/logs/capabilities`.
 * Drives the level filter options, the chart palette, and whether the volume
 * panel renders at all.
 */
export interface LogsCapabilities {
    backend: string | null;
    levels: string[];
    supports_volume: boolean;
    /** Server-side cap on `?tail=`, when the backend advertises one. */
    max_tail: number | null;
}

/** Which stdio stream a line came from, when the backend can tell them apart. */
export type LogStreamKind = 'stdout' | 'stderr';

/**
 * One rendered log line. `id` is stable for the lifetime of the entry and is
 * used as the React key and the expansion key.
 */
export interface LogEntry {
    id: string;
    /** Epoch millis, or 0 when the line carried no parseable timestamp. */
    timestampMs: number;
    /** The RFC3339 prefix the backend emitted, or '' when absent. */
    iso: string;
    /** The line with its timestamp prefix stripped. */
    raw: string;
    level: string;
    /** Set when `raw` contains a JSON object/array; holds the parsed value. */
    isJson: boolean;
    parsed: unknown;
    /** Attribution, when the configured backend can supply it. */
    container?: string;
    replica?: string;
    stream?: LogStreamKind;
}

/** A resolved [start, end) query window. */
export interface LogWindow {
    start: Date;
    end: Date;
}
