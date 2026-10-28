import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { unified } from '@astrojs/markdown-remark';
import { sidebarFor } from '../shared/navigation.mjs';
import remarkDocLinks from '../shared/remark-doc-links.mjs';

const base = process.env.ASTRO_BASE ?? '/docs';

export default defineConfig({
  base,
  markdown: {
    processor: unified({
      remarkPlugins: [[remarkDocLinks, { site: 'user', base, switchUrl: process.env.RISE_DOCS_SWITCH_URL }]],
    }),
  },
  integrations: [
    starlight({
      title: 'Rise User Docs',
      favicon: '/favicon.ico',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/rise-deploy/rise' }],
      customCss: ['../shared/styles/docs.css'],
      routeMiddleware: './src/section-middleware.ts',
      components: {
        Header: './src/components/Header.astro',
        Sidebar: './src/components/Sidebar.astro',
        MobileMenuToggle: '../shared/components/RelocatedControls.astro',
        MobileMenuFooter: '../shared/components/RelocatedControls.astro',
      },
      sidebar: sidebarFor('user'),
    }),
  ],
});
