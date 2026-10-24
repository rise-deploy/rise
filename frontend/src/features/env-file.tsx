import { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { errorText } from '../lib/api-error';
import type { Environment } from '../lib/project-context';
import { useToast } from '../components/toast';
import { Icon } from '../components/icon';
import { Button, Combobox, EnvironmentIcon, Field, Modal, Segmented, cx } from '../components/r-ui';

// .env import and export, one scope at a time. Secret values never reach the
// browser, so an export lists secrets as comments, and an import never turns
// an existing secret into a plain value.

export interface ScopedVar {
    key: string;
    value: string;
    is_secret: boolean;
    is_protected: boolean;
    environment?: string | null;
}

type ImportType = 'plain' | 'secret' | 'protected';

/** Parse `KEY=value` lines; comments, blanks and `export ` prefixes are allowed. */
export function parseEnvFile(text: string): [string, string][] {
    const entries: [string, string][] = [];
    for (const raw of text.split(/\r?\n/)) {
        const line = raw.trim();
        if (!line || line.startsWith('#')) continue;
        const eq = line.indexOf('=');
        if (eq <= 0) continue;
        let key = line.slice(0, eq).trim();
        if (key.startsWith('export ')) key = key.slice(7).trim();
        let value = line.slice(eq + 1).trim();
        if (value.length >= 2 && value[0] === '"' && value.endsWith('"')) {
            value = value.slice(1, -1).replace(/\\n/g, '\n').replace(/\\"/g, '"').replace(/\\\\/g, '\\');
        } else if (value.length >= 2 && value[0] === "'" && value.endsWith("'")) {
            value = value.slice(1, -1);
        }
        if (key) entries.push([key, value]);
    }
    // A key listed twice keeps its last value, as a shell would.
    return [...new Map(entries)];
}

function quote(value: string): string {
    if (/^[A-Za-z0-9_./:@%+,=-]*$/.test(value)) return value;
    return `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n')}"`;
}

/** A scope's variables as a .env file. Secrets become comments, so re-importing skips them. */
export function envFileFor(vars: ScopedVar[], scope: string | null, projectName: string): string {
    const inScope = vars.filter(v => (v.environment || null) === scope).sort((a, b) => a.key.localeCompare(b.key));
    const lines = [`# ${projectName} · ${scope ?? 'Global'} variables`];
    for (const v of inScope) {
        lines.push(v.is_secret ? `# ${v.key}: ${v.is_protected ? 'protected' : 'secret'}, value not exported` : `${v.key}=${quote(v.value ?? '')}`);
    }
    return lines.join('\n') + '\n';
}

function scopeOptions(environments: Environment[]) {
    return [
        { value: '', label: 'Global (all environments)' },
        ...environments.map(env => ({ value: env.name, label: env.name, icon: <EnvironmentIcon color={env.color} size={12} /> })),
    ];
}

export function ExportEnvDialog({ open, onClose, projectName, vars, environments }: {
    open: boolean;
    onClose: () => void;
    projectName: string;
    vars: ScopedVar[];
    environments: Environment[];
}) {
    const [scope, setScope] = useState<string | null>(null);
    useEffect(() => { if (open) setScope(null); }, [open]);
    const inScope = vars.filter(v => (v.environment || null) === scope);
    const secrets = inScope.filter(v => v.is_secret).length;

    const download = () => {
        const blob = new Blob([envFileFor(vars, scope, projectName)], { type: 'text/plain' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `${projectName}.${scope ?? 'global'}.env`;
        a.click();
        URL.revokeObjectURL(url);
        onClose();
    };

    return (
        <Modal
            isOpen={open}
            onClose={onClose}
            title="Export variables"
            sub="One scope per file. Secret values never leave Rise: they are listed as comments."
            footer={<>
                <Button onClick={onClose}>Cancel</Button>
                <Button variant="primary" icon="download" onClick={download} disabled={inScope.length === 0}>Download .env</Button>
            </>}
        >
            <Field label="Scope">
                <Combobox value={scope ?? ''} onChange={v => setScope(v || null)} options={scopeOptions(environments)} placeholder="Global (all environments)" />
            </Field>
            <p className="r-field-hint">
                {inScope.length === 0
                    ? 'No variables are set in this scope.'
                    : `${inScope.length - secrets} value${inScope.length - secrets === 1 ? '' : 's'}${secrets ? `, ${secrets} secret${secrets === 1 ? '' : 's'} as comments` : ''}.`}
            </p>
        </Modal>
    );
}

type RowState = 'new' | 'overwrite' | 'same' | 'blocked';

interface Row {
    key: string;
    value: string;
    existing: ScopedVar | null;
    state: RowState;
}

function rowState(existing: ScopedVar | null, value: string, type: ImportType): RowState {
    if (!existing) return 'new';
    // An import never downgrades a secret: its value cannot be compared, and
    // a plain import would make it readable.
    if (existing.is_secret && type === 'plain') return 'blocked';
    if (!existing.is_secret && type === 'plain' && existing.value === value) return 'same';
    return 'overwrite';
}

const STATE_LABEL: Record<RowState, string> = {
    new: 'New',
    overwrite: 'Overwrites',
    same: 'Unchanged',
    blocked: 'Kept: secret',
};

export function ImportEnvDialog({ file, onClose, projectName, vars, environments, onImported }: {
    /** The parsed file, or null when closed. */
    file: { name: string; entries: [string, string][] } | null;
    onClose: () => void;
    projectName: string;
    vars: ScopedVar[];
    environments: Environment[];
    onImported: (written: { key: string; environment: string | null }[]) => void;
}) {
    const [scope, setScope] = useState<string | null>(null);
    const [type, setType] = useState<ImportType>('plain');
    const [skipped, setSkipped] = useState<Set<string>>(new Set());
    const [importing, setImporting] = useState(false);
    const { showToast } = useToast();

    useEffect(() => {
        if (file) { setScope(null); setType('plain'); setSkipped(new Set()); }
    }, [file]);

    const rows: Row[] = useMemo(() => (file?.entries || []).map(([key, value]) => {
        const existing = vars.find(v => v.key === key && (v.environment || null) === scope) || null;
        return { key, value, existing, state: rowState(existing, value, type) };
    }), [file, vars, scope, type]);

    const selectable = (r: Row) => r.state === 'new' || r.state === 'overwrite';
    const chosen = rows.filter(r => selectable(r) && !skipped.has(r.key));
    const overwrites = chosen.filter(r => r.state === 'overwrite').length;

    const toggle = (key: string) => setSkipped(s => {
        const next = new Set(s);
        if (next.has(key)) next.delete(key); else next.add(key);
        return next;
    });

    const run = async () => {
        setImporting(true);
        const written: { key: string; environment: string | null }[] = [];
        const failed: string[] = [];
        for (const r of chosen) {
            // Overwriting a secret keeps at least its protection.
            const secret = type !== 'plain';
            const isProtected = type === 'protected' || !!r.existing?.is_protected;
            try {
                await api.setEnvVar(projectName, r.key, r.value, secret, secret && isProtected, scope);
                written.push({ key: r.key, environment: scope });
            } catch (err) {
                failed.push(`${r.key} (${errorText(err)})`);
            }
        }
        setImporting(false);
        if (written.length) {
            showToast(`Imported ${written.length} variable${written.length === 1 ? '' : 's'} into ${scope ?? 'Global'}`, 'success');
            onImported(written);
        }
        if (failed.length) showToast(`Not imported: ${failed.join(', ')}`, 'error');
        else onClose();
    };

    return (
        <Modal
            isOpen={!!file}
            onClose={onClose}
            width="wide"
            title={`Import ${file?.name ?? '.env'}`}
            sub="Review what changes before anything is written. Changes apply on the next deploy."
            footer={<>
                <Button onClick={onClose} disabled={importing}>Cancel</Button>
                <Button variant="primary" loading={importing} disabled={chosen.length === 0} onClick={run}>
                    {chosen.length === 0 ? 'Nothing to import' : `Import ${chosen.length}${overwrites ? ` (${overwrites} overwrite${overwrites === 1 ? '' : 's'})` : ''}`}
                </Button>
            </>}
        >
            <div className="r-import-opts">
                <Field label="Into">
                    <Combobox value={scope ?? ''} onChange={v => setScope(v || null)} options={scopeOptions(environments)} placeholder="Global (all environments)" />
                </Field>
                <Field label="As">
                    <Segmented<ImportType>
                        value={type}
                        options={[{ value: 'plain', label: 'Plain' }, { value: 'secret', label: 'Secret' }, { value: 'protected', label: 'Protected' }]}
                        onChange={setType}
                    />
                </Field>
            </div>
            <div className="r-import-table" role="table" aria-label="Variables in the file">
                {rows.map(r => {
                    const on = selectable(r) && !skipped.has(r.key);
                    return (
                        <div key={r.key} role="row" className={cx('r-import-row', !selectable(r) && 'muted-row')}>
                            <span role="cell" className="r-import-check">
                                <input type="checkbox" checked={on} disabled={!selectable(r)} onChange={() => toggle(r.key)} aria-label={`Import ${r.key}`} />
                            </span>
                            <span role="cell" className="mono r-import-key">{r.key}</span>
                            <span role="cell" className="mono r-import-val">
                                {type === 'plain' ? (r.value || <span className="muted">empty</span>) : '••••••••'}
                                {r.state === 'overwrite' && r.existing && !r.existing.is_secret && type === 'plain' && (
                                    <span className="r-import-was">was {r.existing.value || 'empty'}</span>
                                )}
                            </span>
                            <span role="cell" className={cx('r-import-state', r.state)}>
                                {r.state === 'blocked' && <Icon name="lock" size={11} />}
                                {STATE_LABEL[r.state]}
                            </span>
                        </div>
                    );
                })}
            </div>
            {rows.some(r => r.state === 'blocked') && (
                <p className="r-field-hint">Keys that are secrets in this scope are kept. Import them as Secret or Protected to replace their values.</p>
            )}
        </Modal>
    );
}
