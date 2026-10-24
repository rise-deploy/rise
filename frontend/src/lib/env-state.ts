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

/**
 * The badge for an environment: its health, worded so a failed deploy that
 * left the previous one serving doesn't read like an outage.
 */
export function healthBadge(state: EnvState): { status: string; label: string } {
    if (state.health === 'failed' && state.live && state.live.status === 'Healthy') {
        return { status: 'Failed', label: 'Deploy failed' };
    }
    return { status: HEALTH_LABEL[state.health], label: HEALTH_LABEL[state.health] };
}

/** The last segment of an image reference, e.g. `api:59a1ac`. */
export function shortImage(image?: string): string {
    if (!image) return '—';
    const last = image.split('/').pop() || image;
    return last.length > 40 ? `${last.slice(0, 37)}…` : last;
}

/** The first 7 characters of a commit hash. */
export function shortRevision(revision?: string): string {
    return revision ? revision.slice(0, 7) : '';
}

/** `#31` (GitHub) or `!31` (GitLab) for a pull/merge request URL. */
export function pullRequestLabel(url?: string): string | null {
    const m = url?.match(/\/(?:pull|pulls|merge_requests)\/(\d+)/);
    if (!m) return null;
    return `${url!.includes('merge_requests') ? '!' : '#'}${m[1]}`;
}

/**
 * Where a deployment came from, as one fact: its pull request, else the
 * deployment it was created from, else branch@commit, else its image.
 * `DeploymentSource` renders the same fact with the rest in a card.
 */
export function deploymentSource(d: Deployment): string {
    const pr = pullRequestLabel(d.pull_request_url);
    if (pr) return d.git_branch ? `${pr} · ${d.git_branch}` : pr;
    if (d.source_deployment_id) return `Redeploy of ${d.source_deployment_id.split('-').pop()}`;
    if (d.git_revision) return `${d.git_branch ? `${d.git_branch}@` : ''}${shortRevision(d.git_revision)}`;
    return shortImage(d.image);
}

/** A link to the commit on its forge, when the forge's URL layout is known. */
export function commitUrl(repo: string | undefined, revision: string | undefined): string | null {
    if (!repo || !revision) return null;
    try {
        const host = new URL(repo).hostname;
        const base = repo.replace(/\/+$/, '');
        if (host === 'github.com' || host.endsWith('.github.com')) return `${base}/commit/${revision}`;
        if (host.includes('gitlab')) return `${base}/-/commit/${revision}`;
        if (host === 'bitbucket.org') return `${base}/commits/${revision}`;
    } catch {
        // Not a URL.
    }
    return null;
}
