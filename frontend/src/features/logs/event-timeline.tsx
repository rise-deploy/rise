import { useCallback, useEffect, useState } from 'react';
import { navigate } from '../../lib/navigation';
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
    // The API pages; this view reads one page, so say so rather than implying
    // the count is the deployment's whole history.
    const [truncated, setTruncated] = useState(false);

    const load = useCallback(async (signal: AbortSignal) => {
        try {
            const page = await fetchDeploymentEvents({
                projectName,
                deploymentId,
                limit: 200,
                signal,
            });
            // The API pages newest-first by WRITE order, which is not the order
            // things happened: an event derived from a late observation carries
            // an older `occurred_at` than one already returned. A timeline reads
            // by when it happened, so sort rather than merely reversing.
            setEvents(
                [...page.events].sort(
                    (a, b) => Date.parse(a.occurred_at) - Date.parse(b.occurred_at),
                ),
            );
            setTruncated(page.next_cursor !== null && page.next_cursor !== undefined);
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
                sub={events
                    ? `${truncated ? 'latest ' : ''}${events.length} recorded ${events.length === 1 ? 'event' : 'events'}`
                    : undefined}
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
                <span className="r-evt-label">
                    {event.subject && <span className="r-evt-subject">{event.subject}</span>}
                    {describe(event)}
                </span>
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
    git_revision: 'revision',
    git_branch: 'branch',
    git_dirty: 'uncommitted changes',
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
    // Then what was built, and from what.
    'git_branch', 'git_revision', 'git_dirty',
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
        .filter(([key, value]) => !RESERVED_KEYS.has(key) && !isEmptyValue(value))
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

/** `{}` and `[]` carry nothing; rendering them shows a label with no value. */
function isEmptyValue(value: unknown): boolean {
    if (value === null || value === '') return true;
    if (Array.isArray(value)) return value.length === 0;
    if (typeof value === 'object') return Object.keys(value as object).length === 0;
    return false;
}

/**
 * A value that points at something else in Rise.
 *
 * Emitters describe their own references — `{kind, name}` — rather than the
 * reader recognising particular key names. A backend can link to something this
 * build has never heard of and it still renders; only the href needs a `kind`
 * it knows, and an unknown one degrades to plain text rather than a dead link.
 */
interface AttributeRef {
    kind: string;
    name: string;
}

function asRef(value: unknown): AttributeRef | null {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
    const { kind, name } = value as Record<string, unknown>;
    return typeof kind === 'string' && typeof name === 'string' ? { kind, name } : null;
}

/** Where a reference of each kind lives. Unknown kinds render unlinked. */
function hrefForRef(ref: AttributeRef, projectName: string): string | null {
    switch (ref.kind) {
        case 'deployment':
            return `/deployment/${encodeURIComponent(projectName)}/${encodeURIComponent(ref.name)}`;
        case 'project':
            return `/project/${encodeURIComponent(ref.name)}`;
        default:
            return null;
    }
}

/** A value, linked when it names something reachable. */
function AttributeValue({
    name,
    value,
    projectName,
}: {
    name: string;
    value: unknown;
    projectName: string;
}) {
    const ref = asRef(value);
    if (ref) {
        const href = hrefForRef(ref, projectName);
        return href ? <InternalLink href={href}>{ref.name}</InternalLink> : <>{ref.name}</>;
    }

    const text = formatValue(name, value);
    if (name.endsWith('_url') && typeof value === 'string' && /^https?:\/\//.test(value)) {
        return <a className="r-evt-link" href={value} target="_blank" rel="noreferrer">{text}</a>;
    }
    return <>{text}</>;
}

/** An in-app link: a real href for middle-click and copy, routed on plain click. */
function InternalLink({ href, children }: { href: string; children: React.ReactNode }) {
    return (
        <a
            className="r-evt-link"
            href={href}
            onClick={(e) => {
                if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
                e.preventDefault();
                navigate(href);
            }}
        >
            {children}
        </a>
    );
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

    // "yes"/"no" rather than "true"/"false": these read as answers to the
    // label beside them, and `false` is worth showing — "we looked, and the
    // tree was clean" is not the same as saying nothing.
    if (typeof value === 'boolean') return value ? 'yes' : 'no';

    if (name.endsWith('_ms') && typeof value === 'number') return formatMs(value);
    if (name.endsWith('_bytes') && typeof value === 'number') return formatBytes(value);
    // A revision is identified by its first few characters everywhere else a
    // person reads one; the full value stays in the event for anyone matching
    // on it exactly.
    if (name.endsWith('_revision') && typeof value === 'string' && /^[0-9a-f]{40}$/.test(value)) {
        return value.slice(0, 12);
    }
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

/**
 * A one-line reading of the event, from its own attributes.
 *
 * `from`/`to` are read for any kind that carries them, not only status
 * changes: a restart count going `0 → 1` is the same shape of fact as a status
 * going `Healthy → Unhealthy`, and reserving the keys while interpreting them
 * for one kind would swallow the other's only content.
 */
function describe(event: DeploymentEvent): string {
    const from = scalar(event.attributes?.from);
    const to = scalar(event.attributes?.to);

    const transition = to !== null ? (from !== null ? `${from} → ${to}` : to) : null;
    if (event.kind === 'status_changed') {
        return transition ?? event.kind.replace(/_/g, ' ');
    }

    // Another kind names itself first — "replica restarted" — and then says
    // what moved, if anything did.
    const name = event.message || event.kind.replace(/_/g, ' ');
    return transition ? `${name} ${transition}` : name;
}

/** A `from`/`to` endpoint, which may be a status name or a count. */
function scalar(value: unknown): string | null {
    if (typeof value === 'string') return value;
    if (typeof value === 'number' || typeof value === 'boolean') return String(value);
    return null;
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
