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
 * come from the shared navigation metadata; sidebar slugs with no matching
 * file are skipped with a warning, and any unlisted pages are appended as "More".
 */

import { readdir, readFile, writeFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, relative, basename, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildAgentPrompt } from './agent-prompt.mjs';
import { RESOURCE_API_STATUS, sidebarFor } from '../shared/navigation.mjs';
import { resolveDocLink } from '../shared/remark-doc-links.mjs';
import { changelogMarkdownForSlug } from './changelog-data.mjs';

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

const APP_BUILDER_SKILL_FILES = [
  'SKILL.md',
  'reference/cli-cheatsheet.md',
  'reference/rise-toml-reference.md',
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

/** Resolve explicit and autogenerated navigation entries to source documents. */
function flattenItems(items, pages) {
  const out = [];
  for (const item of items ?? []) {
    if (typeof item.slug === 'string') out.push({ label: item.label, slug: item.slug });
    if (item.autogenerate?.directory) {
      const directory = item.autogenerate.directory.replace(/\/+$/, '');
      const generated = pages
        .filter((page) => page.matchSlug === directory || page.matchSlug.startsWith(`${directory}/`))
        .sort((a, b) => a.sidebarOrder - b.sidebarOrder || a.matchSlug.localeCompare(b.matchSlug));
      out.push(...generated.map((page) => ({ label: page.title, slug: page.matchSlug })));
    }
    if (item.items) out.push(...flattenItems(item.items, pages));
  }
  return out;
}

// --- Page collection & ordering ------------------------------------------

async function collectPages(contentDir, site) {
  const pages = [];
  for await (const abs of walk(contentDir)) {
    const rel = relative(contentDir, abs).replace(/\\/g, '/');
    const name = basename(rel);
    if (name.startsWith('_')) continue;            // Starlight partials / drafts
    if (!/\.(md|mdx)$/i.test(name)) continue;
    const text = await readFile(abs, 'utf8');
    const { fm, body: sourceBody } = splitFrontmatter(text);
    const { matchSlug, urlSlug, isRootIndex } = computeSlugs(rel);
    const changelogBody = site.name === 'engineering' ? await changelogMarkdownForSlug(matchSlug) : null;
    const body = changelogBody === null ? sourceBody : changelogBody.replace(/^# [^\n]+\n+/, '');
    const sidebarOrder = Number(/^\s+order:\s*(-?\d+)/m.exec(text)?.[1] ?? 0);
    pages.push({
      absPath: abs, relPath: rel,
      title: fm.title || humanize(name),
      description: fm.description || '',
      body, matchSlug, urlSlug, isRootIndex, sidebarOrder,
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

function buildLlmsTxt(site, indexPage, ordered, orphanItems, extraGroups = []) {
  const title = site.title;
  const tagline = indexPage?.description || '';
  const summary = indexPage ? projectSummary(rawPageBody(indexPage, site)) : '';

  const lines = [];
  lines.push(`# ${title}`, '');
  if (tagline) lines.push(`> ${tagline}`, '');
  if (summary) lines.push(summary, '');

  const renderGroup = (label, items) => {
    lines.push(`## ${label}`);
    for (const p of items) {
      const d = p.description.trim() || deriveDescription(p.body);
      lines.push(`- [${p.linkLabel}](${p.url || urlFor(p, site.baseUrl)})${d ? `: ${d}` : ''}`);
    }
    lines.push('');
  };
  for (const g of ordered) renderGroup(g.label, g.items);
  for (const g of extraGroups) renderGroup(g.label, g.items);
  if (orphanItems.length) renderGroup('More', orphanItems);

  return `${lines.join('\n').replace(/\n+$/, '')}\n`;
}

function buildLlmsFull(site, indexPage, ordered, orphanItems) {
  const title = site.title;
  const all = [...ordered.flatMap((g) => g.items), ...orphanItems];
  const blocks = all.map((p) => `# ${p.title}\n\n${rawPageBody(p, site)}`.replace(/\s+$/, ''));
  const header = `# ${title} (Full)\n\n> Full concatenation of ${title} for LLM consumption.\n`;
  return `${header}\n${blocks.join('\n\n---\n\n')}\n`;
}

const GENERATED_MARKER = '<!-- Generated by docs/scripts/generate-llms.mjs; do not edit. -->';

function skillLinksMarkdown(site) {
  return [
    `- [Rise App Builder \`SKILL.md\`](${site.baseUrl}/skills/rise-app-builder/SKILL.md)`,
    `- [CLI cheatsheet](${site.baseUrl}/skills/rise-app-builder/reference/cli-cheatsheet.md)`,
    `- [\`rise.toml\` reference](${site.baseUrl}/skills/rise-app-builder/reference/rise-toml-reference.md)`,
  ].join('\n');
}

function agentPromptMarkdown(site) {
  return `\`\`\`text\n${buildAgentPrompt(site.baseUrl)}\n\`\`\``;
}

function rawPageBody(page, site) {
  let body = page.body
    .replace(/^import\s+.*?;\s*$/gm, '')
    .replace('<SkillLinks />', skillLinksMarkdown(site))
    .replace('<AgentPrompt />', agentPromptMarkdown(site))
    .replace(/(\]\()([^\s)]+)/g, (_, opening, href) => opening + resolveDocLink(href, {
      site: site.name,
      base: site.baseUrl,
      switchUrl: SITES.find((other) => other.name !== site.name)?.baseUrl,
    }))
    .replace(/^\n+/, '');
  if (site.name === 'engineering' && /^resources(?:\/|$)/.test(page.matchSlug) && !body.includes(RESOURCE_API_STATUS)) {
    body = `> **Work in progress.** ${RESOURCE_API_STATUS}\n\n${body}`;
  }
  return body;
}

const legacyPerPageContent = (page, site) => `# ${page.title}\n\n${rawPageBody(page, site)}\n`;
const perPageContent = (page, site) => `${GENERATED_MARKER}\n${legacyPerPageContent(page, site)}`;

async function publishAppBuilderSkill(site, publicDir) {
  if (site.name !== 'user') return [];

  const sourceRoot = join(DOCS_ROOT, '..', 'skills', 'rise-app-builder');
  const publicRoot = join(publicDir, 'skills', 'rise-app-builder');
  for (const relPath of APP_BUILDER_SKILL_FILES) {
    const destination = join(publicRoot, relPath);
    await mkdir(dirname(destination), { recursive: true });
    await writeFile(destination, await readFile(join(sourceRoot, relPath)));
  }

  return [{
    label: 'AI Skills',
    items: [
      {
        linkLabel: 'Rise App Builder Skill',
        url: `${site.baseUrl}/skills/rise-app-builder/SKILL.md`,
        description: 'Agent instructions and decision routing for building and deploying applications on Rise.',
        body: '',
      },
      {
        linkLabel: 'Rise CLI Cheatsheet',
        url: `${site.baseUrl}/skills/rise-app-builder/reference/cli-cheatsheet.md`,
        description: 'Compact Rise CLI command and flag reference for agents.',
        body: '',
      },
      {
        linkLabel: 'rise.toml Reference',
        url: `${site.baseUrl}/skills/rise-app-builder/reference/rise-toml-reference.md`,
        description: 'Configuration fields and examples for single and multi-container Rise applications.',
        body: '',
      },
    ],
  }];
}

// --- Main -----------------------------------------------------------------

async function main() {
  for (const site of SITES) {
    console.log(`\n== ${site.name} (${site.baseUrl}) ==`);
    const siteDir = join(DOCS_ROOT, site.dir);
    const contentDir = join(siteDir, 'src/content/docs');
    const publicDir = join(siteDir, 'public');

    const pages = await collectPages(contentDir, site);
    const pageBySlug = new Map(pages.map((p) => [p.matchSlug, p]));
    const indexPage = pageBySlug.get('index');

    const groups = sidebarFor(site.name).map((section) => ({
      label: section.label, items: flattenItems(section.items, pages),
    }));
    const { ordered, orphanItems } = resolveOrder(groups, pageBySlug, pages);
    const skillGroups = await publishAppBuilderSkill(site, publicDir);

    await writeFile(join(publicDir, 'llms.txt'), buildLlmsTxt(site, indexPage, ordered, orphanItems, skillGroups));
    await writeFile(join(publicDir, 'llms-full.txt'), buildLlmsFull(site, indexPage, ordered, orphanItems));

    const all = [...ordered.flatMap((g) => g.items), ...orphanItems];
    let wrote = 0, skipped = 0;
    for (const p of all) {
      const destRel = p.isRootIndex ? 'index.md' : `${p.urlSlug}.md`;
      const dest = join(publicDir, destRel);
      // Generated files carry an ownership marker; other public assets are preserved.
      if (existsSync(dest)) {
        const existing = await readFile(dest, 'utf8');
        const isGenerated = existing.startsWith(`${GENERATED_MARKER}\n`);
        const isLegacyGenerated = existing === legacyPerPageContent(p, site);
        if (!isGenerated && !isLegacyGenerated) {
          console.warn(`    skip per-page (existing public asset): ${relative(DOCS_ROOT, dest)}`);
          skipped++; continue;
        }
      }
      await mkdir(dirname(dest), { recursive: true });
      await writeFile(dest, perPageContent(p, site));
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
