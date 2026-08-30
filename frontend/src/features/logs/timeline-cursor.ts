/**
 * Where the reader currently is in the log buffer, published to the volume
 * rail so the chart can show which slice of the timeline is on screen.
 *
 * This is deliberately not React state. Scrolling and hovering update it many
 * times a second, and the console holds a virtualized list plus a Recharts
 * chart — routing every frame through a parent re-render would reconcile both
 * on each tick. The stream writes to the store, the chart's cursor overlay is
 * the only subscriber, and everything else re-renders as before.
 */

export interface TimelineCursor {
    /** Time span covered by the rows currently on screen. */
    startMs: number;
    endMs: number;
    /** Timestamp of the row under the pointer, or `null` when none is hovered. */
    hoverMs: number | null;
}

export interface TimelineCursorStore {
    get: () => TimelineCursor | null;
    set: (next: TimelineCursor | null) => void;
    subscribe: (listener: () => void) => () => void;
}

function same(a: TimelineCursor | null, b: TimelineCursor | null): boolean {
    if (a === b) return true;
    if (!a || !b) return false;
    return a.startMs === b.startMs && a.endMs === b.endMs && a.hoverMs === b.hoverMs;
}

export function createTimelineCursorStore(): TimelineCursorStore {
    let value: TimelineCursor | null = null;
    const listeners = new Set<() => void>();
    return {
        get: () => value,
        set(next) {
            // `useSyncExternalStore` compares snapshots by identity, so an
            // unchanged position must keep the previous object — otherwise
            // every scroll frame re-renders the overlay with the same numbers.
            if (same(value, next)) return;
            value = next;
            for (const listener of listeners) listener();
        },
        subscribe(listener) {
            listeners.add(listener);
            return () => listeners.delete(listener);
        },
    };
}
