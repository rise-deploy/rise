import { useEffect, useState } from 'react';

function toPath(input: string): string {
  const value = input.trim();
  const withoutHash = value.startsWith('#') ? value.slice(1) : value;
  const withSlash = withoutHash.startsWith('/') ? withoutHash : `/${withoutHash}`;
  const cleaned = withSlash.replace(/\/+$/, '');
  return cleaned.length === 0 ? '/projects' : cleaned;
}

export function usePathLocation(): string {
  const [path, setPath] = useState(() => toPath(window.location.pathname));

  useEffect(() => {
    const normalized = toPath(window.location.pathname);
    if (normalized !== window.location.pathname) {
      window.history.replaceState({}, '', normalized + window.location.search + window.location.hash);
    }

    const onChange = () => {
      const next = toPath(window.location.pathname);
      if (next !== window.location.pathname) {
        window.history.replaceState({}, '', next + window.location.search + window.location.hash);
      }
      setPath(next);
    };
    window.addEventListener('popstate', onChange);
    window.addEventListener('rise:navigate', onChange as EventListener);
    return () => {
      window.removeEventListener('popstate', onChange);
      window.removeEventListener('rise:navigate', onChange as EventListener);
    };
  }, []);

  return path;
}

export function navigate(route: string): void {
  const target = toPath(route);
  if (target === toPath(window.location.pathname)) {
    return;
  }
  window.history.pushState({}, '', target);
  window.dispatchEvent(new Event('rise:navigate'));
}

export function maybeMigrateLegacyHashRoute(): void {
  const hash = window.location.hash?.replace(/^#/, '').trim();
  if (!hash) return;

  window.history.replaceState({}, '', toPath(hash));
  window.dispatchEvent(new Event('rise:navigate'));
}

/**
 * Read and write a single URL search parameter.
 *
 * View state that survives a reload and a paste — which tab is open, which time
 * window the logs are showing — belongs in the URL: it is what makes "look at
 * this" a link rather than a set of instructions.
 *
 * `history` decides whether a change is something the Back button should undo.
 * Discrete choices a reader makes deliberately (a tab) push, so Back returns to
 * where they were. Continuous ones (dragging a time range) replace, so a single
 * adjustment does not leave a dozen entries to walk back through.
 *
 * Passing `null` removes the parameter, keeping a default state's URL clean.
 */
export function useQueryParam(
    key: string,
    { history = 'replace' }: { history?: 'push' | 'replace' } = {},
): [string | null, (value: string | null) => void] {
    const read = () => new URLSearchParams(window.location.search).get(key);
    const [value, setValue] = useState<string | null>(read);

    useEffect(() => {
        const onChange = () => setValue(read());
        window.addEventListener('popstate', onChange);
        window.addEventListener('rise:navigate', onChange as EventListener);
        return () => {
            window.removeEventListener('popstate', onChange);
            window.removeEventListener('rise:navigate', onChange as EventListener);
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps -- `read` closes over `key` only
    }, [key]);

    const write = (next: string | null) => {
        const normalized = next === '' ? null : next;
        // Re-selecting what is already selected is not a navigation, and pushing
        // it would make Back appear to do nothing.
        if (normalized === read()) return;

        const params = new URLSearchParams(window.location.search);
        if (normalized === null) {
            params.delete(key);
        } else {
            params.set(key, normalized);
        }
        const query = params.toString();
        const url = window.location.pathname + (query ? `?${query}` : '') + window.location.hash;

        if (history === 'push') window.history.pushState({}, '', url);
        else window.history.replaceState({}, '', url);
        setValue(normalized);
    };

    return [value, write];
}
