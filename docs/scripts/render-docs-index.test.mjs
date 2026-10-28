import test from 'node:test';
import assert from 'node:assert/strict';
import { renderDocsIndex } from './render-docs-index.mjs';

test('selector keeps production and nested preview destinations', () => {
  for (const base of ['/rise', '/rise/preview/pr-123']) {
    const html = renderDocsIndex(`${base}/user/`, `${base}/operator/`);
    assert.ok(html.includes(`href="${base}/user/"`));
    assert.ok(html.includes(`href="${base}/operator/"`));
  }
});

test('selector is self-contained and escapes destination attributes', () => {
  const html = renderDocsIndex('/user/?q="&x=<value>', '/operator/');
  assert.ok(html.includes('href="/user/?q=&quot;&amp;x=&lt;value&gt;"'));
  assert.ok(html.includes('data:font/woff2;base64,'));
  assert.ok(html.includes('data:image/svg+xml;base64,'));
  assert.doesNotMatch(html, /<script|https?:\/\//);
});

test('selector rejects destinations outside the deployment origin', () => {
  assert.throws(() => renderDocsIndex('//example.com/user/', '/operator/'), TypeError);
  assert.throws(() => renderDocsIndex('javascript:alert(1)', '/operator/'), TypeError);
});
