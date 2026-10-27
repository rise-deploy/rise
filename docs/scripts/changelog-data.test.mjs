import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { loadChangelog, validateChangelog, validateChangelogDocument, parseChangelogYaml, stringifyChangelogDocument, renderChangelogMarkdown, changelogMarkdownForSlug, publishChangelogRelease } from './changelog-data.mjs';

const entry = {
  id: 'hostnames', title: 'Ingress hostname templates', type: 'change', impact: 'breaking', backends: ['docker', 'ecs'],
  details: 'Set a bare hostname.\n\n- Configure wildcard DNS.\n- Keep the exact identity settings.\n\n```sql\nSELECT name FROM projects;\n```',
  action: 'Use `{project_name}.apps.example.com` before upgrading.',
};
const upcoming = { id: 'unreleased', status: 'upcoming', version: null, date: null, title: 'Unreleased', introduction: 'Merged changes.', sections: [{ id: 'changes', title: 'Upgrade notes', entries: [entry] }] };
const planned = { id: 'planned', status: 'planned', version: null, date: null, title: 'Planned changes', introduction: 'Not yet merged.', sections: [{ id: 'future', title: 'Not yet scheduled', entries: [{ ...entry, id: 'future-change', targetVersion: '0.25.0' }] }] };

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'rise-changelog-'));
  const dataDir = join(root, 'data');
  const pagesDir = join(root, 'pages');
  await mkdir(dataDir);
  await mkdir(pagesDir);
  const source = '# Review before upgrading.\n' + stringifyChangelogDocument(upcoming).replace('id: hostnames', 'id: hostnames # Keep this stable.');
  await writeFile(join(dataDir, 'unreleased.yaml'), source);
  await writeFile(join(dataDir, 'planned.yaml'), stringifyChangelogDocument(planned));
  t.after(() => rm(root, { recursive: true, force: true }));
  return { dataDir, pagesDir };
}

test('canonical YAML retains archived notes, groups, and rich upgrade instructions', async () => {
  const documents = await loadChangelog();
  const count = document => document.sections.reduce((sum, section) => sum + section.entries.length, 0);
  const entries = documents.flatMap(document => document.sections.flatMap(section => section.entries));
  const ids = new Set(entries.map(entry => entry.id));
  for (const [prefix, length] of [['upcoming', 50], ['planned', 7]]) for (let index = 1; index <= length; index++) assert.ok(ids.has(`${prefix}-${String(index).padStart(3, '0')}`));
  for (const [version, length] of [['0.22.0', 12], ['0.21.1', 1], ['0.21.0', 13], ['0.20.2', 2], ['0.20.1', 11], ['0.20.0', 9]]) assert.equal(count(documents.find(document => document.version === version)), length);
  const index = await changelogMarkdownForSlug('changelog');
  assert.match(index, /Known release-candidate limitations/);
  assert.match(index, /SELECT parent\.name AS organization/);
  assert.match(index, /SERIALIZABLE/);
  assert.match(index, /Routine updates to tokio/);
  assert.match(index, /raw external tokens are refused by default/);
  const deprecated = entries.find(entry => entry.id === 'upcoming-042');
  assert.equal(deprecated.type, 'deprecation');
  assert.equal(deprecated.stage, 'deprecated');
  assert.match(deprecated.migration, /rise sa list/);
  assert.match(deprecated.migration, /RISE_IDENTITY/);
  assert.equal(deprecated.action, undefined);
  assert.match(index, /#### Migration/);
  const released = await changelogMarkdownForSlug('changelog/v0-22-0');
  assert.match(released, /Released 2026-05-18/);
  assert.match(released, /Legacy Metacontroller adoption code removed/);
  assert.match(released, /Routine updates to tokio/);
  assert.equal(await changelogMarkdownForSlug('resources'), null);
});

test('minimal YAML gets documented defaults and preserves literal Markdown', () => {
  const parsed = parseChangelogYaml('version: unreleased\nchanges:\n  - id: sample\n    title: Sample change\n    details: |-\n      First paragraph with `code`.\n\n      - A list item\n      - Another item\n', 'unreleased.yaml');
  assert.equal(parsed.status, 'upcoming');
  assert.equal(parsed.version, null);
  assert.equal(parsed.sections[0].id, 'changes');
  assert.deepEqual(parsed.sections[0].entries[0], { id: 'sample', title: 'Sample change', details: 'First paragraph with `code`.\n\n- A list item\n- Another item', type: 'change', impact: 'none', backends: ['all'] });
  const deprecation = parseChangelogYaml('version: unreleased\nchanges:\n  - id: old-auth\n    title: Old authentication is deprecated\n    type: deprecation\n    feature: old-auth\n    stage: deprecated\n    migration: |-\n      Exchange tokens before authentication.\n');
  assert.equal(deprecation.sections[0].entries[0].impact, 'action-required');
  assert.deepEqual(parseChangelogYaml(stringifyChangelogDocument(upcoming), 'unreleased.yaml'), upcoming);
  assert.match(stringifyChangelogDocument(upcoming), /details: \|-/);
});

test('YAML mistakes fail with their filename instead of dropping author intent', () => {
  const cases = [
    'version: unreleased\nversion: planned\nchanges: []',
    'version: unreleased\nchanges: []\nmigraton: text',
    'version: unreleased\ngroups:\n  - id: x\n    titel: Typo\nchanges: []',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    migraton: text',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    group: unknown',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    backends: [unknown]',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    type: deprecation\n    feature: x\n    stage: deprecated',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    type: deprecation\n    migration: Migrate',
    'version: unreleased\nchanges:\n  - id: x\n    title: X\n    backends:',
  ];
  for (const source of cases) assert.throws(() => parseChangelogYaml(source, 'unreleased.yaml'), /unreleased\.yaml:/);
  assert.throws(() => parseChangelogYaml('version: 0.23.0\ndate: 2026-02-30\nchanges: []', '0.23.0.yaml'), /invalid release date/);
  assert.throws(() => parseChangelogYaml('version: planned\nchanges: []', 'unreleased.yaml'), /filename must match/);
  assert.throws(() => parseChangelogYaml('version: unreleased\ndate: 2026-10-27\nchanges: []'), /release date is only/);
});

test('invalid normalized metadata and duplicate ids fail before publishing', () => {
  for (const mutate of [
    value => value.sections[0].entries[0].impact = 'unknown',
    value => value.sections[0].entries[0].backends = ['all', 'docker'],
    value => value.sections[0].entries.push(value.sections[0].entries[0]),
    value => value.sections.push(value.sections[0]),
  ]) {
    const invalid = structuredClone(upcoming);
    mutate(invalid);
    assert.throws(() => validateChangelogDocument(invalid), /Invalid changelog/);
  }
  assert.deepEqual(validateChangelog([planned, upcoming]).map(document => document.id), ['unreleased', 'planned']);
  assert.throws(() => validateChangelog([upcoming, { ...planned, sections: upcoming.sections }]), /duplicate entry/);
});

test('publishing promotes complete YAML notes with comments, preserves planned items, and creates the same page URL', async t => {
  const paths = await fixture(t);
  const plannedBefore = await readFile(join(paths.dataDir, 'planned.yaml'), 'utf8');
  const release = await publishChangelogRelease('0.23.0', '# Release Notes\n\nRelease summary.\n\n## Download installers\n\nDownload table.', '2026-10-27', paths);
  assert.deepEqual(release.sections, upcoming.sections);
  assert.equal(release.introduction, 'Release summary.');
  const source = await readFile(join(paths.dataDir, '0.23.0.yaml'), 'utf8');
  assert.match(source, /# Review before upgrading\./);
  assert.match(source, /# Keep this stable\./);
  assert.match(source, /details: \|-/);
  assert.equal(parseChangelogYaml(source, '0.23.0.yaml').date, '2026-10-27');
  const nextSource = await readFile(join(paths.dataDir, 'unreleased.yaml'), 'utf8');
  assert.deepEqual(parseChangelogYaml(nextSource, 'unreleased.yaml').sections, []);
  assert.match(nextSource, /# Review before upgrading\./);
  assert.equal(await readFile(join(paths.dataDir, 'planned.yaml'), 'utf8'), plannedBefore);
  const page = await readFile(join(paths.pagesDir, 'v0-23-0.mdx'), 'utf8');
  assert.match(page, /<Changelog releaseId="0.23.0"/);
  assert.match(page, /tableOfContents: false/);
  assert.match(renderChangelogMarkdown(release), /SELECT name FROM projects;/);
  assert.deepEqual((await readdir(paths.dataDir)).sort(), ['0.23.0.yaml', 'planned.yaml', 'unreleased.yaml']);
});

test('publishing a deprecation records its lifecycle through the containing release without a registry', async t => {
  const paths = await fixture(t);
  const source = parseChangelogYaml('version: unreleased\nchanges:\n  - id: old-auth\n    title: Old auth is deprecated\n    type: deprecation\n    feature: old-auth\n    stage: deprecated\n    migration: Exchange tokens.\n');
  await writeFile(join(paths.dataDir, 'unreleased.yaml'), stringifyChangelogDocument(source));
  const release = await publishChangelogRelease('0.24.0', '', '2026-10-27', paths);
  assert.equal(release.sections[0].entries[0].stage, 'deprecated');
  const markdown = await changelogMarkdownForSlug('changelog/deprecations', paths);
  assert.match(markdown, /0\.24\.0/);
  assert.match(markdown, /2026-10-27/);
  assert.match(markdown, /Exchange tokens/);
});

test('an existing release or invalid request cannot consume upcoming YAML notes', async t => {
  const paths = await fixture(t);
  const before = await readFile(join(paths.dataDir, 'unreleased.yaml'), 'utf8');
  await writeFile(join(paths.pagesDir, 'v0-23-0.mdx'), 'Existing page.');
  await assert.rejects(publishChangelogRelease('0.23.0', '', '2026-10-27', paths), /already exists/);
  assert.equal(await readFile(join(paths.dataDir, 'unreleased.yaml'), 'utf8'), before);
  assert.deepEqual((await loadChangelog(paths)).map(document => document.id), ['unreleased', 'planned']);
  await assert.rejects(publishChangelogRelease('0.23.0-rc.1', '', '2026-10-27', paths), /full release version/);
  await assert.rejects(publishChangelogRelease('0.23.0', '', 'invalid', paths), /invalid release date/);
  assert.equal(await readFile(join(paths.dataDir, 'unreleased.yaml'), 'utf8'), before);
});
