import { useCallback, useEffect, useState } from 'react';
import { Empty, Panel, PanelBody, PanelHead } from '../../components/r-ui';
import { fetchDeploymentEvents, type DeploymentEvent } from './api';

/**
 * The deployment's recorded history.
 *
 * Reads the same event log the log console's rail draws on, so the two views
 * cannot disagree about what happened. Nothing here is inferred from the
 * deployment row: every line is a transition the control plane actually wrote.
 */
export function EventTimeline({
    projectName,
    deploymentId,
    /** Refetched when this changes, which is when a new event exists to read. */
    deploymentStatus,
}: {
    projectName: string;
    deploymentId: string;
    deploymentStatus: string;
}) {
    const [events, setEvents] = useState<DeploymentEvent[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    const load = useCallback(async (signal: AbortSignal) => {
        try {
            const page = await fetchDeploymentEvents({
                projectName,
                deploymentId,
                limit: 200,
                signal,
            });
            // The API pages newest-first by write order; a timeline reads down.
            setEvents([...page.events].reverse());
            setError(null);
        } catch (err) {
            if (err instanceof Error && err.name === 'AbortError') return;
            setError(err instanceof Error ? err.message : String(err));
        }
    }, [projectName, deploymentId]);

    useEffect(() => {
        const controller = new AbortController();
        void load(controller.signal);
        return () => controller.abort();
    }, [load, deploymentStatus]);

    return (
        <Panel>
            <PanelHead
                title="Deployment timeline"
                sub={events ? `${events.length} recorded ${events.length === 1 ? 'event' : 'events'}` : undefined}
            />
            <PanelBody>
                {error ? (
                    <Empty title="Could not load the timeline">{error}</Empty>
                ) : !events ? (
                    <Empty>Loading…</Empty>
                ) : events.length === 0 ? (
                    <Empty title="No events recorded">
                        Events are written as the deployment progresses. A deployment that
                        predates the event log has none.
                    </Empty>
                ) : (
                    <div className="r-evt">
                        {events.map((event, i) => (
                            <EventRow
                                key={event.id}
                                event={event}
                                previous={i > 0 ? events[i - 1] : null}
                                projectName={projectName}
                            />
                        ))}
                    </div>
                )}
            </PanelBody>
        </Panel>
    );
}

function EventRow({
    event,
    previous,
    projectName,
}: {
    event: DeploymentEvent;
    previous: DeploymentEvent | null;
    projectName: string;
}) {
    const ts = new Date(event.occurred_at);
    const reason = typeof event.attributes?.reason === 'string' ? event.attributes.reason : null;

    return (
        <div className={`r-evt-row sev-${event.severity}`}>
            <span className="r-evt-dot" aria-hidden="true" />
            <span className="r-evt-time mono">{ts.toLocaleTimeString()}</span>
            <span className="r-evt-body">
                <span className="r-evt-label">{describe(event)}</span>
                {reason && <span className="r-evt-reason">{reason}</span>}
                <EventAttributes event={event} projectName={projectName} />
            </span>
            <span className="r-evt-delta mono">{sincePrevious(event, previous)}</span>
        </div>
    );
}

/**
 * Keys the row already renders structurally: `from`/`to` become the label,
 * `reason` its own line. Rendering them again as attributes would say
 * everything twice.
 */
const RESERVED_KEYS = new Set(['from', 'to', 'reason']);

/**
 * Presentation hints for keys we know about — a shorter label, and an order
 * that puts the most-asked-for facts first.
 *
 * Cosmetic only. This deliberately does **not** decide what is shown: an
 * attribute missing from this table still renders, under its own key. That is
 * the whole point — a writer can add an attribute and have it appear without a
 * frontend change, and the vocabulary in `rise-backend-core::events` stays the
 * single place that decides what an attribute *means*.
 */
const LABELS: Record<string, string> = {
    created_by: 'by',
    build_method: 'build',
    image_size_bytes: 'size',
    superseded_by: 'superseded by',
    rolled_back_from: 'rolled back from',
    job_url: 'job',
    pull_request_url: 'pull request',
};

/**
 * Reading order. Postgres `jsonb` does not preserve insertion order — it stores
 * keys sorted by length then bytes — so without this the fields of an event
 * would appear in an order nobody chose, and one that shifts when a key is
 * renamed.
 */
const ORDER = [
    // Who and what, before how much.
    'created_by', 'superseded_by', 'rolled_back_from', 'stopped_by',
    'container', 'containers', 'replicas', 'cpu', 'memory',
    // Then the build, in the order it happened.
    'build_method', 'build_ms', 'push_ms', 'registry',
    'image', 'image_digest', 'image_size_bytes', 'group',
];

/** Keys that name the subject of a breakdown row, so it can lead. */
const NAME_KEYS = new Set(['container', 'name']);

function orderOf(key: string): number {
    const i = ORDER.indexOf(key);
    return i === -1 ? ORDER.length : i;
}

/**
 * Every attribute the event carries, minus the ones the row already states.
 *
 * Total by construction: an unrecognised key renders under its own name rather
 * than being dropped. An allowlist here would silently discard whatever a
 * newer backend reports, and "recorded but invisible" is indistinguishable
 * from "never recorded" to the person reading the timeline.
 */
function EventAttributes({
    event,
    projectName,
}: {
    event: DeploymentEvent;
    projectName: string;
}) {
    const entries = Object.entries(event.attributes ?? {})
        .filter(([key, value]) => !RESERVED_KEYS.has(key) && value !== null && value !== '')
        .sort(([a], [b]) => orderOf(a) - orderOf(b) || a.localeCompare(b));

    if (entries.length === 0) return null;

    // A list of objects is a breakdown, not a value — per-image build timings,
    // and whatever else later reports in the same shape. It gets its own rows.
    const facts = entries.filter(([, v]) => !isObjectList(v));
    const breakdowns = entries.filter(([, v]) => isObjectList(v));

    return (
        <>
            {facts.length > 0 && (
                <span className="r-evt-facts">
                    {facts.map(([key, value]) => (
                        <span key={key} className="r-evt-fact">
                            <span className="r-evt-fact-k">{LABELS[key] ?? key.replace(/_/g, ' ')}</span>
                            <span className="r-evt-fact-v">
                                <AttributeValue name={key} value={value} projectName={projectName} />
                            </span>
                        </span>
                    ))}
                </span>
            )}
            {breakdowns.map(([key, rows]) => (
                <span key={key} className="r-evt-images">
                    {(rows as Record<string, unknown>[]).map((row, i) => (
                        <span key={i} className="r-evt-image">
                            {Object.entries(row)
                                .filter(([, v]) => v !== null && v !== '')
                                .sort(([a], [b]) => orderOf(a) - orderOf(b) || a.localeCompare(b))
                                .map(([k, v]) => (
                                    <span
                                        key={k}
                                        className={
                                            NAME_KEYS.has(k) ? 'r-evt-image-name' : 'r-evt-image-meta'
                                        }
                                    >
                                        {NAME_KEYS.has(k) ? String(v) : formatValue(k, v)}
                                    </span>
                                ))}
                        </span>
                    ))}
                </span>
            ))}
        </>
    );
}

function isObjectList(value: unknown): boolean {
    return (
        Array.isArray(value) &&
        value.length > 0 &&
        value.every((v) => typeof v === 'object' && v !== null && !Array.isArray(v))
    );
}

/**
 * A value, linked when it names something reachable.
 *
 * Only deployment references are linked: "superseded by X" is the one thing a
 * reader of a dead deployment always wants next, and following it should not
 * mean copying an id into the URL bar.
 */
function AttributeValue({
    name,
    value,
    projectName,
}: {
    name: string;
    value: unknown;
    projectName: string;
}) {
    const text = formatValue(name, value);

    if (name === 'superseded_by' || name === 'rolled_back_from') {
        return <a className="r-evt-link" href={`/deployment/${projectName}/${text}`}>{text}</a>;
    }
    if (name.endsWith('_url') && typeof value === 'string' && /^https?:\/\//.test(value)) {
        return <a className="r-evt-link" href={value} target="_blank" rel="noreferrer">{text}</a>;
    }
    return <>{text}</>;
}

/**
 * Format by what the key's suffix says the value *is*.
 *
 * Convention rather than enumeration, so a new `*_ms` or `*_bytes` attribute
 * formats correctly the day a backend starts reporting it, with nothing to add
 * here. The units live in the key name, which is also why writers should keep
 * naming them that way.
 */
function formatValue(name: string, value: unknown): string {
    if (Array.isArray(value)) return value.map((v) => formatValue(name, v)).join(', ');
    if (typeof value === 'object' && value !== null) return JSON.stringify(value);

    if (name.endsWith('_ms') && typeof value === 'number') return formatMs(value);
    if (name.endsWith('_bytes') && typeof value === 'number') return formatBytes(value);
    if (name.endsWith('_at') && typeof value === 'string') {
        const date = new Date(value);
        if (!Number.isNaN(date.getTime())) return date.toLocaleString();
    }
    return String(value);
}

function formatMs(ms: number): string {
    if (!Number.isFinite(ms)) return '';
    if (ms < 1000) return `${Math.round(ms)}ms`;
    const seconds = ms / 1000;
    if (seconds < 60) return `${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)}s`;
    const minutes = Math.floor(seconds / 60);
    return `${minutes}m ${Math.round(seconds % 60)}s`;
}

function formatBytes(bytes: number): string {
    if (!Number.isFinite(bytes)) return '';
    const units = ['B', 'KiB', 'MiB', 'GiB'];
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
        value /= 1024;
        unit += 1;
    }
    return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** A one-line reading of the event, from its own attributes. */
function describe(event: DeploymentEvent): string {
    if (event.kind === 'status_changed') {
        const from = typeof event.attributes?.from === 'string' ? event.attributes.from : null;
        const to = typeof event.attributes?.to === 'string' ? event.attributes.to : '?';
        return from ? `${from} → ${to}` : to;
    }
    return event.message || event.kind.replace(/_/g, ' ');
}

/**
 * Time since the previous event. This is the number that makes a timeline worth
 * reading — "the rollout sat in Pushed for four minutes" is the finding.
 */
function sincePrevious(event: DeploymentEvent, previous: DeploymentEvent | null): string {
    if (!previous) return '';
    const ms = new Date(event.occurred_at).getTime() - new Date(previous.occurred_at).getTime();
    if (!Number.isFinite(ms) || ms < 0) return '';
    if (ms < 1000) return `+${ms}ms`;
    const seconds = Math.floor(ms / 1000);
    if (seconds < 60) return `+${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `+${minutes}m ${seconds % 60}s`;
    return `+${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}
