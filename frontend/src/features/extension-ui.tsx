// @ts-nocheck
import { useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { copyToClipboard, formatDate } from '../lib/utils';
import { useToast } from '../components/toast';
import { Icon } from '../components/icon';
import {
    Alert,
    Button,
    Combobox,
    Empty,
    Field,
    Input,
    KV,
    KVRow,
    Modal,
    Panel,
    PanelBody,
    PanelHead,
    Pill,
    Status,
    Tabs,
    Textarea,
} from '../components/r-ui';

// Mono-style code block matching the new design system: subtle border, surface-2
// background, small monospace text. Used for JSON specs and example snippets.
function CodeBlock({ children, style }: { children: React.ReactNode; style?: React.CSSProperties }) {
    return (
        <pre
            className="mono"
            style={{
                fontSize: 12,
                lineHeight: 1.55,
                overflow: 'auto',
                margin: 0,
                padding: '10px 12px',
                background: 'var(--surface-2)',
                border: '1px solid var(--border)',
                borderRadius: 'var(--radius-sm)',
                color: 'var(--text)',
                whiteSpace: 'pre',
            }}
        >
            {children}
        </pre>
    );
}

// Inline copy icon button used in env-var tables.
function CopyButton({ onClick, title }: { onClick: () => void; title?: string }) {
    return (
        <button
            type="button"
            onClick={onClick}
            title={title || 'Copy to clipboard'}
            style={{
                display: 'inline-flex',
                alignItems: 'center',
                justifyContent: 'center',
                padding: 4,
                background: 'transparent',
                border: 'none',
                borderRadius: 'var(--radius-sm)',
                color: 'var(--text-muted)',
                cursor: 'pointer',
            }}
        >
            <Icon name="copy" size={14} />
        </button>
    );
}

function renderStatePill(label) {
    if (!label) return null;
    return <Status status={String(label)} />;
}


// AWS RDS Extension UI Component
export function AwsRdsExtensionUI({ spec, schema, onChange }) {
    const [engine, setEngine] = useState(spec?.engine || 'postgres');
    const [engineVersion, setEngineVersion] = useState(spec?.engine_version || '');
    const [databaseIsolation, setDatabaseIsolation] = useState(spec?.database_isolation || 'shared');
    const [databaseUrlEnvVar, setDatabaseUrlEnvVar] = useState(spec?.database_url_env_var || 'DATABASE_URL');
    const [injectPgVars, setInjectPgVars] = useState(spec?.inject_pg_vars !== false);

    // Extract default engine version from schema
    const defaultEngineVersion = schema?.properties?.engine_version?.default || '';

    // Use a ref to store the latest onChange callback
    const onChangeRef = useRef(onChange);
    useEffect(() => {
        onChangeRef.current = onChange;
    }, [onChange]);

    // Update parent when values change
    useEffect(() => {
        // Build the spec object, omitting empty values
        const newSpec = {
            engine,
            database_isolation: databaseIsolation,
            inject_pg_vars: injectPgVars,
        };

        // Only include engine_version if it's not empty
        if (engineVersion) {
            newSpec.engine_version = engineVersion;
        }

        // Only include database_url_env_var if it's not empty
        if (databaseUrlEnvVar && databaseUrlEnvVar.trim() !== '') {
            newSpec.database_url_env_var = databaseUrlEnvVar;
        }

        onChangeRef.current(newSpec);
    }, [engine, engineVersion, databaseIsolation, databaseUrlEnvVar, injectPgVars]);

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
            <Field label="Database Engine">
                <Combobox
                    id="rds-engine"
                    value={engine}
                    onChange={setEngine}
                    options={[{ value: 'postgres', label: 'PostgreSQL' }]}
                />
            </Field>

            <Field label="Engine Version (Optional)">
                <Input
                    id="rds-engine-version"
                    value={engineVersion}
                    onChange={(e) => setEngineVersion(e.target.value)}
                    placeholder={defaultEngineVersion || 'e.g., 16.2'}
                />
            </Field>

            <Field label="Database Isolation">
                <Combobox
                    id="rds-database-isolation"
                    value={databaseIsolation}
                    onChange={setDatabaseIsolation}
                    options={[
                        { value: 'shared', label: 'Shared (All deployment groups use same database)' },
                        { value: 'isolated', label: 'Isolated (Each deployment group gets own database)' },
                    ]}
                />
            </Field>

            <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                <div className="r-section-title">Environment Variables</div>
                <Field
                    label="Database URL Environment Variable"
                    hint="Environment variable name for the database connection string (e.g., DATABASE_URL, POSTGRES_URL). Leave empty to disable."
                >
                    <Input
                        id="rds-database-url-env-var"
                        value={databaseUrlEnvVar}
                        onChange={(e) => setDatabaseUrlEnvVar(e.target.value)}
                        placeholder="DATABASE_URL"
                    />
                </Field>
                <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, color: 'var(--text)', cursor: 'pointer' }}>
                    <input
                        type="checkbox"
                        checked={injectPgVars}
                        onChange={(e) => setInjectPgVars(e.target.checked)}
                    />
                    <span>
                        Inject <code className="mono" style={{ color: 'var(--accent)' }}>PG*</code> variables
                    </span>
                </label>
            </div>

            <Alert tone="info" icon="info">
                <strong>Isolation modes.</strong> <strong>Shared:</strong> all deployment groups (default, staging, etc.) use the same database.{' '}
                <strong>Isolated:</strong> each deployment group gets its own database.
            </Alert>
        </div>
    );
}

// Extension UI API
// Each extension can provide custom implementations for:
// - renderStatusBadge(extension): Custom status badge component
// - renderOverviewTab(extension): Custom overview/detail view component
// - renderConfigureTab(spec, schema, onChange): Custom configuration form component
// - icon: Icon URL for the extension

const AwsRdsExtensionAPI = {
    icon: '/assets/aws_rds_aurora.jpg',

    renderStatusBadge(extension) {
        const status = extension.status || {};
        if (!status.state) return null;
        return renderStatePill(status.state);
    },

    renderOverviewTab(extension, projectName) {
        return <AwsRdsDetailView extension={extension} projectName={projectName} />;
    },

    renderConfigureTab(spec, schema, onChange, projectName, instanceName, isEnabled) {
        return <AwsRdsExtensionUI spec={spec} schema={schema} onChange={onChange} />;
    }
};

// OAuth Extension UI Component
export function OAuthExtensionUI({ spec, schema, onChange, projectName, instanceName, isEnabled }) {
    const [providerName, setProviderName] = useState(spec?.provider_name || '');
    const [description, setDescription] = useState(spec?.description || '');
    const [clientId, setClientId] = useState(spec?.client_id || '');
    const [clientSecretPlaintext, setClientSecretPlaintext] = useState('');
    const [clientSecretEncrypted, setClientSecretEncrypted] = useState(spec?.client_secret_encrypted || '');
    const [hasExistingSecret, setHasExistingSecret] = useState(!!spec?.client_secret_encrypted);
    const [showSecret, setShowSecret] = useState(false);
    const [isEncrypting, setIsEncrypting] = useState(false);
    const [issuerUrl, setIssuerUrl] = useState(spec?.issuer_url || '');
    const [authorizationEndpoint, setAuthorizationEndpoint] = useState(spec?.authorization_endpoint || '');
    const [tokenEndpoint, setTokenEndpoint] = useState(spec?.token_endpoint || '');
    const [showAdvanced, setShowAdvanced] = useState(!!(spec?.authorization_endpoint || spec?.token_endpoint));
    const [scopes, setScopes] = useState(spec?.scopes?.join(', ') || '');
    const [setupStep, setSetupStep] = useState(1);
    const { showToast } = useToast();

    // Build the redirect URI for display
    const backendUrl = CONFIG.backendUrl.replace(/\/$/, ''); // Remove trailing slash
    const displayProjectName = projectName || 'YOUR_PROJECT';
    const displayExtensionName = isEnabled ? instanceName : (instanceName || 'YOUR_EXTENSION_NAME');
    const redirectUri = `${backendUrl}/oidc/${displayProjectName}/${displayExtensionName}/callback`;

    // Use a ref to store the latest onChange callback
    const onChangeRef = useRef(onChange);
    useEffect(() => {
        onChangeRef.current = onChange;
    }, [onChange]);

    // Encrypt client secret when user enters it
    const handleEncryptSecret = async () => {
        if (!clientSecretPlaintext || clientSecretPlaintext.trim() === '') {
            return;
        }

        setIsEncrypting(true);
        try {
            const response = await api.encryptSecret(clientSecretPlaintext);
            setClientSecretEncrypted(response.encrypted);
            setClientSecretPlaintext(''); // Clear plaintext immediately after encryption
            setHasExistingSecret(true);
            showToast('Client secret encrypted successfully', 'success');
        } catch (err) {
            if (err.message.includes('429') || err.message.includes('rate limit')) {
                showToast('Rate limit exceeded. Please try again later (100 requests per hour).', 'error');
            } else {
                showToast(`Failed to encrypt secret: ${err.message}`, 'error');
            }
        } finally {
            setIsEncrypting(false);
        }
    };

    // Update parent when values change
    useEffect(() => {
        // Parse scopes from comma-separated string
        const scopesArray = scopes
            .split(',')
            .map(s => s.trim())
            .filter(s => s.length > 0);

        // Build the spec object
        const newSpec = {
            provider_name: providerName,
            client_id: clientId,
            issuer_url: issuerUrl,
            scopes: scopesArray,
        };

        // Only include description if it's not empty
        if (description && description.trim() !== '') {
            newSpec.description = description;
        }

        // Include encrypted client secret if set
        if (clientSecretEncrypted) {
            newSpec.client_secret_encrypted = clientSecretEncrypted;
        }

        // Include optional endpoint overrides if set
        if (authorizationEndpoint && authorizationEndpoint.trim() !== '') {
            newSpec.authorization_endpoint = authorizationEndpoint;
        }
        if (tokenEndpoint && tokenEndpoint.trim() !== '') {
            newSpec.token_endpoint = tokenEndpoint;
        }

        onChangeRef.current(newSpec);
    }, [providerName, description, clientId, clientSecretEncrypted, issuerUrl, authorizationEndpoint, tokenEndpoint, scopes]);

    const exampleConfigs = {
        google: {
            title: 'Google (OIDC discovery)',
            apply: {
                providerName: 'Google',
                issuerUrl: 'https://accounts.google.com',
                authorizationEndpoint: '',
                tokenEndpoint: '',
                scopes: 'openid, email, profile',
                needsEndpoints: false,
            },
            spec: `{
  "provider_name": "Google",
  "client_id": "your-client-id",
  "issuer_url": "https://accounts.google.com",
  "scopes": ["openid", "email", "profile"]
}`
        },
        github: {
            title: 'GitHub (manual endpoints)',
            apply: {
                providerName: 'GitHub',
                issuerUrl: 'https://github.com',
                authorizationEndpoint: 'https://github.com/login/oauth/authorize',
                tokenEndpoint: 'https://github.com/login/oauth/access_token',
                scopes: 'read:user, user:email',
                needsEndpoints: true,
            },
            spec: `{
  "provider_name": "GitHub",
  "client_id": "your-client-id",
  "issuer_url": "https://github.com",
  "authorization_endpoint": "https://github.com/login/oauth/authorize",
  "token_endpoint": "https://github.com/login/oauth/access_token",
  "scopes": ["read:user", "user:email"]
}`
        },
        snowflake: {
            title: 'Snowflake (manual endpoints)',
            apply: {
                providerName: 'Snowflake',
                issuerUrl: 'https://YOUR_ACCOUNT.snowflakecomputing.com',
                authorizationEndpoint: 'https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/authorize',
                tokenEndpoint: 'https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/token-request',
                scopes: 'refresh_token',
                needsEndpoints: true,
            },
            spec: `{
  "provider_name": "Snowflake",
  "client_id": "your-client-id",
  "issuer_url": "https://YOUR_ACCOUNT.snowflakecomputing.com",
  "authorization_endpoint": "https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/authorize",
  "token_endpoint": "https://YOUR_ACCOUNT.snowflakecomputing.com/oauth/token-request",
  "scopes": ["refresh_token"]
}`
        }
    };

    const applyExampleConfig = (key) => {
        const example = exampleConfigs[key];
        if (!example) return;

        setProviderName(example.apply.providerName);
        setIssuerUrl(example.apply.issuerUrl);
        setAuthorizationEndpoint(example.apply.authorizationEndpoint);
        setTokenEndpoint(example.apply.tokenEndpoint);
        setScopes(example.apply.scopes);
        setShowAdvanced(Boolean(example.apply.needsEndpoints));
        setSetupStep(2);
        showToast(`Applied ${example.apply.providerName} example`, 'success');
    };

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 20 }}>
            <Tabs
                tabs={[
                    { id: '1', label: '1. Upstream Provider Setup' },
                    { id: '2', label: '2. Configuration Inputs' },
                ]}
                active={String(setupStep)}
                onChange={(id) => setSetupStep(Number(id))}
            />

            {setupStep === 1 && (
                <div style={{ display: 'flex', flexDirection: 'column', gap: 20 }}>
                    <section>
                        <div className="r-section-title" style={{ marginBottom: 8 }}>Set up the upstream OAuth / OIDC provider</div>
                        <ol style={{ fontSize: 13, color: 'var(--text-muted)', paddingLeft: 18, display: 'flex', flexDirection: 'column', gap: 6, margin: 0 }}>
                            <li>Register an OAuth app with your provider and collect client credentials.</li>
                            <li>Configure the redirect URI below as an allowed callback in your provider.</li>
                            <li>Return here and continue to enter the configuration inputs.</li>
                        </ol>
                        <p style={{ fontSize: 12, marginTop: 10, color: 'var(--text-soft)' }}>
                            For local development, you can redirect to localhost via the{' '}
                            <code className="mono" style={{ color: 'var(--text)' }}>redirect_uri</code> query parameter even if the provider only allows the Rise callback URL.
                        </p>
                    </section>

                    <section>
                        <div className="r-section-title" style={{ marginBottom: 8 }}>Redirect URI</div>
                        <div style={{ display: 'flex', gap: 8, alignItems: 'stretch' }}>
                            <code
                                className="mono"
                                style={{
                                    flex: 1,
                                    padding: '8px 10px',
                                    fontSize: 12,
                                    wordBreak: 'break-all',
                                    background: 'var(--surface-2)',
                                    border: '1px solid var(--border)',
                                    borderRadius: 'var(--radius-sm)',
                                    color: 'var(--text)',
                                }}
                            >
                                {redirectUri}
                            </code>
                            <Button
                                size="sm"
                                icon="copy"
                                onClick={async () => {
                                    try {
                                        await copyToClipboard(redirectUri);
                                        showToast('Redirect URI copied to clipboard', 'success');
                                    } catch (err) {
                                        showToast(`Failed to copy: ${err.message}`, 'error');
                                    }
                                }}
                            >
                                Copy
                            </Button>
                        </div>
                    </section>

                    <section>
                        <div className="r-section-title" style={{ marginBottom: 8 }}>Example Configurations</div>
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                            {Object.entries(exampleConfigs).map(([key, example]) => (
                                <Panel key={example.title}>
                                    <PanelHead>
                                        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', width: '100%' }}>
                                            <div className="r-panel-title">{example.title}</div>
                                            <Button size="sm" onClick={() => applyExampleConfig(key)}>Apply</Button>
                                        </div>
                                    </PanelHead>
                                    <PanelBody>
                                        <CodeBlock>{example.spec}</CodeBlock>
                                    </PanelBody>
                                </Panel>
                            ))}
                        </div>
                    </section>

                    <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
                        <Button variant="primary" onClick={() => setSetupStep(2)}>Next</Button>
                    </div>
                </div>
            )}

            {setupStep === 2 && (
                <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                    <Field label="Provider Name" hint="Display name for this OAuth provider">
                        <Input
                            id="oauth-provider-name"
                            value={providerName}
                            onChange={(e) => setProviderName(e.target.value)}
                            placeholder="e.g., Snowflake Production"
                        />
                    </Field>

                    <Field label="Description (Optional)" hint="Human-readable description of this OAuth configuration">
                        <Input
                            id="oauth-description"
                            value={description}
                            onChange={(e) => setDescription(e.target.value)}
                            placeholder="e.g., OAuth authentication for analytics access"
                        />
                    </Field>

                    <Field label="Client ID" hint="OAuth client identifier from your provider">
                        <Input
                            id="oauth-client-id"
                            value={clientId}
                            onChange={(e) => setClientId(e.target.value)}
                            placeholder="e.g., ABC123XYZ..."
                        />
                    </Field>

                    <Field
                        label={
                            <>
                                Client Secret{' '}
                                {hasExistingSecret && !clientSecretPlaintext && (
                                    <span style={{ color: 'var(--text-soft)' }}>(configured)</span>
                                )}
                                {clientSecretPlaintext && (
                                    <span style={{ color: 'var(--accent)' }}>(will be updated)</span>
                                )}
                            </>
                        }
                        hint={
                            hasExistingSecret
                                ? 'Secret is configured. Leave blank to keep current secret, or enter a new value and click Encrypt to update it.'
                                : 'Enter the OAuth client secret from your provider and click Encrypt to securely store it.'
                        }
                    >
                        <div style={{ display: 'flex', gap: 8 }}>
                            <div style={{ flex: 1, position: 'relative' }}>
                                <Input
                                    type={showSecret ? 'text' : 'password'}
                                    id="oauth-client-secret"
                                    value={clientSecretPlaintext}
                                    onChange={(e) => setClientSecretPlaintext(e.target.value)}
                                    placeholder={clientSecretEncrypted ? '••••••••' : 'Enter client secret'}
                                    disabled={isEncrypting}
                                    style={{ paddingRight: 52 }}
                                />
                                <button
                                    type="button"
                                    onClick={() => setShowSecret(!showSecret)}
                                    style={{
                                        position: 'absolute',
                                        right: 8,
                                        top: '50%',
                                        transform: 'translateY(-50%)',
                                        fontSize: 11.5,
                                        background: 'transparent',
                                        border: 'none',
                                        color: 'var(--text-muted)',
                                        cursor: 'pointer',
                                    }}
                                >
                                    {showSecret ? 'Hide' : 'Show'}
                                </button>
                            </div>
                            <Button
                                onClick={handleEncryptSecret}
                                disabled={!clientSecretPlaintext || clientSecretPlaintext.trim() === '' || isEncrypting}
                                loading={isEncrypting}
                            >
                                {isEncrypting ? 'Encrypting…' : 'Encrypt'}
                            </Button>
                        </div>
                    </Field>

                    <Field
                        label="Issuer URL"
                        hint="OIDC issuer URL. For OIDC-compliant providers, endpoints are auto-discovered. For non-OIDC providers (GitHub), also set endpoints below."
                    >
                        <Input
                            id="oauth-issuer-url"
                            value={issuerUrl}
                            onChange={(e) => setIssuerUrl(e.target.value)}
                            placeholder="https://accounts.google.com"
                        />
                    </Field>

                    <Field label="Scopes" hint="Comma-separated list of OAuth scopes to request">
                        <Input
                            id="oauth-scopes"
                            value={scopes}
                            onChange={(e) => setScopes(e.target.value)}
                            placeholder="openid, email, profile"
                        />
                    </Field>

                    <div style={{ borderTop: '1px solid var(--border)', paddingTop: 14 }}>
                        <button
                            type="button"
                            onClick={() => setShowAdvanced(!showAdvanced)}
                            style={{
                                display: 'flex',
                                alignItems: 'center',
                                gap: 6,
                                fontSize: 13,
                                fontWeight: 500,
                                background: 'transparent',
                                border: 'none',
                                color: 'var(--text)',
                                cursor: 'pointer',
                                padding: 0,
                            }}
                        >
                            <Icon name="chev" size={12} style={{ transform: showAdvanced ? 'rotate(90deg)' : 'none' }} />
                            Advanced: Manual Endpoint Overrides
                        </button>
                        <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 4 }}>
                            Only needed for non-OIDC providers (GitHub) or if OIDC discovery fails.
                        </p>
                    </div>

                    {showAdvanced && (
                        <div
                            style={{
                                display: 'flex',
                                flexDirection: 'column',
                                gap: 16,
                                paddingLeft: 14,
                                borderLeft: '2px solid var(--border)',
                            }}
                        >
                            <Field
                                label="Authorization Endpoint (Optional)"
                                hint="Override authorization URL (leave empty to use OIDC discovery)"
                            >
                                <Input
                                    id="oauth-authorization-endpoint"
                                    value={authorizationEndpoint}
                                    onChange={(e) => setAuthorizationEndpoint(e.target.value)}
                                    placeholder="https://github.com/login/oauth/authorize"
                                />
                            </Field>

                            <Field
                                label="Token Endpoint (Optional)"
                                hint="Override token URL (leave empty to use OIDC discovery)"
                            >
                                <Input
                                    id="oauth-token-endpoint"
                                    value={tokenEndpoint}
                                    onChange={(e) => setTokenEndpoint(e.target.value)}
                                    placeholder="https://github.com/login/oauth/access_token"
                                />
                            </Field>
                        </div>
                    )}

                    <div>
                        <Button onClick={() => setSetupStep(1)} size="sm">Previous</Button>
                    </div>
                </div>
            )}
        </div>
    );
}

const OAuthExtensionAPI = {
    icon: '/assets/oauth2.jpg',

    renderStatusBadge(extension) {
        const status = extension.status || {};

        if (status.error) {
            return <Status status="Error" />;
        }

        if (status.configured_at) {
            if (status.auth_verified) {
                return <Status status="Configured" />;
            }
            return <Status status="Waiting for auth" />;
        }

        return <Status status="Not configured" />;
    },

    renderOverviewTab(extension, projectName) {
        return <OAuthDetailView extension={extension} projectName={projectName} />;
    },

    renderConfigureTab(spec, schema, onChange, projectName, instanceName, isEnabled) {
        return <OAuthExtensionUI spec={spec} schema={schema} onChange={onChange} projectName={projectName} instanceName={instanceName} isEnabled={isEnabled} />;
    }
};

// Integration Guide Modal Component
function IntegrationGuideModal({ isOpen, onClose, projectName, extensionName }) {
    const [activeTab, setActiveTab] = useState('fragment');

    const backendUrl = CONFIG.backendUrl.replace(/\/$/, '');
    const authorizeUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/authorize`;
    const callbackUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/callback`;
    const tokenUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/token`;

    const codeLabelStyle: React.CSSProperties = { fontSize: 13, fontWeight: 600, color: 'var(--text)', margin: '0 0 6px' };
    const paraStyle: React.CSSProperties = { fontSize: 13, color: 'var(--text-muted)', margin: 0, lineHeight: 1.6 };

    return (
        <Modal isOpen={isOpen} onClose={onClose} title="Integration Guide" width="wide">
                <Tabs
                    tabs={[
                        { id: 'fragment', label: 'PKCE Flow (SPAs)' },
                        { id: 'backend', label: 'Token Endpoint (Backend)' },
                        { id: 'local', label: 'Local Development' },
                    ]}
                    active={activeTab}
                    onChange={setActiveTab}
                />

                {/* Modal Content */}
                <div style={{ display: 'flex', flexDirection: 'column', gap: 16, marginTop: 16 }}>
                    {activeTab === 'fragment' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                <strong style={{ color: 'var(--text)' }}>Fragment Flow</strong> is the default and recommended approach for single-page applications (SPAs).
                                Tokens are returned in the URL fragment (<code className="mono" style={{ color: 'var(--text)' }}>#access_token=...</code>),
                                which never reaches the server.
                            </p>

                            <div>
                                <p style={codeLabelStyle}>Authorization URL:</p>
                                <CodeBlock style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
                                    {authorizeUrl}
                                </CodeBlock>
                            </div>

                            <div>
                                <p style={codeLabelStyle}>Example Code:</p>
                                <CodeBlock>
{`// Initiate OAuth login (fragment flow is default)
function login() {
  const authUrl = '${authorizeUrl}';
  window.location.href = authUrl;
}

// Extract tokens from URL fragment after redirect
function handleCallback() {
  const fragment = window.location.hash.substring(1);
  const params = new URLSearchParams(fragment);

  const accessToken = params.get('access_token');
  const idToken = params.get('id_token');
  const expiresAt = params.get('expires_at');

  if (accessToken) {
    // Store securely (session storage for security)
    sessionStorage.setItem('access_token', accessToken);
    if (idToken) {
      sessionStorage.setItem('id_token', idToken);
    }

    // Clear the fragment from URL
    window.location.hash = '';

    // Redirect to your app
    window.location.href = '/dashboard';
  }
}

// Call on page load
handleCallback();`}
                                </CodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'backend' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                <strong style={{ color: 'var(--text)' }}>Token Endpoint Flow</strong> is designed for server-rendered applications (confidential clients). Your backend receives an
                                authorization code as a query parameter, which it exchanges for OAuth tokens via the RFC 6749-compliant token endpoint using client credentials.
                            </p>

                            <div>
                                <p style={codeLabelStyle}>Authorization URL:</p>
                                <CodeBlock style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
                                    {authorizeUrl}
                                </CodeBlock>
                            </div>

                            <div>
                                <p style={codeLabelStyle}>Token Endpoint:</p>
                                <CodeBlock style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
                                    POST {tokenUrl}
                                </CodeBlock>
                            </div>

                            <div>
                                <p style={codeLabelStyle}>Example Code (Node.js/Express):</p>
                                <CodeBlock>
{`// Initiate OAuth login
app.get('/login', (req, res) => {
  const authUrl = '${authorizeUrl}';
  res.redirect(authUrl);
});

// Handle OAuth callback
app.get('/oauth/callback', async (req, res) => {
  const code = req.query.code;

  if (!code) {
    return res.status(400).send('Missing authorization code');
  }

  try {
    // Exchange authorization code for tokens
    const response = await fetch('${tokenUrl}', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({
        grant_type: 'authorization_code',
        code: code,
        client_id: process.env.${extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_ID,
        client_secret: process.env.${extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_SECRET
      })
    });

    if (!response.ok) {
      const error = await response.json();
      throw new Error(\`Token exchange failed: \${error.error}\`);
    }

    const tokens = await response.json();
    // { access_token, token_type, expires_in, refresh_token, ... }

    // Store in HttpOnly session cookie (recommended)
    req.session.tokens = tokens;

    // Redirect to app
    res.redirect('/dashboard');
  } catch (error) {
    console.error('OAuth error:', error);
    res.status(500).send('Authentication failed');
  }
});`}
                                </CodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'local' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                For local development, you can override the redirect URI to point to your local development server.
                                Rise always handles the OAuth provider callback, so you don't need to register localhost URLs with your OAuth provider.
                            </p>

                            <div>
                                <p style={codeLabelStyle}>PKCE Flow (localhost):</p>
                                <CodeBlock>
{`// Override redirect URI for local development
const authUrl = '${authorizeUrl}?redirect_uri=' +
  encodeURIComponent('http://localhost:3000/callback');

window.location.href = authUrl;

// Handle the callback in your local app
function handleCallback() {
  const fragment = window.location.hash.substring(1);
  const params = new URLSearchParams(fragment);
  const accessToken = params.get('access_token');
  // ... use the token
}`}
                                </CodeBlock>
                            </div>

                            <div>
                                <p style={codeLabelStyle}>Token Endpoint Flow (localhost):</p>
                                <CodeBlock>
{`// Override redirect URI for local development
app.get('/login', (req, res) => {
  const authUrl = '${authorizeUrl}?redirect_uri=' +
    encodeURIComponent('http://localhost:3000/oauth/callback');
  res.redirect(authUrl);
});

// Your local callback handler
app.get('/oauth/callback', async (req, res) => {
  const code = req.query.code;
  // ... same token exchange logic as production
});`}
                                </CodeBlock>
                            </div>

                            <Alert tone="info" icon="info">
                                <strong>How It Works.</strong>{' '}
                                The OAuth provider only redirects to <code className="mono" style={{ color: 'var(--text)' }}>{callbackUrl}</code> (Rise's callback URL).
                                Rise then redirects to your app's redirect_uri with the authorization code. You don't need to configure localhost URLs in your OAuth provider.
                            </Alert>
                        </div>
                    )}
                </div>
        </Modal>
    );
}

// OAuth Detail View Component
export function OAuthDetailView({ extension, projectName }) {
    const status = extension.status || {};
    const spec = extension.spec || {};
    const scopesArray = spec.scopes || [];
    const extensionName = extension.extension;
    const { showToast } = useToast();
    const [showGuideModal, setShowGuideModal] = useState(false);

    // Build URLs using actual backend URL
    const backendUrl = CONFIG.backendUrl.replace(/\/$/, ''); // Remove trailing slash

    const handleTestOAuth = () => {
        // Include the current hash in the redirect URI so we return to the same page
        const redirectUri = window.location.href;
        const authUrl = `/oidc/${projectName}/${extensionName}/authorize?redirect_uri=${encodeURIComponent(redirectUri)}`;
        window.location.href = authUrl;
    };

    return (
        <>
            <IntegrationGuideModal
                isOpen={showGuideModal}
                onClose={() => setShowGuideModal(false)}
                projectName={projectName}
                extensionName={extensionName}
            />

            {/* Two-column layout */}
            <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 2fr) minmax(0, 1fr)', gap: 20, alignItems: 'start' }}>
                {/* Left column - Main content */}
                <div style={{ display: 'flex', flexDirection: 'column', gap: 20, minWidth: 0 }}>
                    {/* Upstream OAuth Provider Configuration */}
                    <Panel>
                        <PanelHead title="Upstream OAuth Provider" />
                        <PanelBody>
                            <KV>
                                <KVRow k="Provider Name">{spec.provider_name || 'N/A'}</KVRow>
                                {spec.description && <KVRow k="Description">{spec.description}</KVRow>}
                                <KVRow k="Client ID"><span className="mono">{spec.client_id || 'N/A'}</span></KVRow>
                                <KVRow k="Issuer URL">
                                    <span className="mono" style={{ wordBreak: 'break-all' }}>{spec.issuer_url || 'N/A'}</span>
                                </KVRow>
                                {spec.authorization_endpoint && (
                                    <KVRow k={<>Authorization Endpoint <span style={{ color: 'var(--text-soft)' }}>(override)</span></>}>
                                        <span className="mono" style={{ wordBreak: 'break-all' }}>{spec.authorization_endpoint}</span>
                                    </KVRow>
                                )}
                                {spec.token_endpoint && (
                                    <KVRow k={<>Token Endpoint <span style={{ color: 'var(--text-soft)' }}>(override)</span></>}>
                                        <span className="mono" style={{ wordBreak: 'break-all' }}>{spec.token_endpoint}</span>
                                    </KVRow>
                                )}
                                <KVRow k={`Scopes (${scopesArray.length})`}>
                                    {scopesArray.length === 0 ? (
                                        <span style={{ color: 'var(--text-soft)', fontStyle: 'italic' }}>No scopes configured</span>
                                    ) : (
                                        <span style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                                            {scopesArray.map((scope, idx) => (
                                                <Pill key={idx} kind="accent">{scope}</Pill>
                                            ))}
                                        </span>
                                    )}
                                </KVRow>
                            </KV>
                            {!spec.authorization_endpoint && !spec.token_endpoint && (
                                <p style={{ fontSize: 12, color: 'var(--text-soft)', fontStyle: 'italic', marginTop: 10 }}>
                                    Endpoints auto-discovered via OIDC discovery
                                </p>
                            )}
                        </PanelBody>
                    </Panel>
                </div>

                {/* Right column - Actions */}
                <div style={{ display: 'flex', flexDirection: 'column', gap: 20, minWidth: 0 }}>
                    {/* Configuration Status */}
                    <Panel>
                        <PanelHead title="Status" />
                        <PanelBody>
                            {status.error ? (
                                <Alert tone="err" icon="info">
                                    <strong>Error:</strong> {status.error}
                                </Alert>
                            ) : status.configured_at ? (
                                status.auth_verified ? (
                                    <Alert tone="info" icon="check">
                                        <strong>Configured</strong>
                                        <div style={{ fontSize: 12, marginTop: 2 }}>{formatDate(status.configured_at)}</div>
                                    </Alert>
                                ) : (
                                    <Alert tone="warn" icon="info">
                                        <strong>Waiting For Auth</strong>
                                        <div style={{ fontSize: 12, marginTop: 2 }}>Complete OAuth flow to verify configuration</div>
                                    </Alert>
                                )
                            ) : (
                                <Alert tone="info" icon="info">Configuration pending…</Alert>
                            )}
                        </PanelBody>
                    </Panel>

                    {/* Test OAuth Flow */}
                    <Panel>
                        <PanelHead title="Test" />
                        <PanelBody>
                            <Button onClick={handleTestOAuth} variant="primary" style={{ width: '100%' }}>
                                Test OAuth Flow
                            </Button>
                            <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 8 }}>
                                Test the OAuth flow and return to this page with a notification.
                            </p>
                        </PanelBody>
                    </Panel>

                    {/* Integration Guide Button */}
                    <Panel>
                        <PanelHead title="Integration" />
                        <PanelBody>
                            <Button onClick={() => setShowGuideModal(true)} style={{ width: '100%' }}>
                                Integration Guide
                            </Button>
                            <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 8 }}>
                                View code examples for PKCE Flow, Token Endpoint Flow, and local development.
                            </p>
                        </PanelBody>
                    </Panel>
                </div>
            </div>

            {/* Injected Environment Variables - Full width below the grid */}
            <Panel style={{ marginTop: 20 }}>
                <PanelHead title="Injected Environment Variables" sub="These environment variables are injected into your deployed application." />
                <PanelBody>
                    <table className="r-table" style={{ width: '100%' }}>
                        <thead>
                            <tr>
                                <th style={{ textAlign: 'left' }}>Variable</th>
                                <th style={{ textAlign: 'left' }}>Value</th>
                                <th style={{ width: 40 }}></th>
                            </tr>
                        </thead>
                        <tbody>
                            <tr>
                                <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_ID</span></td>
                                <td><span className="mono" style={{ wordBreak: 'break-all' }}>{status?.rise_client_id || `${projectName}-${extensionName}`}</span></td>
                                <td>
                                    <CopyButton
                                        title="Copy Client ID"
                                        onClick={async () => {
                                            try {
                                                await copyToClipboard(status?.rise_client_id || `${projectName}-${extensionName}`);
                                                showToast('Client ID copied', 'success');
                                            } catch (err) {
                                                showToast(`Failed to copy: ${err.message}`, 'error');
                                            }
                                        }}
                                    />
                                </td>
                            </tr>
                            <tr>
                                <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_SECRET</span></td>
                                <td><span className="mono" style={{ color: 'var(--text-soft)' }}>••••••••</span></td>
                                <td>
                                    {status?.rise_client_secret && (
                                        <CopyButton
                                            title="Copy Client Secret"
                                            onClick={async () => {
                                                try {
                                                    await copyToClipboard(status.rise_client_secret);
                                                    showToast('Client secret copied', 'success');
                                                } catch (err) {
                                                    showToast(`Failed to copy: ${err.message}`, 'error');
                                                }
                                            }}
                                        />
                                    )}
                                </td>
                            </tr>
                            <tr>
                                <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{extensionName.toUpperCase().replace(/-/g, '_')}_ISSUER</span></td>
                                <td>
                                    <a
                                        className="r-link mono"
                                        style={{ wordBreak: 'break-all' }}
                                        href={`${backendUrl}/oidc/${projectName}/${extensionName}/.well-known/openid-configuration`}
                                        target="_blank"
                                        rel="noopener noreferrer"
                                    >
                                        {`${backendUrl}/oidc/${projectName}/${extensionName}`}
                                    </a>
                                </td>
                                <td>
                                    <CopyButton
                                        title="Copy Issuer URL"
                                        onClick={async () => {
                                            try {
                                                await copyToClipboard(`${backendUrl}/oidc/${projectName}/${extensionName}`);
                                                showToast('Issuer URL copied', 'success');
                                            } catch (err) {
                                                showToast(`Failed to copy: ${err.message}`, 'error');
                                            }
                                        }}
                                    />
                                </td>
                            </tr>
                        </tbody>
                    </table>
                    <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 12 }}>
                        Click the issuer URL to view the OIDC discovery document.
                    </p>
                </PanelBody>
            </Panel>
        </>
    );
}

// Snowflake OAuth Provisioner Extension UI Component
export function SnowflakeOAuthExtensionUI({ spec, schema, onChange }) {
    const [blockedRoles, setBlockedRoles] = useState(spec?.blocked_roles?.join(', ') || '');
    const [scopes, setScopes] = useState(spec?.scopes?.join(', ') || '');

    // Use a ref to store the latest onChange callback
    const onChangeRef = useRef(onChange);
    useEffect(() => {
        onChangeRef.current = onChange;
    }, [onChange]);

    // Update parent when values change
    useEffect(() => {
        const newSpec = {
            blocked_roles: blockedRoles.split(',').map(r => r.trim()).filter(r => r),
            scopes: scopes.split(',').map(s => s.trim()).filter(s => s),
        };

        onChangeRef.current(newSpec);
    }, [blockedRoles, scopes]);

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
            <Field
                label="Additional Blocked Roles"
                hint="Comma-separated list added to backend blocked-role defaults."
            >
                <Textarea
                    id="snowflake-blocked-roles"
                    rows={2}
                    value={blockedRoles}
                    onChange={(e) => setBlockedRoles(e.target.value)}
                    placeholder="SYSADMIN, USERADMIN"
                />
            </Field>

            <Field
                label="Additional OAuth Scopes"
                hint="Comma-separated scopes added to backend defaults."
            >
                <Textarea
                    id="snowflake-scopes"
                    rows={2}
                    value={scopes}
                    onChange={(e) => setScopes(e.target.value)}
                    placeholder="session:role:ANALYST, session:role:DEVELOPER"
                />
            </Field>

            <Alert tone="info" icon="info">
                <strong>Secondary Roles.</strong> Secondary roles are enabled by default (OAUTH_USE_SECONDARY_ROLES = IMPLICIT).
            </Alert>
        </div>
    );
}

// Snowflake OAuth Provisioner Detail View Component
export function SnowflakeOAuthDetailView({ extension, projectName }) {
    const status = extension.status || {};
    const spec = extension.spec || {};

    // Get state badge color
    const getStateBadge = () => {
        return renderStatePill(status.state);
    };

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 20 }}>
            <section style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                    <div className="r-section-title">Snowflake OAuth Provisioner</div>
                    {getStateBadge()}
                </div>
                <p style={{ fontSize: 13, color: 'var(--text-muted)', margin: 0 }}>
                    Automatically provisions Snowflake SECURITY INTEGRATIONs and a linked OAuth extension.
                </p>
                {status.state === 'Available' && status.oauth_extension_name ? (
                    <Alert tone="info" icon="check">
                        <strong>Next Action.</strong> Provisioning completed. Continue by reviewing or testing the linked OAuth extension.
                        <div style={{ marginTop: 8 }}>
                            <a href={`/project/${projectName}/extensions/oauth/${status.oauth_extension_name}`} className="r-btn primary small">
                                Open Linked OAuth Extension
                            </a>
                        </div>
                    </Alert>
                ) : (
                    <Alert tone="warn" icon="info">
                        <strong>Current State.</strong> This extension is still progressing through provisioning states. Use the status sections below to track readiness and errors.
                    </Alert>
                )}
            </section>

            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 20 }}>
                {/* Snowflake Integration Details */}
                <Panel>
                    <PanelHead title="Snowflake Integration Status" />
                    <PanelBody>
                        <KV>
                            {status.integration_name && <KVRow k="Integration Name"><span className="mono">{status.integration_name}</span></KVRow>}
                            {status.oauth_client_id && <KVRow k="OAuth Client ID"><span className="mono">{status.oauth_client_id}</span></KVRow>}
                            {status.redirect_uri && <KVRow k="Redirect URI"><span className="mono" style={{ wordBreak: 'break-all' }}>{status.redirect_uri}</span></KVRow>}
                            {status.created_at && <KVRow k="Created">{formatDate(status.created_at)}</KVRow>}
                        </KV>
                        {status.state === 'Available' && (
                            <div style={{ marginTop: 12 }}>
                                <Alert tone="info" icon="check">Snowflake integration is active and configured.</Alert>
                            </div>
                        )}
                        {status.error && (
                            <div style={{ marginTop: 12 }}>
                                <Alert tone="err" icon="info"><strong>Error:</strong> {status.error}</Alert>
                            </div>
                        )}
                    </PanelBody>
                </Panel>

                {/* OAuth Extension Details */}
                <Panel>
                    <PanelHead title="Linked OAuth Extension" />
                    <PanelBody>
                        {status.oauth_extension_name ? (
                            <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                                <KV>
                                    <KVRow k="Extension Name"><span className="mono">{status.oauth_extension_name}</span></KVRow>
                                </KV>
                                {status.state === 'Available' && (
                                    <div>
                                        <a href={`/project/${projectName}/extensions/oauth/${status.oauth_extension_name}`} className="r-btn primary small">
                                            View OAuth Extension
                                        </a>
                                    </div>
                                )}
                                <Alert tone="info" icon="info">
                                    The OAuth extension is automatically created and managed by this provisioner.
                                    Users can authenticate using their Snowflake credentials.
                                </Alert>
                            </div>
                        ) : (
                            <p style={{ fontSize: 13, color: 'var(--text-muted)', margin: 0 }}>
                                OAuth extension will be created during provisioning
                            </p>
                        )}
                    </PanelBody>
                </Panel>

                {/* Configuration Summary */}
                <Panel style={{ gridColumn: '1 / -1' }}>
                    <PanelHead title="Configuration" />
                    <PanelBody>
                        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 16 }}>
                            <div>
                                <div className="r-field-label">Blocked Roles</div>
                                {spec.blocked_roles && spec.blocked_roles.length > 0 ? (
                                    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                                        {spec.blocked_roles.map((role, idx) => (
                                            <Pill key={idx}>{role}</Pill>
                                        ))}
                                    </div>
                                ) : (
                                    <p style={{ fontSize: 12, color: 'var(--text-soft)', margin: 0 }}>Using backend defaults only</p>
                                )}
                            </div>
                            <div>
                                <div className="r-field-label">OAuth Scopes</div>
                                {spec.scopes && spec.scopes.length > 0 ? (
                                    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                                        {spec.scopes.map((scope, idx) => (
                                            <Pill key={idx} kind="accent">{scope}</Pill>
                                        ))}
                                    </div>
                                ) : (
                                    <p style={{ fontSize: 12, color: 'var(--text-soft)', margin: 0 }}>Using backend defaults only</p>
                                )}
                            </div>
                        </div>
                        <div style={{ marginTop: 16 }}>
                            <Alert tone="warn" icon="info">
                                <strong>Note:</strong> Additional roles and scopes are combined with backend defaults
                                (not replaced). ACCOUNTADMIN, ORGADMIN, and SECURITYADMIN are always blocked.
                            </Alert>
                        </div>
                    </PanelBody>
                </Panel>
            </div>
        </div>
    );
}

const SnowflakeOAuthExtensionAPI = {
    icon: '/assets/snowflake.jpg',

    renderStatusBadge(extension) {
        const status = extension.status || {};
        if (!status.state) return null;
        return renderStatePill(status.state);
    },

    renderOverviewTab(extension, projectName) {
        return <SnowflakeOAuthDetailView extension={extension} projectName={projectName} />;
    },

    renderConfigureTab(spec, schema, onChange, projectName, instanceName, isEnabled) {
        return <SnowflakeOAuthExtensionUI spec={spec} schema={schema} onChange={onChange} />;
    },
};

// AWS S3 Bucket Extension

export function AwsS3ExtensionUI({ spec, schema, onChange }) {
    // No user-configurable fields in v0 — notify parent of empty spec on mount
    const onChangeRef = useRef(onChange);
    useEffect(() => { onChangeRef.current = onChange; }, [onChange]);
    useEffect(() => { onChangeRef.current({}); }, []);

    const envVarStyle: React.CSSProperties = { color: 'var(--accent)' };
    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
            <Alert tone="info" icon="info">
                <strong>Fixed Environment Variables.</strong> This extension automatically injects the following environment variables into every deployment. No configuration is required.
                <div className="mono" style={{ marginTop: 10, fontSize: 12, display: 'flex', flexDirection: 'column', gap: 3 }}>
                    <div><span style={envVarStyle}>S3_BUCKET_NAME</span> — the name of the provisioned S3 bucket</div>
                    <div><span style={envVarStyle}>AWS_ACCESS_KEY_ID</span> — IAM access key ID</div>
                    <div><span style={envVarStyle}>AWS_SECRET_ACCESS_KEY</span> — IAM secret access key</div>
                    <div><span style={envVarStyle}>AWS_REGION</span> — AWS region of the bucket</div>
                </div>
            </Alert>
            <Alert tone="info" icon="info">
                <strong>Bucket Deletion.</strong> When you delete this extension, the IAM user and access key are removed immediately. The S3 bucket is only deleted if it is empty — non-empty buckets block deletion until you choose to empty them.
            </Alert>
        </div>
    );
}

export function AwsS3DetailView({ extension, projectName }) {
    const status = extension.status || {};
    const stateStr = String(status.state || '').toLowerCase();
    const isAvailable = stateStr === 'available';
    const isFailed = stateStr === 'failed';
    const isDeleting = stateStr === 'deleting';
    const isDeletionBlocked = stateStr === 'deletion_blocked';
    const [forceEmptying, setForceEmptying] = useState(false);
    const { showToast } = useToast();

    const handleForceEmpty = async () => {
        setForceEmptying(true);
        try {
            await api.patchExtension(projectName, extension.extension, { force_empty_bucket: true });
            showToast('Bucket emptying initiated — the controller will empty and delete the bucket.', 'success');
        } catch (err) {
            showToast(`Failed to enable force-empty: ${err.message}`, 'error');
        } finally {
            setForceEmptying(false);
        }
    };

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 20 }}>
            <section style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                    <div className="r-section-title">AWS S3 Bucket</div>
                    {renderStatePill(status.state)}
                </div>
                {isAvailable ? (
                    <Alert tone="info" icon="check">
                        <strong>Current State.</strong> S3 bucket is provisioned and credentials are ready. Deploy your project to inject the environment variables.
                    </Alert>
                ) : isFailed ? (
                    <Alert tone="err" icon="info">
                        <strong>Provisioning Failed.</strong> {status.error || 'An error occurred during provisioning. The reconciler will retry automatically.'}
                    </Alert>
                ) : isDeletionBlocked ? (
                    <Alert tone="warn" icon="info">
                        <strong>Deletion Blocked.</strong> {status.error || 'The S3 bucket could not be deleted because it is not empty.'}
                        <div style={{ marginTop: 10 }}>
                            <Button variant="danger" size="sm" onClick={handleForceEmpty} disabled={forceEmptying} loading={forceEmptying}>
                                {forceEmptying ? 'Enabling…' : 'Empty bucket and delete'}
                            </Button>
                        </div>
                    </Alert>
                ) : isDeleting ? (
                    <Alert tone="info" icon="info">
                        <strong>Deleting.</strong> {status.error || 'The extension is being cleaned up. IAM user and S3 bucket will be removed.'}
                    </Alert>
                ) : (
                    <Alert tone="warn" icon="info">
                        <strong>Current State.</strong> Provisioning is in progress. Deployments will fail until the bucket is available.
                    </Alert>
                )}
            </section>

            <Panel>
                <PanelHead title="Bucket Details" />
                <PanelBody>
                    <KV>
                        <KVRow k="State">{renderStatePill(status.state)}</KVRow>
                        <KVRow k="Bucket Name"><span className="mono">{status.bucket_name || '—'}</span></KVRow>
                        <KVRow k="IAM User"><span className="mono">{status.iam_user_name || '—'}</span></KVRow>
                        <KVRow k="Access Key ID"><span className="mono">{status.iam_access_key_id || '—'}</span></KVRow>
                        <KVRow k="Region"><span className="mono">{status.region || '—'}</span></KVRow>
                    </KV>
                </PanelBody>
            </Panel>

            <Panel>
                <PanelHead title="Injected Environment Variables" />
                <PanelBody>
                    <table className="r-table" style={{ width: '100%' }}>
                        <thead>
                            <tr>
                                <th style={{ textAlign: 'left' }}>Variable</th>
                                <th style={{ textAlign: 'left' }}>Value</th>
                            </tr>
                        </thead>
                        <tbody>
                            <tr>
                                <td><span className="mono">S3_BUCKET_NAME</span></td>
                                <td><span className="mono" style={{ color: 'var(--text-muted)' }}>{status.bucket_name || '(pending)'}</span></td>
                            </tr>
                            <tr>
                                <td><span className="mono">AWS_ACCESS_KEY_ID</span></td>
                                <td><span className="mono" style={{ color: 'var(--text-muted)' }}>{status.iam_access_key_id || '(pending)'}</span></td>
                            </tr>
                            <tr>
                                <td><span className="mono">AWS_SECRET_ACCESS_KEY</span></td>
                                <td><Pill>protected</Pill></td>
                            </tr>
                            <tr>
                                <td><span className="mono">AWS_REGION</span></td>
                                <td><span className="mono" style={{ color: 'var(--text-muted)' }}>{status.region || '(pending)'}</span></td>
                            </tr>
                        </tbody>
                    </table>
                </PanelBody>
            </Panel>
        </div>
    );
}

const AwsS3ExtensionAPI = {
    icon: null,

    renderStatusBadge(extension) {
        const status = extension.status || {};
        if (!status.state) return null;
        return renderStatePill(status.state);
    },

    renderOverviewTab(extension, projectName) {
        return <AwsS3DetailView extension={extension} projectName={projectName} />;
    },

    renderConfigureTab(spec, schema, onChange, projectName, instanceName, isEnabled) {
        return <AwsS3ExtensionUI spec={spec} schema={schema} onChange={onChange} />;
    },
};

// Extension UI Registry
// Maps extension type identifiers to their UI API implementations
const ExtensionUIRegistry = {
    'aws-rds-provisioner': AwsRdsExtensionAPI,
    'aws-s3-bucket': AwsS3ExtensionAPI,
    'oauth': OAuthExtensionAPI,
    'snowflake-oauth-provisioner': SnowflakeOAuthExtensionAPI,
    // Add more extension UIs here as needed
};

// AWS RDS Custom Detail View Component
export function AwsRdsDetailView({ extension, projectName }) {
    const status = extension.status || {};
    const spec = extension.spec || {};
    const databases = status.databases || {};

    // Determine instance state badge color
    const getInstanceStateBadge = () => {
        return renderStatePill(status.state);
    };

    return (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 20 }}>
            <section style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                    <div className="r-section-title">AWS RDS Provisioner</div>
                    {getInstanceStateBadge()}
                </div>
                {String(status.state || '').toLowerCase() === 'available' ? (
                    <Alert tone="info" icon="check">
                        <strong>Current State.</strong> Database infrastructure is available. Review endpoint and environment variable settings below before deploying.
                    </Alert>
                ) : (
                    <Alert tone="warn" icon="info">
                        <strong>Current State.</strong> Provisioning is in progress or requires attention. New dependent deployments may be blocked until the instance is available.
                    </Alert>
                )}
            </section>

            {/* Instance Information */}
            <Panel>
                <PanelHead title="RDS Instance Status" />
                <PanelBody>
                    <KV>
                        <KVRow k="State">{getInstanceStateBadge()}</KVRow>
                        <KVRow k="Instance ID">{status.instance_id || 'N/A'}</KVRow>
                        <KVRow k="Instance Size">{status.instance_size || 'N/A'}</KVRow>
                        <KVRow k="Engine">{spec.engine || 'postgres'} {spec.engine_version || ''}</KVRow>
                        <KVRow k="Endpoint"><span className="mono">{status.endpoint || 'Pending…'}</span></KVRow>
                        <KVRow k="Database Isolation"><span style={{ textTransform: 'capitalize' }}>{spec.database_isolation || 'shared'}</span></KVRow>
                        <KVRow k="Master Username">{status.master_username || 'N/A'}</KVRow>
                    </KV>
                    {status.error && (
                        <div style={{ marginTop: 12 }}>
                            <Alert tone="err" icon="info"><strong>Error:</strong> {status.error}</Alert>
                        </div>
                    )}
                </PanelBody>
            </Panel>

            {/* Databases */}
            <section>
                <div className="r-section-title" style={{ marginBottom: 12 }}>
                    Databases ({Object.keys(databases).length})
                </div>
                {Object.keys(databases).length === 0 ? (
                    <Empty title="No databases provisioned yet" />
                ) : (
                    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 16 }}>
                        {Object.entries(databases).map(([dbName, dbStatus]) => (
                            <DatabaseCard
                                key={dbName}
                                name={dbName}
                                status={dbStatus}
                            />
                        ))}
                    </div>
                )}
            </section>

            {/* Configuration */}
            <Panel>
                <PanelHead title="Environment Variables" />
                <PanelBody>
                    <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, color: 'var(--text-muted)' }}>
                            <span>Database URL Variable:</span>
                            <code
                                className="mono"
                                style={{
                                    padding: '2px 8px',
                                    background: 'var(--surface-2)',
                                    border: '1px solid var(--border)',
                                    borderRadius: 'var(--radius-sm)',
                                    color: 'var(--text)',
                                }}
                            >
                                {spec.database_url_env_var || 'DATABASE_URL'}
                            </code>
                        </div>
                        <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, color: 'var(--text)' }}>
                            <input type="checkbox" checked={spec.inject_pg_vars !== false} disabled />
                            <span>
                                Inject <code className="mono" style={{ color: 'var(--accent)' }}>PG*</code> variables
                            </span>
                        </label>
                    </div>
                </PanelBody>
            </Panel>
        </div>
    );
}

// Database Card Component
function DatabaseCard({ name, status }) {
    // Determine status badge label
    let statusText = status.status || 'Unknown';
    const state = (status.status || '').toLowerCase();

    switch (state) {
        case 'pending':
        case 'creatingdatabase':
        case 'creatinguser':
            statusText = 'Provisioning';
            break;
        default:
            break;
    }

    const isScheduledForCleanup = status.cleanup_scheduled_at != null;
    const cleanupDate = isScheduledForCleanup
        ? new Date(status.cleanup_scheduled_at)
        : null;
    const cleanupTime = cleanupDate
        ? new Date(cleanupDate.getTime() + 60 * 60 * 1000) // +1 hour
        : null;

    return (
        <Panel>
            <PanelHead>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', width: '100%' }}>
                    <div className="r-panel-title">{name}</div>
                    <Status status={statusText} />
                </div>
            </PanelHead>
            <PanelBody>
                <KV>
                    <KVRow k="User"><span className="mono">{status.user}</span></KVRow>
                </KV>

                {isScheduledForCleanup && cleanupTime && (
                    <div style={{ marginTop: 12 }}>
                        <Alert tone="warn" icon="info">
                            <strong>Cleanup Scheduled.</strong> Will be deleted at {formatDate(cleanupTime.toISOString())}
                        </Alert>
                    </div>
                )}

                {status.status === 'Available' && !isScheduledForCleanup && (
                    <div style={{ marginTop: 12 }}>
                        <Alert tone="info" icon="check">Database is active and ready.</Alert>
                    </div>
                )}
            </PanelBody>
        </Panel>
    );
}

// Helper functions to access extension UI API

// Check if an extension has a custom UI API registered
export function hasExtensionUI(extensionType) {
    return extensionType in ExtensionUIRegistry;
}

// Get the extension UI API object
export function getExtensionUIAPI(extensionType) {
    return ExtensionUIRegistry[extensionType] || null;
}

// Get the configure tab component (for backward compatibility)
export function getExtensionUI(extensionType) {
    const api = getExtensionUIAPI(extensionType);
    return api?.renderConfigureTab ?
        (props) => api.renderConfigureTab(
            props.spec,
            props.schema,
            props.onChange,
            props.projectName,
            props.instanceName,
            props.isEnabled
        ) :
        null;
}

// Check if extension has custom overview tab
export function hasExtensionDetailView(extensionType) {
    const api = getExtensionUIAPI(extensionType);
    return api?.renderOverviewTab != null;
}

// Get the overview tab component (for backward compatibility)
export function getExtensionDetailView(extensionType) {
    const api = getExtensionUIAPI(extensionType);
    return api?.renderOverviewTab ?
        (props) => api.renderOverviewTab(props.extension, props.projectName) :
        null;
}

// Get custom status badge renderer
export function getExtensionStatusBadge(extensionType) {
    const api = getExtensionUIAPI(extensionType);
    return api?.renderStatusBadge || null;
}

// Get the icon URL for an extension
export function getExtensionIcon(extensionType) {
    const api = getExtensionUIAPI(extensionType);
    return api?.icon || null;
}
