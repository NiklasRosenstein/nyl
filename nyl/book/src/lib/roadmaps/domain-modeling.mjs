// The roadmap of domain modeling, milestone M9, rendered on the
// Domain modeling roadmap page and summarized as M9 on the Orchestration
// roadmap. Tick a deliverable in the change that ships it, and set `done` on a
// slice only when all of it has shipped. The orchestration milestones it
// builds on are the M-numbered needs of its slices. Slice IDs stay stable for
// issues and pull requests: a new slice takes the next free number and is
// placed in rollout order, after the slices it needs, as R12 is after R1.

export const domainModelingRoadmap = {
  id: 'domain-modeling',
  milestone: 'M9',
  title: 'Domain modeling for units and Releases',
  page: 'domain-modeling/roadmap/',
  itemNoun: 'deliverables',
  exitCriterion:
    'Colocated team declarations feed independently registered units and export, without any unit, as versioned bundles for other tools. Local and CI runs produce identical bundles and execution keys. Moving a declaration without changing its membership or admission re-runs nothing. Schema, policy, and missing-evidence failures block only the consuming units, before any effect. Releases select their manifests and are selected by label or path, read the same bundles in every render, render outside orchestration unless a template reads a value that resolves only in an orchestrated publication, and render identically with and without the cache. The guide runs end to end.',
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
        { text: 'Discovery of literal declarations with project-wide identity and Git provenance', done: false },
        { text: 'Schema validation with defaults, and `x-nyl-reference` checks', done: false },
        {
          text: 'Domain resources as a context of the [value-source table](/nyl/reference/value-sources/#contexts): `$fromUnit` and `$fromPromotion` carried at any depth of `spec`, other sources rejected with their fix, literal-only envelope and reference fields, and source positions skipped by schema checks',
          done: false,
        },
        { text: '`nyl validate` covering definitions and declarations', done: false },
        { text: '`nyl get resources` and `nyl schema resources`', done: false },
        { text: 'The running example’s `Team` and `Service` definitions and declarations in `examples/service-catalog/`, with its reference scenario covering `nyl validate`, a misspelled owner, and schema defaults', done: false },
      ],
    },
    {
      id: 'R12',
      title: 'The standalone export',
      summary:
        'Domain modeling works on its own: the admitted catalog and bundles for any selection are exported as versioned documents, without units, drivers, Releases, or Kubernetes.',
      needs: ['M6', 'R1'],
      items: [
        { text: 'The pure `nyl-collections` crate with membership, ordering, the `x-nyl-reference` closure, and bundle assembly, shared by every consumer', done: false },
        { text: 'The versioned `ResourceBundle`, `ResourceBundleProvenance`, and `ResourceCatalog` formats, with golden files and published JSON Schemas', done: false },
        { text: '`nyl get resources -o yaml|json` printing the admitted catalog with defaults applied', done: false },
        { text: '`nyl bundle` selecting members by kind, labels, paths, or a unit’s named collection, with referenced resources and provenance', done: false },
        { text: 'Field selection on the admitted, defaulted `spec`: `matchFields` in collections and `--field` on `nyl get resources` and `nyl bundle`, with equality and set membership on scalar fields, and fields that hold a source never matching', done: false },
        { text: 'Sources left in place and listed under `unresolved` without `-e`, and resolved read-only against current receipts with `-e`', done: false },
        { text: 'The example’s standalone consumers: the on-call export from `nyl bundle --field /spec/tier=critical`, and the static service directory page generated from `nyl get resources -o json`, checked in its reference scenario', done: false },
      ],
      exitCriterion:
        'A project with only `[resources]`, definitions, and declarations, and no Environment, unit, or Release, exports its catalog and a bundle for any selection, byte-identical across machines and worktree layouts; a field selector reads the admitted `spec` and never matches a source; without `-e` the bundle lists its sources and exits 0, and with `-e` they resolve against current receipts, a missing receipt exiting 2; no state is written and no driver is loaded; the published schemas and golden files pin each format.',
    },
    {
      id: 'R2',
      title: 'Collections and the bundle',
      summary: 'Units collect declarations and receive them as one tool-neutral bundle.',
      needs: ['M3', 'R12'],
      items: [
        { text: 'The `collections` unit field with `allowEmpty`, assembled by the code of R12', done: false },
        { text: 'Ownership claims with `access: own` and `access: read`, with removal and transition guards only for owners', done: false },
        { text: '`$fromUnit` and `$fromPromotion` in declarations resolved by the consuming unit’s run, with dependency edges and `blockedBy`', done: false },
        { text: 'The bundle and its provenance in desired state and receipts', done: false },
        { text: 'The bundle digest in the execution key', done: false },
        { text: '`$fromCollection` references and the Command unit’s `collections` input', done: false },
        { text: '`nyl prepare` with fixture outputs, writing the same `bundle.yaml` as `nyl bundle`', done: false },
        { text: 'The example’s `oncall-roster` OpenTofu unit owning the Services, and the `service-directory` Command unit publishing the same page from its `collections` input, both depending on `database` through `checkout`’s `$fromUnit`', done: false },
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
        { text: 'The `referenced` policy context holding the declaration’s `x-nyl-reference` closure, its digest in the admission cache key, and every other lookup failing offline', done: false },
        { text: 'The example’s policy requiring a critical service’s owning Team, read from `referenced`, to name an on-call member', done: false },
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
        { text: 'The example’s removal of a critical service waiting for approval in `oncall-roster` only', done: false },
      ],
    },
    {
      id: 'R5',
      title: 'Transition rules',
      summary: 'Definitions guard changes between versions, such as an account email, by denying them or requiring approval.',
      needs: ['R3', 'R4'],
      items: [
        { text: '`x-nyl-validations` CEL rules with `self` and `oldSelf`, and the `deny` effect', done: false },
        { text: '`--base` comparing against another revision, and an explicit `--no-base`, required outside orchestration and by `nyl bundle`', done: false },
        { text: 'The bundle of the current receipt as the baseline when a unit runs', done: false },
        { text: '`oldObject` and the operation for Kyverno policies', done: false },
        { text: 'Tracked last-applied refs with `exec --advance`: permission preflight, compare-and-swap, recovery steps, and `--bootstrap`', done: false },
        { text: 'The `approve` effect for guarded changes, with removed members as one of them, and `--allow-change` outside orchestration', done: false },
        { text: 'The example’s `onCall` membership rule and the approval of a `tier` downgrade', done: false },
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
      summary: 'Releases read declaration collections as template data, without orchestration.',
      needs: ['R1', 'R12', 'R3', 'R8'],
      items: [
        { text: 'Release `spec.collections`, read-only, with `${ … }` selector values, assembled by the code of R12', done: false },
        { text: '`collections` and `referenced` template variables with YAML-safe output', done: false },
        { text: 'Bundle digests in the dependency recorder and the ownership index', done: false },
        { text: 'Rejection in remote sources', done: false },
        { text: 'Carried sources as unknown values outside orchestration, failing the render only when a template reads one, with the rendered-output backstop, and resolved in an orchestrated `KubernetesPublication`', done: false },
        { text: 'The example’s `service-namespaces` Release rendering a Namespace, a ResourceQuota, and a RoleBinding per Service in `render-tree`, `checkout` included, and failing with `NYL-RELEASE-VALUE-UNKNOWN` once a template reads its `database.host`', done: false },
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
