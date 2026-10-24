import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './icon';
import { cx } from './r-ui';
import { useIsMobile } from '../lib/use-media';

export type CommandKind = 'Action' | 'Project' | 'Team' | 'Go to';

export type CommandItem = {
    id: string;
    label: string;
    kind: CommandKind;
    icon: string;
    /** Secondary text after the label, e.g. a project's owner. */
    sub?: string;
    keywords?: string[];
    run: () => void;
};

const MAX_RESULTS = 12;

export function CommandPalette({
    isOpen,
    onClose,
    items,
    scope,
}: {
    isOpen: boolean;
    onClose: () => void;
    items: CommandItem[];
    /** Restrict the palette to one kind, e.g. the sidebar's project switcher. */
    scope?: 'projects';
}) {
    const [query, setQuery] = useState('');
    const [activeIndex, setActiveIndex] = useState(0);
    const inputRef = useRef<HTMLInputElement>(null);
    const listRef = useRef<HTMLDivElement>(null);
    const isMobile = useIsMobile();

    useEffect(() => {
        if (isOpen) {
            setQuery('');
            setActiveIndex(0);
            setTimeout(() => inputRef.current?.focus(), 30);
        }
    }, [isOpen]);

    const results = useMemo(() => {
        const pool = scope === 'projects' ? items.filter(i => i.kind === 'Project') : items;
        const q = query.trim().toLowerCase();
        const matched = q
            ? pool.filter(item => [item.label, item.sub, item.kind, ...(item.keywords || [])].filter(Boolean).join(' ').toLowerCase().includes(q))
            : pool;
        return matched.slice(0, scope === 'projects' ? matched.length : MAX_RESULTS);
    }, [items, query, scope]);

    useEffect(() => {
        if (!isOpen) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === 'Escape') { onClose(); return; }
            if (e.key === 'ArrowDown') {
                e.preventDefault();
                setActiveIndex(prev => Math.min(prev + 1, Math.max(0, results.length - 1)));
                return;
            }
            if (e.key === 'ArrowUp') {
                e.preventDefault();
                setActiveIndex(prev => Math.max(prev - 1, 0));
                return;
            }
            if (e.key === 'Enter' && results[activeIndex]) {
                e.preventDefault();
                onClose();
                results[activeIndex].run();
            }
        };
        window.addEventListener('keydown', onKey);
        return () => window.removeEventListener('keydown', onKey);
    }, [isOpen, activeIndex, results, onClose]);

    useEffect(() => {
        listRef.current?.querySelector<HTMLElement>('.r-cmdp-item.sel')?.scrollIntoView({ block: 'nearest' });
    }, [activeIndex]);

    if (!isOpen) return null;

    const node = (
        <div className={cx('r-cmdp-mask', isMobile && 'is-mobile')} onClick={onClose}>
            <div className="r-cmdp" role="dialog" aria-modal="true" aria-label="Command palette" onClick={e => e.stopPropagation()}>
                <div className="r-cmdp-input">
                    <Icon name="search" size={17} />
                    <input
                        ref={inputRef}
                        aria-label="Search or run a command"
                        value={query}
                        onChange={e => { setQuery(e.target.value); setActiveIndex(0); }}
                        placeholder={scope === 'projects' ? 'Switch to project…' : 'Search or run a command…'}
                    />
                    {isMobile
                        ? <button type="button" className="r-cmdp-cancel" onClick={onClose}>Cancel</button>
                        : <kbd>Esc</kbd>}
                </div>
                <div className="r-cmdp-list" ref={listRef}>
                    {results.length === 0 ? (
                        <div className="r-cmdp-empty">No matches.</div>
                    ) : results.map((item, idx) => (
                        <button
                            type="button"
                            key={item.id}
                            className={cx('r-cmdp-item', idx === activeIndex && 'sel')}
                            onMouseEnter={() => setActiveIndex(idx)}
                            onClick={() => { onClose(); item.run(); }}
                        >
                            <Icon name={item.icon} size={15} />
                            <span className="label">{item.label}</span>
                            {item.sub && <span className="sub">{item.sub}</span>}
                            <span className="kind">{item.kind}</span>
                        </button>
                    ))}
                </div>
                {!isMobile && (
                    <div className="r-cmdp-foot">
                        <span><kbd>↑↓</kbd> navigate</span>
                        <span><kbd>↵</kbd> select</span>
                        <span><kbd>Esc</kbd> close</span>
                    </div>
                )}
            </div>
        </div>
    );
    return createPortal(node, document.body);
}
