// @ts-nocheck
import { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatDate } from '../lib/utils';
import { useToast } from '../components/toast';
import { AutocompleteInput, Button, ConfirmDialog } from '../components/ui';
import { ProjectTable } from '../components/project-table';
import { EmptyState, ErrorState, LoadingState } from '../components/states';
import { useRowKeyboardNavigation, useSortableData } from '../lib/table';
import { Alert, Panel, PanelBody, PanelHead, Button as RButton, Empty, Field as RField, Input as RInput, Modal as RModal, Pill, colorFor } from '../components/r-ui';
import { AddMenu, RosterTable } from '../components/roster-table';


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
                    <Button variant="primary" onClick={handleCreateClick}>
                        New team
                    </Button>
                )}
            </div>
            {actionStatus && <p className="mono-inline-status mb-3">{actionStatus}</p>}
            {sortedTeams.length === 0 ? (
                <Empty title="No teams yet">Create a team to share project ownership across people.</Empty>
            ) : (
                <div className="r-grid-2">
                    {sortedTeams.map(t => (
                        <TeamCard key={t.id} team={t} projectCount={countProjectsForTeam(projects, t)} onOpen={() => navigate(`/team/${t.name}`)} />
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

function TeamCard({ team, projectCount, onOpen }) {
    const members = team.members || [];
    const owners = team.owners || [];
    const leadEmail = owners[0]?.email || members[0]?.email;
    const visibleMembers = members.slice(0, 8);
    const overflow = Math.max(0, members.length - visibleMembers.length);

    return (
        <Panel>
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
                <RButton size="sm" onClick={onOpen}>Manage</RButton>
            </PanelHead>
            <PanelBody>
                {leadEmail && (
                    <div className="r-meta-bar" style={{ marginBottom: 14 }}>
                        <span style={{ color: 'var(--text-soft)' }}>Lead</span>
                        <span className="dot-sep" />
                        <a className="r-link">{leadEmail}</a>
                    </div>
                )}
                {visibleMembers.length > 0 ? (
                    <div className="r-member-stack">
                        {visibleMembers.map(m => {
                            const label = m.email || m.name || '?';
                            return (
                                <span
                                    key={m.id || label}
                                    className="r-ava-sm"
                                    style={{ background: colorFor(label), width: 22, height: 22, fontSize: 10 }}
                                    title={label}
                                >
                                    {label.trim()[0]?.toUpperCase() || '?'}
                                </span>
                            );
                        })}
                        {overflow > 0 && (
                            <span
                                className="r-ava-sm"
                                style={{ background: 'var(--surface-2)', color: 'var(--text-muted)', width: 22, height: 22, fontSize: 10 }}
                                title={`${overflow} more`}
                            >
                                +{overflow}
                            </span>
                        )}
                    </div>
                ) : (
                    <div style={{ color: 'var(--text-soft)', fontSize: 12.5 }}>No members yet</div>
                )}
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
                api.getProjects(),
            ]);
            setTeam(data);
            const ownedProjects = (projects || []).filter((p) => {
                if (p.owner?.name && p.owner.name === data.name) return true;
                if (p.owner?.id && data.id && p.owner.id === data.id) return true;
                return false;
            });
            setTeamProjects(ownedProjects);
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
            setDetailActionStatus(`Adding owner ${newOwnerEmail}...`);
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
            setDetailActionStatus(`Added owner ${newOwnerEmail}.`);
            setNewOwnerEmail('');
            setAddOwnerOpen(false);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to add owner: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to add owner ${newOwnerEmail}.`);
        } finally {
            setAddingOwner(false);
        }
    };

    const handleRemoveOwner = async (ownerId, email) => {
        try {
            setDetailActionStatus(`Removing owner ${email}...`);
            const currentOwnerIds = team.owners?.map(o => o.id) || [];
            const updatedOwnerIds = currentOwnerIds.filter(id => id !== ownerId);

            if (updatedOwnerIds.length === 0) {
                showToast('Cannot remove last owner', 'error');
                return;
            }

            await api.updateTeam(team.id, { owners: updatedOwnerIds });
            showToast(`Removed ${email} from owners`, 'success');
            setDetailActionStatus(`Removed owner ${email}.`);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to remove owner: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to remove owner ${email}.`);
        }
    };

    const handleAddMember = async () => {
        if (!newMemberEmail.trim()) {
            showToast('Please enter an email address', 'error');
            return;
        }

        setAddingMember(true);
        try {
            setDetailActionStatus(`Adding member ${newMemberEmail}...`);
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
            setDetailActionStatus(`Added member ${newMemberEmail}.`);
            setNewMemberEmail('');
            setAddMemberOpen(false);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to add member: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to add member ${newMemberEmail}.`);
        } finally {
            setAddingMember(false);
        }
    };

    const handleRemoveMember = async (memberId, email) => {
        try {
            setDetailActionStatus(`Removing member ${email}...`);
            const currentMemberIds = team.members?.map(m => m.id) || [];
            const updatedMemberIds = currentMemberIds.filter(id => id !== memberId);
            await api.updateTeam(team.id, { members: updatedMemberIds });
            showToast(`Removed ${email} from members`, 'success');
            setDetailActionStatus(`Removed member ${email}.`);
            await loadTeam();
        } catch (err) {
            showToast(`Failed to remove member: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to remove member ${email}.`);
        }
    };

    const handleDeleteTeam = async () => {
        setDeleting(true);
        setDetailActionStatus(`Deleting team ${team.name}...`);
        try {
            await api.deleteTeam(team.id);
            showToast(`Team ${team.name} deleted successfully`, 'success');
            setDetailActionStatus(`Deleted team ${team.name}.`);
            navigate('/teams');
        } catch (err) {
            showToast(`Failed to delete team: ${err.message}`, 'error');
            setDetailActionStatus(`Failed to delete team ${team.name}.`);
            setDeleting(false);
        }
    };

    if (loading) return <LoadingState label="Loading team..." />;
    if (error) return <ErrorState message={`Error loading team: ${error}`} onRetry={loadTeam} />;
    if (!team) return <EmptyState message="Team not found." />;

    // --- Unified members roster ---
    const owners = team.owners || [];
    const members = team.members || [];
    const allRows = [
        ...owners.map(o => ({ ...o, role: 'owner' })),
        ...members.map(m => ({ ...m, role: 'member' })),
    ];
    const visiblePeople = allRows.filter(p => {
        if (roleFilter === 'owners') return p.role === 'owner';
        if (roleFilter === 'members') return p.role === 'member';
        return true;
    });
    const rosterRows = visiblePeople.map(p => ({
        key: `${p.role}-${p.id || p.email}`,
        icon: 'user',
        name: <span style={{ fontWeight: 500 }}>{p.email}</span>,
        kindLabel: p.role === 'owner' ? 'Owner' : 'Member',
        actions: canEdit ? (
            <RButton
                variant="danger"
                size="sm"
                icon="trash"
                onClick={() => p.role === 'owner'
                    ? handleRemoveOwner(p.id, p.email)
                    : handleRemoveMember(p.id, p.email)}
            >
                Remove
            </RButton>
        ) : null,
    }));

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
                    <div className="r-section-title" style={{ marginBottom: 14 }}>
                        Projects ({teamProjects.length})
                    </div>
                    {teamProjects.length > 0 ? (
                        <ProjectTable
                            projects={teamProjects.slice().sort((a, b) => a.name.localeCompare(b.name))}
                            onRowClick={(project) => navigate(`/project/${project.name}`)}
                        />
                    ) : (
                        <Panel>
                            <div className="r-panel-body">
                                <Empty title="No projects owned by this team" />
                            </div>
                        </Panel>
                    )}
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

            <ConfirmDialog
                isOpen={deleteDialogOpen}
                onClose={() => setDeleteDialogOpen(false)}
                onConfirm={handleDeleteTeam}
                title="Delete Team"
                message={`Delete team "${team.name}"? Impact: projects owned by this team may lose expected ownership workflows.`}
                confirmText="Delete Team"
                variant="danger"
                requireConfirmation={true}
                confirmationText={team.name}
                loading={deleting}
            />
        </section>
    );
}
