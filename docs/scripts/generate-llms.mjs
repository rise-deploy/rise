#!/usr/bin/env node
/**
 * Generate the llms.txt ecosystem files for Rise's two Starlight doc sites.
 *
 * For each site (user, engineering) this writes, into {site}/public/:
 *   - llms.txt       : curated index (llmstxt.org format)
 *   - llms-full.txt  : full concatenation of every doc page
 *   - <slug>.md      : one raw-markdown file per page (frontmatter stripped,
 *                      an H1 title prepended) so each page is individually
 *                      fetchable by an LLM.
 *
 * Pure Node ESM, no dependencies (Node >= 20). Page ordering / section grouping
 * come from each site's astro.config.mjs sidebar; sidebar slugs with no matching
 * file are skipped with a warning, and any unlisted pages are appended as "More".
 */

import { readdir, readFile, writeFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, relative, basename, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

// --- Configuration --------------------------------------------------------

/** docs/ directory (one level up from docs/scripts/). */
const DOCS_ROOT = fileURLToPath(new URL('../', import.meta.url));

function docsBaseUrl(envName, fallback) {
  return (process.env[envName] || fallback).replace(/\/+$/, '');
}

/** The two Starlight sites. `baseUrl` is the public URL prefix for every link. */
const SITES = [
  {
    name: 'user',
    dir: 'user',
    title: 'Rise User Docs',
    baseUrl: docsBaseUrl('RISE_USER_DOCS_URL', 'https://rise-deploy.github.io/rise/user'),
  },
  {
    name: 'engineering',
    dir: 'engineering',
    title: 'Rise Operator Docs',
    baseUrl: docsBaseUrl('RISE_OPERATOR_DOCS_URL', 'https://rise-deploy.github.io/rise/operator'),
  },
];

// --- Small helpers --------------------------------------------------------

/** Recursively yield every file path under `dir`. */
async function* walk(dir) {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full);
    else if (entry.isFile()) yield full;
  }
}

/**
 * Split a markdown file into frontmatter (simple scalar YAML) and body.
 * Only `title` / `description` scalars are extracted — that is all these docs
 * use, and it avoids pulling in a YAML dependency.
 */
function splitFrontmatter(text) {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(text);
  if (!m) return { fm: {}, body: text };
  const fm = {};
  for (const line of m[1].split(/\r?\n/)) {
    const mm = /^([A-Za-z0-9_-]+)\s*:\s*(.*)$/.exec(line);
    if (!mm) continue;
    let value = mm[2].trim();
    if ((value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))) {
      value = value.slice(1, -1);
    }
    fm[mm[1]] = value;
  }
  return { fm, body: text.slice(m[0].length) };
}

/**
 * Compute the sidebar-matching slug and URL path segment for a content file
 * given its path relative to src/content/docs/.
 *
 *   user-guide/getting-started.md                 -> match 'user-guide/getting-started', url 'user-guide/getting-started'
 *   index.md                                      -> match 'index', url ''  (root)
 *   user-guide/authentication-for-apps/index.md   -> match 'user-guide/authentication-for-apps', url same
 */
function computeSlugs(relPath) {
  let s = relPath.replace(/\\/g, '/').replace(/\.(md|mdx)$/i, '');
  if (s === 'index') return { matchSlug: 'index', urlSlug: '', isRootIndex: true };
  if (s.endsWith('/index')) {
    const base = s.slice(0, -'/index'.length);
    return { matchSlug: base, urlSlug: base, isRootIndex: false };
  }
  return { matchSlug: s, urlSlug: s, isRootIndex: false };
}

function humanize(filename) {
  return basename(filename)
    .replace(/\.(md|mdx)$/i, '')
    .replace(/[-_]+/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

/** Public URL for a resolved page. */
function urlFor(page, baseUrl) {
  const markdownPath = page.isRootIndex ? 'index.md' : `${page.urlSlug}.md`;
  return `${baseUrl}/${markdownPath}`;
}

/** Derive a one-line description from the body when frontmatter has none. */
function deriveDescription(body) {
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (line.startsWith('#')) continue;                 // heading
    if (line.startsWith('import ')) continue;           // mdx import
    if (line.startsWith('```') || line.startsWith('|')) continue; // code / table
    if (/^[-*+]\s/.test(line)) continue;                // list item
    let s = line
      .replace(/\*\*([^*]+)\*\*/g, '$1')
      .replace(/\*([^*]+)\*/g, '$1')
      .replace(/`([^`]+)`/g, '$1')
      .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1');
    if (s.length > 160) s = `${s.slice(0, 157).replace(/\s+\S*$/, '')}…`;
    return s;
  }
  return '';
}

/** First 1-2 prose paragraphs of a page (used as the llms.txt summary). */
function projectSummary(body) {
  const paras = body.split(/\n\s*\n/).map((p) => p.trim()).filter(Boolean);
  const out = [];
  for (const p of paras) {
    if (p.startsWith('#')) break;
    out.push(p);
    if (out.length >= 2) break;
  }
  return out.join('\n\n');
}

// --- astro.config.mjs sidebar parser -------------------------------------

/**
 * Minimal parser for the JS-literal subset used by the Starlight `sidebar:`
 * config: objects, arrays, single/double-quoted strings, bareword keys, commas
 * and // line comments. Robust enough to recover the ordered
 * `{ label, slug, items }` tree without importing the module (which would need
 * the Astro deps).
 */
class SidebarParser {
  constructor(src, pos) { this.src = src; this.pos = pos; }
  skipWs() {
    while (this.pos < this.src.length) {
      const c = this.src[this.pos];
      if (/\s/.test(c)) this.pos++;
      else if (c === '/' && this.src[this.pos + 1] === '/') {
        const nl = this.src.indexOf('\n', this.pos);
        this.pos = nl === -1 ? this.src.length : nl + 1;
      } else break;
    }
  }
  parseValue() {
    this.skipWs();
    const c = this.src[this.pos];
    if (c === '{') return this.parseObject();
    if (c === '[') return this.parseArray();
    if (c === "'" || c === '"') return this.parseString();
    const m = /^[^,}\]\s]+/.exec(this.src.slice(this.pos)); // bareword/number
    const v = m ? m[0] : '';
    this.pos += v.length;
    return v;
  }
  parseObject() {
    this.pos++; // '{'
    const obj = {};
    for (;;) {
      this.skipWs();
      if (this.src[this.pos] === '}') { this.pos++; break; }
      let key;
      const c = this.src[this.pos];
      if (c === "'" || c === '"') key = this.parseString();
      else {
        const m = /^[A-Za-z0-9_]+/.exec(this.src.slice(this.pos));
        key = m ? m[0] : '';
        this.pos += key.length;
      }
      this.skipWs();
      if (this.src[this.pos] === ':') this.pos++;
      this.skipWs();
      obj[key] = this.parseValue();
      this.skipWs();
      if (this.src[this.pos] === ',') { this.pos++; continue; }
      if (this.src[this.pos] === '}') { this.pos++; break; }
    }
    return obj;
  }
  parseArray() {
    this.pos++; // '['
    const arr = [];
    for (;;) {
      this.skipWs();
      if (this.src[this.pos] === ']') { this.pos++; break; }
      arr.push(this.parseValue());
      this.skipWs();
      if (this.src[this.pos] === ',') { this.pos++; continue; }
      if (this.src[this.pos] === ']') { this.pos++; break; }
    }
    return arr;
  }
  parseString() {
    const q = this.src[this.pos];
    this.pos++;
    let str = '';
    while (this.pos < this.src.length && this.src[this.pos] !== q) {
      if (this.src[this.pos] === '\\') { str += this.src[this.pos + 1] ?? ''; this.pos += 2; }
      else { str += this.src[this.pos]; this.pos++; }
    }
    this.pos++; // closing quote
    return str;
  }
}

/** Recursively flatten a node's items into ordered `{label, slug}`. */
function flattenItems(items) {
  const out = [];
  if (!Array.isArray(items)) return out;
  for (const it of items) {
    if (!it || typeof it !== 'object') continue;
    if (typeof it.slug === 'string') out.push({ label: it.label, slug: it.slug });
    if (Array.isArray(it.items)) out.push(...flattenItems(it.items));
  }
  return out;
}

/** Parse the sidebar into ordered groups `[{ label, items: [{label, slug}] }]`. */
function parseSidebar(configText) {
  const m = /\bsidebar\s*:\s*\[/.exec(configText);
  if (!m) return [];
  const openIdx = m.index + m[0].length - 1; // index of '['
  const raw = new SidebarParser(configText, openIdx).parseArray();
  const groups = [];
  const leftover = [];
  for (const node of raw) {
    if (!node || typeof node !== 'object') continue;
    if (Array.isArray(node.items)) groups.push({ label: node.label, items: flattenItems(node.items) });
    else if (typeof node.slug === 'string') leftover.push({ label: node.label, slug: node.slug });
  }
  if (leftover.length) groups.push({ label: 'Docs', items: leftover });
  return groups;
}

// --- Page collection & ordering ------------------------------------------

async function collectPages(contentDir) {
  const pages = [];
  for await (const abs of walk(contentDir)) {
    const rel = relative(contentDir, abs).replace(/\\/g, '/');
    const name = basename(rel);
    if (name.startsWith('_')) continue;            // Starlight partials / drafts
    if (!/\.(md|mdx)$/i.test(name)) continue;
    const text = await readFile(abs, 'utf8');
    const { fm, body } = splitFrontmatter(text);
    const { matchSlug, urlSlug, isRootIndex } = computeSlugs(rel);
    pages.push({
      absPath: abs, relPath: rel,
      title: fm.title || humanize(name),
      description: fm.description || '',
      body, matchSlug, urlSlug, isRootIndex,
    });
  }
  return pages;
}

/** Resolve sidebar groups to page objects (in order); collect unlisted orphans. */
function resolveOrder(groups, pageBySlug, allPages) {
  const ordered = [];
  const seen = new Set();
  for (const g of groups) {
    const items = [];
    for (const it of g.items) {
      const page = pageBySlug.get(it.slug);
      if (!page) { console.warn(`    sidebar slug has no matching file: ${it.slug}`); continue; }
      if (seen.has(page.absPath)) continue;
      seen.add(page.absPath);
      items.push({ ...page, linkLabel: it.label || page.title });
    }
    if (items.length) ordered.push({ label: g.label, items });
  }
  const orphanItems = allPages
    .filter((p) => !seen.has(p.absPath))
    .sort((a, b) => a.matchSlug.localeCompare(b.matchSlug))
    .map((p) => ({ ...p, linkLabel: p.title }));
  return { ordered, orphanItems };
}

// --- Output builders ------------------------------------------------------

function buildLlmsTxt(site, indexPage, ordered, orphanItems) {
  const title = indexPage?.title || site.title;
  const tagline = indexPage?.description || '';
  const summary = indexPage ? projectSummary(indexPage.body) : '';

  const lines = [];
  lines.push(`# ${title}`, '');
  if (tagline) lines.push(`> ${tagline}`, '');
  if (summary) lines.push(summary, '');

  const renderGroup = (label, items) => {
    lines.push(`## ${label}`);
    for (const p of items) {
      const d = p.description.trim() || deriveDescription(p.body);
      lines.push(`- [${p.linkLabel}](${urlFor(p, site.baseUrl)})${d ? `: ${d}` : ''}`);
    }
    lines.push('');
  };
  for (const g of ordered) renderGroup(g.label, g.items);
  if (orphanItems.length) renderGroup('More', orphanItems);

  return `${lines.join('\n').replace(/\n+$/, '')}\n`;
}

function buildLlmsFull(site, indexPage, ordered, orphanItems) {
  const title = indexPage?.title || site.title;
  const all = [...ordered.flatMap((g) => g.items), ...orphanItems];
  const blocks = all.map((p) => `# ${p.title}\n\n${p.body.replace(/^\n+/, '')}`.replace(/\s+$/, ''));
  const header = `# ${title} (Full)\n\n> Full concatenation of ${title} for LLM consumption.\n`;
  return `${header}\n${blocks.join('\n\n---\n\n')}\n`;
}

const perPageContent = (page) => `# ${page.title}\n\n${page.body.replace(/^\n+/, '')}\n`;

// --- Main -----------------------------------------------------------------

async function main() {
  for (const site of SITES) {
    console.log(`\n== ${site.name} (${site.baseUrl}) ==`);
    const siteDir = join(DOCS_ROOT, site.dir);
    const contentDir = join(siteDir, 'src/content/docs');
    const publicDir = join(siteDir, 'public');

    const pages = await collectPages(contentDir);
    const pageBySlug = new Map(pages.map((p) => [p.matchSlug, p]));
    const indexPage = pageBySlug.get('index');

    const configText = await readFile(join(siteDir, 'astro.config.mjs'), 'utf8');
    const groups = parseSidebar(configText);
    const { ordered, orphanItems } = resolveOrder(groups, pageBySlug, pages);

    await writeFile(join(publicDir, 'llms.txt'), buildLlmsTxt(site, indexPage, ordered, orphanItems));
    await writeFile(join(publicDir, 'llms-full.txt'), buildLlmsFull(site, indexPage, ordered, orphanItems));

    const all = [...ordered.flatMap((g) => g.items), ...orphanItems];
    let wrote = 0, skipped = 0;
    for (const p of all) {
      const destRel = p.isRootIndex ? 'index.md' : `${p.urlSlug}.md`;
      const dest = join(publicDir, destRel);
      // Don't clobber hand-maintained public assets: our generated files always
      // begin with an H1 (`# Title`); anything else is left untouched.
      if (existsSync(dest)) {
        const head = (await readFile(dest, 'utf8')).slice(0, 2);
        if (head !== '# ') {
          console.warn(`    skip per-page (existing public asset): ${relative(DOCS_ROOT, dest)}`);
          skipped++; continue;
        }
      }
      await mkdir(dirname(dest), { recursive: true });
      await writeFile(dest, perPageContent(p));
      wrote++;
    }

    const listed = ordered.reduce((n, g) => n + g.items.length, 0);
    console.log(
      `    ${pages.length} pages | ${listed} listed + ${orphanItems.length} orphan | ` +
      `llms.txt + llms-full.txt written | ${wrote} per-page .md written, ${skipped} skipped`
    );
  }
  console.log('\nDone.');
}

main().catch((err) => { console.error(err); process.exit(1); });
