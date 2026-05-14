// @ts-nocheck
import { useEffect, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { copyToClipboard, formatDate } from '../lib/utils';
import { useToast } from '../components/toast';
import { Button, FormField, Modal, ModalTabs, MonoCodeBlock, MonoNotice, MonoStatusPill, MonoTabButton, MonoTag } from '../components/ui';
import { MonoTable, MonoTableBody, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';

function statusToneFromState(state) {
    if (!state) return 'muted';
    const normalized = String(state).toLowerCase();

    if (['available', 'configured', 'running'].includes(normalized)) return 'ok';
    if (['creating', 'pending', 'testingconnection', 'creatingintegration', 'retrievingcredentials', 'creatingoauthextension', 'waiting for auth', 'deploying', 'building', 'pushing'].includes(normalized)) return 'warn';
    if (['failed', 'error', 'terminating'].includes(normalized)) return 'bad';
    if (['deletion_blocked'].includes(normalized)) return 'warn';
    return 'muted';
}

function renderStatePill(label, forceTone) {
    if (!label) return null;
    const tone = forceTone || statusToneFromState(label);
    return <MonoStatusPill tone={tone}>{label}</MonoStatusPill>;
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
        <div className="space-y-6">
            <div className="space-y-4">
                <FormField
                    label="Database Engine"
                    id="rds-engine"
                    type="select"
                    value={engine}
                    onChange={(e) => setEngine(e.target.value)}
                    required
                >
                    <option value="postgres">PostgreSQL</option>
                </FormField>

                <FormField
                    label="Engine Version (Optional)"
                    id="rds-engine-version"
                    value={engineVersion}
                    onChange={(e) => setEngineVersion(e.target.value)}
                    placeholder={defaultEngineVersion || "e.g., 16.2"}
                />

                <FormField
                    label="Database Isolation"
                    id="rds-database-isolation"
                    type="select"
                    value={databaseIsolation}
                    onChange={(e) => setDatabaseIsolation(e.target.value)}
                    required
                >
                    <option value="shared">Shared (All deployment groups use same database)</option>
                    <option value="isolated">Isolated (Each deployment group gets own database)</option>
                </FormField>

                <div className="space-y-3">
                    <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300">Environment Variables</h4>
                    <FormField
                        label="Database URL Environment Variable"
                        id="rds-database-url-env-var"
                        value={databaseUrlEnvVar}
                        onChange={(e) => setDatabaseUrlEnvVar(e.target.value)}
                        placeholder="DATABASE_URL"
                        helperText="Environment variable name for the database connection string (e.g., DATABASE_URL, POSTGRES_URL). Leave empty to disable."
                    />
                    <label className="flex items-center space-x-3">
                        <input
                            type="checkbox"
                            checked={injectPgVars}
                            onChange={(e) => setInjectPgVars(e.target.checked)}
                            className="mono-checkbox"
                        />
                        <span className="text-sm text-gray-700 dark:text-gray-300">
                            Inject <code className="mono-token-accent">PG*</code> variables
                        </span>
                    </label>
                </div>

                <MonoNotice tone="muted" title="Isolation Modes">
                    <p><strong>Shared:</strong> all deployment groups (default, staging, etc.) use the same database.</p>
                    <p><strong>Isolated:</strong> each deployment group gets its own database.</p>
                </MonoNotice>
            </div>
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
        <div className="space-y-6">
            <ModalTabs className="px-2">
                <MonoTabButton active={setupStep === 1} onClick={() => setSetupStep(1)}>
                    1. Upstream Provider Setup
                </MonoTabButton>
                <MonoTabButton active={setupStep === 2} onClick={() => setSetupStep(2)}>
                    2. Configuration Inputs
                </MonoTabButton>
            </ModalTabs>

            {setupStep === 1 && (
                <div className="space-y-6">
                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Setup the Upstream OAuth / OIDC Provider</h2>
                        <ol className="text-sm list-decimal list-inside space-y-2">
                            <li>Register an OAuth app with your provider and collect client credentials.</li>
                            <li>Configure the redirect URI below as an allowed callback in your provider.</li>
                            <li>Return here and continue to enter the configuration inputs.</li>
                        </ol>
                        <p className="text-xs mt-3 text-gray-600 dark:text-gray-400">
                            For local development, you can redirect to localhost via the <code className="bg-gray-100 dark:bg-gray-800 px-1 rounded">redirect_uri</code> query parameter even if the provider only allows the Rise callback URL.
                        </p>
                    </section>

                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Redirect URI</h2>
                        <div className="mono-uri-field">
                            <code className="flex-1 px-3 py-2 text-xs break-all font-mono">
                                {redirectUri}
                            </code>
                            <Button
                                onClick={async () => {
                                    try {
                                        await copyToClipboard(redirectUri);
                                        showToast('Redirect URI copied to clipboard', 'success');
                                    } catch (err) {
                                        showToast(`Failed to copy: ${err.message}`, 'error');
                                    }
                                }}
                                className="whitespace-nowrap"
                                size="sm"
                                variant="secondary"
                            >
                                Copy
                            </Button>
                        </div>
                    </section>

                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Example Configurations</h2>
                        <div className="space-y-2">
                            {Object.entries(exampleConfigs).map(([key, example]) => (
                                <details key={example.title} className="mono-table-wrap p-3">
                                    <summary className="cursor-pointer text-sm font-semibold text-gray-900 dark:text-gray-200 flex items-center gap-2">
                                        <span className="flex-1">{example.title}</span>
                                        <Button
                                            size="sm"
                                            variant="secondary"
                                            onClick={(e) => {
                                                e.preventDefault();
                                                e.stopPropagation();
                                                applyExampleConfig(key);
                                            }}
                                        >
                                            Apply
                                        </Button>
                                    </summary>
                                    <MonoCodeBlock className="mt-3 text-xs mono-code-block-dotted">
{example.spec}
                                    </MonoCodeBlock>
                                </details>
                            ))}
                        </div>
                    </section>

                    <div className="flex justify-end">
                        <Button onClick={() => setSetupStep(2)} size="md">
                            Next &gt;
                        </Button>
                    </div>
                </div>
            )}

            {setupStep === 2 && (
                <div className="space-y-4">
                        <FormField
                            label="Provider Name"
                            id="oauth-provider-name"
                            value={providerName}
                            onChange={(e) => setProviderName(e.target.value)}
                            placeholder="e.g., Snowflake Production"
                            required
                            helperText="Display name for this OAuth provider"
                        />

                        <FormField
                            label="Description (Optional)"
                            id="oauth-description"
                            value={description}
                            onChange={(e) => setDescription(e.target.value)}
                            placeholder="e.g., OAuth authentication for analytics access"
                            helperText="Human-readable description of this OAuth configuration"
                        />

                        <FormField
                            label="Client ID"
                            id="oauth-client-id"
                            value={clientId}
                            onChange={(e) => setClientId(e.target.value)}
                            placeholder="e.g., ABC123XYZ..."
                            required
                            helperText="OAuth client identifier from your provider"
                        />

                        <div className="space-y-2">
                            <label className="mono-label">
                                Client Secret <span className="text-red-500">*</span> {hasExistingSecret && !clientSecretPlaintext && <span className="text-gray-500 dark:text-gray-400">(configured)</span>}
                                {clientSecretPlaintext && <span className="text-blue-600 dark:text-blue-400">(will be updated)</span>}
                            </label>
                            <div className="flex gap-2">
                                <div className="flex-1 relative">
                                    <input
                                        type={showSecret ? "text" : "password"}
                                        id="oauth-client-secret"
                                        value={clientSecretPlaintext}
                                        onChange={(e) => setClientSecretPlaintext(e.target.value)}
                                        placeholder={clientSecretEncrypted ? "••••••••" : "Enter client secret"}
                                        disabled={isEncrypting}
                                        className="mono-input pr-16 disabled:opacity-50"
                                    />
                                    <button
                                        type="button"
                                        onClick={() => setShowSecret(!showSecret)}
                                        className="absolute right-2 top-1/2 transform -translate-y-1/2 text-xs text-gray-500 hover:text-gray-300"
                                    >
                                        {showSecret ? 'Hide' : 'Show'}
                                    </button>
                                </div>
                                <Button
                                    onClick={handleEncryptSecret}
                                    disabled={!clientSecretPlaintext || clientSecretPlaintext.trim() === '' || isEncrypting}
                                    variant="secondary"
                                    size="md"
                                >
                                    {isEncrypting ? 'Encrypting...' : 'Encrypt'}
                                </Button>
                            </div>
                            <p className="text-sm text-gray-600 dark:text-gray-400">
                                {hasExistingSecret
                                    ? "Secret is configured. Leave blank to keep current secret, or enter a new value and click Encrypt to update it."
                                    : "Enter the OAuth client secret from your provider and click Encrypt to securely store it"}
                            </p>
                        </div>

                        <FormField
                            label="Issuer URL"
                            id="oauth-issuer-url"
                            value={issuerUrl}
                            onChange={(e) => setIssuerUrl(e.target.value)}
                            placeholder="https://accounts.google.com"
                            required
                            helperText="OIDC issuer URL. For OIDC-compliant providers, endpoints are auto-discovered. For non-OIDC providers (GitHub), also set endpoints below."
                        />

                        <FormField
                            label="Scopes"
                            id="oauth-scopes"
                            value={scopes}
                            onChange={(e) => setScopes(e.target.value)}
                            placeholder="openid, email, profile"
                            required
                            helperText="Comma-separated list of OAuth scopes to request"
                        />

                        <div className="border-t border-gray-300 dark:border-gray-700 pt-4 mt-4">
                            <button
                                type="button"
                                onClick={() => setShowAdvanced(!showAdvanced)}
                                className="flex items-center text-sm font-medium text-gray-700 dark:text-gray-300 hover:text-indigo-600 dark:hover:text-indigo-400"
                            >
                                <span className="mr-2">{showAdvanced ? '▼' : '▶'}</span>
                                Advanced: Manual Endpoint Overrides
                            </button>
                            <p className="text-xs text-gray-600 dark:text-gray-500 mt-1">
                                Only needed for non-OIDC providers (GitHub) or if OIDC discovery fails
                            </p>
                        </div>

                        {showAdvanced && (
                            <div className="space-y-4 pl-4 border-l-2 border-gray-300 dark:border-gray-700">
                                <FormField
                                    label="Authorization Endpoint (Optional)"
                                    id="oauth-authorization-endpoint"
                                    value={authorizationEndpoint}
                                    onChange={(e) => setAuthorizationEndpoint(e.target.value)}
                                    placeholder="https://github.com/login/oauth/authorize"
                                    helperText="Override authorization URL (leave empty to use OIDC discovery)"
                                />

                                <FormField
                                    label="Token Endpoint (Optional)"
                                    id="oauth-token-endpoint"
                                    value={tokenEndpoint}
                                    onChange={(e) => setTokenEndpoint(e.target.value)}
                                    placeholder="https://github.com/login/oauth/access_token"
                                    helperText="Override token URL (leave empty to use OIDC discovery)"
                                />
                            </div>
                        )}

                        <div className="pt-2">
                            <Button onClick={() => setSetupStep(1)} variant="secondary" size="sm">
                                &lt; Previous
                            </Button>
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
            return <MonoStatusPill tone="bad">Error</MonoStatusPill>;
        }

        if (status.configured_at) {
            if (status.auth_verified) {
                return <MonoStatusPill tone="ok">Configured</MonoStatusPill>;
            } else {
                return <MonoStatusPill tone="warn">Waiting For Auth</MonoStatusPill>;
            }
        }

        return <MonoStatusPill tone="muted">Not Configured</MonoStatusPill>;
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

    return (
        <Modal isOpen={isOpen} onClose={onClose} title="Integration Guide" maxWidth="max-w-4xl" bodyClassName="mono-modal-body--flush">
                <ModalTabs className="px-6">
                    <MonoTabButton onClick={() => setActiveTab('fragment')} active={activeTab === 'fragment'}>
                        PKCE Flow (SPAs)
                    </MonoTabButton>
                    <MonoTabButton onClick={() => setActiveTab('backend')} active={activeTab === 'backend'}>
                        Token Endpoint (Backend)
                    </MonoTabButton>
                    <MonoTabButton onClick={() => setActiveTab('local')} active={activeTab === 'local'}>
                        Local Development
                    </MonoTabButton>
                </ModalTabs>

                {/* Modal Content */}
                <div className="p-6 overflow-y-auto max-h-[calc(90vh-140px)]">
                    {activeTab === 'fragment' && (
                        <div className="space-y-4">
                            <p className="text-sm text-gray-700 dark:text-gray-300">
                                <strong>Fragment Flow</strong> is the default and recommended approach for single-page applications (SPAs).
                                Tokens are returned in the URL fragment (<code className="bg-white dark:bg-gray-900 px-1 rounded">#access_token=...</code>),
                                which never reaches the server.
                            </p>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Authorization URL:</p>
                                <MonoCodeBlock as="code" className="block break-all">
                                    {authorizeUrl}
                                </MonoCodeBlock>
                            </div>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Example Code:</p>
                                <MonoCodeBlock>
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
                                </MonoCodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'backend' && (
                        <div className="space-y-4">
                            <p className="text-sm text-gray-700 dark:text-gray-300">
                                <strong>Token Endpoint Flow</strong> is designed for server-rendered applications (confidential clients). Your backend receives an
                                authorization code as a query parameter, which it exchanges for OAuth tokens via the RFC 6749-compliant token endpoint using client credentials.
                            </p>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Authorization URL:</p>
                                <MonoCodeBlock as="code" className="block break-all">
                                    {authorizeUrl}
                                </MonoCodeBlock>
                            </div>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Token Endpoint:</p>
                                <MonoCodeBlock as="code" className="block break-all">
                                    POST {tokenUrl}
                                </MonoCodeBlock>
                            </div>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Example Code (Node.js/Express):</p>
                                <MonoCodeBlock>
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
                                </MonoCodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'local' && (
                        <div className="space-y-4">
                            <p className="text-sm text-gray-700 dark:text-gray-300">
                                For local development, you can override the redirect URI to point to your local development server.
                                Rise always handles the OAuth provider callback, so you don't need to register localhost URLs with your OAuth provider.
                            </p>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">PKCE Flow (localhost):</p>
                                <MonoCodeBlock>
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
                                </MonoCodeBlock>
                            </div>

                            <div>
                                <p className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Token Endpoint Flow (localhost):</p>
                                <MonoCodeBlock>
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
                                </MonoCodeBlock>
                            </div>

                            <MonoNotice tone="info" title="How It Works">
                                <p className="text-xs">
                                    The OAuth provider only redirects to <code className="bg-gray-100 dark:bg-gray-800 px-1 rounded">{callbackUrl}</code> (Rise's callback URL).
                                    Rise then redirects to your app's redirect_uri with the authorization code. You don't need to configure localhost URLs in your OAuth provider.
                                </p>
                            </MonoNotice>
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
            <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
                {/* Left column - Main content */}
                <div className="lg:col-span-2 space-y-6">
                    {/* Upstream OAuth Provider Configuration */}
                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Upstream OAuth Provider</h2>
                        <div className="bg-white dark:bg-gray-900 rounded p-4 space-y-4">
                            {/* Provider info */}
                            <div className="space-y-3">
                                <div>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">Provider Name</p>
                                    <p className="text-gray-700 dark:text-gray-300 font-semibold">{spec.provider_name || 'N/A'}</p>
                                </div>
                                {spec.description && (
                                    <div>
                                        <p className="text-sm text-gray-600 dark:text-gray-500">Description</p>
                                        <p className="text-gray-700 dark:text-gray-300">{spec.description}</p>
                                    </div>
                                )}
                                <div>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">Client ID</p>
                                    <p className="text-gray-700 dark:text-gray-300 font-mono text-sm">{spec.client_id || 'N/A'}</p>
                                </div>
                            </div>

                            {/* Divider */}
                            <div className="border-t border-gray-200 dark:border-gray-700"></div>

                            {/* Endpoints */}
                            <div className="space-y-3">
                                <div>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">Issuer URL</p>
                                    <p className="text-gray-700 dark:text-gray-300 font-mono text-xs break-all">{spec.issuer_url || 'N/A'}</p>
                                </div>
                                {spec.authorization_endpoint && (
                                    <div>
                                        <p className="text-sm text-gray-600 dark:text-gray-500">Authorization Endpoint <span className="text-xs text-gray-500">(override)</span></p>
                                        <p className="text-gray-700 dark:text-gray-300 font-mono text-xs break-all">{spec.authorization_endpoint}</p>
                                    </div>
                                )}
                                {spec.token_endpoint && (
                                    <div>
                                        <p className="text-sm text-gray-600 dark:text-gray-500">Token Endpoint <span className="text-xs text-gray-500">(override)</span></p>
                                        <p className="text-gray-700 dark:text-gray-300 font-mono text-xs break-all">{spec.token_endpoint}</p>
                                    </div>
                                )}
                                {!spec.authorization_endpoint && !spec.token_endpoint && (
                                    <p className="text-xs text-gray-500 dark:text-gray-500 italic">
                                        Endpoints auto-discovered via OIDC discovery
                                    </p>
                                )}
                            </div>

                            {/* Divider */}
                            <div className="border-t border-gray-200 dark:border-gray-700"></div>

                            {/* Scopes */}
                            <div>
                                <p className="text-sm text-gray-600 dark:text-gray-500 mb-2">Scopes ({scopesArray.length})</p>
                                {scopesArray.length === 0 ? (
                                    <p className="text-gray-600 dark:text-gray-400 text-sm italic">No scopes configured</p>
                                ) : (
                                    <div className="flex flex-wrap gap-2">
                                        {scopesArray.map((scope, idx) => (
                                            <MonoTag key={idx} color="indigo">
                                                {scope}
                                            </MonoTag>
                                        ))}
                                    </div>
                                )}
                            </div>
                        </div>
                    </section>
                </div>

                {/* Right column - Actions */}
                <div className="lg:col-span-1 space-y-6">
                    {/* Configuration Status */}
                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Status</h2>
                        <div className="mono-table-wrap p-4">
                            {status.error ? (
                                <MonoNotice tone="error">
                                    <p className="text-sm">
                                        <strong>Error:</strong> {status.error}
                                    </p>
                                </MonoNotice>
                            ) : status.configured_at ? (
                                status.auth_verified ? (
                                    <MonoNotice tone="success">
                                        <p className="text-sm">Configured</p>
                                        <p className="text-xs mt-1">
                                            {formatDate(status.configured_at)}
                                        </p>
                                    </MonoNotice>
                                ) : (
                                    <MonoNotice tone="warn">
                                        <p className="text-sm">Waiting For Auth</p>
                                        <p className="text-xs mt-1">
                                            Complete OAuth flow to verify configuration
                                        </p>
                                    </MonoNotice>
                                )
                            ) : (
                                <MonoNotice tone="muted">
                                    <p className="text-sm">
                                        Configuration pending...
                                    </p>
                                </MonoNotice>
                            )}
                        </div>
                    </section>

                    {/* Test OAuth Flow */}
                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Test</h2>
                        <Button
                            onClick={handleTestOAuth}
                            className="w-full"
                            size="lg"
                        >
                            Test OAuth Flow
                        </Button>
                        <p className="text-xs text-gray-600 dark:text-gray-500 mt-2">
                            Test the OAuth flow and return to this page with a notification.
                        </p>
                    </section>

                    {/* Integration Guide Button */}
                    <section>
                        <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Integration</h2>
                        <Button
                            onClick={() => setShowGuideModal(true)}
                            className="w-full"
                            size="lg"
                            variant="secondary"
                        >
                            Integration Guide
                        </Button>
                        <p className="text-xs text-gray-600 dark:text-gray-500 mt-2">
                            View code examples for PKCE Flow, Token Endpoint Flow, and local development.
                        </p>
                    </section>
                </div>
            </div>

            {/* Injected Environment Variables - Full width below the grid */}
            <section className="mt-6">
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Injected Environment Variables</h2>
                <div className="mono-table-wrap p-4">
                    <p className="text-sm text-gray-600 dark:text-gray-400 mb-3">
                        These environment variables are injected into your deployed application:
                    </p>
                    <div className="overflow-x-auto">
                        <MonoTable className="text-sm">
                            <MonoTableHead>
                                <tr className="border-b border-gray-300 dark:border-gray-700">
                                    <th className="text-left py-2 px-3 text-gray-600 dark:text-gray-400 font-medium">Variable</th>
                                    <th className="text-left py-2 px-3 text-gray-600 dark:text-gray-400 font-medium">Value</th>
                                    <th className="py-2 px-3 w-10"></th>
                                </tr>
                            </MonoTableHead>
                            <MonoTableBody className="font-mono text-xs">
                                <tr className="border-b border-gray-200 dark:border-gray-800">
                                    <td className="py-3 px-3 text-gray-700 dark:text-gray-300 whitespace-nowrap">
                                        {extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_ID
                                    </td>
                                    <td className="py-3 px-3 text-gray-900 dark:text-gray-200 break-all">
                                        {status?.rise_client_id || `${projectName}-${extensionName}`}
                                    </td>
                                    <td className="py-3 px-3">
                                        <button
                                            type="button"
                                            onClick={async () => {
                                                try {
                                                    await copyToClipboard(status?.rise_client_id || `${projectName}-${extensionName}`);
                                                    showToast('Client ID copied', 'success');
                                                } catch (err) {
                                                    showToast(`Failed to copy: ${err.message}`, 'error');
                                                }
                                            }}
                                            className="p-1 hover:bg-gray-200 dark:hover:bg-gray-700 rounded transition-colors"
                                            title="Copy to clipboard"
                                        >
                                            <svg className="w-4 h-4 text-gray-500 dark:text-gray-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                                            </svg>
                                        </button>
                                    </td>
                                </tr>
                                <tr className="border-b border-gray-200 dark:border-gray-800">
                                    <td className="py-3 px-3 text-gray-700 dark:text-gray-300 whitespace-nowrap">
                                        {extensionName.toUpperCase().replace(/-/g, '_')}_CLIENT_SECRET
                                    </td>
                                    <td className="py-3 px-3 text-gray-500 dark:text-gray-500">
                                        ••••••••
                                    </td>
                                    <td className="py-3 px-3">
                                        {status?.rise_client_secret && (
                                            <button
                                                type="button"
                                                onClick={async () => {
                                                    try {
                                                        await copyToClipboard(status.rise_client_secret);
                                                        showToast('Client secret copied', 'success');
                                                    } catch (err) {
                                                        showToast(`Failed to copy: ${err.message}`, 'error');
                                                    }
                                                }}
                                                className="p-1 hover:bg-gray-200 dark:hover:bg-gray-700 rounded transition-colors"
                                                title="Copy to clipboard"
                                            >
                                                <svg className="w-4 h-4 text-gray-500 dark:text-gray-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                                                </svg>
                                            </button>
                                        )}
                                    </td>
                                </tr>
                                <tr className="border-b border-gray-200 dark:border-gray-800">
                                    <td className="py-3 px-3 text-gray-700 dark:text-gray-300 whitespace-nowrap">
                                        {extensionName.toUpperCase().replace(/-/g, '_')}_ISSUER
                                    </td>
                                    <td className="py-3 px-3 text-gray-900 dark:text-gray-200 break-all">
                                        <a
                                            href={`${backendUrl}/oidc/${projectName}/${extensionName}/.well-known/openid-configuration`}
                                            target="_blank"
                                            rel="noopener noreferrer"
                                            className="text-indigo-600 dark:text-indigo-400 hover:underline"
                                        >
                                            {`${backendUrl}/oidc/${projectName}/${extensionName}`}
                                        </a>
                                    </td>
                                    <td className="py-3 px-3">
                                        <button
                                            type="button"
                                            onClick={async () => {
                                                try {
                                                    await copyToClipboard(`${backendUrl}/oidc/${projectName}/${extensionName}`);
                                                    showToast('Issuer URL copied', 'success');
                                                } catch (err) {
                                                    showToast(`Failed to copy: ${err.message}`, 'error');
                                                }
                                            }}
                                            className="p-1 hover:bg-gray-200 dark:hover:bg-gray-700 rounded transition-colors"
                                            title="Copy to clipboard"
                                        >
                                            <svg className="w-4 h-4 text-gray-500 dark:text-gray-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                                            </svg>
                                        </button>
                                    </td>
                                </tr>
                            </MonoTableBody>
                        </MonoTable>
                    </div>
                    <p className="text-xs text-gray-500 dark:text-gray-500 mt-3">
                        Click the issuer URL to view the OIDC discovery document.
                    </p>
                </div>
            </section>
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
        <div className="space-y-6">
            <div className="space-y-4">
                <FormField
                    label="Additional Blocked Roles"
                    id="snowflake-blocked-roles"
                    type="textarea"
                    value={blockedRoles}
                    onChange={(e) => setBlockedRoles(e.target.value)}
                    placeholder="SYSADMIN, USERADMIN"
                    helperText="Comma-separated list added to backend blocked-role defaults."
                />

                <FormField
                    label="Additional OAuth Scopes"
                    id="snowflake-scopes"
                    type="textarea"
                    value={scopes}
                    onChange={(e) => setScopes(e.target.value)}
                    placeholder="session:role:ANALYST, session:role:DEVELOPER"
                    helperText="Comma-separated scopes added to backend defaults."
                />

                <MonoNotice tone="success" title="Secondary Roles">
                    <p>
                        Secondary roles are enabled by default (OAUTH_USE_SECONDARY_ROLES = IMPLICIT).
                    </p>
                </MonoNotice>
            </div>
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
        <>
            <section className="mb-6 space-y-3">
                <div className="flex items-center space-x-3">
                    <h2 className="text-xl font-semibold text-gray-900 dark:text-gray-100">Snowflake OAuth Provisioner</h2>
                    {getStateBadge()}
                </div>
                <p className="text-sm text-gray-600 dark:text-gray-400">
                    Automatically provisions Snowflake SECURITY INTEGRATIONs and a linked OAuth extension.
                </p>
                {status.state === 'Available' && status.oauth_extension_name ? (
                    <MonoNotice tone="success" title="Next Action">
                        <p>Provisioning completed. Continue by reviewing or testing the linked OAuth extension.</p>
                        <div className="mt-2">
                            <a
                                href={`/project/${projectName}/extensions/oauth/${status.oauth_extension_name}`}
                                className="mono-btn mono-btn-primary mono-btn-sm inline-flex"
                            >
                                Open Linked OAuth Extension
                            </a>
                        </div>
                    </MonoNotice>
                ) : (
                    <MonoNotice tone="warn" title="Current State">
                        <p>
                            This extension is still progressing through provisioning states. Use the status sections below to track readiness and errors.
                        </p>
                    </MonoNotice>
                )}
            </section>

            <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
                {/* Snowflake Integration Details */}
                <section className="bg-gray-100 dark:bg-gray-800 rounded-lg p-4">
                    <h3 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-4">Snowflake Integration Status</h3>

                    <div className="space-y-3 text-sm">
                        {status.integration_name && (
                            <div>
                                <span className="text-gray-600 dark:text-gray-500">Integration Name:</span>
                                <span className="text-gray-700 dark:text-gray-300 ml-2 font-mono">{status.integration_name}</span>
                            </div>
                        )}

                        {status.oauth_client_id && (
                            <div>
                                <span className="text-gray-600 dark:text-gray-500">OAuth Client ID:</span>
                                <span className="text-gray-700 dark:text-gray-300 ml-2 font-mono text-xs">{status.oauth_client_id}</span>
                            </div>
                        )}

                        {status.redirect_uri && (
                            <div>
                                <span className="text-gray-600 dark:text-gray-500">Redirect URI:</span>
                                <span className="text-gray-700 dark:text-gray-300 ml-2 font-mono text-xs">{status.redirect_uri}</span>
                            </div>
                        )}

                        {status.created_at && (
                            <div>
                                <span className="text-gray-600 dark:text-gray-500">Created:</span>
                                <span className="text-gray-700 dark:text-gray-300 ml-2">{formatDate(status.created_at)}</span>
                            </div>
                        )}
                    </div>

                    {status.state === 'Available' && (
                        <MonoNotice className="mt-4" tone="success">
                            <p className="text-xs">Snowflake integration is active and configured.</p>
                        </MonoNotice>
                    )}

                    {status.error && (
                        <MonoNotice className="mt-4" tone="error">
                            <p className="text-xs">
                                <strong>Error:</strong> {status.error}
                            </p>
                        </MonoNotice>
                    )}
                </section>

                {/* OAuth Extension Details */}
                <section className="bg-gray-100 dark:bg-gray-800 rounded-lg p-4">
                    <h3 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-4">Linked OAuth Extension</h3>

                    {status.oauth_extension_name ? (
                        <div className="space-y-3">
                            <div className="text-sm">
                                <span className="text-gray-600 dark:text-gray-500">Extension Name:</span>
                                <span className="text-gray-700 dark:text-gray-300 ml-2 font-mono">{status.oauth_extension_name}</span>
                            </div>

                            {status.state === 'Available' && (
                                <div className="mt-4">
                                    <a
                                        href={`/project/${projectName}/extensions/oauth/${status.oauth_extension_name}`}
                                        className="mono-btn mono-btn-primary mono-btn-sm inline-flex"
                                    >
                                        View OAuth Extension
                                    </a>
                                </div>
                            )}

                            <MonoNotice className="mt-4" tone="info">
                                <p className="text-xs">
                                    The OAuth extension is automatically created and managed by this provisioner.
                                    Users can authenticate using their Snowflake credentials.
                                </p>
                            </MonoNotice>
                        </div>
                    ) : (
                        <div className="text-sm text-gray-600 dark:text-gray-400">
                            OAuth extension will be created during provisioning
                        </div>
                    )}
                </section>

                {/* Configuration Summary */}
                <section className="bg-gray-100 dark:bg-gray-800 rounded-lg p-4 lg:col-span-2">
                    <h3 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-4">Configuration</h3>

                    <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
                        <div>
                            <h4 className="text-sm font-semibold text-gray-600 dark:text-gray-400 mb-2">Blocked Roles</h4>
                            {spec.blocked_roles && spec.blocked_roles.length > 0 ? (
                                <div className="flex flex-wrap gap-2">
                                    {spec.blocked_roles.map((role, idx) => (
                                        <MonoTag key={idx} color="red">
                                            {role}
                                        </MonoTag>
                                    ))}
                                </div>
                            ) : (
                                <p className="text-xs text-gray-600 dark:text-gray-500">Using backend defaults only</p>
                            )}
                        </div>

                        <div>
                            <h4 className="text-sm font-semibold text-gray-600 dark:text-gray-400 mb-2">OAuth Scopes</h4>
                            {spec.scopes && spec.scopes.length > 0 ? (
                                <div className="flex flex-wrap gap-2">
                                    {spec.scopes.map((scope, idx) => (
                                        <MonoTag key={idx} color="blue">
                                            {scope}
                                        </MonoTag>
                                    ))}
                                </div>
                            ) : (
                                <p className="text-xs text-gray-600 dark:text-gray-500">Using backend defaults only</p>
                            )}
                        </div>
                    </div>

                    <MonoNotice className="mt-4" tone="warn">
                        <p className="text-xs">
                            <strong>Note:</strong> Additional roles and scopes are combined with backend defaults
                            (not replaced). ACCOUNTADMIN, ORGADMIN, and SECURITYADMIN are always blocked.
                        </p>
                    </MonoNotice>
                </section>
            </div>
        </>
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

    return (
        <div className="space-y-4">
            <MonoNotice tone="info" title="Fixed Environment Variables">
                <p>This extension automatically injects the following environment variables into every deployment. No configuration is required.</p>
                <div className="mt-3 space-y-1 font-mono text-xs">
                    <div><span className="text-blue-600 dark:text-blue-400">S3_BUCKET_NAME</span> — the name of the provisioned S3 bucket</div>
                    <div><span className="text-blue-600 dark:text-blue-400">AWS_ACCESS_KEY_ID</span> — IAM access key ID</div>
                    <div><span className="text-blue-600 dark:text-blue-400">AWS_SECRET_ACCESS_KEY</span> — IAM secret access key</div>
                    <div><span className="text-blue-600 dark:text-blue-400">AWS_REGION</span> — AWS region of the bucket</div>
                </div>
            </MonoNotice>
            <MonoNotice tone="muted" title="Bucket Deletion">
                <p>When you delete this extension, the IAM user and access key are removed immediately. The S3 bucket is only deleted if it is empty — non-empty buckets block deletion until you choose to empty them.</p>
            </MonoNotice>
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
        <div className="space-y-6">
            <section className="space-y-3">
                <div className="flex items-center gap-3">
                    <h2 className="text-xl font-semibold text-gray-900 dark:text-gray-100">AWS S3 Bucket</h2>
                    {renderStatePill(status.state)}
                </div>
                {isAvailable ? (
                    <MonoNotice tone="success" title="Current State">
                        <p>S3 bucket is provisioned and credentials are ready. Deploy your project to inject the environment variables.</p>
                    </MonoNotice>
                ) : isFailed ? (
                    <MonoNotice tone="bad" title="Provisioning Failed">
                        <p>{status.error || 'An error occurred during provisioning. The reconciler will retry automatically.'}</p>
                    </MonoNotice>
                ) : isDeletionBlocked ? (
                    <MonoNotice tone="warn" title="Deletion Blocked">
                        <p>{status.error || 'The S3 bucket could not be deleted because it is not empty.'}</p>
                        <div className="mt-3">
                            <Button variant="danger" size="sm" onClick={handleForceEmpty} disabled={forceEmptying}>
                                {forceEmptying ? 'Enabling...' : 'Empty bucket and delete'}
                            </Button>
                        </div>
                    </MonoNotice>
                ) : isDeleting ? (
                    <MonoNotice tone="muted" title="Deleting">
                        <p>{status.error || 'The extension is being cleaned up. IAM user and S3 bucket will be removed.'}</p>
                    </MonoNotice>
                ) : (
                    <MonoNotice tone="warn" title="Current State">
                        <p>Provisioning is in progress. Deployments will fail until the bucket is available.</p>
                    </MonoNotice>
                )}
            </section>

            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Bucket Details</h2>
                <div className="bg-white dark:bg-gray-900 rounded p-4 grid grid-cols-2 gap-4">
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">State</p>
                        <div className="mt-1">{renderStatePill(status.state)}</div>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Bucket Name</p>
                        <p className="text-gray-700 dark:text-gray-300 font-mono text-sm">{status.bucket_name || '—'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">IAM User</p>
                        <p className="text-gray-700 dark:text-gray-300 font-mono text-sm">{status.iam_user_name || '—'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Access Key ID</p>
                        <p className="text-gray-700 dark:text-gray-300 font-mono text-sm">{status.iam_access_key_id || '—'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Region</p>
                        <p className="text-gray-700 dark:text-gray-300 font-mono text-sm">{status.region || '—'}</p>
                    </div>
                </div>
            </section>

            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Injected Environment Variables</h2>
                <MonoTable>
                    <MonoTableHead>
                        <MonoTableRow>
                            <MonoTh>Variable</MonoTh>
                            <MonoTh>Value</MonoTh>
                        </MonoTableRow>
                    </MonoTableHead>
                    <MonoTableBody>
                        <MonoTableRow>
                            <MonoTd><span className="font-mono text-sm">S3_BUCKET_NAME</span></MonoTd>
                            <MonoTd><span className="font-mono text-sm text-gray-600 dark:text-gray-400">{status.bucket_name || '(pending)'}</span></MonoTd>
                        </MonoTableRow>
                        <MonoTableRow>
                            <MonoTd><span className="font-mono text-sm">AWS_ACCESS_KEY_ID</span></MonoTd>
                            <MonoTd><span className="font-mono text-sm text-gray-600 dark:text-gray-400">{status.iam_access_key_id || '(pending)'}</span></MonoTd>
                        </MonoTableRow>
                        <MonoTableRow>
                            <MonoTd><span className="font-mono text-sm">AWS_SECRET_ACCESS_KEY</span></MonoTd>
                            <MonoTd><MonoTag color="muted">protected</MonoTag></MonoTd>
                        </MonoTableRow>
                        <MonoTableRow>
                            <MonoTd><span className="font-mono text-sm">AWS_REGION</span></MonoTd>
                            <MonoTd><span className="font-mono text-sm text-gray-600 dark:text-gray-400">{status.region || '(pending)'}</span></MonoTd>
                        </MonoTableRow>
                    </MonoTableBody>
                </MonoTable>
            </section>
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
        <div className="space-y-6">
            <section className="space-y-3">
                <div className="flex items-center gap-3">
                    <h2 className="text-xl font-semibold text-gray-900 dark:text-gray-100">AWS RDS Provisioner</h2>
                    {getInstanceStateBadge()}
                </div>
                {String(status.state || '').toLowerCase() === 'available' ? (
                    <MonoNotice tone="success" title="Current State">
                        <p>Database infrastructure is available. Review endpoint and environment variable settings below before deploying.</p>
                    </MonoNotice>
                ) : (
                    <MonoNotice tone="warn" title="Current State">
                        <p>Provisioning is in progress or requires attention. New dependent deployments may be blocked until the instance is available.</p>
                    </MonoNotice>
                )}
            </section>

            {/* Instance Information */}
            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">RDS Instance Status</h2>
                <div className="bg-white dark:bg-gray-900 rounded p-4 grid grid-cols-2 gap-4">
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">State</p>
                        <div className="mt-1">{getInstanceStateBadge()}</div>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Instance ID</p>
                        <p className="text-gray-700 dark:text-gray-300">{status.instance_id || 'N/A'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Instance Size</p>
                        <p className="text-gray-700 dark:text-gray-300">{status.instance_size || 'N/A'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Engine</p>
                        <p className="text-gray-700 dark:text-gray-300">{spec.engine || 'postgres'} {spec.engine_version || ''}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Endpoint</p>
                        <p className="text-gray-700 dark:text-gray-300 font-mono text-xs">{status.endpoint || 'Pending...'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Database Isolation</p>
                        <p className="text-gray-700 dark:text-gray-300 capitalize">{spec.database_isolation || 'shared'}</p>
                    </div>
                    <div>
                        <p className="text-sm text-gray-600 dark:text-gray-500">Master Username</p>
                        <p className="text-gray-700 dark:text-gray-300">{status.master_username || 'N/A'}</p>
                    </div>
                </div>

                {status.error && (
                    <MonoNotice className="mt-3" tone="error">
                        <p className="text-sm">
                            <strong>Error:</strong> {status.error}
                        </p>
                    </MonoNotice>
                )}
            </section>

            {/* Databases */}
            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">
                    Databases ({Object.keys(databases).length})
                </h2>

                {Object.keys(databases).length === 0 ? (
                    <div className="bg-white dark:bg-gray-900 rounded p-4 text-gray-600 dark:text-gray-400 text-center">
                        No databases provisioned yet
                    </div>
                ) : (
                    <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
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
            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Environment Variables</h2>
                <div className="bg-white dark:bg-gray-900 rounded p-4 space-y-3">
                    <div>
                        <span className="text-gray-600 dark:text-gray-400 text-sm">Database URL Variable:</span>
                        <code className="ml-2 bg-gray-100 dark:bg-gray-800 px-2 py-1 rounded text-gray-900 dark:text-gray-200 text-sm">
                            {spec.database_url_env_var || 'DATABASE_URL'}
                        </code>
                    </div>
                    <label className="flex items-center space-x-2">
                        <input
                            type="checkbox"
                            checked={spec.inject_pg_vars !== false}
                            disabled
                            className="rounded"
                        />
                        <span className="text-gray-700 dark:text-gray-300 text-sm">
                            Inject <code className="mono-token-accent">PG*</code> variables
                        </span>
                    </label>
                </div>
            </section>
        </div>
    );
}

// Database Card Component
function DatabaseCard({ name, status }) {
    // Determine status badge tone/label
    let badgeTone = 'muted';
    let statusText = status.status || 'Unknown';
    const state = (status.status || '').toLowerCase();

    switch (state) {
        case 'available':
            badgeTone = 'ok';
            break;
        case 'pending':
        case 'creatingdatabase':
        case 'creatinguser':
            badgeTone = 'warn';
            statusText = 'Provisioning';
            break;
        case 'terminating':
            badgeTone = 'bad';
            break;
        default:
            badgeTone = 'muted';
    }

    const isScheduledForCleanup = status.cleanup_scheduled_at != null;
    const cleanupDate = isScheduledForCleanup
        ? new Date(status.cleanup_scheduled_at)
        : null;
    const cleanupTime = cleanupDate
        ? new Date(cleanupDate.getTime() + 60 * 60 * 1000) // +1 hour
        : null;

    return (
        <div className="bg-white dark:bg-gray-900 rounded-lg p-4 border border-gray-300 dark:border-gray-700">
            <div className="flex items-center justify-between mb-3">
                <h3 className="text-gray-900 dark:text-white font-semibold">{name}</h3>
                <MonoStatusPill tone={badgeTone}>{statusText}</MonoStatusPill>
            </div>

            <div className="space-y-2 text-sm">
                <div>
                    <span className="text-gray-600 dark:text-gray-500">User:</span>
                    <span className="text-gray-700 dark:text-gray-300 ml-2 font-mono">{status.user}</span>
                </div>

                {isScheduledForCleanup && cleanupTime && (
                    <MonoNotice className="mt-3" tone="warn">
                        <p className="text-xs">
                            <strong>Cleanup Scheduled</strong>
                        </p>
                        <p className="text-xs mt-1">
                            Will be deleted at {formatDate(cleanupTime.toISOString())}
                        </p>
                    </MonoNotice>
                )}

                {status.status === 'Available' && !isScheduledForCleanup && (
                    <MonoNotice className="mt-3" tone="success">
                        <p className="text-xs">
                            Database is active and ready.
                        </p>
                    </MonoNotice>
                )}
            </div>
        </div>
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
