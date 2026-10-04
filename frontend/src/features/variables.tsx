import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../lib/api';
import { useCurrentProject, type Environment } from '../lib/project-context';
import { sortEnvironments } from '../lib/env-state';
import { useIsMobile } from '../lib/use-media';
import { useToast } from '../components/toast';
import { Icon } from '../components/icon';
import { SectionHead } from '../components/section-head';
import { ErrorState, LoadingState } from '../components/states';
import {
    Button,
    Combobox,
    ConfirmDialog,
    EnvironmentIcon,
    Field,
    Input,
    Modal,
    Panel,
    SearchInput,
    Segmented,
    Textarea,
    cx,
} from '../components/r-ui';

// Variables live in scopes: Global applies everywhere, an environment's own
// value overrides it there. The page shows one table per scope. Environment
// tables list every key set in *any* environment, so a key one environment
// lacks shows up as a placeholder to fill in rather than going unnoticed.

interface EnvVar {
    key: string;
    value: string;
    is_secret: boolean;
    is_protected: boolean;
    environment?: string | null;
}

type VarType = 'plain' | 'secret' | 'protected';

const typeOf = (v: Pick<EnvVar, 'is_secret' | 'is_protected'>): VarType =>
    !v.is_secret ? 'plain' : v.is_protected ? 'protected' : 'secret';

const typeToApi = (type: VarType) => ({ is_secret: type !== 'plain', is_protected: type === 'protected' });

const scopeKey = (scope: string | null) => scope ?? '';
const revealKey = (v: EnvVar) => `${v.environment || ''}:${v.key}`;

interface Form {
    key: string;
    value: string;
    type: VarType;
    environment: string | null;
}

interface Editing {
    /** The variable being edited; null when adding. */
    original: EnvVar | null;
    /** Filling a placeholder: the key is given. */
    lockKey?: boolean;
}

export function VariablesSection() {
    const current = useCurrentProject()!;
    const { projectName, reload } = current;
    const environments = useMemo(() => sortEnvironments(current.environments), [current.environments]);
    const [vars, setVars] = useState<EnvVar[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [search, setSearch] = useState('');
    const [revealed, setRevealed] = useState<Record<string, string>>({});
    // Keys edited in this browser session: their running deployments still
    // have the old snapshot until the next deploy.
    const [edited, setEdited] = useState<Set<string>>(() => new Set());
    const [editing, setEditing] = useState<Editing | null>(null);
    const [form, setForm] = useState<Form>({ key: '', value: '', type: 'plain', environment: null });
    const [saving, setSaving] = useState(false);
    const [toDelete, setToDelete] = useState<EnvVar | null>(null);
    const [deleting, setDeleting] = useState(false);
    const importInputRef = useRef<HTMLInputElement>(null);
    const { showToast } = useToast();
    const isMobile = useIsMobile();

    const load = useCallback(async () => {
        try {
            const res: { env_vars?: EnvVar[] } = await api.getProjectEnvVars(projectName, null);
            setVars(res.env_vars || []);
            setError(null);
        } catch (err) {
            setError((err as Error).message);
        }
    }, [projectName]);

    useEffect(() => { load(); }, [load]);

    // `#env-<name>` (from an environment card) scrolls to that table.
    useEffect(() => {
        if (!vars) return;
        const id = decodeURIComponent(window.location.hash.slice(1));
        if (id) document.getElementById(id)?.scrollIntoView({ block: 'start' });
    }, [vars]);

    const changed = (key: string) => {
        setEdited(s => new Set(s).add(key));
        load();
        reload();
    };

    const openAdd = (environment: string | null, key = '', type: VarType = 'plain') => {
        setEditing({ original: null, lockKey: !!key });
        setForm({ key, value: '', type, environment });
    };

    const openEdit = (v: EnvVar) => {
        setEditing({ original: v });
        setForm({ key: v.key, value: v.is_secret ? '' : v.value, type: typeOf(v), environment: v.environment || null });
    };

    const save = async () => {
        const original = editing?.original ?? null;
        const key = form.key.trim();
        if (!key) { showToast('Key is required', 'error'); return; }
        if (!original && !form.value) { showToast('Value is required', 'error'); return; }
        const typeChanged = !!original && typeOf(original) !== form.type;
        if (original?.is_secret && typeChanged && !form.value) {
            showToast('Enter the value again to change the type of a secret', 'error');
            return;
        }
        setSaving(true);
        try {
            const from = original?.environment || null;
            const to = form.environment;
            if (original && from !== to) await api.moveEnvVar(projectName, key, from, to);
            // A secret's value is never sent back to the browser, so an empty
            // field keeps it. A plain value is always resent with the type.
            if (form.value || (original && !original.is_secret)) {
                const { is_secret, is_protected } = typeToApi(form.type);
                await api.setEnvVar(projectName, key, form.value, is_secret, is_protected, to);
            }
            showToast(<><span className="mono">{key}</span> saved · redeploy to apply</>, 'success');
            setEditing(null);
            changed(key);
        } catch (err) {
            showToast(`Failed to save ${key}: ${(err as Error).message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const confirmDelete = async () => {
        if (!toDelete) return;
        setDeleting(true);
        try {
            await api.deleteEnvVar(projectName, toDelete.key, toDelete.environment || null);
            showToast(<><span className="mono">{toDelete.key}</span> deleted · redeploy to apply</>, 'success');
            changed(toDelete.key);
            setToDelete(null);
        } catch (err) {
            showToast(`Failed to delete ${toDelete.key}: ${(err as Error).message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    const reveal = async (v: EnvVar) => {
        const k = revealKey(v);
        if (revealed[k] !== undefined) {
            setRevealed(r => { const next = { ...r }; delete next[k]; return next; });
            return;
        }
        try {
            const res: { value: string } = await api.getEnvVarValue(projectName, v.key, v.environment || null);
            setRevealed(r => ({ ...r, [k]: res.value }));
        } catch (err) {
            showToast(`Failed to reveal ${v.key}: ${(err as Error).message}`, 'error');
        }
    };

    // Export the loaded variables as a .env file. Secret values never reach the
    // browser, so secret keys are written without a value.
    const exportEnv = () => {
        const lines = (vars || []).map(v => {
            const scope = v.environment ? ` (${v.environment})` : '';
            if (v.is_secret) return `# ${v.key}${scope} is a secret; value not exported\n${v.key}=`;
            return `${v.environment ? `# ${v.environment}\n` : ''}${v.key}=${v.value ?? ''}`;
        });
        const blob = new Blob([lines.join('\n') + '\n'], { type: 'text/plain' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `${projectName}.env`;
        a.click();
        URL.revokeObjectURL(url);
    };

    // Import a .env file as plain Global variables.
    const importEnv = async (e: React.ChangeEvent<HTMLInputElement>) => {
        const file = e.target.files?.[0];
        e.target.value = '';
        if (!file) return;
        let text = '';
        try {
            text = await file.text();
        } catch (err) {
            showToast(`Failed to read file: ${(err as Error).message}`, 'error');
            return;
        }
        const entries: [string, string][] = [];
        for (const raw of text.split('\n')) {
            const line = raw.trim();
            if (!line || line.startsWith('#')) continue;
            const eq = line.indexOf('=');
            if (eq <= 0) continue;
            let key = line.slice(0, eq).trim();
            if (key.startsWith('export ')) key = key.slice(7).trim();
            let value = line.slice(eq + 1).trim();
            if (value.length >= 2 && ((value[0] === '"' && value.endsWith('"')) || (value[0] === "'" && value.endsWith("'")))) {
                value = value.slice(1, -1);
            }
            if (key) entries.push([key, value]);
        }
        if (entries.length === 0) {
            showToast('No variables found in file', 'error');
            return;
        }
        let ok = 0;
        for (const [key, value] of entries) {
            try {
                await api.setEnvVar(projectName, key, value, false, false, null);
                ok++;
            } catch (err) {
                showToast(`Failed to import ${key}: ${(err as Error).message}`, 'error');
            }
        }
        if (ok > 0) showToast(`Imported ${ok} variable${ok === 1 ? '' : 's'} into Global`, 'success');
        load();
        reload();
    };

    const actions = (
        <>
            <input ref={importInputRef} type="file" accept=".env,text/plain" style={{ display: 'none' }} onChange={importEnv} />
            <Button icon="download" onClick={() => importInputRef.current?.click()} title="Import a .env file as plain Global variables">Import .env</Button>
            <Button icon="ext" onClick={exportEnv} disabled={!vars?.length}>Export</Button>
            <Button variant="primary" icon="plus" onClick={() => openAdd(null)}>Add variable</Button>
        </>
    );

    if (error && !vars) return <><SectionHead section="variables" /><ErrorState message={`Error loading variables: ${error}`} onRetry={load} /></>;
    if (!vars) return <><SectionHead section="variables" /><LoadingState label="Loading variables…" /></>;

    const q = search.trim().toLowerCase();
    const global = vars.filter(v => !v.environment);
    const globalByKey = new Map(global.map(v => [v.key, v]));
    // Every key set in any environment (not just Global), sorted.
    const envKeys = Array.from(new Set(vars.filter(v => v.environment).map(v => v.key))).sort();
    // Variables in environments that no longer exist still deserve a table.
    const orphanScopes = Array.from(new Set(vars.map(v => v.environment).filter((e): e is string => !!e && !environments.some(x => x.name === e))));
    const scopes: { name: string; env: Environment | null }[] = [
        ...environments.map(env => ({ name: env.name, env })),
        ...orphanScopes.map(name => ({ name, env: null })),
    ];
    const matches = (key: string, value?: string) => !q || key.toLowerCase().includes(q) || (value || '').toLowerCase().includes(q);

    const rowProps = { revealed, onReveal: reveal, onEdit: openEdit, onDelete: setToDelete, edited, isMobile };

    return (
        <div>
            <SectionHead section="variables" actions={actions} />
            <div className="r-vars-info">
                <Icon name="lock" size={13} />
                <span>
                    Global values apply to every environment; an environment's own value overrides it there. Secrets stay masked until
                    revealed, and protected secrets can never be read back. Changes apply on the next deployment.
                </span>
            </div>
            <div className="r-toolbar">
                <SearchInput value={search} onChange={setSearch} placeholder="Filter keys or values" style={{ flex: '1 1 220px', maxWidth: 320 }} />
            </div>

            <VarTable
                id="env-global"
                title="Global"
                sub="Every environment"
                count={global.length}
                emptyText={global.length === 0 ? 'No global variables.' : undefined}
                onAdd={() => openAdd(null)}
                isMobile={isMobile}
            >
                {global.filter(v => matches(v.key, v.value)).map(v => <VarRow key={v.key} v={v} {...rowProps} />)}
            </VarTable>

            {scopes.map(({ name, env }) => {
                const own = new Map(vars.filter(v => v.environment === name).map(v => [v.key, v]));
                const missing = envKeys.filter(k => !own.has(k)).length;
                return (
                    <VarTable
                        key={name}
                        id={`env-${name}`}
                        title={name}
                        color={env?.color}
                        sub={env ? `Overrides Global in ${name}` : 'Environment no longer exists'}
                        count={own.size}
                        missing={missing}
                        emptyText={envKeys.length === 0 ? `Nothing set per environment yet. A value here overrides Global in ${name}.` : undefined}
                        onAdd={() => openAdd(name)}
                        isMobile={isMobile}
                    >
                        {envKeys.filter(k => matches(k, own.get(k)?.value)).map(k => {
                            const v = own.get(k);
                            if (v) return <VarRow key={k} v={v} {...rowProps} />;
                            const sibling = vars.find(x => x.key === k && x.environment);
                            return (
                                <PlaceholderRow
                                    key={k}
                                    varKey={k}
                                    env={name}
                                    inherited={globalByKey.get(k) ?? null}
                                    isMobile={isMobile}
                                    onSet={() => openAdd(name, k, sibling ? typeOf(sibling) : 'plain')}
                                />
                            );
                        })}
                    </VarTable>
                );
            })}

            <Modal
                isOpen={!!editing}
                onClose={() => setEditing(null)}
                title={editing?.original ? 'Edit variable' : 'Add variable'}
                sub="Changes apply on the next deploy."
                footer={
                    <>
                        {editing?.original && (
                            <Button variant="ghost" className="r-btn-danger-text r-foot-left" icon="trash" disabled={saving}
                                onClick={() => { const v = editing.original; setEditing(null); setToDelete(v); }}>
                                Delete
                            </Button>
                        )}
                        <Button onClick={() => setEditing(null)} disabled={saving}>Cancel</Button>
                        <Button variant="primary" onClick={save} loading={saving}>Save variable</Button>
                    </>
                }
            >
                <Field label="Scope" hint={form.environment ? `Overrides the Global value in ${form.environment}.` : 'Applies to every environment without its own value.'}>
                    <Combobox
                        value={form.environment || ''}
                        onChange={v => setForm(f => ({ ...f, environment: v || null }))}
                        options={[
                            { value: '', label: 'Global (all environments)' },
                            ...environments.map(env => ({ value: env.name, label: env.name, icon: <EnvironmentIcon color={env.color} size={12} /> })),
                        ]}
                        placeholder="Global (all environments)"
                    />
                </Field>
                <Field label="Key">
                    <Input
                        className="mono"
                        value={form.key}
                        onChange={e => setForm(f => ({ ...f, key: e.target.value.replace(/\s/g, '_') }))}
                        placeholder="DATABASE_URL"
                        disabled={!!editing?.original || !!editing?.lockKey}
                        autoFocus={!editing?.lockKey && !editing?.original}
                    />
                </Field>
                <Field
                    label="Value"
                    hint={editing?.original?.is_secret ? 'Leave empty to keep the current secret value.' : undefined}
                >
                    <Textarea
                        className="mono"
                        value={form.value}
                        onChange={e => setForm(f => ({ ...f, value: e.target.value }))}
                        placeholder={editing?.original?.is_secret ? '••••••••' : 'value'}
                        rows={3}
                        autoFocus={!!editing?.lockKey || !!editing?.original}
                    />
                </Field>
                <Field label="Type" hint="Secrets are encrypted and masked. Protected secrets are write-only: they can't be revealed or read back.">
                    <Segmented<VarType>
                        value={form.type}
                        options={[
                            { value: 'plain', label: 'Plain' },
                            { value: 'secret', label: 'Secret' },
                            { value: 'protected', label: 'Protected' },
                        ]}
                        onChange={type => setForm(f => ({ ...f, type }))}
                    />
                </Field>
            </Modal>

            <ConfirmDialog
                isOpen={!!toDelete}
                onClose={() => setToDelete(null)}
                onConfirm={confirmDelete}
                title={`Delete ${toDelete?.key ?? ''}?`}
                message={`Removes ${toDelete?.key} from ${toDelete?.environment ? toDelete.environment : 'Global'}. Running deployments keep their snapshot until the next deploy.`}
                confirmText="Delete variable"
                confirmTone="danger"
                loading={deleting}
            />
        </div>
    );
}

function VarTable({ id, title, sub, color, count, missing = 0, emptyText, onAdd, isMobile, children }: {
    id: string;
    title: string;
    sub: string;
    color?: string;
    count: number;
    missing?: number;
    emptyText?: string;
    onAdd: () => void;
    isMobile: boolean;
    children: React.ReactNode;
}) {
    return (
        <section id={id} className="r-vars-scope">
            <div className="r-vars-scope-head">
                {color && <EnvironmentIcon color={color} size={13} />}
                <span className="title">{title}</span>
                <span className="sub">{sub} · {count} set{missing > 0 && <span className="missing"> · {missing} missing</span>}</span>
                <button type="button" className="r-row-action" onClick={onAdd}>
                    <Icon name="plus" size={12} /> Add
                </button>
            </div>
            {emptyText ? <Panel><div className="r-vars-empty">{emptyText}</div></Panel> : isMobile ? <div className="r-cards">{children}</div> : (
                <Panel>
                    <table className="r-table r-vars-table">
                        <colgroup>
                            <col style={{ width: '34%' }} />
                            <col />
                            <col style={{ width: 120 }} />
                        </colgroup>
                        <tbody>{children}</tbody>
                    </table>
                </Panel>
            )}
        </section>
    );
}

function TypeBadges({ v }: { v: Pick<EnvVar, 'is_secret' | 'is_protected'> }) {
    if (v.is_protected) return <span className="r-var-badge protected" title="Write-only: can't be revealed or read back"><Icon name="lock" size={10} />Protected</span>;
    if (v.is_secret) return <span className="r-var-badge secret">Secret</span>;
    return null;
}

interface RowProps {
    v: EnvVar;
    revealed: Record<string, string>;
    onReveal: (v: EnvVar) => void;
    onEdit: (v: EnvVar) => void;
    onDelete: (v: EnvVar) => void;
    edited: Set<string>;
    isMobile: boolean;
}

function VarRow({ v, revealed, onReveal, onEdit, onDelete, edited, isMobile }: RowProps) {
    const shown = revealed[revealKey(v)];
    const masked = v.is_secret && shown === undefined;
    const value = masked ? '•••••••••••••' : (shown ?? v.value);
    const canReveal = v.is_secret && !v.is_protected;
    const note = edited.has(v.key) ? 'Edited just now · redeploy to apply' : null;
    const actions = (
        <span className="r-row-actions">
            {canReveal && (
                <button type="button" className="r-icon-btn r-var-act" title={shown === undefined ? 'Reveal' : 'Hide'} aria-label={`${shown === undefined ? 'Reveal' : 'Hide'} ${v.key}`}
                    onClick={e => { e.stopPropagation(); onReveal(v); }}>
                    <Icon name={shown === undefined ? 'eye' : 'eyeoff'} size={14} />
                </button>
            )}
            <button type="button" className="r-icon-btn r-var-act" title="Edit" aria-label={`Edit ${v.key}`} onClick={e => { e.stopPropagation(); onEdit(v); }}>
                <Icon name="edit" size={13} />
            </button>
            {!isMobile && (
                <button type="button" className="r-icon-btn r-var-act danger" title="Delete" aria-label={`Delete ${v.key}`} onClick={e => { e.stopPropagation(); onDelete(v); }}>
                    <Icon name="trash" size={13} />
                </button>
            )}
        </span>
    );
    if (isMobile) {
        return (
            <div className="r-card" role="button" tabIndex={0} onClick={() => onEdit(v)} onKeyDown={e => { if (e.key === 'Enter') onEdit(v); }}>
                <div className="r-card-row">
                    <span className="mono r-var-key">{v.key}</span>
                    <TypeBadges v={v} />
                    <span style={{ marginLeft: 'auto' }}>{actions}</span>
                </div>
                <div className={cx('mono r-var-value', masked && 'masked')}>{value}</div>
                {note && <div className="r-var-note">{note}</div>}
            </div>
        );
    }
    return (
        <tr>
            <td>
                <div className="r-var-keycell">
                    <span className="mono r-var-key">{v.key}</span>
                    <TypeBadges v={v} />
                </div>
                {note && <div className="r-var-note">{note}</div>}
            </td>
            <td className={cx('mono r-var-value', masked && 'masked')} title={masked ? undefined : value}>{value}</td>
            <td style={{ textAlign: 'right' }}>{actions}</td>
        </tr>
    );
}

function PlaceholderRow({ varKey, env, inherited, onSet, isMobile }: {
    varKey: string;
    env: string;
    inherited: EnvVar | null;
    onSet: () => void;
    isMobile: boolean;
}) {
    const state = inherited
        ? <>Not set in {env} · uses the Global value</>
        : <>Not set in {env}</>;
    const button = (
        <button type="button" className="r-row-action" onClick={e => { e.stopPropagation(); onSet(); }}>
            Set value
        </button>
    );
    if (isMobile) {
        return (
            <div className="r-card r-var-placeholder" role="button" tabIndex={0} onClick={onSet} onKeyDown={e => { if (e.key === 'Enter') onSet(); }}>
                <div className="r-card-row">
                    <span className="mono r-var-key">{varKey}</span>
                    <span style={{ marginLeft: 'auto' }}>{button}</span>
                </div>
                <div className="r-var-missing">{state}</div>
            </div>
        );
    }
    return (
        <tr className="r-var-placeholder">
            <td><span className="mono r-var-key">{varKey}</span></td>
            <td className="r-var-missing">{state}</td>
            <td style={{ textAlign: 'right' }}>{button}</td>
        </tr>
    );
}
