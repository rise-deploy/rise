import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import type { Deployment } from '../lib/project-context';
import { commitUrl, pullRequestLabel, shortImage, shortRevision } from '../lib/env-state';
import { navigate } from '../lib/navigation';
import { deploymentPath } from '../lib/routes';
import { isSafeUrl } from '../lib/utils';
import { Icon } from './icon';
import { cx } from './r-ui';

// Where a deployment came from, as one fact: its pull request, else the
// deployment it was created from, else branch@commit, else its image. The
// rest (commit, branch, image, CI job, author) is in a card that opens on
// hover, focus or tap, so a list stays calm and nothing is out of reach.

type Lead = { icon: string; text: React.ReactNode; mono?: boolean };

function lead(d: Deployment): Lead {
    const pr = pullRequestLabel(d.pull_request_url);
    if (pr) return { icon: 'branch', text: <>{pr}{d.git_branch && <span className="r-src-dim"> · {d.git_branch}</span>}</> };
    if (d.source_deployment_id) return { icon: 'refresh', text: <>Redeploy of <span className="mono">{shortId(d.source_deployment_id)}</span></> };
    if (d.git_revision) {
        return { icon: 'commit', mono: true, text: <>{d.git_branch ? `${d.git_branch}@` : ''}{shortRevision(d.git_revision)}</> };
    }
    return { icon: 'cube', mono: true, text: shortImage(d.image) };
}

const shortId = (id: string) => id.split('-').pop() || id;

const linkClick = (href: string) => (e: React.MouseEvent) => {
    e.stopPropagation();
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    navigate(href);
};

function external(href: string | undefined): string | null {
    return href && isSafeUrl(href) ? href : null;
}

function SourceCard({ d, projectName }: { d: Deployment; projectName: string }) {
    const prUrl = external(d.pull_request_url);
    const jobUrl = external(d.job_url);
    const commitHref = commitUrl(external(d.git_repository_url) ?? undefined, d.git_revision);
    const rows: [string, React.ReactNode][] = [];
    if (prUrl) rows.push(['Pull request', <a className="r-link" href={prUrl} target="_blank" rel="noopener noreferrer">{pullRequestLabel(prUrl)}</a>]);
    if (d.git_revision) {
        const sha = shortRevision(d.git_revision);
        rows.push(['Commit', <>
            {commitHref
                ? <a className="r-link mono" href={commitHref} target="_blank" rel="noopener noreferrer" title={d.git_revision}>{sha}</a>
                : <span className="mono" title={d.git_revision}>{sha}</span>}
            {d.git_dirty && <span className="r-dirty">+ uncommitted changes</span>}
        </>]);
    }
    if (d.git_branch) rows.push(['Branch', <span className="mono">{d.git_branch}</span>]);
    if (d.source_deployment_id) {
        const href = deploymentPath(projectName, d.source_deployment_id);
        rows.push(['Created from', <a className="r-link mono" href={href} onClick={linkClick(href)}>{d.source_deployment_id}</a>]);
    }
    if (d.image) rows.push(['Image', <span className="mono r-src-wrap">{d.image}</span>]);
    if (jobUrl) rows.push(['CI job', <a className="r-link" href={jobUrl} target="_blank" rel="noopener noreferrer">Open job</a>]);
    if (d.created_by_email) rows.push(['Author', d.created_by_email]);
    return (
        <dl className="r-src-card-rows">
            {rows.map(([k, v]) => (
                <div key={k}><dt>{k}</dt><dd>{v}</dd></div>
            ))}
        </dl>
    );
}

/** A deployment's source: one fact, with the rest in a card on hover, focus or tap. */
export function DeploymentSource({ d, projectName, className }: { d: Deployment; projectName: string; className?: string }) {
    const triggerRef = useRef<HTMLButtonElement | null>(null);
    const cardRef = useRef<HTMLDivElement | null>(null);
    const [open, setOpen] = useState(false);
    const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
    const closeTimer = useRef<number | null>(null);
    const openTimer = useRef<number | null>(null);

    const clearTimers = () => {
        if (closeTimer.current) window.clearTimeout(closeTimer.current);
        if (openTimer.current) window.clearTimeout(openTimer.current);
        closeTimer.current = openTimer.current = null;
    };
    const show = (delay: number) => {
        clearTimers();
        openTimer.current = window.setTimeout(() => setOpen(true), delay);
    };
    // A short grace period lets the pointer travel from the trigger into the card.
    const hideSoon = () => {
        clearTimers();
        closeTimer.current = window.setTimeout(() => { setOpen(false); setPos(null); }, 160);
    };
    const close = useCallback(() => { clearTimers(); setOpen(false); setPos(null); }, []);
    useEffect(() => () => clearTimers(), []);

    const compute = useCallback(() => {
        const t = triggerRef.current?.getBoundingClientRect();
        const c = cardRef.current?.getBoundingClientRect();
        if (!t || !c) return;
        const margin = 8;
        let left = Math.max(margin, Math.min(t.left, window.innerWidth - c.width - margin));
        let top = t.bottom + 6;
        if (top + c.height > window.innerHeight - margin && t.top - c.height - 6 >= margin) top = t.top - c.height - 6;
        if (left < margin) left = margin;
        setPos({ top, left });
    }, []);

    useLayoutEffect(() => { if (open) compute(); }, [open, compute]);
    useEffect(() => {
        if (!open) return;
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') { close(); triggerRef.current?.focus(); } };
        const onDown = (e: PointerEvent) => {
            const target = e.target as Node;
            if (!triggerRef.current?.contains(target) && !cardRef.current?.contains(target)) close();
        };
        window.addEventListener('keydown', onKey);
        window.addEventListener('pointerdown', onDown, true);
        window.addEventListener('scroll', compute, true);
        window.addEventListener('resize', compute);
        return () => {
            window.removeEventListener('keydown', onKey);
            window.removeEventListener('pointerdown', onDown, true);
            window.removeEventListener('scroll', compute, true);
            window.removeEventListener('resize', compute);
        };
    }, [open, close, compute]);

    const l = lead(d);
    return (
        <>
            <button
                ref={triggerRef}
                type="button"
                className={cx('r-src', className)}
                aria-expanded={open}
                aria-label={`Source of ${d.deployment_id}`}
                onPointerEnter={(e) => { if (e.pointerType !== 'touch') show(220); }}
                onPointerLeave={(e) => { if (e.pointerType !== 'touch') hideSoon(); }}
                onFocus={() => show(0)}
                onBlur={(e) => { if (!cardRef.current?.contains(e.relatedTarget as Node)) hideSoon(); }}
                onClick={(e) => { e.stopPropagation(); clearTimers(); setOpen(o => !o); if (open) setPos(null); }}
                // Rows open their deployment on Enter; the source opens its card instead.
                onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') e.stopPropagation(); }}
            >
                <Icon name={l.icon} size={13} />
                <span className={cx('r-src-text', l.mono && 'mono')}>{l.text}</span>
                {d.git_dirty && <span className="r-dirty" title="Built from a working tree with uncommitted changes">+ uncommitted</span>}
            </button>
            {open && createPortal(
                <div
                    ref={cardRef}
                    className="r-src-card"
                    role="dialog"
                    aria-label={`Source of ${d.deployment_id}`}
                    style={{ position: 'fixed', top: pos?.top ?? 0, left: pos?.left ?? 0, visibility: pos ? 'visible' : 'hidden' }}
                    onPointerEnter={clearTimers}
                    onPointerLeave={(e) => { if (e.pointerType !== 'touch') hideSoon(); }}
                    onClick={(e) => e.stopPropagation()}
                >
                    <SourceCard d={d} projectName={projectName} />
                </div>,
                document.body,
            )}
        </>
    );
}
