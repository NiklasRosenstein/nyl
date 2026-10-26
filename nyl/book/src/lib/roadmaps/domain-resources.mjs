// The roadmap of domain resource collections, milestone M9, rendered on the
// Domain resources roadmap page and summarized as M9 on the Orchestration
// roadmap. Tick a deliverable in the change that ships it, and set `done` on a
// slice only when all of it has shipped. The orchestration milestones it
// builds on are the M-numbered needs of its slices.

export const domainResourcesRoadmap = {
  id: 'domain-resources',
  milestone: 'M9',
  title: 'Domain resource collections for units and Releases',
  page: 'domain-resources/roadmap/',
  itemNoun: 'deliverables',
  exitCriterion:
    'Colocated team declarations feed independently registered units. Local and CI runs produce identical bundles and execution keys. Moving a declaration without changing its membership or admission re-runs nothing. Schema, policy, and missing-evidence failures block only the consuming units, before any effect. Releases select their manifests and are selected by label or path, read the same bundles in every render, and render identically with and without the cache. The guide runs end to end.',
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
        { text: 'Ownership claims with `access: own` and `access: read`, with removal and transition guards only for owners', done: false },
        { text: '`fromUnit` references inside declarations', done: false },
        { text: 'The versioned `ResourceBundle` and its provenance in desired state', done: false },
        { text: 'The bundle digest in the execution key', done: false },
        { text: '`fromCollection` references and the Command unit’s `collections` input', done: false },
        { text: '`nyl prepare` with fixture outputs', done: false },
        { text: 'Referenced resources in the bundle: the `x-nyl-reference` closure, read-only and part of the digest', done: false },
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
      needs: ['M4', 'R2'],
      items: [
        { text: 'The driver `prepare` capability and `nyl prepare --native`', done: false },
        { text: 'OpenTofu and Terraform adaptation, checked by the driver conformance suite', done: false },
        { text: 'Approval before collection members are removed', done: false },
        { text: '`nyl exec` with deployed and worktree bundles', done: false },
        { text: '`nyl prepare --native` and `nyl exec` for a unit outside any Environment, and `prepare --check`', done: false },
        { text: '`--worktree`, `--source`, and `--deployed` with warnings and refusals for relevant uncommitted changes', done: false },
      ],
    },
    {
      id: 'R5',
      title: 'Transition rules',
      summary: 'Definitions guard changes between versions, such as an account email, by denying them or requiring approval.',
      needs: ['R3', 'R4'],
      items: [
        { text: '`x-nyl-validations` CEL rules with `self` and `oldSelf`, and the `deny` effect', done: false },
        { text: '`--base` comparing against another revision, and an explicit `--no-base`, required outside orchestration', done: false },
        { text: 'The bundle of the current receipt as the baseline when a unit runs', done: false },
        { text: '`oldObject` and the operation for Kyverno policies', done: false },
        { text: 'Tracked last-applied refs with `exec --advance`: permission preflight, compare-and-swap, recovery steps, and `--bootstrap`', done: false },
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
    {
      id: 'R8',
      title: 'The Release catalog',
      summary: 'Releases point at their manifests and ApplicationGroups select them by label as well as by path.',
      needs: ['M2'],
      items: [
        { text: 'Literal Release `metadata.labels`, scanned and verified after rendering', done: false },
        { text: '`spec.manifests` under the local path rule, with `spec.include` deprecated', done: false },
        { text: 'Several Releases in one file, next to other Releases or control resources', done: false },
        { text: 'The Release catalog keyed by path and document, and `nyl get releases`', done: false },
        { text: 'ApplicationGroup source `selector` combined with path membership, guarded by an AppProject that derives nothing', done: false },
        { text: '`--release` for shared files in direct commands', done: false },
      ],
    },
    {
      id: 'R9',
      title: 'Release admission',
      summary: 'Policies decide which folders may declare which Releases, so label membership cannot widen a generated AppProject.',
      needs: ['R3', 'R8'],
      items: [
        { text: 'Kyverno validating policies over rendered Releases and their manifests, per target', done: false },
        { text: 'Trusted repository, source, target, and ApplicationGroup annotations, and the effective namespace', done: false },
      ],
    },
    {
      id: 'R10',
      title: 'Collections in Releases',
      summary: 'Releases read domain resource collections as template data, without orchestration.',
      needs: ['R1', 'R3', 'R8'],
      items: [
        { text: 'The pure `nyl-collections` crate with membership, the reference closure, and bundle assembly, shared with R2', done: false },
        { text: 'Release `spec.collections`, read-only, with `${ … }` selector values', done: false },
        { text: '`collections` and `referenced` template variables with YAML-safe output', done: false },
        { text: 'Bundle digests in the dependency recorder and the ownership index', done: false },
        { text: 'Rejection in remote sources and of unresolvable references', done: false },
      ],
    },
    {
      id: 'R11',
      title: 'One Release per resource',
      summary: 'A Release instantiates once per matching declaration with `forEach`.',
      needs: ['R10'],
      items: [
        { text: '`forEach` on Releases with the `resource` context', done: false },
        { text: 'Instance names with the clash check, and instance bindings over base bindings', done: false },
      ],
    },
  ],
};
