import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import Ajv2020 from 'ajv/dist/2020.js';
import { createMarkdownProcessor } from '@astrojs/markdown-remark';
import { resources, resourceGroups, resourceSidebar, summary } from '../src/lib/resources.mjs';
import { fieldAnchor, resourceMarkdown, schemaReference } from '../src/lib/schema-reference.mjs';

const root = new URL('../public/reference/schemas/', import.meta.url);
const schemaFor = (resource) => JSON.parse(readFileSync(new URL(resource.schema, root), 'utf8'));

test('every resource has an accurate, documented schema and valid example', () => {
  const ajv = new Ajv2020({ strict: false, allErrors: true });
  for (const resource of resources) {
    const schema = schemaFor(resource);
    assert.ok(summary(schema).length > 20, resource.name);
    assert.equal(schema.properties.apiVersion.const, resource.apiVersion);
    assert.ok(schema.examples.length, resource.name);
    const validate = ajv.compile(schema);
    for (const example of schema.examples) assert.ok(validate(example), `${resource.name}: ${JSON.stringify(validate.errors)}`);
    for (const [name, definition] of Object.entries({ root: schema, ...schema.$defs })) {
      for (const [field, property] of Object.entries(definition.properties ?? {})) {
        assert.ok(property.description || property.$ref, `${resource.name}: ${name}.${field} needs Rust documentation`);
      }
    }
  }
});

test('cluster destination schema enforces exactly one non-null destination', () => {
  const schema = schemaFor(resources.find((resource) => resource.name === 'Cluster'));
  const validate = new Ajv2020({ strict: false }).compile(schema);
  for (const [destination, expected] of [[{}, false], [{ name: null }, false], [{ name: 'primary', server: 'https://cluster' }, false], [{ name: 'primary', server: null }, true]]) {
    const example = structuredClone(schema.examples[0]);
    example.spec.destination = destination;
    assert.equal(validate(example), expected);
  }
});

test('catalog and sidebar cover the same version-qualified identities', () => {
  assert.equal(resourceGroups().flatMap((group) => group.resources).length, resources.length);
  assert.equal(new Set(resources.map((resource) => resource.route)).size, resources.length);
  const links = resourceSidebar().flatMap((group) => group.items.map((item) => item.link));
  assert.deepEqual(links, resources.map((resource) => resource.route));
});

test('field references retain nested descriptions, sibling annotations, alternatives, and map values', async () => {
  const schema = {
    $defs: { Item: { type: 'object', description: 'An **item**.', required: ['name'], properties: { name: { type: 'string', description: 'Its name.' }, parent: { $ref: '#/$defs/Item' } } } },
    properties: {
      items: { type: 'array', items: { $ref: '#/$defs/Item' } },
      labels: { type: 'object', additionalProperties: { type: 'string' } },
      chosen: { anyOf: [{ $ref: '#/$defs/Item', description: 'Chosen **item**.' }, { type: 'null' }] },
      mode: { enum: ['manual', 'automatic'], default: 'manual' },
    },
  };
  const markdown = schemaReference(schema);
  assert.ok(markdown.includes(`id="${fieldAnchor('items[].name')}"`));
  assert.ok(markdown.includes('Chosen **item**.'));
  assert.ok(markdown.includes('labels[key]'));
  assert.ok(markdown.includes('Recursive reference:'));
  assert.ok(markdown.includes('**default:** `"manual"`'));
  const rendered = await (await createMarkdownProcessor()).render(markdown);
  assert.ok(rendered.code.includes('Chosen <strong>item</strong>'));
});

test('all references render schema examples and resolvable fields', () => {
  for (const resource of resources) {
    const markdown = resourceMarkdown(schemaFor(resource));
    assert.ok(markdown.includes(`apiVersion: ${resource.apiVersion}`));
    assert.ok(markdown.includes('id="field-metadata.name"'));
  }
});

test('literal choices render inline or as a documented table within the field', async () => {
  const schema = {
    $defs: { Policy: { oneOf: [
      { const: 'Foreground', type: 'string', description: 'Delete **with** workloads.' },
      { const: 'Orphan', type: 'string', description: 'Keep workloads | keep data.\nUse `manual|cleanup`.' },
    ] } },
    properties: {
      mode: { enum: ['manual', 'automatic'] },
      policy: { anyOf: [{ $ref: '#/$defs/Policy' }, { type: 'null' }], default: null },
    },
  };
  const { code } = await (await createMarkdownProcessor()).render(schemaReference(schema));
  assert.match(code, /Allowed values:<\/strong> <code>"manual"<\/code> · <code>"automatic"<\/code>/);
  assert.match(code, /Optional<\/strong> · string or null/);
  assert.match(code, /<td><code>"Foreground"<\/code><\/td><td>Delete <strong>with<\/strong> workloads\.<\/td>/);
  assert.match(code, /<td>Keep workloads \| keep data\.<br>Use <code>manual\|cleanup<\/code>\.<\/td>/);
  assert.match(code, /<td><code>null<\/code><\/td>/);
  assert.equal((code.match(/<h3 /g) ?? []).length, 2, 'choices belong to their field, rather than introducing field headings');
});

test('structured variants expose real field paths and unique anchors in expandable sections', async () => {
  const schema = schemaFor(resources.find((resource) => resource.name === 'ApplicationGroup'));
  const { code } = await (await createMarkdownProcessor()).render(schemaReference({ ...schema, properties: { owner: { $ref: '#/$defs/SharedNamespaceOwner' } } }));
  assert.match(code, /Choose exactly one variant:/);
  for (const kind of ['Release', 'Dedicated', 'External']) {
    assert.match(code, new RegExp(`<details><summary>${kind}</summary>[\\s\\S]*?<h3 [^>]*>owner\\.kind</h3>[\\s\\S]*?</details>`));
  }
  assert.match(code, /<h3 [^>]*>owner\.release<\/h3>\s*<p><strong>Required<\/strong> · string/);
  assert.match(code, /The selected workload Release owns the Namespace/);
  const ids = [...code.matchAll(/ id="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(new Set(ids).size, ids.length, 'fields shared by variants must have distinct fragment targets');
});
