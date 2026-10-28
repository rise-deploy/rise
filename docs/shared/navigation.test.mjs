import assert from 'node:assert/strict';
import { readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { join, relative } from 'node:path';
import { test } from 'node:test';
import { getSection, hrefFor, paginationForSection, sectionsFor, sidebarFor } from './navigation.mjs';

function itemsIn(items) {
  return items.flatMap((item) => item.items ? itemsIn(item.items) : [item]);
}

async function pagesIn(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  return (await Promise.all(entries.map(async (entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? pagesIn(path) : /\.mdx?$/.test(entry.name) ? [path] : [];
  }))).flat();
}

function pageSlug(file) {
  const slug = file.replace(/\\/g, '/').replace(/\.mdx?$/, '');
  return slug === 'index' ? slug : slug.replace(/\/index$/, '');
}

test('header sections preserve the audience boundary', () => {
  assert.deepEqual(sectionsFor('user').map((section) => section.label), ['Get started', 'Guides', 'Reference', 'Extensions']);
  assert.deepEqual(sectionsFor('engineering').map((section) => section.label), ['Operate', 'Deployment backends', 'Extend', 'Resource API', 'Contribute', 'Changelog']);
  assert.equal(sectionsFor('engineering').find((section) => section.id === 'resources').wip, true);
  assert.equal(sidebarFor('engineering').find((section) => section.label === 'Resource API').badge.text, 'WIP');
  assert.throws(() => sectionsFor('other'), /Unknown documentation site/);
});

test('section links respect root, bundled, published, and pull-request base paths', () => {
  for (const base of ['/', '/docs', '/rise/user/', '/rise/preview/pr-123/user']) {
    assert.equal(hrefFor(base, 'index'), `${base.replace(/\/$/, '') || ''}/`);
    assert.equal(getSection('user', hrefFor(base, 'guides/deploying-from-ci') + '?query=x#example', base).id, 'guides');
    assert.equal(getSection('user', hrefFor(base, 'user-guide/configuration'), base).id, 'reference');
    assert.equal(getSection('user', hrefFor(base, 'index'), base).id, 'start');
  }
  assert.equal(hrefFor('/rise/operator/', 'resources/'), '/rise/operator/resources/');
  assert.equal(getSection('engineering', '/rise/operator/resources/custom-resources/', '/rise/operator').id, 'resources');
  assert.equal(getSection('engineering', '/rise/operator/kubernetes/configuration/', '/rise/operator').id, 'backends');
  assert.equal(getSection('engineering', '/rise/operator/changelog/v0-22-0/', '/rise/operator').id, 'changelog');
  assert.equal(getSection('engineering', '/rise/operator/adr/0001-unified-permission-model/', '/rise/operator').id, 'contribute');
});

for (const site of ['user', 'engineering']) {
  test(`${site} sidebar covers every content page exactly once and has no missing links`, async () => {
    const root = fileURLToPath(new URL(`../${site}/src/content/docs/`, import.meta.url));
    const files = await pagesIn(root);
    const slugs = new Set(files.map((file) => pageSlug(relative(root, file))));
    const sections = sectionsFor(site);
    const items = sections.map((section) => ({ section, leaves: itemsIn(section.items) }));
    for (const slug of slugs) {
      const owners = items.filter(({ leaves }) => leaves.some((item) => item.slug === slug || item.autogenerate && (slug === item.autogenerate.directory || slug.startsWith(item.autogenerate.directory + '/'))));
      assert.equal(owners.length, 1, `${site}/${slug} must appear in one section`);
      assert.equal(getSection(site, hrefFor('/some/base', slug), '/some/base').id, owners[0].section.id);
    }
    for (const { leaves } of items) {
      for (const item of leaves) {
        if (item.slug) assert.ok(slugs.has(item.slug), `${site}/${item.slug} must exist`);
      }
    }
  });
}

test('previous and next links stay inside the current section', () => {
  const base = '/rise/preview/pr-123/operator';
  const prev = { href: hrefFor(base, 'extensions/oauth'), label: 'OAuth providers' };
  const next = { href: hrefFor(base, 'resources/storage'), label: 'Storage model' };
  assert.deepEqual(paginationForSection('engineering', hrefFor(base, 'resources'), base, { prev, next }), { prev: undefined, next });
  assert.deepEqual(paginationForSection('engineering', hrefFor(base, 'resources/schemas'), base, {
    prev: next, next: { href: hrefFor(base, 'contribute'), label: 'Contribute' },
  }), { prev: next, next: undefined });
  assert.deepEqual(paginationForSection('user', '/docs/', '/docs', {}), { prev: undefined, next: undefined });
});
