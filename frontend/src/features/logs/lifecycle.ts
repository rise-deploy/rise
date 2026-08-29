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
    | 'warning'
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
    warning: 2,
    up: 3,
    rollout: 4,
    done: 5,
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

interface PodEvent {
    type?: string;
    reason?: string;
    last_timestamp?: string;
}

interface Pod {
    name?: string;
    containers?: ContainerStatus[];
    events?: PodEvent[];
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

        for (const event of pod.events ?? []) {
            if (event.type !== 'Warning') continue;
            const ts = toMs(event.last_timestamp);
            if (ts === null) continue;
            markers.push({ ts, label: event.reason || 'Warning', kind: 'warning' });
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
