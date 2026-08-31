import { useCallback, useEffect, useState } from 'react';
import { Empty, Panel, PanelBody, PanelHead, Status } from '../../components/r-ui';
import { fetchDeploymentContainers, type ContainerStatus } from './api';

/**
 * What each replica of a deployment is doing right now.
 *
 * The snapshot half of deployment observability: the Timeline says how the
 * deployment got here, this says where it is. They share a vocabulary — a row's
 * subject is the same string its events carry — so a replica can be followed
 * from one view to the other.
 */
export function ContainerStatusPanel({
    projectName,
    deploymentId,
    /** Refetched when this changes, which is when the picture may have moved. */
    deploymentStatus,
}: {
    projectName: string;
    deploymentId: string;
    deploymentStatus: string;
}) {
    const [containers, setContainers] = useState<ContainerStatus[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    const load = useCallback(async (signal: AbortSignal) => {
        try {
            const page = await fetchDeploymentContainers({ projectName, deploymentId, signal });
            setContainers(page.containers);
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

    const ready = containers?.filter((c) => c.state === 'running').length ?? 0;

    return (
        <Panel>
            <PanelHead
                title="Containers"
                sub={containers ? `${ready}/${containers.length} running` : undefined}
            />
            <PanelBody>
                {error ? (
                    <Empty title="Could not load containers">{error}</Empty>
                ) : !containers ? (
                    <Empty>Loading…</Empty>
                ) : containers.length === 0 ? (
                    <Empty title="No containers observed">
                        The deployment's backend has not reported any replicas. A deployment that
                        has finished has none, and one that predates container observation reports
                        none until its next reconcile.
                    </Empty>
                ) : (
                    <div className="r-ctr">
                        {containers.map((c) => (
                            <ContainerRow key={c.subject} container={c} />
                        ))}
                    </div>
                )}
            </PanelBody>
        </Panel>
    );
}

function ContainerRow({ container }: { container: ContainerStatus }) {
    return (
        <div className="r-ctr-row">
            <span className="r-ctr-subject mono">{container.subject}</span>
            <Status status={statusLabel(container)} />
            <span className="r-ctr-facts">
                {container.restart_count !== undefined && container.restart_count > 0 && (
                    <Fact k="restarts" v={String(container.restart_count)} />
                )}
                {container.exit_code !== undefined && (
                    <Fact k="exit" v={String(container.exit_code)} />
                )}
                {container.reason && <Fact k="reason" v={container.reason} />}
                {container.started_at && container.state === 'running' && (
                    <Fact k="up" v={since(container.started_at)} />
                )}
                {container.image && <Fact k="image" v={container.image} />}
            </span>
        </div>
    );
}

function Fact({ k, v }: { k: string; v: string }) {
    return (
        <span className="r-ctr-fact">
            <span className="r-ctr-fact-k">{k}</span>
            <span className="r-ctr-fact-v mono">{v}</span>
        </span>
    );
}

/**
 * Map the backend-agnostic state onto the lifecycle vocabulary `Status`
 * already renders, so a container reads the same way as a deployment.
 */
function statusLabel(container: ContainerStatus): string {
    switch (container.state) {
        case 'running':
            // Running is not the same as healthy: a container can be up and
            // failing its probe, and saying "Running" would hide that.
            return container.health === 'not-ready' ? 'Unhealthy' : 'Running';
        case 'pending':
            return 'Deploying';
        case 'exited':
            return container.exit_code && container.exit_code !== 0 ? 'Failed' : 'Stopped';
        default:
            return 'Unknown';
    }
}

/** How long it has been up, in the units a person would say it in. */
function since(startedAt: string): string {
    const ms = Date.now() - Date.parse(startedAt);
    if (!Number.isFinite(ms) || ms < 0) return '';
    const seconds = Math.floor(ms / 1000);
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ${minutes % 60}m`;
    return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}
