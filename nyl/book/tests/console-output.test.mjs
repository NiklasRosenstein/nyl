import test from 'node:test';
import assert from 'node:assert/strict';
import { toHtml } from 'hast-util-to-html';
import { ExpressiveCode } from 'astro-expressive-code';
import { consoleOutput } from '../src/lib/console-output.mjs';

test('console blocks dim output lines and copy only the commands', async () => {
  const ec = new ExpressiveCode({ plugins: [consoleOutput()] });
  const code = '$ nyl get targets \\\n  --color never\nNAME  CLUSTER\ndev   dev\n$ nyl validate';
  const { renderedGroupAst } = await ec.render({ code, language: 'console' });
  const html = toHtml(renderedGroupAst);

  const lines = [...html.matchAll(/<div class="(ec-line[^"]*)"/g)].map((match) => match[1].includes('output'));
  assert.deepEqual(lines, [false, false, true, true, false]);
  assert.match(html, /data-code="nyl get targets \\\x7F  --color never\x7Fnyl validate"/);
});
