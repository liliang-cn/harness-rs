// MDXEditor's markdown transformers pull in `@lexical/code`, which reads a
// bare global `Prism`. Import prismjs eagerly here (before the lazy editor
// chunk loads) so `window.Prism` is set — otherwise the editor throws
// "Prism is not defined" when opened.
import 'prismjs';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import './index.css';
import './lib/i18n';
import App from './App.tsx';

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter>
      <App />
    </BrowserRouter>
  </StrictMode>,
);
