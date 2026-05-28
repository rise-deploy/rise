// @ts-nocheck
import { useMemo } from 'react';
import { Bar, BarChart, CartesianGrid, Cell, ResponsiveContainer, Tooltip as RechartsTooltip, XAxis, YAxis } from 'recharts';
import { formatDate } from '../lib/utils';

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

export default function LogVolumeChart({ counts, levels, loading, error, status, rangeStartMs, rangeEndMs, stepSeconds, onSelectBucket, selectedBucketTs }) {
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
            ) : !data.length || totalSum === 0 ? (
                <div className="py-6 text-center text-xs text-[var(--text-soft)]">{statusMessage()}</div>
            ) : (
                <div role="img" aria-label={chartAriaLabel}>
                <ResponsiveContainer width="100%" height={96}>
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
            )}
        </div>
    );
}
