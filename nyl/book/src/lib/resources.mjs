import manifest from '../../public/reference/schemas/resources.json' with { type: 'json' };

/** @typedef {{name: string, apiVersion: string, slug: string, schema: string, dynamicKind: boolean, route: string}} Resource */
/** @type {Resource[]} */
export const resources = manifest.resources.map((resource) => ({
  ...resource,
  route: `/reference/resources/${resource.apiVersion}/${resource.slug}/`,
}));
export const groupLabels = {
  'gitops.nyl/v1': 'Shared GitOps',
  'k8s.gitops.nyl/v1': 'Kubernetes GitOps',
  'k8s.nyl/v1': 'Kubernetes rendering',
  'components.k8s.nyl/v1': 'Kubernetes components',
};
export function summary(schema) { return schema.description.split(/\n\s*\n/)[0]; }
/** Resource-level usage sections authored in Rust doc comments and carried by the schema description. */
export function resourceUsage(schema) {
  const sections = schema.description.split(/^## /m).slice(1);
  return ['When needed', 'If omitted'].map((label) => {
    const matches = sections.filter((section) => section.split('\n', 1)[0].trim() === label);
    const markdown = matches[0]?.slice(matches[0].indexOf('\n') + 1).trim();
    if (matches.length !== 1 || !markdown) throw new Error(`${schema.title}: expected one nonempty "${label}" section in the resource description`);
    return { label, markdown };
  });
}
export function resourceGroups() {
  return [...new Set(resources.map((resource) => resource.apiVersion))].map((apiVersion) => ({
    apiVersion,
    label: groupLabels[apiVersion] ?? apiVersion,
    resources: resources.filter((resource) => resource.apiVersion === apiVersion),
  }));
}
export function resourceSidebar() {
  return resourceGroups().map((group) => ({
    label: group.apiVersion, collapsed: true,
    items: group.resources.map((resource) => ({ label: resource.name, link: resource.route })),
  }));
}
export const resourceRedirects = Object.fromEntries(resources.map((resource) => {
  const oldSlug = resource.name === 'HelmChart' ? 'helmchart' : resource.slug;
  const prefix = ['gitops.nyl/v1', 'k8s.gitops.nyl/v1'].includes(resource.apiVersion) ? 'gitops/' : '';
  return [`/reference/resources/${prefix}${oldSlug}`, resource.route];
}));
