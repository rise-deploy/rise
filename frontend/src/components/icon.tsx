import React from 'react';

// Lightweight stroke icon set used across the redesigned UI.
const PATHS: Record<string, string> = {
    home:       'M3 12 12 4l9 8M5 10v10h4v-6h6v6h4V10',
    cube:       'M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16ZM12 22V12M3.27 6.96 12 12l8.73-5.04',
    activity:   'M22 12h-4l-3 9L9 3l-3 9H2',
    users:      'M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8Zm14 10v-2a4 4 0 0 0-3-3.87m-4-12a4 4 0 0 1 0 7.75',
    db:         'M21 5C21 7.21 16.97 9 12 9S3 7.21 3 5s4.03-4 9-4 9 1.79 9 4Zm0 7c0 2.21-4.03 4-9 4s-9-1.79-9-4M3 5v14c0 2.21 4.03 4 9 4s9-1.79 9-4V5',
    globe:      'M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20Zm0 0c-2.76 0-5-4.48-5-10s2.24-10 5-10 5 4.48 5 10-2.24 10-5 10Zm-10-10h20',
    lock:       'M19 11H5a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7a2 2 0 0 0-2-2ZM7 11V7a5 5 0 0 1 10 0v4',
    key:        'M21 2 19 4l2 2-2 2-2-2-3 3a5 5 0 1 1-3-3l8-8ZM7 17a3 3 0 1 1 0-6 3 3 0 0 1 0 6Z',
    gear:       'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm7.4-3a7.4 7.4 0 0 0-.12-1.34l1.93-1.5-2-3.46-2.27.74a7.4 7.4 0 0 0-2.3-1.34L14.4 3h-4l-.34 2.1a7.4 7.4 0 0 0-2.3 1.34l-2.27-.74-2 3.46 1.93 1.5A7.4 7.4 0 0 0 5.3 12c0 .46.04.91.12 1.34l-1.93 1.5 2 3.46 2.27-.74a7.4 7.4 0 0 0 2.3 1.34l.34 2.1h4l.34-2.1a7.4 7.4 0 0 0 2.3-1.34l2.27.74 2-3.46-1.93-1.5c.08-.43.12-.88.12-1.34Z',
    plus:       'M12 5v14M5 12h14',
    chev:       'M9 18l6-6-6-6',
    chevd:      'M6 9l6 6 6-6',
    chevu:      'M18 15l-6-6-6 6',
    chevl:      'M15 18l-6-6 6-6',
    search:     'M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm10 2-4.35-4.35',
    sun:        'M12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8ZM12 2v2m0 16v2M4.93 4.93l1.41 1.41m11.32 11.32 1.41 1.41M2 12h2m16 0h2M4.93 19.07l1.41-1.41m11.32-11.32 1.41-1.41',
    moon:       'M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79Z',
    bell:       'M18 16v-5a6 6 0 0 0-12 0v5L4 18v1h16v-1l-2-2Zm-4 4a2 2 0 0 1-4 0',
    git:        'M9 19c-5 1.5-5-2.5-7-3m14 6v-3.87a3.37 3.37 0 0 0-.94-2.61c3.14-.35 6.44-1.54 6.44-7A5.44 5.44 0 0 0 20 4.77 5.07 5.07 0 0 0 19.91 1S18.73.65 16 2.48a13.38 13.38 0 0 0-7 0C6.27.65 5.09 1 5.09 1A5.07 5.07 0 0 0 5 4.77a5.44 5.44 0 0 0-1.5 3.78c0 5.42 3.3 6.61 6.44 7A3.37 3.37 0 0 0 9 18.13V22',
    branch:     'M6 3v12M18 9v6M6 15a3 3 0 1 0 0 6 3 3 0 0 0 0-6Zm0-12a3 3 0 1 0 0 6 3 3 0 0 0 0-6Zm12 6a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM6 9a9 9 0 0 0 9 9',
    play:       'M6 4v16l14-8L6 4Z',
    stop:       'M5 5h14v14H5z',
    refresh:    'M23 4v6h-6M1 20v-6h6M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15',
    copy:       'M20 9h-9a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2v-9a2 2 0 0 0-2-2ZM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1',
    ext:        'M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6m4-4h6v6m-9 3 9-9',
    more:       'M12 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1ZM19 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1ZM5 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1Z',
    arrow:      'M5 12h14M13 5l7 7-7 7',
    eye:        'M1 12s4-8 11-8 11 8 11 8-4 8-11 8S1 12 1 12Zm11 3a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z',
    eyeoff:     'M17.94 17.94A10 10 0 0 1 12 20c-7 0-11-8-11-8a18 18 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18 18 0 0 1-2.16 3.19M14.12 14.12A3 3 0 1 1 9.88 9.88M1 1l22 22',
    trash:      'M3 6h18m-2 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6',
    edit:       'M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7m-1.41-9.59a2 2 0 1 1 2.83 2.83L11.83 15H9v-2.83l9.59-9.58Z',
    check:      'M5 12l5 5L20 7',
    close:      'M18 6 6 18M6 6l12 12',
    layer:      'M12 2 2 7l10 5 10-5-10-5ZM2 17l10 5 10-5M2 12l10 5 10-5',
    rocket:     'M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09ZM12 15l-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2ZM9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5',
    flame:      'M8.5 14.5A2.5 2.5 0 0 0 11 12c0-1.38-.5-2-1-3-1.072-2.143-.224-4.054 2-6 .5 2.5 2 4.9 4 6.5 2 1.6 3 3.5 3 5.5a7 7 0 1 1-14 0c0-1.153.433-2.294 1-3a2.5 2.5 0 0 0 2.5 2.5Z',
    user:       'M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2M12 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8Z',
    logout:     'M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9',
    info:       'M12 16v-4M12 8h.01M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0Z',
};

const STROKE_ONLY = new Set([
    'close', 'check', 'plus', 'chev', 'chevd', 'chevu', 'chevl', 'search', 'arrow', 'refresh',
    'sun', 'trash', 'edit', 'branch', 'git', 'activity', 'ext', 'copy', 'key', 'flame',
    'rocket', 'eye', 'eyeoff', 'bell', 'logout', 'user', 'info', 'home', 'cube', 'users',
    'db', 'globe', 'lock', 'gear', 'layer', 'moon',
]);

export interface IconProps {
    name: keyof typeof PATHS | string;
    size?: number;
    className?: string;
    title?: string;
}

export function Icon({ name, size = 16, className, title }: IconProps) {
    const d = PATHS[name];
    if (!d) return null;
    const stroke = STROKE_ONLY.has(name);
    return (
        <svg
            className={className}
            width={size}
            height={size}
            viewBox="0 0 24 24"
            fill={stroke ? 'none' : 'currentColor'}
            stroke={stroke ? 'currentColor' : 'none'}
            strokeWidth={1.7}
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden={title ? undefined : true}
            role={title ? 'img' : undefined}
        >
            {title && <title>{title}</title>}
            <path d={d} />
        </svg>
    );
}
