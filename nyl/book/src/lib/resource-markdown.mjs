import { createMarkdownProcessor } from '@astrojs/markdown-remark';
import { resources } from './resources.mjs';

/** Link unambiguous resource names in prose, preserving authored links, headings, and code. */
function remarkResourceLinks({ base }) {
  const routes = new Map();
  for (const resource of resources) {
    routes.set(resource.name, routes.has(resource.name) ? null : `${base}${resource.route}`);
  }
  const names = [...routes.keys()].filter((name) => routes.get(name));
  const pattern = new RegExp(`\\b(${names.map((name) => name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})(s)?\\b`, 'g');
  function walk(node) {
    if (!node.children || ['link', 'linkReference', 'heading'].includes(node.type)) return;
    node.children = node.children.flatMap((child) => {
      if (child.type !== 'text') { walk(child); return [child]; }
      const children = [];
      let end = 0;
      for (const match of child.value.matchAll(pattern)) {
        if (match.index > end) children.push({ type: 'text', value: child.value.slice(end, match.index) });
        children.push({ type: 'link', url: routes.get(match[1]), children: [{ type: 'text', value: match[0] }] });
        end = match.index + match[0].length;
      }
      if (end < child.value.length) children.push({ type: 'text', value: child.value.slice(end) });
      return children;
    });
  }
  return walk;
}

/** Render schema documentation with resource links rooted at the site's deployment path. */
export function createResourceMarkdownProcessor(base) {
  return createMarkdownProcessor({ remarkPlugins: [[remarkResourceLinks, { base: base.replace(/\/$/, '') }]] });
}
