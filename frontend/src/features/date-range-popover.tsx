// @ts-nocheck
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
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

function applyTimeFromHm(target, hours, minutes) {
    if (!(target instanceof Date)) return null;
    const next = new Date(target);
    next.setHours(hours, minutes, 0, 0);
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

function parseHm(hhmm, fallback) {
    const match = /^(\d{1,2}):(\d{2})$/.exec(hhmm || '');
    if (!match) return fallback;
    return {
        hours: Math.min(23, Number(match[1])),
        minutes: Math.min(59, Number(match[2])),
    };
}

export default function DateRangePopover({ start, end, onChange, disabled }) {
    const [open, setOpen] = useState(false);
    const [popRect, setPopRect] = useState(null);
    const triggerRef = useRef(null);
    const popRef = useRef(null);

    // Track the time-of-day that the user has dialed in via the time inputs.
    // Seeded from the current `start`/`end` props (or the user's now-time-of-day
    // if those are unset) so a fresh first day-click doesn't snap to midnight.
    const now = new Date();
    const [fromTime, setFromTime] = useState(() => formatTimeHm(start instanceof Date ? start : now));
    const [toTime, setToTime] = useState(() => formatTimeHm(end instanceof Date ? end : now));

    // When the popover (re)opens, re-seed time inputs from the current props
    // (or now-time-of-day) so the user can edit times before picking dates.
    useEffect(() => {
        if (!open) return;
        const seedNow = new Date();
        setFromTime(formatTimeHm(start instanceof Date ? start : seedNow));
        setToTime(formatTimeHm(end instanceof Date ? end : seedNow));
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open]);

    // Compute the popover position from the trigger's bounding rect when open,
    // and on scroll/resize. Mirrors the Combobox portal pattern in r-ui.tsx.
    useLayoutEffect(() => {
        if (!open) {
            setPopRect(null);
            return undefined;
        }
        const update = () => {
            if (!triggerRef.current) return;
            const r = triggerRef.current.getBoundingClientRect();
            setPopRect({ top: r.bottom + 6, left: r.left, triggerWidth: r.width });
        };
        update();
        window.addEventListener('resize', update);
        window.addEventListener('scroll', update, true);
        return () => {
            window.removeEventListener('resize', update);
            window.removeEventListener('scroll', update, true);
        };
    }, [open]);

    useEffect(() => {
        if (!open) return undefined;
        const onDocMouseDown = (e) => {
            const tgt = e.target;
            if (triggerRef.current && triggerRef.current.contains(tgt)) return;
            if (popRef.current && popRef.current.contains(tgt)) return;
            setOpen(false);
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
        const fromHm = parseHm(fromTime, { hours: day.getHours(), minutes: day.getMinutes() });
        const toHm = parseHm(toTime, { hours: day.getHours(), minutes: day.getMinutes() });
        if (!start || (start && end)) {
            // Fresh first click — either we have no selection, or we already
            // have a complete one and the user wants to start over. Use the
            // From time-of-day the user has dialed in.
            onChange(applyTimeFromHm(day, fromHm.hours, fromHm.minutes), null);
            return;
        }
        // We have `start` only; this click finishes the range. Order the
        // endpoints by date then apply the From/To times to each.
        let sDay = start;
        let eDay = day;
        if (day.getTime() < start.getTime()) {
            sDay = day;
            eDay = start;
        }
        onChange(
            applyTimeFromHm(sDay, fromHm.hours, fromHm.minutes),
            applyTimeFromHm(eDay, toHm.hours, toHm.minutes),
        );
    };

    const handleFromTimeChange = (value) => {
        setFromTime(value);
        if (start) onChange(applyTimeString(start, value), end);
    };

    const handleToTimeChange = (value) => {
        setToTime(value);
        if (end) onChange(start, applyTimeString(end, value));
    };

    const popover = open && popRect ? (
        <div
            ref={popRef}
            className="r-date-range-popover"
            role="dialog"
            style={{
                position: 'fixed',
                top: popRect.top,
                left: popRect.left,
                transform: 'none',
            }}
        >
            <DayPicker
                mode="range"
                numberOfMonths={2}
                selected={{ from: start || undefined, to: end || undefined }}
                onDayClick={handleDayClick}
                defaultMonth={start || new Date()}
                disabled={disabled}
            />
            <div className="r-date-range-times">
                <label>
                    <span>From</span>
                    <input
                        type="time"
                        value={fromTime}
                        onChange={(e) => handleFromTimeChange(e.target.value)}
                        className="r-field"
                    />
                </label>
                <label>
                    <span>To</span>
                    <input
                        type="time"
                        value={toTime}
                        onChange={(e) => handleToTimeChange(e.target.value)}
                        className="r-field"
                    />
                </label>
                <RButton size="sm" variant="ghost" onClick={() => onChange(null, null)}>Clear</RButton>
                <RButton size="sm" onClick={() => setOpen(false)}>Done</RButton>
            </div>
        </div>
    ) : null;

    return (
        <div className="r-date-range">
            <button
                ref={triggerRef}
                type="button"
                className="r-field r-date-range-trigger"
                onClick={() => setOpen((v) => !v)}
                aria-haspopup="dialog"
                aria-expanded={open}
                aria-label={start && end ? `Selected range ${label}. Click to change.` : 'Select date range'}
            >
                {label}
            </button>
            {popover && createPortal(popover, document.body)}
        </div>
    );
}
