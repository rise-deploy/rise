import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { isSafeUrl } from '../lib/utils';

function cx(...parts: Array<string | false | null | undefined>) {
    return parts.filter(Boolean).join(' ');
}

export function StatusBadge({ status }) {
    const statusColors = {
        Healthy: 'mono-status-ok',
        Running: 'mono-status-ok',
        Deploying: 'mono-status-warn',
        Pending: 'mono-status-warn',
        Building: 'mono-status-warn',
        Pushing: 'mono-status-warn',
        Pushed: 'mono-status-warn',
        Unhealthy: 'mono-status-bad',
        Failed: 'mono-status-bad',
        Stopped: 'mono-status-muted',
        Cancelled: 'mono-status-muted',
        Superseded: 'mono-status-muted',
        Expired: 'mono-status-muted',
        Terminating: 'mono-status-muted',
    };

    const color = statusColors[status] || 'mono-status-muted';

    return <span className={`mono-status ${color}`}>{status}</span>;
}

function iconForUrl(href: string): string {
    try {
        const host = new URL(href).hostname;
        if (host.includes('github')) return '/assets/github.svg';
        if (host.includes('gitlab')) return '/assets/gitlab.svg';
    } catch { /* fall through */ }
    return '/assets/external-link.svg';
}

/** Extract a display label for a PR/MR URL, e.g. "Pull Request (#99)" or "Merge Request (!42)". */
function prLabel(href: string): string {
    try {
        const url = new URL(href);
        const parts = url.pathname.split('/').filter(Boolean);
        if (url.hostname.includes('gitlab')) {
            // GitLab: /owner/repo/-/merge_requests/42
            const idx = parts.indexOf('merge_requests');
            const num = idx >= 0 ? parts[idx + 1] : undefined;
            return num ? `Merge Request (!${num})` : 'Merge Request';
        }
        // GitHub: /owner/repo/pull/99
        const idx = parts.indexOf('pull');
        const num = idx >= 0 ? parts[idx + 1] : undefined;
        return num ? `Pull Request (#${num})` : 'Pull Request';
    } catch {
        return 'Pull Request';
    }
}

export function ExternalLinkButton({ href, label, onClick }: { href: string; label: string; onClick?: (e: React.MouseEvent) => void }) {
    if (!isSafeUrl(href)) return null;
    const icon = iconForUrl(href);
    return (
        <a href={href} target="_blank" rel="noopener noreferrer"
           className="inline-flex items-center gap-1.5 px-2.5 py-1 text-xs border border-[var(--mono-line)] hover:border-[#5a5a5a] text-[var(--mono-muted)] hover:text-[var(--mono-text)] transition-colors"
           onClick={onClick}
        >
            <span className="w-3 h-3 svg-mask inline-block"
                  style={{ maskImage: `url(${icon})`, WebkitMaskImage: `url(${icon})` }}
            />
            {label}
        </a>
    );
}

const sourceLinkBase = "px-2.5 py-1 text-xs cursor-pointer hover:bg-[#2a2a2a] transition-colors";
const sourceLinkDefault = `${sourceLinkBase} text-[var(--mono-muted)] hover:text-[var(--mono-text)]`;

/** A button-style action item for use inside SourceLinkGroup's children. */
export function SourceLinkGroupAction({ onClick, variant, children }: { onClick?: (e: React.MouseEvent) => void; variant?: 'danger'; children: React.ReactNode }) {
    const cls = variant === 'danger'
        ? `${sourceLinkBase} text-[var(--mono-bad)] hover:text-white`
        : sourceLinkDefault;
    return <button type="button" className={cls} onClick={onClick}>{children}</button>;
}

export function SourceLinkGroup({ jobUrl, prUrl, onClick, children }: { jobUrl?: string | null; prUrl?: string | null; onClick?: (e: React.MouseEvent) => void; children?: React.ReactNode }) {
    const safeJob = jobUrl && isSafeUrl(jobUrl) ? jobUrl : null;
    const safePr = prUrl && isSafeUrl(prUrl) ? prUrl : null;
    if (!safeJob && !safePr && !children) return null;

    // If only one link and no extra children, render a standalone ExternalLinkButton
    if (!children && (safeJob ? 1 : 0) + (safePr ? 1 : 0) === 1) {
        const href = (safeJob || safePr)!;
        const label = safeJob ? 'CI Job' : prLabel(href);
        return <ExternalLinkButton href={href} label={label} onClick={onClick} />;
    }

    // Combined pill
    const prIcon = safePr ? iconForUrl(safePr) : null;
    const prText = safePr ? prLabel(safePr) : null;
    const jobIcon = safeJob ? iconForUrl(safeJob) : null;
    return (
        <span className="inline-flex items-center text-xs border border-[var(--mono-line)] hover:border-[#5a5a5a] transition-colors">
            {safePr && (
                <a href={safePr} target="_blank" rel="noopener noreferrer"
                   className={`${sourceLinkDefault} inline-flex items-center gap-1.5`}
                   onClick={onClick}
                >
                    <span className="w-3 h-3 svg-mask inline-block"
                          style={{ maskImage: `url(${prIcon})`, WebkitMaskImage: `url(${prIcon})` }}
                    />
                    {prText}
                </a>
            )}
            {safePr && safeJob && <span className="w-px self-stretch bg-[var(--mono-line)]" />}
            {safeJob && (
                <a href={safeJob} target="_blank" rel="noopener noreferrer"
                   className={`${sourceLinkDefault} inline-flex items-center gap-1.5`}
                   onClick={onClick}
                >
                    <span className="w-3 h-3 svg-mask inline-block"
                          style={{ maskImage: `url(${jobIcon})`, WebkitMaskImage: `url(${jobIcon})` }}
                    />
                    CI Job
                </a>
            )}
            {children && (safeJob || safePr) && <span className="w-px self-stretch bg-[var(--mono-line)]" />}
            {children}
        </span>
    );
}

export function Button({
    children,
    onClick,
    variant = 'primary',
    size = 'md',
    loading = false,
    disabled = false,
    type = 'button',
    className = '',
}) {
    const baseClasses = 'mono-btn';
    const variantClasses = {
        primary: 'mono-btn-primary',
        secondary: 'mono-btn-secondary',
        danger: 'mono-btn-danger',
    };
    const sizeClasses = {
        sm: 'mono-btn-sm',
        md: 'mono-btn-md',
        lg: 'mono-btn-lg',
    };

    return (
        <button
            type={type as 'button' | 'submit' | 'reset'}
            onClick={onClick}
            disabled={disabled || loading}
            className={`${baseClasses} ${variantClasses[variant]} ${sizeClasses[size]} ${className}`}
        >
            {loading && <div className="mono-spinner" />}
            {children}
        </button>
    );
}

export function Modal({
    isOpen,
    onClose,
    title,
    children,
    maxWidth = 'max-w-2xl',
    modalClassName = '',
    bodyClassName = '',
}) {
    const [bodyElement, setBodyElement] = useState<HTMLDivElement | null>(null);

    useEffect(() => {
        const handleEscape = (e) => {
            if (e.key === 'Escape' && isOpen) {
                onClose();
            }
        };
        const handleNavigate = () => {
            if (isOpen) onClose();
        };
        const handleCloseAll = () => {
            if (isOpen) onClose();
        };

        document.addEventListener('keydown', handleEscape);
        window.addEventListener('rise:navigate', handleNavigate as EventListener);
        window.addEventListener('rise:close-modals', handleCloseAll as EventListener);
        return () => {
            document.removeEventListener('keydown', handleEscape);
            window.removeEventListener('rise:navigate', handleNavigate as EventListener);
            window.removeEventListener('rise:close-modals', handleCloseAll as EventListener);
        };
    }, [isOpen, onClose]);

    useEffect(() => {
        if (isOpen) {
            document.body.style.overflow = 'hidden';
        } else {
            document.body.style.overflow = 'unset';
        }
        return () => {
            document.body.style.overflow = 'unset';
        };
    }, [isOpen]);

    useEffect(() => {
        if (!isOpen || !bodyElement) return;

        const timer = window.setTimeout(() => {
            const firstInput = bodyElement.querySelector<HTMLElement>(
                '[data-autofocus], input:not([type="hidden"]):not([disabled]), select:not([disabled]), textarea:not([disabled])'
            );
            firstInput?.focus();
        }, 0);

        return () => window.clearTimeout(timer);
    }, [isOpen, bodyElement]);

    if (!isOpen) return null;

    return (
        <div className="modal-backdrop" onClick={onClose}>
            <div className={cx('modal-content mono-modal-shell', maxWidth, modalClassName)} onClick={(e) => e.stopPropagation()}>
                <div className="modal-header mono-modal-header">
                    <h3 className="modal-title">{title}</h3>
                    <button onClick={onClose} className="modal-close-button" aria-label="Close modal">
                        <div
                            className="w-6 h-6 svg-mask"
                            style={{
                                maskImage: 'url(/assets/close-x.svg)',
                                WebkitMaskImage: 'url(/assets/close-x.svg)',
                            }}
                        ></div>
                    </button>
                </div>
                <div className={cx('modal-body mono-modal-body', bodyClassName)} ref={setBodyElement}>{children}</div>
            </div>
        </div>
    );
}

export function ModalSection({ children, className = '' }) {
    return <div className={cx('mono-modal-section', className)}>{children}</div>;
}

export function ModalActions({ children, className = '' }) {
    return <div className={cx('mono-modal-actions', className)}>{children}</div>;
}

export function ModalTabs({ children, className = '' }) {
    return <div className={cx('mono-modal-tabs', className)}>{children}</div>;
}

export function MonoTabButton({
    children,
    active = false,
    tone = 'default',
    onClick,
    className = '',
}) {
    return (
        <button
            type="button"
            onClick={onClick}
            className={cx('mono-tab-trigger', active && 'active', tone === 'danger' && 'mono-tab-trigger-danger', className)}
        >
            {children}
        </button>
    );
}

export function MonoStatusPill({ tone = 'muted', uppercase = true, className = '', children }) {
    const toneClass = {
        ok: 'mono-pill-ok',
        warn: 'mono-pill-warn',
        bad: 'mono-pill-bad',
        muted: 'mono-pill-muted',
    }[tone] || 'mono-pill-muted';

    return <span className={cx('mono-status-pill', toneClass, uppercase && 'mono-status-pill-up', className)}>{children}</span>;
}

export const ENV_COLOR_STYLES = {
    green:  { color: '#34d399', borderColor: '#2e6c44', background: 'rgba(44, 105, 66, 0.2)' },
    blue:   { color: '#60a5fa', borderColor: '#2e5a8c', background: 'rgba(44, 80, 140, 0.2)' },
    yellow: { color: '#fbbf24', borderColor: '#7b6333', background: 'rgba(139, 112, 57, 0.22)' },
    red:    { color: '#f87171', borderColor: '#7d4b4b', background: 'rgba(125, 75, 75, 0.24)' },
    purple: { color: '#a78bfa', borderColor: '#6b3fa0', background: 'rgba(107, 63, 160, 0.2)' },
    orange: { color: '#fb923c', borderColor: '#8c5a2e', background: 'rgba(140, 90, 46, 0.22)' },
    gray:   { color: '#9ca3af', borderColor: '#4a4a4a', background: 'rgba(74, 74, 74, 0.2)' },
};

const ENV_COLORS = Object.keys(ENV_COLOR_STYLES) as Array<keyof typeof ENV_COLOR_STYLES>;

export function EnvironmentColorDot({ color = 'green', size = '0.75rem', className = '' }) {
    const style = ENV_COLOR_STYLES[color] || ENV_COLOR_STYLES.green;
    return (
        <span
            className={className}
            style={{ display: 'inline-block', width: size, height: size, borderRadius: '50%', backgroundColor: style.color, flexShrink: 0 }}
        />
    );
}

export function EnvironmentColorPicker({ value, onChange }) {
    return (
        <div className="flex items-center gap-2">
            {ENV_COLORS.map((c) => (
                <button
                    key={c}
                    type="button"
                    onClick={() => onChange(c)}
                    className="mono-color-pick"
                    style={{
                        outlineColor: value === c ? ENV_COLOR_STYLES[c].color : 'transparent',
                    }}
                    aria-label={c}
                >
                    <EnvironmentColorDot color={c} size="0.75rem" />
                </button>
            ))}
        </div>
    );
}

export function EnvironmentCombobox({
    environments,
    selected,
    onChange,
    placeholder = 'All environments',
}) {
    const [query, setQuery] = useState('');
    const [isOpen, setIsOpen] = useState(false);
    const wrapRef = useRef<HTMLDivElement | null>(null);
    const listRef = useRef<HTMLDivElement | null>(null);
    const [pos, setPos] = useState({ top: 0, left: 0, width: 0 });

    useEffect(() => {
        function handleClickOutside(e: MouseEvent) {
            const target = e.target as Node;
            if (wrapRef.current?.contains(target) || listRef.current?.contains(target)) return;
            setIsOpen(false);
        }
        document.addEventListener('mousedown', handleClickOutside);
        return () => document.removeEventListener('mousedown', handleClickOutside);
    }, []);

    const filtered = environments.filter((env) =>
        env.name.toLowerCase().includes(query.toLowerCase()) && !selected.includes(env.name)
    );

    const handleSelect = (name: string) => {
        onChange([...selected, name]);
    };

    const handleRemove = (name: string) => {
        onChange(selected.filter((n) => n !== name));
    };

    const selectedEnvs = environments.filter((env) => selected.includes(env.name));

    const open = () => {
        if (wrapRef.current) {
            const rect = wrapRef.current.getBoundingClientRect();
            setPos({ top: rect.bottom + 2, left: rect.left, width: rect.width });
        }
        setIsOpen(true);
    };

    return (
        <>
            <div
                ref={wrapRef}
                className="mono-input flex flex-wrap items-center gap-1 cursor-text"
                style={{ minHeight: '2.1rem', paddingTop: '0.25rem', paddingBottom: '0.25rem' }}
                onClick={() => { if (!isOpen) open(); }}
            >
                {selectedEnvs.map((env) => (
                    <span key={env.name} className="mono-env-chip">
                        <EnvironmentColorDot color={env.color} size="0.5rem" />
                        {env.name}
                        <button
                            type="button"
                            className="mono-env-chip-x"
                            onClick={(e) => { e.stopPropagation(); handleRemove(env.name); }}
                            aria-label={`Remove ${env.name}`}
                        >
                            &times;
                        </button>
                    </span>
                ))}
                <input
                    type="text"
                    className="mono-env-combo-input"
                    placeholder={selectedEnvs.length === 0 ? placeholder : ''}
                    value={query}
                    onChange={(e) => { setQuery(e.target.value); if (!isOpen) open(); }}
                    onFocus={() => { if (!isOpen) open(); }}
                    tabIndex={-1}
                />
            </div>
            {isOpen && createPortal(
                <div
                    ref={listRef}
                    className="mono-dropdown-list"
                    style={{ position: 'fixed', top: pos.top, left: pos.left, width: pos.width }}
                >
                    {filtered.length === 0 ? (
                        <div className="mono-env-combo-empty">No environments</div>
                    ) : (
                        filtered.map((env) => (
                            <button
                                key={env.name}
                                type="button"
                                className="mono-dropdown-option"
                                onClick={() => handleSelect(env.name)}
                            >
                                <EnvironmentColorDot color={env.color} size="0.6rem" />
                                <span>{env.name}</span>
                            </button>
                        ))
                    )}
                </div>,
                document.body
            )}
        </>
    );
}

export function MonoDropdown({
    options,
    value,
    onChange,
    placeholder = 'Select...',
    allowNull = false,
}) {
    const [isOpen, setIsOpen] = useState(false);
    const triggerRef = useRef<HTMLButtonElement | null>(null);
    const listRef = useRef<HTMLDivElement | null>(null);
    const [pos, setPos] = useState({ top: 0, left: 0, width: 0 });

    useEffect(() => {
        function handleClickOutside(e: MouseEvent) {
            const target = e.target as Node;
            if (triggerRef.current?.contains(target) || listRef.current?.contains(target)) return;
            setIsOpen(false);
        }
        document.addEventListener('mousedown', handleClickOutside);
        return () => document.removeEventListener('mousedown', handleClickOutside);
    }, []);

    const open = () => {
        if (triggerRef.current) {
            const rect = triggerRef.current.getBoundingClientRect();
            setPos({ top: rect.bottom + 2, left: rect.left, width: rect.width });
        }
        setIsOpen(true);
    };

    const selected = options.find((opt) => opt.value === value);

    return (
        <>
            <button
                ref={triggerRef}
                type="button"
                className="mono-input mono-dropdown-trigger"
                onClick={() => isOpen ? setIsOpen(false) : open()}
            >
                {selected ? (
                    <span className="inline-flex items-center gap-2">
                        {selected.icon}
                        {selected.label}
                    </span>
                ) : (
                    <span style={{ color: 'var(--mono-muted)' }}>{placeholder}</span>
                )}
            </button>
            {isOpen && createPortal(
                <div
                    ref={listRef}
                    className="mono-dropdown-list"
                    style={{ position: 'fixed', top: pos.top, left: pos.left, width: pos.width }}
                >
                    {allowNull && (
                        <button
                            type="button"
                            className={cx('mono-dropdown-option', value == null && 'active')}
                            onClick={() => { onChange(null); setIsOpen(false); }}
                        >
                            <span style={{ color: 'var(--mono-muted)' }}>{placeholder}</span>
                        </button>
                    )}
                    {options.map((opt) => (
                        <button
                            key={opt.value}
                            type="button"
                            className={cx('mono-dropdown-option', value === opt.value && 'active')}
                            onClick={() => { onChange(opt.value); setIsOpen(false); }}
                        >
                            {opt.icon}
                            <span>{opt.label}</span>
                        </button>
                    ))}
                </div>,
                document.body
            )}
        </>
    );
}

export function EnvironmentDropdown({ environments, value, onChange, placeholder = 'Global (all environments)' }) {
    const options = environments.map((env) => ({
        value: env.name,
        label: env.name,
        icon: <EnvironmentColorDot color={env.color} size="0.6rem" />,
    }));
    return <MonoDropdown options={options} value={value} onChange={onChange} placeholder={placeholder} allowNull />;
}

export function MonoTag({ color = 'muted', className = '', children }) {
    const colorClass = {
        muted: 'mono-tag-muted',
        purple: 'mono-tag-purple',
        yellow: 'mono-tag-yellow',
        gray: 'mono-tag-gray',
        green: 'mono-tag-green',
        blue: 'mono-tag-blue',
        red: 'mono-tag-red',
        orange: 'mono-tag-orange',
        indigo: 'mono-tag-indigo',
    }[color] || 'mono-tag-muted';

    return <span className={cx('mono-tag', colorClass, className)}>{children}</span>;
}

export function MonoNotice({ tone = 'info', title = '', className = '', children }) {
    const toneClass = {
        info: 'mono-notice-info',
        warn: 'mono-notice-warn',
        error: 'mono-notice-error',
        success: 'mono-notice-success',
        muted: 'mono-notice-muted',
    }[tone] || 'mono-notice-info';

    return (
        <div className={cx('mono-notice', toneClass, className)}>
            {title && <h4 className="mono-notice-title">{title}</h4>}
            <div className="mono-notice-body">{children}</div>
        </div>
    );
}

export function MonoCodeBlock({ as = 'pre', className = '', children }) {
    const Tag = as as any;
    return (
        <Tag className={cx('mono-code-block', className)}>
            {children}
        </Tag>
    );
}

export function FormField({
    label,
    id,
    type = 'text',
    value,
    onChange,
    error,
    required = false,
    placeholder,
    disabled = false,
    options = [],
    rows = 3,
    list = undefined,
    children = null,
    autoFocus = false,
}) {
    const inputClasses = `mono-input ${error ? 'mono-input-error' : ''}`;

    return (
        <div className="form-field">
            <label htmlFor={id} className="mono-label">
                {label}
                {required && <span className="text-red-300 ml-1">*</span>}
            </label>

            {type === 'select' ? (
                <select id={id} value={value} onChange={onChange} disabled={disabled} className={inputClasses}>
                    {children
                        ? children
                        : options.map((opt) => (
                              <option key={opt.value} value={opt.value}>
                                  {opt.label}
                              </option>
                          ))}
                </select>
            ) : type === 'textarea' ? (
                <textarea
                    id={id}
                    value={value}
                    onChange={onChange}
                    placeholder={placeholder}
                    disabled={disabled}
                    rows={rows}
                    className={inputClasses}
                />
            ) : type === 'checkbox' ? (
                <div className="flex items-center">
                    <input type="checkbox" id={id} checked={value} onChange={onChange} disabled={disabled} className="mono-checkbox" />
                    <label htmlFor={id} className="ml-2 text-sm text-gray-300">
                        {placeholder}
                    </label>
                </div>
            ) : (
                <input
                    type={type}
                    id={id}
                    value={value}
                    onChange={onChange}
                    placeholder={placeholder}
                    disabled={disabled}
                    list={list}
                    autoFocus={autoFocus}
                    className={inputClasses}
                />
            )}

            {error && <p className="mt-2 text-sm text-red-300">{error}</p>}
        </div>
    );
}

export function AutocompleteInput({
    id,
    value,
    onChange,
    options = [],
    type = 'text',
    placeholder = '',
    disabled = false,
    loading = false,
    multiValue = false,
    noMatchesText = 'No matches',
    className = '',
    onEnter,
}) {
    const [isOpen, setIsOpen] = useState(false);
    const [popRect, setPopRect] = useState<{ top: number; left: number; width: number } | null>(null);
    const wrapRef = useRef<HTMLDivElement | null>(null);
    const inputRef = useRef<HTMLInputElement | null>(null);
    const popRef = useRef<HTMLDivElement | null>(null);

    useEffect(() => {
        function handleClickOutside(e: MouseEvent) {
            const t = e.target as Node;
            if (wrapRef.current && wrapRef.current.contains(t)) return;
            if (popRef.current && popRef.current.contains(t)) return;
            setIsOpen(false);
        }
        document.addEventListener('mousedown', handleClickOutside);
        return () => document.removeEventListener('mousedown', handleClickOutside);
    }, []);

    // Position the suggestion list as a fixed-position portal so it is not
    // clipped by a surrounding modal/overflow container.
    useEffect(() => {
        if (!isOpen) { setPopRect(null); return; }
        const update = () => {
            const el = inputRef.current;
            if (!el) return;
            const r = el.getBoundingClientRect();
            setPopRect({ top: r.bottom + 4, left: r.left, width: r.width });
        };
        update();
        window.addEventListener('scroll', update, true);
        window.addEventListener('resize', update);
        return () => {
            window.removeEventListener('scroll', update, true);
            window.removeEventListener('resize', update);
        };
    }, [isOpen]);

    const rawQuery = multiValue ? (value || '').split(',').pop() || '' : (value || '');
    const query = rawQuery.trim().toLowerCase();
    const uniqueOptions = Array.from(new Set((options || []).filter(Boolean)));
    const filteredOptions = uniqueOptions.filter((opt) => opt.toLowerCase().includes(query));

    const handleSelect = (selected: string) => {
        if (multiValue) {
            const chunks = (value || '').split(',');
            const prefix = chunks.slice(0, -1).map((p) => p.trim()).filter(Boolean).join(', ');
            onChange(prefix ? `${prefix}, ${selected}` : selected);
        } else {
            onChange(selected);
        }
        setIsOpen(false);
    };

    return (
        <div ref={wrapRef} className={cx('relative', className)}>
            <input
                ref={inputRef}
                type={type}
                id={id}
                className="mono-input w-full"
                placeholder={placeholder}
                value={value}
                onChange={(e) => {
                    onChange(e.target.value);
                    if (!isOpen) setIsOpen(true);
                }}
                onKeyDown={(e) => {
                    if (e.key === 'Enter' && onEnter) onEnter();
                }}
                onFocus={() => setIsOpen(true)}
                onClick={() => setIsOpen(true)}
                disabled={disabled || loading}
            />
            {isOpen && popRect && createPortal(
                <div
                    ref={popRef}
                    className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded shadow-lg max-h-48 overflow-y-auto"
                    style={{ position: 'fixed', top: popRect.top, left: popRect.left, width: popRect.width, zIndex: 100 }}
                >
                    {loading ? (
                        <div className="px-3 py-2 text-sm text-gray-500">Loading...</div>
                    ) : filteredOptions.length === 0 ? (
                        <div className="px-3 py-2 text-sm text-gray-500">{noMatchesText}</div>
                    ) : (
                        filteredOptions.map((opt) => (
                            <button
                                key={opt}
                                type="button"
                                className="w-full text-left px-3 py-2 text-sm hover:bg-gray-100 dark:hover:bg-gray-700"
                                onClick={() => handleSelect(opt)}
                            >
                                {opt}
                            </button>
                        ))
                    )}
                </div>,
                document.body
            )}
        </div>
    );
}

export function Footer({ version }) {
    return (
        <footer className="mono-footer">
            <div className="mono-footer-inner">
                <div className="flex items-center gap-4">
                    <span>Rise {version?.version ? `v${version.version}` : ''}</span>
                    {version?.repository && (
                        <a href={version.repository} target="_blank" rel="noopener noreferrer" className="underline">
                            github
                        </a>
                    )}
                </div>
                <div className="text-xs">container deployment platform</div>
            </div>
        </footer>
    );
}
