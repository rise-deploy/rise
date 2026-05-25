import { useCallback, useEffect, useState } from 'react';

export type Palette = 'mint' | 'indigo' | 'ember' | 'slate';
export type Density = 'compact' | 'cozy';
export type Theme = 'system' | 'light' | 'dark';

export interface Prefs {
    palette: Palette;
    density: Density;
    theme: Theme;
}

export const PALETTES: Palette[] = ['mint', 'indigo', 'ember', 'slate'];
export const DENSITIES: Density[] = ['compact', 'cozy'];
export const THEMES: Theme[] = ['system', 'light', 'dark'];

export const DEFAULT_PREFS: Prefs = {
    palette: 'indigo',
    density: 'cozy',
    theme: 'system',
};

const STORAGE_KEY = 'rise.prefs.v1';

function readStored(): Partial<Prefs> {
    try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (!raw) return {};
        const parsed = JSON.parse(raw);
        return (parsed && typeof parsed === 'object') ? parsed : {};
    } catch {
        return {};
    }
}

function sanitize(stored: Partial<Prefs>): Prefs {
    const palette = PALETTES.includes(stored.palette as Palette) ? (stored.palette as Palette) : DEFAULT_PREFS.palette;
    const density = DENSITIES.includes(stored.density as Density) ? (stored.density as Density) : DEFAULT_PREFS.density;
    const theme = THEMES.includes(stored.theme as Theme) ? (stored.theme as Theme) : DEFAULT_PREFS.theme;
    return { palette, density, theme };
}

export function loadPrefs(): Prefs {
    return sanitize(readStored());
}

function persist(prefs: Prefs) {
    try { localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs)); } catch { /* ignore */ }
}

/** Resolve `system` to the OS's current preference. SSR-safe. */
export function resolveTheme(theme: Theme): 'light' | 'dark' {
    if (theme === 'system') {
        if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
            return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
        }
        return 'light';
    }
    return theme;
}

/** Apply prefs to the document root so descendant CSS picks them up. */
export function applyPrefs(prefs: Prefs) {
    const root = document.documentElement;
    root.dataset.palette = prefs.palette;
    root.dataset.density = prefs.density;
    if (resolveTheme(prefs.theme) === 'dark') {
        root.classList.add('dark');
    } else {
        root.classList.remove('dark');
    }
}

const PREFS_EVENT = 'rise:prefs-change';

export function usePrefs(): [Prefs, (next: Partial<Prefs>) => void] {
    const [prefs, setPrefsState] = useState<Prefs>(() => loadPrefs());

    useEffect(() => {
        applyPrefs(prefs);
    }, [prefs.palette, prefs.density, prefs.theme]);

    // Follow OS color-scheme changes while theme is `system`.
    useEffect(() => {
        if (prefs.theme !== 'system') return;
        if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
        const mq = window.matchMedia('(prefers-color-scheme: dark)');
        const onMqChange = () => applyPrefs(prefs);
        mq.addEventListener('change', onMqChange);
        return () => mq.removeEventListener('change', onMqChange);
    }, [prefs.theme]);

    useEffect(() => {
        const onChange = (e: Event) => {
            const detail = (e as CustomEvent<Prefs>).detail;
            if (detail) setPrefsState(detail);
        };
        const onStorage = (e: StorageEvent) => {
            if (e.key === STORAGE_KEY) setPrefsState(loadPrefs());
        };
        window.addEventListener(PREFS_EVENT, onChange as EventListener);
        window.addEventListener('storage', onStorage);
        return () => {
            window.removeEventListener(PREFS_EVENT, onChange as EventListener);
            window.removeEventListener('storage', onStorage);
        };
    }, []);

    const setPrefs = useCallback((next: Partial<Prefs>) => {
        setPrefsState(prev => {
            const merged = sanitize({ ...prev, ...next });
            persist(merged);
            window.dispatchEvent(new CustomEvent(PREFS_EVENT, { detail: merged }));
            return merged;
        });
    }, []);

    return [prefs, setPrefs];
}
