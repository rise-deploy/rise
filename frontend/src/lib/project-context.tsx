import React, { createContext, useCallback, useContext, useEffect, useState } from 'react';
import { api } from './api';

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

export interface Environment {
    name: string;
    primary_deployment_group: string | null;
    is_production: boolean;
    color: string;
    max_deployment_expiration: string | null;
    deployment_constraints?: unknown;
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
    /** The most recent deployments (newest first, capped by the API page). */
    deployments: Deployment[];
    counts: ProjectCounts;
    error: string | null;
    reload: () => void;
}

const ProjectContext = createContext<ProjectContextValue | null>(null);

const DEPLOYMENT_PAGE = 100;

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
        api.getProjectEnvironments(projectName)
            .then((d: unknown) => {
                const list = Array.isArray(d) ? d as Environment[] : [];
                if (!cancelled) setEnvironments(list);
                record('environments', list.length);
            })
            .catch((err: unknown) => console.error('Failed to load environments:', err));
        api.getProjectDeployments(projectName, { limit: DEPLOYMENT_PAGE })
            .then((d: unknown) => {
                const list = Array.isArray(d) ? d as Deployment[] : [];
                if (!cancelled) setDeployments(list);
                record('deployments', list.length);
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
