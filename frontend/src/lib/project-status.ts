// A project's status as the dashboard shows it. The backend's status says
// what the production group serves; a newer production deploy that failed
// while the previous one kept serving is surfaced on top of it.

export interface ProjectStatusFields {
    status?: string;
    active_deployment_id?: string;
    latest_deployment_id?: string;
    latest_deployment_status?: string;
}

const FAILED = new Set(['Failed', 'Unhealthy']);

/** The newest production deploy failed and is not what serves. */
export function hasFailedDeploy(p: ProjectStatusFields): boolean {
    return !!p.latest_deployment_id
        && p.latest_deployment_id !== p.active_deployment_id
        && FAILED.has(p.latest_deployment_status || '');
}

/** The status used for colour, filters and counts. */
export function effectiveStatus(p: ProjectStatusFields): string {
    const status = p.status || 'Unknown';
    if (status === 'Deploying' || status === 'Deleting' || status === 'Terminated') return status;
    return hasFailedDeploy(p) ? 'Failed' : status;
}

/** The words shown on the badge: "Deploy failed" while the previous deployment still serves. */
export function statusLabel(p: ProjectStatusFields): string {
    const status = effectiveStatus(p);
    if (status === 'Failed' && p.status === 'Running') return 'Deploy failed';
    return status;
}
