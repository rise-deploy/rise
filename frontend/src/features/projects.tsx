// @ts-nocheck
import { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { useToast } from '../components/toast';
import { AutocompleteInput } from '../components/ui';
import { Button as RButton, Combobox as RCombobox, Empty, Field as RField, Input as RInput, Modal as RModal, Panel, Pill, Segmented } from '../components/r-ui';

// Access tab component - manages access class and app-level user/team access
export function AppUsersList({ projectName, project, accessClasses, currentUserEmail, onProjectUpdated }) {
    const [newUserEmail, setNewUserEmail] = useState('');
    const [selectedTeamId, setSelectedTeamId] = useState('');
    const [teams, setTeams] = useState([]);
    const [editingOwner, setEditingOwner] = useState(false);
    const [newOwnerType, setNewOwnerType] = useState('user');
    const [newOwnerEmail, setNewOwnerEmail] = useState('');
    const [newOwnerTeamId, setNewOwnerTeamId] = useState('');
    const [updatingOwner, setUpdatingOwner] = useState(false);
    const { showToast } = useToast();

    const appUsers = project?.app_users || [];
    const appTeams = project?.app_teams || [];
    const owner = project?.owner || null;

    const ownerUserEmail = owner?.email ? owner.email.trim() : null;
    const ownerTeamName = owner?.name ? owner.name.trim() : null;
    const ownerTeamId = owner?.id || null;

    const displayedUsers = (() => {
        if (!ownerUserEmail) return appUsers;
        const ownerEmailLower = ownerUserEmail.toLowerCase();
        const nonOwnerUsers = appUsers.filter((u) => (u.email || '').toLowerCase() !== ownerEmailLower);
        return [{ id: owner?.id || `owner-user-${ownerUserEmail}`, email: ownerUserEmail, isOwnerFixed: true }, ...nonOwnerUsers];
    })();

    const displayedTeams = (() => {
        if (!ownerTeamName) return appTeams;

        const nonOwnerTeams = appTeams.filter((t) => {
            if (ownerTeamId && t.id) return t.id !== ownerTeamId;
            return (t.name || '').toLowerCase() !== ownerTeamName.toLowerCase();
        });

        return [{ id: ownerTeamId || `owner-team-${ownerTeamName}`, name: ownerTeamName, isOwnerFixed: true }, ...nonOwnerTeams];
    })();

    useEffect(() => {
        async function loadTeams() {
            try {
                const data = await api.getTeams();
                setTeams(data || []);
            } catch (err) {
                console.error('Failed to load teams:', err);
            }
        }
        loadTeams();
    }, []);

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

    const handleAddUser = async () => {
        if (!newUserEmail.trim()) {
            showToast('User email is required', 'error');
            return;
        }

        try {
            const currentUserEmails = appUsers.map(u => u.email);
            await api.updateProject(projectName, {
                app_users: [...currentUserEmails, newUserEmail.trim()]
            });
            showToast(`Added app user ${newUserEmail}`, 'success');
            setNewUserEmail('');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app user: ${err.message}`, 'error');
        }
    };

    const handleRemoveUser = async (email) => {
        try {
            const updatedEmails = appUsers.filter(u => u.email !== email).map(u => u.email);
            await api.updateProject(projectName, { app_users: updatedEmails });
            showToast(`Removed app user ${email}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to remove app user: ${err.message}`, 'error');
        }
    };

    const handleAddTeam = async () => {
        if (!selectedTeamId) {
            showToast('Team selection is required', 'error');
            return;
        }

        const selectedTeam = teams.find(t => t.id === selectedTeamId);
        try {
            const currentTeamIds = appTeams.map(t => t.id);
            await api.updateProject(projectName, {
                app_teams: [...currentTeamIds, selectedTeamId]
            });
            showToast(`Added app team ${selectedTeam?.name || ''}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to add app team: ${err.message}`, 'error');
        }
    };

    const handleRemoveTeam = async (teamId, teamName) => {
        try {
            const updatedTeamIds = appTeams.filter(t => t.id !== teamId).map(t => t.id);
            await api.updateProject(projectName, { app_teams: updatedTeamIds });
            showToast(`Removed app team ${teamName}`, 'success');
            onProjectUpdated();
        } catch (err) {
            showToast(`Failed to remove app team: ${err.message}`, 'error');
        }
    };

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

            <p style={{ fontSize: 12.5, color: 'var(--text-muted)', margin: 0 }}>
                The project owner always has access and is shown as a fixed entry.
            </p>

            <div className="r-grid-2-1" style={{ gridTemplateColumns: 'repeat(2, 1fr)' }}>
                <Panel>
                    <div className="r-panel-head">
                        <div className="r-section-head" style={{ marginBottom: 0, flex: 1 }}>
                            <div className="r-section-title">Users</div>
                            <Pill>{displayedUsers.length}</Pill>
                        </div>
                    </div>
                    {displayedUsers.length > 0 ? (
                        <table className="r-table">
                            <thead>
                                <tr>
                                    <th>Email</th>
                                    <th style={{ textAlign: 'right' }}>Actions</th>
                                </tr>
                            </thead>
                            <tbody>
                                {displayedUsers.map(user => (
                                    <tr key={user.id}>
                                        <td>
                                            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                                                <span>{user.email}</span>
                                                {user.isOwnerFixed && <Pill>Owner</Pill>}
                                            </span>
                                        </td>
                                        <td style={{ textAlign: 'right' }}>
                                            {user.isOwnerFixed ? (
                                                <span style={{ fontSize: 12, color: 'var(--text-soft)' }}>Always has access</span>
                                            ) : (
                                                <RButton variant="danger" size="sm" onClick={() => handleRemoveUser(user.email)}>
                                                    Remove
                                                </RButton>
                                            )}
                                        </td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    ) : (
                        <div className="r-panel-body"><Empty title="No users" /></div>
                    )}
                    <div className="r-panel-body" style={{ display: 'flex', gap: 8 }}>
                        <RInput
                            type="email"
                            value={newUserEmail}
                            onChange={(e) => setNewUserEmail(e.target.value)}
                            onKeyDown={(e) => { if (e.key === 'Enter') handleAddUser(); }}
                            placeholder="user@example.com"
                            style={{ flex: 1 }}
                        />
                        <RButton variant="primary" onClick={handleAddUser}>Add</RButton>
                    </div>
                </Panel>

                <Panel>
                    <div className="r-panel-head">
                        <div className="r-section-head" style={{ marginBottom: 0, flex: 1 }}>
                            <div className="r-section-title">Teams</div>
                            <Pill>{displayedTeams.length}</Pill>
                        </div>
                    </div>
                    {displayedTeams.length > 0 ? (
                        <table className="r-table">
                            <thead>
                                <tr>
                                    <th>Name</th>
                                    <th style={{ textAlign: 'right' }}>Actions</th>
                                </tr>
                            </thead>
                            <tbody>
                                {displayedTeams.map(team => (
                                    <tr key={team.id}>
                                        <td>
                                            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                                                <a className="r-link" onClick={() => navigate(`/team/${team.name}`)}>{team.name}</a>
                                                {team.isOwnerFixed && <Pill>Owner</Pill>}
                                            </span>
                                        </td>
                                        <td style={{ textAlign: 'right' }}>
                                            {team.isOwnerFixed ? (
                                                <span style={{ fontSize: 12, color: 'var(--text-soft)' }}>Always has access</span>
                                            ) : (
                                                <RButton variant="danger" size="sm" onClick={() => handleRemoveTeam(team.id, team.name)}>
                                                    Remove
                                                </RButton>
                                            )}
                                        </td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    ) : (
                        <div className="r-panel-body"><Empty title="No teams" /></div>
                    )}
                    <div className="r-panel-body" style={{ display: 'flex', gap: 8 }}>
                        <div style={{ flex: 1 }}>
                            <RCombobox
                                value={selectedTeamId}
                                onChange={setSelectedTeamId}
                                options={teams.map(t => ({ value: t.id, label: t.name }))}
                                placeholder="Search teams…"
                            />
                        </div>
                        <RButton variant="primary" onClick={handleAddTeam} disabled={teams.length === 0}>Add</RButton>
                    </div>
                </Panel>
            </div>

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
        </div>
    );
}
