// The roadmap of domain resource collections, milestone M9, rendered on the
// Domain resources roadmap page and summarized as M9 on the Orchestration
// roadmap. Tick a deliverable in the change that ships it, and set `done` on a
// slice only when all of it has shipped. The orchestration milestones it
// builds on are the M-numbered needs of its slices.

export const domainResourcesRoadmap = {
  id: 'domain-resources',
  milestone: 'M9',
  title: 'Domain resource collections as unit inputs',
  page: 'domain-resources/roadmap/',
  itemNoun: 'deliverables',
  exitCriterion:
    'Colocated team declarations feed independently registered units. Local and CI runs produce identical bundles and execution keys. Moving a declaration without changing its membership or admission re-runs nothing. Schema, policy, and missing-evidence failures block only the consuming units, before any effect. The guide runs end to end.',
  // Set when the exit criterion is met, which completes M9.
  done: false,
  // GitHub issues this roadmap delivers; close them when `done` is set.
  issues: [173],
  stages: [
    {
      id: 'R1',
      title: 'Definitions and the catalog',
      summary: 'Teams can declare typed resources and validate them, without orchestration.',
      needs: ['M2'],
      items: [
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
      items: [
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
      items: [
        { text: 'Kyverno validating policies loaded from `[resources] policies`', done: false },
        { text: 'Trusted source-path annotations and a CRD derived from each definition', done: false },
        { text: 'Rejection of mutating policies', done: false },
      ],
    },
    {
      id: 'R4',
      title: 'Native adaptation',
      summary: 'OpenTofu and Terraform consume bundles, with or without orchestration, and removals wait for approval.',
      needs: ['M4', 'M8', 'R2'],
      items: [
        { text: 'The driver `prepare` capability and `nyl prepare --native`', done: false },
        { text: 'OpenTofu and Terraform adaptation, checked by the driver conformance suite', done: false },
        { text: 'Approval before collection members are removed', done: false },
        { text: '`nyl exec` with deployed and worktree bundles', done: false },
        { text: '`nyl prepare --native` and `nyl exec` for a unit outside any Environment, and `prepare --check`', done: false },
      ],
    },
    {
      id: 'R5',
      title: 'Transition rules',
      summary: 'Definitions guard changes between versions, such as an account email, by denying them or requiring approval.',
      needs: ['R3', 'R4'],
      items: [
        { text: '`x-nyl-validations` CEL rules with `self` and `oldSelf`, and the `deny` effect', done: false },
        { text: '`nyl validate --base`, `plan --base`, and `prepare --base` comparing against another revision', done: false },
        { text: 'The bundle of the current receipt as the baseline when a unit runs', done: false },
        { text: '`oldObject` and the operation for Kyverno policies', done: false },
        { text: 'The `approve` effect for guarded changes, with removed members as one of them, and `--allow-change` outside orchestration', done: false },
      ],
    },
    {
      id: 'R6',
      title: 'One unit per resource',
      summary: 'A unit instantiates once per matching declaration with `forEach`.',
      needs: ['R2'],
      items: [
        { text: '`forEach` scanned statically and expanded from the catalog', done: false },
        { text: '`resource` in the structural pass, with only YAML-safe values printed', done: false },
        { text: 'Instance names with the clash check', done: false },
        { text: 'Instances leaving the ownership set under the teardown gate', done: false },
      ],
    },
    {
      id: 'R7',
      title: 'The worked guide',
      summary: 'The AWS, GitHub, and Bao walkthrough becomes runnable.',
      needs: ['R2', 'R3', 'R4', 'R5', 'R6'],
      items: [
        { text: 'The development, testing, and breakglass guide for AWS accounts and access grants', done: false },
        { text: 'The guide validated as a tier-2 reference scenario', done: false },
      ],
    },
  ],
};
