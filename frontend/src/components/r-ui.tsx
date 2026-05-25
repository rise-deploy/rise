import React, { useEffect, useId, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { isSafeUrl } from '../lib/utils';
import { Icon } from './icon';

export function cx(...parts: Array<string | false | null | undefined>) {
    return parts.filter(Boolean).join(' ');
}

// ---------- Status ----------
export function Status({ status, bare = false }: { status: string; bare?: boolean }) {
    const key = (status || '').toLowerCase();
    return (
        <span className={cx('r-status', key, bare && 'bare')}>
            <span className="dot" />
            <span>{status}</span>
        </span>
    );
}

// ---------- Pill ----------
export function Pill({ children, kind, className }: { children: React.ReactNode; kind?: 'env-prod' | 'env-staging' | 'env-global' | 'accent'; className?: string }) {
    return <span className={cx('r-pill', kind, className)}>{children}</span>;
}

// Two-tone chip: a small icon cell tinted with the environment's color
// next to a neutral name cell. The name styling stays consistent across
// environments so different envs read as variants of the same shape; the
// color signal is concentrated in the icon. This is the canonical way to
// render an environment label across the app.
export function EnvPill({ env, color }: { env: string; color?: string }) {
    const palette = color ? ENV_COLOR_STYLES[color] : undefined;
    const iconStyle = palette
        ? { background: palette.background, color: palette.color }
        : undefined;
    return (
        <span className="r-env-pill">
            <span className="r-env-pill-icon" style={iconStyle} aria-hidden>
                <Icon name="layer" size={11} />
            </span>
            <span className="r-env-pill-name">{env}</span>
        </span>
    );
}

// Two-tone chip mirroring EnvPill but for deployment groups. Groups don't
// have a per-group color, so the icon cell stays neutral; the name renders in
// mono since groups are technical identifiers (branch names, PR slugs).
export function GroupPill({ group }: { group: string }) {
    return (
        <span className="r-group-pill">
            <span className="r-group-pill-icon" aria-hidden>
                <Icon name="branch" size={11} />
            </span>
            <span className="r-group-pill-name mono">{group}</span>
        </span>
    );
}

// Tinted layer glyph used inside `EnvPill` (and standalone where an env is
// shown without a surrounding pill). Color falls back to `currentColor` so it
// inherits the pill's text color when no env color is supplied.
export function EnvironmentIcon({ color, size = 11 }: { color?: string; size?: number }) {
    const resolved = color ? ENV_COLOR_STYLES[color]?.color : undefined;
    const fill = resolved || color || 'currentColor';
    return (
        <span
            style={{ display: 'inline-flex', color: fill, flexShrink: 0, lineHeight: 0 }}
            aria-hidden
        >
            <Icon name="layer" size={size} />
        </span>
    );
}

// Resolved palette for the named env colors. Single source of truth for both
// the `EnvironmentIcon` glyph and the bordered `EnvironmentColorDot`/picker.
export const ENV_COLOR_STYLES: Record<string, { color: string; borderColor: string; background: string }> = {
    green:  { color: '#34d399', borderColor: '#2e6c44', background: 'rgba(44, 105, 66, 0.2)' },
    blue:   { color: '#60a5fa', borderColor: '#2e5a8c', background: 'rgba(44, 80, 140, 0.2)' },
    yellow: { color: '#fbbf24', borderColor: '#7b6333', background: 'rgba(139, 112, 57, 0.22)' },
    red:    { color: '#f87171', borderColor: '#7d4b4b', background: 'rgba(125, 75, 75, 0.24)' },
    purple: { color: '#a78bfa', borderColor: '#6b3fa0', background: 'rgba(107, 63, 160, 0.2)' },
    orange: { color: '#fb923c', borderColor: '#8c5a2e', background: 'rgba(140, 90, 46, 0.22)' },
    gray:   { color: '#9ca3af', borderColor: '#4a4a4a', background: 'rgba(74, 74, 74, 0.2)' },
};

const ENV_COLORS = Object.keys(ENV_COLOR_STYLES);

// Visually represents an environment with a layer-stack glyph tinted with the
// environment's color. Ported from legacy `ui.tsx`; accepts the same loose size
// shapes ("0.75rem", "12", number, "12px") so Wave 2 migrations are mechanical.
export function EnvironmentColorDot({ color = 'green', size = '0.75rem', className = '' }: { color?: string; size?: string | number; className?: string }) {
    const style = ENV_COLOR_STYLES[color] || ENV_COLOR_STYLES.green;
    let pxSize = 12;
    if (typeof size === 'number') pxSize = size;
    else if (typeof size === 'string') {
        const match = size.match(/^([\d.]+)(rem|px)?$/);
        if (match) {
            const value = parseFloat(match[1]);
            pxSize = match[2] === 'px' ? value : Math.round(value * 16 + 4);
        }
    }
    return (
        <span
            className={className}
            style={{ display: 'inline-flex', color: style.color, flexShrink: 0, lineHeight: 0 }}
            aria-hidden
        >
            <Icon name="layer" size={pxSize} />
        </span>
    );
}

export function EnvironmentColorPicker({ value, onChange }: { value: string; onChange: (color: string) => void }) {
    return (
        <div role="radiogroup" aria-label="Environment color" style={{ display: 'inline-flex', gap: 6 }}>
            {ENV_COLORS.map((c) => {
                const checked = value === c;
                const style = ENV_COLOR_STYLES[c];
                return (
                    <button
                        key={c}
                        type="button"
                        role="radio"
                        aria-checked={checked}
                        aria-label={c}
                        title={c}
                        onClick={() => onChange(c)}
                        className="r-env-color-pick"
                        style={{ color: style.color }}
                    >
                        <EnvironmentColorDot color={c} size="0.75rem" />
                    </button>
                );
            })}
        </div>
    );
}

// ---------- Source link helpers (ported from legacy ui.tsx) ----------
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
            const idx = parts.indexOf('merge_requests');
            const num = idx >= 0 ? parts[idx + 1] : undefined;
            return num ? `Merge Request (!${num})` : 'Merge Request';
        }
        const idx = parts.indexOf('pull');
        const num = idx >= 0 ? parts[idx + 1] : undefined;
        return num ? `Pull Request (#${num})` : 'Pull Request';
    } catch {
        return 'Pull Request';
    }
}

// Inline class strings mirror the legacy markup. The legacy `--mono-*` tokens
// were retired with the redesign, so we map them onto the current token set
// (`--border` / `--text-soft` / `--text` / `--err`) here.
const sourceLinkBase = 'px-2.5 py-1 text-xs cursor-pointer hover:bg-[var(--hover)] transition-colors';
const sourceLinkDefault = `${sourceLinkBase} text-[var(--text-soft)] hover:text-[var(--text)]`;

export function ExternalLinkButton({ href, label, onClick }: { href: string; label: string; onClick?: (e: React.MouseEvent) => void }) {
    if (!isSafeUrl(href)) return null;
    const icon = iconForUrl(href);
    return (
        <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1.5 px-2.5 py-1 text-xs border border-[var(--border)] hover:border-[var(--border-strong)] text-[var(--text-soft)] hover:text-[var(--text)] transition-colors"
            onClick={onClick}
        >
            <span
                className="w-3 h-3 svg-mask inline-block"
                style={{ maskImage: `url(${icon})`, WebkitMaskImage: `url(${icon})` }}
            />
            {label}
        </a>
    );
}

export function SourceLinkGroupAction({ onClick, variant, children }: { onClick?: (e: React.MouseEvent) => void; variant?: 'danger'; children: React.ReactNode }) {
    const cls = variant === 'danger'
        ? `${sourceLinkBase} text-[var(--err)] hover:text-[var(--text)]`
        : sourceLinkDefault;
    return <button type="button" className={cls} onClick={onClick}>{children}</button>;
}

export function SourceLinkGroup({ jobUrl, prUrl, onClick, children }: { jobUrl?: string | null; prUrl?: string | null; onClick?: (e: React.MouseEvent) => void; children?: React.ReactNode }) {
    const safeJob = jobUrl && isSafeUrl(jobUrl) ? jobUrl : null;
    const safePr = prUrl && isSafeUrl(prUrl) ? prUrl : null;
    if (!safeJob && !safePr && !children) return null;

    // Single link, no extra children → standalone button
    if (!children && (safeJob ? 1 : 0) + (safePr ? 1 : 0) === 1) {
        const href = (safeJob || safePr)!;
        const label = safeJob ? 'CI Job' : prLabel(href);
        return <ExternalLinkButton href={href} label={label} onClick={onClick} />;
    }

    const prIcon = safePr ? iconForUrl(safePr) : null;
    const prText = safePr ? prLabel(safePr) : null;
    const jobIcon = safeJob ? iconForUrl(safeJob) : null;
    return (
        <span className="inline-flex items-center text-xs border border-[var(--border)] hover:border-[var(--border-strong)] transition-colors">
            {safePr && (
                <a
                    href={safePr}
                    target="_blank"
                    rel="noopener noreferrer"
                    className={`${sourceLinkDefault} inline-flex items-center gap-1.5`}
                    onClick={onClick}
                >
                    <span
                        className="w-3 h-3 svg-mask inline-block"
                        style={{ maskImage: `url(${prIcon})`, WebkitMaskImage: `url(${prIcon})` }}
                    />
                    {prText}
                </a>
            )}
            {safePr && safeJob && <span className="w-px self-stretch bg-[var(--border)]" />}
            {safeJob && (
                <a
                    href={safeJob}
                    target="_blank"
                    rel="noopener noreferrer"
                    className={`${sourceLinkDefault} inline-flex items-center gap-1.5`}
                    onClick={onClick}
                >
                    <span
                        className="w-3 h-3 svg-mask inline-block"
                        style={{ maskImage: `url(${jobIcon})`, WebkitMaskImage: `url(${jobIcon})` }}
                    />
                    CI Job
                </a>
            )}
            {children && (safeJob || safePr) && <span className="w-px self-stretch bg-[var(--border)]" />}
            {children}
        </span>
    );
}

// ---------- Button ----------
type ButtonVariant = 'default' | 'primary' | 'danger' | 'ghost';
export interface RButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> {
    variant?: ButtonVariant;
    size?: 'sm' | 'md';
    loading?: boolean;
    icon?: string;
}

export function Button({ variant = 'default', size = 'md', loading, icon, children, className, disabled, ...rest }: RButtonProps) {
    return (
        <button
            type="button"
            className={cx('r-btn', variant !== 'default' && variant, size === 'sm' && 'small', className)}
            disabled={disabled || loading}
            {...rest}
        >
            {loading ? <span className="r-spinner" style={{ width: 12, height: 12, borderWidth: 2 }} /> : icon ? <Icon name={icon} size={13} /> : null}
            {children}
        </button>
    );
}

// ---------- Panel ----------
export function Panel({ children, className, style, onClick }: { children: React.ReactNode; className?: string; style?: React.CSSProperties; onClick?: () => void }) {
    if (onClick) {
        return (
            <div
                className={cx('r-panel', 'r-panel-clickable', className)}
                style={style}
                onClick={onClick}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(); } }}
            >
                {children}
            </div>
        );
    }
    return <div className={cx('r-panel', className)} style={style}>{children}</div>;
}

export function PanelHead({ title, sub, right, children }: { title?: React.ReactNode; sub?: React.ReactNode; right?: React.ReactNode; children?: React.ReactNode }) {
    if (children) return <div className="r-panel-head">{children}</div>;
    return (
        <div className="r-panel-head">
            <div>
                {title && <div className="r-panel-title">{title}</div>}
                {sub && <div className="r-panel-sub">{sub}</div>}
            </div>
            {right && <div>{right}</div>}
        </div>
    );
}

export function PanelBody({ children, className, style }: { children: React.ReactNode; className?: string; style?: React.CSSProperties }) {
    return <div className={cx('r-panel-body', className)} style={style}>{children}</div>;
}

// ---------- Tabs ----------
export interface Tab { id: string; label: string; count?: number | string }
export function Tabs({ tabs, active, onChange }: { tabs: Tab[]; active: string; onChange: (id: string) => void }) {
    return (
        <div className="r-tabs">
            {tabs.map(t => (
                <button key={t.id} className={cx('r-tab', active === t.id && 'active')} onClick={() => onChange(t.id)} type="button">
                    {t.label}
                    {t.count !== undefined && <span className="r-tab-count">{t.count}</span>}
                </button>
            ))}
        </div>
    );
}

// ---------- Segmented ----------
export function Segmented<T extends string>({ value, options, onChange, capitalize }: { value: T; options: { value: T; label: string }[] | T[]; onChange: (v: T) => void; capitalize?: boolean }) {
    const opts = options.map(o => typeof o === 'string' ? { value: o, label: o } : o);
    return (
        <div className="r-seg">
            {opts.map(o => (
                <button key={o.value} className={value === o.value ? 'active' : ''} onClick={() => onChange(o.value)} type="button" style={capitalize ? { textTransform: 'capitalize' } : undefined}>
                    {o.label}
                </button>
            ))}
        </div>
    );
}

// ---------- Stat / StatGrid ----------
export function StatGrid({ children, cols = 4 }: { children: React.ReactNode; cols?: 2 | 3 | 4 }) {
    return <div className={cx('r-stat-grid', cols !== 4 && `cols-${cols}`)}>{children}</div>;
}

export function Stat({ label, value, unit, delta, deltaTone }: { label: React.ReactNode; value: React.ReactNode; unit?: string; delta?: React.ReactNode; deltaTone?: 'up' | 'down' }) {
    return (
        <div className="r-stat">
            <div className="r-stat-label">{label}</div>
            <div className="r-stat-value">{value}{unit && <span className="r-stat-unit">{unit}</span>}</div>
            {delta && <div className={cx('r-stat-delta', deltaTone)}>{delta}</div>}
        </div>
    );
}

// ---------- Modal ----------
// Focusable selector used for autofocus + focus-trap (mirrors the legacy modal).
const FOCUSABLE_SELECTOR = 'a[href], area[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), iframe, object, embed, [tabindex]:not([tabindex="-1"]), [contenteditable]';
const AUTOFOCUS_SELECTOR = '[data-autofocus], input:not([type="hidden"]):not([disabled]), select:not([disabled]), textarea:not([disabled])';

export function Modal({ isOpen, onClose, title, sub, children, footer, width }: { isOpen: boolean; onClose: () => void; title?: React.ReactNode; sub?: React.ReactNode; children: React.ReactNode; footer?: React.ReactNode; width?: 'default' | 'wide' | 'xwide' }) {
    const modalRef = useRef<HTMLDivElement | null>(null);
    const bodyRef = useRef<HTMLDivElement | null>(null);

    // Escape-to-close
    useEffect(() => {
        if (!isOpen) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === 'Escape') onClose();
        };
        window.addEventListener('keydown', handler);
        return () => window.removeEventListener('keydown', handler);
    }, [isOpen, onClose]);

    // Body scroll lock (capture and restore the previous value so nested or
    // strict-mode double-mounts don't accidentally clobber the user's prior
    // value).
    useEffect(() => {
        if (!isOpen) return;
        const prev = document.body.style.overflow;
        document.body.style.overflow = 'hidden';
        return () => { document.body.style.overflow = prev; };
    }, [isOpen]);

    // Autofocus the first focusable input after mount
    useEffect(() => {
        if (!isOpen) return;
        const t = window.setTimeout(() => {
            const el = bodyRef.current?.querySelector<HTMLElement>(AUTOFOCUS_SELECTOR);
            el?.focus();
        }, 0);
        return () => window.clearTimeout(t);
    }, [isOpen]);

    // Focus trap: keep Tab cycling within the modal
    const onKeyDownTrap = (e: React.KeyboardEvent<HTMLDivElement>) => {
        if (e.key !== 'Tab') return;
        const root = modalRef.current;
        if (!root) return;
        const focusables = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR))
            .filter(el => !el.hasAttribute('disabled') && el.tabIndex !== -1 && el.offsetParent !== null);
        if (focusables.length === 0) return;
        const first = focusables[0];
        const last = focusables[focusables.length - 1];
        const active = document.activeElement as HTMLElement | null;
        if (e.shiftKey) {
            if (active === first || !root.contains(active)) {
                e.preventDefault();
                last.focus();
            }
        } else {
            if (active === last) {
                e.preventDefault();
                first.focus();
            }
        }
    };

    if (!isOpen) return null;

    const node = (
        <div className="r-modal-mask" onClick={onClose}>
            <div
                ref={modalRef}
                className={cx('r-modal', width === 'wide' && 'wide', width === 'xwide' && 'xwide')}
                onClick={e => e.stopPropagation()}
                onKeyDown={onKeyDownTrap}
                role="dialog"
                aria-modal="true"
            >
                {(title || sub) && (
                    <div className="r-modal-head">
                        {title && <div className="r-modal-title">{title}</div>}
                        {sub && <div className="r-modal-sub">{sub}</div>}
                    </div>
                )}
                <div className="r-modal-body" ref={bodyRef}>{children}</div>
                {footer && <div className="r-modal-foot">{footer}</div>}
            </div>
        </div>
    );
    return createPortal(node, document.body);
}

// ---------- FieldLabel + Input ----------
export function Field({ label, children, hint }: { label: React.ReactNode; children: React.ReactNode; hint?: React.ReactNode }) {
    return (
        <div>
            <label className="r-field-label">{label}</label>
            {children}
            {hint && <div style={{ fontSize: 11.5, color: 'var(--text-soft)', marginTop: 4 }}>{hint}</div>}
        </div>
    );
}

export function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
    return <input {...props} className={cx('r-field', props.className)} />;
}
export function Textarea(props: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
    return <textarea {...props} className={cx('r-field', props.className)} />;
}

// ---------- Combobox ----------
// A searchable, custom-styled dropdown. Replaces native <select> everywhere so
// every form field shares one look. Supports both single and multi selection.

export interface ComboboxOption {
    value: string;
    label: string;
    hint?: React.ReactNode;
    icon?: React.ReactNode;
    keywords?: string;
}

interface ComboboxBaseProps {
    options: ComboboxOption[];
    placeholder?: string;
    disabled?: boolean;
    searchPlaceholder?: string;
    emptyText?: string;
    className?: string;
    id?: string;
    allowClear?: boolean;
}

interface ComboboxSingleProps extends ComboboxBaseProps {
    multi?: false;
    value: string;
    onChange: (value: string) => void;
}

interface ComboboxMultiProps extends ComboboxBaseProps {
    multi: true;
    value: string[];
    onChange: (value: string[]) => void;
}

export type ComboboxProps = ComboboxSingleProps | ComboboxMultiProps;

const COMBOBOX_POPOVER_ESTIMATED_HEIGHT = 280;

export function Combobox(props: ComboboxProps) {
    const { options, placeholder = 'Select…', disabled, searchPlaceholder = 'Search…', emptyText = 'No matches', className, id, allowClear } = props;
    const [open, setOpen] = useState(false);
    const [query, setQuery] = useState('');
    const [activeIndex, setActiveIndex] = useState(0);
    const [popRect, setPopRect] = useState<{ top: number; left: number; width: number } | null>(null);
    const wrapRef = useRef<HTMLDivElement>(null);
    const triggerRef = useRef<HTMLDivElement>(null);
    const popRef = useRef<HTMLDivElement>(null);
    const inputRef = useRef<HTMLInputElement>(null);

    const listboxId = useId();
    const optionId = (i: number) => `${listboxId}-opt-${i}`;

    const isMulti = (props.multi as boolean) === true;
    const selectedValues = isMulti ? (props.value as string[]) : (props.value ? [props.value as string] : []);

    const filtered = useMemo(() => {
        const q = query.trim().toLowerCase();
        if (!q) return options;
        return options.filter(o => {
            const hay = `${o.label} ${o.keywords || ''}`.toLowerCase();
            return hay.includes(q);
        });
    }, [options, query]);

    // On open, highlight the currently-selected option so Enter just confirms
    // it and ArrowUp/Down navigates away from where the user already is. We
    // only react to `open` (not selectedValues/filtered) so this fires on the
    // open transition and doesn't fight with the [query] reset below.
    useEffect(() => {
        if (!open) return;
        const idx = filtered.findIndex(o => selectedValues.includes(o.value));
        setActiveIndex(idx >= 0 ? idx : 0);
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open]);

    useEffect(() => { setActiveIndex(0); }, [query]);

    useEffect(() => {
        if (!open) return;
        const onClickAway = (e: MouseEvent) => {
            const target = e.target as Node;
            if (wrapRef.current && wrapRef.current.contains(target)) return;
            if (popRef.current && popRef.current.contains(target)) return;
            setOpen(false);
        };
        document.addEventListener('mousedown', onClickAway);
        return () => document.removeEventListener('mousedown', onClickAway);
    }, [open]);

    useEffect(() => {
        if (!open) { setPopRect(null); setQuery(''); return; }
        const update = () => {
            const el = triggerRef.current;
            if (!el) return;
            const r = el.getBoundingClientRect();
            // Prefer the measured popover height once it has rendered; before
            // that, fall back to a sane estimate so the first paint already
            // flips when it needs to.
            const measured = popRef.current?.offsetHeight;
            const popHeight = measured && measured > 0 ? measured : COMBOBOX_POPOVER_ESTIMATED_HEIGHT;
            const wouldOverflow = r.bottom + popHeight > window.innerHeight - 8;
            const top = wouldOverflow
                ? Math.max(8, r.top - popHeight - 4)
                : r.bottom + 4;
            setPopRect({ top, left: r.left, width: r.width });
        };
        update();
        // Re-measure after the popover renders to flip when our estimate was wrong.
        const raf = requestAnimationFrame(update);
        window.addEventListener('scroll', update, true);
        window.addEventListener('resize', update);
        setTimeout(() => inputRef.current?.focus(), 30);
        return () => {
            cancelAnimationFrame(raf);
            window.removeEventListener('scroll', update, true);
            window.removeEventListener('resize', update);
        };
    }, [open]);

    const toggleValue = (v: string) => {
        if (isMulti) {
            const current = selectedValues;
            const next = current.includes(v) ? current.filter(x => x !== v) : [...current, v];
            (props.onChange as (value: string[]) => void)(next);
        } else {
            (props.onChange as (value: string) => void)(v);
            setOpen(false);
            // Return focus to the trigger so keyboard users can immediately
            // reopen the combobox with Space/Enter (matches Escape behaviour).
            triggerRef.current?.focus();
        }
    };

    const onKeyDown = (e: React.KeyboardEvent) => {
        if (e.key === 'Escape') { setOpen(false); triggerRef.current?.focus(); return; }
        if (e.key === 'ArrowDown') { e.preventDefault(); setActiveIndex(i => Math.min(filtered.length - 1, i + 1)); return; }
        if (e.key === 'ArrowUp') { e.preventDefault(); setActiveIndex(i => Math.max(0, i - 1)); return; }
        if (e.key === 'Enter') {
            e.preventDefault();
            const opt = filtered[activeIndex];
            if (opt) toggleValue(opt.value);
        }
    };

    const optionFor = (v: string) => options.find(o => o.value === v);
    const labelFor = (v: string) => optionFor(v)?.label || v;

    let triggerContent: React.ReactNode;
    if (isMulti && selectedValues.length > 0) {
        triggerContent = (
            <span className="r-cbox-chips">
                {selectedValues.map(v => {
                    const opt = optionFor(v);
                    return (
                        <span key={v} className="r-cbox-chip">
                            {opt?.icon && <span className="r-cbox-icon" aria-hidden>{opt.icon}</span>}
                            {labelFor(v)}
                            <button
                                type="button"
                                onClick={(e) => { e.stopPropagation(); toggleValue(v); }}
                                aria-label={`Remove ${labelFor(v)}`}
                            >
                                <Icon name="close" size={10} />
                            </button>
                        </span>
                    );
                })}
            </span>
        );
    } else if (!isMulti && props.value) {
        const opt = optionFor(props.value as string);
        triggerContent = (
            <>
                {opt?.icon && <span className="r-cbox-icon" aria-hidden>{opt.icon}</span>}
                <span className="grow">{labelFor(props.value as string)}</span>
            </>
        );
    } else {
        triggerContent = <span className="grow">{placeholder}</span>;
    }

    const isEmpty = isMulti ? selectedValues.length === 0 : !props.value;
    const showClear = allowClear && !isEmpty && !disabled;

    // Close when focus leaves both the wrapper and the portaled popover. The
    // existing mousedown click-away handles clicks; this covers keyboard Tab.
    const handleSearchBlur = () => {
        window.setTimeout(() => {
            const active = document.activeElement as Node | null;
            if (!active) { setOpen(false); return; }
            if (wrapRef.current?.contains(active)) return;
            if (popRef.current?.contains(active)) return;
            setOpen(false);
        }, 0);
    };

    return (
        <div ref={wrapRef} className={cx('r-cbox', open && 'open', className)}>
            <div
                ref={triggerRef}
                id={id}
                role="combobox"
                tabIndex={disabled ? -1 : 0}
                aria-disabled={disabled || undefined}
                className={cx('r-cbox-trigger', isEmpty && 'placeholder')}
                onClick={() => { if (!disabled) setOpen(o => !o); }}
                onKeyDown={(e) => {
                    if (disabled) return;
                    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); setOpen(o => !o); }
                    else if (e.key === 'ArrowDown') { e.preventDefault(); setOpen(true); }
                }}
                aria-haspopup="listbox"
                aria-expanded={open}
                aria-controls={listboxId}
            >
                {triggerContent}
                {showClear && (
                    <button
                        type="button"
                        className="clear"
                        aria-label="Clear selection"
                        onClick={(e) => {
                            e.stopPropagation();
                            if (isMulti) (props.onChange as (v: string[]) => void)([]);
                            else (props.onChange as (v: string) => void)('');
                        }}
                    >
                        <Icon name="close" size={11} />
                    </button>
                )}
                <Icon name="chevd" size={14} className="chev" />
            </div>
            {open && popRect && createPortal(
                <div
                    ref={popRef}
                    id={listboxId}
                    className="r-cbox-pop"
                    role="listbox"
                    style={{ position: 'fixed', top: popRect.top, left: popRect.left, width: popRect.width }}
                >
                    <div className="r-cbox-search">
                        <Icon name="search" size={13} />
                        <input
                            ref={inputRef}
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            onKeyDown={onKeyDown}
                            onBlur={handleSearchBlur}
                            placeholder={searchPlaceholder}
                            aria-controls={listboxId}
                            aria-activedescendant={filtered[activeIndex] ? optionId(activeIndex) : undefined}
                        />
                    </div>
                    <div className="r-cbox-list">
                        {filtered.length === 0 ? (
                            <div className="r-cbox-empty">{emptyText}</div>
                        ) : (
                            filtered.map((o, i) => {
                                const on = selectedValues.includes(o.value);
                                return (
                                    <div
                                        key={o.value}
                                        id={optionId(i)}
                                        className={cx('r-cbox-item', on && 'on', i === activeIndex && 'sel')}
                                        role="option"
                                        aria-selected={on}
                                        onMouseEnter={() => setActiveIndex(i)}
                                        // Keep focus on the search input during click so the
                                        // input's onBlur handler doesn't close the popover
                                        // before the click event reaches us.
                                        onMouseDown={(e) => e.preventDefault()}
                                        onClick={() => toggleValue(o.value)}
                                    >
                                        {isMulti && (
                                            <span className="check">
                                                {on && <Icon name="check" size={10} />}
                                            </span>
                                        )}
                                        {o.icon && <span className="r-cbox-icon" aria-hidden>{o.icon}</span>}
                                        {o.hint ? (
                                            <span className="r-cbox-item-text">
                                                <span className="label">{o.label}</span>
                                                <span className="hint">{o.hint}</span>
                                            </span>
                                        ) : (
                                            <span className="grow">{o.label}</span>
                                        )}
                                        {!isMulti && on && <Icon name="check" size={13} />}
                                    </div>
                                );
                            })
                        )}
                    </div>
                </div>,
                document.body
            )}
        </div>
    );
}

// Backwards-compat: keep a Select export that delegates to Combobox so existing
// imports keep working. Callers should provide options via the options prop.
export function Select(props: {
    value: string;
    onChange: (value: string) => void;
    options: ComboboxOption[];
    placeholder?: string;
    disabled?: boolean;
    id?: string;
    className?: string;
}) {
    return <Combobox {...props} />;
}

// ---------- SearchInput ----------
export function SearchInput({ value, onChange, placeholder, style }: { value: string; onChange: (v: string) => void; placeholder?: string; style?: React.CSSProperties }) {
    return (
        <div className="r-input" style={style}>
            <Icon name="search" size={13} />
            <input value={value} onChange={e => onChange(e.target.value)} placeholder={placeholder} />
        </div>
    );
}

// ---------- KV ----------
export function KV({ children, className }: { children: React.ReactNode; className?: string }) {
    return <dl className={cx('r-kv', className)}>{children}</dl>;
}
export function KVRow({ k, children }: { k: React.ReactNode; children: React.ReactNode }) {
    return <><dt>{k}</dt><dd>{children}</dd></>;
}

// ---------- Empty ----------
export function Empty({ title, children }: { title?: React.ReactNode; children?: React.ReactNode }) {
    return (
        <div className="r-empty">
            {title && <div className="empty-title">{title}</div>}
            {children}
        </div>
    );
}

// ---------- Alert ----------
export function Alert({ tone, icon, children }: { tone?: 'info' | 'warn' | 'err'; icon?: string; children: React.ReactNode }) {
    return (
        <div className={cx('r-alert', tone)}>
            {icon && <Icon name={icon} size={14} />}
            <div style={{ flex: 1 }}>{children}</div>
        </div>
    );
}

// ---------- Group bar (collapsible row header) ----------
export function GroupBar({ open, onToggle, children, right }: { open: boolean; onToggle?: () => void; children: React.ReactNode; right?: React.ReactNode }) {
    return (
        <div
            className={cx('r-group-bar', onToggle && 'expandable', open && 'open')}
            onClick={onToggle}
        >
            <div className="r-group-name">
                {onToggle && <Icon name="chev" size={12} className="chev" />}
                {children}
            </div>
            {right && <div style={{ display: 'flex', alignItems: 'center', gap: 14 }}>{right}</div>}
        </div>
    );
}

// ---------- Avatar ----------
export function Avatar({ label, color, size = 26, className }: { label: string; color?: string; size?: number; className?: string }) {
    const initial = (label || '?').trim()[0]?.toUpperCase() || '?';
    return (
        <span
            className={cx('r-ava', className)}
            style={{
                width: size,
                height: size,
                background: color || 'oklch(0.55 0.10 230)',
                fontSize: Math.round(size * 0.45),
                borderRadius: 4,
            }}
        >
            {initial}
        </span>
    );
}

// Compute a deterministic-ish color from a string.
export function colorFor(seed: string): string {
    let h = 0;
    for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) | 0;
    const hue = Math.abs(h) % 360;
    return `oklch(0.62 0.13 ${hue})`;
}

// ---------- Confirm dialog (delete confirmation) ----------
export function ConfirmDialog({ isOpen, onClose, onConfirm, title, message, confirmText = 'Confirm', confirmTone = 'danger', loading, requireText }: {
    isOpen: boolean;
    onClose: () => void;
    onConfirm: () => void;
    title: string;
    message: React.ReactNode;
    confirmText?: string;
    confirmTone?: 'primary' | 'danger';
    loading?: boolean;
    requireText?: string;
}) {
    const [text, setText] = React.useState('');
    React.useEffect(() => { if (!isOpen) setText(''); }, [isOpen]);
    const canConfirm = !requireText || text === requireText;
    return (
        <Modal
            isOpen={isOpen}
            onClose={loading ? () => undefined : onClose}
            title={title}
            footer={
                <>
                    <Button onClick={onClose} disabled={loading}>Cancel</Button>
                    <Button variant={confirmTone} onClick={onConfirm} disabled={!canConfirm || loading} loading={loading}>{confirmText}</Button>
                </>
            }
        >
            <div>{message}</div>
            {requireText && (
                <Field label={<>Type <span className="mono" style={{ color: 'var(--text)' }}>{requireText}</span> to confirm.</>}>
                    <Input value={text} onChange={e => setText(e.target.value)} autoFocus />
                </Field>
            )}
        </Modal>
    );
}

// ---------- AutocompleteInput ----------
// Freeform text input with a portaled suggestion dropdown. Unlike `Combobox`,
// this lets users type values that aren't in the suggestion list — useful for
// email entry where the suggestion list is an autocomplete helper, not a
// constraint.
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
}: {
    id?: string;
    value: string;
    onChange: (v: string) => void;
    options?: string[];
    type?: string;
    placeholder?: string;
    disabled?: boolean;
    loading?: boolean;
    multiValue?: boolean;
    noMatchesText?: string;
    className?: string;
    onEnter?: () => void;
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
                    // Above legacy.css `.modal-backdrop` (9998) and modal content (9999) so the
                    // suggestion list stays visible when AutocompleteInput is rendered in a legacy modal.
                    style={{ position: 'fixed', top: popRect.top, left: popRect.left, width: popRect.width, zIndex: 10000 }}
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

// ---------- Tooltip ----------
// Custom hover/focus tooltip, replaces native `title` attributes. The bubble is
// portaled so it is never clipped by a surrounding overflow container.
export function Tooltip({ content, children }: { content: React.ReactNode; children: React.ReactNode }) {
    const [pos, setPos] = React.useState<{ top: number; left: number } | null>(null);
    const ref = React.useRef<HTMLSpanElement | null>(null);
    const show = () => {
        const el = ref.current;
        if (!el) return;
        const r = el.getBoundingClientRect();
        setPos({ top: r.bottom + 6, left: r.left + r.width / 2 });
    };
    const hide = () => setPos(null);
    return (
        <span
            ref={ref}
            className="r-tip"
            tabIndex={0}
            onMouseEnter={show}
            onMouseLeave={hide}
            onFocus={show}
            onBlur={hide}
        >
            {children}
            {pos && createPortal(
                <span
                    className="r-tip-pop"
                    role="tooltip"
                    style={{ position: 'fixed', top: pos.top, left: pos.left, transform: 'translateX(-50%)' }}
                >
                    {content}
                </span>,
                document.body
            )}
        </span>
    );
}
