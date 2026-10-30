import { byNewest, isInProgress } from './env-state';
import type { Deployment, Project } from './project-context';

/** Historical failed attempts stop appearing on Home after seven days. */
export const ATTENTION_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;

export interface Attention {
    project: string;
    kind: 'failed' | 'deploying';
    deployment: Deployment;
    serving: Deployment | null;
}

export function deploymentAttentionKey(project: string, d: Deployment): string {
    return JSON.stringify(['deployment', project, d.id || d.deployment_id, d.status]);
}

export function projectAttentionKey(p: Project): string {
    return JSON.stringify(['project', p.id, p.status, p.latest_deployment_id, p.updated]);
}

/** Per environment and group: the newest failed attempt or an ongoing problem. */
export function attentionFor(project: string, deployments: Deployment[], now = Date.now()): Attention[] {
    const out: Attention[] = [];
    const groups = new Map<string, Deployment[]>();
    for (const d of deployments) {
        const key = JSON.stringify([d.environment, d.deployment_group || 'default']);
        groups.set(key, [...(groups.get(key) ?? []), d]);
    }
    for (const ds of groups.values()) {
        const sorted = [...ds].sort(byNewest);
        const latest = sorted[0];
        const serving = sorted.find(d => d.is_active) ?? null;
        const recent = now - Date.parse(latest.created) <= ATTENTION_MAX_AGE_MS;
        if (latest.status === 'Failed' && serving !== latest && recent) {
            out.push({ project, kind: 'failed', deployment: latest, serving });
        } else if (latest.status === 'Unhealthy' && (latest.is_active || recent)) {
            out.push({ project, kind: 'failed', deployment: latest, serving: null });
        } else if (isInProgress(latest.status)) {
            out.push({ project, kind: 'deploying', deployment: latest, serving });
        }
    }
    return out;
}

/** Deployment failures need dated deployment data; project lifecycle failures are ongoing. */
export function projectNeedsAttention(p: Project): boolean {
    if (p.status === 'Deleting') return true;
    if (p.status !== 'Failed') return false;
    // Ongoing unhealthy service remains actionable even if outside the recent page.
    // Historical failed attempts must use their deployment's age and dismissal.
    return p.active_deployment_status === 'Unhealthy' || !p.latest_deployment_id;
}
