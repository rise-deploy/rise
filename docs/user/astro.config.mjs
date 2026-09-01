import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import license from 'rollup-plugin-license';

const base = process.env.ASTRO_BASE ?? '/docs';

// Third-party attribution for the JavaScript this site ships. Inert unless
// RISE_NOTICES_OUT is set, so `npm run build`, `mise docs:build` and the Docker
// image build are unaffected -- only `mise notices:generate` turns it on.
//
// Note this reports only what Vite bundles. Pagefind's search runtime is
// written into dist/ by a separate binary after the build and is attributed
// through scripts/notices/npm-supplemental.json instead.
const noticesOut = process.env.RISE_NOTICES_OUT;

export default defineConfig({
  base,
  vite: noticesOut
    ? {
        plugins: [
          {
            ...license({
              thirdParty: {
                includePrivate: false,
                output: {
                  file: `${noticesOut}/docs-js.json`,
                  template: (deps) =>
                    JSON.stringify(
                      deps
                        .map((d) => ({
                          name: d.name,
                          version: d.version,
                          license: d.license ?? null,
                          repository:
                            typeof d.repository === 'string'
                              ? d.repository
                              : (d.repository?.url ?? null),
                          licenseText: d.licenseText ?? null,
                          via: 'js',
                        }))
                        // Producer order follows module traversal and is not
                        // guaranteed stable; the notices file is committed.
                        .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)),
                      null,
                      2,
                    ),
                },
              },
            }),
            // Astro runs two Vite builds against the same plugin instance.
            // `apply: 'build'` keeps it out of dev; `enforce: 'post'` runs it
            // after Astro's own transforms so the module graph is final.
            apply: 'build',
            enforce: 'post',
          },
        ],
      }
    : {},
  integrations: [
    starlight({
      title: 'Rise User Docs',
      favicon: '/favicon.ico',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/NiklasRosenstein/rise',
        },
      ],
      components: {
        Header: './src/components/Header.astro',
      },
      sidebar: [
        {
          label: 'Start Here',
          items: [
            { label: 'Overview', slug: 'index' },
            { label: 'Getting Started', slug: 'user-guide/getting-started' },
            { label: 'Project Configuration', slug: 'user-guide/configuration' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Using Rise with AI Agents', slug: 'guides/ai-app-builder' },
            { label: 'Deploying from CI', slug: 'guides/deploying-from-ci' },
            { label: 'Authenticating End Users', slug: 'guides/authenticating-end-users' },
            { label: 'Choosing a Build Backend', slug: 'guides/choosing-a-build-backend' },
          ],
        },
        {
          label: 'Deploy Applications',
          items: [
            { label: 'Deployments', slug: 'user-guide/deployments' },
            { label: 'Environments', slug: 'user-guide/environments' },
            { label: 'CI/CD Setup', slug: 'user-guide/ci-cd' },
            {
              label: 'Building Images',
              items: [
                { label: 'Overview', slug: 'user-guide/builds' },
                { label: 'Docker', slug: 'user-guide/builds/docker' },
                { label: 'Pack (Buildpacks)', slug: 'user-guide/builds/pack' },
                { label: 'Railpack', slug: 'user-guide/builds/railpack' },
              ],
            },
            { label: 'Environment Variables', slug: 'user-guide/environment-variables' },
            { label: 'Custom Domains', slug: 'user-guide/custom-domains' },
            { label: 'Local Development', slug: 'user-guide/local-development' },
            { label: 'Docker Runtime', slug: 'user-guide/docker-runtime' },
          ],
        },
        {
          label: 'Access',
          items: [
            { label: 'Authentication', slug: 'user-guide/authentication' },
            { label: 'Service Accounts', slug: 'user-guide/service-accounts' },
            { label: 'Workload Identity Tokens', slug: 'user-guide/workload-identity-tokens' },
            {
              label: 'Authentication for Applications',
              items: [
                { label: 'Overview', slug: 'user-guide/authentication-for-apps' },
                { label: 'rise_jwt Cookie', slug: 'user-guide/authentication-for-apps/rise-jwt-cookie' },
                { label: 'Validating JWTs', slug: 'user-guide/authentication-for-apps/validating-jwts' },
                { label: 'Example Code', slug: 'user-guide/authentication-for-apps/examples' },
              ],
            },
            { label: 'OAuth Extensions', slug: 'user-guide/oauth' },
            { label: 'SSL & Proxy Configuration', slug: 'user-guide/ssl-proxy' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI Reference', slug: 'user-guide/cli-reference' },
            { label: 'Troubleshooting', slug: 'user-guide/troubleshooting' },
          ],
        },
        {
          label: 'Project Extensions',
          items: [
            { label: 'Overview', slug: 'extensions' },
            { label: 'AWS RDS Provisioner', slug: 'extensions/aws-rds-provisioner' },
            { label: 'AWS S3 Bucket', slug: 'extensions/aws-s3-bucket' },
            { label: 'OAuth Provider', slug: 'extensions/oauth' },
            { label: 'Snowflake OAuth Provisioner', slug: 'extensions/snowflake-oauth-provisioner' },
          ],
        },
      ],
    }),
  ],
});
