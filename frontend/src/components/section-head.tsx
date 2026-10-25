import React from 'react';
import { PROJECT_SECTIONS, type ProjectSection } from '../lib/routes';

/** Title, summary and actions of a project section page. */
export function SectionHead({ section, actions }: { section: ProjectSection; actions?: React.ReactNode }) {
    const info = PROJECT_SECTIONS.find(s => s.id === section)!;
    return (
        <div className="r-page-head">
            <div className="title-stack">
                <h1 className="r-page-title">{info.label}</h1>
                {info.sub && <div className="r-page-sub">{info.sub}</div>}
            </div>
            {actions && <div className="r-page-actions">{actions}</div>}
        </div>
    );
}
