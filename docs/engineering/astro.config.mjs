import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const base = process.env.ASTRO_BASE ?? '/operator-docs';

export default defineConfig({
  base,
  integrations: [
    starlight({
      title: 'Rise Operator Docs',
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
          label: 'Operations',
          items: [
            { label: 'Overview', slug: 'index' },
            { label: 'Operator Guide', slug: 'operator-guide' },
            { label: 'Configuration', slug: 'configuration' },
            {
              label: 'Registry Operations',
              items: [
                { label: 'Overview', slug: 'operator-registry-operations' },
                { label: 'AWS ECR', slug: 'operator-registry-operations/aws-ecr' },
                { label: 'GitLab Container Registry', slug: 'operator-registry-operations/gitlab' },
                { label: 'JFrog Artifactory', slug: 'operator-registry-operations/jfrog' },
                { label: 'Docker/OCI Registry', slug: 'operator-registry-operations/docker-oci' },
              ],
            },
            {
              label: 'Deployment Backends',
              items: [
                { label: 'Overview & Feature Matrix', slug: 'deployment-backends' },
                {
                  label: 'Kubernetes',
                  items: [
                    { label: 'Overview', slug: 'kubernetes' },
                    { label: 'Configuration', slug: 'kubernetes/configuration' },
                    { label: 'Architecture', slug: 'kubernetes/architecture' },
                    { label: 'Private Project Auth', slug: 'kubernetes/private-auth' },
                    { label: 'Custom Domains & TLS', slug: 'kubernetes/custom-domains' },
                    { label: 'Resources Reference', slug: 'kubernetes/resources' },
                    { label: 'Operations & Security', slug: 'kubernetes/operations' },
                  ],
                },
                {
                  label: 'Docker',
                  items: [
                    { label: 'Overview', slug: 'docker' },
                  ],
                },
              ],
            },
            {
              label: 'Persistent Logs',
              items: [
                { label: 'Overview', slug: 'persistent-logs' },
                { label: 'Loki Backend', slug: 'persistent-logs/loki' },
              ],
            },
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
        {
          label: 'Resources',
          items: [
            { label: 'Overview', slug: 'resources' },
            { label: 'Storage Model', slug: 'resources/storage' },
            { label: 'HTTP API', slug: 'resources/api' },
            { label: 'Custom Resources', slug: 'resources/custom-resources' },
            { label: 'Schema Reference', slug: 'resources/schemas' },
          ],
        },
      ],
    }),
  ],
});
