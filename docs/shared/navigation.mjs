/** @typedef {'user' | 'engineering'} DocsSite */

export const RESOURCE_API_STATUS = 'The generic Resource API is a work in progress and is not yet in general production use. It is the target model for evolving the typed API.';

/** @typedef {{ label?: string, slug?: string, items?: SidebarItem[], autogenerate?: { directory: string } }} SidebarItem */
/** @typedef {{ id: string, label: string, slug: string, items: SidebarItem[], prefixes?: string[], wip?: boolean }} DocsSection */

/** @param {string} label @param {string} slug @returns {SidebarItem} */
const page = (label, slug) => ({ label, slug });
/** @param {string} label @param {SidebarItem[]} items @returns {SidebarItem} */
const group = (label, items) => ({ label, items });

/** @type {Record<DocsSite, DocsSection[]>} */
const sections = {
  user: [
    {
      id: 'start', label: 'Get started', slug: 'index',
      items: [group('Introduction', [
        page('Overview', 'index'),
        page('Your first deployment', 'user-guide/getting-started'),
        page('Sign in to Rise', 'user-guide/authentication'),
      ])],
    },
    {
      id: 'guides', label: 'Guides', slug: 'guides',
      items: [
        group('Introduction', [page('Overview', 'guides')]),
        group('Workflows', [
          page('Using Rise with AI agents', 'guides/ai-app-builder'),
          page('Deploying from CI', 'guides/deploying-from-ci'),
          page('Authenticating end users', 'guides/authenticating-end-users'),
          page('Choosing a build backend', 'guides/choosing-a-build-backend'),
        ]),
        group('Build and deploy', [
          page('Deployments', 'user-guide/deployments'),
          page('Environments', 'user-guide/environments'),
          page('CI/CD setup', 'user-guide/ci-cd'),
          group('Building images', [
            page('Overview', 'user-guide/builds'),
            page('Docker', 'user-guide/builds/docker'),
            page('Pack buildpacks', 'user-guide/builds/pack'),
            page('Railpack', 'user-guide/builds/railpack'),
          ]),
          page('Environment variables', 'user-guide/environment-variables'),
          page('Custom domains', 'user-guide/custom-domains'),
          page('Local development', 'user-guide/local-development'),
        ]),
        group('Identity and access', [
          page('Service accounts', 'user-guide/service-accounts'),
          page('Workload identity tokens', 'user-guide/workload-identity-tokens'),
          group('Application authentication', [
            page('Overview', 'user-guide/authentication-for-apps'),
            page('rise_jwt cookie', 'user-guide/authentication-for-apps/rise-jwt-cookie'),
            page('Validating JWTs', 'user-guide/authentication-for-apps/validating-jwts'),
            page('Example code', 'user-guide/authentication-for-apps/examples'),
          ]),
          page('OAuth configuration', 'user-guide/oauth'),
        ]),
      ],
    },
    {
      id: 'reference', label: 'Reference', slug: 'reference',
      items: [
        group('Introduction', [page('Overview', 'reference')]),
        group('CLI and configuration', [
          page('CLI reference', 'user-guide/cli-reference'),
          page('Project configuration', 'user-guide/configuration'),
        ]),
        group('Runtime', [
          page('Docker runtime', 'user-guide/docker-runtime'),
          page('SSL and proxy configuration', 'user-guide/ssl-proxy'),
          page('Troubleshooting', 'user-guide/troubleshooting'),
        ]),
      ],
    },
    {
      id: 'extensions', label: 'Extensions', slug: 'extensions', prefixes: ['extensions'],
      items: [
        group('Introduction', [page('Overview', 'extensions')]),
        group('Data and storage', [
          page('AWS RDS', 'extensions/aws-rds-provisioner'),
          page('AWS S3 buckets', 'extensions/aws-s3-bucket'),
        ]),
        group('Authentication', [
          page('OAuth providers', 'extensions/oauth'),
          page('Snowflake OAuth', 'extensions/snowflake-oauth-provisioner'),
        ]),
      ],
    },
  ],
  engineering: [
    {
      id: 'operate', label: 'Operate', slug: 'index',
      items: [
        group('Introduction', [
          page('Overview', 'index'),
          page('Operator guide', 'operator-guide'),
          page('Production deployment', 'production'),
        ]),
        group('Platform configuration', [
          page('Configuration', 'configuration'),
          page('Authentication and tokens', 'authentication'),
          group('Container registries', [
            page('Overview', 'operator-registry-operations'),
            page('AWS ECR', 'operator-registry-operations/aws-ecr'),
            page('GitLab Container Registry', 'operator-registry-operations/gitlab'),
            page('JFrog Artifactory', 'operator-registry-operations/jfrog'),
            page('Docker/OCI registry', 'operator-registry-operations/docker-oci'),
          ]),
        ]),
        group('Observability', [
          page('Persistent logs', 'persistent-logs'),
          page('Loki backend', 'persistent-logs/loki'),
        ]),
        group('Maintenance', [
          page('Database', 'database'),
          page('PostgreSQL upgrades', 'upgrading-postgresql'),
        ]),
      ],
    },
    {
      id: 'backends', label: 'Deployment backends', slug: 'deployment-backends',
      prefixes: ['kubernetes', 'docker', 'ecs'],
      items: [
        group('Introduction', [page('Overview and feature matrix', 'deployment-backends')]),
        group('Kubernetes', [
          page('Overview', 'kubernetes'),
          page('Configuration', 'kubernetes/configuration'),
          page('Architecture', 'kubernetes/architecture'),
          page('Private project authentication', 'kubernetes/private-auth'),
          page('Custom domains and TLS', 'kubernetes/custom-domains'),
          page('Resources reference', 'kubernetes/resources'),
          page('Operations and security', 'kubernetes/operations'),
        ]),
        group('Docker', [
          page('Overview', 'docker'),
          page('Quick start', 'docker/quick-start'),
          page('Architecture', 'docker/architecture'),
          page('Production deployment', 'docker/production'),
          page('Authentication and ingress', 'docker/authentication'),
          page('Container registry', 'docker/registry'),
          page('Troubleshooting', 'docker/troubleshooting'),
        ]),
        group('Amazon ECS', [
          page('Overview', 'ecs'),
          page('Terraform', 'ecs/terraform'),
        ]),
      ],
    },
    {
      id: 'extend', label: 'Extend', slug: 'extensions', prefixes: ['extensions'],
      items: [
        group('Introduction', [page('Overview', 'extensions')]),
        group('Data and storage', [
          page('AWS RDS provider', 'extensions/aws-rds-provisioner'),
          page('AWS S3 provider', 'extensions/aws-s3-bucket'),
        ]),
        group('Authentication', [
          page('OAuth providers', 'extensions/oauth'),
          page('Snowflake OAuth provider', 'extensions/snowflake-oauth-provisioner'),
        ]),
      ],
    },
    {
      id: 'resources', label: 'Resource API', slug: 'resources', wip: true,
      prefixes: ['resources'],
      items: [
        group('Introduction', [page('Overview', 'resources')]),
        group('Resource model', [
          page('Storage model', 'resources/storage'),
          page('Custom resources', 'resources/custom-resources'),
        ]),
        group('Reference', [
          page('HTTP API', 'resources/api'),
          page('JSON schemas', 'resources/schemas'),
        ]),
      ],
    },
    {
      id: 'contribute', label: 'Contribute', slug: 'contribute', prefixes: ['adr'],
      items: [
        group('Introduction', [page('Overview', 'contribute'), page('Developer guide', 'developer-guide')]),
        group('Development', [page('Local development', 'development'), page('Testing', 'testing')]),
        group('Architecture decisions', [{ autogenerate: { directory: 'adr' } }]),
      ],
    },
    {
      id: 'changelog', label: 'Changelog', slug: 'changelog', prefixes: ['changelog'],
      items: [group('Release notes', [{ autogenerate: { directory: 'changelog' } }])],
    },
  ],
};

/** @param {DocsSite} site @returns {DocsSection[]} */
export function sectionsFor(site) {
  const result = sections[site];
  if (!result) throw new Error(`Unknown documentation site: ${site}`);
  return result;
}

/** @param {SidebarItem[]} items @returns {string[]} */
function slugsIn(items) {
  return items.flatMap((item) => item.slug ? [item.slug] : item.items ? slugsIn(item.items) : []);
}

/** @param {string} path */
function cleanPath(path) {
  return path.split(/[?#]/, 1)[0].replace(/^\/+|\/+$/g, '');
}

/** @param {string} base @param {string} slug */
export function hrefFor(base, slug) {
  return '/' + [cleanPath(base), slug === 'index' ? '' : cleanPath(slug)].filter(Boolean).join('/') + (cleanPath(base) || (slug && slug !== 'index') ? '/' : '');
}

/** @param {DocsSite} site @param {string} pathname @param {string} [base] @returns {DocsSection} */
export function getSection(site, pathname, base = '') {
  const prefix = cleanPath(base);
  let slug = cleanPath(pathname);
  if (prefix && (slug === prefix || slug.startsWith(prefix + '/'))) {
    slug = slug.slice(prefix.length).replace(/^\/+/, '');
  }
  if (!slug) slug = 'index';
  const candidates = sectionsFor(site);
  return candidates.find((section) => slugsIn(section.items).includes(slug))
    ?? candidates.find((section) => section.prefixes?.some((p) => slug === p || slug.startsWith(p + '/')))
    ?? candidates[0];
}

/** @param {DocsSite} site */
export function sidebarFor(site) {
  return sectionsFor(site).map((section) => ({
    label: section.label,
    ...(section.wip ? { badge: { text: 'WIP', variant: 'caution' } } : {}),
    items: section.items,
  }));
}

/**
 * @template {{ href: string }} T
 * @param {DocsSite} site
 * @param {string} pathname
 * @param {string} base
 * @param {{ prev?: T, next?: T }} pagination
 * @returns {{ prev: T | undefined, next: T | undefined }}
 */
export function paginationForSection(site, pathname, base, pagination) {
  const current = getSection(site, pathname, base);
  const scoped = (link) => link && getSection(site, link.href, base).id === current.id ? link : undefined;
  return { prev: scoped(pagination.prev), next: scoped(pagination.next) };
}
