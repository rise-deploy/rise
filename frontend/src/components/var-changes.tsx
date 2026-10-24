import { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { cx } from './r-ui';

// What a new deployment's variables change against the deployment it is made
// from. The server compares the source's snapshot with what the target
// environment would get now, secrets included; values come back only for
// plain variables.

interface DiffEntry {
    key: string;
    change: 'added' | 'removed' | 'changed' | 'unchanged' | 'unknown';
    is_secret: boolean;
    is_protected: boolean;
    from?: string;
    to?: string;
    from_source?: string;
    to_source?: string;
}

/** `global` → Global, `env:staging` → staging, `toml` → rise.toml, `cli` → a CLI flag. */
function sourceLabel(source?: string): string {
    if (!source || source === 'global') return 'Global';
    if (source.startsWith('env:')) return source.slice(4);
    return source === 'toml' ? 'rise.toml' : source === 'cli' ? 'a CLI flag' : source;
}

const ORDER = { changed: 0, added: 1, removed: 2, unknown: 3, unchanged: 4 };

/** The variable changes a deployment made from `sourceId` into `environment` would bring. */
export function VarChanges({ projectName, sourceId, environment }: {
    projectName: string;
    sourceId: string;
    environment: string | null;
}) {
    const [entries, setEntries] = useState<DiffEntry[] | null>(null);
    const [failed, setFailed] = useState(false);

    useEffect(() => {
        let cancelled = false;
        setEntries(null);
        setFailed(false);
        api.diffDeploymentEnvVars(projectName, sourceId, environment)
            .then((res: { changes?: DiffEntry[] }) => { if (!cancelled) setEntries(res?.changes ?? []); })
            .catch(() => { if (!cancelled) setFailed(true); });
        return () => { cancelled = true; };
    }, [projectName, sourceId, environment]);

    if (failed) return <p className="r-var-changes-note">Could not compare variables with this deployment's snapshot.</p>;
    if (!entries) return <p className="r-var-changes-note">Comparing variables…</p>;
    const changes = entries.filter(e => e.change !== 'unchanged').sort((a, b) => ORDER[a.change] - ORDER[b.change] || a.key.localeCompare(b.key));
    const unchanged = entries.length - changes.length;
    if (changes.length === 0) {
        return <p className="r-var-changes-note">No variable changes against this deployment's snapshot{unchanged ? ` (${unchanged} compared)` : ''}.</p>;
    }

    return (
        <div className="r-var-changes">
            <div className="r-section-label">Variable changes</div>
            <ul>
                {changes.map(c => (
                    <li key={c.key} className={cx('r-var-change', c.change)}>
                        <span className="mark" aria-hidden>{c.change === 'added' ? '+' : c.change === 'removed' ? '−' : c.change === 'unknown' ? '?' : '~'}</span>
                        <span className="mono key">{c.key}</span>
                        <span className="what">
                            {c.change === 'added' && <>added{c.to !== undefined && <> · <span className="mono">{c.to}</span></>}</>}
                            {c.change === 'removed' && <>removed</>}
                            {c.change === 'changed' && (c.from !== undefined && c.to !== undefined
                                ? <><span className="mono">{c.from}</span> → <span className="mono">{c.to}</span></>
                                : c.from_source !== c.to_source
                                    ? <>now set in {sourceLabel(c.to_source)} (was {sourceLabel(c.from_source)})</>
                                    : <>{c.is_protected ? 'protected' : 'secret'} value changed</>)}
                            {c.change === 'unknown' && <>{c.is_protected ? 'protected' : 'secret'}; you can't read it, so it isn't compared</>}
                        </span>
                    </li>
                ))}
            </ul>
            {unchanged > 0 && <p className="r-var-changes-note">{unchanged} other variable{unchanged === 1 ? '' : 's'} unchanged.</p>}
        </div>
    );
}
