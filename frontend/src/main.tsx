import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import { ToastProvider } from './components/toast';
import { applyPrefs, loadPrefs } from './lib/prefs';
// Self-host Inter + JetBrains Mono so we don't depend on the
// fonts.googleapis.com CDN at runtime. Weights match what the old CDN
// stylesheet requested (Inter: 400/500/600/700; JetBrains Mono: 400/500/600).
import '@fontsource/inter/400.css';
import '@fontsource/inter/500.css';
import '@fontsource/inter/600.css';
import '@fontsource/inter/700.css';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';
import '@fontsource/jetbrains-mono/600.css';
import 'react-day-picker/style.css';
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
