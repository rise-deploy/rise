import { useEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './icon';
import { Button as RButton, Empty, Panel, Pill, Segmented } from './r-ui';

export interface MenuItem {
    label: string;
    icon?: string;
    onClick: () => void;
}

export interface MenuProps {
    trigger: (args: { toggle: () => void; open: boolean }) => ReactNode;
    items: MenuItem[];
}

// Generic dropdown menu. `trigger` is a render prop receiving { toggle, open };
// the item list is portaled and fixed-positioned so it is never clipped by a
// surrounding panel/overflow container.
export function Menu({ trigger, items }: MenuProps) {
    const [open, setOpen] = useState(false);
    const [rect, setRect] = useState<{ top: number; right: number } | null>(null);
    const wrapRef = useRef<HTMLDivElement | null>(null);
    const popRef = useRef<HTMLDivElement | null>(null);

    useEffect(() => {
        if (!open) { setRect(null); return; }
        const update = () => {
            const el = wrapRef.current;
            if (!el) return;
            const r = el.getBoundingClientRect();
            setRect({ top: r.bottom + 4, right: window.innerWidth - r.right });
        };
        update();
        window.addEventListener('scroll', update, true);
        window.addEventListener('resize', update);
        return () => {
            window.removeEventListener('scroll', update, true);
            window.removeEventListener('resize', update);
        };
    }, [open]);

    useEffect(() => {
        if (!open) return;
        const onClick = (e: MouseEvent) => {
            const target = e.target as Node | null;
            if (target && wrapRef.current && wrapRef.current.contains(target)) return;
            if (target && popRef.current && popRef.current.contains(target)) return;
            setOpen(false);
        };
        document.addEventListener('mousedown', onClick);
        return () => document.removeEventListener('mousedown', onClick);
    }, [open]);

    return (
        <div ref={wrapRef} style={{ position: 'relative', display: 'inline-block' }}>
            {trigger({ toggle: () => setOpen(o => !o), open })}
            {open && rect && createPortal(
                <div
                    ref={popRef}
                    style={{
                        position: 'fixed',
                        top: rect.top,
                        right: rect.right,
                        zIndex: 100,
                        minWidth: 180,
                        background: 'var(--surface)',
                        border: '1px solid var(--border)',
                        borderRadius: 'var(--radius-sm)',
                        boxShadow: 'var(--shadow-md, 0 8px 24px rgba(0,0,0,0.18))',
                        padding: 4,
                    }}
                >
                    {items.map(item => (
                        <button
                            key={item.label}
                            type="button"
                            className="r-menu-item"
                            style={{
                                display: 'flex',
                                alignItems: 'center',
                                gap: 8,
                                width: '100%',
                                padding: '7px 10px',
                                background: 'transparent',
                                border: 'none',
                                borderRadius: 'var(--radius-xs, 4px)',
                                cursor: 'pointer',
                                fontSize: 13,
                                color: 'var(--text)',
                                textAlign: 'left',
                            }}
                            onMouseEnter={(e) => { (e.currentTarget as HTMLButtonElement).style.background = 'var(--surface-2)'; }}
                            onMouseLeave={(e) => { (e.currentTarget as HTMLButtonElement).style.background = 'transparent'; }}
                            onClick={() => { setOpen(false); item.onClick(); }}
                        >
                            {item.icon && <Icon name={item.icon} size={14} />}
                            {item.label}
                        </button>
                    ))}
                </div>,
                document.body
            )}
        </div>
    );
}

// "Add" split button — a primary button that opens a dropdown of choices.
export function AddMenu({ items }: { items: MenuItem[] }) {
    return (
        <Menu
            items={items}
            trigger={({ toggle }) => (
                <RButton variant="primary" icon="plus" onClick={toggle}>Add</RButton>
            )}
        />
    );
}

export interface RosterRow {
    key: string;
    icon?: string;
    name: ReactNode;
    kindLabel?: ReactNode | ReactNode[];
    badge?: ReactNode;
    extra?: ReactNode;
    actions?: ReactNode;
}

export interface RosterFilter<T extends string = string> {
    value: T;
    options: { value: T; label: string }[] | T[];
    onChange: (value: T) => void;
}

export interface RosterTableProps {
    title: ReactNode;
    sub?: ReactNode;
    addControl?: ReactNode;
    filter?: RosterFilter;
    rows: RosterRow[];
    extraColumnLabel?: ReactNode;
    emptyText?: ReactNode;
}

// Reusable "roster" table — a section header with optional add control and
// type filter, followed by a panel with a unified subject/kind/actions table.
//
// rows: { key, icon, name, kindLabel, badge?, extra?, actions? }[]
// kindLabel may be a string or an array of strings (rendered as multiple pills).
export function RosterTable({ title, sub, addControl, filter, rows, extraColumnLabel, emptyText }: RosterTableProps) {
    return (
        <>
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">{title}</div>
                    {sub && <div className="r-section-sub">{sub}</div>}
                </div>
                {(filter || addControl) && (
                    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
                        {filter && (
                            <Segmented value={filter.value} options={filter.options} onChange={filter.onChange} />
                        )}
                        {addControl}
                    </div>
                )}
            </div>

            <Panel>
                {rows.length > 0 ? (
                    <table className="r-table">
                        <thead>
                            <tr>
                                <th>Subject</th>
                                <th>Kind</th>
                                {extraColumnLabel && <th>{extraColumnLabel}</th>}
                                <th style={{ textAlign: 'right' }}>Actions</th>
                            </tr>
                        </thead>
                        <tbody>
                            {rows.map(row => (
                                <tr key={row.key}>
                                    <td>
                                        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                                            {row.icon && <Icon name={row.icon} size={14} />}
                                            <span style={{ fontWeight: 500 }}>{row.name}</span>
                                            {row.badge && <Pill>{row.badge}</Pill>}
                                        </span>
                                    </td>
                                    <td>
                                        <span style={{ display: 'inline-flex', gap: 6, flexWrap: 'wrap' }}>
                                            {(Array.isArray(row.kindLabel) ? row.kindLabel : [row.kindLabel])
                                                .filter(Boolean)
                                                .map((k, i) => <Pill key={typeof k === 'string' ? k : i}>{k}</Pill>)}
                                        </span>
                                    </td>
                                    {extraColumnLabel && (
                                        <td>
                                            {row.extra != null && row.extra !== ''
                                                ? row.extra
                                                : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                        </td>
                                    )}
                                    <td style={{ textAlign: 'right' }}>
                                        {row.actions && (
                                            <div className="row-actions" style={{ display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
                                                {row.actions}
                                            </div>
                                        )}
                                    </td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                ) : (
                    <div className="r-panel-body">
                        <Empty title={emptyText} />
                    </div>
                )}
            </Panel>
        </>
    );
}
