/**
 * Collect packages that reach the bundle only through CSS.
 *
 * `rollup-plugin-license` walks `chunk.modules` and keeps entries with
 * `renderedLength > 0`. Vite extracts CSS during the build and leaves those
 * modules with empty rendered code, so every CSS-only dependency is dropped —
 * verified against this repo: the build emits 40 `.woff2` files from
 * `@fontsource/inter` and `@fontsource/jetbrains-mono`, and the plugin reports
 * neither package.
 *
 * Those fonts ship, and they are OFL-1.1, which requires its notice to travel
 * with them. So this plugin records what the other one cannot see.
 *
 * Sound here specifically because `vite.config.ts` sets `cssCodeSplit: false`:
 * all CSS lands in the single `assets/app.css` that always ships, so "imported
 * during the build" and "reaches a user" are the same set.
 */

import { readFileSync, existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, sep } from 'node:path';

/** Strip Vite's query suffixes (`?used`, `?direct`, `?inline`, `?raw`). */
function stripQuery(id) {
  const q = id.indexOf('?');
  return q === -1 ? id : id.slice(0, q);
}

/** Walk up from a file to the package.json that owns it, without escaping node_modules. */
function owningPackage(file) {
  let dir = dirname(file);
  while (dir.includes(`${sep}node_modules${sep}`) || dir.endsWith(`${sep}node_modules`)) {
    const manifest = join(dir, 'package.json');
    if (existsSync(manifest)) {
      try {
        const pkg = JSON.parse(readFileSync(manifest, 'utf8'));
        if (pkg.name && pkg.version) {
          return { name: pkg.name, version: pkg.version, dir };
        }
      } catch {
        // A package.json that does not parse is not one we can attribute.
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

/** Find the license text a package ships, if it ships one. */
function licenseText(dir) {
  for (const name of ['LICENSE', 'LICENSE.md', 'LICENSE.txt', 'LICENCE', 'LICENCE.md', 'COPYING']) {
    const p = join(dir, name);
    if (existsSync(p)) {
      return readFileSync(p, 'utf8').replace(/\r\n/g, '\n').trimEnd();
    }
  }
  return null;
}

/**
 * @param {{ outFile: string }} options Where to write the manifest. The plugin
 *   is inert unless this is set, so ordinary builds are untouched.
 */
export default function cssDepsPlugin({ outFile }) {
  const found = new Map();

  return {
    name: 'rise-notices-css-deps',
    apply: 'build',

    transform(_code, id) {
      const file = stripQuery(id);
      if (!file.endsWith('.css')) return null;
      if (!file.includes(`${sep}node_modules${sep}`)) return null;

      const pkg = owningPackage(file);
      if (!pkg) return null;

      const key = `${pkg.name}@${pkg.version}`;
      if (!found.has(key)) {
        let manifest = {};
        try {
          manifest = JSON.parse(readFileSync(join(pkg.dir, 'package.json'), 'utf8'));
        } catch {
          // Already handled above; keep the entry with what we know.
        }
        found.set(key, {
          name: pkg.name,
          version: pkg.version,
          license:
            typeof manifest.license === 'string'
              ? manifest.license
              : manifest.license?.type ?? null,
          repository:
            typeof manifest.repository === 'string'
              ? manifest.repository
              : manifest.repository?.url ?? null,
          licenseText: licenseText(pkg.dir),
          via: 'css',
        });
      }
      return null;
    },

    // `closeBundle`, not `buildEnd`: Vite empties `outDir` between them, and
    // the manifest is written outside `dist/` anyway so it never ships.
    closeBundle() {
      if (!outFile) return;
      const entries = [...found.values()].sort((a, b) =>
        a.name < b.name ? -1 : a.name > b.name ? 1 : 0,
      );
      mkdirSync(dirname(outFile), { recursive: true });
      writeFileSync(outFile, `${JSON.stringify(entries, null, 2)}\n`);
    },
  };
}
