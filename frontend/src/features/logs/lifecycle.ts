/**
 * Deployment lifecycle markers, drawn on the log-volume chart's time axis.
 *
 * Log volume alone says *when* a burst happened. Overlaying the deployment's
 * own events on the same axis says *what else was happening then* — that the
 * errors start four seconds after a replica came up, or right at an OOMKill.
 * That reading is only possible because these logs belong to a deployment.
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

const KIND_PRIORITY: Record<LifecycleMarkerKind, number> = {
    failed: 0,
    restart: 1,
    up: 2,
    rollout: 3,
    done: 4,
};

interface ContainerState {
    state_type?: string;
    reason?: string;
    exit_code?: number;
    started_at?: string;
    finished_at?: string;
}

interface ContainerStatus {
    name?: string;
    state?: ContainerState;
    last_state?: ContainerState;
}

interface Pod {
    name?: string;
    containers?: ContainerStatus[];
}

/** Shape of the fields this reads off a deployment; everything else is ignored. */
export interface LifecycleSource {
    status?: string;
    created?: string | null;
    completed_at?: string | null;
    controller_metadata?: {
        pod_status?: { pods?: Pod[] } | null;
    } | null;
}

function toMs(value: string | null | undefined): number | null {
    if (!value) return null;
    const ms = new Date(value).getTime();
    return Number.isNaN(ms) ? null : ms;
}

/** Why a previous run ended, in the words the pod status gives us. */
function terminationReason(state: ContainerState): string {
    if (state.reason) return state.reason;
    if (state.exit_code !== undefined && state.exit_code !== 0) return `exit ${state.exit_code}`;
    return 'restarted';
}

/**
 * Collect the markers worth drawing. Coincident markers with the same label are
 * collapsed, since several deployment fields share a single timestamp and three
 * rules stacked on one pixel read as one thicker rule, not as more information.
 *
 * What is available differs by backend, because this reads a *snapshot* of
 * observed state rather than a history. Rollout and completion come from the
 * deployment row, so every backend has them; container starts come from the
 * pod-status block every backend builds. Restarts need `last_state`, which only
 * the Kubernetes path emits, and it holds one prior termination — so a
 * crash-looping container contributes a single marker regardless of how many
 * times it has restarted.
 */
export function buildLifecycleMarkers(
    deployment: LifecycleSource | null | undefined,
    /**
     * Names the deployment declares. Runtime container names can be long and
     * backend-shaped — Docker's carry the project, group and deployment id —
     * so a declared name is preferred whenever one identifies the same
     * container.
     */
    declaredContainers: string[] = [],
): LifecycleMarker[] {
    if (!deployment) return [];
    const markers: LifecycleMarker[] = [];
    // Longest first, so `api-worker` wins over `api` for `..._api-worker_r0`.
    const declared = [...declaredContainers].filter(Boolean).sort((a, b) => b.length - a.length);
    const displayName = (runtimeName: string) =>
        declared.find((name) => runtimeName.includes(name)) ?? runtimeName;

    const created = toMs(deployment.created);
    if (created !== null) {
        markers.push({ ts: created, label: 'Rollout started', kind: 'rollout' });
    }

    const completed = toMs(deployment.completed_at);
    if (completed !== null) {
        const failed = deployment.status === 'Failed';
        markers.push({
            ts: completed,
            label: failed ? 'Deployment failed' : 'Deployment completed',
            kind: failed ? 'failed' : 'done',
        });
    }

    for (const pod of deployment.controller_metadata?.pod_status?.pods ?? []) {
        for (const container of pod.containers ?? []) {
            const name = displayName(container.name || pod.name || 'container');

            const startedAt = toMs(container.state?.started_at);
            if (startedAt !== null && container.state?.state_type === 'running') {
                markers.push({ ts: startedAt, label: `${name} started`, kind: 'up' });
            }

            const last = container.last_state;
            const finishedAt = toMs(last?.finished_at);
            if (last && finishedAt !== null && last.state_type === 'terminated') {
                markers.push({
                    ts: finishedAt,
                    label: `${name} ${terminationReason(last)}`,
                    kind: 'restart',
                });
            }
        }
    }

    const deduped = new Map<string, LifecycleMarker>();
    for (const marker of markers) {
        const key = `${marker.ts}:${marker.label}`;
        const existing = deduped.get(key);
        if (!existing || KIND_PRIORITY[marker.kind] < KIND_PRIORITY[existing.kind]) {
            deduped.set(key, marker);
        }
    }

    return [...deduped.values()].sort((a, b) => a.ts - b.ts);
}

/**
 * Turn logged events into rail markers.
 *
 * The event log is a history, so it carries what a snapshot cannot: a
 * deployment that went healthy, unhealthy and healthy again contributes three
 * markers here, where `buildLifecycleMarkers` can only ever show the state it
 * last observed.
 *
 * Only `status_changed` is mapped today, because that is all the backend emits.
 * Replica-level events join when the reconcilers derive them, at which point
 * the snapshot-derived markers retire.
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
