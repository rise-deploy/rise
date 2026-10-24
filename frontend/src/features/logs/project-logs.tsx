import { useEffect, useMemo, useRef, useState } from 'react';
import { navigate, useQueryParam } from '../../lib/navigation';
import { useCurrentProject, type Deployment } from '../../lib/project-context';
import { byNewest, deploymentSource, envState, productionEnv } from '../../lib/env-state';
import { formatRelativeTimeRounded } from '../../lib/utils';
import { deploymentPath } from '../../lib/routes';
import { Empty, EnvTag, cx, statusTone } from '../../components/r-ui';
import { Icon } from '../../components/icon';
import { LoadingState } from '../../components/states';
import { LogConsole } from './log-console';

/**
 * The project's Logs section. Logs are per deployment (that is what the backend
 * serves), so the section is a deployment picker in front of the log console.
 * Without an explicit `?deployment=`, it opens on what production is serving.
 */
export function ProjectLogsPage() {
    const current = useCurrentProject()!;
    const { projectName, environments, deployments } = current;
    const [selected, setSelected] = useQueryParam('deployment', { history: 'push' });

    const sorted = useMemo(() => [...deployments].sort(byNewest), [deployments]);
    const fallback = useMemo(() => {
        const prod = productionEnv(environments);
        const state = prod ? envState(prod, deployments) : null;
        return state?.live ?? state?.latest ?? sorted[0] ?? null;
    }, [environments, deployments, sorted]);

    const deployment = sorted.find(d => d.deployment_id === selected) ?? (selected ? null : fallback);

    if (!current.project && deployments.length === 0) return <LoadingState label="Loading deployments…" />;
    if (sorted.length === 0) {
        return <Empty title="No deployments yet">Logs appear once the project has been deployed.</Empty>;
    }

    const picker = (
        <DeploymentPicker
            deployments={sorted}
            selected={deployment}
            selectedId={selected ?? deployment?.deployment_id ?? null}
            projectName={projectName}
            onSelect={id => setSelected(id)}
        />
    );

    if (!deployment) {
        return (
            <div className="r-logs-section">
                <div className="r-logs-picker-row">{picker}</div>
                <Empty title="Deployment not in the recent list">Open it from its <a className="r-link" href={deploymentPath(projectName, selected!)}>deployment page</a> instead.</Empty>
            </div>
        );
    }

    return (
        <LogConsole
            key={deployment.deployment_id}
            variant="page"
            projectName={projectName}
            deploymentId={deployment.deployment_id}
            deploymentStatus={deployment.status}
            deploymentCompletedAt={deployment.completed_at}
            deploymentCreated={deployment.created}
            containers={(deployment.containers as { name?: string }[] | undefined ?? []).map(c => c?.name).filter((n): n is string => !!n)}
            lead={picker}
        />
    );
}

function DeploymentPicker({ deployments, selected, selectedId, projectName, onSelect }: {
    deployments: Deployment[];
    selected: Deployment | null;
    selectedId: string | null;
    projectName: string;
    onSelect: (id: string) => void;
}) {
    const [open, setOpen] = useState(false);
    const ref = useRef<HTMLDivElement>(null);

    useEffect(() => {
        if (!open) return;
        const onDown = (e: MouseEvent) => { if (!ref.current?.contains(e.target as Node)) setOpen(false); };
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false); };
        document.addEventListener('mousedown', onDown);
        window.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDown);
            window.removeEventListener('keydown', onKey);
        };
    }, [open]);

    return (
        <div className="r-dep-picker-wrap">
            <div className="r-dep-picker" ref={ref}>
                <button type="button" className="r-dep-picker-trigger" onClick={() => setOpen(o => !o)} aria-haspopup="listbox" aria-expanded={open}>
                    <span className={cx('r-dot', statusTone(selected?.status))} />
                    <span className="mono id">{selected?.deployment_id ?? selectedId}</span>
                    {selected?.environment && <EnvTag env={selected.environment} />}
                    <span className="msg">{selected ? deploymentSource(selected) : ''}</span>
                    <Icon name="chevd" size={14} />
                </button>
                {open && (
                    <div className="r-dep-picker-pop" role="listbox">
                        {deployments.map(d => (
                            <button
                                type="button"
                                role="option"
                                aria-selected={d.deployment_id === selectedId}
                                key={d.deployment_id}
                                className={cx('r-dep-picker-item', d.deployment_id === selectedId && 'sel')}
                                onClick={() => { setOpen(false); onSelect(d.deployment_id); }}
                            >
                                <span className={cx('r-dot', statusTone(d.status))} />
                                <span className="mono id">{d.deployment_id}</span>
                                {d.environment && <EnvTag env={d.environment} />}
                                <span className="msg">{deploymentSource(d)}</span>
                                <span className="age">{formatRelativeTimeRounded(d.created)}</span>
                            </button>
                        ))}
                    </div>
                )}
            </div>
            {selectedId && (
                <a
                    className="r-link r-dep-picker-open"
                    href={deploymentPath(projectName, selectedId)}
                    onClick={(e) => {
                        if (e.metaKey || e.ctrlKey || e.shiftKey) return;
                        e.preventDefault();
                        navigate(deploymentPath(projectName, selectedId));
                    }}
                >
                    Open deployment →
                </a>
            )}
        </div>
    );
}
