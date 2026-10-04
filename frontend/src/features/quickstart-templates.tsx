import React, { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { Button, Panel, PanelHead } from '../components/r-ui';

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
export function suggestProjectName(template: QuickstartTemplate): string {
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
// at the top level (e.g. `runtime_allows_root`).
export interface PlatformCapabilities {
    runtime_arch: string | null;
    runtime_allows_root: boolean;
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

// Self-contained panel for the home dashboard. Shows up to `limit` templates
// (default 2) followed by a "Show all" button that opens the picker modal.
// Clicking a card or picking from the modal opens the deploy modal.
export function QuickstartPanel({ limit = 2 }: { limit?: number }) {
    const { templates, error } = useQuickstartTemplates();

    if (error || !templates || templates.length === 0) return null;

    const hasMore = templates.length > limit;

    return (
        <Panel>
            <PanelHead
                title="Deploy a quickstart"
                sub="Stateless apps ready to run on Rise in one click."
            />
            <div style={{ padding: '4px 12px 14px', display: 'flex', flexDirection: 'column', gap: 10 }}>
                <QuickstartGrid templates={templates} onSelect={t => navigate(`/projects/new?template=${encodeURIComponent(t.id)}`)} limit={limit} />
                <Button onClick={() => navigate('/projects/new')} icon={hasMore ? 'arrow' : 'plus'}>
                    {hasMore ? `Show all ${templates.length} templates` : 'Browse all templates'}
                </Button>
            </div>
        </Panel>
    );
}
