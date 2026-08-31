/**
 * Minimal Server-Sent Events reader over `fetch`.
 *
 * `EventSource` can't be used here: the log endpoint authenticates with a
 * cookie that needs `credentials: 'include'`, and callers need an
 * `AbortController` to cancel a stream when filters change.
 *
 * Framing follows the SSE spec closely enough for this endpoint: fields are
 * accumulated until a blank line dispatches the event, `data:` may repeat
 * within one event (joined with newlines), and a single optional space after
 * the colon is stripped.
 *
 * Event types are handled by name and unknown ones are dropped, so a server
 * that grows new events does not spill their payloads into the log output.
 */

/** The wire shape of an `event: log` payload. */
export interface LogLinePayload {
    /** Stable per-line identity from the backend, when it supplies one. */
    id?: string;
    /** The raw line, optionally prefixed with an RFC3339 timestamp. */
    line: string;
    /** Backend-emitted level classification. */
    level: string;
    /** Attribution the backend can supply; absent on older servers. */
    container?: string;
    replica?: string;
    stream?: 'stdout' | 'stderr';
}

/**
 * Continuation token for backward pagination. `null` means there is nothing
 * older to fetch — the client must not synthesise one.
 */
export interface CursorPayload {
    next_cursor: string | null;
}

export interface SseHandlers {
    onLine?: (payload: LogLinePayload) => void;
    onStatus?: (payload: string) => void;
    /** Ends the backlog phase of a follow stream; carries a continuation cursor. */
    onBacklogComplete?: (payload: CursorPayload & { count?: number }) => void;
    /** Ends a finite historical page. */
    onPageComplete?: (payload: CursorPayload) => void;
    /** Refreshes the continuation cursor while a follow stream is attached. */
    onCursor?: (payload: CursorPayload) => void;
    /**
     * Called at most once per stream when a payload doesn't parse. The line is
     * still delivered verbatim with `level: 'unknown'`.
     */
    onMalformed?: (err: unknown) => void;
}

/**
 * A cursor event that doesn't parse is treated as "no continuation" rather
 * than as an error: losing the ability to page back is recoverable, inventing
 * a cursor is not — the server rejects a malformed one with a 400.
 */
function parseCursorPayload(data: string): CursorPayload {
    try {
        const obj = JSON.parse(data) as Record<string, unknown>;
        return {
            next_cursor: typeof obj?.next_cursor === 'string' ? obj.next_cursor : null,
        };
    } catch {
        return { next_cursor: null };
    }
}

function readCount(data: string): number | undefined {
    try {
        const obj = JSON.parse(data) as Record<string, unknown>;
        return typeof obj?.count === 'number' ? obj.count : undefined;
    } catch {
        return undefined;
    }
}

function parseLinePayload(data: string): LogLinePayload | null {
    const obj = JSON.parse(data) as unknown;
    if (!obj || typeof obj !== 'object') return null;
    const record = obj as Record<string, unknown>;
    if (typeof record.line !== 'string') return null;
    return {
        id: typeof record.id === 'string' ? record.id : undefined,
        line: record.line,
        level: typeof record.level === 'string' ? record.level : 'unknown',
        container: typeof record.container === 'string' ? record.container : undefined,
        replica: typeof record.replica === 'string' ? record.replica : undefined,
        stream: record.stream === 'stdout' || record.stream === 'stderr' ? record.stream : undefined,
    };
}

export async function readSseResponse(response: Response, handlers: SseHandlers): Promise<void> {
    const reader = response.body?.getReader();
    if (!reader) {
        throw new Error('Log stream response did not include a body');
    }

    const decoder = new TextDecoder();
    let buffer = '';
    let eventType = 'message';
    let data: string[] = [];
    // Per-stream, not module-global: one noisy deployment shouldn't silence the
    // warning for every other stream opened on the same page.
    let warnedMalformed = false;

    const dispatch = () => {
        if (data.length === 0) {
            eventType = 'message';
            return;
        }
        const payload = data.join('\n');
        data = [];
        const type = eventType;
        eventType = 'message';

        if (type === 'status') {
            handlers.onStatus?.(payload);
            return;
        }
        if (type === 'backlog_complete') {
            const parsed = parseCursorPayload(payload);
            handlers.onBacklogComplete?.({
                ...parsed,
                count: readCount(payload),
            });
            return;
        }
        if (type === 'page_complete') {
            handlers.onPageComplete?.(parseCursorPayload(payload));
            return;
        }
        if (type === 'cursor') {
            handlers.onCursor?.(parseCursorPayload(payload));
            return;
        }
        // Only `log` carries a line. Anything else is protocol chatter — a
        // server may add event types this client predates, and treating an
        // unknown one as a line would render its payload as log output.
        if (type !== 'log' && type !== 'message') return;
        if (!payload.trim()) return;

        let parsed: LogLinePayload | null = null;
        let err: unknown = null;
        try {
            parsed = parseLinePayload(payload);
            if (!parsed) err = new Error('payload has no string `line` field');
        } catch (e) {
            err = e;
        }
        if (!parsed) {
            // Render what we got rather than dropping the line.
            parsed = { line: payload, level: 'unknown' };
            if (!warnedMalformed) {
                warnedMalformed = true;
                handlers.onMalformed?.(err);
            }
        }
        handlers.onLine?.(parsed);
    };

    const handleField = (raw: string) => {
        // Tolerate CRLF framing.
        const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
        if (line === '') {
            dispatch();
            return;
        }
        // A leading colon is a comment (keep-alive ping).
        if (line.startsWith(':')) return;
        const colon = line.indexOf(':');
        const field = colon === -1 ? line : line.slice(0, colon);
        let value = colon === -1 ? '' : line.slice(colon + 1);
        if (value.startsWith(' ')) value = value.slice(1);

        if (field === 'event') eventType = value;
        else if (field === 'data') data.push(value);
        // `id` and `retry` are unused by this endpoint.
    };

    for (;;) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n');
        // `split` always yields at least one element, so this is never
        // undefined — but be explicit rather than relying on coercion.
        buffer = lines.pop() ?? '';
        for (const line of lines) handleField(line);
    }

    // A trailing event with no blank line after it is incomplete, and the SSE
    // spec says to discard pending data at end of stream. Dispatching it
    // instead would surface a truncated payload as a garbage log line.
}
