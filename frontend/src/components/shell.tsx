import React, { useEffect, useState } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './icon';
import { Avatar, OwnerLabel, cx, colorFor, statusTone } from './r-ui';
import { navigate } from '../lib/navigation';
import { resolveTheme, usePrefs } from '../lib/prefs';
import { api } from '../lib/api';
import { useCurrentProject, type Project } from '../lib/project-context';
import { useRecentProjects } from '../lib/recent';
import { useIsMobile } from '../lib/use-media';
import {
    MOBILE_PRIMARY_SECTIONS,
    PROJECT_SECTIONS,
    projectPath,
    routeSection,
    type ProjectSection,
    type Route,
} from '../lib/routes';

export interface Crumb { label: string; href?: string; mono?: boolean }

export interface ShellProps {
    route: Route;
    pathname: string;
    breadcrumbs: Crumb[];
    user: { email?: string; id?: string } | null;
    onLogout: () => void;
    onOpenPalette: (scope?: 'projects') => void;
    /** Let the page own the viewport instead of scrolling inside the shell. */
    fullBleed?: boolean;
    children: React.ReactNode;
}

interface GlobalNavItem { id: 'home' | 'projects' | 'teams'; label: string; icon: string; href: string }

const GLOBAL_NAV: GlobalNavItem[] = [
    { id: 'home', label: 'Home', icon: 'home', href: '/home' },
    { id: 'projects', label: 'Projects', icon: 'cube', href: '/projects' },
    { id: 'teams', label: 'Teams', icon: 'users', href: '/teams' },
];

function globalActive(route: Route): GlobalNavItem['id'] | null {
    switch (route.view) {
        case 'home': return 'home';
        case 'projects': return 'projects';
        case 'teams':
        case 'team-detail': return 'teams';
        default: return null;
    }
}

// Count badges and statuses for the global sidebar. Best-effort: errors are
// logged and, on platform-access-denied, refetching stops so we don't hammer a
// backend that's already rejecting us (App.tsx renders the denied screen).
function useGlobalSummary() {
    const [projects, setProjects] = useState<Project[] | null>(null);
    const [teamCount, setTeamCount] = useState<number | undefined>();
    useEffect(() => {
        let cancelled = false;
        let denied = false;
        const onError = (what: string) => (err: { isPlatformAccessDenied?: boolean } | undefined) => {
            if (err?.isPlatformAccessDenied) { denied = true; return; }
            console.error(`Sidebar: failed to load ${what}`, err);
        };
        const load = () => {
            if (denied) return;
            api.getProjects()
                .then((p: unknown) => { if (!cancelled && Array.isArray(p)) setProjects(p as Project[]); })
                .catch(onError('projects'));
            api.getTeams()
                .then((t: unknown) => { if (!cancelled && Array.isArray(t)) setTeamCount(t.length); })
                .catch(onError('teams'));
        };
        load();
        window.addEventListener('rise:mutation', load);
        return () => {
            cancelled = true;
            window.removeEventListener('rise:mutation', load);
        };
    }, []);
    return { projects, teamCount };
}

function linkClick(href: string) {
    return (e: React.MouseEvent) => {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        e.preventDefault();
        navigate(href);
    };
}

export function Shell(props: ShellProps) {
    const isMobile = useIsMobile();
    const [prefs, setPrefs] = usePrefs();
    const theme = resolveTheme(prefs.theme);
    const toggleTheme = () => setPrefs({ theme: theme === 'dark' ? 'light' : 'dark' });
    const summary = useGlobalSummary();

    if (isMobile) {
        return <MobileShell {...props} theme={theme} onToggleTheme={toggleTheme} />;
    }
    return (
        <div className="r-app">
            <div className="r-layout">
                <Sidebar {...props} summary={summary} theme={theme} onToggleTheme={toggleTheme} />
                <main className="r-main">
                    <Topbar breadcrumbs={props.breadcrumbs} onOpenPalette={() => props.onOpenPalette()} />
                    <div className={props.fullBleed ? 'r-content is-full' : 'r-content'}>
                        {props.fullBleed ? props.children : <div className="r-page">{props.children}</div>}
                    </div>
                </main>
            </div>
        </div>
    );
}

type SidebarProps = ShellProps & {
    summary: ReturnType<typeof useGlobalSummary>;
    theme: 'light' | 'dark';
    onToggleTheme: () => void;
};

function Sidebar({ route, user, onLogout, onOpenPalette, summary, theme, onToggleTheme }: SidebarProps) {
    const current = useCurrentProject();
    const userEmail = user?.email || '';

    return (
        <aside className="r-side">
            <button type="button" className="r-brand" onClick={() => navigate('/home')} aria-label="Go to home">
                <div className="r-brand-mark">R</div>
                <div className="r-brand-name">Rise</div>
            </button>

            {current ? (
                <ProjectNav route={route} onOpenPalette={onOpenPalette} />
            ) : (
                <GlobalNav route={route} summary={summary} />
            )}

            <div className="r-side-footer">
                <a className="r-theme-btn" href="/docs/">
                    <Icon name="info" size={14} />
                    Docs
                </a>
                <button className="r-theme-btn" type="button" onClick={onToggleTheme}>
                    <Icon name={theme === 'dark' ? 'sun' : 'moon'} size={14} />
                    {theme === 'dark' ? 'Light mode' : 'Dark mode'}
                </button>
                <div className={cx('r-user', route.view === 'profile' && 'active')}>
                    <button type="button" className="r-user-main" onClick={() => navigate('/profile')} aria-label="Open profile">
                        <Avatar label={userEmail} color={colorFor(userEmail || 'user')} size={26} />
                        <div className="r-user-info">
                            <div className="r-user-name">{userEmail.split('@')[0] || 'You'}</div>
                            <div className="r-user-mail">{userEmail}</div>
                        </div>
                    </button>
                    <button type="button" className="r-user-logout" onClick={onLogout} title="Sign out" aria-label="Sign out">
                        <Icon name="logout" size={13} />
                    </button>
                </div>
            </div>
        </aside>
    );
}

function GlobalNav({ route, summary }: { route: Route; summary: SidebarProps['summary'] }) {
    const active = globalActive(route);
    const recent = useRecentProjects();
    const byName = new Map((summary.projects || []).map(p => [p.name, p]));
    const recentProjects = recent.filter(n => !summary.projects || byName.has(n));
    const countFor = (id: GlobalNavItem['id']) =>
        id === 'projects' ? summary.projects?.length : id === 'teams' ? summary.teamCount : undefined;

    return (
        <>
            <nav className="r-nav">
                {GLOBAL_NAV.map(item => {
                    const count = countFor(item.id);
                    return (
                        <a key={item.id} href={item.href} className={cx(active === item.id && 'active')} onClick={linkClick(item.href)}>
                            <Icon name={item.icon} className="ico" />
                            {item.label}
                            {count !== undefined && <span className="badge">{count}</span>}
                        </a>
                    );
                })}
            </nav>
            {recentProjects.length > 0 && (
                <div className="r-nav">
                    <div className="r-nav-section">Recent</div>
                    {recentProjects.map(name => {
                        const p = byName.get(name);
                        return (
                            <a key={name} href={projectPath(name)} onClick={linkClick(projectPath(name))}>
                                <span className={cx('r-dot', statusTone(p?.status))} />
                                <span className="r-nav-label">{name}</span>
                            </a>
                        );
                    })}
                </div>
            )}
        </>
    );
}

function ProjectNav({ route, onOpenPalette }: { route: Route; onOpenPalette: ShellProps['onOpenPalette'] }) {
    const current = useCurrentProject()!;
    const { projectName, project, counts } = current;
    const active = routeSection(route);

    return (
        <>
            <div className="r-proj-nav-head">
                <a className="r-back-link" href="/projects" onClick={linkClick('/projects')}>
                    <Icon name="chevl" size={13} />
                    All projects
                </a>
                <button type="button" className="r-proj-switch" onClick={() => onOpenPalette('projects')} aria-label="Switch project">
                    <span className={cx('r-dot', statusTone(project?.status))} />
                    <span className="r-proj-switch-text">
                        <span className="name">{projectName}</span>
                        <span className="sub">
                            {project?.owner && <OwnerLabel owner={project.owner} size={11} />}
                            {project?.status && <span>· {project.status}</span>}
                        </span>
                    </span>
                    <Icon name="updown" size={14} />
                </button>
            </div>
            <nav className="r-nav">
                {PROJECT_SECTIONS.map(s => {
                    const href = projectPath(projectName, s.id);
                    const count = s.id === 'overview' || s.id === 'access' || s.id === 'logs' ? undefined : counts[s.id];
                    return (
                        <a key={s.id} href={href} className={cx(active === s.id && 'active')} onClick={linkClick(href)}>
                            <Icon name={s.icon} className="ico" />
                            {s.label}
                            {count !== undefined && <span className="badge">{count}</span>}
                        </a>
                    );
                })}
            </nav>
        </>
    );
}

function Topbar({ breadcrumbs, onOpenPalette }: { breadcrumbs: Crumb[]; onOpenPalette: () => void }) {
    return (
        <header className="r-topbar">
            <nav className="r-crumbs" aria-label="Breadcrumb">
                {breadcrumbs.map((c, i) => {
                    const isLast = i === breadcrumbs.length - 1;
                    return (
                        <React.Fragment key={`${c.label}-${i}`}>
                            {i > 0 && <span className="sep">/</span>}
                            {c.href && !isLast ? (
                                <a href={c.href} className={cx(c.mono && 'mono')} onClick={linkClick(c.href)}>{c.label}</a>
                            ) : (
                                <strong className={cx(c.mono && 'mono')} aria-current="page">{c.label}</strong>
                            )}
                        </React.Fragment>
                    );
                })}
            </nav>
            <div className="right">
                <button className="r-kbar" type="button" onClick={onOpenPalette}>
                    <Icon name="search" size={13} />
                    <span className="placeholder">Search or run a command…</span>
                    <kbd>⌘K</kbd>
                </button>
            </div>
        </header>
    );
}

// ---------- Mobile ----------

function MobileShell({ route, pathname, breadcrumbs, onOpenPalette, fullBleed, children, theme, onToggleTheme }: ShellProps & { theme: 'light' | 'dark'; onToggleTheme: () => void }) {
    const current = useCurrentProject();
    const [moreOpen, setMoreOpen] = useState(false);
    useEffect(() => { setMoreOpen(false); }, [pathname]);

    // The parent is the nearest linked crumb above the current page.
    const parent = [...breadcrumbs.slice(0, -1)].reverse().find(c => c.href);
    const last = breadcrumbs[breadcrumbs.length - 1];
    const section = routeSection(route);
    const sectionInfo = PROJECT_SECTIONS.find(s => s.id === section);

    let title: React.ReactNode = last?.label;
    let sub: React.ReactNode = null;
    if (current) {
        title = (
            <span className="r-mhead-proj">
                <span className={cx('r-dot', statusTone(current.project?.status))} />
                {current.projectName}
            </span>
        );
        if (route.view === 'deployment-detail' || route.view === 'deployment-logs') sub = <span className="mono">{route.deploymentId}</span>;
        else if (route.view === 'project' && route.section === 'overview') sub = null;
        else sub = last?.label !== current.projectName ? last?.label : sectionInfo?.label;
    }

    return (
        <div className="r-app r-app-mobile">
            <header className="r-mhead">
                {parent?.href ? (
                    <button type="button" className="r-tap-btn" onClick={() => navigate(parent.href!)} aria-label={`Back to ${parent.label}`}>
                        <Icon name="chevl" size={20} />
                    </button>
                ) : (
                    <button type="button" className="r-tap-btn" onClick={() => navigate('/home')} aria-label="Go to home">
                        <span className="r-brand-mark">R</span>
                    </button>
                )}
                <div className="r-mhead-title">
                    <div className="t">{title}</div>
                    {sub && <div className="s">{sub}</div>}
                </div>
                <button type="button" className="r-tap-btn" onClick={onToggleTheme} aria-label={theme === 'dark' ? 'Light mode' : 'Dark mode'}>
                    <Icon name={theme === 'dark' ? 'sun' : 'moon'} size={19} />
                </button>
                <button type="button" className="r-tap-btn" onClick={() => onOpenPalette()} aria-label="Search">
                    <Icon name="search" size={19} />
                </button>
            </header>
            <main className={fullBleed ? 'r-content is-full' : 'r-content'}>
                {fullBleed ? children : <div className="r-page">{children}</div>}
            </main>
            <nav className="r-tabbar" aria-label="Primary">
                {current ? (
                    <>
                        {MOBILE_PRIMARY_SECTIONS.map(id => {
                            const s = PROJECT_SECTIONS.find(x => x.id === id)!;
                            return (
                                <TabButton key={id} icon={s.icon} label={s.short} active={section === id}
                                    onClick={() => navigate(projectPath(current.projectName, id))} />
                            );
                        })}
                        <TabButton icon="menu" label="More" active={moreOpen || (!!section && !MOBILE_PRIMARY_SECTIONS.includes(section))}
                            onClick={() => setMoreOpen(true)} />
                    </>
                ) : (
                    <>
                        <TabButton icon="home" label="Home" active={route.view === 'home'} onClick={() => navigate('/home')} />
                        <TabButton icon="cube" label="Projects" active={route.view === 'projects'} onClick={() => navigate('/projects')} />
                        <TabButton icon="search" label="Search" onClick={() => onOpenPalette()} />
                        <TabButton icon="users" label="Teams" active={route.view === 'teams' || route.view === 'team-detail'} onClick={() => navigate('/teams')} />
                    </>
                )}
            </nav>
            {current && moreOpen && (
                <MoreSheet
                    projectName={current.projectName}
                    active={section}
                    onClose={() => setMoreOpen(false)}
                />
            )}
        </div>
    );
}

function TabButton({ icon, label, active, onClick }: { icon: string; label: string; active?: boolean; onClick: () => void }) {
    return (
        <button type="button" className={cx('r-tab-btn', active && 'active')} onClick={onClick} aria-current={active ? 'page' : undefined}>
            <Icon name={icon} size={21} />
            <span>{label}</span>
        </button>
    );
}

function MoreSheet({ projectName, active, onClose }: { projectName: string; active: ProjectSection | null; onClose: () => void }) {
    const rest = PROJECT_SECTIONS.filter(s => !MOBILE_PRIMARY_SECTIONS.includes(s.id));
    useEffect(() => {
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
        window.addEventListener('keydown', onKey);
        return () => window.removeEventListener('keydown', onKey);
    }, [onClose]);
    return createPortal(
        <div className="r-modal-mask r-sheet-mask" onClick={onClose}>
            <div className="r-sheet" role="dialog" aria-modal="true" aria-label="More sections" onClick={e => e.stopPropagation()}>
                <div className="r-sheet-handle" />
                {rest.map(s => (
                    <button key={s.id} type="button" className={cx('r-sheet-item', active === s.id && 'active')}
                        onClick={() => { onClose(); navigate(projectPath(projectName, s.id)); }}>
                        <Icon name={s.icon} size={18} />
                        <span className="grow">{s.label}</span>
                        <Icon name="chev" size={16} />
                    </button>
                ))}
                <button type="button" className="r-sheet-item" onClick={() => { onClose(); navigate('/profile'); }}>
                    <Icon name="user" size={18} />
                    <span className="grow">Profile</span>
                    <Icon name="chev" size={16} />
                </button>
            </div>
        </div>,
        document.body,
    );
}
