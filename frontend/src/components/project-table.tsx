import { navigate } from '../lib/navigation';
import { OwnerLabel, Panel, Status } from './r-ui';
import { useIsMobile } from '../lib/use-media';
import { formatRelativeTimeRounded, stripUrlScheme } from '../lib/utils';
import { effectiveStatus, hasFailedDeploy, statusLabel } from '../lib/project-status';

export interface ProjectOwner {
    id?: string;
    email?: string;
    name?: string;
}

export interface ProjectRow {
    id?: string;
    name: string;
    status?: string;
    active_deployment_id?: string;
    latest_deployment_id?: string;
    latest_deployment_status?: string;
    primary_url?: string;
    access_class?: string;
    owner?: ProjectOwner;
    created?: string;
    updated?: string;
    updated_at?: string;
}

export function ProjectStatusPill({ project, tooltip }: { project: ProjectRow; tooltip?: string }) {
    const failed = hasFailedDeploy(project);
    const label = statusLabel(project);
    const status = <Status status={effectiveStatus(project)} label={label} tooltip={tooltip} />;
    // A failed deploy links to that deployment, where its failure is explained.
    const target = failed ? project.latest_deployment_id : project.active_deployment_id;
    if (!target) return status;

    const href = `/deployment/${project.name}/${target}`;
    return (
        <a
            className="r-project-status-link"
            href={href}
            aria-label={`${failed ? 'Open failed deployment' : 'Open active deployment'} for ${project.name}: ${label}`}
            onClick={(e) => {
                e.stopPropagation();
                if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
                e.preventDefault();
                navigate(href);
            }}
        >{status}</a>
    );
}

export interface AccessClassOption {
    id: string;
    display_name?: string;
    description?: string;
}

export interface ProjectTableProps {
    projects: ProjectRow[];
    accessClasses?: AccessClassOption[];
    onRowClick: (project: ProjectRow) => void;
    emptyText?: string;
}

// Project listing: a table on desktop, stacked cards on phones. Owns its
// empty state.
export function ProjectTable({
    projects,
    accessClasses = [],
    onRowClick,
    emptyText = 'No projects found.',
}: ProjectTableProps) {
    const isMobile = useIsMobile();
    const accessLabel = (p: ProjectRow) => accessClasses.find(a => a.id === p.access_class)?.display_name || p.access_class || '—';

    if (projects.length === 0) {
        return <Panel><div className="r-list-empty r-empty-dashed">{emptyText}</div></Panel>;
    }

    if (isMobile) {
        return (
            <div className="r-cards">
                {projects.map(project => {
                    const updated = project.updated || project.updated_at || project.created;
                    return (
                        <div key={project.id || project.name} className="r-card" role="link" tabIndex={0}
                            onClick={() => onRowClick(project)} onKeyDown={e => { if (e.key === 'Enter') onRowClick(project); }}>
                            <div className="r-card-row">
                                <span className="r-card-title">{project.name}</span>
                                <span style={{ marginLeft: 'auto' }}><ProjectStatusPill project={project} /></span>
                            </div>
                            {project.primary_url && <div className="r-card-meta mono">{stripUrlScheme(project.primary_url)}</div>}
                            <div className="r-card-meta r-card-row">
                                <OwnerLabel owner={project.owner} />
                                <span>· {accessLabel(project)}</span>
                                {updated && <span>· {formatRelativeTimeRounded(updated)}</span>}
                            </div>
                        </div>
                    );
                })}
            </div>
        );
    }

    return (
        <Panel>
            <table className="r-table r-project-table">
                <thead>
                    <tr>
                        <th>Project</th>
                        <th style={{ width: 130 }}>Status</th>
                        <th style={{ width: 170 }}>Active deployment</th>
                        <th style={{ width: 200 }}>Owner</th>
                        <th style={{ width: 120 }}>Access</th>
                        <th style={{ width: 110, textAlign: 'right' }}>Updated</th>
                    </tr>
                </thead>
                <tbody>
                    {projects.map(project => {
                        const updated = project.updated || project.updated_at || project.created;
                        return (
                            <tr key={project.id || project.name} className="click" onClick={() => onRowClick(project)}>
                                <td>
                                    <div className="r-cell-main" style={{ fontWeight: 600 }}>
                                        <a className="r-row-link" href={`/project/${project.name}`}
                                            onClick={(e) => { e.stopPropagation(); if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return; e.preventDefault(); onRowClick(project); }}>
                                            {project.name}
                                        </a>
                                    </div>
                                    {project.primary_url && (
                                        <a
                                            className="r-link mono r-cell-sub r-url-sub"
                                            href={project.primary_url}
                                            target="_blank"
                                            rel="noopener noreferrer"
                                            onClick={(e) => e.stopPropagation()}
                                        >
                                            {stripUrlScheme(project.primary_url)}
                                        </a>
                                    )}
                                </td>
                                <td><ProjectStatusPill project={project} /></td>
                                <td className="mono r-nowrap" style={{ fontSize: 12, color: 'var(--text-muted)' }}>
                                    {project.active_deployment_id || <span className="muted">—</span>}
                                </td>
                                <td className="r-cell-ellipsis"><OwnerLabel owner={project.owner} /></td>
                                <td style={{ color: 'var(--text-muted)' }}>{accessLabel(project)}</td>
                                <td className="r-nowrap" style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                    {updated ? formatRelativeTimeRounded(updated) : '—'}
                                </td>
                            </tr>
                        );
                    })}
                </tbody>
            </table>
        </Panel>
    );
}
