import { stringify } from 'yaml';

const structural = ['oneOf', 'anyOf', 'allOf', 'not', 'if', 'then', 'else', 'dependentRequired', 'dependentSchemas'];
const scalar = ['const', 'enum', 'default', 'format', 'pattern', 'minLength', 'maxLength', 'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum', 'multipleOf', 'minItems', 'maxItems', 'uniqueItems', 'minProperties', 'maxProperties', 'deprecated'];
const code = (value) => `\`${JSON.stringify(value).replaceAll('`', '\\`')}\``;
export function fieldAnchor(path) { return `field-${encodeURIComponent(path)}`; }

/** Convert a resource schema into readable Markdown for static HTML rendering. */
export function schemaReference(schema) {
  const output = ['## Fields', 'Required means required within the containing object. Optional and nullable are distinct; contextual defaults and requirements are explained with each field.'];
  function walk(input, path, required, seen = new Set()) {
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
      value = { ...resolved, ...nonNull, ...annotations, type: [...new Set([...(Array.isArray(resolved.type) ? resolved.type : resolved.type ? [resolved.type] : []), 'null'])] };
    }
    if (path) {
      output.push(`<a id="${fieldAnchor(path)}"></a>`, `### ${path}`);
      const type = value.type ? (Array.isArray(value.type) ? value.type.join(' or ') : value.type) : value.const !== undefined ? typeof value.const : value.oneOf || value.anyOf ? 'alternatives below' : 'any value';
      output.push(`**${required ? 'Required' : 'Optional'}** · ${type}`);
    }
    if (path && value.description) output.push(value.description);
    if (value.readOnly) output.push('Read only.');
    if (value.writeOnly) output.push('Write only.');
    for (const key of scalar) if (key in value) output.push(`**${key}:** ${code(value[key])}`);
    if (value.additionalProperties === false) output.push('Unknown fields are rejected.');
    const documentedAlternatives = value.oneOf?.every((option) => option.const !== undefined || option.type);
    const constraints = Object.fromEntries(structural.filter((key) => key in value && !(key === 'oneOf' && documentedAlternatives)).map((key) => [key, value[key]]));
    if (Object.keys(constraints).length) output.push('<details><summary>Validation constraints</summary>\n', '```yaml\n' + stringify(constraints).trimEnd() + '\n```', '</details>');
    for (const [field, child] of Object.entries(value.properties ?? {})) {
      walk(child, path ? `${path}.${field}` : field, value.required?.includes(field) ?? false, new Set(seen));
    }
    if (value.items) walk(value.items, `${path}[]`, true, new Set(seen));
    if (value.additionalProperties === true) output.push('Additional keys may contain any value.');
    else if (value.additionalProperties && typeof value.additionalProperties === 'object') walk(value.additionalProperties, `${path}[key]`, false, new Set(seen));
    for (const [pattern, child] of Object.entries(value.patternProperties ?? {})) walk(child, `${path}[${pattern}]`, false, new Set(seen));
    for (const keyword of ['oneOf', 'anyOf', 'allOf']) {
      const alternatives = value[keyword] ?? [];
      alternatives.forEach((alternative, index) => {
        // Constraint-only branches are shown above; typed alternatives need their field documentation too.
        if (alternative.$ref || alternative.type || alternative.description || alternative.const !== undefined) {
          const label = alternative.const ?? alternative.properties?.kind?.const ?? `alternative ${index + 1}`;
          walk(alternative, `${path}[${label}]`, required, new Set(seen));
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
