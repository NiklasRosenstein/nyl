import { stringify } from 'yaml';

const structural = ['oneOf', 'anyOf', 'allOf', 'not', 'if', 'then', 'else', 'dependentRequired', 'dependentSchemas'];
const scalar = ['const', 'default', 'format', 'pattern', 'minLength', 'maxLength', 'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum', 'multipleOf', 'minItems', 'maxItems', 'uniqueItems', 'minProperties', 'maxProperties', 'deprecated'];
const code = (value) => `\`${JSON.stringify(value).replaceAll('`', '\\`')}\``;
const html = (value) => String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const cell = (value) => value.replaceAll('|', '\\|').replace(/\r?\n/g, '<br>');
const literalType = (value) => value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value;
function types(value) {
  if (typeof value === 'boolean') return [];
  if (value.type) return Array.isArray(value.type) ? value.type : [value.type];
  if ('const' in value) return [literalType(value.const)];
  if (value.enum) return [...new Set(value.enum.map(literalType))];
  return [...new Set((value.oneOf ?? value.anyOf ?? []).flatMap(types))];
}

// Only annotation-only literal branches can be fully represented by a values table.
function enumChoices(value) {
  for (const keyword of ['oneOf', 'anyOf']) {
    const alternatives = value[keyword];
    if (alternatives?.length && alternatives.every((option) => typeof option === 'object' && 'const' in option && Object.keys(option).every((key) => ['const', 'type', 'description', 'title'].includes(key)))) {
      return { keyword, choices: alternatives.map((option) => ({ value: option.const, description: option.description })) };
    }
  }
  return undefined;
}
export function fieldAnchor(path) { return `field-${encodeURIComponent(path)}`; }

/** Convert a resource schema into readable Markdown for static HTML rendering. */
export function schemaReference(schema) {
  const output = ['## Fields', 'Required means required within the containing object. Optional and nullable are distinct; contextual defaults and requirements are explained with each field.'];
  function walk(input, path, required, seen = new Set(), anchorPath = path, variant = false) {
    if (typeof input === 'boolean') {
      output.push(`### ${path}`, input ? 'Any value is allowed.' : 'No value is allowed.');
      return;
    }
    let value = input;
    if (value.$ref) {
      const ref = value.$ref;
      if (!ref.startsWith('#/')) throw new Error(`Unsupported external field reference: ${ref}`);
      if (seen.has(ref)) { output.push(`Recursive reference: ${code(ref)}.`); return; }
      const resolved = ref.slice(2).split('/').reduce((node, part) => node?.[part.replaceAll('~1', '/').replaceAll('~0', '~')], schema);
      if (!resolved) throw new Error(`Unresolved schema reference: ${ref}`);
      seen = new Set([...seen, ref]);
      const { $ref, ...siblings } = value;
      value = { ...resolved, ...siblings };
    }
    // An optional object may also accept null; keep its authored field path intact.
    const nullable = value.anyOf?.length === 2 && value.anyOf.find((option) => option.type === 'null');
    if (nullable) {
      const { anyOf, ...annotations } = value;
      const nonNull = anyOf.find((option) => option.type !== 'null');
      let resolved = nonNull;
      if (nonNull.$ref) {
        if (seen.has(nonNull.$ref)) { output.push(`Recursive reference: ${code(nonNull.$ref)}.`); return; }
        resolved = nonNull.$ref.slice(2).split('/').reduce((node, part) => node?.[part.replaceAll('~1', '/').replaceAll('~0', '~')], schema);
        if (!resolved) throw new Error(`Unresolved schema reference: ${nonNull.$ref}`);
        seen = new Set([...seen, nonNull.$ref]);
      }
      value = { ...resolved, ...nonNull, ...annotations, type: [...new Set([...types(resolved), 'null'])] };
    }
    const enumeration = enumChoices(value);
    if (path && !variant) {
      output.push(`<a id="${fieldAnchor(anchorPath)}"></a>`, `### ${path}`);
      const inferredTypes = types(value);
      const type = inferredTypes.length ? inferredTypes.join(' or ') : value.oneOf || value.anyOf ? 'alternatives below' : 'any value';
      output.push(`**${required ? 'Required' : 'Optional'}** · ${type}`);
    }
    if (variant && types(value).length) output.push(`**Type:** ${types(value).join(' or ')}`);
    if (path && value.description) output.push(value.description);
    if (value.readOnly) output.push('Read only.');
    if (value.writeOnly) output.push('Write only.');
    for (const key of scalar) if (key in value) output.push(`**${key}:** ${code(value[key])}`);
    if (value.enum || enumeration) {
      const choices = enumeration?.choices ?? value.enum.map((option) => ({ value: option }));
      if (nullable && !choices.some((option) => option.value === null)) choices.push({ value: null });
      if (choices.some((option) => option.description)) {
        output.push('**Allowed values:**', ['| Value | Meaning |', '| --- | --- |', ...choices.map((option) => `| ${cell(code(option.value))} | ${cell(option.description ?? '')} |`)].join('\n'));
      } else {
        output.push(`**Allowed values:** ${choices.map((option) => code(option.value)).join(' · ')}`);
      }
    }
    if (value.additionalProperties === false) output.push('Unknown fields are rejected.');
    const documentedAlternatives = (keyword) => value[keyword]?.length && value[keyword].every((option) => option.const !== undefined || option.type || option.$ref);
    const constraints = Object.fromEntries(structural.filter((key) => key in value && !(['oneOf', 'anyOf', 'allOf'].includes(key) && documentedAlternatives(key))).map((key) => [key, value[key]]));
    if (Object.keys(constraints).length) output.push('<details><summary>Validation constraints</summary>\n', '```yaml\n' + stringify(constraints).trimEnd() + '\n```', '</details>');
    for (const [field, child] of Object.entries(value.properties ?? {})) {
      walk(child, path ? `${path}.${field}` : field, value.required?.includes(field) ?? false, new Set(seen), anchorPath ? `${anchorPath}.${field}` : field);
    }
    if (value.items) walk(value.items, `${path}[]`, true, new Set(seen), `${anchorPath}[]`);
    if (value.additionalProperties === true) output.push('Additional keys may contain any value.');
    else if (value.additionalProperties && typeof value.additionalProperties === 'object') walk(value.additionalProperties, `${path}[key]`, false, new Set(seen), `${anchorPath}[key]`);
    for (const [pattern, child] of Object.entries(value.patternProperties ?? {})) walk(child, `${path}[${pattern}]`, false, new Set(seen), `${anchorPath}[${pattern}]`);
    for (const keyword of ['oneOf', 'anyOf', 'allOf']) {
      if (enumeration?.keyword === keyword) continue;
      const alternatives = value[keyword] ?? [];
      if (documentedAlternatives(keyword)) output.push(keyword === 'oneOf' ? 'Choose exactly one variant:' : keyword === 'anyOf' ? 'Match one or more variants:' : 'All of the following apply:');
      alternatives.forEach((alternative, index) => {
        // Constraint-only branches are shown above; typed alternatives need their field documentation too.
        if (alternative.$ref || alternative.type || alternative.description || alternative.const !== undefined) {
          const label = alternative.title ?? alternative.const ?? alternative.properties?.kind?.const ?? `Variant ${index + 1}`;
          output.push(`<details><summary>${html(label)}</summary>\n`);
          walk(alternative, path, required, new Set(seen), `${anchorPath}[${keyword}-${index + 1}]`, true);
          output.push('</details>');
        }
      });
    }
  }
  walk(schema, '', true);
  return output.join('\n\n');
}

export function resourceMarkdown(schema) {
  return [schema.description, '## Example', '```yaml\n' + stringify(schema.examples[0]).trimEnd() + '\n```', schemaReference(schema)].join('\n\n');
}
