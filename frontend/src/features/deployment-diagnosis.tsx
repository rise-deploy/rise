import React, { useEffect, useState } from 'react';
import { formatISO8601, formatRelativeTimeRounded, isSafeUrl, stripUrlScheme } from '../lib/utils';
import type { Deployment } from '../lib/project-context';
import { shortImage } from '../lib/env-state';
import { Icon } from '../components/icon';
import { SourceLinkGroup, cx } from '../components/r-ui';
import { fetchDeploymentEvents, streamLogs, type DeploymentEvent } from './logs/api';

// The rollout as the status-change log records it. The CLI reports the build
// and push (it builds locally); the controller reports the rest. A deployment
// created from an existing image skips build and push entirely.

type StepState = 'done' | 'failed' | 'running' | 'pending' | 'skipped';

interface RolloutStep {
    label: string;
    /** Statuses that belong to this step; the first one entered starts it. */
    statuses: string[];
    state: StepState;
    startedAt?: string;
    endedAt?: string;
}

const STEP_DEFS: { label: string; statuses: string[] }[] = [
    { label: 'Queued', statuses: ['Pending'] },
    { label: 'Build image', statuses: ['Building'] },
    { label: 'Push to registry', statuses: ['Pushing', 'Pushed'] },
    { label: 'Start containers', statuses: ['Deploying'] },
    { label: 'Healthy', statuses: ['Healthy'] },
];

const FAILURE_STATUSES = new Set(['Failed', 'Unhealthy', 'Cancelled', 'Cancelling']);

/** `Pending → Building → … → Failed` as entered, oldest first. */
function transitions(events: DeploymentEvent[]): { status: string; at: string }[] {
    return events
        .filter(e => e.kind === 'status_changed' && typeof e.attributes?.to === 'string')
        .sort((a, b) => a.occurred_at.localeCompare(b.occurred_at))
        .map(e => ({ status: e.attributes.to as string, at: e.occurred_at }));
}

export function rolloutSteps(deployment: Deployment, events: DeploymentEvent[]): { steps: RolloutStep[]; failedAt: string | null } {
    const moves = [{ status: 'Pending', at: deployment.created }, ...transitions(events)];
    const entered = new Map<string, string>();
    for (const m of moves) if (!entered.has(m.status)) entered.set(m.status, m.at);
    const failure = moves.find(m => FAILURE_STATUSES.has(m.status));
    const reusedImage = !entered.has('Building') && !entered.has('Pushing') && (entered.has('Pushed') || entered.has('Deploying') || entered.has('Healthy') || !!failure);

    const steps: RolloutStep[] = STEP_DEFS.map(def => {
        const startedAt = def.statuses.map(s => entered.get(s)).find(Boolean);
        return { ...def, state: startedAt ? 'done' : 'pending', startedAt };
    });
    if (reusedImage) {
        steps[1].state = 'skipped';
        steps[2].state = 'skipped';
    }
    // Each step ends where the next entered one begins.
    const started = steps.filter(s => s.startedAt);
    started.forEach((s, i) => { s.endedAt = started[i + 1]?.startedAt ?? (failure && failure.at > s.startedAt! ? failure.at : undefined); });

    const last = started[started.length - 1];
    let failedAt: string | null = null;
    if (failure && last) {
        last.state = 'failed';
        last.endedAt = failure.at;
        failedAt = last.label;
    } else if (last && last.label !== 'Healthy' && !['Superseded', 'Stopped', 'Expired'].includes(deployment.status)) {
        last.state = 'running';
        last.endedAt = undefined;
    }
    return { steps, failedAt };
}

function seconds(from?: string, to?: string): string {
    if (!from || !to) return '';
    const s = Math.max(0, Math.round((Date.parse(to) - Date.parse(from)) / 1000));
    if (s >= 86400) return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
    if (s >= 3600) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
    return s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`;
}

export function formatDuration(from?: string, to?: string | null): string {
    return seconds(from, to || new Date().toISOString()) || '—';
}

/** Loads the deployment's status-change events; refreshes when the status moves. */
export function useDeploymentEvents(projectName: string, deploymentId: string, status: string): DeploymentEvent[] | null {
    const [events, setEvents] = useState<DeploymentEvent[] | null>(null);
    useEffect(() => {
        const controller = new AbortController();
        fetchDeploymentEvents({ projectName, deploymentId, limit: 200, kinds: ['status_changed'], signal: controller.signal })
            .then(page => setEvents(page.events))
            .catch(err => { if (!controller.signal.aborted) { console.error('Failed to load deployment events:', err); setEvents([]); } });
        return () => controller.abort();
    }, [projectName, deploymentId, status]);
    return events;
}

export function RolloutPanel({ steps }: { steps: RolloutStep[] }) {
    return (
        <div>
            <div className="r-label-row"><span className="r-section-label">Rollout</span></div>
            <ol className="r-rollout">
                {steps.map(step => (
                    <li key={step.label} className={cx('r-rollout-step', step.state)}>
                        <span className="mark" aria-hidden>
                            {step.state === 'done' && <Icon name="check" size={12} />}
                            {step.state === 'failed' && <Icon name="close" size={12} />}
                        </span>
                        <span className="label">{step.label}</span>
                        <span className="dur">
                            {step.state === 'skipped' ? 'reused image'
                                : step.state === 'running' ? formatDuration(step.startedAt)
                                    : seconds(step.startedAt, step.endedAt) || (step.state === 'pending' ? '—' : '')}
                        </span>
                    </li>
                ))}
            </ol>
        </div>
    );
}

interface ExcerptLine { at: string; level: string; text: string }

const ISO_PREFIX = /^(\d{4}-\d{2}-\d{2}T[\d:.]+(?:Z|[+-]\d{2}:?\d{2}))\s?/;

/** The last warning and error lines of a deployment, for the failure panel. */
function useErrorExcerpt(projectName: string, deployment: Deployment): ExcerptLine[] | null {
    const [lines, setLines] = useState<ExcerptLine[] | null>(null);
    useEffect(() => {
        const controller = new AbortController();
        const collected: ExcerptLine[] = [];
        streamLogs({
            projectName,
            deploymentId: deployment.deployment_id,
            tail: 8,
            levels: ['warn', 'error'],
            start: deployment.created,
            end: deployment.completed_at || new Date().toISOString(),
            signal: controller.signal,
        }, {
            onLine: payload => {
                const m = payload.line.match(ISO_PREFIX);
                const at = m ? new Date(m[1]).toLocaleTimeString([], { hour12: false }) : '';
                collected.push({ at, level: payload.level, text: m ? payload.line.slice(m[0].length) : payload.line });
            },
        })
            .then(() => setLines(collected))
            .catch(() => { if (!controller.signal.aborted) setLines([]); });
        return () => controller.abort();
    }, [projectName, deployment.deployment_id, deployment.created, deployment.completed_at]);
    return lines;
}

export function FailurePanel({ projectName, deployment, failedAt, stillServing, onOpenLogs }: {
    projectName: string;
    deployment: Deployment;
    failedAt: string | null;
    /** The deployment that keeps serving the group, when it isn't this one. */
    stillServing: Deployment | null;
    onOpenLogs: () => void;
}) {
    const excerpt = useErrorExcerpt(projectName, deployment);
    const unhealthy = deployment.status === 'Unhealthy';
    return (
        <div className="r-failure">
            <div className="r-failure-head">
                <div className="title">
                    <Icon name="close" size={14} />
                    {unhealthy ? 'Unhealthy' : failedAt ? <>Failed at {failedAt.toLowerCase()}</> : 'Failed'}
                </div>
                {deployment.error_message && <div className="reason">{deployment.error_message}</div>}
                <div className="reassure">
                    {stillServing
                        ? <><span className="mono">{stillServing.deployment_id}</span> is still serving {deployment.environment || deployment.deployment_group}.</>
                        : unhealthy
                            ? 'This deployment is serving traffic but failing its health checks.'
                            : <>Nothing in group <span className="mono">{deployment.deployment_group}</span> is serving traffic.</>}
                </div>
            </div>
            {excerpt && excerpt.length > 0 && (
                <div className="r-failure-log">
                    {excerpt.map((l, i) => (
                        <div key={i} className={cx('line', `lvl-${l.level}`)}>
                            <span className="t">{l.at}</span>
                            <span className="l">{l.level.toUpperCase()}</span>
                            <span className="m">{l.text}</span>
                        </div>
                    ))}
                </div>
            )}
            <div className="r-failure-foot">
                <span>
                    {excerpt === null ? 'Loading recent warnings and errors…'
                        : excerpt.length === 0 ? 'No warning or error lines were logged.'
                            : 'Last warning and error lines.'}
                    {' '}Variables are snapshotted per deployment; compare them on the Variables tab.
                </span>
                <button type="button" className="r-link r-link-btn" onClick={onOpenLogs}>Open logs →</button>
            </div>
        </div>
    );
}

/** When the rollout settled: healthy, or ended. Duration is measured to here. */
export function rolloutEnd(deployment: Deployment, steps: RolloutStep[] | null): string | null {
    const healthy = steps?.find(s => s.label === 'Healthy')?.startedAt;
    return healthy ?? deployment.completed_at ?? null;
}

export function DeploymentMetaStrip({ deployment, endedAt }: { deployment: Deployment; endedAt: string | null }) {
    const hasLinks = !!(deployment.pull_request_url || deployment.job_url);
    const repo = deployment.git_repository_url && isSafeUrl(deployment.git_repository_url) ? deployment.git_repository_url : null;
    const cells: { k: string; v: React.ReactNode }[] = [
        {
            k: 'Source',
            v: hasLinks ? <SourceLinkGroup jobUrl={deployment.job_url} prUrl={deployment.pull_request_url} />
                : repo ? <a className="r-link mono" href={repo} target="_blank" rel="noopener noreferrer">{stripUrlScheme(repo)}</a>
                    : <span className="muted">—</span>,
        },
        { k: 'Author', v: deployment.created_by_email || '—' },
        { k: 'Started', v: <span title={formatISO8601(deployment.created)}>{formatRelativeTimeRounded(deployment.created)}</span> },
        { k: 'Duration', v: <span className="mono">{formatDuration(deployment.created, endedAt)}</span> },
        { k: 'Image', v: <span className="mono" title={deployment.image}>{shortImage(deployment.image)}</span> },
    ];
    return (
        <div className="r-meta-strip">
            {cells.map(c => (
                <div key={c.k} className="cell">
                    <div className="r-section-label">{c.k}</div>
                    <div className="v">{c.v}</div>
                </div>
            ))}
        </div>
    );
}
