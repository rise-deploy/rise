import Ajv, { type ValidateFunction } from 'ajv';

export interface JsonValidation {
    jsonValid: boolean;
    schemaValid: boolean;
    errors: string[];
}

// A single Ajv instance with a per-schema compiled-validator cache keeps
// keystroke-driven validation (e.g. extension spec editors) cheap.
const ajv = new Ajv({ allErrors: true, strict: false });
const validatorCache = new WeakMap<object, ValidateFunction | null>();

function getValidator(schema: object): ValidateFunction | null {
    if (validatorCache.has(schema)) {
        return validatorCache.get(schema) ?? null;
    }
    try {
        const validate = ajv.compile(schema);
        validatorCache.set(schema, validate);
        return validate;
    } catch {
        // Cache the failure so we don't recompile a broken schema on every call.
        validatorCache.set(schema, null);
        return null;
    }
}

/** Parse `text` as JSON and, when a schema is supplied, validate it with Ajv. */
export function validateJson(text: string, schema?: unknown): JsonValidation {
    let parsed: unknown;
    try {
        parsed = JSON.parse(text);
    } catch (err: any) {
        return { jsonValid: false, schemaValid: false, errors: [`Invalid JSON: ${err.message}`] };
    }
    if (!schema || typeof schema !== 'object') {
        return { jsonValid: true, schemaValid: true, errors: [] };
    }
    const validate = getValidator(schema as object);
    if (!validate) {
        // A malformed schema must not block editing — treat as no schema.
        return { jsonValid: true, schemaValid: true, errors: [] };
    }
    if (validate(parsed)) {
        return { jsonValid: true, schemaValid: true, errors: [] };
    }
    const errors = (validate.errors || []).map((e) => {
        const where = e.instancePath || '(root)';
        return `${where} ${e.message}`.trim();
    });
    return { jsonValid: true, schemaValid: false, errors };
}
