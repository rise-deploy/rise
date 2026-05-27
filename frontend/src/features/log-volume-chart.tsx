// @ts-nocheck
import { useMemo } from 'react';
import { Bar, BarChart, CartesianGrid, Cell, ResponsiveContainer, Tooltip as RechartsTooltip, XAxis, YAxis } from 'recharts';
import { formatDate } from '../lib/utils';

const LOG_CHART_COLOR_INFO = 'var(--r-log-chart-info)';
const LOG_CHART_COLOR_WARN = 'var(--r-log-chart-warn)';
const LOG_CHART_COLOR_ERROR = 'var(--r-log-chart-error)';

function formatTickLabel(ms, rangeMs) {
    const d = new Date(ms);
    const pad = (n) => String(n).padStart(2, '0');
    if (rangeMs <= 24 * 3600 * 1000) {
        return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    }
    return `${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function LogChartTooltip({ active, payload, stepSeconds }: { active?: boolean; payload?: any[]; stepSeconds: number }) {
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
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_INFO }} />
                <span className="lbl">Info</span>
                <span className="val">{b.info || 0}</span>
            </div>
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_WARN }} />
                <span className="lbl">Warn</span>
                <span className="val">{b.warn || 0}</span>
            </div>
            <div className="r-logs-chart-tip-row">
                <span className="dot" style={{ background: LOG_CHART_COLOR_ERROR }} />
                <span className="lbl">Error</span>
                <span className="val">{b.error || 0}</span>
            </div>
            <div className="r-logs-chart-tip-total">
                <span className="lbl">Total</span>
                <span className="val">{b.total || 0}</span>
            </div>
            <div className="r-logs-chart-tip-hint">Click bar to filter logs</div>
        </div>
    );
}

export default function LogVolumeChart({ counts, loading, error, status, rangeStartMs, rangeEndMs, stepSeconds, onSelectBucket, selectedBucketTs }) {
    const data = useMemo(
        () =>
            counts.map((b) => ({
                ...b,
                ts: new Date(b.timestamp).getTime(),
            })),
        [counts],
    );
    const totalSum = useMemo(() => data.reduce((sum, b) => sum + (b.total || 0), 0), [data]);
    const rangeMs = (rangeEndMs || 0) - (rangeStartMs || 0);

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

    return (
        <div className="rounded border border-[var(--border)] bg-[var(--panel)] px-2 py-1">
            {loading ? (
                <div className="py-6 text-center text-xs text-[var(--text-soft)]">Loading chart…</div>
            ) : error ? (
                <div className="py-6 text-center text-xs text-[var(--err)]">{error}</div>
            ) : !data.length || totalSum === 0 ? (
                <div className="py-6 text-center text-xs text-[var(--text-soft)]">{statusMessage()}</div>
            ) : (
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
                            content={<LogChartTooltip stepSeconds={stepSeconds} />}
                            cursor={{ fill: 'oklch(0.22 0.005 80 / 0.45)' }}
                        />
                        <Bar dataKey="info" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                            {data.map((d) => (
                                <Cell
                                    key={`info-${d.ts}`}
                                    fill={LOG_CHART_COLOR_INFO}
                                    fillOpacity={selectedBucketTs == null || selectedBucketTs === d.ts ? 1 : 0.3}
                                />
                            ))}
                        </Bar>
                        <Bar dataKey="warn" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                            {data.map((d) => (
                                <Cell
                                    key={`warn-${d.ts}`}
                                    fill={LOG_CHART_COLOR_WARN}
                                    fillOpacity={selectedBucketTs == null || selectedBucketTs === d.ts ? 1 : 0.3}
                                />
                            ))}
                        </Bar>
                        <Bar dataKey="error" stackId="a" isAnimationActive={false} onClick={handleBucketClick} style={{ cursor: 'pointer' }}>
                            {data.map((d) => (
                                <Cell
                                    key={`error-${d.ts}`}
                                    fill={LOG_CHART_COLOR_ERROR}
                                    fillOpacity={selectedBucketTs == null || selectedBucketTs === d.ts ? 1 : 0.3}
                                />
                            ))}
                        </Bar>
                    </BarChart>
                </ResponsiveContainer>
            )}
        </div>
    );
}
