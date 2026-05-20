// @ts-nocheck
import { Icon } from './icon';
import { Panel, Status } from './r-ui';
import { formatRelativeTimeRounded, stripUrlScheme } from '../lib/utils';

function OwnerCell({ owner }) {
    if (!owner || (!owner.email && !owner.name)) {
        return <span style={{ color: 'var(--text-soft)' }}>—</span>;
    }
    const isTeam = !owner.email && !!owner.name;
    return (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
            <Icon name={isTeam ? 'users' : 'user'} size={13} />
            <span>{owner.email || owner.name}</span>
        </span>
    );
}

// Shared project listing table (r-* design system). Renders inside a Panel and
// owns its empty state. Used by the Projects page and the team detail page.
//
// isOwnRow: optional predicate; matching rows get an accent highlight.
export function ProjectTable({
    projects,
    accessClasses = [],
    onRowClick,
    emptyText = 'No projects found.',
    isOwnRow,
}) {
    return (
        <Panel>
            {projects.length === 0 ? (
                <div style={{ padding: 36, textAlign: 'center', color: 'var(--text-muted)' }}>
                    {emptyText}
                </div>
            ) : (
                <table className="r-table">
                    <thead>
                        <tr>
                            <th>Project</th>
                            <th>Status</th>
                            <th>Primary URL</th>
                            <th>Owner</th>
                            <th>Access</th>
                            <th style={{ textAlign: 'right' }}>Updated</th>
                        </tr>
                    </thead>
                    <tbody>
                        {projects.map(project => {
                            const updated = project.updated || project.updated_at || project.created;
                            // When isOwnRow is provided, mark every row: an accent
                            // bar for owned projects, a gray bar for shared ones.
                            const ownClass = isOwnRow
                                ? (isOwnRow(project) ? ' r-row-own' : ' r-row-shared')
                                : '';
                            return (
                                <tr
                                    key={project.id || project.name}
                                    className={`click${ownClass}`}
                                    onClick={() => onRowClick(project)}
                                >
                                    <td style={{ maxWidth: 280 }}>
                                        <div style={{ fontWeight: 500, fontSize: 13.5 }}>{project.name}</div>
                                    </td>
                                    <td><Status status={project.status || 'Unknown'} /></td>
                                    <td style={{ maxWidth: 300 }}>
                                        {project.primary_url ? (
                                            <a
                                                className="r-link mono"
                                                style={{ fontSize: 12.5, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', display: 'inline-block', maxWidth: '100%' }}
                                                href={project.primary_url}
                                                target="_blank"
                                                rel="noopener noreferrer"
                                                onClick={(e) => e.stopPropagation()}
                                            >
                                                {stripUrlScheme(project.primary_url)}
                                            </a>
                                        ) : (
                                            <span style={{ color: 'var(--text-soft)' }}>—</span>
                                        )}
                                    </td>
                                    <td><OwnerCell owner={project.owner} /></td>
                                    <td>
                                        <span className="r-pill">
                                            {accessClasses.find(a => a.id === project.access_class)?.display_name
                                                || project.access_class || '—'}
                                        </span>
                                    </td>
                                    <td style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                        {updated ? formatRelativeTimeRounded(updated) : '—'}
                                    </td>
                                </tr>
                            );
                        })}
                    </tbody>
                </table>
            )}
        </Panel>
    );
}
