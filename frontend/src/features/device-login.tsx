import React, { useCallback, useEffect, useState } from 'react';
import { api } from '../lib/api';
import { CONFIG } from '../lib/config';
import { useQueryParam } from '../lib/navigation';
import { Alert, Button, Field, Input, KV, KVRow, Panel, PanelBody, PanelHead } from '../components/r-ui';

/** A pending `rise login --device`, as `GET /auth/device` describes it. */
interface DeviceAuthorization {
    user_code: string;
    client_name?: string;
    client_ip?: string;
    created_at: string;
    expires_at: string;
    /** The current session may not approve: it is too old, or predates identity-bound sessions. */
    reauth_required: boolean;
}

type Phase =
    | { kind: 'enter' }
    | { kind: 'loading' }
    | { kind: 'confirm'; request: DeviceAuthorization }
    | { kind: 'approved' }
    | { kind: 'denied' }
    | { kind: 'error'; message: string; request?: DeviceAuthorization };

/** Marks the return from the sign-in this page started, so it never loops. */
const REAUTH_MARKER = 'reauth';

/** Pull the backend's `{"error": "..."}` message out of an API error. */
function errorMessage(err: unknown): string {
    const message = err instanceof Error ? err.message : String(err);
    const json = message.indexOf('{');
    if (json >= 0) {
        try {
            const body = JSON.parse(message.slice(json));
            if (typeof body.error === 'string') return body.error;
        } catch {
            // Not JSON: fall through to the raw message.
        }
    }
    return message;
}

function isAuthError(err: unknown): boolean {
    return err instanceof Error && err.message === 'Authentication required';
}

/**
 * Approving a device login needs a fresh sign-in. Send the user through the
 * browser login and back here, once: if the session is still not fresh on
 * return, show an error rather than redirecting again.
 */
function signInAgain(userCode: string): boolean {
    const url = new URL(window.location.href);
    if (url.searchParams.has(REAUTH_MARKER)) return false;
    url.searchParams.set('user_code', userCode);
    url.searchParams.set(REAUTH_MARKER, '1');
    window.location.href = `${CONFIG.backendUrl}/api/v1/auth/signin/start?rd=${encodeURIComponent(url.toString())}`;
    return true;
}

const REAUTH_FAILED =
    'Rise could not confirm a recent sign-in. Sign out, sign in again, and reopen the link from your terminal.';

export function DeviceLogin() {
    const [userCode, setUserCode] = useQueryParam('user_code');
    const [input, setInput] = useState(userCode ?? '');
    const [phase, setPhase] = useState<Phase>(userCode ? { kind: 'loading' } : { kind: 'enter' });
    const [busy, setBusy] = useState(false);

    const lookup = useCallback(async (code: string) => {
        setPhase({ kind: 'loading' });
        try {
            const request = (await api.getDeviceAuthorization(code)) as DeviceAuthorization;
            if (request.reauth_required) {
                if (!signInAgain(request.user_code)) {
                    setPhase({ kind: 'error', message: REAUTH_FAILED, request });
                }
                return;
            }
            setPhase({ kind: 'confirm', request });
        } catch (err) {
            setPhase({ kind: 'error', message: errorMessage(err) });
        }
    }, []);

    useEffect(() => {
        if (userCode) lookup(userCode);
    }, [userCode, lookup]);

    const decide = async (request: DeviceAuthorization, approve: boolean) => {
        setBusy(true);
        try {
            if (approve) {
                await api.approveDevice(request.user_code);
                setPhase({ kind: 'approved' });
            } else {
                await api.denyDevice(request.user_code);
                setPhase({ kind: 'denied' });
            }
        } catch (err) {
            if (approve && isAuthError(err) && signInAgain(request.user_code)) return;
            setPhase({ kind: 'error', message: isAuthError(err) ? REAUTH_FAILED : errorMessage(err), request });
        } finally {
            setBusy(false);
        }
    };

    const submitCode = (e: React.FormEvent) => {
        e.preventDefault();
        const code = input.trim();
        if (!code) return;
        if (code === userCode) lookup(code);
        else setUserCode(code);
    };

    const startOver = () => {
        setUserCode(null);
        setInput('');
        setPhase({ kind: 'enter' });
    };

    return (
        <section style={{ maxWidth: 560, margin: '0 auto' }}>
            <div className="r-page-head">
                <div className="title-stack">
                    <h1 className="r-page-title">Device login</h1>
                    <div className="r-page-sub">Confirm a <code>rise login --device</code> from your terminal.</div>
                </div>
            </div>

            <Panel>
                {phase.kind === 'enter' && (
                    <>
                        <PanelHead title="Enter the code shown in your terminal" />
                        <PanelBody>
                            <form onSubmit={submitCode} style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
                                <Field label="Code">
                                    <Input
                                        autoFocus
                                        placeholder="BCDF-GHJK"
                                        value={input}
                                        onChange={(e) => setInput(e.target.value)}
                                        className="mono"
                                        style={{ letterSpacing: '0.12em', textTransform: 'uppercase' }}
                                    />
                                </Field>
                                <div>
                                    <Button type="submit" variant="primary" disabled={!input.trim()}>Continue</Button>
                                </div>
                            </form>
                        </PanelBody>
                    </>
                )}

                {phase.kind === 'loading' && (
                    <PanelBody>
                        <div style={{ display: 'flex', justifyContent: 'center', padding: 24 }}>
                            <span className="r-spinner lg" />
                        </div>
                    </PanelBody>
                )}

                {phase.kind === 'confirm' && (
                    <>
                        <PanelHead title="Approve this login?" />
                        <PanelBody>
                            <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
                                <div
                                    className="mono"
                                    style={{
                                        fontSize: 28,
                                        fontWeight: 600,
                                        letterSpacing: '0.12em',
                                        textAlign: 'center',
                                        padding: '8px 0',
                                    }}
                                >
                                    {phase.request.user_code}
                                </div>
                                <KV>
                                    {phase.request.client_name && <KVRow k="Device">{phase.request.client_name}</KVRow>}
                                    {phase.request.client_ip && <KVRow k="IP address">{phase.request.client_ip}</KVRow>}
                                    <KVRow k="Requested">{new Date(phase.request.created_at).toLocaleString()}</KVRow>
                                    <KVRow k="Expires">{new Date(phase.request.expires_at).toLocaleString()}</KVRow>
                                </KV>
                                <Alert tone="warn" icon="info">
                                    Only approve if you just ran <code>rise login --device</code> yourself and this code
                                    matches the one in your terminal. Approving signs that terminal in as you.
                                </Alert>
                                <div style={{ display: 'flex', gap: 8 }}>
                                    <Button variant="primary" icon="check" loading={busy} onClick={() => decide(phase.request, true)}>
                                        Approve
                                    </Button>
                                    <Button icon="close" disabled={busy} onClick={() => decide(phase.request, false)}>
                                        Deny
                                    </Button>
                                </div>
                            </div>
                        </PanelBody>
                    </>
                )}

                {phase.kind === 'approved' && (
                    <PanelBody>
                        <Alert tone="info" icon="check">
                            Device approved. You can close this tab and return to your terminal.
                        </Alert>
                    </PanelBody>
                )}

                {phase.kind === 'denied' && (
                    <PanelBody>
                        <Alert tone="info" icon="close">
                            Login denied. The terminal that requested it will not be signed in.
                        </Alert>
                    </PanelBody>
                )}

                {phase.kind === 'error' && (
                    <PanelBody>
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
                            <Alert tone="err" icon="info">{phase.message}</Alert>
                            <div>
                                <Button onClick={startOver}>Enter a different code</Button>
                            </div>
                        </div>
                    </PanelBody>
                )}
            </Panel>
        </section>
    );
}
