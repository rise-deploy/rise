import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { createSectionMiddleware } from '../../shared/section-middleware';
import { parseChangelogYaml, validateChangelog } from '../../scripts/changelog-data.mjs';
import { deriveDeprecations, groupDeprecations } from '../../scripts/deprecations-data.mjs';

const sectionMiddleware = createSectionMiddleware('engineering');
const data = import.meta.glob<string>('./data/changelog/*.yaml', { eager: true, query: '?raw', import: 'default' });
const records = deriveDeprecations(validateChangelog(Object.entries(data).map(([filename, source]) => parseChangelogYaml(source, filename))));

export const onRequest = defineRouteMiddleware(async (context, next) => {
  await sectionMiddleware(context, next);
  const route = context.locals.starlightRoute;
  if (route.entry.id !== 'changelog/deprecations') return;

  // Component-rendered headings need explicit route metadata for the native ToC.
  const groups = groupDeprecations(records);
  const headings = groups.flatMap(group => [
    { depth: 2, slug: group.id, text: group.title },
    ...group.records.map(record => ({ depth: 3, slug: record.id, text: record.title })),
  ]);
  route.headings.push(...headings);
  route.toc?.items.push(...groups.map(group => ({
    depth: 2, slug: group.id, text: group.title,
    children: group.records.map(record => ({ depth: 3, slug: record.id, text: record.title, children: [] })),
  })));
});
