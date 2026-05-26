import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './icon';

export type CommandItem = {
    id: string;
    label: string;
    group?: string;
    hint?: string;
    keywords?: string[];
    run: () => void;
};

export function CommandPalette({
    isOpen,
    onClose,
    items,
}: {
    isOpen: boolean;
    onClose: () => void;
    items: CommandItem[];
}) {
    const [query, setQuery] = useState('');
    const [activeIndex, setActiveIndex] = useState(0);
    const inputRef = useRef<HTMLInputElement>(null);

    useEffect(() => {
        if (isOpen) {
            setQuery('');
            setActiveIndex(0);
            setTimeout(() => inputRef.current?.focus(), 30);
        }
    }, [isOpen]);

    const filteredItems = useMemo(() => {
        const q = query.trim().toLowerCase();
        if (!q) return items;
        return items.filter((item) => {
            const haystack = [item.label, item.group, item.hint, ...(item.keywords || [])].filter(Boolean).join(' ').toLowerCase();
            return haystack.includes(q);
        });
    }, [items, query]);

    useEffect(() => {
        if (!isOpen) return;

        const onKey = (e: KeyboardEvent) => {
            if (e.key === 'Escape') { onClose(); return; }
            if (e.key === 'ArrowDown') {
                e.preventDefault();
                setActiveIndex((prev) => Math.min(prev + 1, Math.max(0, filteredItems.length - 1)));
                return;
            }
            if (e.key === 'ArrowUp') {
                e.preventDefault();
                setActiveIndex((prev) => Math.max(prev - 1, 0));
                return;
            }
            if (e.key === 'Enter' && filteredItems[activeIndex]) {
                e.preventDefault();
                filteredItems[activeIndex].run();
                onClose();
            }
        };

        window.addEventListener('keydown', onKey);
        return () => window.removeEventListener('keydown', onKey);
    }, [isOpen, activeIndex, filteredItems, onClose]);

    if (!isOpen) return null;

    // Group items into ordered groups.
    const groups: { name: string; items: { item: CommandItem; globalIndex: number }[] }[] = [];
    const seen = new Map<string, { item: CommandItem; globalIndex: number }[]>();
    filteredItems.forEach((item, idx) => {
        const groupName = item.group || 'Other';
        let bucket = seen.get(groupName);
        if (!bucket) {
            bucket = [];
            seen.set(groupName, bucket);
            groups.push({ name: groupName, items: bucket });
        }
        bucket.push({ item, globalIndex: idx });
    });

    const node = (
        <div className="r-cmdp-mask" onClick={onClose}>
            <div className="r-cmdp" onClick={(e) => e.stopPropagation()}>
                <div className="r-cmdp-input">
                    <Icon name="search" size={16} />
                    <input
                        ref={inputRef}
                        aria-label="Command palette"
                        value={query}
                        onChange={(e) => { setQuery(e.target.value); setActiveIndex(0); }}
                        placeholder="Search projects, teams, actions…"
                    />
                    <span style={{ fontSize: 11, color: 'var(--text-soft)' }}>
                        {filteredItems.length} result{filteredItems.length === 1 ? '' : 's'}
                    </span>
                </div>
                <div className="r-cmdp-list">
                    {filteredItems.length === 0 ? (
                        <div style={{ padding: 24, textAlign: 'center', color: 'var(--text-soft)', fontSize: 13 }}>No matches.</div>
                    ) : (
                        groups.map(g => (
                            <div key={g.name}>
                                <div className="r-cmdp-group">{g.name}</div>
                                {g.items.map(({ item, globalIndex }) => (
                                    <div
                                        key={item.id}
                                        className={`r-cmdp-item ${globalIndex === activeIndex ? 'sel' : ''}`}
                                        onMouseEnter={() => setActiveIndex(globalIndex)}
                                        onClick={() => { item.run(); onClose(); }}
                                    >
                                        <div className="grow">{item.label}</div>
                                        {item.hint && <div style={{ fontSize: 11.5, color: 'var(--text-soft)' }}>{item.hint}</div>}
                                    </div>
                                ))}
                            </div>
                        ))
                    )}
                </div>
                <div className="r-cmdp-foot">
                    <span><kbd>↑↓</kbd> navigate</span>
                    <span><kbd>↵</kbd> select</span>
                    <span><kbd>Esc</kbd> close</span>
                </div>
            </div>
        </div>
    );
    return createPortal(node, document.body);
}
