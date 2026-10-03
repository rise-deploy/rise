import React from 'react';
import { Alert, Button, Field, Input, Select } from '../components/r-ui';

/** A named permission (ADR-0006 §3). */
export type Permission =
    | 'view'
    | 'logs'
    | 'deploy'
    | 'env-vars'
    | 'secrets'
    | 'configure'
    | 'service-accounts'
    | 'project-admin';

export type Preset = 'read' | 'deploy' | 'develop' | 'admin';

export interface Grant {
    project: string;
    environment?: string;
    preset?: Preset;
    permissions?: Permission[];
}

/** What a device login asks for, and what an approval grants. */
export type AccessRequest = { kind: 'full' } | { kind: 'restricted'; grants: Grant[] };

const PERMISSIONS: { id: Permission; label: string; environment: boolean }[] = [
    { id: 'view', label: 'View', environment: true },
    { id: 'logs', label: 'Logs', environment: true },
    { id: 'deploy', label: 'Deploy', environment: true },
    { id: 'env-vars', label: 'Env vars', environment: true },
    { id: 'secrets', label: 'Read secrets', environment: true },
    { id: 'configure', label: 'Configure', environment: false },
    { id: 'service-accounts', label: 'Service accounts', environment: false },
    { id: 'project-admin', label: 'Project admin', environment: false },
];

const PRESETS: Record<Preset, Permission[]> = {
    read: ['view'],
    deploy: ['view', 'logs', 'deploy'],
    develop: ['view', 'logs', 'deploy', 'env-vars'],
    admin: PERMISSIONS.map((p) => p.id),
};

/** One editable grant: a target and its permission set. */
export interface GrantRow {
    project: string;
    environment: string;
    permissions: Permission[];
}

export function rowsFromRequest(access: AccessRequest): GrantRow[] {
    if (access.kind !== 'restricted') return [];
    return access.grants.map((grant) => ({
        project: grant.project,
        environment: grant.environment ?? '',
        permissions: Array.from(
            new Set([...(grant.preset ? PRESETS[grant.preset] : []), ...(grant.permissions ?? [])]),
        ),
    }));
}

export function requestFromRows(full: boolean, rows: GrantRow[]): AccessRequest {
    if (full) return { kind: 'full' };
    return {
        kind: 'restricted',
        grants: rows.map((row) => ({
            project: row.project.trim(),
            ...(row.environment.trim() ? { environment: row.environment.trim() } : {}),
            permissions: row.permissions,
        })),
    };
}

/** Why the rows can't be approved as they are, if they can't. */
export function rowsProblem(full: boolean, rows: GrantRow[]): string | null {
    if (full) return null;
    if (rows.length === 0) return 'Add at least one project or environment, or grant full access.';
    for (const row of rows) {
        if (!row.project.trim()) return 'Every row needs a project.';
        if (row.permissions.length === 0) return `Choose what '${row.project}' may do.`;
    }
    return null;
}

function presetOf(row: GrantRow): Preset | 'custom' {
    const set = [...row.permissions].sort().join(',');
    const match = (Object.keys(PRESETS) as Preset[]).find((preset) => [...PRESETS[preset]].sort().join(',') === set);
    return match ?? 'custom';
}

export function AccessEditor({
    requested,
    full,
    rows,
    onChange,
}: {
    requested: AccessRequest;
    full: boolean;
    rows: GrantRow[];
    onChange: (full: boolean, rows: GrantRow[]) => void;
}) {
    const update = (index: number, patch: Partial<GrantRow>) =>
        onChange(
            full,
            rows.map((row, i) => {
                if (i !== index) return row;
                const next = { ...row, ...patch };
                // An environment grant can't carry project-wide permissions.
                if (next.environment.trim()) {
                    next.permissions = next.permissions.filter((p) => PERMISSIONS.find((d) => d.id === p)?.environment);
                }
                return next;
            }),
        );
    const togglePermission = (index: number, permission: Permission) => {
        const row = rows[index];
        const permissions = row.permissions.includes(permission)
            ? row.permissions.filter((p) => p !== permission)
            : [...row.permissions, permission];
        update(index, { permissions });
    };
    const deployAndSecrets = rows.some((row) => row.permissions.includes('deploy') && row.permissions.includes('secrets'));

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
            {requested.kind === 'full' && (
                <Alert tone="warn" icon="info">
                    This login asks for <strong>full access</strong>: everything you can do in Rise, admin rights
                    included. Restrict it below if it only needs some projects or environments.
                </Alert>
            )}
            <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap' }}>
                <label style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
                    <input type="radio" checked={full} onChange={() => onChange(true, rows)} />
                    Full access
                </label>
                <label style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
                    <input
                        type="radio"
                        checked={!full}
                        onChange={() => onChange(false, rows.length ? rows : [{ project: '', environment: '', permissions: ['view'] }])}
                    />
                    Only these projects and environments
                </label>
            </div>

            {!full && (
                <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                    {rows.map((row, index) => {
                        const preset = presetOf(row);
                        const environmentRow = row.environment.trim() !== '';
                        return (
                            <div
                                key={index}
                                style={{ border: '1px solid var(--line, #8883)', borderRadius: 6, padding: 12, display: 'flex', flexDirection: 'column', gap: 10 }}
                            >
                                <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'flex-end' }}>
                                    <div style={{ flex: '1 1 140px' }}>
                                        <Field label="Project">
                                            <Input value={row.project} onChange={(e) => update(index, { project: e.target.value })} className="mono" />
                                        </Field>
                                    </div>
                                    <div style={{ flex: '1 1 140px' }}>
                                        <Field label="Environment" hint="Empty: the whole project">
                                            <Input
                                                value={row.environment}
                                                onChange={(e) => update(index, { environment: e.target.value })}
                                                className="mono"
                                            />
                                        </Field>
                                    </div>
                                    <div style={{ flex: '1 1 140px' }}>
                                        <Field label="Access">
                                            <Select
                                                value={preset}
                                                onChange={(value) => {
                                                    if (value !== 'custom') update(index, { permissions: [...PRESETS[value as Preset]] });
                                                }}
                                                options={[
                                                    { value: 'read', label: 'Read' },
                                                    { value: 'deploy', label: 'Deploy' },
                                                    { value: 'develop', label: 'Develop' },
                                                    ...(environmentRow ? [] : [{ value: 'admin', label: 'Admin' }]),
                                                    { value: 'custom', label: 'Custom' },
                                                ]}
                                            />
                                        </Field>
                                    </div>
                                    <Button icon="close" onClick={() => onChange(full, rows.filter((_, i) => i !== index))}>
                                        Remove
                                    </Button>
                                </div>
                                <div style={{ display: 'flex', gap: 14, flexWrap: 'wrap' }}>
                                    {PERMISSIONS.filter((p) => !environmentRow || p.environment).map((permission) => (
                                        <label key={permission.id} style={{ display: 'flex', gap: 5, alignItems: 'center' }}>
                                            <input
                                                type="checkbox"
                                                checked={row.permissions.includes(permission.id)}
                                                onChange={() => togglePermission(index, permission.id)}
                                            />
                                            {permission.label}
                                        </label>
                                    ))}
                                </div>
                            </div>
                        );
                    })}
                    <div>
                        <Button
                            icon="plus"
                            onClick={() => onChange(full, [...rows, { project: rows[0]?.project ?? '', environment: '', permissions: ['view'] }])}
                        >
                            Add project or environment
                        </Button>
                    </div>
                    {deployAndSecrets && (
                        <Alert tone="info" icon="info">
                            A login that can deploy can also read secrets in practice: a deployment can print its own
                            environment. "Read secrets" only matters without "Deploy".
                        </Alert>
                    )}
                </div>
            )}
        </div>
    );
}
