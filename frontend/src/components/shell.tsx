import React, { useEffect, useState } from 'react';
import { Icon } from './icon';
import { Avatar, cx, colorFor } from './r-ui';
import { navigate } from '../lib/navigation';
import { resolveTheme, usePrefs } from '../lib/prefs';
import { api } from '../lib/api';

interface NavItem { id: string; label: string; icon: string; href: string; matches: (route: string) => boolean }

const PRIMARY: NavItem[] = [
    { id: 'home',     label: 'Home',     icon: 'home',     href: '/home',     matches: r => r === '/home' || r === '/' },
    { id: 'projects', label: 'Projects', icon: 'cube',     href: '/projects', matches: r => r === '/projects' || r.startsWith('/project/') || r.startsWith('/deployment/') },
    { id: 'teams',    label: 'Teams',    icon: 'users',    href: '/teams',    matches: r => r === '/teams' || r.startsWith('/team/') },
];

export interface ShellProps {
    route: string;
    breadcrumbs: { label: string; href?: string }[];
    user: { email?: string; id?: string } | null;
    onLogout: () => void;
    onOpenPalette: () => void;
    children: React.ReactNode;
}

export function Shell({ route, breadcrumbs, user, onLogout, onOpenPalette, children }: ShellProps) {
    const [prefs, setPrefs] = usePrefs();
    const effectiveTheme = resolveTheme(prefs.theme);

    return (
        <div className="r-app">
            <div className="r-layout">
                <Sidebar
                    route={route}
                    user={user}
                    onLogout={onLogout}
                    onToggleTheme={() => setPrefs({ theme: effectiveTheme === 'dark' ? 'light' : 'dark' })}
                    theme={effectiveTheme}
                />
                <main className="r-main">
                    <Topbar breadcrumbs={breadcrumbs} onOpenPalette={onOpenPalette} />
                    <div className="r-content">{children}</div>
                </main>
            </div>
        </div>
    );
}

function Sidebar({ route, user, onLogout, onToggleTheme, theme }: { route: string; user: ShellProps['user']; onLogout: () => void; onToggleTheme: () => void; theme: 'light' | 'dark' }) {
    const userEmail = user?.email || '';
    const userColor = colorFor(userEmail || 'user');
    const [navCounts, setNavCounts] = useState<{ projects?: number; teams?: number }>({});

    // Fetch nav badge counts (projects / teams). Best-effort; failures are ignored.
    // Re-fetches on the global `rise:mutation` event so that creates/deletes
    // elsewhere in the app keep the sidebar counts in sync.
    useEffect(() => {
        let cancelled = false;
        const loadCounts = () => {
            api.getProjects()
                .then((p: any[]) => { if (!cancelled) setNavCounts(c => ({ ...c, projects: Array.isArray(p) ? p.length : undefined })); })
                .catch(() => {});
            api.getTeams()
                .then((t: any[]) => { if (!cancelled) setNavCounts(c => ({ ...c, teams: Array.isArray(t) ? t.length : undefined })); })
                .catch(() => {});
        };
        loadCounts();
        window.addEventListener('rise:mutation', loadCounts);
        return () => {
            cancelled = true;
            window.removeEventListener('rise:mutation', loadCounts);
        };
    }, []);

    const countFor = (id: string): number | undefined => {
        if (id === 'projects') return navCounts.projects;
        if (id === 'teams') return navCounts.teams;
        return undefined;
    };

    return (
        <aside className="r-side">
            <div className="r-brand" onClick={() => navigate('/home')} style={{ cursor: 'pointer' }}>
                <div className="r-brand-mark">R</div>
                <div className="r-brand-name">Rise</div>
            </div>
            {/*
              Non-functional placeholder for future multi-tenancy / workspace
              switching. Renders a static workspace label only — there is no
              dropdown or menu behaviour yet.
            */}
            <div className="r-org" role="presentation">
                <span className="r-org-avatar">W</span>
                <span className="r-org-name">Workspace</span>
                <Icon name="chevd" size={14} />
            </div>
            <nav className="r-nav">
                {PRIMARY.map(item => {
                    const count = countFor(item.id);
                    return (
                        <button
                            key={item.id}
                            className={cx(item.matches(route) && 'active')}
                            onClick={() => navigate(item.href)}
                            type="button"
                        >
                            <Icon name={item.icon} className="ico" />
                            {item.label}
                            {count !== undefined && <span className="badge">{count}</span>}
                        </button>
                    );
                })}
            </nav>
            <div className="r-side-footer">
                <a className="r-theme-btn" href="/docs/" style={{ textDecoration: 'none' }}>
                    <Icon name="info" size={14} />
                    Docs
                </a>
                <button className="r-theme-btn" type="button" onClick={onToggleTheme}>
                    <Icon name={theme === 'dark' ? 'sun' : 'moon'} size={14} />
                    {theme === 'dark' ? 'Light mode' : 'Dark mode'}
                </button>
                <div className={cx('r-user', route === '/profile' && 'active')}>
                    <button
                        type="button"
                        className="r-user-main"
                        onClick={() => navigate('/profile')}
                        aria-label="Open profile"
                        style={{
                            display: 'flex',
                            alignItems: 'center',
                            gap: 10,
                            flex: 1,
                            minWidth: 0,
                            background: 'transparent',
                            border: 'none',
                            padding: 0,
                            cursor: 'pointer',
                            color: 'inherit',
                            textAlign: 'left',
                            font: 'inherit',
                        }}
                    >
                        <Avatar label={userEmail} color={userColor} size={26} />
                        <div className="r-user-info">
                            <div className="r-user-name">{userEmail.split('@')[0] || 'You'}</div>
                            <div className="r-user-mail">{userEmail}</div>
                        </div>
                    </button>
                    <button
                        type="button"
                        className="r-icon-btn"
                        style={{ width: 26, height: 26, border: 'none', background: 'transparent', flex: '0 0 26px' }}
                        onClick={onLogout}
                        title="Logout"
                        aria-label="Logout"
                    >
                        <Icon name="logout" size={13} />
                    </button>
                </div>
            </div>
        </aside>
    );
}

function Topbar({ breadcrumbs, onOpenPalette }: { breadcrumbs: { label: string; href?: string }[]; onOpenPalette: () => void }) {
    return (
        <header className="r-topbar">
            <div className="r-crumbs">
                {breadcrumbs.map((c, i) => {
                    const isLast = i === breadcrumbs.length - 1;
                    return (
                        <React.Fragment key={`${c.label}-${i}`}>
                            {i > 0 && <span className="sep">/</span>}
                            {c.href && !isLast ? (
                                <a
                                    href={c.href}
                                    onClick={(e) => {
                                        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
                                        e.preventDefault();
                                        navigate(c.href!);
                                    }}
                                >
                                    {c.label}
                                </a>
                            ) : (
                                <strong>{c.label}</strong>
                            )}
                        </React.Fragment>
                    );
                })}
            </div>
            <div className="right">
                <button className="r-kbar" type="button" onClick={onOpenPalette}>
                    <Icon name="search" size={13} />
                    <span className="placeholder">Search projects, teams, deployments…</span>
                    <kbd>⌘K</kbd>
                </button>
            </div>
        </header>
    );
}
