import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// Random 3000+ port per repo convention.
const PORT = Number(process.env.VITE_PORT ?? 5779);
const API_TARGET = process.env.VITE_API_TARGET ?? 'http://localhost:6743';

export default defineConfig({
  // User-facing UI lives at site root in production. Old hand-written
  // index.html will be relocated to /legacy/ by the backend during
  // migration.
  base: '/',
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
