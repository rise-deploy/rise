import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from '../../components/icon';
import { levelLabel, orderedLevels } from './format';
import { formatQuery, parseQuery, removeFilter, toggleFilter } from './query';

/**
 * The console's single filter surface: active filters render as removable chips
 * in front of a free-text field, and the caret opens a menu so every filter is
 * reachable without knowing the token syntax.
 */
export interface QueryLineProps {
    value: string;
    onChange: (next: string) => void;
    /** Level vocabulary advertised by the configured backend. */
    levels: string[];
    /** Container names on this deployment, if it declares any. */
    containers: string[];
    inputRef?: React.RefObject<HTMLInputElement | null>;
    /** Match navigation, shown once a search term is active. */
    matchCount?: number;
    matchPosition?: number;
    onPrevMatch?: () => void;
    onNextMatch?: () => void;
}

export function QueryLine({
    value,
    onChange,
    levels,
    containers,
    inputRef,
    matchCount = 0,
    matchPosition = 0,
    onPrevMatch,
    onNextMatch,
}: QueryLineProps) {
    const query = parseQuery(value);
    const [menuOpen, setMenuOpen] = useState(false);
    const triggerRef = useRef<HTMLButtonElement>(null);
    const menuRef = useRef<HTMLDivElement>(null);
    const [menuRect, setMenuRect] = useState<{ top: number; left: number } | null>(null);

    useEffect(() => {
        if (!menuOpen) return undefined;
        const place = () => {
            const rect = triggerRef.current?.getBoundingClientRect();
            if (rect) setMenuRect({ top: rect.bottom + 6, left: rect.left });
        };
        place();
        const onPointerDown = (e: MouseEvent) => {
            const target = e.target as Node;
            if (menuRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
            setMenuOpen(false);
        };
        const onKeyDown = (e: KeyboardEvent) => {
            if (e.key === 'Escape') setMenuOpen(false);
        };
        window.addEventListener('scroll', place, true);
        window.addEventListener('resize', place);
        document.addEventListener('mousedown', onPointerDown);
        document.addEventListener('keydown', onKeyDown);
        return () => {
            window.removeEventListener('scroll', place, true);
            window.removeEventListener('resize', place);
            document.removeEventListener('mousedown', onPointerDown);
            document.removeEventListener('keydown', onKeyDown);
        };
    }, [menuOpen]);

    const chips: { field: 'level' | 'container'; value: string }[] = [
        ...query.levels.map((v) => ({ field: 'level' as const, value: v })),
        ...query.containers.map((v) => ({ field: 'container' as const, value: v })),
    ];

    return (
        <div className="r-logc-query">
            <button
                ref={triggerRef}
                type="button"
                className={menuOpen ? 'r-logc-query-menu-btn is-open' : 'r-logc-query-menu-btn'}
                onClick={() => setMenuOpen((v) => !v)}
                aria-expanded={menuOpen}
                aria-label="Filters"
                title="Filters"
            >
                <Icon name="chevd" size={12} />
            </button>

            {chips.map((chip) => (
                <span key={`${chip.field}:${chip.value}`} className={`r-logc-chip is-${chip.field}`}>
                    <span className="r-logc-chip-field">{chip.field}</span>
                    <span className="r-logc-chip-value">{chip.value}</span>
                    <button
                        type="button"
                        onClick={() => onChange(removeFilter(value, chip.field, chip.value))}
                        aria-label={`Remove ${chip.field} filter ${chip.value}`}
                    >
                        <Icon name="close" size={9} />
                    </button>
                </span>
            ))}

            <input
                ref={inputRef}
                type="text"
                className="r-logc-query-input"
                value={query.search}
                placeholder={chips.length > 0 ? 'Filter these lines…' : 'Search lines, or type level:error'}
                aria-label="Log query"
                onChange={(e) => onChange(formatQuery({ ...query, search: e.target.value }))}
                onKeyDown={(e) => {
                    // Backspace in an empty field peels off the last chip, the
                    // way a tag input behaves.
                    if (e.key === 'Backspace' && query.search === '' && chips.length > 0) {
                        const last = chips[chips.length - 1];
                        onChange(removeFilter(value, last.field, last.value));
                    }
                    if (e.key === 'Enter' && matchCount > 0) {
                        e.preventDefault();
                        (e.shiftKey ? onPrevMatch : onNextMatch)?.();
                    }
                }}
            />

            {query.search && (
                <span className="r-logc-matches">
                    <span className="r-logc-matches-count">
                        {matchCount === 0 ? 'No matches' : `${matchPosition + 1} of ${matchCount}`}
                    </span>
                    <button type="button" onClick={onPrevMatch} disabled={matchCount === 0} aria-label="Previous match">
                        <Icon name="chevu" size={11} />
                    </button>
                    <button type="button" onClick={onNextMatch} disabled={matchCount === 0} aria-label="Next match">
                        <Icon name="chevd" size={11} />
                    </button>
                </span>
            )}

            {menuOpen && menuRect && createPortal(
                <div ref={menuRef} className="r-logc-query-menu" style={{ top: menuRect.top, left: menuRect.left }}>
                    <div className="r-logc-query-menu-group">Level</div>
                    {orderedLevels(levels).map((level) => (
                        <button
                            key={level}
                            type="button"
                            className={query.levels.includes(level) ? 'r-logc-query-menu-item on' : 'r-logc-query-menu-item'}
                            onClick={() => onChange(toggleFilter(value, 'level', level))}
                        >
                            <span className={`r-logc-level-dot lv-${level}`} />
                            {levelLabel(level)}
                        </button>
                    ))}
                    {containers.length > 0 && (
                        <>
                            <div className="r-logc-query-menu-group">Container</div>
                            {containers.map((container) => (
                                <button
                                    key={container}
                                    type="button"
                                    className={query.containers.includes(container) ? 'r-logc-query-menu-item on' : 'r-logc-query-menu-item'}
                                    onClick={() => onChange(toggleFilter(value, 'container', container))}
                                >
                                    {container}
                                </button>
                            ))}
                        </>
                    )}
                </div>,
                document.body,
            )}
        </div>
    );
}
