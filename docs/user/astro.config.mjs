import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  base: '/docs',
  integrations: [
    starlight({
      title: 'Rise Docs',
      favicon: '/favicon.ico',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/NiklasRosenstein/rise',
        },
      ],
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
          ],
        },
        {
          label: 'Access',
          items: [
            { label: 'Authentication', slug: 'user-guide/authentication' },
            { label: 'Service Accounts', slug: 'user-guide/service-accounts' },
            { label: 'Authentication for Applications', slug: 'user-guide/authentication-for-apps' },
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
