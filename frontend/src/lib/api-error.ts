/**
 * An error response from the Rise API.
 *
 * The backend answers errors with `{"error": "...", "suggestions"?: [...],
 * "error_code"?: "...", "details"?: {...}}`; `message` is the `error`
 * sentence, ready for a toast.
 */
export class ApiError extends Error {
    readonly status: number;
    /** Stable code for errors a client branches on, e.g. `platform_access_denied`. */
    readonly code?: string;
    /** Close matches the backend offers, e.g. for a misspelled team name. */
    readonly suggestions?: string[];
    /** Structured context for `code`, e.g. the barriers a `migration_barrier` crossed. */
    readonly details?: unknown;

    constructor(status: number, message: string, code?: string, suggestions?: string[], details?: unknown) {
        super(message);
        this.name = 'ApiError';
        this.status = status;
        this.code = code;
        this.suggestions = suggestions;
        this.details = details;
    }

    /** The account may not use the dashboard at all (as opposed to one resource). */
    get isPlatformAccessDenied(): boolean {
        return this.code === PLATFORM_ACCESS_DENIED;
    }
}

export const PLATFORM_ACCESS_DENIED = 'platform_access_denied';

/** Build an `ApiError` from a failed response, whatever its body looks like. */
export async function apiErrorFromResponse(response: Response): Promise<ApiError> {
    const text = await response.text().catch(() => '');
    let message = text.trim();
    let code: string | undefined;
    let suggestions: string[] | undefined;
    let details: unknown;
    try {
        const body = JSON.parse(text);
        if (body && typeof body.error === 'string') message = body.error;
        if (body && typeof body.error_code === 'string') code = body.error_code;
        if (body && Array.isArray(body.suggestions)) suggestions = body.suggestions.map(String);
        if (body && body.details !== undefined && body.details !== null) details = body.details;
    } catch {
        // Not JSON: keep the text.
    }
    if (!message) message = `Request failed (${response.status} ${response.statusText || 'error'})`;
    return new ApiError(response.status, message, code, suggestions, details);
}

export function isApiError(err: unknown, status?: number): err is ApiError {
    return err instanceof ApiError && (status === undefined || err.status === status);
}

/** The error's message for display, whatever was thrown. */
export function errorText(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}
