import React, { useMemo } from 'react';
import { colorFor } from '../../components/r-ui';
import { formatDay, formatHms } from './format';
import { findMatches } from './query';
import type { LogEntry } from './types';

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

    const attribution = entry.container ?? entry.replica;
    const className = `r-logc-row lv-${entry.level}`
        + (expanded ? ' is-expanded' : '')
        + (outOfRange ? ' is-out-of-range' : '');

    return (
        <div className={className} title={entry.iso || undefined}>
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
                {json ?? markMatches(entry.raw, search, activeMatchOffset)}
            </span>
            <span className="r-logc-actions">
                {canExpand && (
                    <button
                        type="button"
                        className="r-logc-line-btn"
                        onClick={() => onToggleExpand(entry.id)}
                        aria-expanded={expanded}
                    >
                        {expanded ? 'Collapse' : 'JSON'}
                    </button>
                )}
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
