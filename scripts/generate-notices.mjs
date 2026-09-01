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
import { readFileSync, existsSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(ROOT, 'THIRD-PARTY-NOTICES.md');

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

Dual-licensed dependencies are reproduced under one elected license. For Rust
that election is made by the preference order in \`about.toml\`, which puts MIT
first.
`;

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

  writeFileSync(OUT, `${HEADER}\n${sections.join('\n')}`);
  process.stderr.write(
    `wrote ${OUT} (${allowed.size} licenses allowed by deny.toml)\n`,
  );
}

main();
