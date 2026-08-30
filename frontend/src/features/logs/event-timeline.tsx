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
}: {
    event: DeploymentEvent;
    previous: DeploymentEvent | null;
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
                <EventFacts event={event} />
                <EventImages event={event} />
            </span>
            <span className="r-evt-delta mono">{sincePrevious(event, previous)}</span>
        </div>
    );
}

/**
 * Attributes worth surfacing beside the transition, in a fixed order so the
 * eye can find the same fact in the same place on every row. Anything not
 * listed stays in the payload rather than being rendered blindly — an event
 * carries whatever its emitter thought useful, which is not the same as
 * everything being worth a reader's attention.
 */
const FACTS: { key: string; label: string; format?: (v: unknown) => string }[] = [
    { key: 'created_by', label: 'by' },
    { key: 'containers', label: 'containers', format: (v) => (Array.isArray(v) ? v.join(', ') : String(v)) },
    { key: 'replicas', label: 'replicas' },
    { key: 'image', label: 'image' },
    { key: 'group', label: 'group' },
    { key: 'build_method', label: 'build' },
    { key: 'registry', label: 'registry' },
    { key: 'image_size_bytes', label: 'size', format: (v) => formatBytes(Number(v)) },
];

function EventFacts({ event }: { event: DeploymentEvent }) {
    const shown = FACTS
        .map(({ key, label, format }) => {
            const value = event.attributes?.[key];
            if (value === undefined || value === null || value === '') return null;
            return { label, text: format ? format(value) : String(value) };
        })
        .filter((f): f is { label: string; text: string } => f !== null);

    if (shown.length === 0) return null;
    return (
        <span className="r-evt-facts">
            {shown.map((f) => (
                <span key={f.label} className="r-evt-fact">
                    <span className="r-evt-fact-k">{f.label}</span>
                    <span className="r-evt-fact-v">{f.text}</span>
                </span>
            ))}
        </span>
    );
}

/** One image the reporter built or pushed during this transition. */
interface ReportedImage {
    container?: string;
    image?: string;
    build_method?: string;
    build_ms?: number;
    push_ms?: number;
    size_bytes?: number;
}

/**
 * Per-image detail, when the reporter supplied any.
 *
 * A multi-container deployment builds every image inside one `Building` state,
 * so the gap between transitions only gives the total. This is the breakdown
 * that says *which* image took it.
 */
function EventImages({ event }: { event: DeploymentEvent }) {
    const images = event.attributes?.images;
    if (!Array.isArray(images) || images.length === 0) return null;

    return (
        <span className="r-evt-images">
            {(images as ReportedImage[]).map((image, i) => (
                <span key={image.container ?? i} className="r-evt-image">
                    <span className="r-evt-image-name">{image.container ?? image.image ?? '?'}</span>
                    {image.build_method && (
                        <span className="r-evt-image-meta">{image.build_method}</span>
                    )}
                    {typeof image.build_ms === 'number' && (
                        <span className="r-evt-image-meta">build {formatMs(image.build_ms)}</span>
                    )}
                    {typeof image.push_ms === 'number' && (
                        <span className="r-evt-image-meta">push {formatMs(image.push_ms)}</span>
                    )}
                    {typeof image.size_bytes === 'number' && (
                        <span className="r-evt-image-meta">{formatBytes(image.size_bytes)}</span>
                    )}
                </span>
            ))}
        </span>
    );
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
