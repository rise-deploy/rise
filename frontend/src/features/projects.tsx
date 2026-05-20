// @ts-nocheck
import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { navigate } from '../lib/navigation';
import { useToast } from '../components/toast';
import { AutocompleteInput } from '../components/ui';
import {
    Button as RButton,
    Combobox as RCombobox,
    ConfirmDialog as RConfirmDialog,
    Empty,
    Field as RField,
    Input as RInput,
    Modal as RModal,
    Panel,
    Pill,
    Segmented,
} from '../components/r-ui';
import { Icon } from '../components/icon';

// CodeMirror is heavy; load it only when the service-account modal is opened.
const JsonEditor = lazy(() => import('../components/json-editor'));
const JsonEditorFallback = () => (
    <div style={{ padding: 20, fontSize: 12.5, color: 'var(--text-soft)', border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)' }}>
        Loading editor…
    </div>
);

// Small inline dropdown menu (button + absolutely-positioned list, close on click-away).
function AddMenu({ items }) {
    const [open, setOpen] = useState(false);
    const ref = useRef(null);

    useEffect(() => {
        if (!open) return;
        const onClick = (e) => {
            if (ref.current && !ref.current.contains(e.target)) setOpen(false);
        };
        document.addEventListener('mousedown', onClick);
        return () => document.removeEventListener('mousedown', onClick);
    }, [open]);

    return (
        <div ref={ref} style={{ position: 'relative', display: 'inline-block' }}>
            <RButton variant="primary" icon="plus" onClick={() => setOpen(o => !o)}>Add</RButton>
            {open && (
                <div
                    style={{
                        position: 'absolute',
                        top: 'calc(100% + 4px)',
                        right: 0,
                        zIndex: 30,
                        minWidth: 180,
                        background: 'var(--surface)',
                        border: '1px solid var(--border)',
                        borderRadius: 'var(--radius-sm)',
                        boxShadow: 'var(--shadow-md, 0 8px 24px rgba(0,0,0,0.18))',
                        padding: 4,
                    }}
                >
                    {items.map(item => (
                        <button
                            key={item.label}
                            type="button"
                            className="r-menu-item"
                            style={{
                                display: 'flex',
                                alignItems: 'center',
                                gap: 8,
                                width: '100%',
                                padding: '7px 10px',
                                background: 'transparent',
                                border: 'none',
                                borderRadius: 'var(--radius-xs, 4px)',
                                cursor: 'pointer',
                                fontSize: 13,
                                color: 'var(--text)',
                                textAlign: 'left',
                            }}
                            onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--surface-2)'; }}
                            onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}
                            onClick={() => { setOpen(false); item.onClick(); }}
                        >
                            {item.icon && <Icon name={item.icon} size={14} />}
                            {item.label}
                        </button>
                    ))}
                </div>
            )}
        </div>
    );
}

const KIND_META = {
    user: { icon: 'user', label: 'User' },
    team: { icon: 'users', label: 'Team' },
    sa: { icon: 'key', label: 'Service account' },
};

// Access tab component - manages access class and unified user/team/service-account access
export function AppUsersList({ projectName, project, accessClasses, currentUserEmail, onProjectUpdated }) {
    const [teams, setTeams] = useState([]);
    const [serviceAccounts, setServiceAccounts] = useState([]);
    const [environments, setEnvironments] = useState([]);
    const [typeFilter, setTypeFilter] = useState('all');

    const [editingOwner, setEditingOwner] = useState(false);
    const [newOwnerType, setNewOwnerType] = useState('user');
    const [newOwnerEmail, setNewOwnerEmail] = useState('');
    const [newOwnerTeamId, setNewOwnerTeamId] = useState('');
    const [updatingOwner, setUpdatingOwner] = useState(false);

    // Add-user modal
    const [addUserOpen, setAddUserOpen] = useState(false);
    const [newUserEmail, setNewUserEmail] = useState('');
    const [addingUser, setAddingUser] = useState(false);

    // Add-team modal
    const [addTeamOpen, setAddTeamOpen] = useState(false);
    const [selectedTeamId, setSelectedTeamId] = useState('');
    const [addingTeam, setAddingTeam] = useState(false);

    // Add-service-account modal
    const [addSaOpen, setAddSaOpen] = useState(false);
    const [saIssuerUrl, setSaIssuerUrl] = useState('');
    const [saAud, setSaAud] = useState('');
    const [saClaimsText, setSaClaimsText] = useState('{}');
    const [saSelectedEnvs, setSaSelectedEnvs] = useState([]);
    const [addingSa, setAddingSa] = useState(false);

    // Removal confirmation
    const [confirmRemove, setConfirmRemove] = useState(null);
    const [removing, setRemoving] = useState(false);

    const { showToast } = useToast();

    const appUsers = project?.app_users || [];
    const appTeams = project?.app_teams || [];
    const owner = project?.owner || null;

    const ownerUserEmail = owner?.email ? owner.email.trim() : null;
    const ownerTeamName = owner?.name ? owner.name.trim() : null;
    const ownerTeamId = owner?.id || null;

    const displayedUsers = (() => {
        if (!ownerUserEmail) return appUsers.map(u => ({ ...u, isOwnerFixed: false }));
        const ownerEmailLower = ownerUserEmail.toLowerCase();
        const nonOwnerUsers = appUsers.filter((u) => (u.email || '').toLowerCase() !== ownerEmailLower);
        return [{ id: owner?.id || `owner-user-${ownerUserEmail}`, email: ownerUserEmail, isOwnerFixed: true }, ...nonOwnerUsers];
    })();

    const displayedTeams = (() => {
        if (!ownerTeamName) return appTeams.map(t => ({ ...t, isOwnerFixed: false }));
        const nonOwnerTeams = appTeams.filter((t) => {
            if (ownerTeamId && t.id) return t.id !== ownerTeamId;
            return (t.name || '').toLowerCase() !== ownerTeamName.toLowerCase();
        });
        return [{ id: ownerTeamId || `owner-team-${ownerTeamName}`, name: ownerTeamName, isOwnerFixed: true }, ...nonOwnerTeams];
    })();

    const loadServiceAccounts = async () => {
        try {
            const response = await api.getProjectServiceAccounts(projectName);
            setServiceAccounts(response?.workload_identities || []);
        } catch (err) {
            console.error('Failed to load service accounts:', err);
        }
    };

    useEffect(() => {
        api.getTeams().then(d => setTeams(d || [])).catch(err => console.error('Failed to load teams:', err));
    }, []);

    useEffect(() => {
        loadServiceAccounts();
        api.getProjectEnvironments(projectName).then(d => setEnvironments(d || [])).catch(() => {});
    }, [projectName]);

    const handleChangeAccessClass = async (newAccessClass) => {
        if (!project || !newAccessClass || newAccessClass === project.access_class) return;
        try {
            await api.updateProject(projectName, { access_class: newAccessClass });
            const ac = accessClasses.find(a => a.id === newAccessClass);
            showToast(`Access class updated to ${ac ? ac.display_name : newAccessClass}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to update access class: ${err.message}`, 'error');
        }
    };

    const handleEditOwner = () => {
        if (owner?.email) {
            setNewOwnerType('user');
            setNewOwnerEmail(owner.email);
            setNewOwnerTeamId('');
        } else if (owner?.name) {
            setNewOwnerType('team');
            setNewOwnerEmail('');
            const match = teams.find(t => t.id === owner.id || t.name === owner.name);
            setNewOwnerTeamId(match?.id || owner.id || '');
        } else {
            setNewOwnerType('user');
            setNewOwnerEmail('');
            setNewOwnerTeamId('');
        }
        setEditingOwner(true);
    };

    const handleSaveOwner = async () => {
        if (newOwnerType === 'user' && !newOwnerEmail.trim()) {
            showToast('Owner email is required', 'error');
            return;
        }
        if (newOwnerType === 'team' && !newOwnerTeamId) {
            showToast('Owner team is required', 'error');
            return;
        }
        setUpdatingOwner(true);
        try {
            const payload = newOwnerType === 'user'
                ? { user: newOwnerEmail.trim() }
                : { team: newOwnerTeamId };
            await api.updateProject(projectName, { owner: payload });
            showToast('Project owner updated', 'success');
            setEditingOwner(false);
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to update owner: ${err.message}`, 'error');
        } finally {
            setUpdatingOwner(false);
        }
    };

    // --- Add flows ---
    const openAddUser = () => { setNewUserEmail(''); setAddUserOpen(true); };
    const openAddTeam = () => { setSelectedTeamId(''); setAddTeamOpen(true); };
    const openAddSa = () => {
        setSaIssuerUrl('');
        setSaAud(CONFIG.backendUrl);
        setSaClaimsText('{}');
        setSaSelectedEnvs([]);
        setAddSaOpen(true);
    };

    const handleAddUser = async () => {
        if (!newUserEmail.trim()) {
            showToast('User email is required', 'error');
            return;
        }
        setAddingUser(true);
        try {
            const currentUserEmails = appUsers.map(u => u.email);
            await api.updateProject(projectName, {
                app_users: [...currentUserEmails, newUserEmail.trim()],
            });
            showToast(`Added app user ${newUserEmail}`, 'success');
            setAddUserOpen(false);
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app user: ${err.message}`, 'error');
        } finally {
            setAddingUser(false);
        }
    };

    const handleAddTeam = async () => {
        if (!selectedTeamId) {
            showToast('Team selection is required', 'error');
            return;
        }
        const selectedTeam = teams.find(t => t.id === selectedTeamId);
        setAddingTeam(true);
        try {
            const currentTeamIds = appTeams.map(t => t.id);
            await api.updateProject(projectName, {
                app_teams: [...currentTeamIds, selectedTeamId],
            });
            showToast(`Added app team ${selectedTeam?.name || ''}`, 'success');
            setAddTeamOpen(false);
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app team: ${err.message}`, 'error');
        } finally {
            setAddingTeam(false);
        }
    };

    const handleAddServiceAccount = async () => {
        if (!saIssuerUrl.trim()) {
            showToast('Issuer URL is required', 'error');
            return;
        }
        if (!saAud.trim()) {
            showToast('Audience (aud) is required', 'error');
            return;
        }
        let claims = {};
        try {
            if (saClaimsText.trim()) claims = JSON.parse(saClaimsText);
        } catch (err) {
            showToast('Invalid JSON in additional claims', 'error');
            return;
        }
        claims.aud = saAud.trim();
        const allowedEnvs = saSelectedEnvs.length === 0 ? [] : saSelectedEnvs;

        setAddingSa(true);
        try {
            await api.createServiceAccount(projectName, saIssuerUrl.trim(), claims, allowedEnvs);
            showToast('Service account created successfully', 'success');
            setAddSaOpen(false);
            await loadServiceAccounts();
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to create service account: ${err.message}`, 'error');
        } finally {
            setAddingSa(false);
        }
    };

    // --- Removal flows ---
    const handleRemoveUser = async (email) => {
        const updatedEmails = appUsers.filter(u => u.email !== email).map(u => u.email);
        await api.updateProject(projectName, { app_users: updatedEmails });
        showToast(`Removed app user ${email}`, 'success');
        onProjectUpdated();
    };

    const handleRemoveTeam = async (teamId, teamName) => {
        const updatedTeamIds = appTeams.filter(t => t.id !== teamId).map(t => t.id);
        await api.updateProject(projectName, { app_teams: updatedTeamIds });
        showToast(`Removed app team ${teamName}`, 'success');
        onProjectUpdated();
    };

    const handleRemoveServiceAccount = async (sa) => {
        await api.deleteServiceAccount(projectName, sa.id);
        showToast(`Removed service account ${sa.email || ''}`, 'success');
        await loadServiceAccounts();
        onProjectUpdated();
    };

    const handleConfirmRemove = async () => {
        if (!confirmRemove) return;
        setRemoving(true);
        try {
            if (confirmRemove.kind === 'user') {
                await handleRemoveUser(confirmRemove.email);
            } else if (confirmRemove.kind === 'team') {
                await handleRemoveTeam(confirmRemove.teamId, confirmRemove.name);
            } else if (confirmRemove.kind === 'sa') {
                await handleRemoveServiceAccount(confirmRemove.sa);
            }
            setConfirmRemove(null);
        } catch (err) {
            showToast(`Failed to remove: ${err.message}`, 'error');
        } finally {
            setRemoving(false);
        }
    };

    // --- Unified row model ---
    const rows = [
        ...displayedUsers.map(u => ({
            key: `user-${u.id || u.email}`,
            kind: 'user',
            name: u.email,
            isOwnerFixed: !!u.isOwnerFixed,
            members: null,
            remove: () => setConfirmRemove({ kind: 'user', email: u.email, name: u.email }),
        })),
        ...displayedTeams.map(t => {
            const matched = teams.find(team => team.id === t.id || team.name === t.name);
            const memberCount = matched?.member_count;
            return {
                key: `team-${t.id || t.name}`,
                kind: 'team',
                name: t.name,
                teamName: t.name,
                isOwnerFixed: !!t.isOwnerFixed,
                members: typeof memberCount === 'number' ? memberCount : null,
                remove: () => setConfirmRemove({ kind: 'team', teamId: t.id, name: t.name }),
            };
        }),
        ...serviceAccounts.map(sa => ({
            key: `sa-${sa.id}`,
            kind: 'sa',
            name: sa.email,
            isOwnerFixed: false,
            members: null,
            remove: () => setConfirmRemove({ kind: 'sa', sa, name: sa.email }),
        })),
    ];

    const filterMap = { all: null, users: 'user', teams: 'team', sas: 'sa' };
    const visibleRows = rows.filter(r => {
        const wanted = filterMap[typeFilter];
        return wanted === null || r.kind === wanted;
    });

    const accessClassDescription = accessClasses.find(ac => ac.id === project?.access_class)?.description;

    return (
        <div className="r-stack">
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Owner</div>
                    <div className="r-section-sub">
                        {ownerUserEmail
                            ? `Owned by user ${ownerUserEmail}.`
                            : ownerTeamName
                                ? `Owned by team ${ownerTeamName}.`
                                : 'No owner set.'}
                    </div>
                </div>
                <RButton size="sm" icon="edit" onClick={handleEditOwner}>Transfer ownership</RButton>
            </div>

            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Access class</div>
                    <div className="r-section-sub">
                        {accessClassDescription || 'Controls who can reach this project.'}
                    </div>
                </div>
                {accessClasses.length > 0 && project?.access_class && (
                    <Segmented
                        value={project.access_class}
                        options={accessClasses.map(ac => ({ value: ac.id, label: ac.display_name }))}
                        onChange={handleChangeAccessClass}
                    />
                )}
            </div>

            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Who can access this project</div>
                    <div className="r-section-sub">
                        Users, teams and service accounts with access. The project owner always has access.
                    </div>
                </div>
                <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                    <Segmented
                        value={typeFilter}
                        options={[
                            { value: 'all', label: 'All' },
                            { value: 'users', label: 'Users' },
                            { value: 'teams', label: 'Teams' },
                            { value: 'sas', label: 'Service accounts' },
                        ]}
                        onChange={setTypeFilter}
                    />
                    <AddMenu
                        items={[
                            { label: 'Add user', icon: 'user', onClick: openAddUser },
                            { label: 'Add team', icon: 'users', onClick: openAddTeam },
                            { label: 'Add service account', icon: 'key', onClick: openAddSa },
                        ]}
                    />
                </div>
            </div>

            <Panel>
                {visibleRows.length > 0 ? (
                    <table className="r-table">
                        <thead>
                            <tr>
                                <th>Subject</th>
                                <th>Kind</th>
                                <th>Members</th>
                                <th style={{ textAlign: 'right' }}>Actions</th>
                            </tr>
                        </thead>
                        <tbody>
                            {visibleRows.map(row => {
                                const meta = KIND_META[row.kind];
                                return (
                                    <tr key={row.key}>
                                        <td>
                                            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                                                <Icon name={meta.icon} size={14} />
                                                {row.kind === 'team' ? (
                                                    <a className="r-link" onClick={() => navigate(`/team/${row.teamName}`)}>{row.name}</a>
                                                ) : (
                                                    <span style={{ fontWeight: 500 }}>{row.name}</span>
                                                )}
                                                {row.isOwnerFixed && <Pill>Owner</Pill>}
                                            </span>
                                        </td>
                                        <td><Pill>{meta.label}</Pill></td>
                                        <td>
                                            {row.kind === 'team'
                                                ? (row.members != null ? row.members : <span style={{ color: 'var(--text-soft)' }}>—</span>)
                                                : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                        </td>
                                        <td style={{ textAlign: 'right' }}>
                                            {row.isOwnerFixed ? (
                                                <span style={{ fontSize: 12, color: 'var(--text-soft)' }}>Always has access</span>
                                            ) : (
                                                <RButton variant="danger" size="sm" icon="trash" onClick={row.remove}>
                                                    Remove
                                                </RButton>
                                            )}
                                        </td>
                                    </tr>
                                );
                            })}
                        </tbody>
                    </table>
                ) : (
                    <div className="r-panel-body">
                        <Empty title="No access entries">
                            <div style={{ color: 'var(--text-soft)', fontSize: 13 }}>
                                Use "Add" to grant a user, team or service account access to this project.
                            </div>
                        </Empty>
                    </div>
                )}
            </Panel>

            <RModal
                isOpen={editingOwner}
                onClose={() => setEditingOwner(false)}
                title="Transfer project ownership"
                footer={
                    <>
                        <RButton onClick={() => setEditingOwner(false)} disabled={updatingOwner}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleSaveOwner} loading={updatingOwner}>
                            Transfer ownership
                        </RButton>
                    </>
                }
            >
                <RField label="Owner type">
                    <Segmented
                        value={newOwnerType}
                        onChange={setNewOwnerType}
                        options={[
                            { value: 'user', label: 'User' },
                            { value: 'team', label: 'Team' },
                        ]}
                    />
                </RField>
                {newOwnerType === 'user' ? (
                    <RField label="Owner email">
                        <AutocompleteInput
                            type="email"
                            id="transfer-owner-email"
                            value={newOwnerEmail}
                            onChange={setNewOwnerEmail}
                            options={currentUserEmail ? [currentUserEmail] : []}
                            placeholder="owner@example.com"
                            onEnter={handleSaveOwner}
                        />
                    </RField>
                ) : (
                    <RField label="Owner team">
                        <RCombobox
                            value={newOwnerTeamId}
                            onChange={setNewOwnerTeamId}
                            options={teams.map(t => ({ value: t.id, label: t.name }))}
                            placeholder="Select a team"
                            searchPlaceholder="Search teams…"
                        />
                    </RField>
                )}
            </RModal>

            <RModal
                isOpen={addUserOpen}
                onClose={() => setAddUserOpen(false)}
                title="Add user"
                sub="Grant a user access to this project."
                footer={
                    <>
                        <RButton onClick={() => setAddUserOpen(false)} disabled={addingUser}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleAddUser} loading={addingUser}>Add user</RButton>
                    </>
                }
            >
                <RField label="User email">
                    <RInput
                        type="email"
                        value={newUserEmail}
                        onChange={(e) => setNewUserEmail(e.target.value)}
                        onKeyDown={(e) => { if (e.key === 'Enter') handleAddUser(); }}
                        placeholder="user@example.com"
                        autoFocus
                    />
                </RField>
            </RModal>

            <RModal
                isOpen={addTeamOpen}
                onClose={() => setAddTeamOpen(false)}
                title="Add team"
                sub="Grant a team access to this project."
                footer={
                    <>
                        <RButton onClick={() => setAddTeamOpen(false)} disabled={addingTeam}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleAddTeam} loading={addingTeam} disabled={teams.length === 0}>
                            Add team
                        </RButton>
                    </>
                }
            >
                <RField label="Team">
                    <RCombobox
                        value={selectedTeamId}
                        onChange={setSelectedTeamId}
                        options={teams.map(t => ({ value: t.id, label: t.name }))}
                        placeholder="Select a team"
                        searchPlaceholder="Search teams…"
                    />
                </RField>
            </RModal>

            <RModal
                isOpen={addSaOpen}
                onClose={() => setAddSaOpen(false)}
                title="Add service account"
                sub="Service accounts grant workload identity access for CI/CD or external systems."
                footer={
                    <>
                        <RButton onClick={() => setAddSaOpen(false)} disabled={addingSa}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleAddServiceAccount} loading={addingSa}>
                            Create service account
                        </RButton>
                    </>
                }
            >
                {environments.length > 0 && (
                    <RField label="Allowed environments" hint="Leave empty to allow all environments.">
                        <RCombobox
                            multi
                            value={saSelectedEnvs}
                            onChange={setSaSelectedEnvs}
                            options={environments.map(env => ({ value: env.name, label: env.name }))}
                            placeholder="All environments"
                            searchPlaceholder="Search environments…"
                        />
                    </RField>
                )}
                <RField label="Issuer URL">
                    <RInput
                        value={saIssuerUrl}
                        onChange={(e) => setSaIssuerUrl(e.target.value)}
                        placeholder="https://token.actions.githubusercontent.com"
                        autoFocus
                    />
                </RField>
                <RField label="Audience (aud)">
                    <RInput
                        value={saAud}
                        onChange={(e) => setSaAud(e.target.value)}
                        placeholder={CONFIG.backendUrl}
                    />
                </RField>
                <RField
                    label="Additional claims (JSON)"
                    hint={<>Provide a JSON object. The <span className="mono">aud</span> claim is configured separately above.</>}
                >
                    <Suspense fallback={<JsonEditorFallback />}>
                        <JsonEditor
                            value={saClaimsText}
                            onChange={setSaClaimsText}
                            minHeight="120px"
                            ariaLabel="Additional claims JSON"
                        />
                    </Suspense>
                </RField>
            </RModal>

            <RConfirmDialog
                isOpen={!!confirmRemove}
                onClose={() => setConfirmRemove(null)}
                onConfirm={handleConfirmRemove}
                title="Remove access"
                message={
                    confirmRemove
                        ? `Remove ${KIND_META[confirmRemove.kind].label.toLowerCase()} "${confirmRemove.name}" from this project?`
                        : ''
                }
                confirmText="Remove"
                confirmTone="danger"
                loading={removing}
            />
        </div>
    );
}
