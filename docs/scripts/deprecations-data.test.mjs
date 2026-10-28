import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { deriveDeprecations, groupDeprecations, deprecationStatus, lifecycleVersionLabel, changelogChangeLink, renderDeprecationsMarkdown } from './deprecations-data.mjs';

const announcement = {
  id: 'deprecation-signal', title: 'Deprecate direct external tokens',
  type: 'deprecation', feature: 'external-jwts', stage: 'deprecated',
  impact: 'action-required', backends: ['all'], details: 'Find callers of the deprecated path.',
  featureTitle: 'Raw external JWTs', availability: 'Accepted while the compatibility setting is enabled.',
  migration: 'Exchange the token first.', firstCandidate: { version: '0.23.0-rc5', date: '2026-08-18' },
};
const plannedDefault = {
  id: 'default-change', title: 'Disable raw external tokens by default',
  type: 'deprecation', feature: 'external-jwts', stage: 'default-disabled', targetVersion: '0.25.0',
  impact: 'breaking', backends: ['all'], details: 'A planned default change.',
  availability: 'Future availability instructions.', migration: 'Future migration instructions.',
};

function document(status, entries, { version = '0.24.0', date = '2026-10-27' } = {}) {
  return {
    id: status === 'released' ? version : status === 'upcoming' ? 'unreleased' : 'planned',
    status, version: status === 'released' ? version : null, date: status === 'released' ? date : null,
    title: status === 'released' ? version : status, introduction: '',
    sections: [{ id: 'changes', title: '', entries: structuredClone(entries) }],
  };
}

function milestone(records, kind) {
  return records[0].events.find(event => event.kind === kind);
}

test('implemented deprecations derive their lifecycle and retain explicit candidate evidence', () => {
  const documents = [document('upcoming', [announcement]), document('planned', [plannedDefault])];
  const before = structuredClone(documents);
  const records = deriveDeprecations(documents);
  assert.equal(records.length, 1);
  assert.equal(records[0].id, 'external-jwts');
  assert.equal(records[0].title, announcement.featureTitle);
  assert.equal(records[0].availability, announcement.availability);
  assert.equal(records[0].migration, announcement.migration);
  assert.equal(deprecationStatus(records[0]), 'deprecated');
  assert.deepEqual(records[0].events.map(event => event.kind), ['introduced', 'deprecated', 'default-disabled', 'removed']);
  assert.deepEqual(milestone(records, 'introduced'), { kind: 'introduced', state: 'unknown', version: null, date: null });
  assert.deepEqual(milestone(records, 'deprecated'), {
    kind: 'deprecated', state: 'upcoming', version: 'unreleased', date: null,
    changeId: announcement.id, firstCandidate: announcement.firstCandidate,
  });
  assert.notEqual(milestone(records, 'deprecated').firstCandidate, documents[0].sections[0].entries[0].firstCandidate);
  assert.deepEqual(milestone(records, 'removed'), { kind: 'removed', state: 'unscheduled', version: null, date: null });
  assert.deepEqual(documents, before);
});

test('moving an entry into a release inherits its version and date without lifecycle stamping', () => {
  const upcoming = document('upcoming', [announcement]);
  const planned = document('planned', [plannedDefault]);
  const before = deriveDeprecations([upcoming, planned]);
  const released = document('released', upcoming.sections[0].entries, { version: '0.24.0', date: '2026-11-02' });
  const after = deriveDeprecations([document('upcoming', []), released, planned]);
  assert.deepEqual(milestone(after, 'deprecated'), { ...milestone(before, 'deprecated'), state: 'released', version: '0.24.0', date: '2026-11-02' });
  assert.deepEqual(milestone(after, 'default-disabled'), milestone(before, 'default-disabled'));
  assert.equal(changelogChangeLink(announcement.id, [released, planned]), '/operator-docs/changelog/v0-24-0/#change-deprecation-signal');
  assert.equal(changelogChangeLink(plannedDefault.id, [released, planned]), '/operator-docs/changelog/#change-default-change');
  assert.equal(Object.hasOwn(released.sections[0].entries[0], 'date'), false);
});

test('planned defaults and removals cannot change current availability or migration instructions', () => {
  const removal = { ...plannedDefault, id: 'remove-external-jwts', stage: 'removed', targetVersion: '0.26.0' };
  const records = deriveDeprecations([document('planned', [plannedDefault, removal]), document('released', [announcement])]);
  assert.equal(records[0].availability, announcement.availability);
  assert.equal(records[0].migration, announcement.migration);
  assert.equal(deprecationStatus(records[0]), 'deprecated');
  assert.equal(milestone(records, 'removed').state, 'planned');
  const markdown = renderDeprecationsMarkdown(records, { documents: [document('released', [announcement]), document('planned', [plannedDefault, removal])] });
  assert.match(markdown, /Deprecated · still available/);
  assert.match(markdown, /Planned for 0\.25\.0/);
  assert.match(markdown, /Planned for 0\.26\.0/);
  assert.doesNotMatch(markdown, /Future migration instructions/);
  assert.match(markdown, /0\.23\.0-rc5/);
});

test('implemented defaults update current instructions without implying removal', () => {
  const disabled = { ...plannedDefault, availability: 'Available only with an explicit opt-in.', migration: 'Exchange tokens or opt in temporarily.' };
  const older = document('released', [announcement], { version: '0.24.0', date: '2026-11-02' });
  const newer = document('released', [disabled], { version: '0.25.0', date: '2027-01-04' });
  const records = deriveDeprecations([newer, older]);
  assert.equal(records[0].availability, disabled.availability);
  assert.equal(records[0].migration, disabled.migration);
  assert.equal(deprecationStatus(records[0]), 'deprecated');
  assert.deepEqual(deriveDeprecations([older, newer]), records);
  assert.equal(milestone(records, 'default-disabled').date, '2027-01-04');
  assert.equal(lifecycleVersionLabel(milestone(records, 'removed')), 'Not scheduled');
});

test('an implemented removal changes status while preserving the deprecation history', () => {
  const removal = { ...plannedDefault, id: 'remove-external-jwts', stage: 'removed', availability: 'The direct-token path is removed.', migration: 'Use token exchange.' };
  const records = deriveDeprecations([document('released', [announcement]), document('upcoming', [removal])]);
  assert.equal(deprecationStatus(records[0]), 'removed');
  assert.equal(milestone(records, 'removed').version, 'unreleased');
  assert.equal(records[0].availability, removal.availability);
  assert.equal(records[0].migration, removal.migration);
  assert.equal(milestone(records, 'deprecated').version, '0.24.0');
});

test('the register groups current deprecations and removals and archives only their register notices', async () => {
  const { parseChangelogYaml, stringifyChangelogDocument, renderChangelogMarkdown } = await import('./changelog-data.mjs');
  const removal = { ...announcement, id: 'remove-external-jwts', stage: 'removed', details: 'Direct external JWT support is removed.', availability: 'Direct external JWT support is removed.' };
  delete removal.firstCandidate;
  const other = { ...announcement, id: 'other-announcement', feature: 'another-feature', featureTitle: 'Another supported feature' };
  const older = document('released', [announcement, other]);
  const newer = document('released', [removal], { version: '0.25.0', date: '2027-01-04' });
  const records = deriveDeprecations([older, newer]);
  assert.deepEqual(groupDeprecations(records).map(group => [group.id, group.records.map(record => record.id)]), [
    ['deprecated', ['another-feature']], ['removed', ['external-jwts']],
  ]);
  const markdown = renderDeprecationsMarkdown(records, { documents: [older, newer] });
  assert.ok(markdown.indexOf('## Deprecated\n') < markdown.indexOf('## Removed\n'));
  assert.match(markdown, /### Another supported feature/);

  newer.sections[0].entries[0].archived = true;
  const roundTrip = parseChangelogYaml(stringifyChangelogDocument(newer), '0.25.0.yaml');
  const archived = deriveDeprecations([older, roundTrip]);
  assert.equal(archived.find(record => record.id === 'external-jwts').archived, true);
  assert.deepEqual(groupDeprecations(archived).map(group => group.records.map(record => record.id)), [['another-feature'], []]);
  assert.doesNotMatch(renderDeprecationsMarkdown(archived, { documents: [older, roundTrip] }), /Raw external JWTs/);
  assert.match(renderChangelogMarkdown(roundTrip), /Direct external JWT support is removed/);
  assert.match(renderChangelogMarkdown(roundTrip), /Exchange the token first/);
});

test('archiving cannot hide a functioning feature or an unreleased removal', async () => {
  const { validateChangelogDocument } = await import('./changelog-data.mjs');
  for (const [status, stage, archived] of [
    ['released', 'deprecated', true],
    ['released', 'default-disabled', true],
    ['upcoming', 'removed', true],
    ['planned', 'removed', true],
    ['released', 'removed', 'true'],
  ]) {
    assert.throws(() => validateChangelogDocument(document(status, [{ ...announcement, stage, archived }])), /released removal milestones only/);
  }
});

test('implemented milestone details replace stale availability when explicit availability is omitted', () => {
  const older = document('released', [announcement]);
  for (const [stage, details, status] of [
    ['default-disabled', 'Available only with an explicit compatibility opt-in.', 'deprecated'],
    ['removed', 'The direct-token path is unavailable.', 'removed'],
  ]) {
    const entry = { ...plannedDefault, id: `milestone-${stage}`, stage, details };
    delete entry.availability;
    delete entry.migration;
    const records = deriveDeprecations([older, document('upcoming', [entry])]);
    assert.equal(records[0].availability, details);
    assert.equal(records[0].migration, announcement.migration);
    assert.equal(deprecationStatus(records[0]), status);
    assert.notEqual(records[0].availability, announcement.availability);
  }
});

test('planned-only announcements and ordinary changes are not current deprecations', () => {
  const introduced = { ...announcement, id: 'introduced-external-jwts', stage: 'introduced' };
  assert.deepEqual(deriveDeprecations([document('released', [introduced]), document('planned', [announcement])]), []);
  assert.deepEqual(deriveDeprecations([document('upcoming', [{ ...announcement, type: 'change' }])]), []);
  assert.match(renderDeprecationsMarkdown([]), /No implemented deprecations are recorded/);
});

test('unknown milestone versions and dates remain unknown, including unscheduled planned targets', () => {
  const unspecified = { ...plannedDefault };
  delete unspecified.targetVersion;
  const records = deriveDeprecations([document('upcoming', [announcement]), document('planned', [unspecified])]);
  assert.deepEqual(milestone(records, 'introduced'), { kind: 'introduced', state: 'unknown', version: null, date: null });
  assert.equal(lifecycleVersionLabel(milestone(records, 'introduced')), 'Not established');
  assert.equal(lifecycleVersionLabel(milestone(records, 'deprecated')), 'Unreleased');
  assert.equal(milestone(records, 'default-disabled').version, null);
  assert.equal(milestone(records, 'default-disabled').date, null);
  assert.equal(lifecycleVersionLabel(milestone(records, 'default-disabled')), 'Planned · version not announced');
  assert.equal(lifecycleVersionLabel(milestone(records, 'removed')), 'Not scheduled');
});

test('conflicting stages and invalid milestone evidence fail instead of inventing lifecycle facts', () => {
  assert.throws(() => deriveDeprecations([document('upcoming', [announcement, { ...announcement, id: 'duplicate' }])]), /duplicate deprecated milestone/);
  assert.throws(() => deriveDeprecations([document('released', [announcement]), document('planned', [{ ...announcement, id: 'duplicate' }])]), /duplicate deprecated milestone/);
  assert.throws(() => deriveDeprecations([document('upcoming', [{ ...announcement, stage: 'unknown-stage' }])]), /unknown lifecycle stage/);
  assert.throws(() => deriveDeprecations([document('upcoming', [{ ...announcement, migration: ' ' }])]), /needs migration instructions/);
  assert.throws(() => deriveDeprecations([document('released', [announcement], { date: '2027-02-30' })]), /release date/);
  assert.throws(() => deriveDeprecations([document('upcoming', [{ ...announcement, firstCandidate: { version: '0.23.0-rc5', date: '2026-02-30' } }])]), /candidate evidence/);
  assert.throws(() => changelogChangeLink('missing', []), /unknown changelog entry/);
  assert.equal(changelogChangeLink(undefined, []), null);
});

test('full release publishing derives the new milestone from release YAML without a sibling registry', async t => {
  const { publishChangelogRelease, loadChangelog, stringifyChangelogDocument, changelogMarkdownForSlug } = await import('./changelog-data.mjs');
  const root = await mkdtemp(join(tmpdir(), 'rise-derived-deprecations-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const dataDir = join(root, 'changelog');
  const pagesDir = join(root, 'pages');
  await mkdir(dataDir);
  await mkdir(pagesDir);
  await writeFile(join(dataDir, 'unreleased.yaml'), stringifyChangelogDocument(document('upcoming', [announcement])));
  await writeFile(join(dataDir, 'planned.yaml'), stringifyChangelogDocument(document('planned', [plannedDefault])));
  await publishChangelogRelease('0.24.0', 'Release summary.', '2026-11-02', { dataDir, pagesDir });
  const documents = await loadChangelog({ dataDir });
  const records = deriveDeprecations(documents);
  assert.equal(milestone(records, 'deprecated').version, '0.24.0');
  assert.equal(milestone(records, 'deprecated').date, '2026-11-02');
  assert.equal(milestone(records, 'default-disabled').state, 'planned');
  assert.deepEqual((await readdir(root)).sort(), ['changelog', 'pages']);
  const markdown = await changelogMarkdownForSlug('changelog/deprecations', { dataDir });
  assert.match(markdown, /v0-24-0\/#change-deprecation-signal/);
  assert.match(markdown, /Deprecated · still available/);
  assert.match(markdown, /Planned for 0\.25\.0/);
});


test('raw external-token lifecycle uses the release entries and retains its documented metadata', async () => {
  const { loadChangelog } = await import('./changelog-data.mjs');
  const documents = await loadChangelog();
  const record = deriveDeprecations(documents).find(record => record.id === 'raw-external-service-account-tokens');
  const entry = documents.flatMap(document => document.sections.flatMap(section => section.entries)).find(entry => entry.id === 'upcoming-042');
  assert.ok(record);
  assert.equal(record.title, entry.featureTitle);
  assert.equal(record.availability, entry.availability);
  assert.equal(record.migration, entry.migration);
  assert.match(record.migration, /RISE_IDENTITY/);
  assert.match(record.migration, /POST \/api\/v1\/auth\/token/);
  assert.equal(deprecationStatus(record), 'deprecated');
  assert.deepEqual(record.events.find(event => event.kind === 'introduced'), { kind: 'introduced', state: 'unknown', version: null, date: null });
  const deprecated = record.events.find(event => event.kind === 'deprecated');
  assert.equal(deprecated.changeId, 'upcoming-042');
  assert.deepEqual(deprecated.firstCandidate, { version: '0.23.0-rc5', date: '2026-08-18' });
  assert.equal(record.events.find(event => event.kind === 'default-disabled').version, '0.25.0');
  assert.equal(record.events.find(event => event.kind === 'default-disabled').state, 'planned');
  assert.equal(lifecycleVersionLabel(record.events.find(event => event.kind === 'removed')), 'Not scheduled');
  for (const event of record.events) if (event.changeId) assert.ok(changelogChangeLink(event.changeId, documents));
});
