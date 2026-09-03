import { defineConfig } from 'vite';
import { resolve } from 'node:path';

// Multi-page build. Each entry becomes a directory in dist/, which the relay
// serves from an embedded copy — so there is still one binary and no CORS.
export default defineConfig({
  appType: 'mpa',
  // KNOOT_NO_ENV points Vite at an empty env directory, so the dist committed
  // to the repository carries no project keys. See config/no-env/README.md.
  envDir: process.env.KNOOT_NO_ENV ? resolve(__dirname, 'config/no-env') : undefined,
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        site: resolve(__dirname, 'index.html'),
        docs: resolve(__dirname, 'docs/index.html'),
        app: resolve(__dirname, 'app/index.html'),
        status: resolve(__dirname, 'status/index.html'),
        lab: resolve(__dirname, 'lab/index.html'),
        ops: resolve(__dirname, 'ops/index.html'),
      },
    },
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:7499',
      '/ws': { target: 'ws://127.0.0.1:7499', ws: true },
      '/term': { target: 'ws://127.0.0.1:7499', ws: true },
    },
  },
});
