import React, { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { useToast } from '../components/toast';
import { Alert, Button, Combobox, Field, Input, Modal, Panel, PanelHead } from '../components/r-ui';
import { Icon } from '../components/icon';

export interface QuickstartTemplate {
    id: string;
    display_name: string;
    tagline: string;
    description: string;
    icon_url: string;
    image: string;
    http_port: number;
    learn_more_url: string;
    tags?: string[];
    warning?: string;
}

// Default suggestion for the project name: template id, lightly randomized so
// repeated deploys don't collide. Lowercase + hyphens to satisfy the API.
function suggestProjectName(template: QuickstartTemplate): string {
    const suffix = Math.random().toString(36).slice(2, 6);
    return `${template.id}-${suffix}`;
}

export function useQuickstartTemplates() {
    const [templates, setTemplates] = useState<QuickstartTemplate[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    useEffect(() => {
        api.getQuickstartTemplates()
            .then(d => setTemplates(d?.templates || []))
            .catch(err => setError(err.message));
    }, []);
    return { templates, error };
}

// Read-only properties of the deployment platform. Capability flags sit flat
// at the top level under namespaced keys (e.g. `deployments:canRunAsRoot`).
export interface PlatformCapabilities {
    runtime_arch: string | null;
    'deployments:canRunAsRoot': boolean;
}

export function usePlatformCapabilities() {
    const [caps, setCaps] = useState<PlatformCapabilities | null>(null);
    useEffect(() => {
        api.getPlatformCapabilities()
            .then(c => setCaps(c as PlatformCapabilities))
            .catch(() => { /* permissive default applied in api client */ });
    }, []);
    return caps;
}

export function QuickstartCard({ template, onSelect }: { template: QuickstartTemplate; onSelect: () => void }) {
    return (
        <button
            type="button"
            onClick={onSelect}
            className="r-quickstart-card"
            aria-label={`Deploy ${template.display_name} quickstart`}
        >
            <img src={template.icon_url} alt="" className="r-quickstart-card__icon" />
            <div className="r-quickstart-card__body">
                <div className="r-quickstart-card__title">{template.display_name}</div>
                <div className="r-quickstart-card__tagline">{template.tagline}</div>
            </div>
        </button>
    );
}

export function QuickstartGrid({ templates, onSelect, limit }: { templates: QuickstartTemplate[]; onSelect: (t: QuickstartTemplate) => void; limit?: number }) {
    const shown = limit ? templates.slice(0, limit) : templates;
    return (
        <div className="r-quickstart-grid">
            {shown.map(t => (
                <QuickstartCard key={t.id} template={t} onSelect={() => onSelect(t)} />
            ))}
        </div>
    );
}

interface DeployModalProps {
    template: QuickstartTemplate | null;
    onClose: () => void;
    onDeployed?: (projectName: string) => void;
}

// Modal that, given a selected template, lets the user pick a project name +
// access class + owner and then chains createProject -> createDeployment.
export function QuickstartDeployModal({ template, onClose, onDeployed }: DeployModalProps) {
    const { showToast } = useToast();
    const [accessClasses, setAccessClasses] = useState<any[]>([]);
    const [teams, setTeams] = useState<any[]>([]);
    const [currentUser, setCurrentUser] = useState<any>(null);
    const [formData, setFormData] = useState<{ name: string; access_class: string; owner: string }>({ name: '', access_class: 'public', owner: 'self' });
    const [saving, setSaving] = useState(false);
    const caps = usePlatformCapabilities();

    useEffect(() => {
        if (!template) return;
        setFormData({ name: suggestProjectName(template), access_class: 'public', owner: 'self' });
    }, [template]);

    useEffect(() => {
        api.getTeams().then(setTeams).catch(() => {});
        api.getMe().then(setCurrentUser).catch(() => {});
        api.getAccessClasses().then(d => setAccessClasses(d?.access_classes || [])).catch(() => {});
    }, []);

    if (!template) return null;

    // Capability-derived hint: when the platform runs apps as non-root, dropped
    // capabilities mean a privileged port (< 1024) can't be bound and the app
    // will crash-loop. This is computed client-side from the platform
    // capability rather than baked into each template's `warning`.
    const canRunAsRoot = caps?.['deployments:canRunAsRoot'] ?? true;
    const privilegedPortRisk = !canRunAsRoot && template.http_port < 1024;

    const handleDeploy = async () => {
        if (!formData.name) { showToast('Project name is required', 'error'); return; }
        if (!/^[a-z0-9-]+$/.test(formData.name)) {
            showToast('Project name must contain only lowercase letters, numbers, and hyphens', 'error');
            return;
        }
        if (!currentUser) { showToast('Unable to determine current user', 'error'); return; }
        setSaving(true);
        try {
            const owner = formData.owner === 'self' ? { user: currentUser.id } : { team: formData.owner };
            await api.createProject(formData.name, formData.access_class, owner, { id: template.id, image: template.image });
            await api.createDeploymentFromImage(formData.name, template.image, template.http_port);
            showToast(`Deploying ${template.display_name} to ${formData.name}…`, 'success');
            window.dispatchEvent(new Event('rise:mutation'));
            onDeployed?.(formData.name);
            navigate(`/project/${formData.name}`);
            onClose();
        } catch (err: any) {
            showToast(`Failed to deploy quickstart: ${err.message}`, 'error');
        } finally {
            setSaving(false);
        }
    };

    return (
        <Modal
            isOpen={true}
            onClose={onClose}
            width="wide"
            title={
                <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                    <img src={template.icon_url} alt="" width={32} height={32} style={{ borderRadius: 6 }} />
                    <span>Deploy {template.display_name}</span>
                </div>
            }
            sub={template.tagline}
            footer={
                <>
                    <Button onClick={onClose} disabled={saving}>Cancel</Button>
                    <Button variant="primary" loading={saving} icon="rocket" onClick={handleDeploy}>
                        Create project &amp; deploy
                    </Button>
                </>
            }
        >
            <p style={{ margin: '0 0 14px', fontSize: 13, color: 'var(--text-muted)', lineHeight: 1.55 }}>
                {template.description}
            </p>
            {template.warning && (
                <div style={{ marginBottom: 14 }}>
                    <Alert tone="warn" icon="info">{template.warning}</Alert>
                </div>
            )}
            {privilegedPortRisk && (
                <div style={{ marginBottom: 14 }}>
                    <Alert tone="warn" icon="info">
                        This platform runs apps as a non-root user, so binding port {template.http_port} (below 1024) may fail. Prefer an image that listens on a port ≥ 1024.
                    </Alert>
                </div>
            )}
            <div className="r-quickstart-meta">
                <div>
                    <div className="r-quickstart-meta__label">Image</div>
                    <code className="r-quickstart-meta__value mono">{template.image}</code>
                </div>
                <div>
                    <div className="r-quickstart-meta__label">Port</div>
                    <code className="r-quickstart-meta__value mono">{template.http_port}</code>
                </div>
                <div>
                    <div className="r-quickstart-meta__label">Upstream</div>
                    <a className="r-link" href={template.learn_more_url} target="_blank" rel="noopener noreferrer">
                        Learn more <Icon name="ext" size={11} />
                    </a>
                </div>
            </div>
            <div style={{ height: 12 }} />
            <Field label="Project name" hint="Becomes the subdomain. Only lowercase letters, numbers, and hyphens.">
                <Input
                    placeholder="my-app"
                    value={formData.name}
                    onChange={e => setFormData({ ...formData, name: e.target.value.toLowerCase() })}
                    autoFocus
                />
            </Field>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 14, marginTop: 14 }}>
                <Field
                    label="Access class"
                    hint={accessClasses.find(a => a.id === formData.access_class)?.description}
                >
                    <Combobox
                        value={formData.access_class}
                        onChange={(v) => setFormData({ ...formData, access_class: v })}
                        options={accessClasses.map(ac => ({ value: ac.id, label: ac.display_name, hint: ac.description }))}
                        placeholder="Select access class"
                    />
                </Field>
                <Field label="Owner">
                    <Combobox
                        value={formData.owner}
                        onChange={(v) => setFormData({ ...formData, owner: v })}
                        options={[
                            {
                                value: 'self',
                                label: `You (${currentUser?.email || 'me'})`,
                                icon: <Icon name="user" size={13} />,
                                keywords: 'me self you',
                            },
                            ...teams.map(t => ({
                                value: t.id,
                                label: t.name,
                                icon: <Icon name="users" size={13} />,
                                keywords: `team ${t.name}`,
                            })),
                        ]}
                        placeholder="Select owner"
                    />
                </Field>
            </div>
        </Modal>
    );
}

// Picker modal: lists every available template in a grid and forwards the
// selection to the caller (typically opening the deploy modal next).
export function QuickstartPickerModal({ open, onClose, onSelect }: { open: boolean; onClose: () => void; onSelect: (t: QuickstartTemplate) => void }) {
    const { templates, error } = useQuickstartTemplates();
    return (
        <Modal
            isOpen={open}
            onClose={onClose}
            width="wide"
            title="Choose a template"
            sub="Curated stateless apps you can deploy in one click."
            footer={<Button onClick={onClose}>Cancel</Button>}
        >
            {error && <Alert tone="err" icon="info">Failed to load templates: {error}</Alert>}
            {!error && !templates && <div style={{ padding: 12, color: 'var(--text-muted)', fontSize: 13 }}>Loading templates…</div>}
            {templates && templates.length === 0 && (
                <div style={{ padding: 12, color: 'var(--text-muted)', fontSize: 13 }}>No templates available.</div>
            )}
            {templates && templates.length > 0 && (
                <QuickstartGrid templates={templates} onSelect={(t) => { onSelect(t); onClose(); }} />
            )}
        </Modal>
    );
}

// Self-contained panel for the home dashboard. Shows up to `limit` templates
// (default 2) followed by a "Show all" button that opens the picker modal.
// Clicking a card or picking from the modal opens the deploy modal.
export function QuickstartPanel({ limit = 2 }: { limit?: number }) {
    const { templates, error } = useQuickstartTemplates();
    const [selected, setSelected] = useState<QuickstartTemplate | null>(null);
    const [pickerOpen, setPickerOpen] = useState(false);

    if (error || !templates || templates.length === 0) return null;

    const hasMore = templates.length > limit;

    return (
        <>
            <Panel>
                <PanelHead
                    title="Deploy a quickstart"
                    sub="Stateless apps ready to run on Rise in one click."
                />
                <div style={{ padding: '4px 12px 14px', display: 'flex', flexDirection: 'column', gap: 10 }}>
                    <QuickstartGrid templates={templates} onSelect={setSelected} limit={limit} />
                    <Button onClick={() => setPickerOpen(true)} icon={hasMore ? 'arrow' : 'plus'}>
                        {hasMore ? `Show all ${templates.length} templates` : 'Browse all templates'}
                    </Button>
                </div>
            </Panel>
            <QuickstartPickerModal
                open={pickerOpen}
                onClose={() => setPickerOpen(false)}
                onSelect={setSelected}
            />
            <QuickstartDeployModal
                template={selected}
                onClose={() => setSelected(null)}
            />
        </>
    );
}
