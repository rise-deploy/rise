// @ts-nocheck
import { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatDate } from '../lib/utils';
import { useToast } from '../components/toast';
import { AutocompleteInput } from '../components/r-ui';
import { ProjectTable } from '../components/project-table';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { useRowKeyboardNavigation, useSortableData } from '../lib/table';
import { Alert, ConfirmDialog as RConfirmDialog, Panel, PanelBody, PanelHead, Button as RButton, Empty, Field as RField, Input as RInput, Modal as RModal, Pill, Tooltip, colorFor } from '../components/r-ui';
import { AddMenu, Menu, RosterTable } from '../components/roster-table';
import { Icon } from '../components/icon';


// Teams List Component
export function TeamsList({ currentUser, openCreate = false }) {
    const [teams, setTeams] = useState([]);
    const [projects, setProjects] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [formData, setFormData] = useState({ name: '', members: '', owners: '' });
    const [saving, setSaving] = useState(false);
    const [actionStatus, setActionStatus] = useState('');
    const { showToast } = useToast();
    const { sortedItems: sortedTeams, sortKey, sortDirection, requestSort } = useSortableData(teams, 'name');
    const { activeIndex, setActiveIndex, onKeyDown } = useRowKeyboardNavigation(
        (idx) => {
            const team = sortedTeams[idx];
            if (team) navigate(`/team/${team.name}`);
        },
        sortedTeams.length
    );

    const loadTeams = useCallback(async () => {
        try {
            const data = await api.getTeams();
            setTeams(data);
        } catch (err) {
            setError(err.message);
        } finally {
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        loadTeams();
        api.getProjects().then(setProjects).catch(() => {});
    }, [loadTeams]);

    const handleCreateClick = () => {
        setFormData({ name: '', members: '', owners: currentUser?.email || '' });
        setIsModalOpen(true);
    };

    useEffect(() => {
        if (!openCreate) return;
        handleCreateClick();
        window.history.replaceState({}, '', window.location.pathname);
    }, [openCreate, currentUser?.email]);

    const handleCreate = async () => {
        if (!formData.name) {
            showToast('Team name is required', 'error');
            return;
        }

        // Parse comma-separated email lists
        const memberEmails = formData.members
            .split(',')
            .map(e => e.trim())
            .filter(e => e.length > 0);

        const ownerEmails = formData.owners
            .split(',')
            .map(e => e.trim())
            .filter(e => e.length > 0);

        if (ownerEmails.length === 0) {
            showToast('At least one owner is required', 'error');
            return;
        }

        setSaving(true);
        setActionStatus(`Creating team ${formData.name}...`);
        try {
            // Look up user IDs for owners and members
            const ownerLookup = await api.lookupUsers(ownerEmails);
            const memberLookup = memberEmails.length > 0 ? await api.lookupUsers(memberEmails) : { users: [] };

            if (!ownerLookup.users || ownerLookup.users.length !== ownerEmails.length) {
                showToast('One or more owner email addresses not found', 'error');
                setSaving(false);
                return;
            }

            if (memberEmails.length > 0 && (!memberLookup.users || memberLookup.users.length !== memberEmails.length)) {
                showToast('One or more member email addresses not found', 'error');
                setSaving(false);
                return;
            }

            const ownerIds = ownerLookup.users.map(u => u.id);
            const memberIds = memberLookup.users.map(u => u.id);

            await api.createTeam(formData.name, memberIds, ownerIds);
            showToast(`Team ${formData.name} created successfully`, 'success');
            setActionStatus(`Created team ${formData.name}.`);
            setIsModalOpen(false);
            loadTeams();
            window.dispatchEvent(new Event('rise:mutation'));
        } catch (err) {
            showToast(`Failed to create team: ${err.message}`, 'error');
            setActionStatus(`Failed to create team ${formData.name}.`);
        } finally {
            setSaving(false);
        }
    };

    if (loading) return <LoadingState label="Loading teams..." />;
    if (error) return <ErrorState message={`Error loading teams: ${error}`} onRetry={loadTeams} />;

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">Teams</h1>
                    <div className="r-page-sub">
                        {sortedTeams.length} team{sortedTeams.length === 1 ? '' : 's'} ·{' '}
                        {sortedTeams.reduce((n, t) => n + (t.members?.length || 0), 0)} members total
                    </div>
                </div>
                {currentUser?.can_create_teams && (
                    <RButton variant="primary" icon="plus" onClick={handleCreateClick}>
                        New team
                    </RButton>
                )}
            </div>
            {actionStatus && <p className="mono-inline-status mb-3">{actionStatus}</p>}
            {sortedTeams.length === 0 ? (
                <Empty title="No teams yet">Create a team to share project ownership across people.</Empty>
            ) : (
                <div className="r-grid-2">
                    {sortedTeams.map(t => (
                        <TeamCard
                            key={t.id}
                            team={t}
                            projectCount={countProjectsForTeam(projects, t)}
                            isOwner={!!currentUser && (t.owners || []).some(o => o.email === currentUser.email)}
                            onOpen={() => navigate(`/team/${t.name}`)}
                        />
                    ))}
                </div>
            )}

            <RModal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title="Create Team"
                footer={
                    <>
                        <RButton onClick={() => setIsModalOpen(false)} disabled={saving}>
                            Cancel
                        </RButton>
                        <RButton variant="primary" onClick={handleCreate} loading={saving}>
                            Create
                        </RButton>
                    </>
                }
            >
                <RField label="Team Name">
                    <RInput
                        id="team-name"
                        value={formData.name}
                        onChange={(e) => setFormData({ ...formData, name: e.target.value })}
                        placeholder="engineering"
                        autoFocus
                    />
                </RField>

                <RField
                    label="Owners (emails, comma-separated)"
                    hint="Owners can manage the team. At least one owner is required."
                >
                    <AutocompleteInput
                        id="team-owners"
                        type="email"
                        value={formData.owners}
                        onChange={(next) => setFormData({ ...formData, owners: next })}
                        options={currentUser?.email ? [currentUser.email] : []}
                        placeholder="alice@example.com, bob@example.com"
                        multiValue
                    />
                </RField>

                <RField
                    label="Members (emails, comma-separated)"
                    hint="Members can use the team for project ownership."
                >
                    <AutocompleteInput
                        id="team-members"
                        type="email"
                        value={formData.members}
                        onChange={(next) => setFormData({ ...formData, members: next })}
                        options={currentUser?.email ? [currentUser.email] : []}
                        placeholder="charlie@example.com, dana@example.com"
                        multiValue
                    />
                </RField>
            </RModal>
        </section>
    );
}

// Counts projects owned by a given team across the project list. We match by
// id when available (preferred), then fall back to comparing names so that
// callers with a partially populated project list (no owner.id) still work.
function countProjectsForTeam(projects, team) {
    if (!Array.isArray(projects) || projects.length === 0) return 0;
    return projects.filter(p => {
        const owner = p.owner;
        if (!owner) return false;
        if (owner.id && team.id) return owner.id === team.id;
        if (owner.name && team.name) return owner.name === team.name;
        return false;
    }).length;
}

// A labelled, overlapping stack of user avatars; each avatar shows the user's
// email on hover.
function AvatarGroup({ label, users }) {
    const visible = users.slice(0, 8);
    const overflow = Math.max(0, users.length - visible.length);
    return (
        <div className="r-meta-bar" style={{ marginBottom: 0 }}>
            <span style={{ color: 'var(--text-soft)' }}>{label}</span>
            <span className="dot-sep" />
            {users.length === 0 ? (
                <span style={{ color: 'var(--text-soft)' }}>None</span>
            ) : (
                <div className="r-member-stack">
                    {visible.map(u => {
                        const email = u.email || u.name || '?';
                        return (
                            <Tooltip key={u.id || email} content={email}>
                                <span
                                    className="r-ava-sm"
                                    style={{ background: colorFor(email), width: 22, height: 22, fontSize: 10 }}
                                >
                                    {email.trim()[0]?.toUpperCase() || '?'}
                                </span>
                            </Tooltip>
                        );
                    })}
                    {overflow > 0 && (
                        <span
                            className="r-ava-sm"
                            style={{ background: 'var(--surface-2)', color: 'var(--text-muted)', width: 22, height: 22, fontSize: 10 }}
                        >
                            +{overflow}
                        </span>
                    )}
                </div>
            )}
        </div>
    );
}

function TeamCard({ team, projectCount, onOpen, isOwner }) {
    const members = team.members || [];
    const owners = team.owners || [];

    return (
        <Panel onClick={onOpen} className={isOwner ? 'r-panel-own' : undefined}>
            <PanelHead>
                <div style={{ minWidth: 0 }}>
                    <div className="r-panel-title" style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                        <span>{team.name}</span>
                        {team.idp_managed && (
                            <span className="r-pill accent" style={{ fontSize: 10.5, padding: '1px 6px' }}>IDP</span>
                        )}
                    </div>
                    <div className="r-panel-sub">
                        {members.length} member{members.length === 1 ? '' : 's'}
                        {' · '}{projectCount} project{projectCount === 1 ? '' : 's'}
                    </div>
                </div>
            </PanelHead>
            <PanelBody>
                <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                    <AvatarGroup label="Owners" users={owners} />
                    <AvatarGroup label="Members" users={members} />
                </div>
            </PanelBody>
        </Panel>
    );
}

// Team Detail Component
export function TeamDetail({ teamName, currentUser }) {
    const [team, setTeam] = useState(null);
    const [teamProjects, setTeamProjects] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [newOwnerEmail, setNewOwnerEmail] = useState('');
    const [newMemberEmail, setNewMemberEmail] = useState('');
    const [addOwnerOpen, setAddOwnerOpen] = useState(false);
    const [addMemberOpen, setAddMemberOpen] = useState(false);
    const [addingOwner, setAddingOwner] = useState(false);
    const [addingMember, setAddingMember] = useState(false);
    const [roleFilter, setRoleFilter] = useState('all');
    const [deleteDialogOpen, setDeleteDialogOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const { showToast } = useToast();

    const loadTeam = useCallback(async () => {
        try {
            const [data, projects] = await Promise.all([
                api.getTeam(teamName),
                api.getTeamProjects(teamName),
            ]);
            setTeam(data);
            setTeamProjects(projects || []);
        } catch (err) {
            setError(err.message);
        } finally {
            setLoading(false);
        }
    }, [teamName]);

    useEffect(() => {
        loadTeam();
    }, [loadTeam]);

    // Check if user can manage this team
    const canManage = currentUser && team && (
        currentUser.is_admin ||
        (team.owners && team.owners.some(o => o.email === currentUser.email))
    );

    // IDP-managed teams can only be managed by admins
    const canEdit = canManage && (!team?.idp_managed || currentUser?.is_admin);

    const handleAddOwner = async () => {
        if (!newOwnerEmail.trim()) {
            showToast('Please enter an email address', 'error');
            return;
        }

        setAddingOwner(true);
        try {
            // Look up user ID by email
            const lookupResult = await api.lookupUsers([newOwnerEmail.trim()]);
            if (!lookupResult.users || lookupResult.users.length === 0) {
                showToast(`User with email ${newOwnerEmail} not found`, 'error');
                return;
            }

            const currentOwnerIds = team.owners?.map(o => o.id) || [];
            const newOwnerId = lookupResult.users[0].id;

            await api.updateTeam(team.id, {
                owners: [...currentOwnerIds, newOwnerId]
            });
            showToast(`Added ${newOwnerEmail} as owner`, 'success');
            setNewOwnerEmail('');
            setAddOwnerOpen(false);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to add owner: ${err.message}`, 'error');
        } finally {
            setAddingOwner(false);
        }
    };

    const handleRemoveOwner = async (ownerId, email) => {
        try {
            const currentOwnerIds = team.owners?.map(o => o.id) || [];
            const updatedOwnerIds = currentOwnerIds.filter(id => id !== ownerId);

            if (updatedOwnerIds.length === 0) {
                showToast('Cannot remove last owner', 'error');
                return;
            }

            await api.updateTeam(team.id, { owners: updatedOwnerIds });
            showToast(`Removed ${email} from owners`, 'success');
            await loadTeam();
        } catch (err) {
            showToast(`Failed to remove owner: ${err.message}`, 'error');
        }
    };

    const handleAddMember = async () => {
        if (!newMemberEmail.trim()) {
            showToast('Please enter an email address', 'error');
            return;
        }

        setAddingMember(true);
        try {
            // Look up user ID by email
            const lookupResult = await api.lookupUsers([newMemberEmail.trim()]);
            if (!lookupResult.users || lookupResult.users.length === 0) {
                showToast(`User with email ${newMemberEmail} not found`, 'error');
                return;
            }

            const currentMemberIds = team.members?.map(m => m.id) || [];
            const newMemberId = lookupResult.users[0].id;

            await api.updateTeam(team.id, {
                members: [...currentMemberIds, newMemberId]
            });
            showToast(`Added ${newMemberEmail} as member`, 'success');
            setNewMemberEmail('');
            setAddMemberOpen(false);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to add member: ${err.message}`, 'error');
        } finally {
            setAddingMember(false);
        }
    };

    const handleRemoveMember = async (memberId, email) => {
        try {
            const currentMemberIds = team.members?.map(m => m.id) || [];
            const updatedMemberIds = currentMemberIds.filter(id => id !== memberId);
            await api.updateTeam(team.id, { members: updatedMemberIds });
            showToast(`Removed ${email} from members`, 'success');
            await loadTeam();
        } catch (err) {
            showToast(`Failed to remove member: ${err.message}`, 'error');
        }
    };

    const handleDeleteTeam = async () => {
        setDeleting(true);
        try {
            await api.deleteTeam(team.id);
            showToast(`Team ${team.name} deleted successfully`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            navigate('/teams');
        } catch (err) {
            showToast(`Failed to delete team: ${err.message}`, 'error');
            setDeleting(false);
        }
    };

    if (loading) return <LoadingState label="Loading team..." />;
    if (error) return <ErrorState message={`Error loading team: ${error}`} onRetry={loadTeam} />;
    if (!team) return <EmptyState message="Team not found." />;

    // --- Unified members roster ---
    // A user can be both an owner and a member; collapse those into one row.
    const owners = team.owners || [];
    const members = team.members || [];
    const peopleMap = new Map();
    const keyOf = (u) => u.id || u.email;
    for (const o of owners) {
        const k = keyOf(o);
        if (!peopleMap.has(k)) peopleMap.set(k, { ...o, isOwner: false, isMember: false });
        peopleMap.get(k).isOwner = true;
    }
    for (const m of members) {
        const k = keyOf(m);
        if (!peopleMap.has(k)) peopleMap.set(k, { ...m, isOwner: false, isMember: false });
        peopleMap.get(k).isMember = true;
    }
    const visiblePeople = [...peopleMap.values()].filter(p => {
        if (roleFilter === 'owners') return p.isOwner;
        if (roleFilter === 'members') return p.isMember;
        return true;
    });
    const rosterRows = visiblePeople.map(p => {
        const kinds = [];
        if (p.isOwner) kinds.push('Owner');
        if (p.isMember) kinds.push('Member');
        let actions = null;
        if (canEdit) {
            if (p.isOwner && p.isMember) {
                actions = (
                    <Menu
                        items={[
                            { label: 'Remove as owner', icon: 'trash', onClick: () => handleRemoveOwner(p.id, p.email) },
                            { label: 'Remove as member', icon: 'trash', onClick: () => handleRemoveMember(p.id, p.email) },
                        ]}
                        trigger={({ toggle }) => (
                            <RButton variant="danger" size="sm" icon="trash" onClick={toggle}>
                                Remove<Icon name="chevd" size={12} />
                            </RButton>
                        )}
                    />
                );
            } else {
                const removeRole = p.isOwner ? 'owner' : 'member';
                actions = (
                    <RButton
                        variant="danger"
                        size="sm"
                        icon="trash"
                        onClick={() => removeRole === 'owner'
                            ? handleRemoveOwner(p.id, p.email)
                            : handleRemoveMember(p.id, p.email)}
                    >
                        Remove
                    </RButton>
                );
            }
        }
        return {
            key: `person-${keyOf(p)}`,
            icon: 'user',
            name: <span style={{ fontWeight: 500 }}>{p.email}</span>,
            kindLabel: kinds,
            actions,
        };
    });

    const isTeamOwnedProject = (project) => {
        const owner = project.owner;
        if (!owner) return false;
        if (team.id && owner.id) return owner.id === team.id;
        return !owner.email && owner.name === team.name;
    };

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title" style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                        <span>{team.name}</span>
                        {team.idp_managed && <Pill kind="accent">IDP</Pill>}
                    </h1>
                    <div className="r-page-sub">Updated {formatDate(team.updated)}</div>
                </div>
                {canEdit && (
                    <RButton variant="danger" onClick={() => setDeleteDialogOpen(true)}>
                        Delete
                    </RButton>
                )}
            </div>

            {team.idp_managed && !currentUser?.is_admin && (
                <div style={{ marginBottom: 20 }}>
                    <Alert tone="info" icon="info">
                        This team is managed by your identity provider and can only be modified by administrators.
                    </Alert>
                </div>
            )}

            <div className="r-stack">
                <RosterTable
                    title="People"
                    sub="Owners can manage the team. Members can use it for project ownership."
                    addControl={canEdit ? (
                        <AddMenu
                            items={[
                                { label: 'Add owner', icon: 'user', onClick: () => { setNewOwnerEmail(''); setAddOwnerOpen(true); } },
                                { label: 'Add member', icon: 'user', onClick: () => { setNewMemberEmail(''); setAddMemberOpen(true); } },
                            ]}
                        />
                    ) : undefined}
                    filter={{
                        value: roleFilter,
                        options: [
                            { value: 'all', label: 'All' },
                            { value: 'owners', label: 'Owners' },
                            { value: 'members', label: 'Members' },
                        ],
                        onChange: setRoleFilter,
                    }}
                    rows={rosterRows}
                    emptyText="No people in this team"
                />

                <div>
                    <div style={{ marginBottom: 14 }}>
                        <div className="r-section-title">Projects ({teamProjects.length})</div>
                        {teamProjects.length > 0 && (
                            <div className="r-section-sub">
                                Highlighted rows are owned by this team. The rest are shared with it via project access.
                            </div>
                        )}
                    </div>
                    <ProjectTable
                        projects={teamProjects.slice().sort((a, b) => a.name.localeCompare(b.name))}
                        onRowClick={(project) => navigate(`/project/${project.name}`)}
                        emptyText="No projects owned by or shared with this team"
                        isOwnRow={isTeamOwnedProject}
                    />
                </div>
            </div>

            <RModal
                isOpen={addOwnerOpen}
                onClose={() => setAddOwnerOpen(false)}
                title="Add owner"
                sub="Owners can manage this team."
                footer={
                    <>
                        <RButton onClick={() => setAddOwnerOpen(false)} disabled={addingOwner}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleAddOwner} loading={addingOwner}>Add owner</RButton>
                    </>
                }
            >
                <RField label="Owner email">
                    <AutocompleteInput
                        type="email"
                        id="add-owner-email"
                        value={newOwnerEmail}
                        onChange={setNewOwnerEmail}
                        options={currentUser?.email ? [currentUser.email] : []}
                        placeholder="owner@example.com"
                        onEnter={handleAddOwner}
                    />
                </RField>
            </RModal>

            <RModal
                isOpen={addMemberOpen}
                onClose={() => setAddMemberOpen(false)}
                title="Add member"
                sub="Members can use this team for project ownership."
                footer={
                    <>
                        <RButton onClick={() => setAddMemberOpen(false)} disabled={addingMember}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleAddMember} loading={addingMember}>Add member</RButton>
                    </>
                }
            >
                <RField label="Member email">
                    <AutocompleteInput
                        type="email"
                        id="add-member-email"
                        value={newMemberEmail}
                        onChange={setNewMemberEmail}
                        options={currentUser?.email ? [currentUser.email] : []}
                        placeholder="member@example.com"
                        onEnter={handleAddMember}
                    />
                </RField>
            </RModal>

            <RConfirmDialog
                isOpen={deleteDialogOpen}
                onClose={() => setDeleteDialogOpen(false)}
                onConfirm={handleDeleteTeam}
                title="Delete Team"
                message={`Delete team "${team.name}"? Impact: projects owned by this team may lose expected ownership workflows.`}
                confirmText="Delete Team"
                confirmTone="danger"
                requireText={team.name}
                loading={deleting}
            />
        </section>
    );
}
