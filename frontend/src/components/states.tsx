import React from 'react';
import { Button } from './r-ui';

export function LoadingState({ label = 'Loading…' }: { label?: string }) {
    return (
        <div style={{ padding: '40px 0', textAlign: 'center', color: 'var(--text-muted)' }}>
            <span className="r-spinner lg" />
            <p style={{ marginTop: 12, fontSize: 13 }}>{label}</p>
        </div>
    );
}

export function ErrorState({ message, onRetry }: { message: string; onRetry?: () => void }) {
    return (
        <div className="r-alert err" role="alert" style={{ alignItems: 'center', flexWrap: 'wrap' }}>
            <div style={{ flex: 1 }}>{message}</div>
            {onRetry && <Button size="sm" onClick={onRetry}>Retry</Button>}
        </div>
    );
}

export function EmptyState({
    message,
    actionLabel,
    onAction,
}: {
    message: string;
    actionLabel?: string;
    onAction?: () => void;
}) {
    return (
        <div className="r-empty">
            <div>{message}</div>
            {actionLabel && onAction && (
                <div style={{ marginTop: 12 }}>
                    <Button variant="primary" size="sm" onClick={onAction}>
                        {actionLabel}
                    </Button>
                </div>
            )}
        </div>
    );
}

export function PlatformAccessDenied({ userEmail, onLogout }: { userEmail: string; onLogout: () => void }) {
    return (
        <div className="r-login-wrap">
            <div className="r-login-card">
                <div className="r-login-logo" style={{ background: 'var(--err)' }}>!</div>
                <div>
                    <div style={{ fontSize: 20, fontWeight: 600, letterSpacing: '-0.02em' }}>Platform Access Denied</div>
                    <div style={{ color: 'var(--text-muted)', fontSize: 13.5, marginTop: 6 }}>
                        Your account is not authorized to access Rise platform features.
                    </div>
                </div>
                <p style={{ color: 'var(--text-muted)', fontSize: 13, margin: 0 }}>
                    You can authenticate to access deployed applications, but you cannot use the Rise Dashboard, CLI, or API.
                </p>
                <div className="mono" style={{
                    background: 'var(--surface-2)',
                    border: '1px solid var(--border-faint)',
                    borderRadius: 5,
                    padding: '10px 12px',
                    fontSize: 12,
                    color: 'var(--text)',
                    width: '100%',
                    textAlign: 'center',
                    wordBreak: 'break-all',
                }}>
                    {userEmail}
                </div>
                <p style={{ color: 'var(--text-soft)', fontSize: 12, margin: 0 }}>
                    If you believe this is an error, please contact your administrator.
                </p>
                <Button onClick={onLogout} style={{ width: '100%', justifyContent: 'center' }}>Logout</Button>
            </div>
        </div>
    );
}
