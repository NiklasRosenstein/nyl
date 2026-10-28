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
  if (sections.length === 0) return [];
  return ['When needed', 'If omitted'].map((label) => {
    const matches = sections.filter((section) => section.split('\n', 1)[0].trim() === label);
    const markdown = matches[0]?.slice(matches[0].indexOf('\n') + 1).trim();
    if (matches.length !== 1 || !markdown) throw new Error(`${schema.title}: expected one nonempty "${label}" section in the resource description`);
    return { label, markdown };
  });
}
/**
 * The reference splits resources like the docs' header sections. A section
 * takes the resources of its API versions plus `include`, minus `exclude`;
 * included resources come first. The Release is the unit Nyl renders in both
 * modes, so it opens the Kubernetes section.
 */
export const resourceSections = [
  {
    id: 'kubernetes',
    label: 'Kubernetes Resources',
    route: '/reference/resources/kubernetes/',
    apiVersions: ['k8s.nyl/v1', 'components.k8s.nyl/v1'],
    include: ['Release'],
  },
  {
    id: 'rendered-gitops',
    label: 'Rendered GitOps Resources',
    route: '/reference/resources/rendered-gitops/',
    apiVersions: ['gitops.nyl/v1', 'k8s.gitops.nyl/v1'],
    exclude: ['Release', 'Environment', 'EnvironmentGroup'],
  },
  {
    id: 'orchestration',
    label: 'Orchestration Resources',
    route: '/reference/resources/orchestration/',
    apiVersions: [],
    include: ['Environment', 'EnvironmentGroup'],
  },
];
/** Each resource's usage guide: its section's guide directory, unless it lives elsewhere. */
const guideDirectories = { kubernetes: 'kubernetes', 'rendered-gitops': 'rendered-gitops', orchestration: 'orchestration' };
const guideOverrides = { Release: 'rendered-gitops', Cluster: 'rendered-gitops' };
export function guideRoute(resource) {
  return `/${guideOverrides[resource.name] ?? guideDirectories[sectionOf(resource).id]}/resource-guides/${resource.slug}/`;
}
function inSection(section, resource) {
  if (section.include?.includes(resource.name)) return true;
  return section.apiVersions.includes(resource.apiVersion) && !section.exclude?.includes(resource.name);
}
export function sectionOf(resource) {
  const sections = resourceSections.filter((section) => inSection(section, resource));
  if (sections.length !== 1) throw new Error(`${resource.name}: expected one reference section, found ${sections.length}`);
  return sections[0];
}
/** Resources by API version, optionally only those of one section, its included resources first. */
export function resourceGroups(section) {
  const members = section
    ? [
        ...resources.filter((resource) => section.include?.includes(resource.name)),
        ...resources.filter((resource) => !section.include?.includes(resource.name) && inSection(section, resource)),
      ]
    : resources;
  return [...new Set(members.map((resource) => resource.apiVersion))].map((apiVersion) => ({
    apiVersion,
    label: groupLabels[apiVersion] ?? apiVersion,
    resources: members.filter((resource) => resource.apiVersion === apiVersion),
  }));
}
/** One sidebar group per section: its overview, its included resources, then the rest by name. */
export function resourceSidebar() {
  const byName = (left, right) => left.name.localeCompare(right.name);
  return resourceSections.map((section) => {
    const members = resources.filter((resource) => sectionOf(resource) === section);
    const included = members.filter((resource) => section.include?.includes(resource.name));
    const rest = members.filter((resource) => !included.includes(resource)).sort(byName);
    return {
      label: section.label,
      items: [
        { label: 'Overview', link: section.route },
        ...[...included, ...rest].map((resource) => ({ label: resource.name, link: resource.route })),
      ],
    };
  });
}
export const resourceRedirects = Object.fromEntries(resources.map((resource) => {
  const oldSlug = resource.name === 'HelmChart' ? 'helmchart' : resource.slug;
  const prefix = ['gitops.nyl/v1', 'k8s.gitops.nyl/v1'].includes(resource.apiVersion) ? 'gitops/' : '';
  return [`/reference/resources/${prefix}${oldSlug}`, resource.route];
}));
