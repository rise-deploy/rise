// URL → view mapping. Routes are keyed by names (projects, teams, environments),
// never by internal IDs, so every screen is a shareable link.

export type ProjectSection =
    | 'overview'
    | 'deployments'
    | 'environments'
    | 'variables'
    | 'domains'
    | 'extensions'
    | 'access';

export interface ProjectSectionInfo {
    id: ProjectSection;
    label: string;
    /** Shorter label for the mobile tab bar. */
    short: string;
    icon: string;
    /** Section summary shown under the page title. */
    sub: string;
}

export const PROJECT_SECTIONS: ProjectSectionInfo[] = [
    { id: 'overview', label: 'Overview', short: 'Overview', icon: 'grid', sub: '' },
    { id: 'deployments', label: 'Deployments', short: 'Deploys', icon: 'commit', sub: 'Every deployment of this project, newest first' },
    { id: 'environments', label: 'Environments', short: 'Envs', icon: 'layer', sub: 'Promote, roll back and redeploy each environment' },
    { id: 'variables', label: 'Variables', short: 'Vars', icon: 'key', sub: 'Environment variables, applied on the next deployment' },
    { id: 'domains', label: 'Domains', short: 'Domains', icon: 'globe', sub: 'Custom domains per environment' },
    { id: 'extensions', label: 'Extensions', short: 'Extensions', icon: 'puzzle', sub: 'Managed resources and integrations attached to this project' },
    { id: 'access', label: 'Access', short: 'Access', icon: 'lock', sub: 'Who can reach the deployed app, ownership and service accounts' },
];

/** Sections shown directly in the mobile tab bar; the rest live under "More". */
export const MOBILE_PRIMARY_SECTIONS: ProjectSection[] = ['overview', 'deployments', 'environments', 'variables'];

// Paths that predate the current section names keep working.
const SECTION_ALIASES: Record<string, ProjectSection> = {
    'env-vars': 'variables',
    'service-accounts': 'access',
};

export type Route =
    | { view: 'home' }
    | { view: 'profile' }
    | { view: 'device' }
    | { view: 'projects' }
    | { view: 'teams' }
    | { view: 'team-detail'; teamName: string }
    | { view: 'project'; projectName: string; section: ProjectSection }
    | { view: 'environment-deployment'; projectName: string; environmentName: string; groupName?: string }
    | { view: 'extension-detail'; projectName: string; extensionType: string; extensionInstance: string | null }
    | { view: 'deployment-detail'; projectName: string; deploymentId: string }
    | { view: 'deployment-logs'; projectName: string; deploymentId: string };

export function parseRoute(pathname: string): Route {
    const parts = pathname.replace(/^\//, '').split('/').filter(Boolean).map(decodeURIComponent);
    const [head, a, b, c, d, e] = parts;

    switch (head) {
        case undefined:
        case 'home':
            return { view: 'home' };
        case 'profile':
            return { view: 'profile' };
        case 'device':
            return { view: 'device' };
        case 'projects':
            return { view: 'projects' };
        case 'teams':
            return { view: 'teams' };
        case 'team':
            return a ? { view: 'team-detail', teamName: a } : { view: 'teams' };
        case 'deployment':
            if (!a || !b) return { view: 'projects' };
            return c === 'logs'
                ? { view: 'deployment-logs', projectName: a, deploymentId: b }
                : { view: 'deployment-detail', projectName: a, deploymentId: b };
        case 'project': {
            if (!a) return { view: 'projects' };
            if (b === 'environment' && c) {
                return { view: 'environment-deployment', projectName: a, environmentName: c, groupName: d === 'group' && e ? e : undefined };
            }
            if (b === 'extensions' && c) {
                return { view: 'extension-detail', projectName: a, extensionType: c, extensionInstance: d && d !== '@new' ? d : null };
            }
            const section = SECTION_ALIASES[b ?? ''] ?? (PROJECT_SECTIONS.some(s => s.id === b) ? b as ProjectSection : 'overview');
            return { view: 'project', projectName: a, section };
        }
        default:
            return { view: 'home' };
    }
}

/** The project a route belongs to, if it is project-scoped. */
export function routeProject(route: Route): string | null {
    return 'projectName' in route ? route.projectName : null;
}

/** The project section a route highlights in the sidebar. */
export function routeSection(route: Route): ProjectSection | null {
    switch (route.view) {
        case 'project': return route.section;
        case 'deployment-detail':
        case 'deployment-logs': return 'deployments';
        case 'environment-deployment': return 'environments';
        case 'extension-detail': return 'extensions';
        default: return null;
    }
}

export function projectPath(projectName: string, section: ProjectSection = 'overview'): string {
    return section === 'overview' ? `/project/${projectName}` : `/project/${projectName}/${section}`;
}

export function deploymentPath(projectName: string, deploymentId: string): string {
    return `/deployment/${projectName}/${deploymentId}`;
}
