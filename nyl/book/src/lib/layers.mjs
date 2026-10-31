// The four layers of Nyl, in the order a project adopts them. The header tabs,
// the sidebar's layer rail, and the landing page read this one list, so the
// numbers, names, and status of a layer cannot disagree between them. `route`
// is the section's route under the docs base; `tone` picks its color:
// teal when shipped, amber while in development, grey while planned.

/** @typedef {{id: string, name: string, route: string, tone: 'teal' | 'amber' | 'grey', status?: string}} Layer */
/** @type {Layer[]} */
export const layers = [
  { id: 'kubernetes-rendering', name: 'Kubernetes Rendering', route: '/kubernetes-rendering/', tone: 'teal' },
  { id: 'kubernetes-gitops', name: 'Kubernetes GitOps', route: '/kubernetes-gitops/', tone: 'teal' },
  { id: 'orchestration', name: 'Orchestration', route: '/orchestration/', tone: 'amber', status: 'In development' },
  { id: 'domain-modeling', name: 'Domain modeling', route: '/domain-modeling/', tone: 'grey', status: 'Planned' },
];

/** The layer a link or route belongs to, matched on its section segment. */
export function layerOf(link) {
  return layers.find((layer) => link.includes(layer.route));
}

/** The layer's position, counted from 1. */
export function layerNumber(layer) {
  return layers.indexOf(layer) + 1;
}
