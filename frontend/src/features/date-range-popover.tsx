// @ts-nocheck
import { useEffect, useRef, useState } from 'react';
import { DayPicker } from 'react-day-picker';
import 'react-day-picker/style.css';
import { Button as RButton } from '../components/r-ui';

function formatDateTimeShort(date) {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return '';
    const pad = (n) => String(n).padStart(2, '0');
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function formatTimeHm(date) {
    if (!(date instanceof Date) || Number.isNaN(date.getTime())) return '00:00';
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function applyTimeOfDay(target, source) {
    if (!(target instanceof Date)) return null;
    const next = new Date(target);
    if (source instanceof Date && !Number.isNaN(source.getTime())) {
        next.setHours(source.getHours(), source.getMinutes(), 0, 0);
    }
    return next;
}

function applyTimeString(date, hhmm) {
    if (!(date instanceof Date)) return date;
    const match = /^(\d{1,2}):(\d{2})$/.exec(hhmm || '');
    if (!match) return date;
    const next = new Date(date);
    next.setHours(Math.min(23, Number(match[1])), Math.min(59, Number(match[2])), 0, 0);
    return next;
}

export default function DateRangePopover({ start, end, onChange }) {
    const [open, setOpen] = useState(false);
    const rootRef = useRef(null);

    useEffect(() => {
        if (!open) return undefined;
        const onDocMouseDown = (e) => {
            if (rootRef.current && !rootRef.current.contains(e.target)) setOpen(false);
        };
        const onKey = (e) => { if (e.key === 'Escape') setOpen(false); };
        document.addEventListener('mousedown', onDocMouseDown);
        document.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDocMouseDown);
            document.removeEventListener('keydown', onKey);
        };
    }, [open]);

    const label = start && end
        ? `${formatDateTimeShort(start)} → ${formatDateTimeShort(end)}`
        : 'Select range';

    // We drive selection from `onDayClick` rather than the built-in
    // mode="range" `onSelect` so that clicking inside an already-complete
    // range starts a fresh selection (the default behavior tries to extend
    // the existing range instead).
    const handleDayClick = (day) => {
        if (!start || (start && end)) {
            // Fresh first click - either we have no selection, or we already
            // have a complete one and the user wants to start over.
            onChange(applyTimeOfDay(day, start), null);
            return;
        }
        // We have `start` only; this click finishes the range.
        let s = start;
        let e = day;
        if (day.getTime() < start.getTime()) {
            s = day;
            e = start;
        }
        onChange(applyTimeOfDay(s, start), applyTimeOfDay(e, end));
    };

    return (
        <div ref={rootRef} className="r-date-range">
            <button
                type="button"
                className="r-field r-date-range-trigger"
                onClick={() => setOpen((v) => !v)}
            >
                {label}
            </button>
            {open && (
                <div className="r-date-range-popover">
                    <DayPicker
                        mode="range"
                        numberOfMonths={2}
                        selected={{ from: start || undefined, to: end || undefined }}
                        onDayClick={handleDayClick}
                        defaultMonth={start || new Date()}
                    />
                    <div className="r-date-range-times">
                        <label>
                            <span>From</span>
                            <input
                                type="time"
                                value={formatTimeHm(start)}
                                onChange={(e) => onChange(applyTimeString(start, e.target.value), end)}
                                className="r-field"
                            />
                        </label>
                        <label>
                            <span>To</span>
                            <input
                                type="time"
                                value={formatTimeHm(end)}
                                onChange={(e) => onChange(start, applyTimeString(end, e.target.value))}
                                className="r-field"
                            />
                        </label>
                        <RButton size="sm" variant="ghost" onClick={() => onChange(null, null)}>Clear</RButton>
                        <RButton size="sm" onClick={() => setOpen(false)}>Done</RButton>
                    </div>
                </div>
            )}
        </div>
    );
}
