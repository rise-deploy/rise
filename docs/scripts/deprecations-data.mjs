export const lifecycleLabels = {
  introduced: 'Introduced',
  deprecated: 'Deprecated',
  'default-disabled': 'Disabled by default',
  removed: 'Removed',
};

const stages = Object.keys(lifecycleLabels);
const implemented = (document) => document.status === 'upcoming' || document.status === 'released';
const text = (value) => typeof value === 'string' && value.trim().length > 0;
const fullVersion = /^\d+\.\d+\.\d+$/;

function assert(condition, message) {
  if (!condition) throw new Error(`Invalid deprecation data: ${message}`);
}

function validDate(date) {
  const timestamp = Date.parse(`${date}T00:00:00Z`);
  return /^\d{4}-\d{2}-\d{2}$/.test(date) && Number.isFinite(timestamp) && new Date(timestamp).toISOString().startsWith(date);
}

function eventFor(entry, document) {
  assert(['upcoming', 'released', 'planned'].includes(document.status), `${entry.id}: unknown document status`);
  let version = null;
  let date = null;
  if (document.status === 'released') {
    assert(fullVersion.test(document.version) && validDate(document.date), `${entry.id}: released milestones need a document version and release date`);
    version = document.version;
    date = document.date;
  } else if (document.status === 'upcoming') {
    version = 'unreleased';
  } else if (entry.targetVersion !== undefined && entry.targetVersion !== null) {
    assert(fullVersion.test(entry.targetVersion), `${entry.id}: planned targetVersion must be X.Y.Z`);
    version = entry.targetVersion;
  }
  const event = { kind: entry.stage, state: document.status, version, date, changeId: entry.id };
  if (entry.firstCandidate) {
    assert(/^\d+\.\d+\.\d+-[0-9A-Za-z.-]+$/.test(entry.firstCandidate.version) && validDate(entry.firstCandidate.date), `${entry.id}: invalid first candidate evidence`);
    event.firstCandidate = { ...entry.firstCandidate };
  }
  return event;
}

function compareImplemented(a, b) {
  const upcoming = Number(a.document.status === 'upcoming') - Number(b.document.status === 'upcoming');
  if (upcoming) return upcoming;
  const byDate = (a.document.date ?? '').localeCompare(b.document.date ?? '');
  if (byDate) return byDate;
  const byVersion = (a.document.version ?? '').localeCompare(b.document.version ?? '', undefined, { numeric: true });
  return byVersion || stages.indexOf(a.entry.stage) - stages.indexOf(b.entry.stage);
}

/** Project feature lifecycles from the entries that announce their milestones. */
export function deriveDeprecations(documents) {
  const features = new Map();
  for (const document of documents) {
    for (const section of document.sections) {
      for (const entry of section.entries) {
        if (entry.type !== 'deprecation') continue;
        assert(text(entry.feature) && /^[a-z0-9][a-z0-9-]*$/.test(entry.feature), `${entry.id}: feature needs a stable identifier`);
        assert(stages.includes(entry.stage), `${entry.id}: unknown lifecycle stage ${entry.stage}`);
        const milestones = features.get(entry.feature) ?? new Map();
        assert(!milestones.has(entry.stage), `${entry.feature}: duplicate ${entry.stage} milestone`);
        milestones.set(entry.stage, { entry, document, event: eventFor(entry, document) });
        features.set(entry.feature, milestones);
      }
    }
  }

  const records = [];
  for (const [id, milestones] of features) {
    const announcement = milestones.get('deprecated');
    if (!announcement || !implemented(announcement.document)) continue;
    assert(text(announcement.entry.migration), `${id}: implemented deprecation needs migration instructions`);
    const current = [...milestones.values()].filter(({ document }) => implemented(document)).sort(compareImplemented);
    const latest = (field) => current.findLast(({ entry }) => text(entry[field]))?.entry[field];
    const last = current.at(-1).entry;
    const events = stages.flatMap((kind) => {
      const milestone = milestones.get(kind);
      if (milestone) return [milestone.event];
      if (kind === 'introduced') return [{ kind, state: 'unknown', version: null, date: null }];
      if (kind === 'removed') return [{ kind, state: 'unscheduled', version: null, date: null }];
      return [];
    });
    records.push({
      id,
      title: latest('featureTitle') ?? announcement.entry.title,
      availability: text(last.availability) ? last.availability : last.details ?? '',
      migration: latest('migration'),
      events,
      ...(milestones.get('removed')?.document.status === 'released' && milestones.get('removed').entry.archived === true ? { archived: true } : {}),
    });
  }
  return records.sort((a, b) => a.id.localeCompare(b.id));
}

export function deprecationStatus(record) {
  return record.events.some(event => event.kind === 'removed' && ['upcoming', 'released'].includes(event.state)) ? 'removed' : 'deprecated';
}

/** Group the current register; archived removal notices remain in release history. */
export function groupDeprecations(records) {
  return [
    { id: 'deprecated', title: 'Deprecated', description: 'These features still work under the conditions documented below. Follow the migration instructions before their planned default changes or removal.', records: records.filter(record => deprecationStatus(record) === 'deprecated') },
    { id: 'removed', title: 'Removed', description: 'These features are unavailable in current releases. Migration instructions cover upgrades from affected versions.', records: records.filter(record => deprecationStatus(record) === 'removed' && !record.archived) },
  ];
}

export function lifecycleVersionLabel(event) {
  if (event.state === 'unknown') return 'Not established';
  if (event.state === 'unscheduled') return 'Not scheduled';
  if (event.state === 'upcoming') return 'Unreleased';
  if (event.state === 'planned') return event.version ? `Planned for ${event.version}` : 'Planned · version not announced';
  return event.version;
}

export function changelogChangeLink(changeId, documents) {
  if (!changeId) return null;
  const document = documents.find(document => document.sections.some(section => section.entries.some(entry => entry.id === changeId)));
  if (!document) throw new Error(`Deprecation references unknown changelog entry: ${changeId}`);
  const page = document.status === 'released' ? `v${document.version.replaceAll('.', '-')}/` : '';
  return `/operator-docs/changelog/${page}#change-${changeId}`;
}

export function renderDeprecationsMarkdown(records, { documents = [] } = {}) {
  const parts = ['# Deprecated and removed features', 'A planned default change and a feature\'s removal are separate milestones. Planned milestones are not release guarantees.'];
  for (const group of groupDeprecations(records)) {
    parts.push(`## ${group.title}`, group.description);
    if (group.records.length === 0) parts.push(group.id === 'deprecated' ? 'No implemented deprecations are recorded.' : 'No removal notices are listed.');
    for (const record of group.records) {
      parts.push(`### ${record.title}`, `**${deprecationStatus(record) === 'removed' ? 'Removed' : 'Deprecated · still available'}**`, record.availability);
      parts.push('| Milestone | Version | Release date |\n|---|---|---|\n' + record.events.map(event => `| ${lifecycleLabels[event.kind]} | ${lifecycleVersionLabel(event)} | ${event.date ?? 'Not announced / not established'} |`).join('\n'));
      for (const event of record.events) {
        if (event.firstCandidate) parts.push(`${lifecycleLabels[event.kind]} first appears in candidate **${event.firstCandidate.version}**, published **${event.firstCandidate.date}**. The candidate date is separate from the full-release date.`);
        const link = changelogChangeLink(event.changeId, documents);
        if (link) parts.push(`[${lifecycleLabels[event.kind]} release note](${link})`);
      }
      parts.push('#### Migration', record.migration);
    }
  }
  return `${parts.join('\n\n')}\n`;
}
