#!/usr/bin/env node
/**
 * Generate THIRD-PARTY-NOTICES.md.
 *
 * MIT and Apache-2.0 — which nearly every dependency here uses — require their
 * notices to be reproduced when the software is distributed. Rise distributes a
 * container image and release binaries, so this file has to accompany them.
 *
 * It covers what actually *ships*, which is not the same as what the lockfiles
 * resolve: build-time toolchain never reaches a user and is deliberately absent.
 *
 *   Rust  : cargo-about over the workspace at --all-features (the image's
 *           feature set), configured by about.toml + about.hbs.
 *   npm   : emitted by rollup-plugin-license during the real Vite/Astro builds,
 *           so it reflects what was bundled rather than what was installed.
 *
 * Licenses are validated against deny.toml's allow list — the single source of
 * truth shared with cargo-deny — so a package sneaking in under an unreviewed
 * license fails here too, not just on the Rust side.
 *
 * Pure Node ESM, no dependencies (Node >= 20).
 */

import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(ROOT, 'THIRD-PARTY-NOTICES.md');
const NOTICES_DIR = join(ROOT, 'target', 'notices');
const FALLBACK_TEXTS = JSON.parse(
  readFileSync(join(ROOT, 'scripts', 'notices', 'spdx-fallback-texts.json'), 'utf8'),
);

/** Sort by code unit, not locale: `localeCompare` varies with the Node ICU build. */
const byName = (a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);

/**
 * Read the allowed-license list out of deny.toml.
 *
 * Deliberately a targeted scrape of the one `allow = [...]` array rather than a
 * TOML parser: it keeps this script dependency-free, and if the shape of
 * deny.toml changes enough to break the scrape we want to notice rather than
 * silently fall back to allowing everything.
 */
function allowedLicenses() {
  const toml = readFileSync(join(ROOT, 'deny.toml'), 'utf8');
  const block = toml.match(/^allow = \[$([\s\S]*?)^\]$/m);
  if (!block) {
    throw new Error('deny.toml: could not find the `allow = [...]` list');
  }
  const ids = [...block[1].matchAll(/^\s*"([^"]+)",/gm)].map((m) => m[1]);
  if (ids.length === 0) {
    throw new Error('deny.toml: `allow` list parsed as empty');
  }
  return new Set(ids);
}

function rustSection() {
  return execFileSync(
    'cargo',
    [
      'about',
      'generate',
      // The image is built --all-features, so this is the shipped superset.
      '--all-features',
      // Determinism, both of them. Without --offline, a crate that ships no
      // license file sends cargo-about to the network, so the output would
      // depend on what a remote service says today. Without --locked, a stale
      // Cargo.lock is silently updated and the notices describe a dependency
      // set nobody reviewed.
      '--offline',
      '--locked',
      'about.hbs',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
}

/**
 * Rise's own crates must never appear in a *third-party* notices file, and two
 * of them are BUSL-1.1 — publishing their text here would misrepresent the
 * license of code we own. about.toml's `private = { ignore = true }` should
 * already exclude them; this asserts it rather than trusting it, because the
 * failure is silent and the consequence is not.
 */
function assertNoFirstPartyCrates(body) {
  const offenders = [...body.matchAll(/^- \[(rise-[\w-]+) /gm)].map((m) => m[1]);
  if (offenders.length > 0) {
    throw new Error(
      `first-party crates leaked into the notices: ${[...new Set(offenders)].join(', ')}`,
    );
  }
  if (body.includes('BUSL')) {
    throw new Error('BUSL text appeared in a third-party notices file');
  }
}

const HEADER = `# Third-Party Notices

Rise is distributed with third-party software. This file reproduces the license
notices that software requires, and is generated — run \`mise run notices:generate\`
rather than editing it.

It covers what is **shipped**, not everything the lockfiles resolve: build-time
toolchain is excluded because it never reaches a user. Rise's own license is
separate and is not described here.

The two artifacts differ in what they contain. The \`rise\` container image has
all of it — the server binary and the bundled web UI. A release archive holds
only the CLI binary, so the JavaScript section below does not apply to it; it is
listed here rather than split across two files, because over-attributing costs
a reader a scroll and under-attributing is a license breach.

Dual-licensed dependencies are reproduced under one elected license. For Rust
that election is made by the preference order in \`about.toml\`, which puts MIT
first.
`;

/**
 * Split an SPDX expression into the licenses actually in force.
 *
 * `A AND B` means both apply, so both must be allowed and both texts
 * reproduced. `A OR B` is a choice, so one allowed license suffices and we
 * elect the first — matching how about.toml elects for Rust.
 */
function licensesInForce(expression) {
  const expr = String(expression).replace(/[()]/g, ' ').trim();
  if (/\bAND\b/.test(expr)) {
    return { ids: expr.split(/\bAND\b/).map((s) => s.trim()).filter(Boolean), conjunction: true };
  }
  return { ids: expr.split(/\bOR\b/).map((s) => s.trim()).filter(Boolean), conjunction: false };
}

function readManifest(name) {
  const path = join(NOTICES_DIR, name);
  if (!existsSync(path)) {
    throw new Error(
      `missing ${path}. Run the JS builds with RISE_NOTICES_OUT set — ` +
        '`mise run notices:generate` does this for you.',
    );
  }
  return JSON.parse(readFileSync(path, 'utf8'));
}

/**
 * Merge the per-build manifests into one attributed set.
 *
 * A package can arrive through both JS and CSS (react-day-picker does), so
 * entries are deduplicated on name@version, preferring whichever copy actually
 * carries a license text.
 */
function mergePackages(manifests) {
  const merged = new Map();
  for (const entry of manifests.flat()) {
    const key = `${entry.name}@${entry.version}`;
    const existing = merged.get(key);
    if (!existing || (!existing.licenseText && entry.licenseText)) {
      merged.set(key, { ...entry, key });
    }
  }
  return [...merged.values()].sort(byName);
}

/** Fail closed: an unreviewed license must never reach a shipped artifact silently. */
function enforceAllowList(packages, allowed) {
  const problems = [];
  for (const pkg of packages) {
    if (!pkg.license) {
      problems.push(`${pkg.key}: declares no license`);
      continue;
    }
    const { ids, conjunction } = licensesInForce(pkg.license);
    const disallowed = ids.filter((id) => !allowed.has(id));
    // Under AND every named license applies, so every one must be allowed.
    // Under OR we only need one we can elect.
    const bad = conjunction ? disallowed : disallowed.length === ids.length;
    if (conjunction ? disallowed.length > 0 : bad) {
      problems.push(`${pkg.key}: ${pkg.license} is not in deny.toml's allow list`);
    }
  }
  if (problems.length > 0) {
    throw new Error(
      `npm dependencies with unreviewed licenses:\n  ${problems.join('\n  ')}\n` +
        'Add the license to deny.toml only if it is genuinely acceptable to ship.',
    );
  }
}

/**
 * Guard the one omission with real legal teeth.
 *
 * The fonts reach the bundle through CSS, which `rollup-plugin-license` cannot
 * see — verified: it reports 57 packages and none of them is `@fontsource`. The
 * companion CSS collector is what catches them. If that collector ever breaks,
 * the font files would keep shipping while their OFL-1.1 notice quietly
 * disappeared, so fail loudly instead.
 */
function assertFontsAttributed(packages) {
  const assets = join(ROOT, 'frontend', 'dist', 'assets');
  if (!existsSync(assets)) return;
  const fonts = readdirSync(assets).filter((f) => /\.(woff2?|ttf|otf|eot)$/.test(f));
  if (fonts.length === 0) return;
  const attributed = packages.filter((p) => p.via === 'css' || /font/i.test(p.name));
  if (attributed.length === 0) {
    throw new Error(
      `${fonts.length} font files ship in frontend/dist/assets but no font package ` +
        'was attributed — the CSS dependency collector is not working.',
    );
  }
}

function renderPackages(packages) {
  const out = [];
  for (const pkg of packages) {
    const { ids, conjunction } = licensesInForce(pkg.license);
    const elected = conjunction ? ids : [ids.find((id) => id) ?? ids[0]];
    const home = pkg.repository ? ` — ${pkg.repository.replace(/^git\+/, '')}` : '';
    out.push(`#### ${pkg.name} ${pkg.version}\n`);
    out.push(`License: ${pkg.license}${home}\n`);

    if (pkg.licenseText) {
      out.push('```\n' + pkg.licenseText.replace(/\r\n/g, '\n').trimEnd() + '\n```\n');
      continue;
    }
    // No license file in the package. Substitute the canonical text and say so,
    // rather than emitting a package with a license name and no notice.
    for (const id of elected) {
      const text = FALLBACK_TEXTS[id];
      if (!text) {
        throw new Error(
          `${pkg.key} declares ${id} but ships no license file, and there is no ` +
            'canonical text for it in scripts/notices/spdx-fallback-texts.json.',
        );
      }
      out.push(`> This package ships no license file. Canonical ${id} text:\n`);
      out.push('```\n' + text + '\n```\n');
    }
  }
  return out.join('\n');
}

function main() {
  const allowed = allowedLicenses();
  const sections = [];

  sections.push('## Rust crates\n');
  sections.push(
    'Every crate linked into the \`rise\` binary. The server image is built with\n' +
      '`--all-features`, so this is the superset; release archives contain a subset.\n',
  );
  const rust = rustSection();
  assertNoFirstPartyCrates(rust);
  sections.push(rust);

  const web = mergePackages([
    readManifest('frontend-js.json'),
    readManifest('frontend-css.json'),
  ]);
  enforceAllowList(web, allowed);
  assertFontsAttributed(web);
  sections.push('## Web UI\n');
  sections.push(
    'Every npm package bundled into the web interface served by the `rise`\n' +
      'server image. Build-time tooling is absent: this is what the Vite build\n' +
      'actually emitted, not what the lockfile resolves.\n',
  );
  sections.push(renderPackages(web));

  writeFileSync(OUT, `${HEADER}\n${sections.join('\n')}`);
  process.stderr.write(
    `wrote ${OUT}: ${web.length} npm packages, ` +
      `${allowed.size} licenses allowed by deny.toml\n`,
  );
}

main();
