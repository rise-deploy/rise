import { existsSync } from 'node:fs';
import { dirname, relative, resolve, sep } from 'node:path';

const sitePrefixes = { user: '/docs', engineering: '/operator-docs' };

/** Resolve links to the two documentation sites under their publishing prefixes. */
export function resolveDocLink(href, { site, base, switchUrl } = {}) {
  if (typeof href !== 'string') return href;
  const published = /^https:\/\/rise-deploy\.github\.io\/rise\/(user|operator)(?=\/|[?#]|$)/.exec(href);
  if (published) {
    const own = published[1] === (site === 'user' ? 'user' : 'operator');
    const target = own ? base : switchUrl;
    if (target === undefined || target === '') return href;
    const suffix = href.slice(published[0].length);
    return `${String(target).replace(/\/+$/, '')}${suffix.startsWith('/') ? suffix : `/${suffix}`}`;
  }
  const match = /^(\/operator-docs|\/docs)(?=\/|[?#]|$)/.exec(href);
  if (!match) return href;
  const ownPrefix = sitePrefixes[site];
  const target = match[1] === ownPrefix ? base : switchUrl;
  if (target === undefined || target === '') return href;
  const prefix = String(target).replace(/\/+$/, '');
  const suffix = href.slice(match[1].length);
  return `${prefix}${suffix.startsWith('/') ? suffix : `/${suffix}`}`;
}

function localDocLink(href, sourcePath, options) {
  const published = resolveDocLink(href, options);
  if (published !== href || !sourcePath || /^(?:[a-z][a-z\d+.-]*:|[\/#?])/i.test(href)) {
    return published;
  }
  const marker = `${sep}src${sep}content${sep}docs${sep}`;
  const rootEnd = sourcePath.indexOf(marker);
  if (rootEnd < 0) return published;
  const contentRoot = sourcePath.slice(0, rootEnd + marker.length);
  const [, pathname, suffix = ''] = /^([^?#]*)(.*)$/.exec(href);
  let decoded;
  try { decoded = decodeURIComponent(pathname); } catch { return published; }
  const target = resolve(dirname(sourcePath), decoded);
  const rootRelative = relative(contentRoot, target);
  if (rootRelative === '..' || rootRelative.startsWith(`..${sep}`)) return published;
  const candidates = /\.mdx?$/.test(target)
    ? [target]
    : [`${target}.md`, `${target}.mdx`, resolve(target, 'index.md'), resolve(target, 'index.mdx')];
  const document = candidates.find((candidate) => existsSync(candidate));
  if (!document) return published;
  const slug = relative(contentRoot, document).split(sep).join('/')
    .replace(/\.mdx?$/, '').replace(/(?:^|\/)index$/, '');
  const prefix = String(options.base ?? '').replace(/\/+$/, '');
  return `${prefix}/${slug ? `${slug}/` : ''}${suffix}`;
}

/** Markdown links resolve from their source document, independent of deployment base. */
export default function remarkDocLinks(options) {
  return (tree, file) => {
    function visit(node) {
      if ((node.type === 'link' || node.type === 'definition') && typeof node.url === 'string') {
        node.url = localDocLink(node.url, file.path, options);
      }
      if (node.type === 'mdxJsxFlowElement' || node.type === 'mdxJsxTextElement') {
        for (const attribute of node.attributes ?? []) {
          if (attribute.name === 'href' && typeof attribute.value === 'string') {
            attribute.value = localDocLink(attribute.value, file.path, options);
          }
        }
      }
      for (const child of node.children ?? []) visit(child);
    }
    visit(tree);
  };
}
