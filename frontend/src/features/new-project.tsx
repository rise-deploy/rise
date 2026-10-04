import React, { useEffect, useState } from 'react';
import { api } from '../lib/api';
import { navigate } from '../lib/navigation';
import { projectPath } from '../lib/routes';
import { useToast } from '../components/toast';
import { Icon } from '../components/icon';
import { Alert, Button, Panel, cx } from '../components/r-ui';
import { suggestProjectName, usePlatformCapabilities, useQuickstartTemplates, type QuickstartTemplate } from './quickstart-templates';

// A project is created in three steps: what it runs (empty, an existing image,
// or a catalog template), what it's called and who owns it, and which
// environments it starts with. Only image and template projects deploy right
// away — an empty project waits for `rise deploy`, which builds locally.

type SourceKind = 'empty' | 'image' | 'template';

interface AccessClass { id: string; display_name: string; description?: string }
interface TeamSummary { id: string; name: string }
interface CurrentUser { id: string; email: string }

// DNS-1123 label, as the backend validates (`validate_project_name`).
const NAME_RE = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

function nameError(name: string, taken: Set<string>): string | null {
    if (!name) return null;
    if (!/^[a-z0-9-]*$/.test(name)) return 'Only lowercase letters, digits and hyphens.';
    if (!NAME_RE.test(name)) return 'Must start and end with a letter or digit, at most 63 characters.';
    if (taken.has(name)) return 'A project with this name already exists.';
    return null;
}

const STEPS = ['Source', 'Configure', 'Environments'];

export function NewProjectPage() {
    const { templates } = useQuickstartTemplates();
    const caps = usePlatformCapabilities();
    const { showToast } = useToast();
    const [step, setStep] = useState(1);
    const [kind, setKind] = useState<SourceKind>('empty');
    const [template, setTemplate] = useState<QuickstartTemplate | null>(null);
    const [image, setImage] = useState('');
    const [port, setPort] = useState('8080');
    const [name, setName] = useState('');
    const [owner, setOwner] = useState('self');
    const [access, setAccess] = useState('');
    const [staging, setStaging] = useState(false);
    const [previews, setPreviews] = useState(false);
    const [accessClasses, setAccessClasses] = useState<AccessClass[]>([]);
    const [teams, setTeams] = useState<TeamSummary[]>([]);
    const [me, setMe] = useState<CurrentUser | null>(null);
    const [taken, setTaken] = useState<Set<string>>(new Set());
    const [creating, setCreating] = useState(false);
    const [created, setCreated] = useState<string | null>(null);

    useEffect(() => {
        api.getAccessClasses().then((d: { access_classes?: AccessClass[] }) => {
            const list = d?.access_classes || [];
            setAccessClasses(list);
            setAccess(a => a || list[0]?.id || '');
        }).catch(() => {});
        api.getTeams().then((t: TeamSummary[]) => setTeams(t || [])).catch(() => {});
        api.getMe().then(setMe).catch(() => {});
        api.getProjects().then((p: { name: string }[]) => setTaken(new Set((p || []).map(x => x.name)))).catch(() => {});
    }, []);

    // `?template=<id>` (from the Home quickstart panel) preselects a template.
    useEffect(() => {
        const id = new URLSearchParams(window.location.search).get('template');
        const t = id && templates?.find(x => x.id === id);
        if (t) { pickTemplate(t); setStep(2); }
        // eslint-disable-next-line react-hooks/exhaustive-deps -- once templates load
    }, [templates]);

    const pickTemplate = (t: QuickstartTemplate) => {
        setKind('template');
        setTemplate(t);
        setImage(t.image);
        setPort(String(t.http_port));
        setName(n => n || suggestProjectName(t));
    };

    const err = nameError(name, taken);
    const portNum = Number.parseInt(port, 10);
    const sourceValid = kind === 'empty' || (kind === 'template' && !!template) || (kind === 'image' && !!image.trim() && portNum > 0 && portNum < 65536);
    const configValid = !!name && !err && !!access && !!me;
    const canContinue = step === 1 ? sourceValid : step === 2 ? configValid : true;
    const deploysNow = kind !== 'empty';
    const privilegedPortRisk = deploysNow && caps?.runtime_allows_root === false && portNum < 1024;

    const create = async () => {
        if (!me) return;
        setCreating(true);
        try {
            const ownerRef = owner === 'self' ? { user: me.id } : { team: owner };
            await api.createProject(name, access, ownerRef, kind === 'template' && template ? { id: template.id, image: template.image } : null);
            if (staging) {
                await api.createEnvironment(name, {
                    name: 'staging',
                    primary_deployment_group: 'staging',
                    is_production: false,
                    color: 'yellow',
                    max_deployment_expiration: previews ? '7d' : null,
                });
            }
            window.dispatchEvent(new Event('rise:mutation'));
            if (deploysNow) {
                await api.createDeploymentFromImage(name, image.trim(), portNum);
                showToast(`${name} created · first deploy started`, 'success');
                navigate(projectPath(name));
            } else {
                showToast(`${name} created`, 'success');
                setCreated(name);
            }
        } catch (e) {
            showToast(`Failed to create project: ${(e as Error).message}`, 'error');
        } finally {
            setCreating(false);
        }
    };

    if (created) return <NextSteps name={created} staging={staging} previews={previews} />;

    const ownerName = owner === 'self' ? me?.email : teams.find(t => t.id === owner)?.name;
    const accessName = accessClasses.find(a => a.id === access)?.display_name;

    return (
        <section className="r-new-project">
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">New project</h1>
                    <div className="r-page-sub">Three steps. Everything can be changed later.</div>
                </div>
            </div>

            <ol className="r-stepper">
                {STEPS.map((label, i) => {
                    const n = i + 1;
                    const done = n < step;
                    return (
                        <li key={label} className={cx(n <= step && 'on')}>
                            <button type="button" disabled={!done} onClick={() => setStep(n)}>
                                <span className="bar" />
                                <span className="label">{n}. {label}</span>
                            </button>
                        </li>
                    );
                })}
            </ol>

            {step === 1 && (
                <div className="r-choice-grid">
                    <Choice selected={kind === 'empty'} onClick={() => { setKind('empty'); setTemplate(null); }}
                        icon="terminal" title="Empty project" text="Deploy from your machine or CI with rise deploy. Builds run locally." />
                    <Choice selected={kind === 'image'} onClick={() => { setKind('image'); setTemplate(null); if (!image || template) { setImage(''); setPort('8080'); } }}
                        icon="cube" title="Container image" text="Run an existing public image, e.g. ghcr.io/org/app:tag." />
                    {(templates || []).map(t => (
                        <Choice key={t.id} selected={kind === 'template' && template?.id === t.id} onClick={() => pickTemplate(t)}
                            img={t.icon_url} title={t.display_name} text={t.tagline} />
                    ))}
                </div>
            )}

            {step === 2 && (
                <div className="r-stack">
                    {kind === 'template' && template && (
                        <Panel className="r-template-summary">
                            <div className="r-template-summary-body">
                                {template.description}
                                {' '}<a className="r-link" href={template.learn_more_url} target="_blank" rel="noopener noreferrer">Learn more <Icon name="ext" size={11} /></a>
                            </div>
                            {template.warning && <Alert tone="warn" icon="info">{template.warning}</Alert>}
                        </Panel>
                    )}
                    {kind === 'image' && (
                        <div className="r-form-grid r-form-grid-image">
                            <label className="r-form-field">
                                <span className="r-field-label">Image</span>
                                <input className="r-field mono" value={image} onChange={e => setImage(e.target.value)} placeholder="ghcr.io/org/app:1.2.3" autoFocus />
                            </label>
                            <label className="r-form-field">
                                <span className="r-field-label">HTTP port</span>
                                <input className="r-field mono" value={port} onChange={e => setPort(e.target.value.replace(/\D/g, ''))} inputMode="numeric" />
                            </label>
                        </div>
                    )}
                    {privilegedPortRisk && (
                        <Alert tone="warn" icon="info">
                            This platform runs apps as a non-root user, so binding port {portNum} (below 1024) may fail. Prefer an image that listens on a port ≥ 1024.
                        </Alert>
                    )}
                    <label className="r-form-field">
                        <span className="r-field-label">Project name</span>
                        <input className="r-field mono" value={name} onChange={e => setName(e.target.value.toLowerCase())} placeholder="my-service" autoFocus={kind !== 'image'} />
                        <span className={cx('r-field-hint', err && 'err')}>{err ?? 'Becomes part of the app URL, e.g. https://<name>.<your Rise domain>.'}</span>
                    </label>
                    <div>
                        <span className="r-field-label">Owner</span>
                        <div className="r-chips">
                            <button type="button" className={cx('r-chip-btn', owner === 'self' && 'on')} onClick={() => setOwner('self')}>
                                <Icon name="user" size={12} />You
                            </button>
                            {teams.map(t => (
                                <button type="button" key={t.id} className={cx('r-chip-btn', owner === t.id && 'on')} onClick={() => setOwner(t.id)}>
                                    <Icon name="users" size={12} />{t.name}
                                </button>
                            ))}
                        </div>
                    </div>
                    <div>
                        <span className="r-field-label">Access</span>
                        <div className="r-choice-grid small">
                            {accessClasses.map(a => (
                                <Choice key={a.id} selected={access === a.id} onClick={() => setAccess(a.id)} title={a.display_name} text={a.description || ''} />
                            ))}
                        </div>
                    </div>
                </div>
            )}

            {step === 3 && (
                <div className="r-stack">
                    <div className="r-toggles">
                        <Toggle on locked title="production" text="Created with every project. Serves the deployment group default." />
                        <Toggle on={staging} onChange={v => { setStaging(v); if (!v) setPreviews(false); }} title="staging"
                            text="A second environment for testing; deploy to it with rise deploy -E staging." />
                        <Toggle on={previews} disabled={!staging} onChange={setPreviews} title="Preview deployments"
                            text="Short-lived deployments in staging, one per merge request (rise deploy -E staging --group mr/<n>). They expire after at most 7 days." />
                    </div>
                    <Panel className="r-summary">
                        <div><span className="k">Project</span><span className="mono">{name}</span></div>
                        <div><span className="k">Source</span><span>{kind === 'empty' ? 'Empty — deploy with rise deploy' : kind === 'template' ? template?.display_name : <span className="mono">{image}</span>}</span></div>
                        <div><span className="k">Owner</span><span className="r-owner"><Icon name={owner === 'self' ? 'user' : 'users'} size={12} />{ownerName}</span></div>
                        <div><span className="k">Access</span><span>{accessName}</span></div>
                        <div><span className="k">Environments</span><span>production{staging ? ', staging' : ''}{previews ? ' (+ previews)' : ''}</span></div>
                    </Panel>
                </div>
            )}

            <div className="r-wizard-foot">
                <Button onClick={() => (step === 1 ? navigate('/projects') : setStep(step - 1))} disabled={creating}>
                    {step === 1 ? 'Cancel' : 'Back'}
                </Button>
                {step < 3 ? (
                    <Button variant="primary" disabled={!canContinue} onClick={() => setStep(step + 1)}>Continue</Button>
                ) : (
                    <Button variant="primary" icon={deploysNow ? 'rocket' : 'plus'} loading={creating} onClick={create}>
                        {deploysNow ? 'Create & deploy' : 'Create project'}
                    </Button>
                )}
            </div>
        </section>
    );
}

function Choice({ selected, onClick, icon, img, title, text }: { selected: boolean; onClick: () => void; icon?: string; img?: string; title: string; text: string }) {
    return (
        <button type="button" className={cx('r-choice', selected && 'on')} onClick={onClick} aria-pressed={selected}>
            {img ? <img src={img} alt="" width={26} height={26} /> : icon ? <span className="r-choice-icon"><Icon name={icon} size={15} /></span> : null}
            <span className="title">{title}</span>
            {text && <span className="text">{text}</span>}
            {selected && <span className="check"><Icon name="check" size={12} /></span>}
        </button>
    );
}

function Toggle({ on, locked, disabled, onChange, title, text }: { on: boolean; locked?: boolean; disabled?: boolean; onChange?: (v: boolean) => void; title: string; text: string }) {
    return (
        <label className={cx('r-toggle-row', (locked || disabled) && 'locked')}>
            <span className="body">
                <span className="title">{title}{locked && <span className="muted"> · always on</span>}</span>
                <span className="text">{text}</span>
            </span>
            <input type="checkbox" role="switch" checked={on} disabled={locked || disabled} onChange={e => onChange?.(e.target.checked)} />
            <span className="r-switch" aria-hidden />
        </label>
    );
}

function NextSteps({ name, staging, previews }: { name: string; staging: boolean; previews: boolean }): React.ReactElement {
    return (
        <section className="r-new-project">
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">{name} is ready</h1>
                    <div className="r-page-sub">Deploy it from the project directory. Rise builds the image on your machine and pushes it.</div>
                </div>
            </div>
            <Panel className="r-next-steps">
                <pre className="mono">{`cd my-app
rise deploy -p ${name}${staging ? `\n\n# staging\nrise deploy -p ${name} -E staging` : ''}${previews ? `\n\n# a preview per merge request\nrise deploy -p ${name} -E staging --group mr/123 --expire 7d` : ''}`}</pre>
            </Panel>
            <div className="r-wizard-foot">
                <a className="r-link" href="/docs/user-guide/getting-started/">Getting started guide</a>
                <Button variant="primary" icon="arrow" onClick={() => navigate(projectPath(name))}>Open project</Button>
            </div>
        </section>
    );
}
