import { useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { formatRelativeTimeRounded } from '../lib/utils';
import { useIsMobile } from '../lib/use-media';
import type { Project } from '../lib/project-context';
import { projectPath } from '../lib/routes';
import { useToast } from '../components/toast';
import { Menu } from '../components/roster-table';
import { Icon } from '../components/icon';
import { ErrorState, LoadingState } from '../components/states';
import {
    Alert,
    AutocompleteInput,
    Button,
    ConfirmDialog,
    Empty,
    Field,
    Input,
    Modal,
    Panel,
    Segmented,
    colorFor,
    cx,
    statusTone,
} from '../components/r-ui';

interface TeamUser { id: string; email: string }

interface Team {
    id: string;
    name: string;
    members: TeamUser[];
    owners: TeamUser[];
    idp_managed: boolean;
    updated: string;
}

interface CurrentUser { id: string; email: string; is_admin?: boolean; can_create_teams?: boolean }

interface Person extends TeamUser { isOwner: boolean; isMember: boolean }

type Role = 'member' | 'owner';

function teamProjectCount(projects: Project[], team: Team): number {
    return projects.filter(p => p.owner && (p.owner.id === team.id || (!p.owner.email && p.owner.name === team.name))).length;
}

/** Everyone on a team once, with the roles they hold — a person can hold both. */
function people(team: Team): Person[] {
    const map = new Map<string, Person>();
    for (const o of team.owners || []) map.set(o.id, { ...o, isOwner: true, isMember: false });
    for (const m of team.members || []) {
        const p = map.get(m.id);
        if (p) p.isMember = true;
        else map.set(m.id, { ...m, isOwner: false, isMember: true });
    }
    return [...map.values()].sort((a, b) => Number(b.isOwner) - Number(a.isOwner) || a.email.localeCompare(b.email));
}

function initials(email: string): string {
    const local = email.split('@')[0] || email;
    const parts = local.split(/[._-]/).filter(Boolean);
    return ((parts[0]?.[0] || '') + (parts[1]?.[0] || '')).toUpperCase() || local.slice(0, 2).toUpperCase();
}

/**
 * Teams own projects. Desktop shows the list and the selected team side by
 * side; on mobile the list and a team are separate screens.
 */
export function TeamsPage({ currentUser, teamName, openCreate = false }: { currentUser: CurrentUser; teamName?: string; openCreate?: boolean }) {
    const [teams, setTeams] = useState<Team[] | null>(null);
    const [projects, setProjects] = useState<Project[]>([]);
    const [error, setError] = useState<string | null>(null);
    const [createOpen, setCreateOpen] = useState(false);
    const isMobile = useIsMobile();

    const load = useCallback(async () => {
        try {
            const data: Team[] = await api.getTeams();
            setTeams([...data].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' })));
            setError(null);
        } catch (err) {
            setError((err as Error).message);
        }
        api.getProjects().then((p: Project[]) => setProjects(p || [])).catch(() => {});
    }, []);

    useEffect(() => {
        load();
        window.addEventListener('rise:mutation', load);
        return () => window.removeEventListener('rise:mutation', load);
    }, [load]);

    useEffect(() => {
        if (!openCreate) return;
        setCreateOpen(true);
        window.history.replaceState({}, '', window.location.pathname);
    }, [openCreate]);

    if (error && !teams) return <ErrorState message={`Error loading teams: ${error}`} onRetry={load} />;
    if (!teams) return <LoadingState label="Loading teams…" />;

    const showList = !isMobile || !teamName;
    const showDetail = !!teamName;

    return (
        <section>
            {showList && (
                <div className="r-page-head">
                    <div className="title-stack">
                        <h1 className="r-page-title">Teams</h1>
                        <div className="r-page-sub">Teams own projects. Owners manage the team and its members.</div>
                    </div>
                    {currentUser.can_create_teams && (
                        <Button variant="primary" icon="plus" onClick={() => setCreateOpen(true)}>New team</Button>
                    )}
                </div>
            )}

            <div className={cx(!isMobile && 'r-teams-layout')}>
                {showList && (
                    teams.length === 0 ? (
                        <Panel><Empty title="No teams yet">Create a team to share project ownership across people.</Empty></Panel>
                    ) : (
                        <Panel className="r-list r-team-list">
                            {teams.map(t => {
                                const mine = t.owners?.some(o => o.email === currentUser.email);
                                const count = teamProjectCount(projects, t);
                                return (
                                    <div
                                        key={t.id}
                                        role="link"
                                        tabIndex={0}
                                        aria-current={t.name === teamName ? 'page' : undefined}
                                        className={cx('r-list-row r-team-row', t.name === teamName && 'sel')}
                                        onClick={() => navigate(`/team/${t.name}`)}
                                        onKeyDown={e => { if (e.key === 'Enter') navigate(`/team/${t.name}`); }}
                                    >
                                        <span className="r-team-tile">{t.name.slice(0, 1).toUpperCase()}</span>
                                        <span className="body">
                                            <span className="name">{t.name}{t.idp_managed && <span className="r-env-tag">IdP</span>}{mine && <span className="r-env-tag prod">owner</span>}</span>
                                            <span className="sub">{people(t).length} {people(t).length === 1 ? 'person' : 'people'} · {count} project{count === 1 ? '' : 's'}</span>
                                        </span>
                                        <Icon name="chev" size={14} />
                                    </div>
                                );
                            })}
                        </Panel>
                    )
                )}
                {showDetail ? (
                    <TeamDetail key={teamName} teamName={teamName!} currentUser={currentUser} />
                ) : !isMobile && teams.length > 0 ? (
                    <Panel><Empty title="Select a team">Pick a team to see its people and projects.</Empty></Panel>
                ) : null}
            </div>

            <CreateTeamModal open={createOpen} onClose={() => setCreateOpen(false)} currentUser={currentUser} />
        </section>
    );
}

function TeamDetail({ teamName, currentUser }: { teamName: string; currentUser: CurrentUser }) {
    const [team, setTeam] = useState<Team | null>(null);
    const [teamProjects, setTeamProjects] = useState<Project[]>([]);
    const [error, setError] = useState<string | null>(null);
    const [addOpen, setAddOpen] = useState(false);
    const [deleteOpen, setDeleteOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const { showToast } = useToast();

    const load = useCallback(async () => {
        try {
            const [data, projects] = await Promise.all([api.getTeam(teamName), api.getTeamProjects(teamName)]);
            setTeam(data);
            setTeamProjects(projects || []);
            setError(null);
        } catch (err) {
            setError((err as Error).message);
        }
    }, [teamName]);

    useEffect(() => { load(); }, [load]);

    if (error && !team) return <ErrorState message={`Error loading team: ${error}`} onRetry={load} />;
    if (!team) return <LoadingState label="Loading team…" />;

    const canManage = !!currentUser.is_admin || team.owners?.some(o => o.email === currentUser.email);
    // IdP-managed teams can only be changed by admins.
    const canEdit = canManage && (!team.idp_managed || !!currentUser.is_admin);
    const roster = people(team);
    const ownerIds = (team.owners || []).map(o => o.id);
    const memberIds = (team.members || []).map(m => m.id);

    /** Saves the roster; resolves to whether it was saved. */
    const update = async (owners: string[], members: string[], message: string): Promise<boolean> => {
        if (owners.length === 0) {
            showToast('A team needs at least one owner', 'error');
            return false;
        }
        try {
            await api.updateTeam(team.id, { owners, members });
            showToast(message, 'success');
            await load();
            window.dispatchEvent(new Event('rise:mutation'));
            return true;
        } catch (err) {
            showToast(`Failed to update team: ${(err as Error).message}`, 'error');
            return false;
        }
    };

    const actionsFor = (p: Person) => {
        const items: { label: string; icon?: string; onClick: () => void }[] = [];
        if (!p.isOwner) items.push({ label: 'Make owner', icon: 'user', onClick: () => update([...ownerIds, p.id], memberIds, `${p.email} is now an owner`) });
        if (p.isOwner && ownerIds.length > 1) items.push({ label: 'Remove owner role', icon: 'user', onClick: () => update(ownerIds.filter(id => id !== p.id), p.isMember ? memberIds : [...memberIds, p.id], `${p.email} is now a member`) });
        if (p.isOwner && !p.isMember) items.push({ label: 'Also make member', icon: 'user', onClick: () => update(ownerIds, [...memberIds, p.id], `${p.email} is now also a member`) });
        if (p.isMember && p.isOwner) items.push({ label: 'Remove member role', icon: 'user', onClick: () => update(ownerIds, memberIds.filter(id => id !== p.id), `${p.email} is no longer a member`) });
        if (!(p.isOwner && ownerIds.length === 1)) {
            items.push({ label: 'Remove from team', icon: 'trash', onClick: () => update(ownerIds.filter(id => id !== p.id), memberIds.filter(id => id !== p.id), `Removed ${p.email}`) });
        }
        return items;
    };

    const handleDelete = async () => {
        setDeleting(true);
        try {
            await api.deleteTeam(team.id);
            showToast(`Team ${team.name} deleted`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            navigate('/teams');
        } catch (err) {
            showToast(`Failed to delete team: ${(err as Error).message}`, 'error');
            setDeleting(false);
        }
    };

    const owned = (p: Project) => !!p.owner && (p.owner.id === team.id || (!p.owner.email && p.owner.name === team.name));

    return (
        <div className="r-team-detail">
            <div className="r-team-head">
                <div className="title-stack">
                    <h2 className="r-team-title"><Icon name="users" size={18} />{team.name}{team.idp_managed && <span className="r-env-tag">IdP</span>}</h2>
                    <div className="r-page-sub">{roster.length} {roster.length === 1 ? 'person' : 'people'} · updated {formatRelativeTimeRounded(team.updated)}</div>
                </div>
                {canEdit && (
                    <div className="r-page-actions">
                        <Button variant="primary" icon="plus" onClick={() => setAddOpen(true)}>Add member</Button>
                        <button type="button" className="r-icon-btn" title="Delete team" aria-label="Delete team" onClick={() => setDeleteOpen(true)}>
                            <Icon name="trash" size={14} />
                        </button>
                    </div>
                )}
            </div>

            {team.idp_managed && !currentUser.is_admin && (
                <Alert tone="info" icon="info">
                    This team is managed by your identity provider and can only be modified by administrators.
                </Alert>
            )}

            <div>
                <div className="r-label-row"><span className="r-section-label">People</span></div>
                <Panel className="r-list">
                    {roster.length === 0 && <div className="r-list-empty">No people in this team.</div>}
                    {roster.map(p => (
                        <div key={p.id} className="r-person">
                            <span className="r-person-ava" style={{ background: colorFor(p.email) }}>{initials(p.email)}</span>
                            <span className="body">
                                <span className="name">{p.email.split('@')[0]}{p.email === currentUser.email && <span className="muted"> (you)</span>}</span>
                                <span className="sub">{p.email}</span>
                            </span>
                            <span className="r-roles">
                                {p.isOwner && <span className="r-role owner">Owner</span>}
                                {p.isMember && <span className="r-role">Member</span>}
                            </span>
                            {canEdit && actionsFor(p).length > 0 ? (
                                <Menu
                                    items={actionsFor(p)}
                                    trigger={({ toggle }) => (
                                        <button type="button" className="r-icon-btn r-var-act" aria-label={`Change ${p.email}`} onClick={toggle}>
                                            <Icon name="more" size={16} />
                                        </button>
                                    )}
                                />
                            ) : canEdit ? <span className="r-person-spacer" title="The last owner can't be removed" /> : null}
                        </div>
                    ))}
                </Panel>
            </div>

            <div>
                <div className="r-label-row"><span className="r-section-label">Projects</span></div>
                {teamProjects.length === 0 ? (
                    <Panel><div className="r-list-empty">No projects owned by or shared with this team.</div></Panel>
                ) : (
                    <div className="r-team-projects">
                        {[...teamProjects].sort((a, b) => a.name.localeCompare(b.name)).map(p => (
                            <button type="button" key={p.name} className="r-team-project" onClick={() => navigate(projectPath(p.name))}>
                                <span className={cx('r-dot', statusTone(p.status))} />
                                <span className="name">{p.name}</span>
                                <span className="status">{owned(p) ? p.status : 'shared'}</span>
                            </button>
                        ))}
                    </div>
                )}
            </div>

            <AddPersonModal open={addOpen} onClose={() => setAddOpen(false)} team={team}
                knownEmails={currentUser.email ? [currentUser.email] : []}
                onAdd={(id, email, role) => update(
                    role === 'owner' ? Array.from(new Set([...ownerIds, id])) : ownerIds,
                    role === 'member' ? Array.from(new Set([...memberIds, id])) : memberIds,
                    `Added ${email} as ${role}`,
                )} />

            <ConfirmDialog
                isOpen={deleteOpen}
                onClose={() => setDeleteOpen(false)}
                onConfirm={handleDelete}
                title={`Delete team ${team.name}?`}
                message="Projects owned by this team lose their owning team. This cannot be undone."
                confirmText="Delete team"
                confirmTone="danger"
                requireText={team.name}
                loading={deleting}
            />
        </div>
    );
}

/**
 * Rise has no invitations: a person is added by email once they have signed in
 * to Rise at least once (that's when their account exists).
 */
function AddPersonModal({ open, onClose, team, knownEmails, onAdd }: {
    open: boolean;
    onClose: () => void;
    team: Team;
    knownEmails: string[];
    onAdd: (id: string, email: string, role: Role) => Promise<boolean>;
}) {
    const [email, setEmail] = useState('');
    const [role, setRole] = useState<Role>('member');
    const [busy, setBusy] = useState(false);
    const { showToast } = useToast();
    useEffect(() => { if (open) { setEmail(''); setRole('member'); } }, [open]);

    const submit = async () => {
        const value = email.trim();
        if (!value.includes('@')) { showToast('Enter an email address', 'error'); return; }
        setBusy(true);
        try {
            const res: { users?: TeamUser[] } = await api.lookupUsers([value]);
            const user = res.users?.[0];
            if (!user) {
                showToast(`No Rise account for ${value} — they need to sign in to Rise once first`, 'error');
                return;
            }
            if (await onAdd(user.id, user.email, role)) onClose();
        } catch (err) {
            const msg = (err as Error).message;
            showToast(msg.includes('404') ? `No Rise account for ${value} — they need to sign in to Rise once first` : `Failed to add ${value}: ${msg}`, 'error');
        } finally {
            setBusy(false);
        }
    };

    return (
        <Modal
            isOpen={open}
            onClose={onClose}
            title={`Add to ${team.name}`}
            sub="People are added by the email they sign in to Rise with."
            footer={
                <>
                    <Button onClick={onClose} disabled={busy}>Cancel</Button>
                    <Button variant="primary" onClick={submit} loading={busy}>Add {role}</Button>
                </>
            }
        >
            <Field label="Email">
                <AutocompleteInput type="email" value={email} onChange={setEmail} options={knownEmails} placeholder="name@example.com" onEnter={submit} />
            </Field>
            <Field label="Role" hint={role === 'owner' ? 'Owners manage the team and its members.' : 'Members can own projects through the team.'}>
                <Segmented<Role> value={role} options={[{ value: 'member', label: 'Member' }, { value: 'owner', label: 'Owner' }]} onChange={setRole} />
            </Field>
        </Modal>
    );
}

function CreateTeamModal({ open, onClose, currentUser }: { open: boolean; onClose: () => void; currentUser: CurrentUser }) {
    const [name, setName] = useState('');
    const [owners, setOwners] = useState('');
    const [members, setMembers] = useState('');
    const [saving, setSaving] = useState(false);
    const { showToast } = useToast();
    useEffect(() => {
        if (open) { setName(''); setOwners(currentUser.email || ''); setMembers(''); }
    }, [open, currentUser.email]);

    const emails = useMemo(() => (s: string) => s.split(',').map(e => e.trim()).filter(Boolean), []);

    const create = async () => {
        if (!name) { showToast('Team name is required', 'error'); return; }
        const ownerEmails = emails(owners);
        const memberEmails = emails(members);
        if (ownerEmails.length === 0) { showToast('At least one owner is required', 'error'); return; }
        setSaving(true);
        try {
            const ownerLookup = await api.lookupUsers(ownerEmails);
            const memberLookup = memberEmails.length > 0 ? await api.lookupUsers(memberEmails) : { users: [] };
            if (!ownerLookup.users || ownerLookup.users.length !== ownerEmails.length) {
                showToast('One or more owner email addresses have no Rise account', 'error');
                return;
            }
            if (memberEmails.length > 0 && (!memberLookup.users || memberLookup.users.length !== memberEmails.length)) {
                showToast('One or more member email addresses have no Rise account', 'error');
                return;
            }
            await api.createTeam(name, memberLookup.users.map((u: TeamUser) => u.id), ownerLookup.users.map((u: TeamUser) => u.id));
            showToast(`Team ${name} created`, 'success');
            onClose();
            window.dispatchEvent(new Event('rise:mutation'));
            navigate(`/team/${name}`);
        } catch (err) {
            showToast(`Failed to create team: ${(err as Error).message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    return (
        <Modal
            isOpen={open}
            onClose={onClose}
            title="New team"
            footer={
                <>
                    <Button onClick={onClose} disabled={saving}>Cancel</Button>
                    <Button variant="primary" onClick={create} loading={saving}>Create team</Button>
                </>
            }
        >
            <Field label="Team name">
                <Input value={name} onChange={e => setName(e.target.value)} placeholder="engineering" autoFocus />
            </Field>
            <Field label="Owners (emails, comma-separated)" hint="Owners can manage the team. At least one owner is required.">
                <AutocompleteInput type="email" value={owners} onChange={setOwners} options={currentUser.email ? [currentUser.email] : []} placeholder="alice@example.com, bob@example.com" multiValue />
            </Field>
            <Field label="Members (emails, comma-separated)" hint="Members can own projects through the team.">
                <AutocompleteInput type="email" value={members} onChange={setMembers} options={currentUser.email ? [currentUser.email] : []} placeholder="charlie@example.com" multiValue />
            </Field>
        </Modal>
    );
}
