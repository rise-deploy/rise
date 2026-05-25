import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
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

export function EnvPill({ env, color }: { env: string; color?: string }) {
    const kind = env === 'production' ? 'env-prod' : env === 'staging' ? 'env-staging' : 'env-global';
    return (
        <Pill kind={kind}>
            <EnvironmentIcon color={color} />
            {env}
        </Pill>
    );
}

// Tinted layer glyph used inside `EnvPill` (and standalone where an env is
// shown without a surrounding pill). Color falls back to `currentColor` so it
// inherits the pill's text color when no env color is supplied.
export function EnvironmentIcon({ color, size = 11 }: { color?: string; size?: number }) {
    const fill = color ? (ENV_ICON_COLORS[color] || color) : 'currentColor';
    return (
        <span
            style={{ display: 'inline-flex', color: fill, flexShrink: 0, lineHeight: 0 }}
            aria-hidden
        >
            <Icon name="layer" size={size} />
        </span>
    );
}

// Resolved palette for the named env colors. Kept here so r-ui has no
// dependency on the `ui.tsx` ENV_COLOR_STYLES table.
const ENV_ICON_COLORS: Record<string, string> = {
    green:  '#34d399',
    blue:   '#60a5fa',
    yellow: '#fbbf24',
    red:    '#f87171',
    purple: '#a78bfa',
    orange: '#fb923c',
    gray:   '#9ca3af',
};

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
export function Modal({ isOpen, onClose, title, sub, children, footer, width }: { isOpen: boolean; onClose: () => void; title?: React.ReactNode; sub?: React.ReactNode; children: React.ReactNode; footer?: React.ReactNode; width?: 'default' | 'wide' | 'xwide' }) {
    useEffect(() => {
        if (!isOpen) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === 'Escape') onClose();
        };
        window.addEventListener('keydown', handler);
        return () => window.removeEventListener('keydown', handler);
    }, [isOpen, onClose]);

    if (!isOpen) return null;

    const node = (
        <div className="r-modal-mask" onClick={onClose}>
            <div className={cx('r-modal', width === 'wide' && 'wide', width === 'xwide' && 'xwide')} onClick={e => e.stopPropagation()}>
                {(title || sub) && (
                    <div className="r-modal-head">
                        {title && <div className="r-modal-title">{title}</div>}
                        {sub && <div className="r-modal-sub">{sub}</div>}
                    </div>
                )}
                <div className="r-modal-body">{children}</div>
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

    useEffect(() => { setActiveIndex(0); }, [query, open]);

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
            setPopRect({ top: r.bottom + 4, left: r.left, width: r.width });
        };
        update();
        window.addEventListener('scroll', update, true);
        window.addEventListener('resize', update);
        setTimeout(() => inputRef.current?.focus(), 30);
        return () => {
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

    const labelFor = (v: string) => options.find(o => o.value === v)?.label || v;

    let triggerContent: React.ReactNode;
    if (isMulti && selectedValues.length > 0) {
        triggerContent = (
            <span className="r-cbox-chips">
                {selectedValues.map(v => (
                    <span key={v} className="r-cbox-chip">
                        {labelFor(v)}
                        <button
                            type="button"
                            onClick={(e) => { e.stopPropagation(); toggleValue(v); }}
                            aria-label={`Remove ${labelFor(v)}`}
                        >
                            <Icon name="close" size={10} />
                        </button>
                    </span>
                ))}
            </span>
        );
    } else if (!isMulti && props.value) {
        triggerContent = <span className="grow">{labelFor(props.value as string)}</span>;
    } else {
        triggerContent = <span className="grow">{placeholder}</span>;
    }

    const isEmpty = isMulti ? selectedValues.length === 0 : !props.value;
    const showClear = allowClear && !isEmpty && !disabled;

    return (
        <div ref={wrapRef} className={cx('r-cbox', open && 'open', className)}>
            <div
                ref={triggerRef}
                id={id}
                role="button"
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
                            placeholder={searchPlaceholder}
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
                                        className={cx('r-cbox-item', on && 'on', i === activeIndex && 'sel')}
                                        role="option"
                                        aria-selected={on}
                                        onMouseEnter={() => setActiveIndex(i)}
                                        onClick={() => toggleValue(o.value)}
                                    >
                                        {isMulti && (
                                            <span className="check">
                                                {on && <Icon name="check" size={10} />}
                                            </span>
                                        )}
                                        <span className="grow">{o.label}</span>
                                        {o.hint && <span className="hint">{o.hint}</span>}
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
