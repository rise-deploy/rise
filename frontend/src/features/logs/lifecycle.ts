/**
 * Deployment lifecycle markers, drawn on the log-volume chart's time axis.
 *
 * Log volume alone says *when* a burst happened. Overlaying the deployment's
 * recorded events on the same axis says *what else was happening then* — that
 * the errors start four seconds after the rollout went healthy. That reading is
 * only possible because these logs belong to a deployment.
 */

/** Ordered loosely by how much a reader cares when markers collide. */
export type LifecycleMarkerKind =
    | 'failed'
    | 'restart'
    | 'up'
    | 'rollout'
    | 'done';

export interface LifecycleMarker {
    /** Epoch milliseconds. */
    ts: number;
    label: string;
    kind: LifecycleMarkerKind;
}

function toMs(value: string | null | undefined): number | null {
    if (!value) return null;
    const ms = new Date(value).getTime();
    return Number.isNaN(ms) ? null : ms;
}

/**
 * Turn logged events into rail markers.
 *
 * The event log is the only source: it is a history, so a deployment that went
 * healthy, unhealthy and healthy again contributes three markers rather than
 * only the state last observed.
 *
 * Only `status_changed` is mapped today, because that is all the backend emits.
 * Replica-level markers appear once the reconcilers emit replica events — until
 * then their absence here is the honest reading of what is recorded.
 */
export function markersFromEvents(events: LoggedEvent[]): LifecycleMarker[] {
    const markers: LifecycleMarker[] = [];

    for (const event of events) {
        const ts = toMs(event.occurred_at);
        if (ts === null) continue;

        if (event.kind === 'status_changed') {
            const to = typeof event.attributes?.to === 'string' ? event.attributes.to : null;
            if (!to) continue;
            const kind = statusMarkerKind(to);
            if (!kind) continue;
            const reason =
                typeof event.attributes?.reason === 'string' ? event.attributes.reason : null;
            const from = typeof event.attributes?.from === 'string' ? event.attributes.from : null;
            const label = from ? to : `${to} (created)`;
            markers.push({
                ts,
                label: reason ? `${label} — ${reason}` : label,
                kind,
            });
        }
    }

    return markers.sort((a, b) => a.ts - b.ts);
}

/** Shape this reads off an event row; everything else is ignored. */
export interface LoggedEvent {
    occurred_at: string;
    kind: string;
    attributes?: Record<string, unknown> | null;
}

/**
 * Which statuses earn a marker. Intermediate build phases do not: on a healthy
 * rollout they all land within a second or two of each other and would render
 * as one indistinguishable cluster, which is noise rather than history.
 */
function statusMarkerKind(to: string): LifecycleMarkerKind | null {
    switch (to) {
        // Acceptance and rollout are different moments — a slow build separates
        // them by minutes, and seeing that gap is the point. They cluster into
        // one marker when they are genuinely close.
        case 'Pending':
        case 'Deploying':
            return 'rollout';
        case 'Healthy':
            return 'up';
        case 'Failed':
            return 'failed';
        case 'Unhealthy':
            return 'restart';
        case 'Stopped':
        case 'Cancelled':
        case 'Superseded':
        case 'Expired':
            return 'done';
        default:
            return null;
    }
}
