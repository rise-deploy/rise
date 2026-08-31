import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { Bar, BarChart, CartesianGrid, Cell, ReferenceLine, ResponsiveContainer, Tooltip as RechartsTooltip, XAxis, YAxis } from 'recharts';
import { formatDate } from '../lib/utils';
import { Tooltip } from '../components/r-ui';
import type { TimelineCursor, TimelineCursorStore } from './logs/timeline-cursor';

// Color for any level the backend advertises. Driven by CSS variables so
// dark/light mode and design-token changes don't need code edits. Levels
// not in the table fall back to `--r-log-chart-unknown` (neutral grey).
const LEVEL_COLOR_VAR = {
    info: '--r-log-chart-info',
    notice: '--r-log-chart-info',
    warn: '--r-log-chart-warn',
    error: '--r-log-chart-error',
    critical: '--r-log-chart-critical',
    fatal: '--r-log-chart-fatal',
    debug: '--r-log-chart-debug',
    trace: '--r-log-chart-trace',
    unknown: '--r-log-chart-unknown',
};

const MARKER_COLOR = {
    rollout: 'var(--accent)',
    up: 'var(--ok)',
    done: 'var(--ok)',
    restart: 'var(--warn)',
    failed: 'var(--err)',
};

function markerColor(kind) {
    return MARKER_COLOR[kind] || 'var(--text-soft)';
}

function levelColor(level) {
    const v = LEVEL_COLOR_VAR[level] || '--r-log-chart-unknown';
    return `var(${v})`;
}

function levelLabel(level) {
    return level.charAt(0).toUpperCase() + level.slice(1);
}

function formatTickLabel(ms, rangeMs) {
    const d = new Date(ms);
    const pad = (n) => String(n).padStart(2, '0');
    if (rangeMs <= 24 * 3600 * 1000) {
        return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    }
    return `${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function LogChartTooltip({ active, payload, stepSeconds, levels }: { active?: boolean; payload?: any[]; stepSeconds: number; levels: string[] }) {
    if (!active || !payload || payload.length === 0) return null;
    const b = payload[0].payload;
    // Each bucket counts the preceding window (T - step, T], so the bar at
    // ts=T represents lines from start = T - step up to end = T.
    const end = new Date(b.ts);
    const start = new Date(b.ts - stepSeconds * 1000);
    return (
        <div className="r-logs-chart-tip">
            <div className="r-logs-chart-tip-title">
                {formatDate(start.toISOString())} – {formatTickLabel(end.getTime(), 0)}
            </div>
            {levels.map((level) => (
                <div key={level} className="r-logs-chart-tip-row">
                    <span className="dot" style={{ background: levelColor(level) }} />
                    <span className="lbl">{levelLabel(level)}</span>
                    <span className="val">{b[level] || 0}</span>
                </div>
            ))}
            <div className="r-logs-chart-tip-total">
                <span className="lbl">Total</span>
                <span className="val">{b.total || 0}</span>
            </div>
            <div className="r-logs-chart-tip-hint">Click bar to filter logs</div>
        </div>
    );
}

export default function LogVolumeChart({ counts, levels, loading, error, status, rangeStartMs, rangeEndMs, stepSeconds, onSelectBucket, selectedBucketTs, height = 96, markers = [], timelineCursor = null }) {
    // Flatten each bucket's `by_level` map into top-level keys so Recharts'
    // `<Bar dataKey="info">` can read them directly.
    const data = useMemo(
        () =>
            counts.map((b) => {
                const flat = {
                    ts: new Date(b.timestamp).getTime(),
                    total: b.total,
                };
                const byLevel = b.by_level || {};
                for (const k of Object.keys(byLevel)) {
                    flat[k] = byLevel[k];
                }
                return flat;
            }),
        [counts],
    );
    const totalSum = useMemo(() => data.reduce((sum, b) => sum + (b.total || 0), 0), [data]);
    // The x-axis domain is the data extent, not the picked range, so a marker
    // outside it has nowhere to sit and would read as though it happened at
    // whichever edge it was clamped to.
    const domain = useMemo(() => {
        if (data.length > 0) return { min: data[0].ts, max: data[data.length - 1].ts };
        // No volume to chart, but markers may still need an axis — a backend
        // without a historical log store reports no counts and still has a
        // deployment timeline.
        if (rangeStartMs && rangeEndMs && rangeEndMs > rangeStartMs) {
            return { min: rangeStartMs, max: rangeEndMs };
        }
        return null;
    }, [data, rangeStartMs, rangeEndMs]);
    /**
     * The marker lane and the reader cursor have to line up with the bars, and
     * only Recharts knows where its plot area actually is: the YAxis reserves
     * width for its tick labels, so the inset is not something the caller can
     * compute. Read it off the rendered axis lines — the x-axis line spans
     * exactly the plot width, the y-axis line its height.
     */
    const bodyRef = useRef(null);
    const [plot, setPlot] = useState(null);
    const measurePlot = useCallback(() => {
        const body = bodyRef.current;
        if (!body) return;
        const axis = body.querySelector('.recharts-xAxis .recharts-cartesian-axis-line');
        if (!axis) return;
        const a = axis.getBoundingClientRect();
        const b = body.getBoundingClientRect();
        if (a.width <= 0) return;
        // The vertical extent is optional: the marker lane only needs the
        // horizontal one, so a chart rendered without a y-axis line still
        // positions its markers and simply carries no cursor overlay.
        const yAxis = body.querySelector('.recharts-yAxis .recharts-cartesian-axis-line');
        const y = yAxis ? yAxis.getBoundingClientRect() : null;
        setPlot((prev) => {
            const next = {
                left: a.left - b.left,
                width: a.width,
                top: y ? y.top - b.top : 0,
                height: y ? y.height : 0,
            };
            const settled = prev
                && Math.abs(prev.left - next.left) < 0.5
                && Math.abs(prev.width - next.width) < 0.5
                && Math.abs(prev.top - next.top) < 0.5
                && Math.abs(prev.height - next.height) < 0.5;
            return settled ? prev : next;
        });
    }, []);

    useLayoutEffect(() => { measurePlot(); });

    useEffect(() => {
        const body = bodyRef.current;
        if (!body || typeof ResizeObserver === 'undefined') return undefined;
        const observer = new ResizeObserver(measurePlot);
        observer.observe(body);
        return () => observer.disconnect();
    }, [measurePlot]);

    const visibleMarkers = useMemo(() => {
        if (!domain) return [];
        // Bucket timestamps are right edges, so the first bar covers the step
        // *before* `domain.min`. Rollout events cluster at the start of a
        // window, so excluding that step would hide exactly the markers worth
        // seeing; they are admitted and pinned to the leading edge instead.
        const lowerBound = domain.min - stepSeconds * 1000;
        return markers.filter((m) => m.ts >= lowerBound && m.ts <= domain.max);
    }, [markers, domain, stepSeconds]);
    const rangeMs = (rangeEndMs || 0) - (rangeStartMs || 0);

    // Render bars for every advertised level, even when a bucket has 0 for
    // it — keeps the stack order stable across refreshes and lets the legend
    // line up. Levels are passed pre-sorted by severity.
    const stackLevels = levels && levels.length > 0
        ? levels
        : ['info', 'warn', 'error'];

    const statusMessage = () => {
        if (!status) return 'No log volume found for the selected range.';
        if (status.reason === 'retention_expired_possible') {
            return status.retention_hint
                ? `No log volume found. Runtime logs are retained for ${status.retention_hint}, so this range may no longer be available.`
                : 'No log volume found. It may have expired based on the log backend retention policy.';
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
        // Loki's count_over_time(...[Xs]) at step T counts the preceding
        // X-second window (T - step, T], so the bucket at ts=T covers that
        // range, not [T, T + step).
        const end = entry.ts;
        const start = entry.ts - stepSeconds * 1000;
        // Toggle: re-clicking the same bar clears the selection.
        if (selectedBucketTs === end) {
            onSelectBucket(null);
        } else {
            onSelectBucket({ startMs: start, endMs: end });
        }
    };

    const chartAriaLabel = `Log volume chart: ${totalSum.toLocaleString()} log lines across ${data.length} buckets, stacked by level`;

    return (
        <div className="rounded border border-[var(--border)] bg-[var(--panel)] px-2 py-1">
            {loading ? (
                <div className="py-6 text-center text-xs text-[var(--text-soft)]">Loading chart…</div>
            ) : error ? (
                <div className="py-6 text-center text-xs text-[var(--err)]">{error}</div>
            ) : (!data.length || totalSum === 0) && visibleMarkers.length === 0 ? (
                <div className="py-6 text-center text-xs text-[var(--text-soft)]">{statusMessage()}</div>
            ) : (!data.length || totalSum === 0) ? (
                // Markers without volume: render the lane against a bare axis
                // rather than hiding the deployment's timeline behind a
                // capability it does not depend on.
                <div className="r-logc-chart-body" ref={bodyRef}>
                    <MarkerOnlyAxis domain={domain} rangeMs={rangeMs} markers={visibleMarkers} />
                </div>
            ) : (
                <div className="r-logc-chart-body" ref={bodyRef}>
                <div role="img" aria-label={chartAriaLabel}>
                <ResponsiveContainer width="100%" height={height}>
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
                            content={<LogChartTooltip stepSeconds={stepSeconds} levels={stackLevels} />}
                            cursor={{ fill: 'var(--r-logs-chart-cursor)' }}
                        />
                        {visibleMarkers.map((marker, i) => (
                            <ReferenceLine
                                key={`marker-${marker.ts}-${i}`}
                                x={marker.ts}
                                stroke={markerColor(marker.kind)}
                                strokeDasharray="3 2"
                                strokeOpacity={0.85}
                                ifOverflow="hidden"
                            />
                        ))}
                        {stackLevels.map((level) => (
                            <Bar
                                key={level}
                                dataKey={level}
                                stackId="a"
                                isAnimationActive={false}
                                onClick={handleBucketClick}
                                style={{ cursor: 'pointer' }}
                            >
                                {data.map((d, i) => (
                                    // Bucket index is the stable key — guards
                                    // against duplicate `ts` values from edge
                                    // cases (e.g. step transitions).
                                    <Cell
                                        key={`${level}-${i}`}
                                        fill={levelColor(level)}
                                        fillOpacity={selectedBucketTs == null || selectedBucketTs === d.ts ? 1 : 0.3}
                                    />
                                ))}
                            </Bar>
                        ))}
                    </BarChart>
                </ResponsiveContainer>
                </div>
                {timelineCursor && domain && plot && plot.height > 0 && (
                    <ChartTimelineCursor store={timelineCursor} domain={domain} plot={plot} />
                )}
                {visibleMarkers.length > 0 && domain && plot && (
                    /* Its own lane under the chart rather than an overlay: the
                       reference lines already carry the eye down through the
                       bars, and a lane cannot collide with the axis labels. */
                    <div
                        className="r-logc-markers"
                        style={{ left: plot.left, width: plot.width }}
                    >
                        {clusterMarkers(visibleMarkers, domain, plot.width).map((cluster, i) => {
                            const marker = cluster.lead;
                            const pct = cluster.pct;
                            return (
                                /* The slot carries the position so the tooltip's
                                   own wrapper hugs the dot; anchoring a tooltip
                                   to an absolutely positioned child leaves it
                                   pointing at the lane's origin instead. */
                                <span
                                    key={`lane-${marker.ts}-${i}`}
                                    className="r-logc-marker-slot"
                                    style={{ left: `${pct}%` }}
                                >
                                    <Tooltip
                                        content={cluster.markers.map((m) => (
                                            <div key={`${m.ts}-${m.label}`}>
                                                {m.label} · {formatMarkerTime(m.ts)}
                                            </div>
                                        ))}
                                    >
                                        <span
                                            className={`r-logc-marker is-${marker.kind}${cluster.markers.length > 1 ? ' is-cluster' : ''}`}
                                            style={{ background: markerColor(marker.kind) }}
                                            aria-label={cluster.markers
                                                .map((m) => `${m.label} at ${formatMarkerTime(m.ts)}`)
                                                .join('; ')}
                                        />
                                    </Tooltip>
                                </span>
                            );
                        })}
                    </div>
                )}
            </div>
            )}
        </div>
    );
}

/**
 * Marks where the reader currently is: a faint band over everything loaded, a
 * stronger one over the rows on screen inside it, and a hairline at the row
 * under the pointer.
 *
 * The two bands answer different questions. At a wide range the loaded buffer
 * is often a thin slice of the window — the faint band shows how much of the
 * chart scrolling alone can reach, which is what makes a narrow viewport band
 * at the right-hand edge legible instead of puzzling.
 *
 * Its own subscriber rather than a prop on the chart. The span changes on every
 * scroll frame, and re-rendering the Recharts tree that often to move two
 * absolutely positioned elements would cost far more than it draws.
 *
 * Positions map linearly onto the x-axis domain, exactly as the marker lane
 * does, so a timestamp lands where the axis says that moment is. Bars are
 * centred on their bucket's right edge, so a line inside the newest bucket sits
 * up to half a bar left of it — the same offset the axis tick labels carry.
 */
function ChartTimelineCursor({ store, domain, plot }: {
    store: TimelineCursorStore;
    domain: { min: number; max: number };
    plot: { left: number; width: number; top: number; height: number };
}) {
    const cursor = useSyncExternalStore<TimelineCursor | null>(
        store.subscribe,
        store.get,
        store.get,
    );
    if (!cursor) return null;

    const span = Math.max(1, domain.max - domain.min);
    const pct = (ms: number) => ((ms - domain.min) / span) * 100;
    /**
     * Clamp a span to the plot, or drop it when it falls wholly outside. Lines
     * paged in from before the charted window have nothing to point at, and a
     * band pinned to the edge would claim the reader is somewhere they are not.
     */
    const band = (startMs: number, endMs: number) => {
        const from = pct(startMs);
        const to = pct(endMs);
        if (to < 0 || from > 100) return null;
        const left = Math.max(0, Math.min(100, from));
        const right = Math.max(0, Math.min(100, to));
        return { left: `${left}%`, width: `${Math.max(0, right - left)}%` };
    };
    const buffer = band(cursor.bufferStartMs, cursor.bufferEndMs);
    const view = band(cursor.viewStartMs, cursor.viewEndMs);
    const hover = cursor.hoverMs === null ? null : pct(cursor.hoverMs);
    const hoverVisible = hover !== null && hover >= 0 && hover <= 100;

    return (
        <div
            className="r-logc-chart-cursor"
            style={{ left: plot.left, top: plot.top, width: plot.width, height: plot.height }}
            aria-hidden="true"
        >
            {buffer && <span className="r-logc-chart-cursor-buffer" style={buffer} />}
            {view && <span className="r-logc-chart-cursor-span" style={view} />}
            {hoverVisible && (
                <span className="r-logc-chart-cursor-line" style={{ left: `${hover}%` }} />
            )}
        </div>
    );
}

const MARKER_KIND_PRIORITY = ['failed', 'restart', 'up', 'rollout', 'done'];

/**
 * Merge markers that would land on the same few pixels.
 *
 * Deployment events happen seconds apart while a window spans hours, so at most
 * ranges several markers collapse onto one spot. Stacked dots hide each other
 * and only the topmost can be hovered, so a crowd becomes one dot whose tooltip
 * lists everything in it. The dot takes the most severe kind present, so an
 * OOMKill is never masked by a routine start alongside it.
 */
function clusterMarkers(markers, domain, laneWidth) {
    const span = Math.max(1, domain.max - domain.min);
    const threshold = 10;
    const clusters = [];

    for (const marker of markers) {
        const pct = Math.min(100, Math.max(0, ((marker.ts - domain.min) / span) * 100));
        const x = (pct / 100) * laneWidth;
        const open = clusters[clusters.length - 1];
        if (open && Math.abs(x - open.x) <= threshold) {
            open.markers.push(marker);
        } else {
            clusters.push({ x, pct, markers: [marker] });
        }
    }

    return clusters.map((cluster) => ({
        ...cluster,
        lead: [...cluster.markers].sort(
            (a, b) => MARKER_KIND_PRIORITY.indexOf(a.kind) - MARKER_KIND_PRIORITY.indexOf(b.kind),
        )[0],
    }));
}

/**
 * The timeline when the log backend reports no volume.
 *
 * Recharts is not involved, so there is no plot area to measure — the lane is
 * the full width minus a small inset, and markers position by percentage
 * against the same domain the chart would have used.
 */
function MarkerOnlyAxis({ domain, rangeMs, markers }) {
    if (!domain) return null;
    const span = Math.max(1, domain.max - domain.min);
    const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => domain.min + span * f);

    return (
        <div className="r-logc-bare-axis">
            <div className="r-logc-bare-line" />
            <div className="r-logc-bare-ticks">
                {ticks.map((ts, i) => (
                    <span key={i} className="r-logc-bare-tick">
                        {formatTickLabel(ts, rangeMs)}
                    </span>
                ))}
            </div>
            <div className="r-logc-markers" style={{ left: 0, right: 0 }}>
                {clusterMarkers(markers, domain, 600).map((cluster, i) => (
                    <span
                        key={`bare-${cluster.pct}-${i}`}
                        className="r-logc-marker-slot"
                        style={{ left: `${cluster.pct}%` }}
                    >
                        <Tooltip
                            content={cluster.markers.map((m) => (
                                <div key={`${m.ts}-${m.label}`}>
                                    {m.label} · {formatMarkerTime(m.ts)}
                                </div>
                            ))}
                        >
                            <span
                                className={`r-logc-marker is-${cluster.lead.kind}${cluster.markers.length > 1 ? ' is-cluster' : ''}`}
                                style={{ background: markerColor(cluster.lead.kind) }}
                                aria-label={cluster.markers
                                    .map((m) => `${m.label} at ${formatMarkerTime(m.ts)}`)
                                    .join('; ')}
                            />
                        </Tooltip>
                    </span>
                ))}
            </div>
        </div>
    );
}

function formatMarkerTime(ms) {
    const d = new Date(ms);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}
