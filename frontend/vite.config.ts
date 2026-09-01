import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import license from 'rollup-plugin-license';
import cssDeps from '../scripts/notices/vite-plugin-css-deps.mjs';

// Third-party attribution for whatever this build actually bundles. Inert
// unless RISE_NOTICES_OUT is set, so `npm run build`, `mise frontend:build` and
// the Docker image build are all unaffected -- only `mise notices:generate`
// turns it on. See scripts/generate-notices.mjs.
const noticesOut = process.env.RISE_NOTICES_OUT;
const noticesPlugins = noticesOut
  ? [
      // Packages reached through JS.
      license({
        thirdParty: {
          includePrivate: false,
          output: {
            file: `${noticesOut}/frontend-js.json`,
            template: (dependencies: any[]) =>
              JSON.stringify(
                dependencies
                  .map((d) => ({
                    name: d.name,
                    version: d.version,
                    license: d.license ?? null,
                    repository:
                      typeof d.repository === 'string'
                        ? d.repository
                        : (d.repository?.url ?? null),
                    licenseText: d.licenseText ?? null,
                    via: 'js',
                  }))
                  // Producer order follows module traversal, which is not
                  // guaranteed stable; the notices file is committed, so sort.
                  .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)),
                null,
                2,
              ),
          },
        },
      }),
      // Packages reached only through CSS, which the plugin above cannot see.
      // This is what catches the OFL-1.1 fonts whose .woff2 files ship.
      cssDeps({ outFile: `${noticesOut}/frontend-css.json` }),
    ]
  : [];

export default defineConfig({
  plugins: [react(), tailwindcss(), ...(noticesPlugins as any[])],
  server: {
    port: 5173,
    hmr: {
      host: 'localhost',
      port: 5173,
      clientPort: 5173,
      protocol: 'ws'
    },
    proxy: {
      '/api': 'http://localhost:3000',
      '/.well-known': 'http://localhost:3000',
      '/.rise': 'http://localhost:3000',
      '/assets': 'http://localhost:3000',
      '/auth': 'http://localhost:3000'
    }
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: (assetInfo) => {
          if (assetInfo.name?.endsWith('.css')) {
            return 'assets/app.css';
          }
          return 'assets/[name][extname]';
        }
      }
    }
  }
});
