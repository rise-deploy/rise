import { useEffect, useRef } from 'react';

/**
 * Calls `callback` immediately, then again every `intervalMs` while the document
 * is visible. Pauses when the tab is hidden, and re-fires immediately when it
 * becomes visible again so users don't stare at stale data for a whole interval.
 *
 * Pass `enabled = false` (or `intervalMs <= 0`) to disable polling without
 * changing the call site shape.
 */
export function usePolling(
    callback: () => void,
    intervalMs: number,
    enabled: boolean = true,
): void {
    // Keep the latest callback in a ref so we don't re-create the timer on
    // every render just because `callback` changed identity.
    const callbackRef = useRef(callback);
    useEffect(() => {
        callbackRef.current = callback;
    }, [callback]);

    useEffect(() => {
        if (!enabled || intervalMs <= 0) return;

        let timer: ReturnType<typeof setInterval> | null = null;

        const tick = () => callbackRef.current();

        const start = () => {
            if (timer !== null) return;
            timer = setInterval(tick, intervalMs);
        };

        const stop = () => {
            if (timer === null) return;
            clearInterval(timer);
            timer = null;
        };

        const onVisibilityChange = () => {
            if (document.visibilityState === 'hidden') {
                stop();
            } else {
                // Re-fetch immediately so the user sees fresh data, then resume.
                tick();
                start();
            }
        };

        // Initial fetch + schedule (only if currently visible).
        tick();
        if (document.visibilityState !== 'hidden') {
            start();
        }

        document.addEventListener('visibilitychange', onVisibilityChange);
        return () => {
            document.removeEventListener('visibilitychange', onVisibilityChange);
            stop();
        };
    }, [intervalMs, enabled]);
}
