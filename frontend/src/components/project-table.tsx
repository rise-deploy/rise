// @ts-nocheck
import { Icon } from './icon';
import { Empty, Panel, Status } from './r-ui';

function OwnerCell({ owner }) {
    if (!owner || (!owner.email && !owner.name)) {
        return <span style={{ color: 'var(--text-soft)' }}>—</span>;
    }
    const isTeam = !owner.email && !!owner.name;
    return (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
            <Icon name={isTeam ? 'users' : 'user'} size={14} />
            <span>{owner.email || owner.name}</span>
        </span>
    );
}

// Project listing table styled with the r-* design system. Renders inside a
// Panel and handles its own empty state.
export function ProjectTable({ projects, onRowClick, emptyText = 'No projects found.' }) {
    return (
        <Panel>
            {projects.length === 0 ? (
                <div className="r-panel-body">
                    <Empty title={emptyText} />
                </div>
            ) : (
                <table className="r-table">
                    <thead>
                        <tr>
                            <th>Name</th>
                            <th>Status</th>
                            <th>Owner</th>
                            <th>Access</th>
                            <th>URL</th>
                        </tr>
                    </thead>
                    <tbody>
                        {projects.map(project => (
                            <tr
                                key={project.id || project.name}
                                className="click"
                                onClick={() => onRowClick(project)}
                            >
                                <td><span style={{ fontWeight: 500 }}>{project.name}</span></td>
                                <td><Status status={project.status || 'Unknown'} /></td>
                                <td style={{ color: 'var(--text-muted)' }}>
                                    <OwnerCell owner={project.owner} />
                                </td>
                                <td style={{ color: 'var(--text-muted)' }}>
                                    {project.access_class || '—'}
                                </td>
                                <td>
                                    {project.primary_url ? (
                                        <a
                                            className="r-link mono"
                                            style={{ fontSize: 12.5 }}
                                            href={project.primary_url}
                                            target="_blank"
                                            rel="noopener noreferrer"
                                            onClick={(e) => e.stopPropagation()}
                                        >
                                            {project.primary_url}
                                        </a>
                                    ) : (
                                        <span style={{ color: 'var(--text-soft)' }}>—</span>
                                    )}
                                </td>
                            </tr>
                        ))}
                    </tbody>
                </table>
            )}
        </Panel>
    );
}
