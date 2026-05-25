import React from 'react';
import { Palette, Density, PALETTES, DENSITIES, usePrefs } from '../lib/prefs';
import { Avatar, Button, Field, KV, KVRow, Panel, PanelBody, PanelHead, Segmented, cx, colorFor } from '../components/r-ui';
import { Icon } from '../components/icon';

interface ProfileProps {
    user: { email?: string; id?: string } | null;
}

const PALETTE_SWATCHES: Record<Palette, { primary: string; secondary: string; tertiary: string }> = {
    mint:   { primary: 'oklch(0.55 0.10 165)', secondary: 'oklch(0.65 0.13 145)', tertiary: 'oklch(0.985 0.004 80)' },
    indigo: { primary: 'oklch(0.55 0.17 270)', secondary: 'oklch(0.62 0.18 245)', tertiary: 'oklch(0.985 0.005 260)' },
    ember:  { primary: 'oklch(0.62 0.16 40)',  secondary: 'oklch(0.62 0.17 22)',  tertiary: 'oklch(0.985 0.008 70)' },
    slate:  { primary: 'oklch(0.32 0.012 250)', secondary: 'oklch(0.50 0.012 250)', tertiary: 'oklch(0.97 0.003 250)' },
};

const DENSITY_DESCRIPTIONS: Record<Density, string> = {
    compact: 'Tighter padding and smaller type. More on screen at a glance.',
    cozy: 'Default rhythm. Generous spacing without feeling sparse.',
};

export function Profile({ user }: ProfileProps) {
    const [prefs, setPrefs] = usePrefs();
    const email = user?.email || 'unknown';
    const userName = email.split('@')[0];

    return (
        <section>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">Profile</h1>
                    <div className="r-page-sub">
                        Your account information and appearance preferences for Rise. Preferences are stored locally in this browser.
                    </div>
                </div>
            </div>

            <div className="r-grid-2-1">
                <div className="r-stack">
                    <Panel>
                        <PanelHead title="Appearance" sub="Choose the accent color and density used across Rise." />
                        <PanelBody>
                            <div className="r-stack">
                                <div>
                                    <label className="r-field-label">Color theme</label>
                                    <PalettePicker value={prefs.palette} onChange={p => setPrefs({ palette: p })} />
                                    <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 10, marginBottom: 0 }}>
                                        Default: <span className="mono">indigo</span>. The palette changes the accent color used for primary actions, active nav items, and links.
                                    </p>
                                </div>

                                <div>
                                    <label className="r-field-label">Density</label>
                                    <div style={{ marginTop: 4 }}>
                                        <Segmented<Density>
                                            value={prefs.density}
                                            options={DENSITIES.map(d => ({ value: d, label: d[0].toUpperCase() + d.slice(1) }))}
                                            onChange={(d) => setPrefs({ density: d })}
                                        />
                                    </div>
                                    <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 10, marginBottom: 0 }}>
                                        {DENSITY_DESCRIPTIONS[prefs.density]}
                                    </p>
                                </div>

                                <div>
                                    <label className="r-field-label">Color mode</label>
                                    <div style={{ marginTop: 4 }}>
                                        <Segmented
                                            value={prefs.theme}
                                            options={[
                                                { value: 'system', label: 'System' },
                                                { value: 'light', label: 'Light' },
                                                { value: 'dark', label: 'Dark' },
                                            ]}
                                            onChange={(t) => setPrefs({ theme: t as 'system' | 'light' | 'dark' })}
                                        />
                                    </div>
                                    {prefs.theme === 'system' && (
                                        <p style={{ fontSize: 12, color: 'var(--text-soft)', marginTop: 10, marginBottom: 0 }}>
                                            Follows your operating system preference.
                                        </p>
                                    )}
                                </div>
                            </div>
                        </PanelBody>
                    </Panel>
                </div>

                <div className="r-stack">
                    <Panel>
                        <PanelHead title="Account" />
                        <PanelBody>
                            <div style={{ display: 'flex', alignItems: 'center', gap: 14, marginBottom: 18 }}>
                                <Avatar label={userName} color={colorFor(email)} size={44} />
                                <div style={{ minWidth: 0 }}>
                                    <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text)' }}>{userName || 'You'}</div>
                                    <div style={{ fontSize: 12.5, color: 'var(--text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{email}</div>
                                </div>
                            </div>
                            <KV>
                                <KVRow k="Email"><span className="mono" style={{ fontSize: 12.5 }}>{email}</span></KVRow>
                                <KVRow k="Sign in"><span>SSO (OIDC)</span></KVRow>
                            </KV>
                        </PanelBody>
                    </Panel>

                    <Panel>
                        <PanelHead title="Reset preferences" />
                        <PanelBody>
                            <p style={{ fontSize: 13, color: 'var(--text-muted)', marginTop: 0 }}>
                                Restores the defaults: <span className="mono">indigo</span> palette, <span className="mono">cozy</span> density, and system color mode.
                            </p>
                            <Button
                                onClick={() => setPrefs({ palette: 'indigo', density: 'cozy', theme: 'system' })}
                                icon="refresh"
                            >
                                Reset to defaults
                            </Button>
                        </PanelBody>
                    </Panel>
                </div>
            </div>
        </section>
    );
}

function PalettePicker({ value, onChange }: { value: Palette; onChange: (p: Palette) => void }) {
    return (
        <div className="r-palette-grid" role="radiogroup" aria-label="Color theme">
            {PALETTES.map(p => {
                const colors = PALETTE_SWATCHES[p];
                const checked = value === p;
                return (
                    <button
                        key={p}
                        type="button"
                        role="radio"
                        aria-checked={checked}
                        aria-label={p}
                        className="r-palette-btn"
                        onClick={() => onChange(p)}
                    >
                        <div className="r-palette-btn-swatch">
                            <i style={{ background: colors.primary }} />
                            <div>
                                <i style={{ background: colors.secondary }} />
                                <i style={{ background: colors.tertiary }} />
                            </div>
                        </div>
                        {checked && (
                            <svg className="r-palette-btn-check" viewBox="0 0 12 12">
                                <path d="M2.5 6.5l2.4 2.4L9.5 3.6" fill="none" stroke="white" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
                            </svg>
                        )}
                        <span className="r-palette-btn-label">{p}</span>
                    </button>
                );
            })}
        </div>
    );
}
