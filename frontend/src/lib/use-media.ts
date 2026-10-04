import { useEffect, useState } from 'react';

/** The single layout breakpoint: below it the app uses the mobile shell. */
export const MOBILE_QUERY = '(max-width: 767px)';

export function useMediaQuery(query: string): boolean {
    const get = () => typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia(query).matches;
    const [matches, setMatches] = useState(get);
    useEffect(() => {
        if (typeof window.matchMedia !== 'function') return;
        const mql = window.matchMedia(query);
        const onChange = () => setMatches(mql.matches);
        onChange();
        mql.addEventListener('change', onChange);
        return () => mql.removeEventListener('change', onChange);
    }, [query]);
    return matches;
}

export function useIsMobile(): boolean {
    return useMediaQuery(MOBILE_QUERY);
}
