/**
 * The console's query line accepts `field:value` filter tokens mixed with free
 * text. `level:error container:web timeout` narrows to error-level lines from
 * the `web` container containing "timeout".
 *
 * Values may be quoted when they contain spaces: `container:"side car"`.
 * Anything that isn't a recognised field is treated as search text, so a bare
 * `http://host:8080` doesn't silently become a filter.
 */

export interface LogQuery {
    levels: string[];
    containers: string[];
    search: string;
}

export const EMPTY_QUERY: LogQuery = { levels: [], containers: [], search: '' };

const FIELDS = ['level', 'container'] as const;
type Field = (typeof FIELDS)[number];

function isField(name: string): name is Field {
    return (FIELDS as readonly string[]).includes(name);
}

/** Split on whitespace, keeping quoted runs together. */
function tokenize(input: string): string[] {
    const tokens: string[] = [];
    let current = '';
    let quote: '"' | "'" | null = null;

    for (const ch of input) {
        if (quote) {
            if (ch === quote) quote = null;
            else current += ch;
            continue;
        }
        if (ch === '"' || ch === "'") {
            quote = ch;
            continue;
        }
        if (ch === ' ' || ch === '\t') {
            if (current) tokens.push(current);
            current = '';
            continue;
        }
        current += ch;
    }
    if (current) tokens.push(current);
    return tokens;
}

export function parseQuery(input: string): LogQuery {
    const levels: string[] = [];
    const containers: string[] = [];
    const words: string[] = [];

    for (const token of tokenize(input)) {
        const colon = token.indexOf(':');
        if (colon > 0) {
            const field = token.slice(0, colon).toLowerCase();
            const value = token.slice(colon + 1);
            if (value && isField(field)) {
                const bucket = field === 'level' ? levels : containers;
                if (!bucket.includes(value)) bucket.push(value);
                continue;
            }
        }
        words.push(token);
    }

    return { levels, containers, search: words.join(' ') };
}

/** Quote a value only when it needs it, so the round-trip stays readable. */
function quote(value: string): string {
    return /[\s"']/.test(value) ? `"${value.replace(/"/g, '')}"` : value;
}

export function formatQuery(query: LogQuery): string {
    const parts = [
        ...query.levels.map((v) => `level:${quote(v)}`),
        ...query.containers.map((v) => `container:${quote(v)}`),
    ];
    if (query.search.trim()) parts.push(query.search.trim());
    return parts.join(' ');
}

/** Add or remove one filter value, returning the new query string. */
export function toggleFilter(input: string, field: Field, value: string): string {
    const query = parseQuery(input);
    const bucket = field === 'level' ? query.levels : query.containers;
    const index = bucket.indexOf(value);
    if (index === -1) bucket.push(value);
    else bucket.splice(index, 1);
    return formatQuery(query);
}

/** Remove one filter value, returning the new query string. */
export function removeFilter(input: string, field: Field, value: string): string {
    const query = parseQuery(input);
    const bucket = field === 'level' ? query.levels : query.containers;
    const index = bucket.indexOf(value);
    if (index !== -1) bucket.splice(index, 1);
    return formatQuery(query);
}

/**
 * Case-insensitive match ranges for highlighting. Returns `[start, end)` pairs
 * over `text`; empty when there's nothing to highlight.
 */
export function findMatches(text: string, needle: string): [number, number][] {
    if (!needle) return [];
    const haystack = text.toLowerCase();
    const target = needle.toLowerCase();
    const ranges: [number, number][] = [];
    let from = 0;
    for (;;) {
        const at = haystack.indexOf(target, from);
        if (at === -1) break;
        ranges.push([at, at + target.length]);
        from = at + target.length;
    }
    return ranges;
}
