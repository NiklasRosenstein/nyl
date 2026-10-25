// The roadmap of domain resource collections (milestone M9), rendered on the
// Domain resources roadmap page. ROADMAP.md lists only the milestone row; the
// slices and their deliverables live here. Tick a deliverable when it ships;
// a slice's status follows from its deliverables.

export const domainResourcesRoadmap = {
  milestone: 'M9',
  title: 'Domain resource collections as unit inputs',
  issue: 'https://github.com/NiklasRosenstein/nyl/issues/173',
  design: 'https://github.com/NiklasRosenstein/nyl/blob/develop/design/domain-resources.md',
  exitCriterion:
    'Colocated team declarations feed independently registered units. Local and CI runs produce identical bundles and execution keys. Moving a declaration re-runs nothing. Schema, policy, and missing-evidence failures block only the consuming units, before any effect. The guide runs end to end.',
  // Orchestration milestones the slices build on, with their status in ROADMAP.md.
  prerequisites: [
    { id: 'M2', title: 'Release inputs without orchestration', status: 'done' },
    { id: 'M3', title: 'Orchestration core with a constrained command unit', status: 'in-progress' },
    { id: 'M4', title: 'Container image and Terraform units', status: 'planned' },
    { id: 'M8', title: 'Provider-neutral CI execution and first GitLab adapter', status: 'planned' },
  ],
  slices: [
    {
      id: 'R1',
      title: 'Definitions and the catalog',
      summary: 'Teams can declare typed resources and validate them, without orchestration.',
      needs: ['M2'],
      deliverables: [
        { text: '`ResourceDefinition` loaded only from the `[resources]` paths of `nyl.toml`', done: false },
        { text: 'Discovery of literal domain resources with project-wide identity and Git provenance', done: false },
        { text: 'Schema validation with defaults, and `x-nyl-reference` checks', done: false },
        { text: '`nyl validate` covering definitions and declarations', done: false },
        { text: '`nyl get resources` and `nyl schema resources`', done: false },
      ],
    },
    {
      id: 'R2',
      title: 'Collections and the bundle',
      summary: 'Units collect declarations and receive them as one tool-neutral bundle.',
      needs: ['M3', 'R1'],
      deliverables: [
        { text: 'The `collections` unit field with membership, ordering, and `allowEmpty`', done: false },
        { text: 'Ownership claims with `access: own` and `access: read`', done: false },
        { text: '`fromUnit` references inside declarations', done: false },
        { text: 'The versioned `ResourceBundle` and its provenance in desired state', done: false },
        { text: 'The bundle digest in the execution key', done: false },
        { text: '`fromCollection` references and the Command unit’s `collections` input', done: false },
        { text: '`nyl prepare` with fixture outputs', done: false },
      ],
    },
    {
      id: 'R3',
      title: 'Policy admission',
      summary: 'Policies restrict what each folder may declare.',
      needs: ['R1'],
      deliverables: [
        { text: 'Kyverno validating policies loaded from `[resources] policies`', done: false },
        { text: 'Trusted source-path annotations and a CRD derived from each definition', done: false },
        { text: 'Rejection of mutating policies', done: false },
      ],
    },
    {
      id: 'R4',
      title: 'Native adaptation',
      summary: 'OpenTofu and Terraform consume bundles, and removals wait for approval.',
      needs: ['M4', 'M8', 'R2'],
      deliverables: [
        { text: 'The driver `prepare` capability and `nyl prepare --native`', done: false },
        { text: 'OpenTofu and Terraform adaptation, checked by the driver conformance suite', done: false },
        { text: 'Approval before collection members are removed', done: false },
        { text: '`nyl exec` with deployed and worktree bundles', done: false },
      ],
    },
    {
      id: 'R5',
      title: 'One unit per resource',
      summary: 'A unit instantiates once per matching declaration with `forEach`.',
      needs: ['R2'],
      deliverables: [
        { text: '`forEach` scanned statically and expanded from the catalog', done: false },
        { text: '`resource` in the structural pass, with YAML-escaped values', done: false },
        { text: 'Instance names with the clash check', done: false },
        { text: 'Instances leaving the ownership set under the teardown gate', done: false },
      ],
    },
    {
      id: 'R6',
      title: 'The worked guide',
      summary: 'The AWS, GitHub, and Bao walkthrough becomes runnable.',
      needs: ['R2', 'R3', 'R4', 'R5'],
      deliverables: [
        { text: 'The development, testing, and breakglass guide for AWS accounts and access grants', done: false },
        { text: 'The guide validated as a tier-2 reference scenario', done: false },
      ],
    },
  ],
};

/** A slice's status, derived from its deliverables. */
export function sliceStatus(slice) {
  const done = slice.deliverables.filter((deliverable) => deliverable.done).length;
  if (done === slice.deliverables.length) return 'done';
  return done > 0 ? 'in-progress' : 'planned';
}

/** Delivered and total deliverables across every slice. */
export function roadmapProgress(roadmap) {
  const deliverables = roadmap.slices.flatMap((slice) => slice.deliverables);
  return { done: deliverables.filter((deliverable) => deliverable.done).length, total: deliverables.length };
}

export const statusLabels = { done: 'Done', 'in-progress': 'In progress', planned: 'Planned' };
