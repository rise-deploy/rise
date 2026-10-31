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
import { OAuthExtensionUI } from './oauth-extension-config';

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
        const newSpec: Record<string, unknown> = {
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
        return <OAuthExtensionUI spec={spec} onChange={onChange} projectName={projectName} instanceName={instanceName} isEnabled={isEnabled} />;
    }
};

// Integration Guide Modal Component
function IntegrationGuideModal({ isOpen, onClose, projectName, extensionName, clientId, isPublic }) {
    const [activeTab, setActiveTab] = useState(isPublic ? 'spa' : 'backend');

    const backendUrl = CONFIG.backendUrl.replace(/\/$/, '');
    const authorizeUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/authorize`;
    const callbackUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/callback`;
    const tokenUrl = `${backendUrl}/oidc/${projectName}/${extensionName}/token`;
    const envPrefix = extensionName.toUpperCase().replace(/-/g, '_');

    const codeLabelStyle: React.CSSProperties = { fontSize: 13, fontWeight: 600, color: 'var(--text)', margin: '0 0 6px' };
    const paraStyle: React.CSSProperties = { fontSize: 13, color: 'var(--text-muted)', margin: 0, lineHeight: 1.6 };
    const strongStyle: React.CSSProperties = { color: 'var(--text)' };
    const monoStyle: React.CSSProperties = { color: 'var(--text)' };

    return (
        <Modal isOpen={isOpen} onClose={onClose} title="Integration Guide" width="wide">
                <Tabs
                    tabs={[
                        { id: 'spa', label: 'Public client (SPAs)' },
                        { id: 'backend', label: 'Confidential client (Backend)' },
                        { id: 'local', label: 'Local Development' },
                    ]}
                    active={activeTab}
                    onChange={setActiveTab}
                />

                {/* Modal Content */}
                <div style={{ display: 'flex', flexDirection: 'column', gap: 16, marginTop: 16 }}>
                    {activeTab === 'spa' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                <strong style={strongStyle}>Public clients</strong> are browser and native apps that cannot keep a secret.
                                They must use PKCE, receive no client secret, and get single-use refresh tokens.
                                Set <code className="mono" style={monoStyle}>client_type</code> to <code className="mono" style={monoStyle}>public</code> in the extension configuration.
                                {!isPublic && ' This extension is currently a confidential client, so the example below is rejected until you change it.'}
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
                                <p style={codeLabelStyle}>Example Code:</p>
                                <CodeBlock>
{`const CLIENT_ID = '${clientId}';
const REDIRECT_URI = window.location.origin + '/callback';

const b64url = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes)))
  .replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/, '');

// 1. Start login with a PKCE challenge
async function login() {
  const verifier = b64url(crypto.getRandomValues(new Uint8Array(32)));
  const challenge = b64url(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier)));
  const state = b64url(crypto.getRandomValues(new Uint8Array(16)));
  sessionStorage.setItem('oauth', JSON.stringify({ verifier, state }));

  window.location.href = '${authorizeUrl}?' + new URLSearchParams({
    redirect_uri: REDIRECT_URI,
    code_challenge: challenge,
    code_challenge_method: 'S256',
    state,
  });
}

// 2. On /callback, exchange the code. There is no client secret.
async function handleCallback() {
  const params = new URLSearchParams(window.location.search);
  const saved = JSON.parse(sessionStorage.getItem('oauth') || '{}');
  if (params.get('state') !== saved.state) throw new Error('state mismatch');

  const res = await fetch('${tokenUrl}', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'authorization_code',
      code: params.get('code'),
      client_id: CLIENT_ID,
      redirect_uri: REDIRECT_URI, // must match the authorize request
      code_verifier: saved.verifier,
    }),
  });
  if (!res.ok) throw new Error('Token exchange failed: ' + (await res.json()).error);
  return res.json(); // { access_token, id_token, refresh_token, ... }
}

// 3. Refresh. Every refresh token is single use: store the new one.
async function refresh(refreshToken) {
  const res = await fetch('${tokenUrl}', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'refresh_token',
      refresh_token: refreshToken,
      client_id: CLIENT_ID,
    }),
  });
  return res.json(); // refresh_token is the successor
}`}
                                </CodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'backend' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                <strong style={strongStyle}>Confidential clients</strong> are apps with a backend. Your server exchanges the authorization code
                                at the RFC 6749 token endpoint and must send the client secret on every token request, code and refresh.
                                PKCE is optional and enforced when you use it. The <code className="mono" style={monoStyle}>refresh_token</code> in
                                the response is a Rise handle, never the provider's token.
                                {isPublic && ' This extension is currently a public client, so the example below is rejected until you change it.'}
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
{`const REDIRECT_URI = 'https://my-app.example.com/oauth/callback';

// Initiate OAuth login
app.get('/login', (req, res) => {
  req.session.state = crypto.randomUUID();
  const params = new URLSearchParams({ redirect_uri: REDIRECT_URI, state: req.session.state });
  res.redirect('${authorizeUrl}?' + params);
});

// Handle OAuth callback
app.get('/oauth/callback', async (req, res) => {
  const { code, state } = req.query;
  if (!code || state !== req.session.state) {
    return res.status(400).send('Invalid callback');
  }

  // Exchange authorization code for tokens
  const response = await fetch('${tokenUrl}', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'authorization_code',
      code,
      redirect_uri: REDIRECT_URI, // must match the authorize request
      client_id: process.env.${envPrefix}_CLIENT_ID,
      client_secret: process.env.${envPrefix}_CLIENT_SECRET,
    }),
  });

  if (!response.ok) {
    console.error('Token exchange failed', await response.json());
    return res.status(500).send('Authentication failed');
  }

  // { access_token, token_type, expires_in, refresh_token, id_token, ... }
  req.session.tokens = await response.json(); // HttpOnly session cookie
  res.redirect('/dashboard');
});

// Refresh: the secret is required here too
async function refresh(refreshToken) {
  const response = await fetch('${tokenUrl}', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'refresh_token',
      refresh_token: refreshToken,
      client_id: process.env.${envPrefix}_CLIENT_ID,
      client_secret: process.env.${envPrefix}_CLIENT_SECRET,
    }),
  });
  return response.json();
}`}
                                </CodeBlock>
                            </div>
                        </div>
                    )}

                    {activeTab === 'local' && (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                            <p style={paraStyle}>
                                Every authorize request carries a <code className="mono" style={monoStyle}>redirect_uri</code>. Rise accepts the URLs of the
                                project's active deployments (including custom domains) and, for local development, http URLs on
                                {' '}<code className="mono" style={monoStyle}>localhost</code>, <code className="mono" style={monoStyle}>127.0.0.1</code> and{' '}
                                <code className="mono" style={monoStyle}>[::1]</code> with any port and path. Send the same value again with the code exchange.
                            </p>

                            <div>
                                <p style={codeLabelStyle}>Local redirect:</p>
                                <CodeBlock>
{`const REDIRECT_URI = 'http://localhost:3000/callback';

window.location.href = '${authorizeUrl}?' + new URLSearchParams({
  redirect_uri: REDIRECT_URI,
  // public clients also send code_challenge and code_challenge_method=S256
});

// In the callback handler, repeat redirect_uri in the token request
// (plus code_verifier for public clients, or client_secret for confidential ones).`}
                                </CodeBlock>
                            </div>

                            <Alert tone="info" icon="info">
                                <strong>How It Works.</strong>{' '}
                                The OAuth provider only redirects to <code className="mono" style={monoStyle}>{callbackUrl}</code> (Rise's callback URL).
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

    const isPublic = spec.client_type === 'public';
    const clientId = status?.rise_client_id || `${projectName}-${extensionName}`;

    // The test runs through the project API, which checks the signed-in
    // member and records the result; it returns to this page afterwards.
    const handleTestOAuth = () => {
        const testUrl = `/api/v1/projects/${encodeURIComponent(projectName)}/extensions/${encodeURIComponent(extensionName)}/oauth/test`;
        window.location.href = `${testUrl}?return_to=${encodeURIComponent(window.location.href)}`;
    };

    return (
        <>
            <IntegrationGuideModal
                isOpen={showGuideModal}
                onClose={() => setShowGuideModal(false)}
                projectName={projectName}
                extensionName={extensionName}
                clientId={clientId}
                isPublic={isPublic}
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
                                <KVRow k="Client Authentication">
                                    {spec.token_endpoint_auth_method === 'client_secret_basic'
                                        ? 'HTTP Basic header (client_secret_basic)'
                                        : 'Request body (client_secret_post)'}
                                </KVRow>
                                <KVRow k="App Client Type">
                                    {isPublic ? 'Public (PKCE required, no client secret)' : 'Confidential (client secret required)'}
                                </KVRow>
                                <KVRow k="Refresh Tokens">
                                    {isPublic || spec.refresh_token_mode === 'rotating'
                                        ? 'Rotating (single-use handles)'
                                        : 'Sealed (stateless handles)'}
                                </KVRow>
                                <KVRow k={`Scopes (${scopesArray.length})`}>
                                    {scopesArray.length === 0 ? (
                                        <span style={{ color: 'var(--text-soft)', fontStyle: 'italic' }}>None requested</span>
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
                                        <div style={{ fontSize: 12, marginTop: 2 }}>Run the OAuth test to verify configuration</div>
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
                                Sign in through this extension as yourself and return to this page with the result. Only project members can run the test.
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
                                View code examples for public clients, confidential clients, and local development.
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
                            {!isPublic && (
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
                            )}
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

                {/* Injected Environment Variables */}
                {status.oauth_extension_name && (
                    <Panel style={{ gridColumn: '1 / -1' }}>
                        <PanelHead
                            title="Injected Environment Variables"
                            sub="The linked OAuth extension injects these environment variables into your deployed application."
                        />
                        <PanelBody>
                            <table className="r-table" style={{ width: '100%' }}>
                                <thead>
                                    <tr>
                                        <th style={{ textAlign: 'left' }}>Variable</th>
                                        <th style={{ textAlign: 'left' }}>Source</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <tr>
                                        <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{status.oauth_extension_name.toUpperCase().replace(/-/g, '_')}_CLIENT_ID</span></td>
                                        <td><span style={{ color: 'var(--text-muted)' }}>OAuth client ID</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{status.oauth_extension_name.toUpperCase().replace(/-/g, '_')}_CLIENT_SECRET</span></td>
                                        <td><span className="r-var-badge protected"><Icon name="lock" size={10} />Protected</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{status.oauth_extension_name.toUpperCase().replace(/-/g, '_')}_ISSUER</span></td>
                                        <td><span style={{ color: 'var(--text-muted)' }}>OIDC issuer URL</span></td>
                                    </tr>
                                </tbody>
                            </table>
                        </PanelBody>
                    </Panel>
                )}
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
                                <td><span className="r-var-badge protected"><Icon name="lock" size={10} />Protected</span></td>
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

            {/* Injected Environment Variables */}
            <Panel>
                <PanelHead title="Injected Environment Variables" sub="These environment variables are injected into your deployed application." />
                <PanelBody>
                    <table className="r-table" style={{ width: '100%' }}>
                        <thead>
                            <tr>
                                <th style={{ textAlign: 'left' }}>Variable</th>
                                <th style={{ textAlign: 'left' }}>Source</th>
                            </tr>
                        </thead>
                        <tbody>
                            <tr>
                                <td><span className="mono" style={{ whiteSpace: 'nowrap' }}>{spec.database_url_env_var || 'DATABASE_URL'}</span></td>
                                <td><span style={{ color: 'var(--text-muted)' }}>Full PostgreSQL connection string</span></td>
                            </tr>
                            {spec.inject_pg_vars !== false && (
                                <>
                                    <tr>
                                        <td><span className="mono">PGHOST</span></td>
                                        <td><span className="mono" style={{ color: 'var(--text-muted)' }}>{status.endpoint || '(pending)'}</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono">PGPORT</span></td>
                                        <td><span style={{ color: 'var(--text-muted)' }}>Database port</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono">PGUSER</span></td>
                                        <td><span style={{ color: 'var(--text-muted)' }}>Database user</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono">PGPASSWORD</span></td>
                                        <td><span className="r-var-badge protected"><Icon name="lock" size={10} />Protected</span></td>
                                    </tr>
                                    <tr>
                                        <td><span className="mono">PGDATABASE</span></td>
                                        <td><span style={{ color: 'var(--text-muted)' }}>Database name</span></td>
                                    </tr>
                                </>
                            )}
                        </tbody>
                    </table>
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
