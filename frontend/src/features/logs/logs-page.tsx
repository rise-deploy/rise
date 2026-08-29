import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../lib/api';
import { navigate } from '../../lib/navigation';
import { usePolling } from '../../lib/polling';
import { ErrorState, LoadingState } from '../../components/states';
import { Icon } from '../../components/icon';
import { LogConsole } from './log-console';
import { buildLifecycleMarkers } from './lifecycle';

/** Statuses that will not change again, so polling can stop. */
const TERMINAL_STATUSES = new Set([
    'Cancelled', 'Stopped', 'Failed', 'Superseded', 'Expired',
]);

interface DeploymentSummary {
    status: string;
    completed_at?: string | null;
    created?: string | null;
    containers?: { name?: string }[] | null;
    controller_metadata?: { pod_status?: unknown } | null;
}

/**
 * The log console at full height. The deployment tab embeds the same component;
 * this route exists so the stream can have the whole viewport, and so a link to
 * a deployment's logs is shareable on its own.
 */
export function DeploymentLogsPage({
    projectName,
    deploymentId,
}: {
    projectName: string;
    deploymentId: string;
}) {
    const [deployment, setDeployment] = useState<DeploymentSummary | null>(null);
    const [error, setError] = useState<string | null>(null);

    const load = useCallback(async () => {
        try {
            const data = await api.getDeployment(projectName, deploymentId);
            setDeployment(data as DeploymentSummary);
            setError(null);
        } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
        }
    }, [projectName, deploymentId]);

    useEffect(() => { void load(); }, [load]);

    // Keep the status chip and live/pause affordance honest while a rollout is
    // still moving; stop once nothing more can change.
    const settled = deployment !== null && TERMINAL_STATUSES.has(deployment.status);
    usePolling(load, 5000, deployment !== null && !settled);

    const containers = useMemo(
        () => (deployment?.containers ?? [])
            .map((c) => c?.name)
            .filter((name): name is string => !!name),
        [deployment],
    );

    const markers = useMemo(
        () => buildLifecycleMarkers(deployment, containers),
        [deployment, containers],
    );

    if (error) return <ErrorState message={error} onRetry={load} />;
    if (!deployment) return <LoadingState label="Loading deployment…" />;

    const detailHref = `/deployment/${projectName}/${deploymentId}`;

    return (
        <LogConsole
            variant="page"
            projectName={projectName}
            deploymentId={deploymentId}
            deploymentStatus={deployment.status}
            deploymentCompletedAt={deployment.completed_at}
            deploymentCreated={deployment.created}
            containers={containers}
            markers={markers}
            lead={(
                <a
                    className="r-logc-back"
                    href={detailHref}
                    onClick={(e) => {
                        if (e.metaKey || e.ctrlKey || e.shiftKey) return;
                        e.preventDefault();
                        navigate(detailHref);
                    }}
                >
                    <Icon name="chevl" size={12} />
                    Deployment
                </a>
            )}
        />
    );
}

export default DeploymentLogsPage;
