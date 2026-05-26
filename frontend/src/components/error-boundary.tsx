import React from 'react';
import { ErrorState } from './states';

interface Props {
    children: React.ReactNode;
}

interface State {
    error: Error | null;
}

/**
 * Catches render errors in the routed view so a single failing screen shows a
 * recoverable message instead of blanking the whole app. Reset it by passing a
 * changing `key` (the current route) from the parent.
 */
export class ErrorBoundary extends React.Component<Props, State> {
    state: State = { error: null };

    static getDerivedStateFromError(error: Error): State {
        return { error };
    }

    componentDidCatch(error: Error, info: React.ErrorInfo) {
        console.error('Unhandled error in view:', error, info.componentStack);
    }

    render() {
        if (this.state.error) {
            return (
                <ErrorState
                    message={`Something went wrong rendering this page: ${this.state.error.message}`}
                    onRetry={() => window.location.reload()}
                />
            );
        }
        return this.props.children;
    }
}
