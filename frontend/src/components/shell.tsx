import React, { useEffect, useState } from 'react';
import { Icon } from './icon';
import { Avatar, cx, colorFor } from './r-ui';
import { navigate } from '../lib/navigation';
import { usePrefs } from '../lib/prefs';

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

    return (
        <div className="r-app">
            <div className="r-layout">
                <Sidebar route={route} user={user} onLogout={onLogout} onToggleTheme={() => setPrefs({ theme: prefs.theme === 'dark' ? 'light' : 'dark' })} theme={prefs.theme} />
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
    return (
        <aside className="r-side">
            <div className="r-brand" onClick={() => navigate('/home')} style={{ cursor: 'pointer' }}>
                <div className="r-brand-mark">R</div>
                <div className="r-brand-name">Rise</div>
            </div>
            <nav className="r-nav">
                {PRIMARY.map(item => (
                    <button
                        key={item.id}
                        className={cx(item.matches(route) && 'active')}
                        onClick={() => navigate(item.href)}
                        type="button"
                    >
                        <Icon name={item.icon} className="ico" />
                        {item.label}
                    </button>
                ))}
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
                <div
                    className="r-user"
                    role="button"
                    tabIndex={0}
                    onClick={() => navigate('/profile')}
                    onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); navigate('/profile'); } }}
                    aria-label="Open profile"
                >
                    <Avatar label={userEmail} color={userColor} size={26} />
                    <div className="r-user-info">
                        <div className="r-user-name">{userEmail.split('@')[0] || 'You'}</div>
                        <div className="r-user-mail">{userEmail}</div>
                    </div>
                    <button
                        type="button"
                        className="r-icon-btn"
                        style={{ width: 26, height: 26, border: 'none', background: 'transparent', flex: '0 0 26px' }}
                        onClick={(e) => { e.stopPropagation(); onLogout(); }}
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
                                <a onClick={() => navigate(c.href!)}>{c.label}</a>
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
