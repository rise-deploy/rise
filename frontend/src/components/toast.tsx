import React, { createContext, useCallback, useContext, useState } from 'react';
import { Icon } from './icon';

export type ToastType = 'success' | 'error' | 'info';

interface ToastItem { id: number; message: React.ReactNode; type: ToastType }

interface ToastContextValue {
    showToast: (message: React.ReactNode, type?: ToastType) => void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

// Errors stay up longer: they usually need reading, not just noticing.
const DISMISS_MS: Record<ToastType, number> = { success: 2800, info: 2800, error: 6000 };
const ICONS: Record<ToastType, string> = { success: 'check', info: 'info', error: 'alert' };

export function ToastProvider({ children }: { children: React.ReactNode }) {
    const [toasts, setToasts] = useState<ToastItem[]>([]);

    const removeToast = useCallback((id: number) => {
        setToasts(prev => prev.filter(t => t.id !== id));
    }, []);

    const showToast = useCallback((message: React.ReactNode, type: ToastType = 'info') => {
        const id = Date.now() + Math.random();
        setToasts(prev => [...prev, { id, message, type }]);
        setTimeout(() => removeToast(id), DISMISS_MS[type] ?? DISMISS_MS.info);
    }, [removeToast]);

    return (
        <ToastContext.Provider value={{ showToast }}>
            {children}
            <div className="r-toasts" role="status" aria-live="polite">
                {toasts.map(toast => (
                    <div key={toast.id} className={`r-toast ${toast.type}`}>
                        <Icon name={ICONS[toast.type]} size={15} className="r-toast-icon" />
                        <span className="r-toast-msg">{toast.message}</span>
                        <button type="button" className="r-toast-close" onClick={() => removeToast(toast.id)} aria-label="Dismiss">
                            <Icon name="close" size={13} />
                        </button>
                    </div>
                ))}
            </div>
        </ToastContext.Provider>
    );
}

// Hook to use toast from any component
export function useToast(): ToastContextValue {
    const context = useContext(ToastContext);
    if (!context) {
        throw new Error('useToast must be used within ToastProvider');
    }
    return context;
}
