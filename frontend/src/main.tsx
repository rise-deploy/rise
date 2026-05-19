import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import { ToastProvider } from './components/toast';
import { applyPrefs, loadPrefs } from './lib/prefs';
import './styles/index.css';
import './styles/rise.css';
import './styles/legacy.css';

// Apply theme/density/palette before first paint so we don't flash defaults.
applyPrefs(loadPrefs());

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ToastProvider>
      <App />
    </ToastProvider>
  </React.StrictMode>
);
