import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  integrations: [
    starlight({
      title: 'Rise Engineering Docs',
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
          label: 'Operations',
          items: [
            { label: 'Overview', slug: 'index' },
            { label: 'Operator Guide', slug: 'operator-guide' },
            { label: 'Configuration', slug: 'configuration' },
            { label: 'Registry Operations', slug: 'operator-registry-operations' },
            { label: 'Kubernetes', slug: 'kubernetes' },
            { label: 'Production Deployment', slug: 'production' },
            { label: 'Database', slug: 'database' },
            { label: 'PostgreSQL Upgrades', slug: 'upgrading-postgresql' },
          ],
        },
        {
          label: 'Development',
          items: [
            { label: 'Developer Guide', slug: 'developer-guide' },
            { label: 'Local Development', slug: 'development' },
            { label: 'Testing', slug: 'testing' },
          ],
        },
        {
          label: 'Extensions',
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
