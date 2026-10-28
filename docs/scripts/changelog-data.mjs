import { readdir, readFile, writeFile, mkdir, access, rename, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { basename, join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { Document, Scalar, isMap, isScalar, isSeq, parseDocument, visit } from 'yaml';
import { deriveDeprecations, renderDeprecationsMarkdown } from './deprecations-data.mjs';

export const CHANGELOG_DATA_DIR = fileURLToPath(new URL('../engineering/src/data/changelog/', import.meta.url));
export const CHANGELOG_PAGES_DIR = fileURLToPath(new URL('../engineering/src/content/docs/changelog/', import.meta.url));
export const IMPACTS = ['breaking', 'action-required', 'configuration', 'none'];
export const CHANGE_TYPES = ['feature', 'fix', 'change', 'maintenance', 'limitation', 'deprecation'];
export const BACKENDS = ['all', 'kubernetes', 'docker', 'ecs'];
export const DEPRECATION_STAGES = ['introduced', 'deprecated', 'default-disabled', 'removed'];
export const impactLabels = { breaking: 'Breaking', 'action-required': 'Action required', configuration: 'Config change', none: 'No action required' };
const fullVersion = /^\d+\.\d+\.\d+$/;

function assert(condition, message) { if (!condition) throw new Error(`Invalid changelog: ${message}`); }
function validDate(date) {
  const timestamp = Date.parse(`${date}T00:00:00Z`);
  return /^\d{4}-\d{2}-\d{2}$/.test(date) && Number.isFinite(timestamp) && new Date(timestamp).toISOString().startsWith(date);
}

export function validateChangelogDocument(document) {
  assert(document && typeof document === 'object', 'document must be an object');
  assert(typeof document.id === 'string' && /^[a-z0-9][a-z0-9.-]*$/.test(document.id), 'id must be a stable lowercase identifier');
  assert(['upcoming', 'released', 'planned'].includes(document.status), `${document.id}: unknown status`);
  assert(typeof document.title === 'string' && document.title.length > 0, `${document.id}: title is required`);
  assert(typeof document.introduction === 'string', `${document.id}: introduction must be Markdown`);
  if (document.status === 'released') {
    assert(fullVersion.test(document.version), `${document.id}: released version must be X.Y.Z`);
    assert(validDate(document.date), `${document.id}: invalid release date`);
    assert(document.id === document.version, `${document.id}: release id must equal its version`);
  } else {
    assert(document.version === null && document.date === null, `${document.id}: only released documents have a version and date`);
  }
  assert(Array.isArray(document.sections), `${document.id}: sections must be an array`);
  const ids = new Set();
  const sectionIds = new Set();
  for (const section of document.sections) {
    assert(typeof section.id === 'string' && /^[a-z0-9][a-z0-9-]*$/.test(section.id) && !sectionIds.has(section.id), `${document.id}: invalid or duplicate section id`);
    sectionIds.add(section.id);
    assert(typeof section.title === 'string', `${document.id}: section title must be a string`);
    assert(section.introduction === undefined || typeof section.introduction === 'string', `${document.id}: section introduction must be Markdown`);
    assert(Array.isArray(section.entries), `${document.id}/${section.id}: entries must be an array`);
    for (const entry of section.entries) {
      assert(typeof entry.id === 'string' && /^[a-z0-9][a-z0-9-]*$/.test(entry.id) && !ids.has(entry.id), `${document.id}: invalid or duplicate entry id ${entry.id}`);
      ids.add(entry.id);
      assert(typeof entry.title === 'string' && entry.title.length > 0, `${entry.id}: title is required`);
      assert(typeof entry.details === 'string', `${entry.id}: details must be Markdown`);
      assert(CHANGE_TYPES.includes(entry.type), `${entry.id}: unknown change type`);
      assert(IMPACTS.includes(entry.impact), `${entry.id}: unknown impact`);
      assert(Array.isArray(entry.backends) && entry.backends.length > 0 && entry.backends.every(backend => BACKENDS.includes(backend)), `${entry.id}: unknown backend`);
      assert(!entry.backends.includes('all') || entry.backends.length === 1, `${entry.id}: all cannot be combined with specific backends`);
      for (const field of ['action', 'migration', 'featureTitle', 'availability']) assert(entry[field] === undefined || typeof entry[field] === 'string', `${entry.id}: ${field} must be Markdown`);
      if (entry.type === 'deprecation') {
        assert(typeof entry.feature === 'string' && /^[a-z0-9][a-z0-9-]*$/.test(entry.feature), `${entry.id}: deprecation feature must have a stable identifier`);
        assert(DEPRECATION_STAGES.includes(entry.stage), `${entry.id}: deprecation stage is required`);
        if (entry.stage === 'deprecated') assert(typeof entry.migration === 'string' && entry.migration.trim(), `${entry.id}: a deprecated feature needs migration instructions`);
      } else {
        assert(entry.feature === undefined && entry.stage === undefined, `${entry.id}: feature and stage require type deprecation`);
      }
      if (entry.archived !== undefined) assert(typeof entry.archived === 'boolean' && entry.type === 'deprecation' && entry.stage === 'removed' && document.status === 'released', `${entry.id}: archived is a boolean for released removal milestones only`);
      assert(entry.targetVersion === undefined || document.status === 'planned' && fullVersion.test(entry.targetVersion), `${entry.id}: targetVersion is a planned X.Y.Z version`);
      if (entry.firstCandidate) assert(/^\d+\.\d+\.\d+-[0-9A-Za-z.-]+$/.test(entry.firstCandidate.version) && validDate(entry.firstCandidate.date), `${entry.id}: invalid candidate evidence`);
    }
  }
  return document;
}

function parseYamlDocument(text, filename) {
  const document = parseDocument(text, { uniqueKeys: true });
  assert(document.errors.length === 0, `${filename ?? 'YAML'}: ${document.errors.map(error => error.message).join('; ')}`);
  return document;
}

function checkFields(object, allowed, label) {
  assert(Object.keys(object).every(key => allowed.includes(key)), `${label}: unknown field ${Object.keys(object).filter(key => !allowed.includes(key)).join(', ')}`);
}

function normalizeChangelogYaml(text, filename) {
  const source = parseYamlDocument(text, filename).toJS({ maxAliasCount: 100 });
  assert(source && typeof source === 'object' && !Array.isArray(source), `${filename ?? 'YAML'}: document must be a mapping`);
  checkFields(source, ['version', 'date', 'title', 'introduction', 'groups', 'changes'], 'document');
  const version = source.version;
  assert(typeof version === 'string' && (version === 'unreleased' || version === 'planned' || fullVersion.test(version)), 'version must be unreleased, planned, or X.Y.Z');
  if (filename) assert(basename(filename).replace(/\?.*$/, '') === `${version}.yaml`, `${filename}: filename must match version ${version}.yaml`);
  assert(Array.isArray(source.changes), `${version}: changes must be an array`);
  assert(source.groups === undefined || Array.isArray(source.groups), `${version}: groups must be an array`);
  assert(!['id', 'status', 'sections'].some(key => Object.hasOwn(source, key)), `${version}: use version, optional groups, and flat changes`);
  if (version === 'unreleased' || version === 'planned') assert(source.date === undefined || source.date === null, `${version}: release date is only set for X.Y.Z releases`);
  const sections = (source.groups ?? []).map(group => {
    assert(group && typeof group === 'object' && !Array.isArray(group), `${version}: group must be a mapping`);
    checkFields(group, ['id', 'title', 'introduction'], 'group');
    return { ...group, title: group.title ?? '', entries: [] };
  });
  const byId = new Map(sections.map(section => [section.id, section]));
  for (const value of source.changes) {
    assert(value && typeof value === 'object' && !Array.isArray(value), `${version}: change must be a mapping`);
    checkFields(value, ['group', 'id', 'title', 'type', 'impact', 'backends', 'details', 'action', 'migration', 'feature', 'stage', 'featureTitle', 'availability', 'firstCandidate', 'targetVersion', 'archived'], `change ${value.id ?? ''}`);
    if (value.firstCandidate) checkFields(value.firstCandidate, ['version', 'date'], 'firstCandidate');
    const { group, ...entry } = value;
    let section;
    if (group !== undefined) {
      section = byId.get(group);
      assert(section, `${entry.id ?? version}: unknown group ${group}`);
    } else {
      section = byId.get('changes');
      if (!section) {
        section = { id: 'changes', title: '', entries: [] };
        sections.unshift(section);
        byId.set(section.id, section);
      }
    }
    const type = entry.type === undefined ? 'change' : entry.type;
    section.entries.push({ ...entry, type, impact: entry.impact === undefined ? (type === 'deprecation' ? 'action-required' : 'none') : entry.impact, backends: entry.backends === undefined ? ['all'] : entry.backends, details: entry.details === undefined ? '' : entry.details });
  }
  const status = version === 'unreleased' ? 'upcoming' : version === 'planned' ? 'planned' : 'released';
  return validateChangelogDocument({
    id: version, status, version: status === 'released' ? version : null,
    date: status === 'released' ? source.date : null,
    title: source.title ?? (version === 'unreleased' ? 'Unreleased' : version === 'planned' ? 'Planned changes' : version),
    introduction: source.introduction ?? '', sections,
  });
}

export function parseChangelogYaml(text, filename) {
  try { return normalizeChangelogYaml(text, filename); }
  catch (error) {
    if (filename) throw new Error(`${filename}: ${error.message}`, { cause: error });
    throw error;
  }
}

function literalStrings(document) {
  visit(document, {
    Pair(_, pair) {
      if (isScalar(pair.value) && typeof pair.value.value === 'string' && (pair.value.value.includes('\n') || ['details', 'migration', 'introduction', 'availability', 'action'].includes(pair.key?.value))) pair.value.type = Scalar.BLOCK_LITERAL;
      if (isScalar(pair.key) && pair.key.value === 'backends' && isSeq(pair.value)) pair.value.flow = true;
    },
  });
  return document.toString({ lineWidth: 0 });
}

export function stringifyChangelogDocument(input) {
  const document = validateChangelogDocument(input);
  const version = document.status === 'released' ? document.version : document.status === 'upcoming' ? 'unreleased' : 'planned';
  const title = version === 'unreleased' ? 'Unreleased' : version === 'planned' ? 'Planned changes' : version;
  const groups = document.sections.filter((section, index) => index !== 0 || section.id !== 'changes' || section.title || section.introduction).map(({ entries, ...group }) => group);
  const source = {
    version,
    ...(document.status === 'released' ? { date: document.date } : {}),
    ...(document.title !== title ? { title: document.title } : {}),
    ...(document.introduction ? { introduction: document.introduction } : {}),
    ...(groups.length ? { groups } : {}),
    changes: document.sections.flatMap(section => section.entries.map(entry => {
      const { id, title, type, impact, backends, details, ...other } = entry;
      return {
        id, title,
        ...(section.id !== 'changes' ? { group: section.id } : {}),
        ...other,
        ...(type !== 'change' ? { type } : {}),
        ...(impact !== (type === 'deprecation' ? 'action-required' : 'none') ? { impact } : {}),
        ...(backends.length !== 1 || backends[0] !== 'all' ? { backends } : {}),
        ...(details ? { details } : {}),
      };
    })),
  };
  return literalStrings(new Document(source));
}

export async function loadChangelog({ dataDir = CHANGELOG_DATA_DIR } = {}) {
  const files = (await readdir(dataDir)).filter(file => file.endsWith('.yaml')).sort();
  return validateChangelog(await Promise.all(files.map(async file => parseChangelogYaml(await readFile(join(dataDir, file), 'utf8'), file))));
}

export function validateChangelog(input) {
  assert(Array.isArray(input), 'documents must be an array');
  const documents = input.map(validateChangelogDocument);
  const ids = new Set();
  const entryIds = new Set();
  for (const document of documents) {
    assert(!ids.has(document.id), `duplicate document ${document.id}`);
    ids.add(document.id);
    for (const section of document.sections) for (const entry of section.entries) {
      assert(!entryIds.has(entry.id), `duplicate entry ${entry.id}`);
      entryIds.add(entry.id);
    }
  }
  return documents.sort((a, b) => {
    const rank = { upcoming: 0, released: 1, planned: 2 };
    return rank[a.status] - rank[b.status] || (b.version ?? '').localeCompare(a.version ?? '', undefined, { numeric: true });
  });
}

export function renderChangelogMarkdown(document, { includeTitle = true } = {}) {
  const parts = [];
  if (includeTitle) parts.push(`# ${document.title}`);
  if (document.status === 'released') parts.push(`Released ${document.date} · [GitHub release](https://github.com/rise-deploy/rise/releases/tag/v${document.version})`);
  if (document.introduction) parts.push(document.introduction);
  for (const section of document.sections) {
    if (section.title) parts.push(`## ${section.title}`);
    if (section.introduction) parts.push(section.introduction);
    for (const entry of section.entries) {
      parts.push(`### ${entry.title}`);
      parts.push(`**${impactLabels[entry.impact]}** · ${entry.backends.map(backend => ({ all: 'All backends', kubernetes: 'Kubernetes', docker: 'Docker', ecs: 'Amazon ECS' })[backend]).join(', ')}`);
      if (entry.details) parts.push(entry.details);
      if (entry.action) parts.push(`**Before you upgrade:** ${entry.action}`);
      if (entry.migration) parts.push('#### Migration', entry.migration);
    }
  }
  return `${parts.join('\n\n')}\n`;
}

export async function changelogMarkdownForSlug(slug, options = {}) {
  if (slug !== 'changelog' && slug !== 'changelog/deprecations' && !/^changelog\/v\d+-\d+-\d+$/.test(slug)) return null;
  const documents = await loadChangelog(options);
  if (slug === 'changelog/deprecations') return renderDeprecationsMarkdown(deriveDeprecations(documents), { documents });
  if (slug === 'changelog') return ['# Changelog', 'Release notes and upgrade instructions. Upcoming changes are merged; planned changes are not yet merged.', ...documents.map(document => renderChangelogMarkdown(document))].join('\n\n');
  const version = slug.slice('changelog/v'.length).replaceAll('-', '.');
  const document = documents.find(document => document.version === version);
  return document ? renderChangelogMarkdown(document) : null;
}

export function releasePage(version, date) {
  const [major, minor, patch] = version.split('.').map(Number);
  const order = 1_000_000_000 - major * 1_000_000 - minor * 1_000 - patch;
  return `---\ntitle: "${version}"\ndescription: Rise ${version}, released ${date}.\ntableOfContents: false\nsidebar:\n  order: ${order}\n---\n\nimport Changelog from '../../../components/changelog/Changelog.astro';\n\n<Changelog releaseId="${version}" />\n`;
}

export async function publishChangelogRelease(version, notes, date, { dataDir = CHANGELOG_DATA_DIR, pagesDir = CHANGELOG_PAGES_DIR } = {}) {
  assert(fullVersion.test(version), 'full release version must be X.Y.Z');
  const documents = await loadChangelog({ dataDir });
  assert(!documents.some(document => document.id === version), `release ${version} already exists`);
  const upcoming = documents.find(document => document.id === 'unreleased' && document.status === 'upcoming');
  assert(upcoming, 'unreleased document is required');
  assert(validDate(date), `${version}: invalid release date`);
  const cleanedNotes = notes.replace(/^\s*#+\s+release notes\s*\n/i, '').split(/^## Download /m)[0].trim().replace(/(?:\s*---)+$/, '').trim();
  const upcomingPath = join(dataDir, 'unreleased.yaml');
  const original = await readFile(upcomingPath, 'utf8');
  const releaseYaml = parseYamlDocument(original, 'unreleased.yaml');
  releaseYaml.set('version', version);
  releaseYaml.set('date', date);
  releaseYaml.delete('title');
  if (cleanedNotes) releaseYaml.set('introduction', cleanedNotes); else releaseYaml.delete('introduction');
  for (const change of releaseYaml.get('changes').items) if (isMap(change)) change.delete('targetVersion');
  const published = literalStrings(releaseYaml);
  const document = parseChangelogYaml(published, `${version}.yaml`);
  const nextUpcoming = parseYamlDocument(original, 'unreleased.yaml');
  nextUpcoming.set('changes', []);
  nextUpcoming.delete('groups');
  const next = literalStrings(nextUpcoming);
  const dataPath = join(dataDir, `${version}.yaml`);
  const stem = `v${version.replaceAll('.', '-')}`;
  const pagePath = join(pagesDir, `${stem}.mdx`);
  await mkdir(pagesDir, { recursive: true });
  for (const path of [dataPath, pagePath, join(pagesDir, `${stem}.md`)]) {
    try { await access(path); throw new Error(`Changelog destination already exists: ${path}`); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
  const temporary = `${upcomingPath}.${randomUUID()}.tmp`;
  const created = [];
  try {
    await writeFile(dataPath, published, { flag: 'wx' });
    created.push(dataPath);
    await writeFile(pagePath, releasePage(version, date), { flag: 'wx' });
    created.push(pagePath);
    await writeFile(temporary, next, { flag: 'wx' });
    await rename(temporary, upcomingPath);
  } catch (error) {
    await Promise.all(created.map(path => rm(path, { force: true })));
    throw error;
  } finally {
    await rm(temporary, { force: true });
  }
  return document;
}
