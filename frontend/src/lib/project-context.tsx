import React, { createContext, useCallback, useContext, useEffect, useState } from 'react';
import { api } from './api';
import { usePolling } from './polling';
import { isInProgress } from './env-state';

// Shapes of the API responses the shell and the project sections share. Only
// the fields the UI reads are listed; the backend models in `src/server` are
// the source of truth.
export interface ProjectOwner { id: string; email?: string; name?: string }

export interface Project {
    id: string;
    name: string;
    status: string;
    access_class: string;
    owner?: ProjectOwner;
    active_deployment_status?: string;
    active_deployment_id?: string;
    /** Newest deployment of the production group; differs from the active one after a failed or in-flight deploy. */
    latest_deployment_status?: string;
    latest_deployment_id?: string;
    default_url?: string;
    primary_url?: string;
    custom_domain_urls?: string[];
    deployment_groups?: string[];
    source_url?: string;
    resolved_source_url?: string;
    platform_constraints?: unknown;
    template?: { id: string; image: string };
    created: string;
    updated: string;
    [key: string]: unknown;
}

export interface RollbackGuardConfig {
    enabled: boolean;
    unhealthy_timeout_seconds: number;
}

export type CreatedVia = 'deploy' | 'redeploy' | 'promote' | 'rollback_guard';

export interface Environment {
    name: string;
    primary_deployment_group: string | null;
    is_production: boolean;
    color: string;
    max_deployment_expiration: string | null;
    deployment_constraints?: unknown;
    /** Only deployments promoted from this environment (or redeploys within this one) may land here. */
    promotion_source_environment?: string | null;
    rollback_guard?: RollbackGuardConfig;
    created_at: string;
    updated_at: string;
}

export interface Deployment {
    id: string;
    deployment_id: string;
    project: string;
    created_by_email: string;
    status: string;
    deployment_group: string;
    environment?: string;
    environment_color?: string;
    expires_at?: string;
    error_message?: string;
    completed_at?: string;
    build_logs?: string;
    primary_url?: string;
    all_urls?: string[];
    image?: string;
    image_digest?: string;
    http_port: number;
    is_active: boolean;
    can_rollback: boolean;
    replicas: number;
    cpu: string;
    memory: string;
    job_url?: string;
    pull_request_url?: string;
    git_repository_url?: string;
    /** Commit the image was built from (inherited by redeploys, rollbacks and promotions). */
    git_revision?: string;
    git_branch?: string;
    /** Built from a working tree with uncommitted changes. */
    git_dirty?: boolean;
    /** The deployment this one was created from, if any. */
    source_deployment_id?: string;
    created_via?: CreatedVia | string;
    /** The immediate source of a promotion, in another environment. */
    promoted_from_deployment_id?: string;
    promoted_from_environment?: string;
    /** `[project].migration_version` of the code, inherited along redeploys and promotions. */
    migration_version?: number;
    unhealthy_since?: string;
    /** Set on a Rollback Guard replacement: the unhealthy deployment it replaces. */
    rollback_guard_for_deployment_id?: string;
    created: string;
    updated: string;
    [key: string]: unknown;
}

export interface ProjectCounts {
    deployments?: number;
    environments?: number;
    variables?: number;
    domains?: number;
    extensions?: number;
}

interface ProjectContextValue {
    projectName: string;
    project: Project | null;
    environments: Environment[];
    /**
     * Newest first: the most recent page, plus every active deployment and the
     * recent history of each environment's primary group.
     */
    deployments: Deployment[];
    counts: ProjectCounts;
    error: string | null;
    reload: () => void;
}

const ProjectContext = createContext<ProjectContextValue | null>(null);

const DEPLOYMENT_PAGE = 100;
/** History read per primary group: enough for what it serves and what it replaced. */
const GROUP_PAGE = 20;

/**
 * Loads the data every project-scoped screen and the project sidebar share,
 * once per project, and refreshes it on the app-wide `rise:mutation` event.
 */
export function ProjectProvider({ projectName, children }: { projectName: string; children: React.ReactNode }) {
    const [project, setProject] = useState<Project | null>(null);
    const [environments, setEnvironments] = useState<Environment[]>([]);
    const [deployments, setDeployments] = useState<Deployment[]>([]);
    const [counts, setCounts] = useState<ProjectCounts>({});
    const [error, setError] = useState<string | null>(null);
    const [generation, setGeneration] = useState(0);

    const reload = useCallback(() => setGeneration(g => g + 1), []);
    // CLI and CI deploys change what environments serve without telling this
    // page: refresh quickly while a rollout is in flight, slowly otherwise.
    const busy = project?.status === 'Deploying' || deployments.some(d => isInProgress(d.status));
    usePolling(reload, busy ? 5000 : 20000);

    useEffect(() => {
        setProject(null);
        setEnvironments([]);
        setDeployments([]);
        setCounts({});
        setError(null);
    }, [projectName]);

    useEffect(() => {
        let cancelled = false;
        const record = (key: keyof ProjectCounts, n: number) => {
            if (!cancelled) setCounts(c => ({ ...c, [key]: n }));
        };
        api.getProject(projectName)
            .then((p: Project) => { if (!cancelled) { setProject(p); setError(null); } })
            .catch((err: Error) => { if (!cancelled) setError(err.message); });
        const envsReq: Promise<Environment[]> = api.getProjectEnvironments(projectName)
            .then((d: unknown) => {
                const list = Array.isArray(d) ? d as Environment[] : [];
                if (!cancelled) setEnvironments(list);
                record('environments', list.length);
                return list;
            })
            .catch((err: unknown) => {
                console.error('Failed to load environments:', err);
                return [] as Environment[];
            });
        // The recent page alone can miss what an environment serves: enough
        // newer preview deployments push production's live one off it. So the
        // active deployments and each environment's primary-group history are
        // read as well, and merged in.
        const recentReq = api.getProjectDeploymentsPage(projectName, { limit: DEPLOYMENT_PAGE })
            .then(({ items, total }: { items: unknown; total: number | null }) => {
                const list = Array.isArray(items) ? items as Deployment[] : [];
                record('deployments', total ?? list.length);
                return list;
            });
        const activeReq = api.getProjectDeployments(projectName, { active: true, limit: DEPLOYMENT_PAGE })
            .then((d: unknown) => Array.isArray(d) ? d as Deployment[] : [])
            .catch(() => [] as Deployment[]);
        const groupsReq = envsReq.then(envs => Promise.all(
            Array.from(new Set(envs.map(e => e.primary_deployment_group).filter((g): g is string => !!g))).map(group =>
                api.getProjectDeployments(projectName, { group, limit: GROUP_PAGE })
                    .then((d: unknown) => Array.isArray(d) ? d as Deployment[] : [])
                    .catch(() => [] as Deployment[])),
        ));
        Promise.all([recentReq, activeReq, groupsReq])
            .then(([recent, active, groups]) => {
                if (cancelled) return;
                const byId = new Map<string, Deployment>();
                for (const d of [...recent, ...active, ...groups.flat()]) byId.set(d.deployment_id, d);
                setDeployments([...byId.values()].sort((a, b) => (b.created || '').localeCompare(a.created || '')));
            })
            .catch((err: unknown) => console.error('Failed to load deployments:', err));
        api.getProjectEnvVars(projectName, null)
            .then((d: { env_vars?: { key: string }[] }) => record('variables', new Set((d?.env_vars || []).map(v => v.key)).size))
            .catch(() => {});
        api.getProjectDomains(projectName)
            .then((d: unknown) => record('domains', Array.isArray(d) ? d.length : ((d as { domains?: unknown[] })?.domains?.length ?? 0)))
            .catch(() => {});
        api.getProjectExtensions(projectName)
            .then((d: unknown) => record('extensions', Array.isArray(d) ? d.length : ((d as { extensions?: unknown[] })?.extensions?.length ?? 0)))
            .catch(() => {});
        return () => { cancelled = true; };
    }, [projectName, generation]);

    useEffect(() => {
        window.addEventListener('rise:mutation', reload);
        return () => window.removeEventListener('rise:mutation', reload);
    }, [reload]);

    return (
        <ProjectContext.Provider value={{ projectName, project, environments, deployments, counts, error, reload }}>
            {children}
        </ProjectContext.Provider>
    );
}

/** The current project, when rendered inside a `ProjectProvider`. */
export function useCurrentProject(): ProjectContextValue | null {
    return useContext(ProjectContext);
}
