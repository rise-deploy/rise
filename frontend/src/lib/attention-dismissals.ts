import { useEffect, useState } from 'react';

const CHANGE_EVENT = 'rise:attention-dismissed';
const MAX_DISMISSALS = 200;

function read(key: string): string[] {
    try {
        const value: unknown = JSON.parse(localStorage.getItem(key) || '[]');
        return Array.isArray(value) ? value.filter((v): v is string => typeof v === 'string').slice(0, MAX_DISMISSALS) : [];
    } catch {
        return [];
    }
}

/** Dismissals belong to one user in this browser and one deployment/status. */
export function useAttentionDismissals(userId: string) {
    const key = `rise.attentionDismissals.v1.${userId}`;
    const [state, setState] = useState(() => ({ key, ids: read(key) }));
    const ids = state.key === key ? state.ids : read(key);

    useEffect(() => {
        const update = () => setState({ key, ids: read(key) });
        update();
        window.addEventListener('storage', update);
        window.addEventListener(CHANGE_EVENT, update);
        return () => {
            window.removeEventListener('storage', update);
            window.removeEventListener(CHANGE_EVENT, update);
        };
    }, [key]);

    const dismiss = (id: string) => {
        const next = [id, ...ids.filter(v => v !== id)].slice(0, MAX_DISMISSALS);
        try { localStorage.setItem(key, JSON.stringify(next)); } catch { /* Works in memory when storage is unavailable. */ }
        window.dispatchEvent(new Event(CHANGE_EVENT));
        setState({ key, ids: next });
    };

    const restore = () => {
        try { localStorage.removeItem(key); } catch { /* Works in memory when storage is unavailable. */ }
        window.dispatchEvent(new Event(CHANGE_EVENT));
        setState({ key, ids: [] });
    };

    return { dismissed: new Set(ids), dismiss, restore };
}
