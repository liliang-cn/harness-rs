import { transform } from 'sucrase';

// esm.sh module map for the iframe. React pinned to the host's major; recharts
// shares that React so hooks work across the boundary.
const IMPORT_MAP = {
  imports: {
    react: 'https://esm.sh/react@19.2.6',
    'react/jsx-runtime': 'https://esm.sh/react@19.2.6/jsx-runtime',
    'react-dom/client': 'https://esm.sh/react-dom@19.2.6/client',
    recharts: 'https://esm.sh/recharts@3.8.0?deps=react@19.2.6',
  },
};

/** Compile the AI's JSX/TSX to JS (automatic runtime → no React import needed).
 *  Throws on syntax errors; callers show the message instead of mounting. */
export function transpile(code: string): string {
  return transform(code, {
    transforms: ['jsx', 'typescript'],
    jsxRuntime: 'automatic',
    production: true,
  }).code;
}

/** Build a self-contained sandbox document. The iframe is rendered with
 *  sandbox="allow-scripts" and NO allow-same-origin, so this runs in an opaque
 *  origin: it cannot read the parent DOM, localStorage, cookies, or call
 *  same-origin APIs. Data arrives only via postMessage({type:'artifact-data', data}). */
export function buildSrcdoc(code: string): string {
  const compiled = transpile(code);
  return `<!doctype html>
<html>
<head>
<meta charset="utf-8" />
<script type="importmap">${JSON.stringify(IMPORT_MAP)}<\/script>
<style>
  :root { color-scheme: light dark; }
  body { margin: 0; padding: 16px; font: 14px/1.5 system-ui, -apple-system, sans-serif; }
  #root { min-height: 100vh; }
</style>
</head>
<body>
<div id="root"></div>
<script type="module">
${compiled}
window.App = (typeof App !== 'undefined') ? App : (window.App || null);
<\/script>
<script type="module">
  import React from 'react';
  import { createRoot } from 'react-dom/client';
  const post = (m) => { try { parent.postMessage(m, '*'); } catch {} };
  window.onerror = (msg) => post({ type: 'artifact-error', message: String(msg) });
  const root = createRoot(document.getElementById('root'));
  function mount() {
    try {
      const C = window.App;
      root.render(C ? React.createElement(C) : null);
    } catch (e) {
      post({ type: 'artifact-error', message: String((e && e.stack) || e) });
    }
  }
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'artifact-data') {
      window.DATA = e.data.data;
      mount();
    }
  });
  post({ type: 'artifact-ready' });
<\/script>
</body>
</html>`;
}
