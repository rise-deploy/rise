import { useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { copyToClipboard } from '../lib/utils';
import { useToast } from '../components/toast';
import { Avatar, Button, Field, Input, Panel, PanelBody, PanelHead, Pill, Segmented, colorFor } from '../components/r-ui';

export type TokenEndpointAuthMethod = 'client_secret_post' | 'client_secret_basic';

export interface OAuthSpec {
    provider_name?: string;
    description?: string;
    client_id?: string;
    client_secret_encrypted?: string;
    issuer_url?: string;
    authorization_endpoint?: string;
    token_endpoint?: string;
    token_endpoint_auth_method?: TokenEndpointAuthMethod;
    scopes?: string[];
    [key: string]: unknown;
}

type EndpointMode = 'discovery' | 'manual';

interface ProviderTemplate {
    id: string;
    name: string;
    tagline: string;
    /** Where the OAuth client is registered, if the provider has a console for it. */
    consoleUrl?: string;
    /** Provider-specific registration steps, shown next to the redirect URI. */
    setup: string[];
    endpointMode: EndpointMode;
    issuerUrl: string;
    authorizationEndpoint: string;
    tokenEndpoint: string;
    authMethod: TokenEndpointAuthMethod;
    scopes: string[];
}

export const OAUTH_PROVIDER_TEMPLATES: ProviderTemplate[] = [
    {
        id: 'google',
        name: 'Google',
        tagline: 'Sign in with Google (OIDC)',
        consoleUrl: 'https://console.cloud.google.com/apis/credentials',
        setup: ['Create an OAuth client ID of type "Web application".', 'Add the redirect URI under "Authorized redirect URIs".'],
        endpointMode: 'discovery',
        issuerUrl: 'https://accounts.google.com',
        authorizationEndpoint: '',
        tokenEndpoint: '',
        authMethod: 'client_secret_post',
        scopes: ['openid', 'email', 'profile'],
    },
    {
        id: 'github',
        name: 'GitHub',
        tagline: 'Sign in with GitHub (OAuth 2.0)',
        consoleUrl: 'https://github.com/settings/developers',
        setup: ['Create a new OAuth App.', 'Use the redirect URI as the "Authorization callback URL".'],
        endpointMode: 'manual',
        issuerUrl: 'https://github.com',
        authorizationEndpoint: 'https://github.com/login/oauth/authorize',
        tokenEndpoint: 'https://github.com/login/oauth/access_token',
        authMethod: 'client_secret_post',
        scopes: ['read:user', 'user:email'],
    },
    {
        id: 'notion',
        name: 'Notion',
        tagline: 'Connect a Notion workspace',
        consoleUrl: 'https://www.notion.so/profile/integrations',
        setup: [
            'Create a new integration and set its type to "Public".',
            'Add the redirect URI under "Redirect URIs".',
            'Copy the OAuth client ID and secret from the integration\'s configuration.',
            'Notion has no scopes: users choose the pages to share on the consent screen.',
        ],
        endpointMode: 'manual',
        issuerUrl: 'https://api.notion.com',
        authorizationEndpoint: 'https://api.notion.com/v1/oauth/authorize?owner=user',
        tokenEndpoint: 'https://api.notion.com/v1/oauth/token',
        authMethod: 'client_secret_basic',
        scopes: [],
    },
    {
        id: 'snowflake',
        name: 'Snowflake',
        tagline: 'Snowflake OAuth for a custom client',
        setup: [
            'Create a SECURITY INTEGRATION of TYPE = OAUTH with OAUTH_CLIENT = CUSTOM and the redirect URI as OAUTH_REDIRECT_URI.',
            'Read the client ID and secret with SYSTEM$SHOW_OAUTH_CLIENT_SECRETS.',
            'Replace YOUR_ACCOUNT in the endpoints below with your account identifier.',
            'The Snowflake OAuth Provisioner extension can create the integration and this extension for you.',
        ],
        endpointMode: 'manual',
        issuerUrl: 'https://YOUR_ACCOUNT.snowflakecomputing.com',
        authorizationEndpoint: 'https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/authorize',
        tokenEndpoint: 'https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/token-request',
        authMethod: 'client_secret_post',
        scopes: ['refresh_token'],
    },
    {
        id: 'custom-oidc',
        name: 'Custom OIDC',
        tagline: 'Any provider with OIDC discovery',
        setup: ['Register an OAuth client with your provider and allow the redirect URI.'],
        endpointMode: 'discovery',
        issuerUrl: '',
        authorizationEndpoint: '',
        tokenEndpoint: '',
        authMethod: 'client_secret_post',
        scopes: ['openid', 'email', 'profile'],
    },
    {
        id: 'custom-oauth2',
        name: 'Custom OAuth 2.0',
        tagline: 'Enter the endpoints by hand',
        setup: ['Register an OAuth client with your provider and allow the redirect URI.'],
        endpointMode: 'manual',
        issuerUrl: '',
        authorizationEndpoint: '',
        tokenEndpoint: '',
        authMethod: 'client_secret_post',
        scopes: [],
    },
];

const AUTH_METHOD_OPTIONS: { value: TokenEndpointAuthMethod; label: string }[] = [
    { value: 'client_secret_post', label: 'Request body' },
    { value: 'client_secret_basic', label: 'HTTP Basic header' },
];

// Keys this form owns; every other key in the spec is passed through untouched.
const MANAGED_KEYS = [
    'provider_name',
    'description',
    'client_id',
    'client_secret_encrypted',
    'issuer_url',
    'authorization_endpoint',
    'token_endpoint',
    'token_endpoint_auth_method',
    'scopes',
];

/** The template an existing spec was built from, matched on its endpoints. */
function detectTemplate(spec: OAuthSpec): ProviderTemplate | null {
    if (!spec.issuer_url && !spec.authorization_endpoint && !spec.token_endpoint) return null;
    const known = OAUTH_PROVIDER_TEMPLATES.find(t =>
        !t.id.startsWith('custom-')
        && t.issuerUrl === spec.issuer_url
        && t.authorizationEndpoint === (spec.authorization_endpoint || '')
        && t.tokenEndpoint === (spec.token_endpoint || ''));
    if (known) return known;
    const custom = spec.authorization_endpoint || spec.token_endpoint ? 'custom-oauth2' : 'custom-oidc';
    return OAUTH_PROVIDER_TEMPLATES.find(t => t.id === custom) ?? null;
}

// Scope tokens cannot contain whitespace (RFC 6749 §3.3), so both commas and
// whitespace separate them.
function parseScopes(value: string): string[] {
    return value.split(/[\s,]+/).filter(s => s.length > 0);
}

function originOf(url: string): string {
    try {
        return new URL(url).origin;
    } catch {
        return '';
    }
}

const ENCRYPT_DEBOUNCE_MS = 500;

interface OAuthExtensionUIProps {
    spec: OAuthSpec | null | undefined;
    onChange: (spec: OAuthSpec) => void;
    projectName?: string;
    instanceName?: string;
    isEnabled?: boolean;
}

export function OAuthExtensionUI({ spec: initialSpec, onChange, projectName, instanceName, isEnabled }: OAuthExtensionUIProps) {
    const spec = initialSpec || {};
    const initialTemplate = detectTemplate(spec);
    const { showToast } = useToast();

    const [templateId, setTemplateId] = useState<string | null>(initialTemplate?.id ?? null);
    const [providerName, setProviderName] = useState(spec.provider_name || '');
    const [description, setDescription] = useState(spec.description || '');
    const [clientId, setClientId] = useState(spec.client_id || '');
    const [clientSecretEncrypted, setClientSecretEncrypted] = useState(spec.client_secret_encrypted || '');
    const [clientSecretPlaintext, setClientSecretPlaintext] = useState('');
    const [isEncrypting, setIsEncrypting] = useState(false);
    const [showSecret, setShowSecret] = useState(false);
    const [endpointMode, setEndpointMode] = useState<EndpointMode>(
        spec.authorization_endpoint || spec.token_endpoint ? 'manual' : 'discovery',
    );
    const [issuerUrl, setIssuerUrl] = useState(spec.issuer_url || '');
    const [authorizationEndpoint, setAuthorizationEndpoint] = useState(spec.authorization_endpoint || '');
    const [tokenEndpoint, setTokenEndpoint] = useState(spec.token_endpoint || '');
    const [authMethod, setAuthMethod] = useState<TokenEndpointAuthMethod>(spec.token_endpoint_auth_method || 'client_secret_post');
    const [scopes, setScopes] = useState((spec.scopes || []).join(' '));

    const passthroughRef = useRef<OAuthSpec>(
        Object.fromEntries(Object.entries(spec).filter(([k]) => !MANAGED_KEYS.includes(k))),
    );
    const onChangeRef = useRef(onChange);
    useEffect(() => {
        onChangeRef.current = onChange;
    }, [onChange]);

    const template = OAUTH_PROVIDER_TEMPLATES.find(t => t.id === templateId) ?? null;
    const backendUrl = CONFIG.backendUrl.replace(/\/$/, '');
    const extensionName = instanceName || (isEnabled ? '' : 'YOUR_EXTENSION_NAME');
    const redirectUri = `${backendUrl}/oidc/${projectName || 'YOUR_PROJECT'}/${extensionName}/callback`;
    const scopeList = parseScopes(scopes);
    const secretPending = clientSecretPlaintext.trim() !== '';

    const applyTemplate = (next: ProviderTemplate) => {
        const previousName = template?.name;
        setTemplateId(next.id);
        if (!providerName || providerName === previousName) {
            setProviderName(next.id.startsWith('custom-') ? '' : next.name);
        }
        setEndpointMode(next.endpointMode);
        setIssuerUrl(next.issuerUrl);
        setAuthorizationEndpoint(next.authorizationEndpoint);
        setTokenEndpoint(next.tokenEndpoint);
        setAuthMethod(next.authMethod);
        setScopes(next.scopes.join(' '));
    };

    // Encrypt the secret shortly after typing stops (or on blur) so a pasted
    // secret is never lost by forgetting to press a button.
    const plaintextRef = useRef('');
    plaintextRef.current = clientSecretPlaintext;
    const encryptTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
    const encryptRequestRef = useRef(0);
    const encryptNow = async () => {
        clearTimeout(encryptTimerRef.current);
        const plaintext = plaintextRef.current;
        if (plaintext.trim() === '') return;
        const request = ++encryptRequestRef.current;
        setIsEncrypting(true);
        try {
            const response = await api.encryptSecret(plaintext);
            // Typing during the request makes the result stale; the next run replaces it.
            if (request !== encryptRequestRef.current || plaintextRef.current !== plaintext) return;
            setClientSecretEncrypted(response.encrypted);
            setClientSecretPlaintext('');
            setShowSecret(false);
        } catch (err) {
            if (request !== encryptRequestRef.current) return;
            const message = err instanceof Error ? err.message : String(err);
            showToast(
                message.includes('429') || message.includes('rate limit')
                    ? 'Rate limit exceeded while encrypting the client secret. Try again later.'
                    : `Failed to encrypt client secret: ${message}`,
                'error',
            );
        } finally {
            if (request === encryptRequestRef.current) setIsEncrypting(false);
        }
    };
    useEffect(() => {
        if (!secretPending) return;
        encryptTimerRef.current = setTimeout(encryptNow, ENCRYPT_DEBOUNCE_MS);
        return () => clearTimeout(encryptTimerRef.current);
    }, [clientSecretPlaintext]);

    useEffect(() => {
        const next: OAuthSpec = {
            ...passthroughRef.current,
            provider_name: providerName,
            client_id: clientId,
            issuer_url: issuerUrl,
            scopes: scopeList,
        };
        if (description.trim() !== '') next.description = description;
        // A secret still being typed withholds the stored one, so saving in
        // that moment fails validation instead of silently keeping the old secret.
        if (clientSecretEncrypted && !secretPending) next.client_secret_encrypted = clientSecretEncrypted;
        if (endpointMode === 'manual') {
            if (authorizationEndpoint.trim() !== '') next.authorization_endpoint = authorizationEndpoint.trim();
            if (tokenEndpoint.trim() !== '') next.token_endpoint = tokenEndpoint.trim();
        }
        if (authMethod !== 'client_secret_post') next.token_endpoint_auth_method = authMethod;
        onChangeRef.current(next);
    }, [providerName, description, clientId, clientSecretEncrypted, secretPending, endpointMode, issuerUrl, authorizationEndpoint, tokenEndpoint, authMethod, scopes]);

    const copyRedirectUri = async () => {
        try {
            await copyToClipboard(redirectUri);
            showToast('Redirect URI copied to clipboard', 'success');
        } catch (err) {
            showToast(`Failed to copy: ${err instanceof Error ? err.message : String(err)}`, 'error');
        }
    };

    const secretStatus = isEncrypting
        ? 'Encrypting…'
        : secretPending
            ? 'Encrypted automatically when you stop typing.'
            : clientSecretEncrypted
                ? 'Stored encrypted. Enter a new value to replace it.'
                : 'Paste the client secret from your provider. It is encrypted before it is saved.';

    return (
        <div className="r-stack">
            <Panel>
                <PanelHead title="Provider" sub="Pick a provider to prefill its endpoints, or start from a custom one." />
                <PanelBody>
                    <div className="r-quickstart-grid">
                        {OAUTH_PROVIDER_TEMPLATES.map(t => (
                            <button
                                key={t.id}
                                type="button"
                                className={`r-quickstart-card${t.id === templateId ? ' selected' : ''}`}
                                aria-pressed={t.id === templateId}
                                onClick={() => applyTemplate(t)}
                            >
                                <Avatar label={t.name} color={colorFor(t.id)} size={32} />
                                <div className="r-quickstart-card__body">
                                    <div className="r-quickstart-card__title">{t.name}</div>
                                    <div className="r-quickstart-card__tagline">{t.tagline}</div>
                                </div>
                            </button>
                        ))}
                    </div>
                </PanelBody>
            </Panel>

            <Panel>
                <PanelHead
                    title={template && !template.id.startsWith('custom-') ? `Register Rise with ${template.name}` : 'Register Rise with your provider'}
                    right={template?.consoleUrl && (
                        <a className="r-btn small" href={template.consoleUrl} target="_blank" rel="noopener noreferrer">
                            Open {template.name} console
                        </a>
                    )}
                />
                <PanelBody>
                    <Field
                        label="Redirect URI"
                        hint="Register this as the only callback URL. Rise forwards to your app's URLs and localhost from here."
                    >
                        <div style={{ display: 'flex', gap: 8 }}>
                            <Input readOnly value={redirectUri} className="mono" onFocus={e => e.currentTarget.select()} />
                            <Button icon="copy" onClick={copyRedirectUri}>Copy</Button>
                        </div>
                    </Field>
                    {template && (
                        <ol style={{ listStyle: 'decimal', fontSize: 12.5, color: 'var(--text-muted)', paddingLeft: 18, margin: '12px 0 0', display: 'flex', flexDirection: 'column', gap: 4 }}>
                            {template.setup.map(step => <li key={step}>{step}</li>)}
                        </ol>
                    )}
                </PanelBody>
            </Panel>

            <Panel>
                <PanelHead title="Client credentials" />
                <PanelBody>
                    <div className="r-stack">
                        <Field label="Client ID">
                            <Input value={clientId} onChange={e => setClientId(e.target.value)} placeholder="Client ID from your provider" />
                        </Field>
                        <Field
                            label={<>Client secret {clientSecretEncrypted && !secretPending && <Pill kind="accent">stored</Pill>}</>}
                            hint={secretStatus}
                        >
                            <div style={{ display: 'flex', gap: 8 }}>
                                <Input
                                    type={showSecret ? 'text' : 'password'}
                                    autoComplete="off"
                                    value={clientSecretPlaintext}
                                    onChange={e => setClientSecretPlaintext(e.target.value)}
                                    onBlur={encryptNow}
                                    placeholder={clientSecretEncrypted ? '••••••••' : 'Client secret'}
                                />
                                <Button
                                    icon={showSecret ? 'eyeoff' : 'eye'}
                                    onClick={() => setShowSecret(!showSecret)}
                                    disabled={!secretPending}
                                    aria-label={showSecret ? 'Hide secret' : 'Show secret'}
                                />
                            </div>
                        </Field>
                    </div>
                </PanelBody>
            </Panel>

            <Panel>
                <PanelHead
                    title="Endpoints"
                    right={
                        <Segmented<EndpointMode>
                            value={endpointMode}
                            onChange={setEndpointMode}
                            options={[
                                { value: 'discovery', label: 'OIDC discovery' },
                                { value: 'manual', label: 'Manual' },
                            ]}
                        />
                    }
                />
                <PanelBody>
                    <div className="r-stack">
                        {endpointMode === 'manual' && (
                            <>
                                <Field label="Authorization endpoint" hint="Query parameters in this URL are kept, e.g. Notion's owner=user.">
                                    <Input
                                        value={authorizationEndpoint}
                                        onChange={e => setAuthorizationEndpoint(e.target.value)}
                                        onBlur={() => { if (!issuerUrl) setIssuerUrl(originOf(authorizationEndpoint)); }}
                                        placeholder="https://provider.example.com/oauth/authorize"
                                    />
                                </Field>
                                <Field label="Token endpoint">
                                    <Input
                                        value={tokenEndpoint}
                                        onChange={e => setTokenEndpoint(e.target.value)}
                                        placeholder="https://provider.example.com/oauth/token"
                                    />
                                </Field>
                            </>
                        )}
                        <Field
                            label="Issuer URL"
                            hint={endpointMode === 'discovery'
                                ? 'Endpoints are read from <issuer>/.well-known/openid-configuration.'
                                : 'Identifies the provider. Discovery from it fills any endpoint left empty above.'}
                        >
                            <Input value={issuerUrl} onChange={e => setIssuerUrl(e.target.value)} placeholder="https://accounts.google.com" />
                        </Field>
                        <Field
                            label="Client authentication"
                            hint={authMethod === 'client_secret_basic'
                                ? 'Rise sends the client ID and secret as an HTTP Basic Authorization header (client_secret_basic).'
                                : 'Rise sends the client ID and secret in the token request body (client_secret_post). Switch to HTTP Basic if the provider answers invalid_client.'}
                        >
                            <Segmented<TokenEndpointAuthMethod> value={authMethod} onChange={setAuthMethod} options={AUTH_METHOD_OPTIONS} />
                        </Field>
                    </div>
                </PanelBody>
            </Panel>

            <Panel>
                <PanelHead title="Scopes & display" />
                <PanelBody>
                    <div className="r-stack">
                        <Field
                            label="Scopes"
                            hint={scopeList.length === 0
                                ? 'No scopes: the authorization request carries no scope parameter.'
                                : <span style={{ display: 'inline-flex', flexWrap: 'wrap', gap: 4 }}>{scopeList.map(s => <Pill key={s} kind="accent">{s}</Pill>)}</span>}
                        >
                            <Input value={scopes} onChange={e => setScopes(e.target.value)} placeholder="Space-separated, e.g. openid email profile" />
                        </Field>
                        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
                            <Field label="Display name">
                                <Input value={providerName} onChange={e => setProviderName(e.target.value)} placeholder="e.g. Google" />
                            </Field>
                            <Field label="Description (optional)">
                                <Input value={description} onChange={e => setDescription(e.target.value)} placeholder="e.g. Sign in with Google" />
                            </Field>
                        </div>
                    </div>
                </PanelBody>
            </Panel>

        </div>
    );
}
