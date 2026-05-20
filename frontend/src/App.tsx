// @ts-nocheck
import React, { useEffect, useState } from 'react';
import { logout, login } from './lib/auth';
import { api } from './lib/api';
import { CONFIG } from './lib/config';
import { maybeMigrateLegacyHashRoute, navigate, usePathLocation } from './lib/navigation';
import { useToast } from './components/toast';
import { PlatformAccessDenied, LoadingState } from './components/states';
import { CommandPalette } from './components/command-palette';
import { Shell } from './components/shell';
import { Button } from './components/r-ui';
import { Icon } from './components/icon';
import { Home } from './features/home';
import { Profile } from './features/profile';
import { ProjectsList } from './features/projects-list';
import { ProjectDetail } from './features/project-detail';
import { DeploymentDetail, EnvironmentDeploymentView } from './features/deployments';
import { ExtensionDetailPage } from './features/resources';
import { TeamDetail, TeamsList } from './features/teams';
import { usePrefs } from './lib/prefs';
import { ErrorBoundary } from './components/error-boundary';

const TAB_LABELS = {
    deployments: 'Deployments',
    environments: 'Environments',
    'service-accounts': 'Service Accounts',
    'env-vars': 'Env Vars',
    domains: 'Domains',
    extensions: 'Extensions',
    access: 'Access',
};

function LoginPage() {
    const [status, setStatus] = useState('');
    const [loading, setLoading] = useState(false);
    return (
        <div className="r-login-wrap">
            <div className="r-login-card">
                <div className="r-login-logo">R</div>
                <div>
                    <div style={{ fontSize: 20, fontWeight: 600, letterSpacing: '-0.02em' }}>Welcome to Rise</div>
                    <div style={{ color: 'var(--text-muted)', fontSize: 13.5, marginTop: 6 }}>
                        Sign in to deploy and manage your apps.
                    </div>
                </div>
                {loading ? (
                    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 12, padding: '12px 0' }}>
                        <span className="r-spinner lg" />
                        <p style={{ color: 'var(--text-muted)', fontSize: 13, margin: 0 }}>{status}</p>
                    </div>
                ) : (
                    <>
                        <Button
                            variant="primary"
                            icon="lock"
                            style={{ width: '100%', justifyContent: 'center', padding: '10px 14px' }}
                            onClick={async () => {
                                setStatus('Redirecting to login…');
                                setLoading(true);
                                try { await login(); } catch (e) { setStatus(`Error: ${e.message}`); setLoading(false); }
                            }}
                        >
                            Continue with SSO
                        </Button>
                        {status && <p style={{ color: 'var(--err)', fontSize: 12, margin: 0 }}>{status}</p>}
                    </>
                )}
                <div style={{ color: 'var(--text-soft)', fontSize: 11.5 }}>
                    By continuing you agree to your organization's terms of use.
                </div>
            </div>
        </div>
    );
}

export function App() {
    // Initialize prefs hook so it applies defaults to the DOM on first render.
    usePrefs();

    const [user, setUser] = useState(null);
    const [authChecked, setAuthChecked] = useState(false);
    const [platformAccessDenied, setPlatformAccessDenied] = useState(false);
    const [commandPaletteOpen, setCommandPaletteOpen] = useState(false);
    const [paletteProjects, setPaletteProjects] = useState([]);
    const [paletteTeams, setPaletteTeams] = useState([]);
    const pathname = usePathLocation();
    const { showToast } = useToast();

    useEffect(() => {
        maybeMigrateLegacyHashRoute();

        if (window.location.hash && (window.location.hash.includes('access_token=') || window.location.hash.includes('error='))) {
            const fragment = window.location.hash.substring(1);
            const params = new URLSearchParams(fragment);
            const returnPath = sessionStorage.getItem('oauth_return_path');
            const error = params.get('error');
            const errorDescription = params.get('error_description');
            const accessToken = params.get('access_token');
            if (error) {
                showToast(errorDescription || `OAuth flow failed: ${error}`, 'error');
            } else if (accessToken) {
                const expiresIn = params.get('expires_in');
                const expiresAt = params.get('expires_at');
                let expiresAtDate;
                if (expiresAt) expiresAtDate = new Date(expiresAt);
                else if (expiresIn) expiresAtDate = new Date(Date.now() + parseInt(expiresIn) * 1000);
                showToast(`OAuth flow successful! Token expires ${expiresAtDate ? expiresAtDate.toLocaleString() : 'soon'}`, 'success');
            }
            sessionStorage.removeItem('oauth_return_path');
            if (returnPath) navigate(returnPath);
            else navigate('/home');
        }

        async function loadUser() {
            try {
                const userData = await api.getMe();
                setUser(userData);
            } catch (err) {
                console.error('Failed to load user:', err);
                setUser(null);
            } finally {
                setAuthChecked(true);
            }
        }
        loadUser();
    }, []);

    useEffect(() => {
        const handler = (e) => {
            const isModifierK = (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k';
            if (!isModifierK) return;
            const target = e.target;
            const isTypingTarget =
                target instanceof HTMLInputElement ||
                target instanceof HTMLTextAreaElement ||
                target?.isContentEditable;
            const modalOpen = Boolean(document.querySelector('.r-modal-mask, .modal-backdrop'));
            if (isTypingTarget && !modalOpen) return;
            e.preventDefault();
            setCommandPaletteOpen(true);
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
        async function loadPaletteTargets() {
            try {
                const [projects, teams] = await Promise.all([api.getProjects(), api.getTeams()]);
                setPaletteProjects(projects || []);
                setPaletteTeams(teams || []);
            } catch (err) {
                if (err.isPlatformAccessDenied) { setPlatformAccessDenied(true); return; }
                console.error('Failed to load command palette targets:', err);
            }
        }
        loadPaletteTargets();
    }, [user?.id]);

    if (!authChecked) {
        return (
            <div className="r-login-wrap">
                <span className="r-spinner lg" />
            </div>
        );
    }
    if (!user) return <LoginPage />;
    if (platformAccessDenied) return <PlatformAccessDenied userEmail={user.email} onLogout={logout} />;

    // ----- Route parsing -----
    let view = 'home';
    const params: any = {};
    const route = pathname.replace(/^\//, '');

    if (route === '' || route === 'home') {
        view = 'home';
    } else if (route === 'profile') {
        view = 'profile';
    } else if (route === 'projects') {
        view = 'projects';
    } else if (route === 'teams') {
        view = 'teams';
    } else if (route.startsWith('project/')) {
        const parts = route.split('/');
        if (parts[2] === 'environment' && parts[3]) {
            view = 'environment-deployment';
            params.projectName = parts[1];
            params.environmentName = parts[3];
            if (parts[4] === 'group' && parts[5]) params.groupName = parts[5];
        } else if (parts[2] === 'extensions') {
            if (parts.length === 3) { view = 'project-detail'; params.projectName = parts[1]; params.tab = 'extensions'; }
            else if (parts.length === 4 && parts[3] === '@new') { view = 'extension-create'; params.projectName = parts[1]; params.extensionType = null; }
            else if (parts.length === 5 && parts[3]) {
                if (parts[4] === '@new') { view = 'extension-detail'; params.projectName = parts[1]; params.extensionType = parts[3]; params.extensionInstance = null; }
                else { view = 'extension-detail'; params.projectName = parts[1]; params.extensionType = parts[3]; params.extensionInstance = parts[4]; }
            } else if (parts.length === 4 && parts[3]) {
                view = 'extension-detail'; params.projectName = parts[1]; params.extensionType = parts[3]; params.extensionInstance = null;
            }
        } else {
            view = 'project-detail';
            params.projectName = parts[1];
            params.tab = parts[2] || 'overview';
        }
    } else if (route.startsWith('team/')) {
        view = 'team-detail';
        params.teamName = route.split('/')[1];
    } else if (route.startsWith('deployment/')) {
        view = 'deployment-detail';
        const parts = route.split('/');
        params.projectName = parts[1];
        params.deploymentId = parts[2];
    } else {
        view = 'home';
    }

    // ----- Breadcrumbs -----
    let breadcrumbs: { label: string; href?: string }[];
    switch (view) {
        case 'home':              breadcrumbs = [{ label: 'Home' }]; break;
        case 'profile':           breadcrumbs = [{ label: 'Profile' }]; break;
        case 'projects':          breadcrumbs = [{ label: 'Projects' }]; break;
        case 'teams':             breadcrumbs = [{ label: 'Teams' }]; break;
        case 'project-detail':    breadcrumbs = [
            { label: 'Projects', href: '/projects' },
            { label: params.projectName, href: `/project/${params.projectName}` },
            ...(params.tab && params.tab !== 'overview' ? [{ label: TAB_LABELS[params.tab] || params.tab }] : []),
        ]; break;
        case 'team-detail':       breadcrumbs = [
            { label: 'Teams', href: '/teams' },
            { label: params.teamName },
        ]; break;
        case 'environment-deployment': breadcrumbs = [
            { label: 'Projects', href: '/projects' },
            { label: params.projectName, href: `/project/${params.projectName}` },
            { label: 'Environments', href: `/project/${params.projectName}/environments` },
            { label: params.environmentName + (params.groupName ? ` / ${params.groupName}` : '') },
        ]; break;
        case 'deployment-detail': breadcrumbs = [
            { label: 'Projects', href: '/projects' },
            { label: params.projectName, href: `/project/${params.projectName}` },
            { label: params.deploymentId },
        ]; break;
        case 'extension-detail':
        case 'extension-create':  breadcrumbs = [
            { label: 'Projects', href: '/projects' },
            { label: params.projectName, href: `/project/${params.projectName}` },
            { label: 'Extensions', href: `/project/${params.projectName}/extensions` },
            { label: params.extensionInstance || params.extensionType || 'Extension' },
        ]; break;
        default:                  breadcrumbs = [{ label: 'Home' }]; break;
    }

    // ----- Command palette items -----
    const commandItems = [
        { id: 'go-home', group: 'Navigate', label: 'Home', keywords: ['home', 'overview'], run: () => navigate('/home') },
        { id: 'go-projects', group: 'Navigate', label: 'Projects', keywords: ['projects'], run: () => navigate('/projects') },
        { id: 'go-teams', group: 'Navigate', label: 'Teams', keywords: ['teams'], run: () => navigate('/teams') },
        { id: 'go-profile', group: 'Navigate', label: 'Profile', keywords: ['profile', 'preferences', 'theme', 'density'], run: () => navigate('/profile') },
        { id: 'create-project', group: 'Action', label: 'Create project', hint: 'new', keywords: ['project', 'create', 'new'], run: () => navigate('/projects?create=project') },
        { id: 'create-team', group: 'Action', label: 'Create team', hint: 'new', keywords: ['team', 'create', 'new'], run: () => navigate('/teams?create=team') },
        { id: 'open-docs', group: 'Open', label: 'Documentation', hint: 'docs', keywords: ['help', 'docs'], run: () => { window.location.href = '/docs/'; } },
    ];
    paletteProjects.forEach(p => commandItems.push({
        id: `project-${p.id || p.name}`,
        group: 'Projects',
        label: p.name,
        hint: p.owner?.email || p.owner?.name || '',
        keywords: ['project', p.name, p.owner?.email, p.owner?.name].filter(Boolean) as string[],
        run: () => navigate(`/project/${p.name}`),
    }));
    paletteTeams.forEach(t => commandItems.push({
        id: `team-${t.id || t.name}`,
        group: 'Teams',
        label: t.name,
        hint: `${t.member_count || 0} members`,
        keywords: ['team', t.name],
        run: () => navigate(`/team/${t.name}`),
    }));

    const createIntent = new URLSearchParams(window.location.search).get('create');

    return (
        <>
            <Shell route={pathname} breadcrumbs={breadcrumbs} user={user} onLogout={logout} onOpenPalette={() => setCommandPaletteOpen(true)}>
                <ErrorBoundary key={pathname}>
                    {view === 'home' && <Home user={user} />}
                    {view === 'profile' && <Profile user={user} />}
                    {view === 'projects' && <ProjectsList openCreate={createIntent === 'project'} />}
                    {view === 'teams' && <TeamsList currentUser={user} openCreate={createIntent === 'team'} />}
                    {view === 'project-detail' && <ProjectDetail projectName={params.projectName} initialTab={params.tab} />}
                    {view === 'team-detail' && <TeamDetail teamName={params.teamName} currentUser={user} />}
                    {view === 'environment-deployment' && <EnvironmentDeploymentView projectName={params.projectName} environmentName={params.environmentName} groupName={params.groupName} />}
                    {view === 'deployment-detail' && <DeploymentDetail projectName={params.projectName} deploymentId={params.deploymentId} />}
                    {view === 'extension-detail' && <ExtensionDetailPage projectName={params.projectName} extensionType={params.extensionType} extensionInstance={params.extensionInstance} />}
                </ErrorBoundary>
            </Shell>
            <CommandPalette
                isOpen={commandPaletteOpen}
                onClose={() => setCommandPaletteOpen(false)}
                items={commandItems}
            />
        </>
    );
}
