import React, { useMemo, useRef } from 'react';
import { colorFor } from '../../components/r-ui';
import { formatDay, formatHms } from './format';
import { findMatches } from './query';
import type { LogEntry } from './types';

/** Pointer travel, in px, past which a click is treated as a text selection. */
const DRAG_SLOP_PX = 4;

/** Regex-based JSON syntax highlighting — cheap, and only runs when expanded. */
function highlightJson(value: unknown): React.ReactElement {
    const pretty = JSON.stringify(value, null, 2);
    const re = /("(?:\\.|[^"\\])*"\s*:|"(?:\\.|[^"\\])*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|\b(?:true|false|null)\b)/g;
    const out: React.ReactNode[] = [];
    let last = 0;
    let key = 0;
    pretty.replace(re, (match, _group, offset: number) => {
        if (offset > last) out.push(pretty.slice(last, offset));
        let cls = 'json-num';
        if (match.startsWith('"')) cls = match.endsWith(':') ? 'json-key' : 'json-str';
        else if (match === 'true' || match === 'false') cls = 'json-bool';
        else if (match === 'null') cls = 'json-null';
        out.push(<span key={key++} className={cls}>{match}</span>);
        last = offset + match.length;
        return match;
    });
    if (last < pretty.length) out.push(pretty.slice(last));
    return <pre className="r-logc-json">{out}</pre>;
}

/** Wrap search hits in `<mark>`, flagging the one the user is currently on. */
function markMatches(
    text: string,
    needle: string,
    activeOffset: number | null,
): React.ReactNode {
    const ranges = findMatches(text, needle);
    if (ranges.length === 0) return text;
    const out: React.ReactNode[] = [];
    let last = 0;
    ranges.forEach(([start, end], i) => {
        if (start > last) out.push(text.slice(last, start));
        out.push(
            <mark key={i} className={start === activeOffset ? 'r-logc-hit is-active' : 'r-logc-hit'}>
                {text.slice(start, end)}
            </mark>,
        );
        last = end;
    });
    if (last < text.length) out.push(text.slice(last));
    return out;
}

export interface LogRowProps {
    entry: LogEntry;
    /** Prefix the time with a day, for windows that span more than one. */
    showDay: boolean;
    /** Dim the timestamp: pagination reached past the selected window. */
    outOfRange: boolean;
    expanded: boolean;
    onToggleExpand: (id: string) => void;
    onCopy: (entry: LogEntry) => void;
    /** Active search term, for inline highlighting. */
    search: string;
    /** Character offset of the focused match, when this row holds it. */
    activeMatchOffset: number | null;
}

function LogRowImpl({
    entry,
    showDay,
    outOfRange,
    expanded,
    onToggleExpand,
    onCopy,
    search,
    activeMatchOffset,
}: LogRowProps) {
    const canExpand = entry.isJson;
    const json = useMemo(
        () => (expanded && entry.isJson ? highlightJson(entry.parsed) : null),
        [expanded, entry.isJson, entry.parsed],
    );
    const prefix = entry.jsonPrefix?.trimEnd();

    const attribution = entry.container ?? entry.replica;
    const className = `r-logc-row lv-${entry.level}`
        + (canExpand ? ' is-expandable' : '')
        + (expanded ? ' is-expanded' : '')
        + (outOfRange ? ' is-out-of-range' : '');

    /**
     * Clicking a JSON line expands it — but a log viewer's other primary
     * gesture is selecting text, and a drag ending inside the row also fires a
     * click. Compare the press and release positions and let anything that
     * moved count as a selection, not a toggle.
     */
    const pressRef = useRef<{ x: number; y: number } | null>(null);
    const onPointerDown = (e: React.PointerEvent) => {
        pressRef.current = { x: e.clientX, y: e.clientY };
    };
    const onClick = (e: React.MouseEvent) => {
        if (!canExpand) return;
        // Never swallow a click on the row's own controls.
        if ((e.target as HTMLElement).closest('button')) return;
        const press = pressRef.current;
        pressRef.current = null;
        if (press && Math.hypot(e.clientX - press.x, e.clientY - press.y) > DRAG_SLOP_PX) return;
        if (!window.getSelection()?.isCollapsed) return;
        onToggleExpand(entry.id);
    };

    return (
        <div
            className={className}
            title={entry.iso || undefined}
            onPointerDown={canExpand ? onPointerDown : undefined}
            onClick={canExpand ? onClick : undefined}
            onKeyDown={canExpand ? (e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    onToggleExpand(entry.id);
                }
            } : undefined}
            role={canExpand ? 'button' : undefined}
            tabIndex={canExpand ? 0 : undefined}
            aria-expanded={canExpand ? expanded : undefined}
        >
            <span className="r-logc-spine" aria-hidden="true" />
            <span className="r-logc-ts">
                {showDay && entry.timestampMs ? `${formatDay(entry.timestampMs)} ` : ''}
                {formatHms(entry.timestampMs)}
            </span>
            {attribution && (
                <span
                    className="r-logc-src"
                    style={{ color: colorFor(attribution) }}
                    title={entry.replica ? `${attribution} · ${entry.replica}` : attribution}
                >
                    {attribution}
                </span>
            )}
            <span className="r-logc-msg">
                {json ? (
                    <>
                        {/* Whatever preceded the JSON is part of the line too. */}
                        {prefix && <span className="r-logc-json-prefix">{prefix}</span>}
                        {json}
                    </>
                ) : markMatches(entry.raw, search, activeMatchOffset)}
            </span>
            <span className="r-logc-actions">
                <button
                    type="button"
                    className="r-logc-line-btn"
                    onClick={() => onCopy(entry)}
                    aria-label="Copy this line"
                >
                    Copy
                </button>
            </span>
        </div>
    );
}

export const LogRow = React.memo(LogRowImpl);
