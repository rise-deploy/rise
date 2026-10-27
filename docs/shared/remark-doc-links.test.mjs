import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import remarkDocLinks, { resolveDocLink } from './remark-doc-links.mjs';

test('documentation prefixes follow deployment and cross-site URLs', () => {
  const options = { site: 'engineering', base: '/rise/preview/pr-42/operator/', switchUrl: '/rise/preview/pr-42/user/' };
  assert.equal(resolveDocLink('/operator-docs/resources/api/#authorization', options), '/rise/preview/pr-42/operator/resources/api/#authorization');
  assert.equal(resolveDocLink('/docs/user-guide/builds/', options), '/rise/preview/pr-42/user/user-guide/builds/');
  assert.equal(resolveDocLink('/operator-docs?search=identity', options), '/rise/preview/pr-42/operator/?search=identity');
  assert.equal(resolveDocLink('/docs/', { site: 'user', base: '/' }), '/');
  assert.equal(resolveDocLink('/docs/', { site: 'user', base: 'https://docs.example.com/user' }), 'https://docs.example.com/user/');
  for (const href of ['/docs-other/', '/api/v1/resources', 'https://example.com/docs/', '#heading', 'mailto:user@example.com']) {
    assert.equal(resolveDocLink(href, options), href);
  }
  assert.equal(resolveDocLink('/operator-docs/', { site: 'user', base: '/docs' }), '/operator-docs/');
  const register = 'https://rise-deploy.github.io/rise/operator/changelog/deprecations/';
  assert.equal(resolveDocLink(register, { site: 'user', base: '/docs' }), register);
  assert.equal(resolveDocLink(register, { site: 'user', base: '/preview/user', switchUrl: '/preview/operator/' }), '/preview/operator/changelog/deprecations/');
  assert.equal(resolveDocLink(register, { site: 'engineering', base: '/operator-docs' }), '/operator-docs/changelog/deprecations/');
});

test('relative source links resolve correctly from nested pretty URLs', () => {
  const fixture = mkdtempSync(join(tmpdir(), 'rise-doc-links-'));
  try {
    const root = join(fixture, 'src/content/docs');
    mkdirSync(join(root, 'resources'), { recursive: true });
    for (const name of ['index.md', 'resources/index.mdx', 'resources/api.md', 'resources/storage.md']) writeFileSync(join(root, name), '');
    const tree = { type: 'root', children: [
      { type: 'link', url: './storage.md#revisions' },
      { type: 'link', url: '../index.md' },
      { type: 'definition', url: './storage' },
      { type: 'link', url: 'unrelated.json' },
      { type: 'link', url: '../../outside.md' },
      { type: 'mdxJsxFlowElement', attributes: [{ name: 'href', value: './storage.md' }] },
    ] };
    remarkDocLinks({ site: 'engineering', base: '/preview/operator' })(tree, { path: join(root, 'resources/api.md') });
    assert.deepEqual(tree.children.slice(0, 5).map((node) => node.url), [
      '/preview/operator/resources/storage/#revisions', '/preview/operator/',
      '/preview/operator/resources/storage/', 'unrelated.json', '../../outside.md',
    ]);
    assert.equal(tree.children[5].attributes[0].value, '/preview/operator/resources/storage/');
  } finally { rmSync(fixture, { recursive: true, force: true }); }
});
