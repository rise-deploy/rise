import React, { useCallback, useEffect, useState } from 'react';
import { logout, login } from './lib/auth';
import { api } from './lib/api';
import { maybeMigrateLegacyHashRoute, navigate, usePathLocation } from './lib/navigation';
import { useToast } from './components/toast';
import { PlatformAccessDenied } from './components/states';
import { CommandPalette, type CommandItem } from './components/command-palette';
import { Shell, type Crumb } from './components/shell';
import { Icon } from './components/icon';
import { Home } from './features/home';
import { Profile } from './features/profile';
import { DeviceLogin } from './features/device-login';
import { ProjectsList } from './features/projects-list';
import { NewProjectPage } from './features/new-project';
import { ProjectDetail } from './features/project-detail';
import { DeploymentDetail, EnvironmentDeploymentView } from './features/deployments';
import { DeploymentLogsPage } from './features/logs/logs-page';
import { ExtensionDetailPage } from './features/resources';
import { TeamsPage } from './features/teams';
import { resolveTheme, usePrefs } from './lib/prefs';
import { ErrorBoundary } from './components/error-boundary';
import { ProjectProvider, useCurrentProject, type Project } from './lib/project-context';
import { DeployActionsProvider, useDeployActions } from './components/deploy-actions';
import { envState, productionEnv } from './lib/env-state';
import { ProjectLogsPage } from './features/logs/project-logs';
import { rememberProject } from './lib/recent';
import { PROJECT_SECTIONS, logsPath, parseRoute, projectPath, routeProject, type Route } from './lib/routes';

export interface CurrentUser {
    id: string;
    email: string;
    is_admin?: boolean;
    is_operator?: boolean;
    can_create_teams?: boolean;
}

interface TeamSummary { id: string; name: string; members?: unknown[] }

function LoginPage() {
    const [status, setStatus] = useState('');
    const [loading, setLoading] = useState(false);
    const signIn = async () => {
        setStatus('Redirecting to sign-in…');
        setLoading(true);
        try {
            await login();
        } catch (e) {
            setStatus(`Error: ${(e as Error).message}`);
            setLoading(false);
        }
    };
    return (
        <div className="r-login-wrap">
            <div className="r-signin">
                <div className="r-signin-brand">
                    <div className="r-brand-mark">R</div>
                    <div className="r-brand-name">Rise</div>
                </div>
                <div>
                    <h1 className="r-page-title">Sign in</h1>
                    <div className="r-page-sub">Deploy and operate your projects.</div>
                </div>
                {loading ? (
                    <div className="r-signin-pending">
                        <span className="r-spinner" />
                        <span>{status}</span>
                    </div>
                ) : (
                    <>
                        <button type="button" className="r-btn primary r-btn-block" onClick={signIn}>
                            <Icon name="lock" size={14} />
                            Continue with SSO
                        </button>
                        {status && <p className="r-signin-error">{status}</p>}
                    </>
                )}
                <div className="r-signin-foot">By continuing you agree to your organization's terms of use.</div>
            </div>
        </div>
    );
}

/**
 * The command palette with the current project's deploy actions on top: what
 * production serves can be redeployed or rolled back without leaving the
 * keyboard.
 */
function ProjectPalette(props: React.ComponentProps<typeof CommandPalette>) {
    const current = useCurrentProject();
    const { request } = useDeployActions();
    const items: CommandItem[] = [];
    const prod = current ? productionEnv(current.environments) : null;
    if (current && prod) {
        const state = envState(prod, current.deployments);
        const p = current.projectName;
        if (state.live) {
            const live = state.live;
            items.push({ id: 'act-redeploy', kind: 'Action', icon: 'refresh', label: `Redeploy ${p} · ${prod.name}`, run: () => request({ kind: 'redeploy', source: live }) });
        }
        if (state.previous) {
            const previous = state.previous;
            items.push({ id: 'act-rollback', kind: 'Action', icon: 'rollback', label: `Roll back ${p} · ${prod.name}`, sub: `to ${previous.deployment_id}`, run: () => request({ kind: 'rollback', source: previous }) });
        }
        const logTarget = state.live ?? state.latest;
        items.push({ id: 'act-logs', kind: 'Go to', icon: 'terminal', label: `${p} · ${prod.name} logs`, run: () => navigate(logsPath(p, logTarget?.deployment_id)) });
    }
    return <CommandPalette {...props} items={[...items, ...props.items]} />;
}

function sectionLabel(id: string): string {
    return PROJECT_SECTIONS.find(s => s.id === id)?.label ?? id;
}

function breadcrumbsFor(route: Route): Crumb[] {
    const projects: Crumb = { label: 'Projects', href: '/projects' };
    const project = (name: string): Crumb => ({ label: name, href: projectPath(name) });
    const section = (name: string, id: Parameters<typeof projectPath>[1]): Crumb => ({ label: sectionLabel(id!), href: projectPath(name, id) });
    switch (route.view) {
        case 'home': return [{ label: 'Home' }];
        case 'profile': return [{ label: 'Profile' }];
        case 'device': return [{ label: 'Device login' }];
        case 'projects': return [{ label: 'Projects' }];
        case 'new-project': return [{ label: 'Projects', href: '/projects' }, { label: 'New project' }];
        case 'teams': return [{ label: 'Teams' }];
        case 'team-detail': return [{ label: 'Teams', href: '/teams' }, { label: route.teamName }];
        case 'project':
            return route.section === 'overview'
                ? [projects, { label: route.projectName }]
                : [projects, project(route.projectName), { label: sectionLabel(route.section) }];
        case 'environment-deployment':
            return [projects, project(route.projectName), section(route.projectName, 'environments'),
                { label: route.environmentName + (route.groupName ? ` / ${route.groupName}` : '') }];
        case 'extension-detail':
            return [projects, project(route.projectName), section(route.projectName, 'extensions'),
                { label: route.extensionInstance || route.extensionType }];
        case 'deployment-detail':
            return [projects, project(route.projectName), section(route.projectName, 'deployments'),
                { label: route.deploymentId, mono: true }];
        case 'deployment-logs':
            return [projects, project(route.projectName), section(route.projectName, 'deployments'),
                { label: route.deploymentId, href: `/deployment/${route.projectName}/${route.deploymentId}`, mono: true },
                { label: 'Logs' }];
    }
}

export function App() {
    // Initialize prefs hook so it applies defaults to the DOM on first render.
    const [prefs, setPrefs] = usePrefs();

    const [user, setUser] = useState<CurrentUser | null>(null);
    const [authChecked, setAuthChecked] = useState(false);
    const [platformAccessDenied, setPlatformAccessDenied] = useState(false);
    const [palette, setPalette] = useState<{ open: boolean; scope?: 'projects' }>({ open: false });
    const [paletteProjects, setPaletteProjects] = useState<Project[]>([]);
    const [paletteTeams, setPaletteTeams] = useState<TeamSummary[]>([]);
    let pathname = usePathLocation();
    const { showToast } = useToast();

    useEffect(() => {
        maybeMigrateLegacyHashRoute();

        // The OAuth extension's "Test OAuth Flow" returns here with the result
        // in the URL fragment.
        if (window.location.hash && (window.location.hash.includes('access_token=') || window.location.hash.includes('error='))) {
            const params = new URLSearchParams(window.location.hash.substring(1));
            const returnPath = sessionStorage.getItem('oauth_return_path');
            const error = params.get('error');
            const errorDescription = params.get('error_description');
            const accessToken = params.get('access_token');
            if (error) {
                showToast(errorDescription || `OAuth flow failed: ${error}`, 'error');
            } else if (accessToken) {
                const expiresIn = params.get('expires_in');
                const expiresAt = params.get('expires_at');
                let expiresAtDate: Date | undefined;
                if (expiresAt) expiresAtDate = new Date(expiresAt);
                else if (expiresIn) expiresAtDate = new Date(Date.now() + parseInt(expiresIn, 10) * 1000);
                showToast(`OAuth flow successful! Token expires ${expiresAtDate ? expiresAtDate.toLocaleString() : 'soon'}`, 'success');
            }
            sessionStorage.removeItem('oauth_return_path');
            navigate(returnPath || '/home');
        }

        api.getMe()
            .then((me: CurrentUser) => setUser(me))
            .catch((err: unknown) => {
                console.error('Failed to load user:', err);
                setUser(null);
            })
            .finally(() => setAuthChecked(true));
        // eslint-disable-next-line react-hooks/exhaustive-deps -- runs once on boot
    }, []);

    const openPalette = useCallback((scope?: 'projects') => setPalette({ open: true, scope }), []);
    const closePalette = useCallback(() => setPalette({ open: false }), []);

    useEffect(() => {
        const handler = (e: KeyboardEvent) => {
            const isModifierK = (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k';
            if (!isModifierK) return;
            const target = e.target;
            const isTypingTarget =
                target instanceof HTMLInputElement ||
                target instanceof HTMLTextAreaElement ||
                (target instanceof HTMLElement && target.isContentEditable);
            const modalOpen = Boolean(document.querySelector('.r-modal-mask, .modal-backdrop, .r-cmdp-mask'));
            if (isTypingTarget && !modalOpen) return;
            e.preventDefault();
            setPalette(p => p.open ? { open: false } : { open: true });
        };
        window.addEventListener('keydown', handler);
        return () => window.removeEventListener('keydown', handler);
    }, []);

    useEffect(() => {
        if (!user) {
            setPaletteProjects([]);
            setPaletteTeams([]);
            return;
        }
        let cancelled = false;
        async function loadPaletteTargets() {
            try {
                const [projects, teams] = await Promise.all([api.getProjects(), api.getTeams()]);
                if (cancelled) return;
                setPaletteProjects(projects || []);
                setPaletteTeams(teams || []);
            } catch (err) {
                if ((err as { isPlatformAccessDenied?: boolean }).isPlatformAccessDenied) { setPlatformAccessDenied(true); return; }
                console.error('Failed to load command palette targets:', err);
            }
        }
        loadPaletteTargets();
        window.addEventListener('rise:mutation', loadPaletteTargets);
        return () => {
            cancelled = true;
            window.removeEventListener('rise:mutation', loadPaletteTargets);
        };
    }, [user?.id]);

    // `/projects?create=project` predates the new-project page.
    if (pathname === '/projects' && new URLSearchParams(window.location.search).get('create') === 'project') {
        window.history.replaceState({}, '', '/projects/new');
        pathname = '/projects/new';
    }
    const route = parseRoute(pathname);
    const currentProject = routeProject(route);

    useEffect(() => {
        if (currentProject) rememberProject(currentProject);
    }, [currentProject]);

    if (!authChecked) {
        return (
            <div className="r-login-wrap">
                <span className="r-spinner lg" />
            </div>
        );
    }
    if (!user) return <LoginPage />;
    if (platformAccessDenied) return <PlatformAccessDenied userEmail={user.email} onLogout={logout} />;

    const theme = resolveTheme(prefs.theme);
    const commandItems: CommandItem[] = [];
    if (currentProject) {
        PROJECT_SECTIONS.filter(s => s.id !== 'overview' && s.id !== 'logs').forEach(s => commandItems.push({
            id: `section-${s.id}`,
            kind: 'Go to',
            icon: s.icon,
            label: `${currentProject} · ${s.label.toLowerCase()}`,
            keywords: [s.id],
            run: () => navigate(projectPath(currentProject, s.id)),
        }));
    }
    paletteProjects.forEach(p => commandItems.push({
        id: `project-${p.id || p.name}`,
        kind: 'Project',
        icon: 'cube',
        label: p.name,
        sub: p.owner?.name || p.owner?.email || '',
        run: () => navigate(projectPath(p.name)),
    }));
    commandItems.push(
        { id: 'create-project', kind: 'Action', icon: 'plus', label: 'New project', keywords: ['create'], run: () => navigate('/projects/new') },
        { id: 'create-team', kind: 'Action', icon: 'plus', label: 'New team', keywords: ['create'], run: () => navigate('/teams?create=team') },
    );
    paletteTeams.forEach(t => commandItems.push({
        id: `team-${t.id || t.name}`,
        kind: 'Team',
        icon: 'users',
        label: t.name,
        sub: `${t.members?.length || 0} members`,
        run: () => navigate(`/team/${t.name}`),
    }));
    commandItems.push(
        { id: 'go-home', kind: 'Go to', icon: 'home', label: 'Go to Home', run: () => navigate('/home') },
        { id: 'go-projects', kind: 'Go to', icon: 'cube', label: 'Go to Projects', run: () => navigate('/projects') },
        { id: 'go-teams', kind: 'Go to', icon: 'users', label: 'Go to Teams', run: () => navigate('/teams') },
        { id: 'go-profile', kind: 'Go to', icon: 'user', label: 'Profile & preferences', keywords: ['theme', 'density', 'palette'], run: () => navigate('/profile') },
        { id: 'toggle-theme', kind: 'Action', icon: theme === 'dark' ? 'sun' : 'moon', label: theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode', keywords: ['theme', 'dark', 'light'], run: () => setPrefs({ theme: theme === 'dark' ? 'light' : 'dark' }) },
        { id: 'open-docs', kind: 'Go to', icon: 'info', label: 'Documentation', keywords: ['help', 'docs'], run: () => { window.location.href = '/docs/'; } },
    );

    const createIntent = new URLSearchParams(window.location.search).get('create');

    const page = (
        <ErrorBoundary key={pathname}>
            {route.view === 'home' && <Home user={user} />}
            {route.view === 'profile' && <Profile user={user} onLogout={logout} />}
            {route.view === 'device' && <DeviceLogin />}
            {route.view === 'projects' && <ProjectsList />}
            {route.view === 'new-project' && <NewProjectPage />}
            {route.view === 'teams' && <TeamsPage currentUser={user} openCreate={createIntent === 'team'} />}
            {route.view === 'team-detail' && <TeamsPage currentUser={user} teamName={route.teamName} />}
            {route.view === 'project' && route.section === 'logs' && <ProjectLogsPage />}
            {route.view === 'project' && route.section !== 'logs' && <ProjectDetail section={route.section} />}
            {route.view === 'environment-deployment' && <EnvironmentDeploymentView projectName={route.projectName} environmentName={route.environmentName} groupName={route.groupName} />}
            {route.view === 'deployment-detail' && <DeploymentDetail projectName={route.projectName} deploymentId={route.deploymentId} />}
            {route.view === 'deployment-logs' && <DeploymentLogsPage projectName={route.projectName} deploymentId={route.deploymentId} />}
            {route.view === 'extension-detail' && <ExtensionDetailPage projectName={route.projectName} extensionType={route.extensionType} extensionInstance={route.extensionInstance} />}
        </ErrorBoundary>
    );

    const shell = (
        <Shell
            route={route}
            pathname={pathname}
            breadcrumbs={breadcrumbsFor(route)}
            user={user}
            onLogout={logout}
            fullBleed={route.view === 'deployment-logs' || (route.view === 'project' && route.section === 'logs')}
            onOpenPalette={openPalette}
        >
            {page}
        </Shell>
    );

    const paletteProps = { isOpen: palette.open, scope: palette.scope, onClose: closePalette, items: commandItems };
    if (!currentProject) {
        return (
            <>
                {shell}
                <CommandPalette {...paletteProps} />
            </>
        );
    }
    return (
        <ProjectProvider projectName={currentProject}>
            <DeployActionsProvider>
                {shell}
                <ProjectPalette {...paletteProps} />
            </DeployActionsProvider>
        </ProjectProvider>
    );
}
