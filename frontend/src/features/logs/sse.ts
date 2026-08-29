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
 */

/** The wire shape of an `event: log` payload. */
export interface LogLinePayload {
    /** The raw line, optionally prefixed with an RFC3339 timestamp. */
    line: string;
    /** Backend-emitted level classification. */
    level: string;
    /** Attribution the backend can supply; absent on older servers. */
    container?: string;
    replica?: string;
    stream?: 'stdout' | 'stderr';
}

export interface SseHandlers {
    onLine?: (payload: LogLinePayload) => void;
    onStatus?: (payload: string) => void;
    onBacklogComplete?: (payload: string) => void;
    /**
     * Called at most once per stream when a payload doesn't parse. The line is
     * still delivered verbatim with `level: 'unknown'`.
     */
    onMalformed?: (err: unknown) => void;
}

function parseLinePayload(data: string): LogLinePayload | null {
    const obj = JSON.parse(data) as unknown;
    if (!obj || typeof obj !== 'object') return null;
    const record = obj as Record<string, unknown>;
    if (typeof record.line !== 'string') return null;
    return {
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
            handlers.onBacklogComplete?.(payload);
            return;
        }
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

    // Flush whatever the server left unterminated.
    buffer += decoder.decode();
    if (buffer) handleField(buffer);
    dispatch();
}
