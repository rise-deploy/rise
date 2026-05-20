// @ts-nocheck
import { useEffect, useRef, useState } from 'react';
import { Icon } from './icon';
import { Button as RButton, Empty, Panel, Pill, Segmented } from './r-ui';

// Small inline dropdown menu (button + absolutely-positioned list, close on click-away).
export function AddMenu({ items }) {
    const [open, setOpen] = useState(false);
    const ref = useRef(null);

    useEffect(() => {
        if (!open) return;
        const onClick = (e) => {
            if (ref.current && !ref.current.contains(e.target)) setOpen(false);
        };
        document.addEventListener('mousedown', onClick);
        return () => document.removeEventListener('mousedown', onClick);
    }, [open]);

    return (
        <div ref={ref} style={{ position: 'relative', display: 'inline-block' }}>
            <RButton variant="primary" icon="plus" onClick={() => setOpen(o => !o)}>Add</RButton>
            {open && (
                <div
                    style={{
                        position: 'absolute',
                        top: 'calc(100% + 4px)',
                        right: 0,
                        zIndex: 30,
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
                            onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--surface-2)'; }}
                            onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}
                            onClick={() => { setOpen(false); item.onClick(); }}
                        >
                            {item.icon && <Icon name={item.icon} size={14} />}
                            {item.label}
                        </button>
                    ))}
                </div>
            )}
        </div>
    );
}

// Reusable "roster" table — a section header with optional add control and
// type filter, followed by a panel with a unified subject/kind/actions table.
//
// rows: { key, icon, name, kindLabel, badge?, extra?, actions? }[]
export function RosterTable({ title, sub, addControl, filter, rows, extraColumnLabel, emptyText }) {
    return (
        <>
            <div className="r-section-head">
                <div>
                    <div className="r-section-title">{title}</div>
                    {sub && <div className="r-section-sub">{sub}</div>}
                </div>
                {addControl && <div>{addControl}</div>}
            </div>

            {filter && (
                <div style={{ marginBottom: 14 }}>
                    <Segmented value={filter.value} options={filter.options} onChange={filter.onChange} />
                </div>
            )}

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
                                    <td><Pill>{row.kindLabel}</Pill></td>
                                    {extraColumnLabel && (
                                        <td>
                                            {row.extra != null && row.extra !== ''
                                                ? row.extra
                                                : <span style={{ color: 'var(--text-soft)' }}>—</span>}
                                        </td>
                                    )}
                                    <td style={{ textAlign: 'right' }}>{row.actions}</td>
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
