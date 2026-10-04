import type { Deployment, Environment } from './project-context';

// What each environment is serving, derived from the project's recent
// deployments. An environment serves the active deployment of its primary
// deployment group; other groups (merge-request previews, branches) run beside
// it on their own URLs.

const IN_PROGRESS = new Set(['Pending', 'Building', 'Pushing', 'Pushed', 'Deploying']);
const TERMINAL = new Set(['Cancelled', 'Stopped', 'Superseded', 'Failed', 'Expired']);

export type EnvHealth = 'healthy' | 'deploying' | 'failed' | 'stopped';

export interface EnvState {
    env: Environment;
    /** The group whose active deployment serves the environment's URL. */
    group: string;
    /** Newest-first deployments of the primary group. */
    deployments: Deployment[];
    live: Deployment | null;
    latest: Deployment | null;
    /** The newest deployment that `live` replaced and can be rolled back to. */
    previous: Deployment | null;
    /** A deployment newer than `live` that failed; `live` kept serving. */
    failedAttempt: Deployment | null;
    health: EnvHealth;
    /** Active deployments of the environment's other groups. */
    otherActive: Deployment[];
}

export function isInProgress(status?: string): boolean {
    return !!status && IN_PROGRESS.has(status);
}

export function isTerminal(status?: string): boolean {
    return !!status && TERMINAL.has(status);
}

export function byNewest(a: Deployment, b: Deployment): number {
    return (b.created || '').localeCompare(a.created || '');
}

export function envState(env: Environment, deployments: Deployment[]): EnvState {
    const group = env.primary_deployment_group || 'default';
    const inEnv = deployments.filter(d => d.environment === env.name).sort(byNewest);
    const ds = inEnv.filter(d => (d.deployment_group || 'default') === group);
    const live = ds.find(d => d.is_active) ?? null;
    const latest = ds[0] ?? null;
    const liveIdx = live ? ds.indexOf(live) : -1;
    const previous = live
        ? ds.slice(liveIdx + 1).find(d => d.status === 'Superseded' && d.can_rollback) ?? null
        : null;
    const failedAttempt = latest && latest !== live && (latest.status === 'Failed' || latest.status === 'Unhealthy')
        && (!live || latest.created > live.created) ? latest : null;

    let health: EnvHealth;
    if (latest && isInProgress(latest.status)) health = 'deploying';
    else if (failedAttempt || live?.status === 'Unhealthy') health = 'failed';
    else if (live) health = 'healthy';
    else health = 'stopped';

    const otherActive = inEnv.filter(d => d.is_active && (d.deployment_group || 'default') !== group);
    return { env, group, deployments: ds, live, latest, previous, failedAttempt, health, otherActive };
}

/** Production first, then the rest by name. */
export function sortEnvironments(envs: Environment[]): Environment[] {
    return [...envs].sort((a, b) => Number(b.is_production) - Number(a.is_production) || a.name.localeCompare(b.name));
}

export function productionEnv(envs: Environment[]): Environment | null {
    return envs.find(e => e.is_production) ?? envs.find(e => e.name === 'production') ?? envs[0] ?? null;
}

/**
 * Environments a deployment in `from` can be promoted to: any other
 * environment with a primary group (a promote lands in that group). The
 * production environment is offered first, since that's the usual direction.
 */
export function promoteTargets(from: string, envs: Environment[]): Environment[] {
    return sortEnvironments(envs).filter(e => e.name !== from && !!e.primary_deployment_group);
}

export const HEALTH_LABEL: Record<EnvHealth, string> = {
    healthy: 'Healthy',
    deploying: 'Deploying',
    failed: 'Failed',
    stopped: 'Stopped',
};

/** The last segment of an image reference, e.g. `api:59a1ac`. */
export function shortImage(image?: string): string {
    if (!image) return '—';
    const last = image.split('/').pop() || image;
    return last.length > 40 ? `${last.slice(0, 37)}…` : last;
}

/** A one-line description of where a deployment came from. */
export function deploymentSource(d: Deployment): string {
    const pr = d.pull_request_url?.match(/\/(?:pull|merge_requests)\/(\d+)/);
    if (pr) return `${d.pull_request_url!.includes('merge_requests') ? '!' : '#'}${pr[1]} · ${shortImage(d.image)}`;
    return shortImage(d.image);
}
