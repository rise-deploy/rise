// @ts-nocheck
import { useEffect, useRef, useState } from 'react';
import { logout, login } from './lib/auth';
import { api } from './lib/api';
import { CONFIG } from './lib/config';
import { maybeMigrateLegacyHashRoute, navigate, usePathLocation } from './lib/navigation';
import { Footer } from './components/ui';
import { useToast } from './components/toast';
import { PlatformAccessDenied } from './components/states';
import { DeploymentDetail, EnvironmentDeploymentView } from './features/deployments';
import { ProjectsList, ProjectDetail } from './features/projects';
import { ExtensionDetailPage } from './features/resources';
import { TeamDetail, TeamsList } from './features/teams';
import { CommandPalette } from './components/command-palette';

function Sidebar({ currentView, user, onLogout }) {
    const isProjectsActive = currentView === 'projects' || currentView === 'project-detail' || currentView === 'deployment-detail' || currentView === 'environment-deployment' || currentView === 'extension-detail';
    const isTeamsActive = currentView === 'teams' || currentView === 'team-detail';

    return (
        <aside className="mono-sidebar">
            <div className="mono-brand" onClick={() => navigate('/projects')}>
                <div
                    className="mono-brand-logo svg-mask"
                    aria-hidden="true"
                    style={{
                        maskImage: 'url(/assets/logo.svg)',
                        WebkitMaskImage: 'url(/assets/logo.svg)',
                    }}
                ></div>
                <div>
                    <strong>RISE</strong>
                    <p>OPS TERMINAL</p>
                </div>
            </div>

            <nav className="mono-nav" aria-label="Main">
                <a
                    href="/projects"
                    onClick={(e) => {
                        e.preventDefault();
                        navigate('/projects');
                    }}
                    className={isProjectsActive ? 'active' : ''}
                >
                    <span className="w-4 h-4 svg-mask inline-block flex-shrink-0" aria-hidden="true" style={{ maskImage: 'url(/assets/lightning.svg)', WebkitMaskImage: 'url(/assets/lightning.svg)' }}></span>
                    Projects
                </a>
                <a
                    href="/teams"
                    onClick={(e) => {
                        e.preventDefault();
                        navigate('/teams');
                    }}
                    className={isTeamsActive ? 'active' : ''}
                >
                    <span className="w-4 h-4 svg-mask inline-block flex-shrink-0" aria-hidden="true" style={{ maskImage: 'url(/assets/user.svg)', WebkitMaskImage: 'url(/assets/user.svg)' }}></span>
                    Teams
                </a>
            </nav>

            <div style={{ marginTop: 'auto', display: 'flex', flexDirection: 'column', gap: '1rem' }}>
                <nav className="mono-nav" aria-label="Secondary">
                    <a href="/docs/">
                        <span className="w-4 h-4 svg-mask inline-block flex-shrink-0" aria-hidden="true" style={{ maskImage: 'url(/assets/file.svg)', WebkitMaskImage: 'url(/assets/file.svg)' }}></span>
                        Docs
                    </a>
                    <div className="mono-nav-static" style={{ minWidth: 0 }}>
                        <span
                            className="w-4 h-4 svg-mask inline-block flex-shrink-0"
                            aria-hidden="true"
                            style={{
                                maskImage: 'url(/assets/user.svg)',
                                WebkitMaskImage: 'url(/assets/user.svg)',
                            }}
                        ></span>
                        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>
                            {user?.email}
                        </span>
                        <span className="mono-nav-vsep" aria-hidden="true"></span>
                        <button
                            onClick={onLogout}
                            className="mono-icon-button"
                            aria-label="Logout"
                            title="Logout"
                        >
                            <span
                                className="w-4 h-4 svg-mask inline-block"
                                aria-hidden="true"
                                style={{
                                    maskImage: 'url(/assets/logout.svg)',
                                    WebkitMaskImage: 'url(/assets/logout.svg)',
                                }}
                            ></span>
                        </button>
                    </div>
                </nav>
                <div className="mono-shortcut-hint" aria-label="Command palette shortcut">
                    <span>Command palette</span>
                    <code>Ctrl/Cmd + K</code>
                </div>
            </div>
        </aside>
    );
}

function TopBar({ breadcrumbs = [] }) {
    return (
        <header className="mono-topbar">
            <div>
                <p className="mono-kicker">context</p>
                <div className="mono-breadcrumbs">
                    {breadcrumbs.map((crumb, idx) => {
                        const isLast = idx === breadcrumbs.length - 1;
                        return (
                            <span key={`${crumb.label}-${idx}`} className="mono-breadcrumb-item">
                                {crumb.href && !isLast ? (
                                    <a
                                        href={crumb.href}
                                        onClick={(e) => {
                                            e.preventDefault();
                                            navigate(crumb.href);
                                        }}
                                    >
                                        {crumb.label}
                                    </a>
                                ) : (
                                    <strong>{crumb.label}</strong>
                                )}
                                {!isLast && <span className="mono-breadcrumb-sep">/</span>}
                            </span>
                        );
                    })}
                </div>
            </div>
        </header>
    );
}

function LoginPage() {
    const [status, setStatus] = useState('');
    const [loading, setLoading] = useState(false);

    const handleLogin = async () => {
        setStatus('Redirecting to login...');
        setLoading(true);
        try {
            await login();
        } catch (error) {
            setStatus(`Error: ${error.message}`);
            setLoading(false);
        }
    };

    return (
        <div className="mono-login-wrap">
            <div className="mono-login-card">
                <div className="text-center mb-8">
                    <div
                        className="mono-login-logo svg-mask mx-auto mb-4"
                        aria-hidden="true"
                        style={{
                            maskImage: 'url(/assets/logo.svg)',
                            WebkitMaskImage: 'url(/assets/logo.svg)',
                        }}
                    ></div>
                    <h1 className="mono-login-title">RISE</h1>
                    <p className="mono-login-subtitle">Container deployment platform</p>
                </div>

                {loading ? (
                    <div className="flex flex-col items-center gap-4 py-8">
                        <div className="w-12 h-12 border-2 border-gray-300 border-t-transparent rounded-full animate-spin"></div>
                        <p className="text-gray-300">{status}</p>
                    </div>
                ) : (
                    <>
                        <button onClick={handleLogin} className="mono-login-button">
                            login_with_oauth
                        </button>
                        {status && <p className="text-center text-sm text-red-300 mt-3">{status}</p>}
                    </>
                )}
            </div>
        </div>
    );
}

export function App() {
    const [user, setUser] = useState(null);
    const [authChecked, setAuthChecked] = useState(false);
    const [platformAccessDenied, setPlatformAccessDenied] = useState(false);
    const [version, setVersion] = useState(null);
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
                const message = errorDescription || `OAuth flow failed: ${error}`;
                showToast(message, 'error');
            } else if (accessToken) {
                const expiresIn = params.get('expires_in');
                const expiresAt = params.get('expires_at');

                let expiresAtDate;
                if (expiresAt) {
                    expiresAtDate = new Date(expiresAt);
                } else if (expiresIn) {
                    expiresAtDate = new Date(Date.now() + parseInt(expiresIn) * 1000);
                }

                const message = `OAuth flow successful! Token expires ${expiresAtDate ? expiresAtDate.toLocaleString() : 'soon'}`;
                showToast(message, 'success');
            }

            sessionStorage.removeItem('oauth_return_path');

            if (returnPath) {
                navigate(returnPath);
            } else {
                navigate('/projects');
            }
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
            const modalOpen = Boolean(document.querySelector('.modal-backdrop'));

            if (isTypingTarget && !modalOpen) return;

            e.preventDefault();
            setCommandPaletteOpen(true);
        };

        window.addEventListener('keydown', handler);
        return () => window.removeEventListener('keydown', handler);
    }, []);

    useEffect(() => {
        async function fetchVersion() {
            try {
                const response = await fetch(`${CONFIG.backendUrl}/api/v1/version`);
                const data = await response.json();
                setVersion(data);
            } catch (err) {
                console.error('Failed to fetch version:', err);
            }
        }
        fetchVersion();
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
                // Check if this is a platform access denial
                if (err.isPlatformAccessDenied) {
                    setPlatformAccessDenied(true);
                    return;
                }
                console.error('Failed to load command palette targets:', err);
            }
        }

        loadPaletteTargets();
    }, [user?.id]);

    const handleLogout = () => {
        logout();
    };

    if (!authChecked) {
        return (
            <div className="mono-login-wrap">
                <div className="w-8 h-8 border-2 border-gray-300 border-t-transparent rounded-full animate-spin"></div>
            </div>
        );
    }

    if (!user) {
        return <LoginPage />;
    }

    if (platformAccessDenied) {
        return <PlatformAccessDenied userEmail={user.email} onLogout={handleLogout} />;
    }

    let view = 'projects';
    let params = {};
    const route = pathname.replace(/^\//, '');

    if (route.startsWith('project/')) {
        const parts = route.split('/');
        if (parts[2] === 'environment' && parts[3]) {
            view = 'environment-deployment';
            params.projectName = parts[1];
            params.environmentName = parts[3];
        } else if (parts[2] === 'extensions') {
            if (parts.length === 3) {
                view = 'project-detail';
                params.projectName = parts[1];
                params.tab = 'extensions';
            } else if (parts.length === 4 && parts[3] === '@new') {
                view = 'extension-create';
                params.projectName = parts[1];
                params.extensionType = null;
            } else if (parts.length === 5 && parts[3]) {
                if (parts[4] === '@new') {
                    view = 'extension-detail';
                    params.projectName = parts[1];
                    params.extensionType = parts[3];
                    params.extensionInstance = null;
                } else {
                    view = 'extension-detail';
                    params.projectName = parts[1];
                    params.extensionType = parts[3];
                    params.extensionInstance = parts[4];
                }
            } else if (parts.length === 4 && parts[3]) {
                view = 'extension-detail';
                params.projectName = parts[1];
                params.extensionType = parts[3];
                params.extensionInstance = null;
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
    } else if (route === 'teams') {
        view = 'teams';
    } else {
        view = 'projects';
    }

    const projectTabLabelMap = {
        deployments: 'Deployments',
        environments: 'Environments',
        'service-accounts': 'Service Accounts',
        'env-vars': 'Environment Variables',
        domains: 'Domains',
        extensions: 'Extensions',
        access: 'Access',
    };
    const breadcrumbs =
        view === 'projects'
            ? [{ label: 'Projects' }]
            : view === 'teams'
            ? [{ label: 'Teams' }]
            : view === 'project-detail'
            ? [
                  { label: 'Projects', href: '/projects' },
                  { label: `Project: ${params.projectName}` },
                  ...(params.tab && params.tab !== 'overview'
                      ? [{ label: projectTabLabelMap[params.tab] || params.tab }]
                      : []),
              ]
            : view === 'team-detail'
            ? [
                  { label: 'Teams', href: '/teams' },
                  { label: `Team: ${params.teamName}` },
              ]
            : view === 'environment-deployment'
            ? [
                  { label: 'Projects', href: '/projects' },
                  { label: `Project: ${params.projectName}`, href: `/project/${params.projectName}` },
                  { label: 'Environments', href: `/project/${params.projectName}/environments` },
                  { label: `Environment: ${params.environmentName}` },
              ]
            : view === 'deployment-detail'
            ? [
                  { label: 'Projects', href: '/projects' },
                  { label: `Project: ${params.projectName}`, href: `/project/${params.projectName}` },
                  { label: `Deployment: ${params.deploymentId}` },
              ]
            : view === 'extension-detail' || view === 'extension-create'
            ? [
                  { label: 'Projects', href: '/projects' },
                  { label: `Project: ${params.projectName}`, href: `/project/${params.projectName}` },
                  { label: 'Extensions', href: `/project/${params.projectName}/extensions` },
                  {
                      label: params.extensionInstance
                          ? `Extension: ${params.extensionInstance}`
                          : params.extensionType
                          ? `Extension: ${params.extensionType}`
                          : 'Extension',
                  },
              ]
            : [{ label: 'Projects' }];

    const commandItems = [
        {
            id: 'go-projects',
            label: 'Navigate: Projects',
            keywords: ['projects', 'navigation', 'list'],
            run: () => navigate('/projects'),
        },
        {
            id: 'go-teams',
            label: 'Navigate: Teams',
            keywords: ['teams', 'navigation', 'list'],
            run: () => navigate('/teams'),
        },
        {
            id: 'create-project',
            label: 'Action: Create project',
            keywords: ['project', 'create', 'new'],
            run: () => navigate('/projects?create=project'),
        },
        {
            id: 'create-team',
            label: 'Action: Create team',
            keywords: ['team', 'create', 'new'],
            run: () => navigate('/teams?create=team'),
        },
        {
            id: 'open-docs',
            label: 'Open: Docs',
            keywords: ['help', 'docs', 'onboarding'],
            run: () => {
                window.location.href = '/docs/';
            },
        },
    ];

    if (view === 'project-detail' && params?.projectName) {
        commandItems.unshift({
            id: 'go-project-overview',
            label: `Navigate: Project ${params.projectName}`,
            keywords: ['project', 'detail', params.projectName],
            run: () => navigate(`/project/${params.projectName}`),
        });
    }

    const projectCommands = paletteProjects.map((project) => ({
        id: `project-${project.id || project.name}`,
        label: `Project: ${project.name}`,
        keywords: ['project', project.name, project.owner_team_name, project.owner_user_email].filter(Boolean),
        run: () => navigate(`/project/${project.name}`),
    }));

    const teamCommands = paletteTeams.map((team) => ({
        id: `team-${team.id || team.name}`,
        label: `Team: ${team.name}`,
        keywords: ['team', team.name],
        run: () => navigate(`/team/${team.name}`),
    }));

    commandItems.push(...projectCommands, ...teamCommands);

    const createIntent = new URLSearchParams(window.location.search).get('create');

    return (
        <div className="mono-app">
            <div className="mono-shell">
                <Sidebar currentView={view} user={user} onLogout={handleLogout} />
                <div className="mono-main-shell">
                    <TopBar breadcrumbs={breadcrumbs} />
                    <main className="mono-main">
                        {view === 'projects' && <ProjectsList openCreate={createIntent === 'project'} />}
                        {view === 'teams' && <TeamsList currentUser={user} openCreate={createIntent === 'team'} />}
                        {view === 'project-detail' && <ProjectDetail projectName={params.projectName} initialTab={params.tab} />}
                        {view === 'team-detail' && <TeamDetail teamName={params.teamName} currentUser={user} />}
                        {view === 'environment-deployment' && <EnvironmentDeploymentView projectName={params.projectName} environmentName={params.environmentName} />}
                        {view === 'deployment-detail' && <DeploymentDetail projectName={params.projectName} deploymentId={params.deploymentId} />}
                        {view === 'extension-detail' && <ExtensionDetailPage projectName={params.projectName} extensionType={params.extensionType} extensionInstance={params.extensionInstance} />}
                    </main>
                    <Footer version={version} />
                </div>
            </div>
            <CommandPalette
                isOpen={commandPaletteOpen}
                onClose={() => setCommandPaletteOpen(false)}
                items={commandItems}
            />
        </div>
    );
}
