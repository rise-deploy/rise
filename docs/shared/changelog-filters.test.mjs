import assert from 'node:assert/strict';
import test from 'node:test';
import { matchesBackend } from './changelog-filters.mjs';

test('universal upgrade guidance remains visible under every backend filter', () => {
  for (const selected of ['all', 'kubernetes', 'docker', 'ecs']) {
    assert.equal(matchesBackend(['all'], selected), true, selected);
  }
});

test('backend-specific changes appear only for matching backends or the all filter', () => {
  const cases = [
    { supported: ['kubernetes'], visible: ['all', 'kubernetes'] },
    { supported: ['docker', 'ecs'], visible: ['all', 'docker', 'ecs'] },
  ];
  for (const { supported, visible } of cases) {
    for (const selected of ['all', 'kubernetes', 'docker', 'ecs']) {
      assert.equal(matchesBackend(supported, selected), visible.includes(selected), `${supported}: ${selected}`);
    }
  }
});
