import Ajv from 'ajv';

export interface JsonValidation {
    jsonValid: boolean;
    schemaValid: boolean;
    errors: string[];
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
    try {
        const ajv = new Ajv({ allErrors: true, strict: false });
        const validate = ajv.compile(schema as object);
        if (validate(parsed)) {
            return { jsonValid: true, schemaValid: true, errors: [] };
        }
        const errors = (validate.errors || []).map((e) => {
            const where = e.instancePath || '(root)';
            return `${where} ${e.message}`.trim();
        });
        return { jsonValid: true, schemaValid: false, errors };
    } catch {
        // A malformed schema must not block editing — treat as no schema.
        return { jsonValid: true, schemaValid: true, errors: [] };
    }
}
