// @ts-nocheck
import { createElement, lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { navigate } from '../lib/navigation';
import { formatDate, formatRelativeTimeRounded } from '../lib/utils';
import { useToast } from '../components/toast';
import {
    Alert as RAlert,
    Button as RButton,
    Combobox as RCombobox,
    ConfirmDialog as RConfirmDialog,
    Empty as REmpty,
    EnvironmentColorDot,
    EnvironmentColorPicker,
    Field as RField,
    GroupBar as RGroupBar,
    GroupPill as RGroupPill,
    Input as RInput,
    KV as RKV,
    KVRow as RKVRow,
    Modal as RModal,
    Panel as RPanel,
    PanelBody as RPanelBody,
    PanelHead as RPanelHead,
    Pill as RPill,
    SearchInput as RSearchInput,
    Segmented as RSegmented,
    Status as RStatus,
    Tabs as RTabs,
    Textarea as RTextarea,
} from '../components/r-ui';
import { validateJson } from '../lib/json-validate';
import { MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
import { Icon } from '../components/icon';

// CodeMirror is heavy; load it only when an extension's Spec tab is opened.
const JsonEditor = lazy(() => import('../components/json-editor'));
const JsonEditorFallback = () => (
    <div style={{ padding: 20, fontSize: 12.5, color: 'var(--text-soft)', border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)' }}>
        Loading editor…
    </div>
);
import { LoadingState, ErrorState } from '../components/states';
import {
  AwsRdsDetailView,
  AwsRdsExtensionUI,
  OAuthDetailView,
  OAuthExtensionUI,
  SnowflakeOAuthDetailView,
  SnowflakeOAuthExtensionUI,
  getExtensionDetailView,
  getExtensionIcon,
  getExtensionStatusBadge,
  getExtensionUI,
  getExtensionUIAPI,
  hasExtensionDetailView,
  hasExtensionUI,
} from './extension-ui';

function extensionDocsHref(extensionType) {
    return `/docs/extensions/${extensionType}`;
}

const PREVIEW_EXTENSIONS_STORAGE_KEY = 'rise.previewExtensions';
const PREVIEW_EXTENSION_CATALOG = [
    {
        extension_type: 'aws-rds-provisioner',
        display_name: 'AWS RDS',
        description: 'AWS RDS provisioner extension',
        spec_schema: {
            type: 'object',
            additionalProperties: false,
            properties: {
                engine: { type: 'string', enum: ['postgres'], default: 'postgres' },
                engine_version: { type: 'string', default: '' },
                database_isolation: { type: 'string', enum: ['shared', 'isolated'], default: 'shared' },
                database_url_env_var: { type: 'string', default: 'DATABASE_URL' },
                inject_pg_vars: { type: 'boolean', default: true },
            },
            required: ['engine'],
        },
    },
    {
        extension_type: 'snowflake-oauth-provisioner',
        display_name: 'Snowflake OAuth',
        description: 'Snowflake OAuth provisioner extension',
        spec_schema: {
            type: 'object',
            additionalProperties: false,
            properties: {
                oauth_extension_name: { type: 'string' },
                snowflake_account_locator: { type: 'string' },
                allowed_roles: { type: 'array', items: { type: 'string' } },
            },
            required: ['oauth_extension_name', 'snowflake_account_locator'],
        },
    },
];

function resolvePreviewExtensionIds() {
    const catalogIds = PREVIEW_EXTENSION_CATALOG.map(ext => ext.extension_type);
    const parseRawValue = (raw) => {
        if (!raw) return [];
        const normalized = raw.trim().toLowerCase();
        if (!normalized) return [];
        if (['1', 'true', 'all', '*'].includes(normalized)) return catalogIds;
        if (['0', 'false', 'none', 'off'].includes(normalized)) return [];
        return raw.split(',').map(value => value.trim()).filter(Boolean);
    };

    try {
        const params = new URLSearchParams(window.location.search);
        const queryValue = params.get('preview_extensions');
        if (queryValue !== null) {
            const ids = parseRawValue(queryValue);
            window.localStorage.setItem(PREVIEW_EXTENSIONS_STORAGE_KEY, ids.join(','));
            return new Set(ids);
        }
    } catch (err) {
        // Ignore URL parsing errors and fall back to storage/default.
    }

    try {
        const storedValue = window.localStorage.getItem(PREVIEW_EXTENSIONS_STORAGE_KEY) || '';
        return new Set(parseRawValue(storedValue));
    } catch (err) {
        return new Set();
    }
}

function mergeExtensionTypesWithPreview(backendExtensionTypes) {
    const previewIds = resolvePreviewExtensionIds();
    if (previewIds.size === 0) {
        return backendExtensionTypes || [];
    }

    const merged = [...(backendExtensionTypes || [])];
    const existingIds = new Set(merged.map(ext => ext.extension_type));

    PREVIEW_EXTENSION_CATALOG
        .filter(ext => previewIds.has(ext.extension_type))
        .forEach(ext => {
            if (!existingIds.has(ext.extension_type)) {
                merged.push(ext);
            }
        });

    return merged;
}

function disablePreviewExtensions(projectName) {
    try {
        window.localStorage.removeItem(PREVIEW_EXTENSIONS_STORAGE_KEY);
    } catch (err) {
        // Ignore storage errors.
    }

    navigate(`/project/${projectName}/extensions`);
}


// Helper function to normalize JSON for comparison (sorts keys recursively)
function normalizeJSON(jsonString) {
    try {
        const obj = JSON.parse(jsonString);
        return JSON.stringify(sortObjectKeys(obj), null, 2);
    } catch (e) {
        return jsonString;
    }
}

// Recursively sort object keys for consistent comparison
function sortObjectKeys(obj) {
    if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) {
        return obj;
    }
    return Object.keys(obj)
        .sort()
        .reduce((sorted, key) => {
            sorted[key] = sortObjectKeys(obj[key]);
            return sorted;
        }, {});
}

// Deployment statuses considered terminal — those deployments are no longer
// running or relevant to the environment's current state, so they're excluded
// from the environment's active-deployments view.
const TERMINAL_DEPLOYMENT_STATUSES = new Set([
    'Cancelled', 'Stopped', 'Superseded', 'Failed', 'Expired',
]);

function EnvironmentCard({ env, projectName, deployments, onEdit, onDelete }) {
    // Only the production environment card starts expanded; other envs
    // collapse by default to keep the page compact on first load.
    const [open, setOpen] = useState(!!env.is_production);

    const activeDeployments = deployments.filter(d => !TERMINAL_DEPLOYMENT_STATUSES.has(d.status));

    const primaryGroup = env.primary_deployment_group || 'default';
    const isPrimary = (d) =>
        d.is_active && (d.deployment_group || 'default') === primaryGroup;

    // The deployment serving traffic on the env's primary URL — drives the
    // status/replicas summary shown in the collapsed header.
    const primaryDeploy = activeDeployments.find(isPrimary);
    const headerStatus = primaryDeploy?.status || (activeDeployments[0]?.status) || null;
    const headerReplicas = primaryDeploy?.replicas;

    return (
        <RPanel>
            <RGroupBar
                open={open}
                onToggle={() => setOpen(o => !o)}
                right={
                    <>
                        {headerStatus && <RStatus status={headerStatus} />}
                        {headerReplicas != null && (
                            <span style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>
                                {headerReplicas} replica{headerReplicas === 1 ? '' : 's'}
                            </span>
                        )}
                        <RButton
                            size="sm"
                            icon="edit"
                            onClick={(e) => { e.stopPropagation(); onEdit(env); }}
                        >
                            Edit
                        </RButton>
                        <RButton
                            size="sm"
                            variant="danger"
                            icon="trash"
                            disabled={env.is_production}
                            onClick={(e) => { e.stopPropagation(); onDelete(env); }}
                        >
                            Delete
                        </RButton>
                    </>
                }
            >
                <EnvironmentColorDot color={env.color} size="0.7rem" />
                <a
                    className="r-link"
                    style={{ fontWeight: 600, fontSize: 14, textTransform: 'capitalize' }}
                    onClick={(e) => {
                        e.stopPropagation();
                        navigate(`/project/${projectName}/environment/${env.name}`);
                    }}
                    title="View the active deployment of this environment"
                >
                    {env.name}
                </a>
                {env.is_production && env.name.toLowerCase() !== 'production' && (
                    <span className="r-pill env-prod">production</span>
                )}
                <span style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>
                    · {activeDeployments.length} active deployment{activeDeployments.length === 1 ? '' : 's'}
                </span>
            </RGroupBar>
            {open && (
                activeDeployments.length === 0 ? (
                    <div style={{ padding: 28, textAlign: 'center', color: 'var(--text-soft)', fontSize: 13 }}>
                        No active deployments in this environment.
                    </div>
                ) : (
                    <table className="r-table r-table-fixed">
                        <colgroup>
                            <col style={{ width: 230 }} />
                            <col style={{ width: 130 }} />
                            <col style={{ width: 180 }} />
                            <col />
                            <col style={{ width: 110 }} />
                        </colgroup>
                        <thead>
                            <tr>
                                <th>ID</th>
                                <th>Status</th>
                                <th>Group</th>
                                <th>Author</th>
                                <th style={{ textAlign: 'right' }}>Age</th>
                            </tr>
                        </thead>
                        <tbody>
                            {activeDeployments.map(d => {
                                const group = d.deployment_group || 'default';
                                const groupIsPrimary = group === primaryGroup;
                                return (
                                    <tr
                                        key={d.deployment_id}
                                        className="click"
                                        onClick={() => navigate(`/deployment/${projectName}/${d.deployment_id}`)}
                                    >
                                        <td className="mono" style={{ fontSize: 12.25 }}>
                                            {d.deployment_id}
                                            {isPrimary(d) && (
                                                <span style={{
                                                    marginLeft: 8, fontSize: 10.5, padding: '1px 6px',
                                                    background: 'var(--accent-soft)', color: 'var(--accent)',
                                                    borderRadius: 4, fontFamily: 'Inter',
                                                }} title="Currently serving this environment's primary URL">PRIMARY</span>
                                            )}
                                        </td>
                                        <td><RStatus status={d.status || 'Unknown'} /></td>
                                        <td>
                                            <RGroupPill group={group} />
                                            {groupIsPrimary && (
                                                <span style={{ marginLeft: 6, fontSize: 10.5, color: 'var(--text-soft)' }} title="Primary group for this environment">primary</span>
                                            )}
                                        </td>
                                        <td>{d.created_by_email || <span style={{ color: 'var(--text-soft)' }}>—</span>}</td>
                                        <td style={{ textAlign: 'right', color: 'var(--text-muted)' }}>
                                            {d.created ? formatRelativeTimeRounded(d.created) : '—'}
                                        </td>
                                    </tr>
                                );
                            })}
                        </tbody>
                    </table>
                )
            )}
        </RPanel>
    );
}

// Environments Component
export function EnvironmentsList({ projectName, platformConstraints = null }) {
    const [environments, setEnvironments] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [editingEnv, setEditingEnv] = useState(null);
    const [formData, setFormData] = useState({
        name: '', primary_deployment_group: '', is_production: false, color: 'green',
        min_replicas: '', max_replicas: '', min_cpu: '', max_cpu: '', min_memory: '', max_memory: '',
    });
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [envToDelete, setEnvToDelete] = useState(null);
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [currentUser, setCurrentUser] = useState(null);
    const [deployments, setDeployments] = useState([]);
    const { showToast } = useToast();

    const loadEnvironments = useCallback(async () => {
        try {
            const [envData, deployData] = await Promise.all([
                api.getProjectEnvironments(projectName),
                api.getProjectDeployments(projectName, { limit: 100 }).catch(() => []),
            ]);
            setEnvironments(envData || []);
            setDeployments(deployData || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadEnvironments();
    }, [loadEnvironments]);

    useEffect(() => {
        api.getMe().then(setCurrentUser).catch(() => {});
    }, []);

    const isAdmin = currentUser?.is_admin ?? false;

    const handleAddClick = () => {
        setEditingEnv(null);
        setFormData({
            name: '', primary_deployment_group: '', is_production: false, color: 'green',
            min_replicas: '', max_replicas: '', min_cpu: '', max_cpu: '', min_memory: '', max_memory: '',
        });
        setIsModalOpen(true);
    };

    const handleEditClick = (env) => {
        setEditingEnv(env);
        const c = env.deployment_constraints;
        setFormData({
            name: env.name,
            primary_deployment_group: env.primary_deployment_group || '',
            is_production: env.is_production,
            color: env.color || 'green',
            min_replicas: c?.min_replicas?.toString() ?? '',
            max_replicas: c?.max_replicas?.toString() ?? '',
            min_cpu: c?.min_cpu ?? '',
            max_cpu: c?.max_cpu ?? '',
            min_memory: c?.min_memory ?? '',
            max_memory: c?.max_memory ?? '',
        });
        setIsModalOpen(true);
    };

    const handleDeleteClick = (env) => {
        setEnvToDelete(env);
        setConfirmDialogOpen(true);
    };

    const handleSave = async () => {
        if (!formData.name) {
            showToast('Environment name is required', 'error');
            return;
        }

        setSaving(true);
        try {
            if (editingEnv) {
                const updates: any = {
                    primary_deployment_group: formData.primary_deployment_group || null,
                    is_production: formData.is_production,
                    color: formData.color,
                };
                // Include deployment constraints if admin
                if (isAdmin) {
                    const hasAnyConstraint = formData.min_replicas || formData.max_replicas ||
                        formData.min_cpu || formData.max_cpu || formData.min_memory || formData.max_memory;
                    updates.deployment_constraints = hasAnyConstraint ? {
                        min_replicas: formData.min_replicas ? parseInt(formData.min_replicas) : null,
                        max_replicas: formData.max_replicas ? parseInt(formData.max_replicas) : null,
                        min_cpu: formData.min_cpu || null,
                        max_cpu: formData.max_cpu || null,
                        min_memory: formData.min_memory || null,
                        max_memory: formData.max_memory || null,
                    } : {
                        min_replicas: null, max_replicas: null,
                        min_cpu: null, max_cpu: null, min_memory: null, max_memory: null,
                    };
                }
                await api.updateEnvironment(projectName, editingEnv.name, updates);
                showToast(`Environment ${editingEnv.name} updated`, 'success');
            } else {
                await api.createEnvironment(projectName, {
                    name: formData.name,
                    primary_deployment_group: formData.primary_deployment_group || null,
                    is_production: formData.is_production,
                    color: formData.color,
                });
                showToast(`Environment ${formData.name} created`, 'success');
            }
            setIsModalOpen(false);
            loadEnvironments();
        } catch (err) {
            showToast(`Failed to ${editingEnv ? 'update' : 'create'} environment: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDeleteConfirm = async () => {
        if (!envToDelete) return;

        setDeleting(true);
        try {
            await api.deleteEnvironment(projectName, envToDelete.name);
            showToast(`Environment ${envToDelete.name} deleted`, 'success');
            setConfirmDialogOpen(false);
            setEnvToDelete(null);
            loadEnvironments();
        } catch (err) {
            showToast(`Failed to delete environment: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    if (loading) return <LoadingState label="Loading environments…" />;
    if (error) return <ErrorState message={`Error loading environments: ${error}`} onRetry={loadEnvironments} />;

    const deploymentsForEnv = (envName) => deployments.filter(d => d.environment === envName);

    return (
        <div>
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Environments</div>
                    <div className="r-section-sub">Deployment targets and their deployment groups for this project.</div>
                </div>
                <RButton variant="primary" icon="plus" onClick={handleAddClick}>
                    New environment
                </RButton>
            </div>

            {environments.length === 0 ? (
                <RPanel>
                    <REmpty title="No environments">
                        <div style={{ color: 'var(--text-soft)', fontSize: 13 }}>
                            No environments configured. Create one to define a deployment target.
                        </div>
                    </REmpty>
                </RPanel>
            ) : (
                <div className="r-stack">
                    {environments.map(env => (
                        <EnvironmentCard
                            key={env.name}
                            env={env}
                            projectName={projectName}
                            deployments={deploymentsForEnv(env.name)}
                            onEdit={handleEditClick}
                            onDelete={handleDeleteClick}
                        />
                    ))}
                </div>
            )}

            <RModal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingEnv ? 'Edit Environment' : 'Create Environment'}
                footer={
                    <>
                        <RButton onClick={() => setIsModalOpen(false)} disabled={saving}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleSave} loading={saving}>
                            {editingEnv ? 'Update' : 'Create'}
                        </RButton>
                    </>
                }
            >
                <RField label={<>Name<span className="text-red-300 ml-1">*</span></>}>
                    <RInput
                        value={formData.name}
                        onChange={(e) => setFormData({ ...formData, name: e.target.value.toLowerCase() })}
                        placeholder="staging"
                        disabled={editingEnv !== null}
                    />
                </RField>
                <RField
                    label="Primary Deployment Group"
                    hint="The deployment group that maps to this environment. Leave empty if not applicable."
                >
                    <RInput
                        value={formData.primary_deployment_group}
                        onChange={(e) => setFormData({ ...formData, primary_deployment_group: e.target.value })}
                        placeholder="default"
                    />
                </RField>
                <div className="flex gap-6">
                    <label className="flex items-center gap-2 cursor-pointer">
                        <input
                            type="checkbox"
                            checked={formData.is_production}
                            onChange={(e) => setFormData({ ...formData, is_production: e.target.checked })}
                            className="w-4 h-4 text-indigo-600 border-gray-300 rounded focus:ring-indigo-500"
                        />
                        <span className="text-sm text-gray-700 dark:text-gray-300">Production</span>
                    </label>
                </div>
                <RField label="Color">
                    <EnvironmentColorPicker
                        value={formData.color}
                        onChange={(c) => setFormData({ ...formData, color: c })}
                    />
                </RField>

                {isAdmin && editingEnv && (
                    <div className="pt-4 border-t border-gray-200 dark:border-gray-700">
                        <span className="mono-label">Resource Constraints</span>
                        <p className="text-xs text-gray-500 dark:text-gray-400 mb-3">
                            Bounds for what a deployment can <em>request</em> at deploy time (via <span className="mono">rise.toml</span> or the CLI). These are not Kubernetes requests/limits — they constrain the user's choice. Leave empty to use platform defaults.
                        </p>
                        <div className="grid grid-cols-2 gap-3">
                            <RField label="Min Replicas">
                                <RInput
                                    type="number"
                                    value={formData.min_replicas}
                                    onChange={(e) => setFormData({ ...formData, min_replicas: e.target.value })}
                                    placeholder={platformConstraints?.min_replicas?.toString() ?? '1'}
                                />
                            </RField>
                            <RField label="Max Replicas">
                                <RInput
                                    type="number"
                                    value={formData.max_replicas}
                                    onChange={(e) => setFormData({ ...formData, max_replicas: e.target.value })}
                                    placeholder={platformConstraints?.max_replicas?.toString() ?? '1'}
                                />
                            </RField>
                            <RField label="Min CPU">
                                <RInput
                                    value={formData.min_cpu}
                                    onChange={(e) => setFormData({ ...formData, min_cpu: e.target.value })}
                                    placeholder={platformConstraints?.min_cpu ?? '100m'}
                                />
                            </RField>
                            <RField label="Max CPU">
                                <RInput
                                    value={formData.max_cpu}
                                    onChange={(e) => setFormData({ ...formData, max_cpu: e.target.value })}
                                    placeholder={platformConstraints?.max_cpu ?? '2'}
                                />
                            </RField>
                            <RField label="Min Memory">
                                <RInput
                                    value={formData.min_memory}
                                    onChange={(e) => setFormData({ ...formData, min_memory: e.target.value })}
                                    placeholder={platformConstraints?.min_memory ?? '64Mi'}
                                />
                            </RField>
                            <RField label="Max Memory">
                                <RInput
                                    value={formData.max_memory}
                                    onChange={(e) => setFormData({ ...formData, max_memory: e.target.value })}
                                    placeholder={platformConstraints?.max_memory ?? '2Gi'}
                                />
                            </RField>
                        </div>
                    </div>
                )}
            </RModal>

            <RConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setEnvToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Environment"
                message={`Are you sure you want to delete the environment "${envToDelete?.name}"? This action cannot be undone.`}
                confirmText="Delete Environment"
                confirmTone="danger"
                loading={deleting}
            />
        </div>
    );
}

// Custom Domains Component
export function DomainsList({ projectName, defaultUrl = null }) {
    const [domains, setDomains] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [formData, setFormData] = useState({ domain: '' });
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [domainToDelete, setDomainToDelete] = useState(null);
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [updatingPrimaryDomain, setUpdatingPrimaryDomain] = useState(null);
    const { showToast } = useToast();

    const hasStarredCustomDomain = domains.some(d => d.is_primary);
    const isDefaultPrimary = !hasStarredCustomDomain;

    const loadDomains = useCallback(async () => {
        try {
            const response = await api.getProjectDomains(projectName);
            setDomains(response.domains || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadDomains();
    }, [loadDomains]);

    const handleAddClick = () => {
        setFormData({ domain: '' });
        setIsModalOpen(true);
    };

    const handleDeleteClick = (domain) => {
        setDomainToDelete(domain);
        setConfirmDialogOpen(true);
    };

    const handleSave = async () => {
        if (!formData.domain) {
            showToast('Domain is required', 'error');
            return;
        }

        setSaving(true);
        try {
            await api.addCustomDomain(projectName, formData.domain);
            showToast(`Custom domain ${formData.domain} added successfully`, 'success');
            setIsModalOpen(false);
            loadDomains();
        } catch (err) {
            showToast(`Failed to add custom domain: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDeleteConfirm = async () => {
        if (!domainToDelete) return;

        setDeleting(true);
        try {
            await api.deleteCustomDomain(projectName, domainToDelete.domain);
            showToast(`Custom domain ${domainToDelete.domain} deleted successfully`, 'success');
            setConfirmDialogOpen(false);
            setDomainToDelete(null);
            loadDomains();
        } catch (err) {
            showToast(`Failed to delete custom domain: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    const handleTogglePrimary = async (domain) => {
        setUpdatingPrimaryDomain(domain.domain);
        try {
            if (domain.is_primary) {
                // Unset primary
                await api.unsetCustomDomainPrimary(projectName, domain.domain);
                showToast(`Removed ${domain.domain} as primary domain`, 'success');
            } else {
                // Set primary
                await api.setCustomDomainPrimary(projectName, domain.domain);
                showToast(`Set ${domain.domain} as primary domain`, 'success');
            }
            loadDomains();
        } catch (err) {
            showToast(`Failed to update primary domain: ${err.message}`, 'error');
        } finally {
            setUpdatingPrimaryDomain(null);
        }
    };

    // Starring the default URL row = unstarring the currently starred custom domain
    const handleStarDefault = async () => {
        const starredDomain = domains.find(d => d.is_primary);
        if (!starredDomain) return; // Already default primary
        setUpdatingPrimaryDomain('__default__');
        try {
            await api.unsetCustomDomainPrimary(projectName, starredDomain.domain);
            showToast('Default URL set as primary domain', 'success');
            loadDomains();
        } catch (err) {
            showToast(`Failed to update primary domain: ${err.message}`, 'error');
        } finally {
            setUpdatingPrimaryDomain(null);
        }
    };

    if (loading) return <LoadingState label="Loading custom domains…" />;
    if (error) return <ErrorState message={`Error loading custom domains: ${error}`} onRetry={loadDomains} />;

    const defaultHost = (() => {
        if (!defaultUrl) return null;
        try { return new URL(defaultUrl).hostname; } catch { return defaultUrl; }
    })();

    return (
        <div>
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Custom domains</div>
                    <div className="r-section-sub">Point your own domains at this project. The primary domain is used as the default URL.</div>
                </div>
                <RButton variant="primary" icon="plus" onClick={handleAddClick}>
                    Add domain
                </RButton>
            </div>

            <RPanel>
                {!defaultUrl && domains.length === 0 ? (
                    <REmpty title="No custom domains">
                        <div style={{ color: 'var(--text-soft)', fontSize: 13 }}>
                            No custom domains configured. Add one to serve this project from your own domain.
                        </div>
                    </REmpty>
                ) : (
                    <table className="r-table">
                        <thead>
                            <tr>
                                <th>Domain</th>
                                <th>Created</th>
                                <th style={{ textAlign: 'right' }}>Actions</th>
                            </tr>
                        </thead>
                        <tbody>
                            {defaultUrl && (
                                <tr>
                                    <td>
                                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                                            <Icon name="globe" size={14} />
                                            <a className="r-link mono" style={{ fontSize: 13 }} href={defaultUrl} target="_blank" rel="noopener noreferrer">
                                                {defaultHost}
                                            </a>
                                            <RPill>default</RPill>
                                            {isDefaultPrimary && <RPill kind="accent">primary</RPill>}
                                        </span>
                                    </td>
                                    <td style={{ color: 'var(--text-soft)' }}>—</td>
                                    <td>
                                        <div className="row-actions" style={{ display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
                                            <RButton
                                                size="sm"
                                                icon="check"
                                                onClick={handleStarDefault}
                                                loading={updatingPrimaryDomain === '__default__'}
                                                disabled={isDefaultPrimary}
                                            >
                                                {isDefaultPrimary ? 'Primary' : 'Set primary'}
                                            </RButton>
                                        </div>
                                    </td>
                                </tr>
                            )}
                            {domains.map(domain => (
                                <tr key={domain.id}>
                                    <td>
                                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                                            <Icon name="globe" size={14} />
                                            <span className="mono" style={{ fontSize: 13 }}>{domain.domain}</span>
                                            {domain.is_primary && <RPill kind="accent">primary</RPill>}
                                        </span>
                                    </td>
                                    <td style={{ color: 'var(--text-muted)' }}>{formatDate(domain.created_at)}</td>
                                    <td>
                                        <div className="row-actions" style={{ display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
                                            <RButton
                                                size="sm"
                                                icon="check"
                                                onClick={() => handleTogglePrimary(domain)}
                                                loading={updatingPrimaryDomain === domain.domain}
                                            >
                                                {domain.is_primary ? 'Unset primary' : 'Set primary'}
                                            </RButton>
                                            <RButton size="sm" variant="danger" icon="trash" onClick={() => handleDeleteClick(domain)}>
                                                Delete
                                            </RButton>
                                        </div>
                                    </td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                )}
            </RPanel>

            <RModal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title="Add Custom Domain"
                footer={
                    <>
                        <RButton onClick={() => setIsModalOpen(false)} disabled={saving}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleSave} loading={saving}>
                            Add Domain
                        </RButton>
                    </>
                }
            >
                <RField label={<>Domain<span className="text-red-300 ml-1">*</span></>}>
                    <RInput
                        value={formData.domain}
                        onChange={(e) => setFormData({ ...formData, domain: e.target.value })}
                        placeholder="example.com"
                    />
                </RField>
                <p className="text-sm text-gray-600 dark:text-gray-500">
                    <strong>Note:</strong> Make sure to configure your DNS to point this domain to your Rise deployment before adding it.
                    The domain will be added to the ingress for the default deployment group only.
                </p>
            </RModal>

            <RConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setDomainToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Custom Domain"
                message={`Are you sure you want to delete the custom domain "${domainToDelete?.domain}"? This action cannot be undone.`}
                confirmText="Delete Domain"
                confirmTone="danger"
                loading={deleting}
            />
        </div>
    );
}

const ENV_VAR_SOURCE_COLORS = {
    system: 'gray',
    global: 'green',
    extension: 'purple',
    toml: 'yellow',
    cli: 'blue',
};

function EnvVarSourceTag({ source, environments = [] }) {
    if (!source) return null;
    const envPrefix = 'env:';
    if (source.startsWith(envPrefix)) {
        const envName = source.slice(envPrefix.length);
        const envColor = environments.find(e => e.name === envName)?.color;
        return (
            <span className="r-pill">
                <EnvironmentColorDot color={envColor} size="0.5rem" />
                {envName}
            </span>
        );
    }
    return <span className="r-pill">{source}</span>;
}

// One collapsible scope panel (Global, or a single environment) of env vars.
function EnvVarScopeGroup({ scope, vars, searching, defaultOpen, environments, renderTypeTag, renderValueCell, onAdd, onEdit, onDelete }) {
    const [open, setOpen] = useState(defaultOpen);
    const secretCount = vars.filter(v => v.is_secret).length;
    // When searching, collapse empty scopes entirely.
    if (searching && vars.length === 0) return null;

    return (
        <RPanel>
            <RGroupBar
                open={open}
                onToggle={() => setOpen(o => !o)}
                right={
                    <>
                        <span style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>
                            {vars.length} variable{vars.length !== 1 ? 's' : ''}
                        </span>
                        {secretCount > 0 && (
                            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5, fontSize: 11.5, color: 'var(--text-soft)' }}>
                                <Icon name="lock" size={11} /> {secretCount} secret{secretCount !== 1 ? 's' : ''}
                            </span>
                        )}
                        <RButton size="sm" icon="plus" onClick={(e) => { e.stopPropagation(); onAdd(); }}>
                            Add
                        </RButton>
                    </>
                }
            >
                <span className={`r-pill ${scope.kind}`}>
                    {scope.color && <EnvironmentColorDot color={scope.color} />}
                    {scope.label}
                </span>
                <span style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>· {scope.hint}</span>
            </RGroupBar>
            {open && (
                vars.length === 0 ? (
                    <div style={{ padding: 28, textAlign: 'center', color: 'var(--text-soft)', fontSize: 13 }}>
                        No variables in this scope.
                    </div>
                ) : (
                    <table className="r-table r-table-fixed">
                        <colgroup>
                            <col style={{ width: '28%' }} />
                            <col />
                            <col style={{ width: 110 }} />
                            <col style={{ width: 130 }} />
                        </colgroup>
                        <thead>
                            <tr><th>Key</th><th>Value</th><th>Type</th><th></th></tr>
                        </thead>
                        <tbody>
                            {vars.map(v => (
                                <tr key={`${v.key}-${v.environment || ''}`}>
                                    <td className="mono" style={{ fontSize: 13, fontWeight: 500 }}>{v.key}</td>
                                    <td>{renderValueCell(v)}</td>
                                    <td>{renderTypeTag(v)}</td>
                                    <td>
                                        <div className="row-actions" style={{ display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
                                            <RButton size="sm" icon="edit" onClick={() => onEdit(v)}>Edit</RButton>
                                            <RButton size="sm" variant="danger" icon="trash" onClick={() => onDelete(v)}>Delete</RButton>
                                        </div>
                                    </td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                )
            )}
        </RPanel>
    );
}

// Environment Variables Component
export function EnvVarsList({ projectName, deploymentId }) {
    const [envVars, setEnvVars] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [editingEnvVar, setEditingEnvVar] = useState(null);
    const [formData, setFormData] = useState({ key: '', value: '', type: 'plain', environment: null });
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [envVarToDelete, setEnvVarToDelete] = useState(null);
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [environments, setEnvironments] = useState([]);
    const [search, setSearch] = useState('');
    const [revealed, setRevealed] = useState({});
    const importInputRef = useRef(null);
    const { showToast } = useToast();

    // Convert API representation to UI type
    const apiToType = (isSecret, isProtected) => {
        if (!isSecret) return 'plain';
        return isProtected ? 'protected' : 'secret';
    };

    // Convert UI type to API representation
    const typeToApi = (type) => ({
        is_secret: type !== 'plain',
        is_protected: type === 'protected',
    });

    // Load environments for source color dots
    useEffect(() => {
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
    }, [projectName]);

    const loadEnvVars = useCallback(async () => {
        try {
            const response = deploymentId
                ? await api.getDeploymentEnvVars(projectName, deploymentId)
                : await api.getProjectEnvVars(projectName, null);
            setEnvVars(response.env_vars || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName, deploymentId]);

    useEffect(() => {
        loadEnvVars();
    }, [loadEnvVars]);

    const handleAddClick = (environment = null) => {
        setEditingEnvVar(null);
        setFormData({ key: '', value: '', type: 'plain', environment: environment || null });
        setIsModalOpen(true);
    };

    const handleEditClick = (envVar) => {
        setEditingEnvVar(envVar);
        setFormData({
            key: envVar.key,
            value: envVar.is_secret ? '' : envVar.value,
            type: apiToType(envVar.is_secret, envVar.is_protected),
            environment: envVar.environment || null,
        });
        setIsModalOpen(true);
    };

    const handleDeleteClick = (envVar) => {
        setEnvVarToDelete(envVar);
        setConfirmDialogOpen(true);
    };

    const handleSave = async () => {
        if (!formData.key || (!editingEnvVar && !formData.value)) {
            showToast('Key and value are required', 'error');
            return;
        }

        setSaving(true);
        try {
            const originalEnv = editingEnvVar?.environment || null;
            const targetEnv = formData.environment || null;
            const envChanged = editingEnvVar && originalEnv !== targetEnv;

            // Move to different environment if needed
            if (envChanged) {
                await api.moveEnvVar(projectName, formData.key, originalEnv, targetEnv);
            }

            // Update value/type (skip for secrets with no new value provided)
            if (formData.value) {
                const { is_secret, is_protected } = typeToApi(formData.type);
                await api.setEnvVar(projectName, formData.key, formData.value, is_secret, is_protected, targetEnv);
            }

            showToast(`Environment variable ${formData.key} ${editingEnvVar ? 'updated' : 'created'} successfully`, 'success');
            setIsModalOpen(false);
            loadEnvVars();
        } catch (err) {
            showToast(`Failed to save environment variable: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDeleteConfirm = async () => {
        if (!envVarToDelete) return;

        setDeleting(true);
        try {
            await api.deleteEnvVar(projectName, envVarToDelete.key, envVarToDelete.environment || null);
            showToast(`Environment variable ${envVarToDelete.key} deleted successfully`, 'success');
            setConfirmDialogOpen(false);
            setEnvVarToDelete(null);
            loadEnvVars();
        } catch (err) {
            showToast(`Failed to delete environment variable: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    // Export currently-loaded variables as a .env file. Secret values are not
    // available client-side, so secret keys are written without a value.
    const handleExport = () => {
        const lines = envVars.map(v => {
            if (v.is_secret) return `# ${v.key} is a secret; value not exported\n${v.key}=`;
            return `${v.key}=${v.value ?? ''}`;
        });
        const blob = new Blob([lines.join('\n') + '\n'], { type: 'text/plain' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `${projectName}.env`;
        a.click();
        URL.revokeObjectURL(url);
    };

    // Parse an uploaded .env file and create each variable as a plain global var.
    const handleImportFile = async (e) => {
        const file = e.target.files?.[0];
        e.target.value = '';
        if (!file) return;
        let text = '';
        try {
            text = await file.text();
        } catch (err) {
            showToast(`Failed to read file: ${err.message}`, 'error');
            return;
        }
        const entries = [];
        for (const raw of text.split('\n')) {
            const line = raw.trim();
            if (!line || line.startsWith('#')) continue;
            const eq = line.indexOf('=');
            if (eq <= 0) continue;
            let key = line.slice(0, eq).trim();
            if (key.startsWith('export ')) key = key.slice(7).trim();
            let value = line.slice(eq + 1).trim();
            if (value.length >= 2 && ((value[0] === '"' && value.endsWith('"')) || (value[0] === "'" && value.endsWith("'")))) {
                value = value.slice(1, -1);
            }
            if (key) entries.push([key, value]);
        }
        if (entries.length === 0) {
            showToast('No variables found in file', 'error');
            return;
        }
        let ok = 0;
        for (const [key, value] of entries) {
            try {
                await api.setEnvVar(projectName, key, value, false, false, null);
                ok++;
            } catch (err) {
                showToast(`Failed to import ${key}: ${err.message}`, 'error');
            }
        }
        if (ok > 0) showToast(`Imported ${ok} variable${ok === 1 ? '' : 's'} into Global`, 'success');
        loadEnvVars();
    };

    if (loading) return <LoadingState label="Loading environment variables…" />;
    if (error) return <ErrorState message={`Error loading environment variables: ${error}`} onRetry={loadEnvVars} />;

    const revealSecret = async (envVar) => {
        const revealKey = `${envVar.environment || ''}:${envVar.key}`;
        if (revealed[revealKey] !== undefined) {
            setRevealed(r => { const next = { ...r }; delete next[revealKey]; return next; });
            return;
        }
        try {
            const response = deploymentId
                ? await api.getDeploymentEnvVarValue(projectName, deploymentId, envVar.key)
                : await api.getEnvVarValue(projectName, envVar.key, envVar.environment || null);
            setRevealed(r => ({ ...r, [revealKey]: response.value }));
        } catch (err) {
            showToast(`Failed to reveal secret: ${err.message}`, 'error');
            console.error('Failed to fetch secret:', err);
        }
    };

    const TypeTag = ({ envVar }) => {
        if (envVar.is_protected) {
            return <span className="r-pill" style={{ color: 'var(--info)', background: 'var(--info-bg)', borderColor: 'transparent' }}>protected</span>;
        }
        if (envVar.is_secret) {
            return <span className="r-pill" style={{ color: 'var(--warn)', background: 'var(--warn-bg)', borderColor: 'transparent' }}>secret</span>;
        }
        return <span className="r-pill">plain</span>;
    };

    // Renders the masked/revealed value cell with a reveal eye for readable secrets.
    const ValueCell = ({ envVar }) => {
        const revealKey = `${envVar.environment || ''}:${envVar.key}`;
        const isRevealed = revealed[revealKey] !== undefined;
        const canReveal = envVar.is_secret && !envVar.is_protected;
        return (
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, overflow: 'hidden' }}>
                <span
                    className="mono"
                    style={{
                        fontSize: 12.5,
                        color: envVar.is_secret && !isRevealed ? 'var(--text-soft)' : 'var(--text)',
                        overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1, minWidth: 0,
                    }}
                >
                    {envVar.is_secret && !isRevealed ? '••••••••••••' : (isRevealed ? revealed[revealKey] : envVar.value)}
                </span>
                {canReveal && (
                    <RButton
                        variant="ghost"
                        size="sm"
                        icon={isRevealed ? 'eyeoff' : 'eye'}
                        onClick={() => revealSecret(envVar)}
                        title={isRevealed ? 'Hide' : 'Reveal'}
                        style={{ flex: '0 0 auto', padding: '2px 6px' }}
                    />
                )}
            </div>
        );
    };

    // Deployment snapshot mode: a single read-only table.
    if (deploymentId) {
        return (
            <div>
                <RPanel>
                    {envVars.length === 0 ? (
                        <REmpty title="No environment variables">
                            <div style={{ color: 'var(--text-soft)', fontSize: 13 }}>
                                This deployment has no environment variables.
                            </div>
                        </REmpty>
                    ) : (
                        <table className="r-table r-table-fixed">
                            <colgroup>
                                <col style={{ width: '28%' }} />
                                <col />
                                <col style={{ width: 110 }} />
                                <col style={{ width: 150 }} />
                            </colgroup>
                            <thead>
                                <tr><th>Key</th><th>Value</th><th>Type</th><th>Source</th></tr>
                            </thead>
                            <tbody>
                                {envVars.map(env => (
                                    <tr key={`${env.key}-${env.environment || ''}`}>
                                        <td className="mono" style={{ fontSize: 13, fontWeight: 500 }}>{env.key}</td>
                                        <td><ValueCell envVar={env} /></td>
                                        <td><TypeTag envVar={env} /></td>
                                        <td><EnvVarSourceTag source={env.source} environments={environments} /></td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    )}
                </RPanel>
                <p style={{ marginTop: 16, fontSize: 12.5, color: 'var(--text-muted)' }}>
                    Environment variables are read-only snapshots taken at deployment time.
                    Secret values are always masked unless revealed.
                </p>
            </div>
        );
    }

    // Project mode: group all vars by scope (Global + one per environment).
    const search_q = search.trim().toLowerCase();
    const matchesSearch = (v) =>
        !search_q || v.key.toLowerCase().includes(search_q) || (v.value || '').toLowerCase().includes(search_q);

    const envOrder = environments.map(e => e.name);
    const scopeNames = Array.from(new Set(envVars.map(v => v.environment).filter(Boolean)));
    // Keep declared environment order first, then any extra scopes present in the data.
    const orderedEnvScopes = [
        ...envOrder.filter(n => scopeNames.includes(n)),
        ...scopeNames.filter(n => !envOrder.includes(n)),
    ];

    const scopes = [
        { id: null, label: 'Global', hint: 'applies to every environment', kind: 'env-global' },
        ...orderedEnvScopes.map(name => ({
            id: name,
            label: name,
            hint: `overrides global for ${name}`,
            kind: 'env-prod',
            color: environments.find(e => e.name === name)?.color,
        })),
    ];

    return (
        <div>
            <div className="r-section-head">
                <RSearchInput
                    value={search}
                    onChange={setSearch}
                    placeholder="Filter keys or values…"
                    style={{ maxWidth: 320 }}
                />
                <div style={{ display: 'flex', gap: 8 }}>
                    <input
                        ref={importInputRef}
                        type="file"
                        accept=".env,text/plain"
                        style={{ display: 'none' }}
                        onChange={handleImportFile}
                    />
                    <RButton icon="copy" onClick={() => importInputRef.current?.click()}>
                        Import .env
                    </RButton>
                    <RButton icon="ext" onClick={handleExport}>
                        Export
                    </RButton>
                    <RButton variant="primary" icon="plus" onClick={() => handleAddClick()}>
                        Add variable
                    </RButton>
                </div>
            </div>

            <RAlert icon="lock">
                Global variables apply to all environments. A variable redefined in a specific environment
                <strong> overrides</strong> the global one for that env. Secrets are encrypted at rest.
            </RAlert>

            <div className="r-stack tight" style={{ marginTop: 16 }}>
                {scopes.map((scope, i) => (
                    <EnvVarScopeGroup
                        key={scope.id || '__global__'}
                        scope={scope}
                        vars={envVars.filter(v => (v.environment || null) === scope.id && matchesSearch(v))}
                        searching={!!search_q}
                        defaultOpen={i < 2}
                        environments={environments}
                        renderTypeTag={(v) => <TypeTag envVar={v} />}
                        renderValueCell={(v) => <ValueCell envVar={v} />}
                        onAdd={() => handleAddClick(scope.id)}
                        onEdit={handleEditClick}
                        onDelete={handleDeleteClick}
                    />
                ))}
            </div>
            <p style={{ marginTop: 16, fontSize: 12.5, color: 'var(--text-muted)' }}>
                Environment variables are snapshots at deployment time. Changes to project variables only
                apply to new deployments, not existing ones. Secret values are always masked unless revealed.
            </p>

            <RModal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingEnvVar ? 'Edit environment variable' : 'Add environment variable'}
                footer={
                    <>
                        <RButton onClick={() => setIsModalOpen(false)} disabled={saving}>Cancel</RButton>
                        <RButton variant="primary" onClick={handleSave} loading={saving}>
                            {editingEnvVar ? 'Update' : 'Add'}
                        </RButton>
                    </>
                }
            >
                {!deploymentId && environments.length > 0 && (
                    <RField label="Environment">
                        <RCombobox
                            value={formData.environment || ''}
                            onChange={(v) => setFormData({ ...formData, environment: v || null })}
                            options={[
                                { value: '', label: 'Global (all environments)' },
                                ...environments.map(env => ({ value: env.name, label: env.name })),
                            ]}
                            placeholder="Global (all environments)"
                        />
                    </RField>
                )}
                <RField label="Key">
                    <RInput
                        value={formData.key}
                        onChange={(e) => setFormData({ ...formData, key: e.target.value })}
                        placeholder="DATABASE_URL"
                        disabled={editingEnvVar !== null}
                        autoFocus={!editingEnvVar}
                    />
                </RField>
                <RField label="Value">
                    <RTextarea
                        value={formData.value}
                        onChange={(e) => setFormData({ ...formData, value: e.target.value })}
                        placeholder="postgres://…"
                        rows={3}
                    />
                </RField>
                <RField
                    label="Type"
                    hint="Protected secrets are write-only and cannot be read back. Secret values can be retrieved for development and CI."
                >
                    <RSegmented
                        value={formData.type}
                        options={[
                            { value: 'plain', label: 'Plain' },
                            { value: 'secret', label: 'Secret' },
                            { value: 'protected', label: 'Protected' },
                        ]}
                        onChange={(type) => setFormData({ ...formData, type })}
                    />
                </RField>
            </RModal>

            <RConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setEnvVarToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Environment Variable"
                message={`Are you sure you want to delete the environment variable "${envVarToDelete?.key}"? This action cannot be undone.`}
                confirmText="Delete Variable"
                confirmTone="danger"
                loading={deleting}
            />
        </div>
    );
}

// Extensions Component
export function ExtensionsList({ projectName }) {
    const [availableExtensions, setAvailableExtensions] = useState([]);
    const [enabledExtensions, setEnabledExtensions] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isConfigModalOpen, setIsConfigModalOpen] = useState(false);
    const [isAddModalOpen, setIsAddModalOpen] = useState(false);
    const [selectedExtension, setSelectedExtension] = useState(null);
    const [selectedExtensionData, setSelectedExtensionData] = useState(null);
    const [formData, setFormData] = useState({ spec: '{}' });
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [editMode, setEditMode] = useState(false);
    const [modalTab, setModalTab] = useState('ui');
    const [uiSpec, setUiSpec] = useState({});
    const [deleteConfirmName, setDeleteConfirmName] = useState('');
    const { showToast } = useToast();

    const loadExtensions = useCallback(async () => {
        try {
            // Load both available types and enabled extensions in parallel
            const [typesResponse, enabledResponse] = await Promise.all([
                api.getExtensionTypes(),
                api.getProjectExtensions(projectName)
            ]);

            setAvailableExtensions(mergeExtensionTypesWithPreview(typesResponse.extension_types || []));
            setEnabledExtensions(enabledResponse.extensions || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadExtensions();

        // Auto-refresh every 5 seconds
        const interval = setInterval(loadExtensions, 5000);
        return () => clearInterval(interval);
    }, [loadExtensions]);

    // Handle UI spec changes - merge with JSON spec (upsert)
    const handleUiSpecChange = useCallback((newUiSpec) => {
        setUiSpec(newUiSpec);

        // Parse current JSON spec
        let currentSpec = {};
        try {
            currentSpec = JSON.parse(formData.spec);
        } catch (err) {
            // If JSON is invalid, start fresh
            currentSpec = {};
        }

        // Merge UI spec into current spec (upsert - UI values override, but preserve unknown fields)
        const mergedSpec = { ...currentSpec, ...newUiSpec };

        // Update JSON spec
        setFormData({ spec: JSON.stringify(mergedSpec, null, 2) });
    }, [formData.spec]);

    // Handle JSON spec changes - update UI spec
    const handleJsonSpecChange = useCallback((newJsonString) => {
        setFormData({ spec: newJsonString });

        // Try to parse and update UI spec
        try {
            const parsed = JSON.parse(newJsonString);
            setUiSpec(parsed);
        } catch (err) {
            // Invalid JSON, don't update UI spec
        }
    }, []);

    const handleSave = async () => {
        if (!selectedExtension) return;

        // Parse spec JSON
        let spec;
        try {
            spec = JSON.parse(formData.spec);
        } catch (err) {
            showToast('Invalid JSON in spec: ' + err.message, 'error');
            return;
        }

        setSaving(true);
        try {
            if (editMode) {
                await api.updateExtension(projectName, selectedExtension.name, spec);
                showToast(`Extension ${selectedExtension.name} updated successfully`, 'success');
            } else {
                await api.createExtension(projectName, selectedExtension.name, spec);
                showToast(`Extension ${selectedExtension.name} enabled successfully`, 'success');
            }
            setIsConfigModalOpen(false);
            loadExtensions();
        } catch (err) {
            showToast(`Failed to ${editMode ? 'update' : 'enable'} extension: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDelete = async () => {
        if (!selectedExtensionData || deleteConfirmName !== selectedExtensionData.extension) {
            showToast('Please enter the extension name to confirm deletion', 'error');
            return;
        }

        setDeleting(true);
        try {
            await api.deleteExtension(projectName, selectedExtensionData.extension);
            showToast(`Extension ${selectedExtensionData.extension} deleted successfully`, 'success');
            setIsConfigModalOpen(false);
            loadExtensions();
        } catch (err) {
            showToast(`Failed to delete extension: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    // Helper to check if an extension type is enabled
    const isEnabled = (extensionTypeName) => {
        return enabledExtensions.some(e => e.extension_type === extensionTypeName);
    };

    // Helper to get enabled extension data
    const getEnabledExtension = (extensionTypeName) => {
        return enabledExtensions.find(e => e.extension_type === extensionTypeName);
    };

    if (loading) return <LoadingState label="Loading extensions…" />;
    if (error) return <ErrorState message={`Error loading extensions: ${error}`} onRetry={loadExtensions} />;

    const sortedAvailable = [...availableExtensions].sort((a, b) => a.display_name.localeCompare(b.display_name));
    const sortedEnabled = [...enabledExtensions].sort((a, b) => a.extension.localeCompare(b.extension));

    return (
        <div>
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">Extensions</div>
                    <div className="r-section-sub">Datastores and managed services connected to this project.</div>
                </div>
                {availableExtensions.length > 0 && (
                    <RButton variant="primary" icon="plus" onClick={() => setIsAddModalOpen(true)}>
                        Add extension
                    </RButton>
                )}
            </div>

            {enabledExtensions.length === 0 ? (
                <RPanel>
                    <REmpty title="No extensions enabled">
                        <div style={{ color: 'var(--text-soft)', fontSize: 13 }}>
                            {availableExtensions.length > 0
                                ? 'Add an extension to connect a datastore or managed service.'
                                : 'No extension types are available.'}
                        </div>
                    </REmpty>
                </RPanel>
            ) : (
                <div className="r-grid-2">
                    {sortedEnabled.map(ext => {
                        const extType = availableExtensions.find(t => t.extension_type === ext.extension_type);
                        const iconUrl = getExtensionIcon(ext.extension_type);
                        const abbr = (ext.extension_type || '').replace(/[^a-zA-Z]/g, '').slice(0, 2).toUpperCase() || '?';

                        return (
                            <RPanel
                                key={ext.extension}
                                onClick={() => navigate(`/project/${projectName}/extensions/${ext.extension_type}/${ext.extension}`)}
                            >
                                <div className="r-panel-head">
                                    <div style={{ display: 'flex', alignItems: 'center', gap: 12, minWidth: 0 }}>
                                        {iconUrl ? (
                                            <img src={iconUrl} alt={ext.extension} style={{ width: 36, height: 36, borderRadius: 8, objectFit: 'contain' }} />
                                        ) : (
                                            <div style={{
                                                width: 36, height: 36, borderRadius: 8, background: 'var(--accent)', color: '#fff',
                                                display: 'grid', placeItems: 'center', fontWeight: 600, fontSize: 13,
                                            }}>{abbr}</div>
                                        )}
                                        <div style={{ minWidth: 0 }}>
                                            <div className="r-panel-title">{ext.extension}</div>
                                            <div className="r-panel-sub mono">{extType?.display_name || ext.extension_type}</div>
                                        </div>
                                    </div>
                                    {renderExtensionStatusBadge(ext)}
                                </div>
                                <div className="r-panel-body">
                                    <RKV>
                                        <RKVRow k="Type">
                                            <span className="mono" style={{ fontSize: 12.5 }}>{ext.extension_type}</span>
                                        </RKVRow>
                                        <RKVRow k="Status">{ext.status_summary || '—'}</RKVRow>
                                        <RKVRow k="Updated">{formatDate(ext.updated)}</RKVRow>
                                    </RKV>
                                </div>
                            </RPanel>
                        );
                    })}
                </div>
            )}

            {/* Add extension picker */}
            <RModal
                isOpen={isAddModalOpen}
                onClose={() => setIsAddModalOpen(false)}
                title="Add Extension"
            >
                <p className="text-sm text-gray-600 dark:text-gray-500">
                    Choose an extension type to add to this project.
                </p>
                <div className="r-stack tight">
                    {sortedAvailable.map(extType => {
                            const iconUrl = getExtensionIcon(extType.extension_type);
                            const abbr = (extType.extension_type || '').replace(/[^a-zA-Z]/g, '').slice(0, 2).toUpperCase() || '?';
                            return (
                                <button
                                    key={extType.extension_type}
                                    type="button"
                                    className="r-panel"
                                    style={{
                                        display: 'flex', alignItems: 'center', gap: 12, padding: 12,
                                        textAlign: 'left', cursor: 'pointer', background: 'var(--surface)',
                                        font: 'inherit', color: 'inherit', width: '100%',
                                    }}
                                    onClick={() => {
                                        setIsAddModalOpen(false);
                                        navigate(`/project/${projectName}/extensions/${extType.extension_type}/@new`);
                                    }}
                                >
                                    {iconUrl ? (
                                        <img src={iconUrl} alt={extType.display_name} style={{ width: 32, height: 32, borderRadius: 6, objectFit: 'contain' }} />
                                    ) : (
                                        <div style={{
                                            width: 32, height: 32, borderRadius: 6, background: 'var(--accent)', color: '#fff',
                                            display: 'grid', placeItems: 'center', fontWeight: 600, fontSize: 12,
                                        }}>{abbr}</div>
                                    )}
                                    <div style={{ minWidth: 0 }}>
                                        <div style={{ fontSize: 13.5, fontWeight: 500 }}>{extType.display_name}</div>
                                        {extType.description && (
                                            <div style={{ fontSize: 12, color: 'var(--text-soft)' }}>{extType.description}</div>
                                        )}
                                    </div>
                                </button>
                            );
                        })}
                </div>
            </RModal>

            {/* Configuration Modal */}
            <RModal
                isOpen={isConfigModalOpen}
                onClose={() => setIsConfigModalOpen(false)}
                title={editMode ? `Extension: ${selectedExtension?.name}` : `Enable Extension: ${selectedExtension?.name}`}
                width="xwide"
                footer={selectedExtension && (
                    modalTab === 'delete' && editMode && selectedExtensionData ? (
                        <>
                            <RButton
                                onClick={() => setModalTab(hasExtensionUI(selectedExtension.extension_type) ? 'ui' : 'config')}
                                disabled={deleting}
                            >
                                Cancel
                            </RButton>
                            <RButton
                                variant="danger"
                                onClick={handleDelete}
                                loading={deleting}
                                disabled={deleteConfirmName !== selectedExtensionData.extension}
                            >
                                Delete Extension
                            </RButton>
                        </>
                    ) : (
                        <>
                            <RButton onClick={() => setIsConfigModalOpen(false)} disabled={saving}>Cancel</RButton>
                            <RButton variant="primary" onClick={handleSave} loading={saving}>
                                {editMode ? 'Update' : 'Enable'}
                            </RButton>
                        </>
                    )
                )}
            >
                    {selectedExtension && (
                        <>
                            {/* Tab Navigation */}
                            <RTabs
                                tabs={[
                                    ...(hasExtensionUI(selectedExtension.extension_type)
                                        ? [{ id: 'ui', label: 'Configure' }]
                                        : []),
                                    { id: 'config', label: hasExtensionUI(selectedExtension.extension_type) ? 'JSON' : 'Configuration' },
                                    { id: 'schema', label: 'Schema' },
                                    ...(editMode && selectedExtensionData
                                        ? [
                                            { id: 'status', label: 'Status' },
                                            { id: 'delete', label: 'Delete' },
                                        ]
                                        : []),
                                ]}
                                active={modalTab}
                                onChange={setModalTab}
                            />

                            {/* Tab Content */}
                            {modalTab === 'ui' && hasExtensionUI(selectedExtension.extension_type) && (
                                <div className="space-y-4">
                                    {createElement(getExtensionUI(selectedExtension.extension_type), {
                                        spec: uiSpec,
                                        schema: selectedExtension.spec_schema,
                                        onChange: handleUiSpecChange,
                                        projectName,
                                        instanceName: selectedExtension.name,
                                        isEnabled: editMode,
                                    })}
                                </div>
                            )}

                            {modalTab === 'config' && (
                                <div className="space-y-4">
                                    <RField label={<>Configuration Spec (JSON)<span className="text-red-300 ml-1">*</span></>}>
                                        <RTextarea
                                            value={formData.spec}
                                            onChange={(e) => handleJsonSpecChange(e.target.value)}
                                            placeholder="{}"
                                            rows={15}
                                        />
                                    </RField>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">
                                        Enter the extension configuration as a JSON object. See the Schema tab and Project Extensions docs for valid fields and examples.
                                        {hasExtensionUI(selectedExtension.extension_type) && <span> Use the Configure tab for a form-based interface.</span>}
                                    </p>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">
                                        Extension docs: <a href={extensionDocsHref(selectedExtension.extension_type)} onClick={(e) => {
                                            e.preventDefault();
                                            navigate(extensionDocsHref(selectedExtension.extension_type));
                                        }} className="underline">Open {selectedExtension.extension_type} documentation</a>
                                    </p>
                                </div>
                            )}

                            {modalTab === 'schema' && (
                                <div className="space-y-4">
                                    <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300">Schema</h4>
                                    <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-gray-100 dark:bg-gray-800 p-4 rounded overflow-x-auto max-h-96">
                                        {JSON.stringify(selectedExtension.spec_schema, null, 2)}
                                    </pre>
                                    <p className="text-sm text-gray-600 dark:text-gray-500">
                                        This JSON schema defines the valid structure for the extension configuration.
                                    </p>
                                </div>
                            )}

                            {modalTab === 'status' && editMode && selectedExtensionData && (
                                <div className="space-y-4">
                                    <div>
                                        <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Status Summary</h4>
                                        <p className="text-gray-900 dark:text-gray-200">{selectedExtensionData.status_summary}</p>
                                    </div>

                                    <div>
                                        <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Current Spec</h4>
                                        <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-gray-100 dark:bg-gray-800 p-3 rounded overflow-x-auto">
                                            {JSON.stringify(selectedExtensionData.spec, null, 2)}
                                        </pre>
                                    </div>

                                    <div>
                                        <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Full Status</h4>
                                        <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-gray-100 dark:bg-gray-800 p-3 rounded overflow-x-auto max-h-96">
                                            {JSON.stringify(selectedExtensionData.status, null, 2)}
                                        </pre>
                                    </div>

                                    <div className="text-xs text-gray-600 dark:text-gray-500">
                                        <p>Created: {formatDate(selectedExtensionData.created)}</p>
                                        <p>Updated: {formatDate(selectedExtensionData.updated)}</p>
                                    </div>
                                </div>
                            )}

                            {modalTab === 'delete' && editMode && selectedExtensionData && (
                                <div className="space-y-4">
                                    <div className="bg-red-900/20 border border-red-700 rounded-lg p-4">
                                        <h4 className="text-sm font-semibold text-red-300 mb-2">Warning: Permanent Deletion</h4>
                                        <p className="text-sm text-red-200">
                                            Deleting this extension will deprovision all resources created by this extension.
                                            This action cannot be undone.
                                        </p>
                                    </div>

                                    <RField label={<>{`Type "${selectedExtensionData.extension}" to confirm deletion`}<span className="text-red-300 ml-1">*</span></>}>
                                        <RInput
                                            value={deleteConfirmName}
                                            onChange={(e) => setDeleteConfirmName(e.target.value)}
                                            placeholder={selectedExtensionData.extension}
                                        />
                                    </RField>
                                </div>
                            )}
                        </>
                    )}
            </RModal>

        </div>
    );
}

// Helper function to render status badges with color coding
function renderExtensionStatusBadge(extension) {
    // Show pending deletion badge when extension is marked for deletion
    if (extension.deleted) {
        const state = (extension.status?.state || '').toLowerCase();
        if (state === 'deletion_blocked') return <RStatus status="deletion_blocked" />;
        return <RStatus status={state === 'deleting' ? 'deleting' : 'pending'} />;
    }

    // Check if extension has custom status badge renderer
    const customRenderer = getExtensionStatusBadge(extension.extension_type);
    if (customRenderer) {
        const customBadge = customRenderer(extension);
        if (customBadge) return customBadge;
    }

    // Fallback: render the lifecycle state (or summary) as a design status pill.
    const statusText = extension.status?.state || extension.status_summary || 'Unknown';
    return <RStatus status={statusText} />;
}

// Mono JSON block used across extension detail tabs.
const extJsonPreStyle = {
    margin: 0,
    fontSize: 12,
    lineHeight: 1.5,
    overflow: 'auto',
    maxHeight: 420,
    whiteSpace: 'pre',
};

function ExtensionJsonPanel({ title, sub, value, maxHeight }: any) {
    return (
        <RPanel>
            <RPanelHead title={title} sub={sub} />
            <RPanelBody>
                <pre className="mono" style={{ ...extJsonPreStyle, maxHeight: maxHeight ?? extJsonPreStyle.maxHeight }}>
                    {JSON.stringify(value, null, 2)}
                </pre>
            </RPanelBody>
        </RPanel>
    );
}

// Generic Extension Detail View (fallback for extensions without custom UI)
function GenericExtensionDetailView({ extension }) {
    return (
        <div className="r-stack">
            <RPanel>
                <RPanelHead title="Status" />
                <RPanelBody>
                    <p style={{ margin: 0, color: 'var(--text)' }}>{extension.status_summary}</p>
                    {extension.status && extension.status.error && (
                        <div style={{ marginTop: 12 }}>
                            <RAlert tone="err">
                                <strong>Error:</strong> {extension.status.error}
                            </RAlert>
                        </div>
                    )}
                </RPanelBody>
            </RPanel>

            <ExtensionJsonPanel title="Configuration" value={extension.spec} />

            <ExtensionJsonPanel title="Full Status" value={extension.status} />

            <RPanel>
                <RPanelHead title="Metadata" />
                <RPanelBody>
                    <RKV>
                        <RKVRow k="Created">{formatDate(extension.created)}</RKVRow>
                        <RKVRow k="Updated">{formatDate(extension.updated)}</RKVRow>
                    </RKV>
                </RPanelBody>
            </RPanel>
        </div>
    );
}

// Extension Detail Page Component
export function ExtensionDetailPage({ projectName, extensionType: extensionTypeProp, extensionInstance }) {
    // Helper to get a user-friendly default name for an extension type
    const getDefaultExtensionName = (extensionType) => {
        if (!extensionType) return '';

        // Map extension types to friendly default names
        const defaults = {
            'aws-rds-provisioner': 'rds',
            'snowflake-oauth-provisioner': 'snowflake',
        };

        return defaults[extensionType] || extensionType;
    };

    const [extensionType, setExtensionType] = useState(null);
    const [backendExtensionTypeIds, setBackendExtensionTypeIds] = useState([]);
    const [enabledExtension, setEnabledExtension] = useState(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [activeTab, setActiveTab] = useState('overview');
    const [formData, setFormData] = useState({ spec: '{}' });
    const [uiSpec, setUiSpec] = useState({});
    const [saving, setSaving] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const [confirmDeleteOpen, setConfirmDeleteOpen] = useState(false);
    const [originalSpec, setOriginalSpec] = useState('{}');
    const [instanceName, setInstanceName] = useState(getDefaultExtensionName(extensionTypeProp)); // Instance name for new extensions
    const { showToast } = useToast();

    const isEnabled = enabledExtension !== null;
    const isPreviewOnly = !!(extensionType && !backendExtensionTypeIds.includes(extensionType.extension_type));

    // Check if there are unsaved changes (normalize JSON to ignore key order)
    const hasUnsavedChanges = normalizeJSON(formData.spec) !== normalizeJSON(originalSpec);

    // Memoize the extension UI API
    const extensionAPI = useMemo(() => {
        return extensionType ? getExtensionUIAPI(extensionType.extension_type) : null;
    }, [extensionType]);

    useEffect(() => {
        async function fetchData() {
            try {
                // Fetch both extension type info and enabled extension data
                const [typesResponse, enabledResponse] = await Promise.all([
                    api.getExtensionTypes(),
                    api.getProjectExtensions(projectName)
                ]);
                const backendTypes = typesResponse.extension_types || [];
                const mergedTypes = mergeExtensionTypesWithPreview(backendTypes);
                setBackendExtensionTypeIds(backendTypes.map(t => t.extension_type));

                let enabled = null;
                let extType = null;

                if (extensionInstance) {
                    // We're viewing a specific instance - find it by name
                    enabled = enabledResponse.extensions.find(e => e.extension === extensionInstance);
                    if (!enabled) {
                        setError('Extension instance not found');
                        setLoading(false);
                        return;
                    }
                    // Find the type for this instance
                    extType = mergedTypes.find(t => t.extension_type === enabled.extension_type);
                    if (!extType) {
                        setError('Extension type not found for this instance');
                        setLoading(false);
                        return;
                    }
                } else if (extensionTypeProp) {
                    // We're creating a new instance of a specific type
                    extType = mergedTypes.find(t => t.extension_type === extensionTypeProp);
                    if (!extType) {
                        setError('Extension type not found');
                        setLoading(false);
                        return;
                    }
                } else {
                    setError('No extension type or instance specified');
                    setLoading(false);
                    return;
                }

                setExtensionType(extType);
                setEnabledExtension(enabled || null);

                // Set form data only on initial load
                if (loading) {
                    if (enabled) {
                        const specJson = JSON.stringify(enabled.spec, null, 2);
                        setFormData({ spec: specJson });
                        setOriginalSpec(specJson);
                        setUiSpec(enabled.spec);
                        setInstanceName(enabled.extension); // Set instance name from enabled extension
                        setActiveTab('overview');
                    } else {
                        const defaultSpec = {};
                        const specJson = JSON.stringify(defaultSpec, null, 2);
                        setFormData({ spec: specJson });
                        setOriginalSpec(specJson);
                        setUiSpec(defaultSpec);
                        setActiveTab(hasExtensionUI(extType.extension_type) ? 'configure' : 'config');
                    }
                }
            } catch (err) {
                setError(err.message);
            } finally {
                setLoading(false);
            }
        }
        fetchData();
    }, [projectName, extensionTypeProp, extensionInstance]);

    // Handle UI spec changes
    const handleUiSpecChange = useCallback((newUiSpec) => {
        setUiSpec(newUiSpec);
        setFormData({ spec: JSON.stringify(newUiSpec, null, 2) });
    }, []);

    // Handle JSON spec changes
    const handleJsonSpecChange = useCallback((newJsonString) => {
        setFormData({ spec: newJsonString });
        try {
            const parsed = JSON.parse(newJsonString);
            setUiSpec(parsed);
        } catch (err) {
            // Invalid JSON, don't update UI spec
        }
    }, []);

    const handleSave = async () => {
        if (isPreviewOnly) {
            showToast(`"${extensionType.display_name}" is in preview mode only. Install the provider in the backend to enable saving.`, 'error');
            return;
        }

        // Validate instance name for new extensions
        if (!isEnabled && !instanceName.trim()) {
            showToast('Extension name is required', 'error');
            return;
        }

        let spec;
        try {
            spec = JSON.parse(formData.spec);
        } catch (err) {
            showToast('Invalid JSON in spec: ' + err.message, 'error');
            return;
        }

        const wasEnabled = isEnabled;
        setSaving(true);
        try {
            if (isEnabled) {
                // Update existing extension using its instance name
                await api.updateExtension(projectName, enabledExtension.extension, spec);
                showToast(`Extension ${extensionType.display_name} updated successfully`, 'success');
            } else {
                // Create new extension using user-provided instance name
                await api.createExtension(projectName, instanceName.trim(), extensionType.extension_type, spec);
                showToast(`Extension ${instanceName.trim()} created successfully`, 'success');
                navigate(`/project/${projectName}/extensions`);
                return;
            }
            // Refresh data
            const enabledResponse = await api.getProjectExtensions(projectName);
            // After creating, look for the newly created instance by name
            // After updating, refresh the existing instance
            let enabled;
            if (wasEnabled) {
                enabled = enabledResponse.extensions.find(e => e.extension === enabledExtension.extension);
            } else {
                enabled = enabledResponse.extensions.find(e => e.extension === instanceName.trim());
            }
            setEnabledExtension(enabled || null);
            if (enabled) {
                const specJson = JSON.stringify(enabled.spec, null, 2);
                setFormData({ spec: specJson });
                setOriginalSpec(specJson);
                setUiSpec(enabled.spec);
                setInstanceName(enabled.extension); // Update instance name
                // Only switch to overview tab when first enabling, not when updating
                if (!wasEnabled) {
                    setActiveTab('overview');
                }
            }
        } catch (err) {
            showToast(`Failed to ${isEnabled ? 'update' : 'enable'} extension: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDelete = async () => {
        if (!enabledExtension) return;

        setDeleting(true);
        try {
            await api.deleteExtension(projectName, enabledExtension.extension);
            showToast(`Extension ${enabledExtension.extension} deleted successfully`, 'success');
            navigate(`/project/${projectName}/extensions`);
        } catch (err) {
            showToast(`Failed to delete extension: ${err.message}`, 'error');
            setDeleting(false);
        }
    };

    if (loading) {
        return <LoadingState label="Loading extension…" />;
    }

    if (error || !extensionType) {
        return <ErrorState message={error || 'Extension type not found'} />;
    }

    const CustomDetailView = getExtensionDetailView(extensionType.extension_type);
    const hasUI = hasExtensionUI(extensionType.extension_type);
    const iconUrl = getExtensionIcon(extensionType.extension_type);
    const unsavedMark = hasUnsavedChanges ? ' *' : '';

    // Build the tab set, keeping the original conditionals.
    const tabs = [];
    if (isEnabled) tabs.push({ id: 'overview', label: 'Overview' });
    if (hasUI) tabs.push({ id: 'configure', label: `Configure${unsavedMark}` });
    tabs.push({ id: 'config', label: `Spec${unsavedMark}` });

    // Validity of the current spec JSON against the extension schema.
    const specValidation = validateJson(formData.spec, extensionType.spec_schema);

    const openRawSchema = () => {
        const blob = new Blob([JSON.stringify(extensionType.spec_schema, null, 2)], {
            type: 'application/json',
        });
        const url = URL.createObjectURL(blob);
        window.open(url, '_blank', 'noopener,noreferrer');
    };

    const instanceNameField = (id) => (
        <RField
            label="Extension Name"
            hint="Give this extension instance a unique name. You can create multiple instances of the same extension type with different names."
        >
            <RInput
                id={id}
                value={instanceName}
                onChange={(e) => setInstanceName(e.target.value)}
                placeholder={getDefaultExtensionName(extensionTypeProp)}
                required
            />
        </RField>
    );

    const saveButton = (
        <RButton
            variant="primary"
            onClick={handleSave}
            loading={saving}
            disabled={isPreviewOnly || !specValidation.jsonValid}
            className={!isEnabled ? 'mono-btn-cta' : ''}
        >
            {isPreviewOnly ? 'Preview Only' : (isEnabled ? 'Update' : 'Enable')}
        </RButton>
    );

    return (
        <section>
            <div className="r-page-head">
                {iconUrl && (
                    <img
                        src={iconUrl}
                        alt={extensionType.display_name}
                        style={{ width: 44, height: 44, borderRadius: 6, objectFit: 'contain' }}
                    />
                )}
                <div className="title-stack">
                    <h1 className="r-page-title">{extensionType.display_name}</h1>
                    {extensionType.description && (
                        <div className="r-page-sub">{extensionType.description}</div>
                    )}
                </div>
                <RButton
                    variant="default"
                    size="sm"
                    onClick={() => navigate(extensionDocsHref(extensionType.extension_type))}
                >
                    Extension Docs
                </RButton>
                {isEnabled && (
                    <RButton variant="danger" size="sm" icon="trash" onClick={() => setConfirmDeleteOpen(true)}>
                        Delete
                    </RButton>
                )}
                {isEnabled ? (
                    renderExtensionStatusBadge(enabledExtension)
                ) : (
                    <RStatus status="Not Enabled" />
                )}
            </div>

            {isPreviewOnly && (
                <div style={{ marginBottom: 20 }}>
                    <RAlert tone="warn">
                        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                            <span>Preview mode: this extension UI is available for configuration testing only. Install the backend provider to enable create and update actions.</span>
                            <RButton
                                variant="default"
                                size="sm"
                                onClick={() => disablePreviewExtensions(projectName)}
                            >
                                Disable Preview Mode
                            </RButton>
                        </div>
                    </RAlert>
                </div>
            )}

            <RTabs tabs={tabs} active={activeTab} onChange={setActiveTab} />

            <div>
                {activeTab === 'overview' && isEnabled && CustomDetailView && (
                    <CustomDetailView extension={enabledExtension} projectName={projectName} />
                )}

                {activeTab === 'overview' && isEnabled && !CustomDetailView && (
                    <GenericExtensionDetailView extension={enabledExtension} />
                )}

                {activeTab === 'configure' && extensionAPI?.renderConfigureTab && (
                    <div className="r-stack">
                        {!isEnabled && (
                            <RPanel>
                                <RPanelBody>{instanceNameField('extension-instance-name')}</RPanelBody>
                            </RPanel>
                        )}
                        {extensionAPI.renderConfigureTab(uiSpec, extensionType.spec_schema, handleUiSpecChange, projectName, instanceName, isEnabled)}
                        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 12 }}>
                            {saveButton}
                        </div>
                    </div>
                )}

                {activeTab === 'config' && (
                    <Suspense fallback={<JsonEditorFallback />}>
                    <div className="r-stack">
                        {!isEnabled && (
                            <RPanel>
                                <RPanelBody>{instanceNameField('extension-instance-name-config')}</RPanelBody>
                            </RPanel>
                        )}
                        <RPanel>
                            <RPanelHead
                                title="Configuration Spec (JSON)"
                                sub="Enter the extension configuration as a JSON object. See the raw schema and Project Extensions docs for valid fields and examples."
                            />
                            <RPanelBody>
                                <div
                                    style={{
                                        display: 'flex',
                                        alignItems: 'center',
                                        justifyContent: 'space-between',
                                        gap: 12,
                                        flexWrap: 'wrap',
                                        marginBottom: 8,
                                    }}
                                >
                                    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                                        <strong style={{ fontSize: 13 }}>Spec</strong>
                                        {specValidation.jsonValid && specValidation.schemaValid ? (
                                            <RPill kind="accent">Schema valid</RPill>
                                        ) : !specValidation.jsonValid ? (
                                            <RPill>Invalid JSON</RPill>
                                        ) : (
                                            <RPill>
                                                {specValidation.errors.length} issue
                                                {specValidation.errors.length === 1 ? '' : 's'}
                                            </RPill>
                                        )}
                                    </div>
                                    <RButton size="sm" icon="ext" onClick={openRawSchema}>
                                        View raw schema
                                    </RButton>
                                </div>
                                <JsonEditor
                                    value={formData.spec}
                                    onChange={handleJsonSpecChange}
                                    ariaLabel="Extension configuration spec"
                                />
                                {specValidation.errors.length > 0 && (
                                    <div style={{ marginTop: 8 }}>
                                        <RAlert tone="err">
                                            <strong>
                                                {specValidation.jsonValid
                                                    ? 'Schema validation issues'
                                                    : 'Invalid JSON'}
                                            </strong>
                                            <ul style={{ margin: '4px 0 0', paddingLeft: 18 }}>
                                                {specValidation.errors.map((err, i) => (
                                                    <li key={i}>{err}</li>
                                                ))}
                                            </ul>
                                        </RAlert>
                                    </div>
                                )}
                                <p style={{ fontSize: 12.5, color: 'var(--text-soft)', marginTop: 8 }}>
                                    {hasUI && 'Use the Configure tab for a form-based interface. '}
                                    Extension docs:{' '}
                                    <a
                                        href={extensionDocsHref(extensionType.extension_type)}
                                        onClick={(e) => {
                                            e.preventDefault();
                                            navigate(extensionDocsHref(extensionType.extension_type));
                                        }}
                                        className="r-link"
                                    >
                                        Open {extensionType.extension_type} documentation
                                    </a>
                                </p>
                            </RPanelBody>
                        </RPanel>

                        {isEnabled && enabledExtension.status && (
                            <RPanel>
                                <RPanelHead
                                    title="Status"
                                    right={
                                        <span style={{ display: 'inline-flex', color: 'var(--text-soft)' }} title="Read-only">
                                            <Icon name="lock" size={13} />
                                        </span>
                                    }
                                />
                                <RPanelBody>
                                    <JsonEditor
                                        value={JSON.stringify(enabledExtension.status, null, 2)}
                                        readOnly
                                        ariaLabel="Extension status"
                                    />
                                </RPanelBody>
                            </RPanel>
                        )}

                        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 12 }}>
                            {saveButton}
                        </div>
                    </div>
                    </Suspense>
                )}

            </div>

            {isEnabled && (
                <RConfirmDialog
                    isOpen={confirmDeleteOpen}
                    onClose={() => setConfirmDeleteOpen(false)}
                    onConfirm={handleDelete}
                    title={`Delete extension ${enabledExtension.extension}?`}
                    message={
                        <p style={{ marginTop: 0 }}>
                            Deleting this extension will deprovision all resources it created.
                            This cannot be undone.
                        </p>
                    }
                    confirmText="Delete extension"
                    requireText={enabledExtension.extension}
                    loading={deleting}
                />
            )}
        </section>
    );
}
