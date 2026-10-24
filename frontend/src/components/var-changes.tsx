import { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { cx } from './r-ui';

// What a new deployment's variables change against the deployment it is made
// from: the source's snapshot versus what the target environment and group
// would get now. Only variables set on the project (Global or an environment)
// are compared; rise.toml, CLI, extension and system variables come from
// elsewhere and would only add noise.

interface Var {
    key: string;
    value: string;
    is_secret: boolean;
    is_protected: boolean;
    source?: string | null;
}

type Change =
    | { kind: 'added'; key: string; value: string | null }
    | { kind: 'removed'; key: string; value: string | null }
    | { kind: 'changed'; key: string; from: string | null; to: string | null }
    | { kind: 'unknown'; key: string };

const projectSet = (v: Var) => !!v.source && (v.source === 'global' || v.source.startsWith('env:'));
const MASK = '••••••••';
// A value the browser may show: plain, or an unprotected secret the API revealed.
const readable = (v: Var) => !v.is_protected && v.value !== MASK;
const shown = (v: Var) => (v.is_secret ? null : v.value);

export function diffVars(before: Var[], after: Var[]): Change[] {
    const a = new Map(before.filter(projectSet).map(v => [v.key, v]));
    const b = new Map(after.filter(projectSet).map(v => [v.key, v]));
    const out: Change[] = [];
    for (const [key, v] of b) {
        const old = a.get(key);
        if (!old) out.push({ kind: 'added', key, value: shown(v) });
        else if (!readable(old) || !readable(v)) {
            if (old.is_protected || v.is_protected) out.push({ kind: 'unknown', key });
        } else if (old.value !== v.value) out.push({ kind: 'changed', key, from: shown(old), to: shown(v) });
    }
    for (const [key, v] of a) if (!b.has(key)) out.push({ kind: 'removed', key, value: shown(v) });
    const order = { changed: 0, added: 1, removed: 2, unknown: 3 };
    return out.sort((x, y) => order[x.kind] - order[y.kind] || x.key.localeCompare(y.key));
}

const unwrap = (d: unknown): Var[] => (Array.isArray(d) ? d : ((d as { env_vars?: Var[] })?.env_vars ?? [])) as Var[];

/** The variable changes a deployment made from `sourceId` into `environment`/`group` would bring. */
export function VarChanges({ projectName, sourceId, environment, group }: {
    projectName: string;
    sourceId: string;
    environment: string | null;
    group: string | null;
}) {
    const [changes, setChanges] = useState<Change[] | null>(null);
    const [failed, setFailed] = useState(false);

    useEffect(() => {
        let cancelled = false;
        setChanges(null);
        setFailed(false);
        Promise.all([
            api.getDeploymentEnvVars(projectName, sourceId, true),
            api.previewEnvVars(projectName, environment, group),
        ])
            .then(([before, after]) => { if (!cancelled) setChanges(diffVars(unwrap(before), unwrap(after))); })
            .catch(() => { if (!cancelled) setFailed(true); });
        return () => { cancelled = true; };
    }, [projectName, sourceId, environment, group]);

    if (failed) return <p className="r-var-changes-note">Could not compare variables with this deployment's snapshot.</p>;
    if (!changes) return <p className="r-var-changes-note">Comparing variables…</p>;
    const known = changes.filter(c => c.kind !== 'unknown');
    const protectedCount = changes.length - known.length;
    const protectedNote = protectedCount > 0
        ? ` ${protectedCount} protected value${protectedCount === 1 ? '' : 's'} can't be read back, so ${protectedCount === 1 ? 'it is' : 'they are'} not compared.`
        : '';
    if (known.length === 0) {
        return <p className="r-var-changes-note">No variable changes against this deployment's snapshot.{protectedNote}</p>;
    }

    return (
        <div className="r-var-changes">
            <div className="r-section-label">Variable changes</div>
            <ul>
                {known.map(c => (
                    <li key={c.key} className={cx('r-var-change', c.kind)}>
                        <span className="mark" aria-hidden>{c.kind === 'added' ? '+' : c.kind === 'removed' ? '−' : '~'}</span>
                        <span className="mono key">{c.key}</span>
                        <span className="what">
                            {c.kind === 'added' && <>added{c.value !== null && <> · <span className="mono">{c.value}</span></>}</>}
                            {c.kind === 'removed' && <>removed</>}
                            {c.kind === 'changed' && (c.from !== null && c.to !== null
                                ? <><span className="mono">{c.from}</span> → <span className="mono">{c.to}</span></>
                                : <>secret value changed</>)}
                        </span>
                    </li>
                ))}
            </ul>
            {protectedNote && <p className="r-var-changes-note">{protectedNote.trim()}</p>}
        </div>
    );
}
