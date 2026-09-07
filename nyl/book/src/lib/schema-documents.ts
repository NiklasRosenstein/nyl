import type { Resource } from './resources.mjs';

const schemas = import.meta.glob<Record<string, any>>('../../public/reference/schemas/*/*/*.json', { eager: true, import: 'default' });

export function schemaFor(resource: Resource) {
  const schema = schemas[`../../public/reference/schemas/${resource.schema}`];
  if (!schema) throw new Error(`Missing resource schema: ${resource.schema}`);
  return schema;
}
