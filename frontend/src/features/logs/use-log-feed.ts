import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { StaleCursorError, fetchLogVolume, getLogsCapabilities, streamLogs } from './api';
import {
    chooseCountStepSeconds,
    entryKey,
    parseLogLine,
    resolveLogWindow,
} from './format';
import { parseQuery } from './query';
import type {
    LogEntry,
    LogStatus,
    LogVolumeBucket,
    LogWindow,
    LogsCapabilities,
} from './types';

/** Statuses whose workload can still be followed live. */
export const STREAMABLE_LOG_STATUSES = new Set([
    'Deploying', 'Healthy', 'Unhealthy', 'Cancelling', 'Terminating',
]);

/** Statuses that can have logs at all — terminal ones included, via history. */
export const LOGGABLE_LOG_STATUSES = new Set([
    'Deploying', 'Healthy', 'Unhealthy', 'Cancelling', 'Terminating',
    'Cancelled', 'Stopped', 'Failed', 'Superseded', 'Expired',
]);

/** Lines requested per historical page. */
const LOG_PAGE_SIZE = 200;

/**
 * Ceiling on lines held in memory. The list is virtualized, so this is about
 * memory and search cost rather than DOM size. Whichever end of the buffer is
 * furthest from where the user is reading gets trimmed.
 */
const LOG_BUFFER_CAP = 50_000;

/**
 * How far back a streamed line is checked for having already arrived. Sized
 * well above a page so the whole backlog/live seam is covered.
 */
const LIVE_DEDUP_WINDOW = 1_000;

/** Distance from the bottom, in px, that still counts as "following". */
export const LOG_FOLLOW_THRESHOLD_PX = 24;

/**
 * Cushion added to a terminated deployment's `completed_at` when anchoring the
 * range end. A deployment is marked complete before its Pods are guaranteed to
 * be torn down, so lines can still arrive briefly after; without this the
 * default range clips them off-screen.
 */
const LOG_TERMINATED_END_CUSHION_MS = 10 * 60 * 1000;

const AUTO_REFRESH_STORAGE_KEY = 'rise.deploymentLogs.autoRefreshSeconds';
/** Preset windows the range control offers, plus `custom`. */
const RANGE_VALUES = new Set(['15m', '1h', '6h', '24h', '7d', 'custom']);
const DEFAULT_RANGE = '6h';

function readRangeFromUrl(): string {
    const value = new URLSearchParams(window.location.search).get('range');
    // An unrecognised range is treated as absent: a stale or hand-edited link
    // should open a working view, not an empty one.
    if (!value || !RANGE_VALUES.has(value)) return DEFAULT_RANGE;
    // `custom` without a usable pair of instants resolves to no window at all,
    // which stops the stream and shows an error. A truncated paste is exactly
    // the case the line above exists to survive, so it falls back too.
    if (value === 'custom') {
        const start = readInstantFromUrl('from');
        const end = readInstantFromUrl('to');
        if (!start || !end || start >= end) return DEFAULT_RANGE;
    }
    return value;
}

function readInstantFromUrl(key: 'from' | 'to'): Date | null {
    const value = new URLSearchParams(window.location.search).get(key);
    if (!value) return null;
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? null : date;
}

/**
 * Mirror the selected window into the URL.
 *
 * `replaceState`, not `pushState`: dragging a custom range would otherwise
 * leave one history entry per adjustment for the Back button to walk through.
 * The default preset writes no parameter at all, so an unmodified view links
 * cleanly.
 */
function writeRangeToUrl(range: string, start: Date | null, end: Date | null): void {
    const params = new URLSearchParams(window.location.search);

    if (range === DEFAULT_RANGE) params.delete('range');
    else params.set('range', range);

    if (range === 'custom' && start && end) {
        params.set('from', start.toISOString());
        params.set('to', end.toISOString());
    } else {
        params.delete('from');
        params.delete('to');
    }

    const query = params.toString();
    window.history.replaceState(
        {},
        '',
        window.location.pathname + (query ? `?${query}` : '') + window.location.hash,
    );
}

const DEFAULT_AUTO_REFRESH_SECONDS = 300;

/** A chart bucket the user clicked, narrowing the log query to its span. */
export interface SelectedBucket {
    startMs: number;
    endMs: number;
}

export interface UseLogFeedOptions {
    projectName: string;
    deploymentId: string;
    deploymentStatus: string;
    deploymentCompletedAt?: string | null;
    deploymentCreated?: string | null;
}

function readStoredAutoRefresh(): number {
    if (typeof window === 'undefined') return DEFAULT_AUTO_REFRESH_SECONDS;
    try {
        const raw = window.localStorage.getItem(AUTO_REFRESH_STORAGE_KEY);
        if (raw === null) return DEFAULT_AUTO_REFRESH_SECONDS;
        const parsed = Number.parseInt(raw, 10);
        if (Number.isNaN(parsed) || parsed < 0) return DEFAULT_AUTO_REFRESH_SECONDS;
        return parsed;
    } catch {
        return DEFAULT_AUTO_REFRESH_SECONDS;
    }
}

/** Shallow order-sensitive equality, to keep filter identities stable. */
function sameValues(a: string[], b: string[]): boolean {
    return a.length === b.length && a.every((v, i) => v === b[i]);
}

function isAbort(err: unknown): boolean {
    return err instanceof Error && err.name === 'AbortError';
}

function errorMessage(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}

/**
 * Owns everything behind the log console: the SSE feed, backward pagination,
 * the visible time range, filters, and the volume histogram.
 *
 * Entries are kept **ascending** (oldest first, newest last) to match tail
 * semantics. Live lines append; pagination prepends.
 */
export function useLogFeed({
    projectName,
    deploymentId,
    deploymentStatus,
    deploymentCompletedAt,
    deploymentCreated,
}: UseLogFeedOptions) {
    const streamable = STREAMABLE_LOG_STATUSES.has(deploymentStatus);
    const loggable = LOGGABLE_LOG_STATUSES.has(deploymentStatus);

    const [entries, setEntries] = useState<LogEntry[]>([]);
    const [streaming, setStreaming] = useState(false);
    /** User intent, independent of whether a stream is currently open. */
    const [live, setLive] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [status, setStatus] = useState<LogStatus | null>(null);

    const [counts, setCounts] = useState<LogVolumeBucket[]>([]);
    const [countsLoading, setCountsLoading] = useState(false);
    const [countsError, setCountsError] = useState<string | null>(null);
    const [countsStatus, setCountsStatus] = useState<LogStatus | null>(null);

    /**
     * The console has one filter surface, so the hook holds one query string
     * and derives the wire params from it. Empty arrays mean "no filter"; each
     * value is sent as its own query param.
     */
    const [queryText, setQueryText] = useState('');
    const [levelFilter, setLevelFilter] = useState<string[]>([]);
    const [containerFilter, setContainerFilter] = useState<string[]>([]);
    const [searchActive, setSearchActive] = useState('');

    // Assume full support until the first capabilities response lands, so the
    // filter UI is usable immediately rather than appearing progressively.
    const [capabilities, setCapabilities] = useState<LogsCapabilities>({
        backend: null,
        levels: ['info', 'warn', 'error'],
        supports_volume: true,
        max_tail: null,
    });

    // The selected window lives in the URL: a link to a deployment's logs is
    // only useful if it carries *which* logs. `from`/`to` are ISO instants and
    // only present for a custom range; a preset is just its own name.
    const [rangeValue, setRangeValue] = useState(readRangeFromUrl);
    const [customStart, setCustomStart] = useState<Date | null>(() => readInstantFromUrl('from'));
    const [customEnd, setCustomEnd] = useState<Date | null>(() => readInstantFromUrl('to'));
    // Bumped on refresh to re-anchor a preset window's end at "now". Without
    // it, lines that streamed in after page load stay marked out-of-range.
    const [rangeNowTick, setRangeNowTick] = useState(0);

    const [hasMore, setHasMore] = useState(false);
    const [loadingMore, setLoadingMore] = useState(false);
    const [selectedBucket, setSelectedBucket] = useState<SelectedBucket | null>(null);
    const [autoRefreshSeconds, setAutoRefreshSeconds] = useState(readStoredAutoRefresh);

    // Read by `changeRange` when switching to a custom window, without making
    // the callback depend on values that change as the user drags the range.
    const customStartRef = useRef(customStart);
    const customEndRef = useRef(customEnd);
    customStartRef.current = customStart;
    customEndRef.current = customEnd;

    const streamAbortRef = useRef<AbortController | null>(null);
    const countsAbortRef = useRef<AbortController | null>(null);
    const olderAbortRef = useRef<AbortController | null>(null);
    const countsRequestIdRef = useRef(0);
    const seqRef = useRef(0);
    const loadingMoreRef = useRef(false);
    const streamGenRef = useRef(0);
    /**
     * Continuation token for older lines, refreshed by every `page_complete`,
     * `backlog_complete` and `cursor` event. `null` means the backend has said
     * there is nothing older — which is also how a backend that cannot page
     * backwards at all (Docker) reports itself.
     */
    const paginationCursorRef = useRef<string | null>(null);

    /**
     * For a terminal deployment, anchor preset ranges to when it stopped —
     * otherwise "Last 6h" on a deployment that died three days ago is empty.
     */
    const anchorEnd = useMemo(() => {
        if (streamable || !deploymentCompletedAt) return null;
        const d = new Date(deploymentCompletedAt);
        if (Number.isNaN(d.getTime())) return null;
        return new Date(d.getTime() + LOG_TERMINATED_END_CUSHION_MS);
    }, [streamable, deploymentCompletedAt]);

    // eslint-disable-next-line react-hooks/exhaustive-deps -- rangeNowTick re-anchors preset windows
    const rangeWindow = useMemo(
        () => resolveLogWindow(rangeValue, customStart, customEnd, anchorEnd),
        [rangeValue, customStart, customEnd, anchorEnd, rangeNowTick],
    );

    const rangeStepSeconds = useMemo(() => {
        if (!rangeWindow) return 60;
        return chooseCountStepSeconds(rangeWindow.end.getTime() - rangeWindow.start.getTime());
    }, [rangeWindow]);

    /**
     * The window log queries actually use. Selecting a chart bucket narrows
     * this without collapsing `rangeWindow`, so the chart keeps its context.
     */
    const logWindow = useMemo<LogWindow | null>(() => {
        if (selectedBucket) {
            return { start: new Date(selectedBucket.startMs), end: new Date(selectedBucket.endMs) };
        }
        return rangeWindow;
    }, [rangeWindow, selectedBucket]);

    /** Honour the backend's advertised tail ceiling rather than guessing. */
    const pageSize = useMemo(() => {
        const cap = capabilities.max_tail;
        if (cap && cap > 0) return Math.min(LOG_PAGE_SIZE, cap);
        return LOG_PAGE_SIZE;
    }, [capabilities.max_tail]);

    const filters = useMemo(
        () => ({ levels: levelFilter, containers: containerFilter, search: searchActive }),
        [levelFilter, containerFilter, searchActive],
    );

    useEffect(() => {
        let cancelled = false;
        void (async () => {
            const caps = await getLogsCapabilities();
            if (cancelled) return;
            setCapabilities({
                backend: caps.backend,
                levels: caps.levels.length > 0 ? caps.levels : ['info', 'warn', 'error'],
                supports_volume: caps.supports_volume,
                max_tail: caps.max_tail,
            });
        })();
        return () => { cancelled = true; };
    }, []);

    useEffect(() => {
        if (typeof window === 'undefined') return;
        try {
            window.localStorage.setItem(AUTO_REFRESH_STORAGE_KEY, String(autoRefreshSeconds));
        } catch {
            /* localStorage may be disabled — best effort */
        }
    }, [autoRefreshSeconds]);

    /** Adopt a continuation token; `hasMore` is exactly "a cursor exists". */
    const applyCursor = useCallback((next: string | null) => {
        paginationCursorRef.current = next;
        setHasMore(next !== null);
    }, []);

    const abortOlder = useCallback(() => {
        if (olderAbortRef.current) {
            olderAbortRef.current.abort();
            olderAbortRef.current = null;
            loadingMoreRef.current = false;
            setLoadingMore(false);
        }
    }, []);

    const stopStreaming = useCallback(() => {
        if (streamAbortRef.current) {
            streamAbortRef.current.abort();
            streamAbortRef.current = null;
        }
        setStreaming(false);
    }, []);

    // ---- volume -----------------------------------------------------------

    const volumeSupported = capabilities.supports_volume
        && countsStatus?.reason !== 'historical_backend_not_configured';

    const refreshCounts = useCallback(async () => {
        if (!rangeWindow) {
            setCounts([]);
            setCountsStatus(null);
            setCountsError('Select a valid time range');
            return;
        }

        const requestId = ++countsRequestIdRef.current;
        countsAbortRef.current?.abort();
        const controller = new AbortController();
        countsAbortRef.current = controller;
        setCountsLoading(true);
        setCountsError(null);

        try {
            const data = await fetchLogVolume({
                projectName,
                deploymentId,
                start: rangeWindow.start.toISOString(),
                end: rangeWindow.end.toISOString(),
                stepSeconds: rangeStepSeconds,
                signal: controller.signal,
                ...filters,
            });
            if (countsRequestIdRef.current !== requestId) return;
            setCounts(data.buckets ?? []);
            setCountsStatus(data.status ?? null);
        } catch (err) {
            if (countsRequestIdRef.current !== requestId || isAbort(err)) return;
            console.error('Failed to load log volume:', err);
            setCounts([]);
            setCountsStatus(null);
            setCountsError(errorMessage(err));
        } finally {
            if (countsRequestIdRef.current === requestId) {
                setCountsLoading(false);
                if (countsAbortRef.current === controller) countsAbortRef.current = null;
            }
        }
    }, [deploymentId, projectName, rangeStepSeconds, rangeWindow, filters]);

    // ---- loading ----------------------------------------------------------

    const resetForReload = useCallback(() => {
        abortOlder();
        setEntries([]);
        setError(null);
        setStatus(null);
        // A cursor is signed over the filters it was minted for, so any reload
        // invalidates it.
        applyCursor(null);
    }, [abortOlder, applyCursor]);

    const loadHistoricalLogs = useCallback(async () => {
        const gen = ++streamGenRef.current;
        if (!logWindow) {
            setError('Select a valid time range.');
            setCounts([]);
            setCountsStatus(null);
            setCountsError('Select a valid time range.');
            setCountsLoading(false);
            return;
        }

        resetForReload();
        setStreaming(false);

        const controller = new AbortController();
        streamAbortRef.current?.abort();
        streamAbortRef.current = controller;

        const collected: LogEntry[] = [];
        let newStatus: LogStatus | null = null;
        let nextCursor: string | null = null;

        try {
            await streamLogs(
                {
                    projectName,
                    deploymentId,
                    follow: false,
                    start: logWindow.start.toISOString(),
                    end: logWindow.end.toISOString(),
                    tail: pageSize,
                    signal: controller.signal,
                    ...filters,
                },
                {
                    onLine: (payload) => {
                        if (gen !== streamGenRef.current) return;
                        collected.push(parseLogLine(payload, seqRef.current++));
                    },
                    onStatus: (payload) => {
                        if (gen !== streamGenRef.current) return;
                        try {
                            newStatus = JSON.parse(payload) as LogStatus;
                        } catch {
                            newStatus = { reason: 'backend_unavailable' };
                        }
                    },
                    onPageComplete: ({ next_cursor }) => {
                        if (gen !== streamGenRef.current) return;
                        nextCursor = next_cursor;
                    },
                    onMalformed: (err) => {
                        console.warn('Malformed log payload; rendering the raw line', err);
                    },
                },
            );
            if (gen !== streamGenRef.current) return;
            // The backend already yields ascending, which is the order we keep.
            setEntries(collected);
            setStatus(newStatus);
            applyCursor(nextCursor);
        } catch (err) {
            if (isAbort(err) || gen !== streamGenRef.current) return;
            console.error('Failed to load logs:', err);
            setError(errorMessage(err));
        }
    }, [projectName, deploymentId, logWindow, pageSize, filters, resetForReload, applyCursor]);

    const startStreaming = useCallback(async () => {
        const gen = ++streamGenRef.current;
        resetForReload();
        setStreaming(true);

        const controller = new AbortController();
        streamAbortRef.current?.abort();
        streamAbortRef.current = controller;

        try {
            await streamLogs(
                {
                    projectName,
                    deploymentId,
                    follow: true,
                    // Bound the backlog to the selected window; without a start
                    // the backend falls back to the deployment's creation time.
                    start: rangeWindow?.start.toISOString(),
                    tail: pageSize,
                    signal: controller.signal,
                    ...filters,
                },
                {
                    onLine: (payload) => {
                        if (streamGenRef.current !== gen) return;
                        const entry = parseLogLine(payload, seqRef.current++);
                        setEntries((prev) => appendLive(prev, entry));
                    },
                    onStatus: (payload) => {
                        if (streamGenRef.current !== gen) return;
                        try {
                            setStatus(JSON.parse(payload) as LogStatus);
                        } catch {
                            setStatus({ reason: 'backend_unavailable' });
                        }
                    },
                    onBacklogComplete: ({ next_cursor }) => {
                        if (streamGenRef.current !== gen) return;
                        applyCursor(next_cursor);
                    },
                    // A long-lived follow stream refreshes its continuation
                    // token as the backend's own window advances.
                    onCursor: ({ next_cursor }) => {
                        if (streamGenRef.current !== gen) return;
                        applyCursor(next_cursor);
                    },
                    onMalformed: (err) => {
                        console.warn('Malformed log payload; rendering the raw line', err);
                    },
                },
            );
        } catch (err) {
            if (!isAbort(err) && streamGenRef.current === gen) {
                console.error('Log stream failed:', err);
                setError(errorMessage(err));
            }
        } finally {
            if (streamGenRef.current === gen) {
                if (streamAbortRef.current === controller) streamAbortRef.current = null;
                setStreaming(false);
            }
        }
    }, [projectName, deploymentId, rangeWindow, pageSize, filters, resetForReload, applyCursor]);

    const loadOlder = useCallback(async () => {
        if (loadingMoreRef.current) return;
        const cursor = paginationCursorRef.current;
        if (!cursor) return;

        olderAbortRef.current?.abort();
        const controller = new AbortController();
        olderAbortRef.current = controller;
        loadingMoreRef.current = true;
        setLoadingMore(true);

        const collected: LogEntry[] = [];
        let nextCursor: string | null = null;
        try {
            await streamLogs(
                {
                    projectName,
                    deploymentId,
                    // The cursor carries the window and the filters it was
                    // minted for, so nothing else about the query is restated.
                    cursor,
                    tail: pageSize,
                    signal: controller.signal,
                    ...filters,
                },
                {
                    onLine: (payload) => {
                        collected.push(parseLogLine(payload, seqRef.current++));
                    },
                    onPageComplete: ({ next_cursor }) => {
                        nextCursor = next_cursor;
                    },
                },
            );
            if (controller.signal.aborted) return;

            setEntries((prev) => {
                // Line ids are stable across requests, so identity alone
                // settles what a page has already delivered.
                const seen = new Set(prev.map(entryKey));
                const fresh = collected.filter((e) => !seen.has(entryKey(e)));
                if (fresh.length === 0) return prev;
                fresh.sort((a, b) => a.timestampMs - b.timestampMs);
                // Prepending: trim the newest end if the buffer overflows, since
                // the user is reading backwards.
                return trimTail(fresh.concat(prev));
            });
            applyCursor(nextCursor);
        } catch (err) {
            if (isAbort(err)) return;
            if (err instanceof StaleCursorError) {
                // The token no longer matches this deployment or filter set.
                // Paging back is simply over; it isn't a stream failure.
                applyCursor(null);
                return;
            }
            console.error('Failed to load older logs:', err);
        } finally {
            if (olderAbortRef.current === controller) {
                olderAbortRef.current = null;
                loadingMoreRef.current = false;
                setLoadingMore(false);
            }
        }
    }, [projectName, deploymentId, pageSize, filters, applyCursor]);

    // ---- effects ----------------------------------------------------------

    /**
     * Choose the feed mode and (re)open it. A preset range means "live tail
     * with this much history"; a custom range or a selected bucket means the
     * user is looking at a fixed past window, so don't follow.
     */
    const shouldStream = loggable && live && streamable
        && rangeValue !== 'custom' && selectedBucket === null;

    // eslint-disable-next-line react-hooks/exhaustive-deps -- the loaders are recreated per filter change; depending on them would re-open the stream twice
    useEffect(() => {
        if (!loggable) return undefined;
        if (shouldStream) void startStreaming();
        else void loadHistoricalLogs();
        return () => {
            stopStreaming();
            abortOlder();
        };
    }, [
        loggable, shouldStream, deploymentId, deploymentStatus,
        levelFilter, containerFilter, searchActive,
        rangeValue, customStart, customEnd, selectedBucket,
    ]);

    /**
     * Push the parsed query onto the wire params. Chips (levels, containers)
     * apply immediately since they come from discrete clicks; free text is
     * debounced so typing doesn't reopen the stream on every keystroke.
     */
    useEffect(() => {
        const parsed = parseQuery(queryText);
        const applyChips = () => {
            setLevelFilter((prev) => (sameValues(prev, parsed.levels) ? prev : parsed.levels));
            setContainerFilter((prev) => (sameValues(prev, parsed.containers) ? prev : parsed.containers));
        };
        applyChips();

        const nextSearch = parsed.search.trim();
        if (nextSearch === searchActive) return undefined;
        const handle = window.setTimeout(() => setSearchActive(nextSearch), 350);
        return () => window.clearTimeout(handle);
    }, [queryText, searchActive]);

    /** Any filter change invalidates a bucket selection tied to the old query. */
    useEffect(() => {
        setSelectedBucket(null);
    }, [levelFilter, containerFilter, searchActive]);

    /** Refresh the histogram when the range or filters move. */
    useEffect(() => {
        if (!loggable || !rangeWindow || !capabilities.supports_volume) return undefined;
        const handle = window.setTimeout(() => { void refreshCounts(); }, 250);
        return () => window.clearTimeout(handle);
    }, [loggable, rangeWindow, capabilities.supports_volume, refreshCounts]);

    const refresh = useCallback(() => {
        // Slide preset windows forward so lines streamed in since page load
        // fall back inside the range.
        setRangeNowTick((t) => t + 1);
        if (shouldStream) {
            // A live stream already delivers fresh lines; restarting it would
            // wipe paginated entries and reset scroll on every auto-refresh.
            if (streaming) return;
            void startStreaming();
        } else {
            void loadHistoricalLogs();
        }
    }, [shouldStream, streaming, startStreaming, loadHistoricalLogs]);

    // Keep the interval keyed only on the cadence, so filter changes don't
    // reset the timer.
    const refreshRef = useRef(refresh);
    useEffect(() => { refreshRef.current = refresh; }, [refresh]);

    useEffect(() => {
        if (autoRefreshSeconds <= 0) return undefined;
        const id = window.setInterval(() => {
            if (typeof document !== 'undefined' && document.hidden) return;
            refreshRef.current();
        }, autoRefreshSeconds * 1000);
        return () => window.clearInterval(id);
    }, [autoRefreshSeconds]);

    // ---- filter setters that clear a bucket selection ---------------------

    const changeRange = useCallback((value: string) => {
        setRangeValue(value);
        setSelectedBucket(null);
        if (value === 'custom') {
            const start = customStartRef.current ?? new Date(Date.now() - 6 * 60 * 60 * 1000);
            const end = customEndRef.current ?? new Date();
            setCustomStart(start);
            setCustomEnd(end);
            writeRangeToUrl(value, start, end);
            return;
        }
        writeRangeToUrl(value, null, null);
    }, []);

    const changeCustomRange = useCallback((start: Date | null, end: Date | null) => {
        setCustomStart(start);
        setCustomEnd(end);
        setSelectedBucket(null);
        writeRangeToUrl('custom', start, end);
    }, []);

    return {
        loggable,
        streamable,
        entries,
        capabilities,
        streaming,
        live,
        setLive,
        error,
        status,
        counts,
        countsLoading,
        countsError,
        countsStatus,
        volumeSupported,
        hasMore,
        loadingMore,
        loadOlder,
        rangeValue,
        changeRange,
        customStart,
        customEnd,
        changeCustomRange,
        rangeWindow,
        rangeStepSeconds,
        logWindow,
        anchorEnd,
        deploymentCreated,
        queryText,
        setQueryText,
        levelFilter,
        containerFilter,
        searchActive,
        selectedBucket,
        setSelectedBucket,
        autoRefreshSeconds,
        setAutoRefreshSeconds,
        refresh,
    };
}

/**
 * Append a streamed line, keeping the list ascending and free of repeats.
 *
 * A follow stream re-delivers the tail end of its own backlog, so the seam
 * between the two phases produces duplicates. They are always near the end —
 * a tail never re-sends something thousands of lines back — so a bounded scan
 * backwards catches them at constant cost, where checking the whole buffer
 * would be O(n) per line.
 *
 * The same scan finds the insertion point: Loki's tail can deliver slightly
 * out of order, but only by a little, so it settles in one comparison for the
 * common case.
 */
function appendLive(prev: LogEntry[], entry: LogEntry): LogEntry[] {
    const floor = Math.max(0, prev.length - LIVE_DEDUP_WINDOW);
    for (let i = prev.length - 1; i >= floor; i--) {
        if (prev[i].id === entry.id) return prev;
    }

    const last = prev.length > 0 ? prev[prev.length - 1] : null;
    if (!last || entry.timestampMs >= last.timestampMs) {
        return trimHead(prev.concat(entry));
    }
    let i = prev.length - 1;
    while (i > 0 && prev[i - 1].timestampMs > entry.timestampMs) i--;
    const next = prev.slice();
    next.splice(i, 0, entry);
    return trimHead(next);
}

/** Drop the oldest lines when the buffer overflows while tailing forward. */
function trimHead(entries: LogEntry[]): LogEntry[] {
    if (entries.length <= LOG_BUFFER_CAP) return entries;
    return entries.slice(entries.length - LOG_BUFFER_CAP);
}

/** Drop the newest lines when the buffer overflows while paging backwards. */
function trimTail(entries: LogEntry[]): LogEntry[] {
    if (entries.length <= LOG_BUFFER_CAP) return entries;
    return entries.slice(0, LOG_BUFFER_CAP);
}
