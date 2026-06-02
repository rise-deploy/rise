import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const base = process.env.ASTRO_BASE ?? '/docs';

export default defineConfig({
  base,
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
