import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { getSection, paginationForSection } from './navigation.mjs';

export function createSectionMiddleware(site: 'user' | 'engineering') {
  return defineRouteMiddleware((context) => {
    const route = context.locals.starlightRoute;
    const base = import.meta.env.BASE_URL;
    const section = getSection(site, context.url.pathname, base);
    route.sidebar = route.sidebar.filter((entry) => entry.type === 'group' && entry.label === section.label);
    route.pagination = paginationForSection(site, context.url.pathname, base, route.pagination);
  });
}
