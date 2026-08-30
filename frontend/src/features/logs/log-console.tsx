import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Button as RButton, Combobox, Empty, Status, Tooltip } from '../../components/r-ui';
import { Icon } from '../../components/icon';
import { useToast } from '../../components/toast';
import {
    LOG_RANGE_PRESETS,
    describeLogStatus,
    formatDateTimeShort,
    orderedLevels,
    windowSpansDays,
} from './format';
import { findMatches } from './query';
import { QueryLine } from './query-line';
import { LogStream, type FocusRequest } from './log-stream';
import { useLogFeed } from './use-log-feed';
import type { LogEntry } from './types';
import { markersFromEvents, type LifecycleMarker } from './lifecycle';
import { fetchDeploymentEvents } from './api';
import { createTimelineCursorStore } from './timeline-cursor';

// Recharts and react-day-picker are heavy and each pull their own CSS; keep
// them out of the main bundle until the console actually needs them.
const LogVolumeChart = lazy(() => import('../log-volume-chart'));
const DateRangePopover = lazy(() => import('../date-range-popover'));

const AUTO_REFRESH_OPTIONS = [
    { value: '0', label: 'Auto: Off' },
    { value: '10', label: 'Auto: 10s' },
    { value: '30', label: 'Auto: 30s' },
    { value: '60', label: 'Auto: 1m' },
    { value: '300', label: 'Auto: 5m' },
];

/**
 * Live/pause on the left, follow lock on the right. Two related toggles that
 * belong to one control rather than two competing buttons.
 */
function StreamToggle({
    live, streaming, following, onToggleLive, onToggleFollow, disabled,
}: {
    live: boolean;
    streaming: boolean;
    following: boolean;
    onToggleLive: () => void;
    onToggleFollow: () => void;
    disabled: boolean;
}) {
    return (
        <div className="r-split-btn">
            <button
                type="button"
                className={`r-split-btn-main${live && streaming ? ' active' : ''}`}
                onClick={onToggleLive}
                aria-pressed={live}
                disabled={disabled}
                title={live ? 'Pause the live stream' : 'Resume live streaming'}
            >
                <span className="dot" />
                <span>{live ? 'Live' : 'Paused'}</span>
            </button>
            <button
                type="button"
                className={`r-split-btn-side${following ? ' active' : ''}`}
                onClick={onToggleFollow}
                aria-pressed={following}
                aria-label="Follow newest lines"
                title={following ? 'Following newest lines' : 'Not following'}
            >
                <Icon name="chevd" size={12} />
            </button>
        </div>
    );
}

export interface LogConsoleProps {
    projectName: string;
    deploymentId: string;
    deploymentStatus: string;
    deploymentCompletedAt?: string | null;
    deploymentCreated?: string | null;
    /** Container names declared by the deployment, for the container filter. */
    containers?: string[];
    /** Deployment events drawn on the volume rail's time axis. */
    /** Deployment metadata panels, shown in the slide-over details drawer. */
    details?: React.ReactNode;
    /** `page` fills the viewport; `embedded` sits inside the deployment tab. */
    variant?: 'page' | 'embedded';
    /** Rendered at the far left of the status bar (breadcrumb, back link). */
    lead?: React.ReactNode;
}

export function LogConsole({
    projectName,
    deploymentId,
    deploymentStatus,
    deploymentCompletedAt,
    deploymentCreated,
    containers = [],
    details,
    variant = 'embedded',
    lead,
}: LogConsoleProps) {
    const feed = useLogFeed({
        projectName,
        deploymentId,
        deploymentStatus,
        deploymentCompletedAt,
        deploymentCreated,
    });
    const { showToast } = useToast();

    /**
     * Markers from the event log — the rail's only source. Being a history, a
     * deployment that flapped contributes every transition rather than just the
     * state it ended in. Replica-level markers appear once the backends emit
     * replica events.
     */
    const [eventMarkers, setEventMarkers] = useState<LifecycleMarker[]>([]);

    const [wrap, setWrap] = useState(true);
    const [following, setFollowing] = useState(true);
    const [railOpen, setRailOpen] = useState(true);
    const [detailsOpen, setDetailsOpen] = useState(false);
    const [expandedIds, setExpandedIds] = useState<Set<string>>(() => new Set());
    const [matchCursor, setMatchCursor] = useState(0);
    // Written by the stream on scroll/hover, read by the volume rail's cursor
    // overlay. Created once so both sides keep the same store across renders.
    const [timelineCursor] = useState(createTimelineCursorStore);
    const [focusRequest, setFocusRequest] = useState<FocusRequest | null>(null);
    const focusTokenRef = useRef(0);
    const queryInputRef = useRef<HTMLInputElement>(null);

    const { entries, searchActive, rangeWindow, capabilities } = feed;

    // Refetched when the deployment's status changes, which is exactly when a
    // new status event exists to read.
    useEffect(() => {
        const controller = new AbortController();
        void (async () => {
            try {
                const page = await fetchDeploymentEvents({
                    projectName,
                    deploymentId,
                    limit: 200,
                    signal: controller.signal,
                });
                setEventMarkers(markersFromEvents(page.events));
            } catch (err) {
                if (err instanceof Error && err.name === 'AbortError') return;
                // The rail simply has no markers on failure, which is the
                // honest rendering of "we could not read the log".
                // here degrades the timeline rather than breaking the console.
                console.warn('Could not load deployment events:', err);
            }
        })();
        return () => controller.abort();
    }, [projectName, deploymentId, deploymentStatus]);

    // One source, so nothing to merge: the rail shows what was recorded.
    const railMarkers = eventMarkers;

    /**
     * The rail is the time axis, and log volume is only one thing that can sit
     * on it. Deployment events come from a different source entirely, so gating
     * the whole rail on `supports_volume` would hide the deployment's timeline
     * on every backend without a historical log store — which is most of them.
     */
    const railHasContent = feed.volumeSupported || railMarkers.length > 0;

    // Reset expansion when the underlying set of lines is replaced wholesale.
    useEffect(() => {
        setExpandedIds(new Set());
    }, [deploymentId, feed.rangeValue, feed.levelFilter, feed.containerFilter, searchActive]);

    /** Every (entry index, character offset) pair the search term hits. */
    const matches = useMemo(() => {
        if (!searchActive) return [];
        const found: { index: number; id: string; offset: number }[] = [];
        for (let i = 0; i < entries.length; i++) {
            for (const [start] of findMatches(entries[i].raw, searchActive)) {
                found.push({ index: i, id: entries[i].id, offset: start });
            }
        }
        return found;
    }, [entries, searchActive]);

    useEffect(() => { setMatchCursor(0); }, [searchActive]);

    const activeMatch = matches.length > 0
        ? matches[Math.min(matchCursor, matches.length - 1)]
        : null;

    const jumpToMatch = useCallback((delta: number) => {
        if (matches.length === 0) return;
        setMatchCursor((prev) => {
            const next = (prev + delta + matches.length) % matches.length;
            setFocusRequest({ index: matches[next].index, token: ++focusTokenRef.current });
            return next;
        });
    }, [matches]);

    const toggleExpanded = useCallback((id: string) => {
        setExpandedIds((prev) => {
            const next = new Set(prev);
            if (next.has(id)) next.delete(id);
            else next.add(id);
            return next;
        });
    }, []);

    const copyLine = useCallback(async (entry: LogEntry) => {
        const text = entry.iso ? `${entry.iso} ${entry.raw}` : entry.raw;
        try {
            await navigator.clipboard.writeText(text);
            showToast('Line copied', 'success');
        } catch {
            showToast('Could not copy to the clipboard', 'error');
        }
    }, [showToast]);

    const copyAll = useCallback(async () => {
        const text = entries.map((e) => (e.iso ? `${e.iso} ${e.raw}` : e.raw)).join('\n');
        try {
            await navigator.clipboard.writeText(text);
            showToast(`Copied ${entries.length.toLocaleString()} lines`, 'success');
        } catch {
            showToast('Could not copy to the clipboard', 'error');
        }
    }, [entries, showToast]);

    /** Serialise the loaded buffer to a file the user can open elsewhere. */
    const download = useCallback(() => {
        const text = entries.map((e) => (e.iso ? `${e.iso} ${e.raw}` : e.raw)).join('\n');
        const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
        const link = document.createElement('a');
        link.href = url;
        link.download = `${projectName}-${deploymentId}.log`;
        link.click();
        URL.revokeObjectURL(url);
    }, [entries, projectName, deploymentId]);

    // ---- keyboard ---------------------------------------------------------

    useEffect(() => {
        const onKeyDown = (e: KeyboardEvent) => {
            const target = e.target as HTMLElement | null;
            const typing = !!target && (
                target.tagName === 'INPUT'
                || target.tagName === 'TEXTAREA'
                || target.isContentEditable
            );

            if (e.key === 'Escape' && typing) {
                (target as HTMLInputElement).blur();
                return;
            }
            if (typing || e.metaKey || e.ctrlKey || e.altKey) return;

            switch (e.key) {
                case '/':
                    e.preventDefault();
                    queryInputRef.current?.focus();
                    break;
                case 'f':
                    e.preventDefault();
                    setFollowing((v) => !v);
                    break;
                case 'w':
                    e.preventDefault();
                    setWrap((v) => !v);
                    break;
                case 'g':
                    e.preventDefault();
                    if (entries.length > 0) {
                        setFollowing(false);
                        setFocusRequest({ index: 0, token: ++focusTokenRef.current });
                    }
                    break;
                case 'G':
                    e.preventDefault();
                    setFollowing(true);
                    break;
                case 'n':
                    e.preventDefault();
                    jumpToMatch(1);
                    break;
                case 'N':
                    e.preventDefault();
                    jumpToMatch(-1);
                    break;
                default:
                    break;
            }
        };
        document.addEventListener('keydown', onKeyDown);
        return () => document.removeEventListener('keydown', onKeyDown);
    }, [entries.length, jumpToMatch]);

    // A deployment that has not reached a loggable state has nothing to stream.
    // Embedded, the surrounding page still explains itself and rendering
    // nothing is right; on its own route it would be a blank screen.
    if (!feed.loggable) {
        if (variant !== 'page') return null;
        return (
            <div className="r-logc r-logc-page">
                <div className="r-logc-bar">
                    <div className="r-logc-bar-left">
                        {lead}
                        <Status status={deploymentStatus} />
                    </div>
                </div>
                <Empty title="No logs yet">
                    This deployment is {deploymentStatus.toLowerCase()}; logs start once its
                    workload is running.
                </Empty>
            </div>
        );
    }

    const showDay = windowSpansDays(rangeWindow);
    const emptyMessage = describeLogStatus(feed.status)
        ?? (feed.streaming ? 'Waiting for logs…' : 'No logs in the selected range.');

    return (
        <div className={`r-logc r-logc-${variant}`}>
            <div className="r-logc-bar">
                <div className="r-logc-bar-left">
                    {lead}
                    <Status status={deploymentStatus} />
                    <span className="r-logc-count">
                        {entries.length.toLocaleString()} {entries.length === 1 ? 'line' : 'lines'}
                    </span>
                </div>
                <div className="r-logc-bar-right">
                    <StreamToggle
                        live={feed.live}
                        streaming={feed.streaming}
                        following={following}
                        onToggleLive={() => feed.setLive(!feed.live)}
                        onToggleFollow={() => setFollowing((v) => !v)}
                        disabled={!feed.streamable}
                    />
                    <Tooltip content={wrap ? 'Wrap long lines (w)' : 'Do not wrap lines (w)'}>
                        <button
                            type="button"
                            className={wrap ? 'r-logc-icon-btn is-on' : 'r-logc-icon-btn'}
                            onClick={() => setWrap((v) => !v)}
                            aria-pressed={wrap}
                            aria-label="Toggle line wrapping"
                        >
                            <Icon name="wrap" size={13} />
                        </button>
                    </Tooltip>
                    <Tooltip content="Copy loaded lines">
                        <button
                            type="button"
                            className="r-logc-icon-btn"
                            onClick={copyAll}
                            disabled={entries.length === 0}
                            aria-label="Copy loaded lines"
                        >
                            <Icon name="copy" size={13} />
                        </button>
                    </Tooltip>
                    <Tooltip content="Download loaded lines">
                        <button
                            type="button"
                            className="r-logc-icon-btn"
                            onClick={download}
                            disabled={entries.length === 0}
                            aria-label="Download loaded lines"
                        >
                            <Icon name="download" size={13} />
                        </button>
                    </Tooltip>
                    {details && (
                        <RButton size="sm" onClick={() => setDetailsOpen((v) => !v)}>
                            Details
                        </RButton>
                    )}
                </div>
            </div>

            {feed.error && (
                <div className="r-alert err r-logc-alert">
                    <Icon name="info" size={14} />
                    <div style={{ flex: 1 }}>Error: {feed.error}</div>
                </div>
            )}

            <div className={railOpen ? 'r-logc-rail is-open' : 'r-logc-rail'}>
                <button
                    type="button"
                    className="r-logc-rail-toggle"
                    onClick={() => setRailOpen((v) => !v)}
                    aria-expanded={railOpen}
                    disabled={!railHasContent}
                    title={railHasContent
                        ? 'Toggle the timeline'
                        : 'Nothing to show on the timeline yet'}
                >
                    <Icon name={railOpen ? 'chevd' : 'chev'} size={11} />
                    <span>Timeline</span>
                </button>
                {railHasContent && railOpen && (
                    <div className="r-logc-rail-body r-logs-chart">
                        <Suspense fallback={<div className="r-logc-rail-fallback">Loading chart…</div>}>
                            <LogVolumeChart
                                counts={feed.counts}
                                levels={orderedLevels(capabilities.levels)}
                                loading={feed.countsLoading}
                                error={feed.countsError}
                                status={feed.countsStatus}
                                rangeStartMs={rangeWindow?.start.getTime() ?? 0}
                                rangeEndMs={rangeWindow?.end.getTime() ?? 0}
                                stepSeconds={feed.rangeStepSeconds}
                                onSelectBucket={feed.setSelectedBucket}
                                selectedBucketTs={feed.selectedBucket?.endMs ?? null}
                                height={72}
                                markers={railMarkers}
                                timelineCursor={timelineCursor}
                            />
                        </Suspense>
                    </div>
                )}
            </div>

            <div className="r-logc-controls">
                <QueryLine
                    value={feed.queryText}
                    onChange={feed.setQueryText}
                    levels={capabilities.levels}
                    containers={containers}
                    inputRef={queryInputRef}
                    matchCount={matches.length}
                    matchPosition={matches.length > 0 ? Math.min(matchCursor, matches.length - 1) : 0}
                    onPrevMatch={() => jumpToMatch(-1)}
                    onNextMatch={() => jumpToMatch(1)}
                />
                <div className="r-logc-range">
                    <Combobox
                        value={feed.rangeValue}
                        options={LOG_RANGE_PRESETS.map((option) => ({
                            value: option.value,
                            label: option.value === 'custom' ? 'Custom range' : `Last ${option.label}`,
                        }))}
                        onChange={feed.changeRange}
                    />
                </div>
                {feed.rangeValue === 'custom' && (
                    <Suspense fallback={null}>
                        <DateRangePopover
                            start={feed.customStart}
                            end={feed.customEnd}
                            onChange={feed.changeCustomRange}
                            // Nothing before the deployment existed can have
                            // logs, and a future end just truncates to now.
                            disabled={[
                                ...(deploymentCreated ? [{ before: new Date(deploymentCreated) }] : []),
                                { after: new Date() },
                            ]}
                        />
                    </Suspense>
                )}
                {feed.anchorEnd && feed.rangeValue !== 'custom' && (
                    <Tooltip content="This deployment is terminal, so preset ranges end at its stop time rather than now.">
                        <span className="r-logc-anchor">ending {formatDateTimeShort(feed.anchorEnd)}</span>
                    </Tooltip>
                )}
                <RButton size="sm" icon="refresh" onClick={feed.refresh}>Refresh</RButton>
                <div className="r-logc-autorefresh">
                    <Combobox
                        value={String(feed.autoRefreshSeconds)}
                        options={AUTO_REFRESH_OPTIONS}
                        onChange={(v) => {
                            const parsed = Number.parseInt(v, 10);
                            feed.setAutoRefreshSeconds(Number.isNaN(parsed) ? 0 : parsed);
                        }}
                    />
                </div>
            </div>

            {feed.selectedBucket && (
                <div className="r-logc-bucket" role="status">
                    <span>
                        Narrowed to {new Date(feed.selectedBucket.startMs).toLocaleTimeString()}
                        –{new Date(feed.selectedBucket.endMs).toLocaleTimeString()}
                    </span>
                    <button
                        type="button"
                        onClick={() => feed.setSelectedBucket(null)}
                        aria-label="Clear the time narrowing"
                    >
                        <Icon name="close" size={11} />
                    </button>
                </div>
            )}

            <LogStream
                entries={entries}
                rangeWindow={rangeWindow}
                openEnded={feed.streaming}
                showDay={showDay}
                wrap={wrap}
                search={searchActive}
                expandedIds={expandedIds}
                onToggleExpand={toggleExpanded}
                onCopyLine={copyLine}
                following={following}
                onFollowingChange={setFollowing}
                hasMore={feed.hasMore}
                loadingMore={feed.loadingMore}
                onLoadOlder={feed.loadOlder}
                activeMatch={activeMatch ? { id: activeMatch.id, offset: activeMatch.offset } : null}
                focusRequest={focusRequest}
                empty={emptyMessage}
                timelineCursor={timelineCursor}
            />

            {details && detailsOpen && (
                <>
                    <div className="r-logc-scrim" onClick={() => setDetailsOpen(false)} />
                    <aside className="r-logc-details" aria-label="Deployment details">
                        <div className="r-logc-details-head">
                            <span>Details</span>
                            <button type="button" onClick={() => setDetailsOpen(false)} aria-label="Close details">
                                <Icon name="close" size={13} />
                            </button>
                        </div>
                        <div className="r-logc-details-body">{details}</div>
                    </aside>
                </>
            )}
        </div>
    );
}

export default LogConsole;
