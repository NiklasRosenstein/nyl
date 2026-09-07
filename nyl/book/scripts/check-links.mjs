import { readFileSync, readdirSync, existsSync, statSync } from 'node:fs';
import { resolve, join, relative } from 'node:path';

const root = resolve('dist');
const base = (process.env.BASE_PATH ?? '/nyl').replace(/\/$/, '');
function htmlFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? htmlFiles(path) : path.endsWith('.html') ? [path] : [];
  });
}
const pages = new Map(htmlFiles(root).map((path) => [path, readFileSync(path, 'utf8')]));
const errors = [];
for (const [path, html] of pages) {
  const pageUrl = new URL(`${base}/${relative(root, path)}`, 'https://docs.invalid');
  for (const [, href] of html.matchAll(/<a\b[^>]*\bhref="([^"]*)"/g)) {
    const url = new URL(href.replaceAll('&amp;', '&'), pageUrl);
    if (url.origin !== pageUrl.origin) continue;
    if (!url.pathname.startsWith(`${base}/`)) { errors.push(`${relative(root, path)}: wrong base path: ${href}`); continue; }
    let target = resolve(root, decodeURIComponent(url.pathname.slice(base.length + 1)));
    if (existsSync(target) && statSync(target).isDirectory()) target = join(target, 'index.html');
    if (!existsSync(target)) { errors.push(`${relative(root, path)}: missing target: ${href}`); continue; }
    if (url.hash && target.includes('/reference/resources/') && pages.has(target)) {
      const ids = new Set([...pages.get(target).matchAll(/\bid="([^"]*)"/g)].map((match) => match[1]));
      if (!ids.has(decodeURIComponent(url.hash.slice(1)))) errors.push(`${relative(root, path)}: missing field or section anchor: ${href}`);
    }
  }
}
if (errors.length) throw new Error(errors.join('\n'));
console.log(`Checked links in ${pages.size} generated HTML pages for ${base}/`);
