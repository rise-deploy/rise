import { useEffect, useState } from 'react';

function toPath(input: string): string {
  const value = input.trim();
  const withoutHash = value.startsWith('#') ? value.slice(1) : value;
  const withSlash = withoutHash.startsWith('/') ? withoutHash : `/${withoutHash}`;
  const cleaned = withSlash.replace(/\/+$/, '');
  return cleaned.length === 0 ? '/projects' : cleaned;
}

export function usePathLocation(): string {
  const resolved = toPath(window.location.pathname);
  if (resolved !== window.location.pathname) {
    window.history.replaceState({}, '', resolved);
  }
  const [path, setPath] = useState(resolved);

  useEffect(() => {
    const onChange = () => {
      const next = toPath(window.location.pathname);
      if (next !== window.location.pathname) {
        window.history.replaceState({}, '', next);
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
