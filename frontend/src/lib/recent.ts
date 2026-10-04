import { useEffect, useState } from 'react';

// Recently opened projects, newest first. Per-browser convenience only: the
// sidebar and Home read it to offer one-click returns, so losing it (private
// window, cleared storage) just empties those lists.
const STORAGE_KEY = 'rise.recentProjects.v1';
const MAX_RECENT = 4;
const CHANGE_EVENT = 'rise:recent-change';

function read(): string[] {
    try {
        const parsed = JSON.parse(localStorage.getItem(STORAGE_KEY) || '[]');
        return Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : [];
    } catch {
        return [];
    }
}

export function rememberProject(name: string): void {
    const next = [name, ...read().filter(n => n !== name)].slice(0, MAX_RECENT);
    try { localStorage.setItem(STORAGE_KEY, JSON.stringify(next)); } catch { /* ignore */ }
    window.dispatchEvent(new Event(CHANGE_EVENT));
}

export function useRecentProjects(): string[] {
    const [recent, setRecent] = useState<string[]>(read);
    useEffect(() => {
        const onChange = () => setRecent(read());
        window.addEventListener(CHANGE_EVENT, onChange);
        window.addEventListener('storage', onChange);
        return () => {
            window.removeEventListener(CHANGE_EVENT, onChange);
            window.removeEventListener('storage', onChange);
        };
    }, []);
    return recent;
}
