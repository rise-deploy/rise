import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { NOTABLE_LEVELS, levelSeverityRank } from './format';
import { LOG_FOLLOW_THRESHOLD_PX } from './use-log-feed';
import { LogRow } from './log-row';
import type { LogEntry, LogWindow } from './types';
import type { TimelineCursorStore } from './timeline-cursor';

/** Row height guess before measurement; a single unwrapped line at 12px/1.55. */
const ESTIMATED_ROW_PX = 20;

/** Vertical resolution of the heat track, in ticks. */
const HEAT_SLOTS = 160;

/** Distance from the top, in px, that triggers loading the previous page. */
const LOAD_OLDER_THRESHOLD_PX = 240;

export interface FocusRequest {
    index: number;
    /** Changes on every request so repeat jumps to the same row still fire. */
    token: number;
}

export interface LogStreamProps {
    entries: LogEntry[];
    /** Window the user selected, for dimming rows paginated in from outside it. */
    rangeWindow: LogWindow | null;
    /**
     * A live preset range means "the last N minutes up to now", so its end
     * keeps moving. Without this, every line that streams in after the window
     * was resolved would render as though it fell outside the range.
     */
    openEnded: boolean;
    showDay: boolean;
    wrap: boolean;
    search: string;
    expandedIds: Set<string>;
    onToggleExpand: (id: string) => void;
    onCopyLine: (entry: LogEntry) => void;
    following: boolean;
    onFollowingChange: (following: boolean) => void;
    hasMore: boolean;
    loadingMore: boolean;
    onLoadOlder: () => void;
    /** Character offset of the focused match, keyed by entry id. */
    activeMatch: { id: string; offset: number } | null;
    focusRequest: FocusRequest | null;
    /**
     * Receives the time span the visible rows cover, and the timestamp of the
     * hovered row, so the volume rail can mark where the reader is.
     */
    timelineCursor?: TimelineCursorStore;
    /** Rendered when there are no entries at all. */
    empty: React.ReactNode;
}

export function LogStream({
    entries,
    rangeWindow,
    openEnded,
    showDay,
    wrap,
    search,
    expandedIds,
    onToggleExpand,
    onCopyLine,
    following,
    onFollowingChange,
    hasMore,
    loadingMore,
    onLoadOlder,
    activeMatch,
    focusRequest,
    empty,
    timelineCursor,
}: LogStreamProps) {
    const scrollRef = useRef<HTMLDivElement>(null);
    // Distance from the bottom captured before a prepend, so scroll position
    // can be restored once the older page has been measured.
    const anchorFromBottomRef = useRef<number | null>(null);
    // Suppress the follow-off detection while we move the scroll ourselves.
    const programmaticScrollRef = useRef(false);

    const virtualizer = useVirtualizer({
        count: entries.length,
        getScrollElement: () => scrollRef.current,
        estimateSize: () => ESTIMATED_ROW_PX,
        overscan: 24,
        getItemKey: (index) => entries[index].id,
        // Dynamic measurement fires from a ref callback while React is still
        // rendering, so the default synchronous flush both warns and forces one
        // re-render per measured row. Batching costs a frame of settle on rows
        // whose height was mis-estimated, which is invisible next to the churn.
        useFlushSync: false,
    });

    const items = virtualizer.getVirtualItems();
    const totalSize = virtualizer.getTotalSize();
    // The heat track maps buffer position, so it only means anything once the
    // buffer is taller than the viewport.
    const [scrolls, setScrolls] = useState(false);

    const rangeStartMs = rangeWindow?.start.getTime() ?? 0;
    const rangeEndMs = openEnded
        ? Number.POSITIVE_INFINITY
        : rangeWindow?.end.getTime() ?? 0;

    /**
     * Ticks for the heat track: one per notable line at its relative offset,
     * collapsed to a fixed number of slots. Without the collapse a busy buffer
     * paints a solid bar — and renders thousands of nodes to say nothing.
     * The most severe level in a slot wins, so an error is never hidden by the
     * warnings around it.
     */
    const heatTicks = useMemo(() => {
        if (entries.length === 0) return [];
        const slots = new Map<number, string>();
        for (let i = 0; i < entries.length; i++) {
            const level = entries[i].level;
            if (!NOTABLE_LEVELS.has(level)) continue;
            const slot = Math.floor((i / entries.length) * HEAT_SLOTS);
            const current = slots.get(slot);
            if (current === undefined || levelSeverityRank(level) > levelSeverityRank(current)) {
                slots.set(slot, level);
            }
        }
        return [...slots.entries()].map(([slot, level]) => ({
            pct: (slot / HEAT_SLOTS) * 100,
            level,
        }));
    }, [entries]);

    useLayoutEffect(() => {
        const el = scrollRef.current;
        if (!el) return;
        setScrolls(el.scrollHeight > el.clientHeight + 8);
    }, [totalSize, entries.length]);

    const scrollToBottom = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        programmaticScrollRef.current = true;
        el.scrollTop = el.scrollHeight;
        requestAnimationFrame(() => { programmaticScrollRef.current = false; });
    }, []);

    /** Stick to the newest line while following. */
    useLayoutEffect(() => {
        if (!following) return;
        scrollToBottom();
    }, [following, entries.length, totalSize, scrollToBottom]);

    /** Restore the reading position after an older page is prepended. */
    useLayoutEffect(() => {
        const fromBottom = anchorFromBottomRef.current;
        if (fromBottom === null) return;
        const el = scrollRef.current;
        if (!el) return;
        anchorFromBottomRef.current = null;
        programmaticScrollRef.current = true;
        el.scrollTop = el.scrollHeight - fromBottom;
        requestAnimationFrame(() => { programmaticScrollRef.current = false; });
    }, [entries.length, totalSize]);

    /** Jump to a specific row (search navigation, heat-track clicks). */
    useEffect(() => {
        if (!focusRequest) return;
        onFollowingChange(false);
        programmaticScrollRef.current = true;
        virtualizer.scrollToIndex(focusRequest.index, { align: 'center' });
        requestAnimationFrame(() => { programmaticScrollRef.current = false; });
        // `virtualizer` identity changes every render; the token is the trigger.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [focusRequest?.token]);

    /**
     * Timestamp of the row under the pointer. Kept in a ref rather than state:
     * it only ever feeds the store, so re-rendering the list on hover would
     * buy nothing.
     */
    const hoverMsRef = useRef<number | null>(null);
    const cursorFrameRef = useRef(0);

    /**
     * Publish the visible span. Rows are oldest-first, so the span runs from
     * the first timestamped row on screen to the last one; rows whose line
     * carried no parseable timestamp are skipped rather than reported as the
     * epoch.
     */
    const publishCursor = useCallback(() => {
        if (!timelineCursor) return;
        const el = scrollRef.current;
        if (!el || entries.length === 0) {
            timelineCursor.set(null);
            return;
        }
        const first = virtualizer.getVirtualItemForOffset(el.scrollTop);
        const last = virtualizer.getVirtualItemForOffset(el.scrollTop + el.clientHeight);
        if (!first || !last) {
            timelineCursor.set(null);
            return;
        }
        let startMs = 0;
        for (let i = first.index; i <= last.index; i++) {
            if (entries[i]?.timestampMs > 0) { startMs = entries[i].timestampMs; break; }
        }
        let endMs = 0;
        for (let i = last.index; i >= first.index; i--) {
            if (entries[i]?.timestampMs > 0) { endMs = entries[i].timestampMs; break; }
        }
        if (startMs === 0 || endMs === 0) {
            timelineCursor.set(null);
            return;
        }
        timelineCursor.set({ startMs, endMs, hoverMs: hoverMsRef.current });
        // `virtualizer` is a new object every render; its scroll geometry is
        // read live above, so it does not belong in the dependency list.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [entries, timelineCursor]);

    /**
     * Coalesce the scroll/hover bursts into at most one publish per frame.
     *
     * Reads the publisher through a ref so this callback keeps one identity for
     * the life of the component: a live tail hands `entries` a new array on
     * every batch, and a `scheduleCursor` that changed with it would tear down
     * and re-create the observer below on each one.
     */
    const publishCursorRef = useRef(publishCursor);
    useLayoutEffect(() => { publishCursorRef.current = publishCursor; });
    const scheduleCursor = useCallback(() => {
        if (cursorFrameRef.current) return;
        cursorFrameRef.current = requestAnimationFrame(() => {
            cursorFrameRef.current = 0;
            publishCursorRef.current();
        });
    }, []);

    /** Republish when the buffer changes under a stationary viewport. */
    useEffect(() => { scheduleCursor(); }, [scheduleCursor, entries, totalSize]);

    /** A resized viewport shows a different set of rows without scrolling. */
    useEffect(() => {
        const el = scrollRef.current;
        if (!el || typeof ResizeObserver === 'undefined') return undefined;
        const observer = new ResizeObserver(scheduleCursor);
        observer.observe(el);
        return () => observer.disconnect();
    }, [scheduleCursor]);

    useEffect(() => () => {
        if (cursorFrameRef.current) cancelAnimationFrame(cursorFrameRef.current);
        // Clearing the handle matters as much as cancelling it: a remount
        // reuses the same ref, and a stale id left behind makes every later
        // `scheduleCursor` believe a frame is already pending.
        cursorFrameRef.current = 0;
        timelineCursor?.set(null);
    }, [timelineCursor]);

    /**
     * Hover is tracked by delegation off the rows container. Per-row handlers
     * would give every one of them a fresh callback identity on each render
     * and defeat `LogRow`'s memoization.
     */
    const handleRowHover = useCallback((event: React.MouseEvent<HTMLDivElement>) => {
        if (!timelineCursor) return;
        const slot = (event.target as HTMLElement).closest?.('[data-index]');
        const index = slot ? Number(slot.getAttribute('data-index')) : NaN;
        const ms = Number.isNaN(index) ? null : entries[index]?.timestampMs ?? null;
        const next = ms && ms > 0 ? ms : null;
        if (hoverMsRef.current === next) return;
        hoverMsRef.current = next;
        scheduleCursor();
    }, [entries, scheduleCursor, timelineCursor]);

    const handleRowsLeave = useCallback(() => {
        if (!timelineCursor || hoverMsRef.current === null) return;
        hoverMsRef.current = null;
        scheduleCursor();
    }, [scheduleCursor, timelineCursor]);

    const handleScroll = useCallback(() => {
        const el = scrollRef.current;
        if (!el) return;
        // Programmatic scrolls still move the reader, so the rail follows them
        // even though they must not switch follow mode off.
        scheduleCursor();
        if (programmaticScrollRef.current) return;

        const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
        const atBottom = distanceFromBottom <= LOG_FOLLOW_THRESHOLD_PX;
        if (atBottom !== following) onFollowingChange(atBottom);

        if (el.scrollTop <= LOAD_OLDER_THRESHOLD_PX && hasMore && !loadingMore) {
            anchorFromBottomRef.current = el.scrollHeight - el.scrollTop;
            onLoadOlder();
        }
    }, [following, onFollowingChange, hasMore, loadingMore, onLoadOlder, scheduleCursor]);

    const jumpToTick = useCallback((pct: number) => {
        const index = Math.min(entries.length - 1, Math.floor((pct / 100) * entries.length));
        if (index < 0) return;
        onFollowingChange(false);
        programmaticScrollRef.current = true;
        virtualizer.scrollToIndex(index, { align: 'center' });
        requestAnimationFrame(() => { programmaticScrollRef.current = false; });
    }, [entries.length, onFollowingChange, virtualizer]);

    return (
        <div className="r-logc-stream-wrap">
            <div
                ref={scrollRef}
                className={wrap ? 'r-logc-stream' : 'r-logc-stream is-nowrap'}
                onScroll={handleScroll}
                role="log"
                aria-label="Deployment log lines"
                aria-live={following ? 'polite' : 'off'}
                tabIndex={0}
            >
                {entries.length === 0 ? (
                    <div className="r-logc-empty">{empty}</div>
                ) : (
                    <>
                        {/* Oldest first, so the boundary marker belongs above
                            the rows: older lines load as you scroll up. */}
                        <div className={hasMore ? 'r-logc-loader' : 'r-logc-loader is-end'}>
                            {!hasMore ? 'Start of available logs' : loadingMore ? (
                                <>
                                    <span className="r-spinner" style={{ width: 12, height: 12, borderWidth: 1.5 }} />
                                    Loading older…
                                </>
                            ) : 'Scroll up for older lines'}
                        </div>
                        <div
                            className="r-logc-rows"
                            style={{ height: totalSize }}
                            onMouseOver={handleRowHover}
                            onMouseLeave={handleRowsLeave}
                        >
                            {items.map((item) => {
                                const entry = entries[item.index];
                                const outOfRange = rangeWindow !== null
                                    && entry.timestampMs > 0
                                    && (entry.timestampMs < rangeStartMs || entry.timestampMs > rangeEndMs);
                                return (
                                    <div
                                        key={item.key}
                                        data-index={item.index}
                                        ref={virtualizer.measureElement}
                                        className="r-logc-slot"
                                        style={{ transform: `translateY(${item.start}px)` }}
                                    >
                                        <LogRow
                                            entry={entry}
                                            showDay={showDay}
                                            outOfRange={outOfRange}
                                            expanded={expandedIds.has(entry.id)}
                                            onToggleExpand={onToggleExpand}
                                            onCopy={onCopyLine}
                                            search={search}
                                            activeMatchOffset={
                                                activeMatch?.id === entry.id ? activeMatch.offset : null
                                            }
                                        />
                                    </div>
                                );
                            })}
                        </div>
                    </>
                )}
            </div>

            {scrolls && heatTicks.length > 0 && (
                <div
                    className="r-logc-heat"
                    role="presentation"
                    title={`${heatTicks.length} warning or error lines`}
                    onClick={(e) => {
                        const rect = e.currentTarget.getBoundingClientRect();
                        jumpToTick(((e.clientY - rect.top) / rect.height) * 100);
                    }}
                >
                    {heatTicks.map((tick, i) => (
                        <span
                            key={i}
                            className={`r-logc-heat-tick lv-${tick.level}`}
                            style={{ top: `${tick.pct}%` }}
                        />
                    ))}
                </div>
            )}

            {!following && entries.length > 0 && (
                <button
                    type="button"
                    className="r-logc-jump"
                    onClick={() => { onFollowingChange(true); scrollToBottom(); }}
                >
                    Jump to latest ↓
                </button>
            )}
        </div>
    );
}
