// @ts-nocheck
import { createElement, useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { navigate } from '../lib/navigation';
import { copyToClipboard, formatDate } from '../lib/utils';
import { useToast } from '../components/toast';
import { Button, ConfirmDialog, EnvironmentColorDot, EnvironmentColorPicker, EnvironmentCombobox, EnvironmentDropdown, FormField, Modal, ModalActions, ModalSection, ModalTabs, MonoStatusPill, MonoTabButton, MonoTag, SegmentedRadioGroup } from '../components/ui';
import { MonoTable, MonoTableBody, MonoTableEmptyRow, MonoTableFrame, MonoTableHead, MonoTableRow, MonoTd, MonoTh } from '../components/table';
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
    const { showToast } = useToast();

    const loadEnvironments = useCallback(async () => {
        try {
            const data = await api.getProjectEnvironments(projectName);
            setEnvironments(data || []);
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

    if (loading) return <div className="text-center py-8"><div className="inline-block w-8 h-8 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div></div>;
    if (error) return <p className="text-red-600 dark:text-red-400">Error loading environments: {error}</p>;

    return (
        <div>
            <div className="mb-4 flex justify-end">
                <Button variant="primary" size="sm" onClick={handleAddClick}>
                    Create Environment
                </Button>
            </div>
            <MonoTableFrame>
                <MonoTable>
                    <MonoTableHead>
                        <tr>
                            <MonoTh className="px-6 py-3 text-left">Name</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Primary Group</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Resources</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Created</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Actions</MonoTh>
                        </tr>
                    </MonoTableHead>
                    <MonoTableBody>
                        {environments.length === 0 ? (
                            <MonoTableEmptyRow colSpan={5}>No environments configured.</MonoTableEmptyRow>
                        ) : (
                            environments.map(env => (
                                <MonoTableRow
                                    key={env.name}
                                    interactive
                                    className="transition-colors"
                                    onClick={() => navigate(`/project/${projectName}/environment/${env.name}`)}
                                    style={{ cursor: 'pointer' }}
                                >
                                    <MonoTd className="px-6 py-4 text-sm">
                                        <div className="flex items-center gap-2">
                                            <EnvironmentColorDot color={env.color} />
                                            <span>{env.name}</span>
                                            {env.is_production && <MonoTag>production</MonoTag>}
                                        </div>
                                    </MonoTd>
                                    <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">{env.primary_deployment_group || '-'}</MonoTd>
                                    <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">
                                        {env.deployment_constraints ? (
                                            <span className="text-xs">
                                                {env.deployment_constraints.min_replicas != null && env.deployment_constraints.max_replicas != null
                                                    ? `${env.deployment_constraints.min_replicas}–${env.deployment_constraints.max_replicas} replicas`
                                                    : null}
                                                {env.deployment_constraints.min_cpu && env.deployment_constraints.max_cpu
                                                    ? `${env.deployment_constraints.min_replicas != null ? ' · ' : ''}${env.deployment_constraints.min_cpu}–${env.deployment_constraints.max_cpu} cpu`
                                                    : null}
                                                {env.deployment_constraints.min_memory && env.deployment_constraints.max_memory
                                                    ? `${(env.deployment_constraints.min_cpu || env.deployment_constraints.min_replicas != null) ? ' · ' : ''}${env.deployment_constraints.min_memory}–${env.deployment_constraints.max_memory} mem`
                                                    : null}
                                            </span>
                                        ) : '-'}
                                    </MonoTd>
                                    <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">{formatDate(env.created_at)}</MonoTd>
                                    <MonoTd className="px-6 py-4 text-sm">
                                        <div className="flex gap-2">
                                            <Button
                                                variant="secondary"
                                                size="sm"
                                                onClick={(e) => { e.stopPropagation(); handleEditClick(env); }}
                                            >
                                                Edit
                                            </Button>
                                            <Button
                                                variant="danger"
                                                size="sm"
                                                onClick={(e) => { e.stopPropagation(); handleDeleteClick(env); }}
                                                disabled={env.is_production}
                                            >
                                                Delete
                                            </Button>
                                        </div>
                                    </MonoTd>
                                </MonoTableRow>
                            ))
                        )}
                    </MonoTableBody>
                </MonoTable>
            </MonoTableFrame>

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingEnv ? 'Edit Environment' : 'Create Environment'}
            >
                <ModalSection>
                    <FormField
                        label="Name"
                        id="env-name"
                        value={formData.name}
                        onChange={(e) => setFormData({ ...formData, name: e.target.value.toLowerCase() })}
                        placeholder="staging"
                        disabled={editingEnv !== null}
                        required
                    />
                    <FormField
                        label="Primary Deployment Group"
                        id="env-primary-group"
                        value={formData.primary_deployment_group}
                        onChange={(e) => setFormData({ ...formData, primary_deployment_group: e.target.value })}
                        placeholder="default"
                    />
                    <p className="text-xs text-gray-500 dark:text-gray-400 -mt-2">
                        The deployment group that maps to this environment. Leave empty if not applicable.
                    </p>
                    <div className="flex gap-6 mt-2">
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
                    <div className="mt-2">
                        <span className="mono-label">Color</span>
                        <EnvironmentColorPicker
                            value={formData.color}
                            onChange={(c) => setFormData({ ...formData, color: c })}
                        />
                    </div>

                    {isAdmin && editingEnv && (
                        <div className="mt-4 pt-4 border-t border-gray-200 dark:border-gray-700">
                            <span className="mono-label">Resource Constraints</span>
                            <p className="text-xs text-gray-500 dark:text-gray-400 mb-3">
                                Limit the resource range for deployments in this environment. Leave empty to use platform defaults.
                            </p>
                            <div className="grid grid-cols-2 gap-3">
                                <FormField
                                    label="Min Replicas"
                                    id="env-min-replicas"
                                    type="number"
                                    value={formData.min_replicas}
                                    onChange={(e) => setFormData({ ...formData, min_replicas: e.target.value })}
                                    placeholder={platformConstraints?.min_replicas?.toString() ?? '1'}
                                />
                                <FormField
                                    label="Max Replicas"
                                    id="env-max-replicas"
                                    type="number"
                                    value={formData.max_replicas}
                                    onChange={(e) => setFormData({ ...formData, max_replicas: e.target.value })}
                                    placeholder={platformConstraints?.max_replicas?.toString() ?? '1'}
                                />
                                <FormField
                                    label="Min CPU"
                                    id="env-min-cpu"
                                    value={formData.min_cpu}
                                    onChange={(e) => setFormData({ ...formData, min_cpu: e.target.value })}
                                    placeholder={platformConstraints?.min_cpu ?? '100m'}
                                />
                                <FormField
                                    label="Max CPU"
                                    id="env-max-cpu"
                                    value={formData.max_cpu}
                                    onChange={(e) => setFormData({ ...formData, max_cpu: e.target.value })}
                                    placeholder={platformConstraints?.max_cpu ?? '2'}
                                />
                                <FormField
                                    label="Min Memory"
                                    id="env-min-memory"
                                    value={formData.min_memory}
                                    onChange={(e) => setFormData({ ...formData, min_memory: e.target.value })}
                                    placeholder={platformConstraints?.min_memory ?? '64Mi'}
                                />
                                <FormField
                                    label="Max Memory"
                                    id="env-max-memory"
                                    value={formData.max_memory}
                                    onChange={(e) => setFormData({ ...formData, max_memory: e.target.value })}
                                    placeholder={platformConstraints?.max_memory ?? '2Gi'}
                                />
                            </div>
                        </div>
                    )}

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => setIsModalOpen(false)}
                            disabled={saving}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleSave}
                            loading={saving}
                        >
                            {editingEnv ? 'Update' : 'Create'}
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setEnvToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Environment"
                message={`Are you sure you want to delete the environment "${envToDelete?.name}"? This action cannot be undone.`}
                confirmText="Delete Environment"
                variant="danger"
                loading={deleting}
            />
        </div>
    );
}

// Service Accounts Component
export function ServiceAccountsList({ projectName }) {
    const [serviceAccounts, setServiceAccounts] = useState([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(null);
    const [isModalOpen, setIsModalOpen] = useState(false);
    const [editingSA, setEditingSA] = useState(null);
    const [formData, setFormData] = useState({ issuer_url: '', aud: '', claims: {} });
    const [claimsText, setClaimsText] = useState('');
    const [confirmDialogOpen, setConfirmDialogOpen] = useState(false);
    const [saToDelete, setSAToDelete] = useState(null);
    const [deleting, setDeleting] = useState(false);
    const [saving, setSaving] = useState(false);
    const [environments, setEnvironments] = useState([]);
    const [selectedEnvs, setSelectedEnvs] = useState([]);
    const { showToast } = useToast();

    const loadServiceAccounts = useCallback(async () => {
        try {
            const response = await api.getProjectServiceAccounts(projectName);
            setServiceAccounts(response.service_accounts || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName]);

    useEffect(() => {
        loadServiceAccounts();
    }, [loadServiceAccounts]);

    useEffect(() => {
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
    }, [projectName]);

    const handleAddClick = () => {
        setEditingSA(null);
        // Default aud to Rise backend URL (where the API is hosted)
        const defaultAud = CONFIG.backendUrl;
        setFormData({ issuer_url: '', aud: defaultAud, claims: {} });
        setClaimsText('');
        setSelectedEnvs([]);
        setIsModalOpen(true);
    };

    const handleEditClick = (sa) => {
        setEditingSA(sa);
        // Extract aud from existing claims
        const aud = sa.claims?.aud || '';
        setFormData({ issuer_url: sa.issuer_url, aud, claims: sa.claims || {} });
        // Convert claims object to JSON string for editing (excluding aud)
        const claimsObj = { ...sa.claims };
        delete claimsObj.aud; // aud is handled separately
        setClaimsText(JSON.stringify(claimsObj, null, 2));
        setSelectedEnvs(sa.allowed_environments || []);
        setIsModalOpen(true);
    };

    const handleDeleteClick = (sa) => {
        setSAToDelete(sa);
        setConfirmDialogOpen(true);
    };

    const handleSave = async () => {
        if (!formData.issuer_url) {
            showToast('Issuer URL is required', 'error');
            return;
        }

        if (!formData.aud) {
            showToast('Audience (aud) is required', 'error');
            return;
        }

        // Parse additional claims from text
        let claims = {};
        try {
            if (claimsText.trim()) {
                claims = JSON.parse(claimsText);
            }
        } catch (err) {
            showToast('Invalid JSON in additional claims', 'error');
            return;
        }

        // Add aud claim from form data
        claims.aud = formData.aud;

        const allowedEnvs = selectedEnvs.length === 0 ? [] : selectedEnvs;

        setSaving(true);
        try {
            if (editingSA) {
                await api.updateServiceAccount(projectName, editingSA.id, formData.issuer_url, claims, allowedEnvs);
                showToast('Service account updated successfully', 'success');
            } else {
                await api.createServiceAccount(projectName, formData.issuer_url, claims, allowedEnvs);
                showToast('Service account created successfully', 'success');
            }
            setIsModalOpen(false);
            loadServiceAccounts();
        } catch (err) {
            showToast(`Failed to ${editingSA ? 'update' : 'create'} service account: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    const handleDeleteConfirm = async () => {
        if (!saToDelete) return;

        setDeleting(true);
        try {
            await api.deleteServiceAccount(projectName, saToDelete.id);
            showToast(`Service account ${saToDelete.email} deleted successfully`, 'success');
            setConfirmDialogOpen(false);
            setSAToDelete(null);
            loadServiceAccounts();
        } catch (err) {
            showToast(`Failed to delete service account: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    if (loading) return <div className="text-center py-8"><div className="inline-block w-8 h-8 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div></div>;
    if (error) return <p className="text-red-600 dark:text-red-400">Error loading service accounts: {error}</p>;

    return (
        <div>
            <div className="mb-4 flex justify-end">
                <Button variant="primary" size="sm" onClick={handleAddClick}>
                    Create Service Account
                </Button>
            </div>
            <MonoTableFrame>
                <MonoTable>
                    <MonoTableHead>
                        <tr>
                            <MonoTh className="px-6 py-3 text-left">Email</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Issuer URL</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Claims</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Environments</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Created</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Actions</MonoTh>
                        </tr>
                    </MonoTableHead>
                    <MonoTableBody>
                        {serviceAccounts.length === 0 ? (
                            <MonoTableEmptyRow colSpan={6}>No service accounts found.</MonoTableEmptyRow>
                        ) : (
                            serviceAccounts.map(sa => (
                            <MonoTableRow key={sa.id} interactive className="transition-colors">
                                <MonoTd className="px-6 py-4 text-sm text-gray-900 dark:text-gray-200">{sa.email}</MonoTd>
                                <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300 break-all max-w-xs">{sa.issuer_url}</MonoTd>
                                <MonoTd className="px-6 py-4 text-xs font-mono text-gray-700 dark:text-gray-300">
                                    {Object.entries(sa.claims || {})
                                        .map(([key, value]) => `${key}=${value}`)
                                        .join(', ')}
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">
                                    {sa.allowed_environments ? sa.allowed_environments.map((envName, i) => (
                                        <span key={envName} className="inline-flex items-center gap-1.5">
                                            {i > 0 && ', '}
                                            <EnvironmentColorDot color={environments.find(e => e.name === envName)?.color} />
                                            {envName}
                                        </span>
                                    )) : 'All'}
                                </MonoTd>
                                <MonoTd className="px-6 py-4 whitespace-nowrap text-sm text-gray-700 dark:text-gray-300">{formatDate(sa.created_at)}</MonoTd>
                                <MonoTd className="px-6 py-4 text-sm">
                                    <div className="flex gap-2">
                                        <Button
                                            variant="secondary"
                                            size="sm"
                                            onClick={() => handleEditClick(sa)}
                                        >
                                            Edit
                                        </Button>
                                        <Button
                                            variant="danger"
                                            size="sm"
                                            onClick={() => handleDeleteClick(sa)}
                                        >
                                            Delete
                                        </Button>
                                    </div>
                                </MonoTd>
                            </MonoTableRow>
                        ))
                        )}
                    </MonoTableBody>
                </MonoTable>
            </MonoTableFrame>

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingSA ? 'Edit Service Account' : 'Create Service Account'}
            >
                <ModalSection>
                    {environments.length > 0 && (
                        <div>
                            <span className="mono-label">Allowed Environments</span>
                            <EnvironmentCombobox
                                environments={environments}
                                selected={selectedEnvs}
                                onChange={setSelectedEnvs}
                                placeholder="All environments"
                            />
                        </div>
                    )}
                    <FormField
                        label="Issuer URL"
                        id="sa-issuer-url"
                        value={formData.issuer_url}
                        onChange={(e) => setFormData({ ...formData, issuer_url: e.target.value })}
                        placeholder="https://token.actions.githubusercontent.com"
                        required
                        autoFocus
                    />
                    <FormField
                        label="Audience (aud)"
                        id="sa-aud"
                        value={formData.aud}
                        onChange={(e) => setFormData({ ...formData, aud: e.target.value })}
                        placeholder={CONFIG.backendUrl}
                        required
                    />
                    <FormField
                        label="Additional Claims (JSON)"
                        id="sa-claims"
                        type="textarea"
                        value={claimsText}
                        onChange={(e) => setClaimsText(e.target.value)}
                        placeholder={`{\n  "sub": "repo:myorg/myrepo:*"\n}`}
                        rows={5}
                    />
                    <p className="text-sm text-gray-600 dark:text-gray-500">
                        <strong>Note:</strong> Additional claims should be provided as a JSON object. The <code className="bg-gray-100 dark:bg-gray-800 px-1 rounded">aud</code> claim is configured separately above.
                    </p>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => setIsModalOpen(false)}
                            disabled={saving}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleSave}
                            loading={saving}
                        >
                            {editingSA ? 'Update' : 'Create'}
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setSAToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Service Account"
                message={`Are you sure you want to delete the service account "${saToDelete?.email}"? This action cannot be undone.`}
                confirmText="Delete Service Account"
                variant="danger"
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

    if (loading) return <div className="text-center py-8"><div className="inline-block w-8 h-8 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div></div>;
    if (error) return <p className="text-red-600 dark:text-red-400">Error loading custom domains: {error}</p>;

    return (
        <div>
            <div className="mb-4 flex justify-end">
                <Button variant="primary" size="sm" onClick={handleAddClick}>
                    Add Domain
                </Button>
            </div>
            <MonoTableFrame>
                <MonoTable>
                    <MonoTableHead>
                        <tr>
                            <MonoTh className="px-6 py-3 text-left">Domain</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Primary</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Created</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Actions</MonoTh>
                        </tr>
                    </MonoTableHead>
                    <MonoTableBody>
                        {defaultUrl && (
                            <MonoTableRow interactive className="transition-colors">
                                <MonoTd className="px-6 py-4 text-sm font-mono text-gray-900 dark:text-gray-200">
                                    <a href={defaultUrl} target="_blank" rel="noopener noreferrer" className="underline">
                                        {(() => { try { return new URL(defaultUrl).hostname; } catch { return defaultUrl; } })()}
                                    </a>
                                    <MonoTag className="ml-2">default</MonoTag>
                                    {isDefaultPrimary && (
                                        <MonoTag color="yellow" className="ml-1">primary</MonoTag>
                                    )}
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm">
                                    <button
                                        onClick={handleStarDefault}
                                        disabled={isDefaultPrimary || updatingPrimaryDomain === '__default__'}
                                        className="text-gray-400 hover:text-yellow-500 dark:text-gray-500 dark:hover:text-yellow-400 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                                        title={isDefaultPrimary ? "Default URL is primary" : "Set default URL as primary"}
                                    >
                                        {updatingPrimaryDomain === '__default__' ? (
                                            <svg className="w-5 h-5 animate-spin" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
                                                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4"></circle>
                                                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                            </svg>
                                        ) : isDefaultPrimary ? (
                                            <svg className="w-5 h-5 fill-current text-yellow-500 dark:text-yellow-400" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
                                                <path d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z"/>
                                            </svg>
                                        ) : (
                                            <svg className="w-5 h-5 stroke-current" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth="2">
                                                <path strokeLinecap="round" strokeLinejoin="round" d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z"/>
                                            </svg>
                                        )}
                                    </button>
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">-</MonoTd>
                                <MonoTd className="px-6 py-4 text-sm"></MonoTd>
                            </MonoTableRow>
                        )}
                        {domains.map(domain => (
                            <MonoTableRow key={domain.id} interactive className="transition-colors">
                                <MonoTd className="px-6 py-4 text-sm font-mono text-gray-900 dark:text-gray-200">
                                    {domain.domain}
                                    {domain.is_primary && (
                                        <MonoTag color="yellow" className="ml-2">primary</MonoTag>
                                    )}
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm">
                                    <button
                                        onClick={() => handleTogglePrimary(domain)}
                                        disabled={updatingPrimaryDomain === domain.domain}
                                        className="text-gray-400 hover:text-yellow-500 dark:text-gray-500 dark:hover:text-yellow-400 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                                        title={domain.is_primary ? "Remove as primary" : "Set as primary"}
                                    >
                                        {updatingPrimaryDomain === domain.domain ? (
                                            <svg className="w-5 h-5 animate-spin" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
                                                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4"></circle>
                                                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                            </svg>
                                        ) : domain.is_primary ? (
                                            <svg className="w-5 h-5 fill-current text-yellow-500 dark:text-yellow-400" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
                                                <path d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z"/>
                                            </svg>
                                        ) : (
                                            <svg className="w-5 h-5 stroke-current" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth="2">
                                                <path strokeLinecap="round" strokeLinejoin="round" d="M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z"/>
                                            </svg>
                                        )}
                                    </button>
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">{formatDate(domain.created_at)}</MonoTd>
                                <MonoTd className="px-6 py-4 text-sm">
                                    <Button
                                        variant="danger"
                                        size="sm"
                                        onClick={() => handleDeleteClick(domain)}
                                    >
                                        Delete
                                    </Button>
                                </MonoTd>
                            </MonoTableRow>
                        ))}
                        {!defaultUrl && domains.length === 0 && (
                            <MonoTableEmptyRow colSpan={4}>No domains configured.</MonoTableEmptyRow>
                        )}
                    </MonoTableBody>
                </MonoTable>
            </MonoTableFrame>

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title="Add Custom Domain"
            >
                <ModalSection>
                    <FormField
                        label="Domain"
                        id="domain-name"
                        value={formData.domain}
                        onChange={(e) => setFormData({ ...formData, domain: e.target.value })}
                        placeholder="example.com"
                        required
                    />
                    <p className="text-sm text-gray-600 dark:text-gray-500">
                        <strong>Note:</strong> Make sure to configure your DNS to point this domain to your Rise deployment before adding it.
                        The domain will be added to the ingress for the default deployment group only.
                    </p>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => setIsModalOpen(false)}
                            disabled={saving}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleSave}
                            loading={saving}
                        >
                            Add Domain
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setDomainToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Custom Domain"
                message={`Are you sure you want to delete the custom domain "${domainToDelete?.domain}"? This action cannot be undone.`}
                confirmText="Delete Domain"
                variant="danger"
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
            <span className="inline-flex items-center gap-1.5">
                <EnvironmentColorDot color={envColor} />
                <MonoTag color="blue">{envName}</MonoTag>
            </span>
        );
    }
    return <MonoTag color={ENV_VAR_SOURCE_COLORS[source] || 'gray'}>{source}</MonoTag>;
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
    const [envFilter, setEnvFilter] = useState('');
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

    // Load environments for filter dropdown and source color dots
    useEffect(() => {
        api.getProjectEnvironments(projectName)
            .then(data => setEnvironments(data || []))
            .catch(() => {});
    }, [projectName]);

    const loadEnvVars = useCallback(async () => {
        try {
            const response = deploymentId
                ? await api.getDeploymentEnvVars(projectName, deploymentId)
                : await api.getProjectEnvVars(projectName, envFilter || null);
            setEnvVars(response.env_vars || []);
            setLoading(false);
        } catch (err) {
            setError(err.message);
            setLoading(false);
        }
    }, [projectName, deploymentId, envFilter]);

    useEffect(() => {
        loadEnvVars();
    }, [loadEnvVars]);

    const handleAddClick = () => {
        setEditingEnvVar(null);
        setFormData({ key: '', value: '', type: 'plain', environment: envFilter || null });
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

    if (loading) return <div className="text-center py-8"><div className="inline-block w-8 h-8 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div></div>;
    if (error) return <p className="text-red-600 dark:text-red-400">Error loading environment variables: {error}</p>;

    const showEnvColumn = !deploymentId && envFilter === '' && environments.length > 0;

    return (
        <div>
            {!deploymentId && (
                <div className="mb-4 flex justify-between items-center">
                    <div className="flex items-center gap-2">
                        {environments.length > 0 && (
                            <select
                                value={envFilter}
                                onChange={(e) => { setEnvFilter(e.target.value); setLoading(true); }}
                                className="mono-select text-sm"
                            >
                                <option value="">All environments</option>
                                {environments.map(env => (
                                    <option key={env.name} value={env.name}>{env.name}</option>
                                ))}
                            </select>
                        )}
                    </div>
                    <Button variant="primary" size="sm" onClick={handleAddClick}>
                        Add Variable
                    </Button>
                </div>
            )}
            <MonoTableFrame>
                <MonoTable>
                    <MonoTableHead>
                        <tr>
                            <MonoTh className="px-6 py-3 text-left">Key</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Value</MonoTh>
                            <MonoTh className="px-6 py-3 text-left">Type</MonoTh>
                            {deploymentId && (
                                <MonoTh className="px-6 py-3 text-left">Source</MonoTh>
                            )}
                            {showEnvColumn && (
                                <MonoTh className="px-6 py-3 text-left">Environment</MonoTh>
                            )}
                            {!deploymentId && (
                                <MonoTh className="px-6 py-3 text-left">Actions</MonoTh>
                            )}
                        </tr>
                    </MonoTableHead>
                    <MonoTableBody>
                        {envVars.length === 0 ? (
                            <MonoTableEmptyRow colSpan={deploymentId ? 4 : (showEnvColumn ? 5 : 4)}>No environment variables configured.</MonoTableEmptyRow>
                        ) : (
                            envVars.map(env => (
                            <MonoTableRow key={`${env.key}-${env.environment || ''}`} interactive className="transition-colors">
                                <MonoTd className="px-6 py-4 text-sm font-mono text-gray-900 dark:text-gray-200">{env.key}</MonoTd>
                                <MonoTd className="px-6 py-4 text-sm font-mono text-gray-900 dark:text-gray-200">
                                    <div className="flex items-center gap-2">
                                        <span>{env.value}</span>
                                        {env.is_secret && !env.is_protected && (
                                            <button
                                                onClick={async () => {
                                                    try {
                                                        const response = deploymentId
                                                            ? await api.getDeploymentEnvVarValue(projectName, deploymentId, env.key)
                                                            : await api.getEnvVarValue(projectName, env.key, env.environment || null);
                                                        await copyToClipboard(response.value);
                                                        showToast('Secret copied to clipboard!', 'success');
                                                    } catch (err) {
                                                        showToast(`Failed to copy secret: ${err.message}`, 'error');
                                                        console.error('Failed to fetch secret:', err);
                                                    }
                                                }}
                                                className="p-1 text-gray-500 hover:text-blue-600 dark:text-gray-400 dark:hover:text-blue-400 transition-colors rounded hover:bg-gray-100 dark:hover:bg-gray-700"
                                                title="Copy secret value to clipboard"
                                            >
                                                <svg
                                                    xmlns="http://www.w3.org/2000/svg"
                                                    className="h-4 w-4"
                                                    fill="none"
                                                    viewBox="0 0 24 24"
                                                    stroke="currentColor"
                                                    strokeWidth={2}
                                                >
                                                    <path
                                                        strokeLinecap="round"
                                                        strokeLinejoin="round"
                                                        d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z"
                                                    />
                                                </svg>
                                            </button>
                                        )}
                                    </div>
                                </MonoTd>
                                <MonoTd className="px-6 py-4 text-sm">
                                    {env.is_protected ? (
                                        <MonoTag color="purple">
                                            <svg xmlns="http://www.w3.org/2000/svg" className="h-3 w-3" viewBox="0 0 20 20" fill="currentColor">
                                                <path fillRule="evenodd" d="M5 9V7a5 5 0 0110 0v2a2 2 0 012 2v5a2 2 0 01-2 2H5a2 2 0 01-2-2v-5a2 2 0 012-2zm8-2v2H7V7a3 3 0 016 0z" clipRule="evenodd" />
                                            </svg>
                                            protected
                                        </MonoTag>
                                    ) : env.is_secret ? (
                                        <MonoTag color="yellow">secret</MonoTag>
                                    ) : (
                                        <MonoTag color="gray">plain</MonoTag>
                                    )}
                                </MonoTd>
                                {deploymentId && (
                                    <MonoTd className="px-6 py-4 text-sm">
                                        <EnvVarSourceTag source={env.source} environments={environments} />
                                    </MonoTd>
                                )}
                                {showEnvColumn && (
                                    <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">
                                        {env.environment ? (
                                            <span className="inline-flex items-center gap-2">
                                                <EnvironmentColorDot color={environments.find(e => e.name === env.environment)?.color} />
                                                {env.environment}
                                            </span>
                                        ) : '(global)'}
                                    </MonoTd>
                                )}
                                {!deploymentId && (
                                    <MonoTd className="px-6 py-4 text-sm">
                                        <div className="flex gap-2">
                                            <Button
                                                variant="secondary"
                                                size="sm"
                                                onClick={() => handleEditClick(env)}
                                            >
                                                Edit
                                            </Button>
                                            <Button
                                                variant="danger"
                                                size="sm"
                                                onClick={() => handleDeleteClick(env)}
                                            >
                                                Delete
                                            </Button>
                                        </div>
                                    </MonoTd>
                                )}
                            </MonoTableRow>
                        ))
                        )}
                    </MonoTableBody>
                </MonoTable>
            </MonoTableFrame>
            {deploymentId ? (
                <p className="mt-4 text-sm text-gray-600 dark:text-gray-500">
                    <strong>Note:</strong> Environment variables are read-only snapshots taken at deployment time.
                    Secret values are always masked for security.
                </p>
            ) : (
                <p className="mt-4 text-sm text-gray-600 dark:text-gray-500">
                    <strong>Note:</strong> Environment variables are snapshots at deployment time.
                    Changes to project variables will only apply to new deployments, not existing ones.
                    Secret values are always masked for security.
                </p>
            )}

            <Modal
                isOpen={isModalOpen}
                onClose={() => setIsModalOpen(false)}
                title={editingEnvVar ? 'Edit Environment Variable' : 'Add Environment Variable'}
            >
                <ModalSection>
                    {!deploymentId && environments.length > 0 && (
                        <div>
                            <span className="mono-label">Environment</span>
                            <EnvironmentDropdown
                                environments={environments}
                                value={formData.environment}
                                onChange={(env) => setFormData({ ...formData, environment: env })}
                            />
                        </div>
                    )}
                    <FormField
                        label="Key"
                        id="env-key"
                        value={formData.key}
                        onChange={(e) => setFormData({ ...formData, key: e.target.value })}
                        placeholder="DATABASE_URL"
                        disabled={editingEnvVar !== null}
                        required
                    />
                    <FormField
                        label="Value"
                        id="env-value"
                        type="textarea"
                        value={formData.value}
                        onChange={(e) => setFormData({ ...formData, value: e.target.value })}
                        placeholder="postgres://..."
                        required
                        rows={3}
                    />
                    <SegmentedRadioGroup
                        label="Type"
                        name="env-var-type"
                        value={formData.type}
                        onChange={(type) => setFormData({ ...formData, type })}
                        ariaLabel="Variable type"
                        options={[
                            { value: 'plain', label: 'PLAIN' },
                            { value: 'secret', label: 'SECRET' },
                            { value: 'protected', label: 'PROTECTED' },
                        ]}
                    />
                    <p className="text-xs text-gray-500 dark:text-gray-400 -mt-2">
                        Protected secrets are write-only and cannot be read back. Secret values can be retrieved for development and CI.
                    </p>

                    <ModalActions>
                        <Button
                            variant="secondary"
                            onClick={() => setIsModalOpen(false)}
                            disabled={saving}
                        >
                            Cancel
                        </Button>
                        <Button
                            variant="primary"
                            onClick={handleSave}
                            loading={saving}
                        >
                            {editingEnvVar ? 'Update' : 'Add'}
                        </Button>
                    </ModalActions>
                </ModalSection>
            </Modal>

            <ConfirmDialog
                isOpen={confirmDialogOpen}
                onClose={() => {
                    setConfirmDialogOpen(false);
                    setEnvVarToDelete(null);
                }}
                onConfirm={handleDeleteConfirm}
                title="Delete Environment Variable"
                message={`Are you sure you want to delete the environment variable "${envVarToDelete?.key}"? This action cannot be undone.`}
                confirmText="Delete Variable"
                variant="danger"
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

    if (loading) return <div className="text-center py-8"><div className="inline-block w-8 h-8 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div></div>;
    if (error) return <p className="text-red-600 dark:text-red-400">Error loading extensions: {error}</p>;

    return (
        <div className="space-y-6">
            {/* Enabled Extensions - Table with Add Icons */}
            <div>
                <div className="flex items-center justify-between gap-3 mb-3">
                    {availableExtensions.length > 0 && (
                        <div className="flex items-center gap-2 ml-auto">
                            {/* Plus icon indicator (non-clickable) */}
                            <div className="w-8 h-8 flex items-center justify-center text-gray-600 dark:text-gray-500">
                                <div className="w-5 h-5 svg-mask" style={{
                                    maskImage: 'url(/assets/plus.svg)',
                                    WebkitMaskImage: 'url(/assets/plus.svg)'
                                }}></div>
                            </div>
                            {/* Extension add icons */}
                            {availableExtensions
                                .sort((a, b) => a.display_name.localeCompare(b.display_name))
                                .map(extType => {
                                    const iconUrl = getExtensionIcon(extType.extension_type);

                                    return (
                                        <button
                                            key={extType.extension_type}
                                            onClick={() => {
                                                navigate(`/project/${projectName}/extensions/${extType.extension_type}/@new`);
                                            }}
                                            className="mono-extension-create-button w-8 h-8 flex items-center justify-center bg-gray-100 dark:bg-gray-800 hover:bg-gray-100 dark:hover:bg-gray-700 border border-gray-300 dark:border-gray-700 hover:border-indigo-500 transition-all"
                                            title={`Add ${extType.display_name}`}
                                        >
                                            {iconUrl ? (
                                                <img
                                                    src={iconUrl}
                                                    alt={extType.display_name}
                                                    className="w-6 h-6 object-contain"
                                                />
                                            ) : (
                                                <div className="w-6 h-6 flex items-center justify-center text-gray-600 dark:text-gray-400 text-xs font-bold">
                                                    {extType.display_name.charAt(0).toUpperCase()}
                                                </div>
                                            )}
                                        </button>
                                    );
                                })
                            }
                        </div>
                    )}
                </div>
                {enabledExtensions.length === 0 ? (
                    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 px-6 py-8 text-center">
                        <p className="text-gray-600 dark:text-gray-400 text-sm">
                            No extensions enabled yet. Click an extension icon to add one.
                        </p>
                    </div>
                ) : (
                    <MonoTableFrame>
                        <MonoTable>
                            <MonoTableHead>
                                <tr>
                                    <MonoTh className="w-12 px-3 py-3"></MonoTh>
                                    <MonoTh className="px-6 py-3 text-left">Extension</MonoTh>
                                    <MonoTh className="px-6 py-3 text-left">Type</MonoTh>
                                    <MonoTh className="px-6 py-3 text-left">Status</MonoTh>
                                    <MonoTh className="px-6 py-3 text-left">Updated</MonoTh>
                                </tr>
                            </MonoTableHead>
                            <MonoTableBody>
                                {enabledExtensions
                                    .sort((a, b) => a.extension.localeCompare(b.extension))
                                    .map(ext => {
                                        const extType = availableExtensions.find(t => t.extension_type === ext.extension_type);
                                        const iconUrl = getExtensionIcon(ext.extension_type);

                                        return (
                                            <MonoTableRow
                                                key={ext.extension}
                                                interactive
                                                className="transition-colors cursor-pointer"
                                                onClick={() => {
                                                    // Navigate to the specific extension instance
                                                    navigate(`/project/${projectName}/extensions/${ext.extension_type}/${ext.extension}`);
                                                }}
                                            >
                                                <MonoTd className="px-3 py-4">
                                                    {iconUrl ? (
                                                        <img
                                                            src={iconUrl}
                                                            alt={ext.extension}
                                                            className="w-8 h-8 rounded object-contain"
                                                        />
                                                    ) : (
                                                        <div className="w-8 h-8"></div>
                                                    )}
                                                </MonoTd>
                                                <MonoTd className="px-6 py-4 text-sm">
                                                    <span className="font-mono text-gray-900 dark:text-gray-200">{ext.extension}</span>
                                                </MonoTd>
                                                <MonoTd className="px-6 py-4 text-sm text-gray-700 dark:text-gray-300">
                                                    {extType?.description || ext.extension_type}
                                                </MonoTd>
                                                <MonoTd className="px-6 py-4 text-sm">
                                                    {renderExtensionStatusBadge(ext)}
                                                </MonoTd>
                                                <MonoTd className="px-6 py-4 text-sm text-gray-600 dark:text-gray-400">
                                                    {formatDate(ext.updated)}
                                                </MonoTd>
                                            </MonoTableRow>
                                        );
                                    })}
                            </MonoTableBody>
                        </MonoTable>
                    </MonoTableFrame>
                )}
            </div>

            {/* Configuration Modal */}
            <Modal
                isOpen={isConfigModalOpen}
                onClose={() => setIsConfigModalOpen(false)}
                title={editMode ? `Extension: ${selectedExtension?.name}` : `Enable Extension: ${selectedExtension?.name}`}
                maxWidth="max-w-4xl"
            >
                <ModalSection>
                    {selectedExtension && (
                        <>
                            {/* Tab Navigation */}
                            <ModalTabs className="px-2">
                                    {hasExtensionUI(selectedExtension.extension_type) && (
                                        <MonoTabButton className="mr-4" active={modalTab === 'ui'} onClick={() => setModalTab('ui')}>
                                            Configure
                                        </MonoTabButton>
                                    )}
                                    <MonoTabButton className="mr-4" active={modalTab === 'config'} onClick={() => setModalTab('config')}>
                                        {hasExtensionUI(selectedExtension.extension_type) ? 'JSON' : 'Configuration'}
                                    </MonoTabButton>
                                    <MonoTabButton className="mr-4" active={modalTab === 'schema'} onClick={() => setModalTab('schema')}>
                                        Schema
                                    </MonoTabButton>
                                    {editMode && selectedExtensionData && (
                                        <>
                                            <MonoTabButton className="mr-4" active={modalTab === 'status'} onClick={() => setModalTab('status')}>
                                                Status
                                            </MonoTabButton>
                                            <MonoTabButton tone="danger" active={modalTab === 'delete'} onClick={() => setModalTab('delete')}>
                                                Delete
                                            </MonoTabButton>
                                        </>
                                    )}
                            </ModalTabs>

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
                                    <FormField
                                        label="Configuration Spec (JSON)"
                                        id="extension-spec"
                                        type="textarea"
                                        value={formData.spec}
                                        onChange={(e) => handleJsonSpecChange(e.target.value)}
                                        placeholder="{}"
                                        required
                                        rows={15}
                                    />
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

                                    <FormField
                                        label={`Type "${selectedExtensionData.extension}" to confirm deletion`}
                                        id="delete-confirm-name"
                                        value={deleteConfirmName}
                                        onChange={(e) => setDeleteConfirmName(e.target.value)}
                                        placeholder={selectedExtensionData.extension}
                                        required
                                    />

                                    <div className="flex justify-end gap-3 pt-4">
                                        <Button
                                            variant="secondary"
                                            onClick={() => setModalTab(hasExtensionUI(selectedExtension.extension_type) ? 'ui' : 'config')}
                                            disabled={deleting}
                                        >
                                            Cancel
                                        </Button>
                                        <Button
                                            variant="danger"
                                            onClick={handleDelete}
                                            loading={deleting}
                                            disabled={deleteConfirmName !== selectedExtensionData.extension}
                                        >
                                            Delete Extension
                                        </Button>
                                    </div>
                                </div>
                            )}

                            {/* Action buttons - only shown when not on delete tab */}
                            {modalTab !== 'delete' && (
                                <div className="flex justify-end gap-3 pt-4">
                                    <Button
                                        variant="secondary"
                                        onClick={() => setIsConfigModalOpen(false)}
                                        disabled={saving}
                                    >
                                        Cancel
                                    </Button>
                                    <Button
                                        variant="primary"
                                        onClick={handleSave}
                                        loading={saving}
                                    >
                                        {editMode ? 'Update' : 'Enable'}
                                    </Button>
                                </div>
                            )}
                        </>
                    )}
                </ModalSection>
            </Modal>

        </div>
    );
}

// Helper function to render status badges with color coding
function renderExtensionStatusBadge(extension) {
    // Show pending deletion badge when extension is marked for deletion
    if (extension.deleted) {
        const state = (extension.status?.state || '').toLowerCase();
        if (state === 'deletion_blocked') {
            return <MonoStatusPill tone="warn">deletion blocked</MonoStatusPill>;
        }
        return <MonoStatusPill tone="muted">{state === 'deleting' ? 'deleting' : 'pending deletion'}</MonoStatusPill>;
    }

    // Check if extension has custom status badge renderer
    const customRenderer = getExtensionStatusBadge(extension.extension_type);
    if (customRenderer) {
        const customBadge = customRenderer(extension);
        if (customBadge) return customBadge;
    }

    // Fallback to generic status badge using status_summary
    let badgeTone = 'muted';
    let statusText = extension.status_summary || 'Unknown';

    // Parse status JSON to determine color and text
    if (extension.status && extension.status.state) {
        const state = extension.status.state.toLowerCase();
        statusText = extension.status.state; // Use the original state value as the text

        switch (state) {
            case 'available':
                badgeTone = 'ok';
                break;
            case 'creating':
            case 'pending':
                badgeTone = 'warn';
                break;
            case 'failed':
                badgeTone = 'bad';
                break;
            case 'deleting':
            case 'deleted':
                badgeTone = 'muted';
                break;
            case 'deletion_blocked':
                badgeTone = 'warn';
                break;
            default:
                badgeTone = 'muted';
        }
    }

    return <MonoStatusPill tone={badgeTone}>{statusText}</MonoStatusPill>;
}

// Generic Extension Detail View (fallback for extensions without custom UI)
function GenericExtensionDetailView({ extension }) {
    return (
        <div className="space-y-6">
            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Status</h2>
                <div className="bg-white dark:bg-gray-900 rounded p-4">
                    <p className="text-gray-700 dark:text-gray-300">{extension.status_summary}</p>

                    {extension.status && extension.status.error && (
                        <div className="mt-3 p-3 bg-red-900/20 border border-red-700 rounded">
                            <p className="text-sm text-red-300">
                                <strong>Error:</strong> {extension.status.error}
                            </p>
                        </div>
                    )}
                </div>
            </section>

            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Configuration</h2>
                <pre className="bg-white dark:bg-gray-900 rounded p-4 overflow-x-auto">
                    <code className="text-sm text-gray-700 dark:text-gray-300">
                        {JSON.stringify(extension.spec, null, 2)}
                    </code>
                </pre>
            </section>

            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Full Status</h2>
                <pre className="bg-white dark:bg-gray-900 rounded p-4 overflow-x-auto">
                    <code className="text-sm text-gray-700 dark:text-gray-300">
                        {JSON.stringify(extension.status, null, 2)}
                    </code>
                </pre>
            </section>

            <section>
                <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-200 mb-3">Metadata</h2>
                <div className="bg-white dark:bg-gray-900 rounded p-4 space-y-2">
                    <p className="text-sm text-gray-700 dark:text-gray-300">
                        <span className="text-gray-600 dark:text-gray-500">Created:</span> {formatDate(extension.created)}
                    </p>
                    <p className="text-sm text-gray-700 dark:text-gray-300">
                        <span className="text-gray-600 dark:text-gray-500">Updated:</span> {formatDate(extension.updated)}
                    </p>
                </div>
            </section>
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
    const [deleteConfirmName, setDeleteConfirmName] = useState('');
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
        if (!enabledExtension || deleteConfirmName !== enabledExtension.extension) {
            showToast('Please enter the extension instance name to confirm deletion', 'error');
            return;
        }

        setDeleting(true);
        try {
            await api.deleteExtension(projectName, enabledExtension.extension);
            showToast(`Extension ${enabledExtension.extension} deleted successfully`, 'success');
            navigate(`/project/${projectName}/extensions`);
        } catch (err) {
            showToast(`Failed to delete extension: ${err.message}`, 'error');
        } finally {
            setDeleting(false);
        }
    };

    if (loading) {
        return (
            <div className="flex items-center justify-center min-h-[400px]">
                <div className="w-12 h-12 border-4 border-indigo-600 border-t-transparent rounded-full animate-spin"></div>
            </div>
        );
    }

    if (error || !extensionType) {
        return (
            <div className="max-w-7xl mx-auto">
                <div className="bg-red-900/20 border border-red-700 rounded-lg p-6">
                    <h2 className="text-lg font-semibold text-red-300 mb-2">Error</h2>
                    <p className="text-red-200">{error || 'Extension type not found'}</p>
                </div>
            </div>
        );
    }

    const CustomDetailView = getExtensionDetailView(extensionType.extension_type);

    return (
        <div className="max-w-7xl mx-auto">
            <div className="bg-gray-100 dark:bg-gray-800 rounded-lg shadow-xl p-6">
                <div className="flex items-center justify-between mb-6">
                    <div className="flex items-center space-x-4">
                        {getExtensionIcon(extensionType.extension_type) && (
                            <img
                                src={getExtensionIcon(extensionType.extension_type)}
                                alt={extensionType.display_name}
                                className="w-12 h-12 rounded object-contain"
                            />
                        )}
                        <div>
                            <h1 className="text-2xl font-bold text-gray-900 dark:text-white">
                                {extensionType.display_name}
                            </h1>
                            <p className="text-gray-600 dark:text-gray-400">{extensionType.description}</p>
                        </div>
                    </div>
                    <div className="flex items-center gap-3">
                        <Button
                            variant="secondary"
                            size="sm"
                            onClick={() => navigate(extensionDocsHref(extensionType.extension_type))}
                        >
                            Extension Docs
                        </Button>
                        {isEnabled ? (
                            renderExtensionStatusBadge(enabledExtension)
                        ) : (
                            <MonoStatusPill tone="muted">Not Enabled</MonoStatusPill>
                        )}
                    </div>
                </div>

                {isPreviewOnly && (
                    <div className="mb-6 rounded-lg border border-amber-400/40 bg-amber-50 px-4 py-3 text-sm text-amber-900 dark:border-amber-500/40 dark:bg-amber-900/20 dark:text-amber-100">
                        <div className="flex items-center justify-between gap-3">
                            <span>Preview mode: this extension UI is available for configuration testing only. Install the backend provider to enable create and update actions.</span>
                            <Button
                                variant="secondary"
                                size="sm"
                                onClick={() => disablePreviewExtensions(projectName)}
                            >
                                Disable Preview Mode
                            </Button>
                        </div>
                    </div>
                )}

                {/* Tab Navigation */}
                <div className="border-b border-gray-300 dark:border-gray-700 mb-6">
                    <div className="flex gap-6">
                        {/* Left-aligned: Extension-specific tabs */}
                        {isEnabled && (
                            <MonoTabButton active={activeTab === 'overview'} onClick={() => setActiveTab('overview')}>
                                Overview
                            </MonoTabButton>
                        )}
                        {hasExtensionUI(extensionType.extension_type) && (
                            <MonoTabButton active={activeTab === 'configure'} onClick={() => setActiveTab('configure')}>
                                Configure{hasUnsavedChanges && ' *'}
                            </MonoTabButton>
                        )}

                        {/* Spacer to push common tabs to the right */}
                        <div className="flex-1"></div>

                        {/* Right-aligned: Common tabs */}
                        <MonoTabButton active={activeTab === 'config'} onClick={() => setActiveTab('config')}>
                            Spec{hasUnsavedChanges && ' *'}
                        </MonoTabButton>
                        {isEnabled && (
                            <MonoTabButton active={activeTab === 'status'} onClick={() => setActiveTab('status')}>
                                Status
                            </MonoTabButton>
                        )}
                        <MonoTabButton active={activeTab === 'schema'} onClick={() => setActiveTab('schema')}>
                            Schema
                        </MonoTabButton>
                        {isEnabled && (
                            <MonoTabButton tone="danger" active={activeTab === 'delete'} onClick={() => setActiveTab('delete')}>
                                Delete
                            </MonoTabButton>
                        )}
                    </div>
                </div>

                {/* Tab Content */}
                {activeTab === 'overview' && isEnabled && CustomDetailView && (
                    <CustomDetailView extension={enabledExtension} projectName={projectName} />
                )}

                {activeTab === 'overview' && isEnabled && !CustomDetailView && (
                    <GenericExtensionDetailView extension={enabledExtension} />
                )}

                {activeTab === 'configure' && extensionAPI?.renderConfigureTab && (
                    <div className="space-y-4">
                        {!isEnabled && (
                            <div className="pb-4 border-b border-gray-300 dark:border-gray-700">
                                <FormField
                                    label="Extension Name"
                                    id="extension-instance-name"
                                    value={instanceName}
                                    onChange={(e) => setInstanceName(e.target.value)}
                                    placeholder={getDefaultExtensionName(extensionTypeProp)}
                                    required
                                />
                                <p className="text-xs text-gray-600 dark:text-gray-500 mt-2">
                                    Give this extension instance a unique name. You can create multiple instances of the same extension type with different names.
                                </p>
                            </div>
                        )}
                        {extensionAPI.renderConfigureTab(uiSpec, extensionType.spec_schema, handleUiSpecChange, projectName, instanceName, isEnabled)}
                        <div className="flex justify-end gap-3 pt-4 border-t border-gray-300 dark:border-gray-700">
                            <Button
                                variant="primary"
                                onClick={handleSave}
                                loading={saving}
                                disabled={isPreviewOnly}
                                className={!isEnabled ? 'mono-btn-cta' : ''}
                            >
                                {isPreviewOnly ? 'Preview Only' : (isEnabled ? 'Update' : 'Enable')}
                            </Button>
                        </div>
                    </div>
                )}

                {activeTab === 'config' && (
                    <div className="space-y-4">
                        {!isEnabled && (
                            <div className="pb-4 border-b border-gray-300 dark:border-gray-700">
                                <FormField
                                    label="Extension Name"
                                    id="extension-instance-name-config"
                                    value={instanceName}
                                    onChange={(e) => setInstanceName(e.target.value)}
                                    placeholder={getDefaultExtensionName(extensionTypeProp)}
                                    required
                                />
                                <p className="text-xs text-gray-600 dark:text-gray-500 mt-2">
                                    Give this extension instance a unique name. You can create multiple instances of the same extension type with different names.
                                </p>
                            </div>
                        )}
                        <FormField
                            label="Configuration Spec (JSON)"
                            id="extension-spec"
                            type="textarea"
                            value={formData.spec}
                            onChange={(e) => handleJsonSpecChange(e.target.value)}
                            placeholder="{}"
                            required
                            rows={15}
                        />
                        <p className="text-sm text-gray-600 dark:text-gray-500">
                            Enter the extension configuration as a JSON object. See the Schema tab and Project Extensions docs for valid fields and examples.
                            {hasExtensionUI(extensionType.extension_type) && <span> Use the Configure tab for a form-based interface.</span>}
                        </p>
                        <p className="text-sm text-gray-600 dark:text-gray-500">
                            Extension docs: <a href={extensionDocsHref(extensionType.extension_type)} onClick={(e) => {
                                e.preventDefault();
                                navigate(extensionDocsHref(extensionType.extension_type));
                            }} className="underline">Open {extensionType.extension_type} documentation</a>
                        </p>
                        <div className="flex justify-end gap-3 pt-4 border-t border-gray-300 dark:border-gray-700">
                            <Button
                                variant="primary"
                                onClick={handleSave}
                                loading={saving}
                                disabled={isPreviewOnly}
                                className={!isEnabled ? 'mono-btn-cta' : ''}
                            >
                                {isPreviewOnly ? 'Preview Only' : (isEnabled ? 'Update' : 'Enable')}
                            </Button>
                        </div>
                    </div>
                )}

                {activeTab === 'status' && isEnabled && (
                    <div className="space-y-4">
                        <div>
                            <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Status Summary</h4>
                            <p className="text-gray-900 dark:text-gray-200">{enabledExtension.status_summary}</p>
                        </div>

                        <div>
                            <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Current Spec</h4>
                            <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-white dark:bg-gray-900 p-3 rounded overflow-x-auto">
                                {JSON.stringify(enabledExtension.spec, null, 2)}
                            </pre>
                        </div>

                        <div>
                            <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300 mb-2">Full Status</h4>
                            <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-white dark:bg-gray-900 p-3 rounded overflow-x-auto max-h-96">
                                {JSON.stringify(enabledExtension.status, null, 2)}
                            </pre>
                        </div>

                        <div className="text-xs text-gray-600 dark:text-gray-500">
                            <p>Created: {formatDate(enabledExtension.created)}</p>
                            <p>Updated: {formatDate(enabledExtension.updated)}</p>
                        </div>
                    </div>
                )}

                {activeTab === 'schema' && (
                    <div className="space-y-4">
                        <h4 className="text-sm font-semibold text-gray-700 dark:text-gray-300">Schema</h4>
                        <pre className="text-xs font-mono text-gray-700 dark:text-gray-300 bg-white dark:bg-gray-900 p-4 rounded overflow-x-auto max-h-96">
                            {JSON.stringify(extensionType.spec_schema, null, 2)}
                        </pre>
                        <p className="text-sm text-gray-600 dark:text-gray-500">
                            This JSON schema defines the valid structure for the extension configuration.
                        </p>
                    </div>
                )}

                {activeTab === 'delete' && isEnabled && (
                    <div className="space-y-4">
                        <div className="bg-red-900/20 border border-red-700 rounded-lg p-4">
                            <h4 className="text-sm font-semibold text-red-300 mb-2">Warning: Permanent Deletion</h4>
                            <p className="text-sm text-red-200">
                                Deleting this extension will deprovision all resources created by this extension.
                                This action cannot be undone.
                            </p>
                        </div>

                        <FormField
                            label={`Type the extension instance name "${enabledExtension.extension}" to confirm deletion`}
                            id="delete-confirm-name"
                            value={deleteConfirmName}
                            onChange={(e) => setDeleteConfirmName(e.target.value)}
                            placeholder={enabledExtension.extension}
                            required
                        />

                        <div className="flex justify-end gap-3 pt-4 border-t border-gray-300 dark:border-gray-700">
                            <Button
                                variant="secondary"
                                onClick={() => setActiveTab('overview')}
                                disabled={deleting}
                            >
                                Cancel
                            </Button>
                            <Button
                                variant="danger"
                                onClick={handleDelete}
                                loading={deleting}
                                disabled={deleteConfirmName !== enabledExtension.extension}
                            >
                                Delete Extension
                            </Button>
                        </div>
                    </div>
                )}
            </div>
        </div>
    );
}
