import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// Random 3000+ port per repo convention. Override with VITE_PORT.
const PORT = Number(process.env.VITE_PORT ?? 5778);
// Where the Rust backend serves its API. Override with VITE_API_TARGET.
const API_TARGET = process.env.VITE_API_TARGET ?? 'http://localhost:6743';

export default defineConfig({
  // Production build serves under /admin/ from the Rust binary; dev server
  // (port 5778) serves at root. The leading-slash convention here makes the
  // emitted asset URLs `/admin/assets/...`, matching how the backend mounts
  // them via include_dir!.
  base: process.env.NODE_ENV === 'production' ? '/admin/' : '/',
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    port: PORT,
    strictPort: true,
    proxy: {
      '/api': {
        target: API_TARGET,
        changeOrigin: true,
      },
    },
  },
});
