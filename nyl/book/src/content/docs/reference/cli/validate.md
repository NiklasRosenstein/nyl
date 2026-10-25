---
title: 'validate'
---

Validate project configuration and search paths.

## Synopsis

```bash
nyl validate [path]
nyl validate [path] [--strict]
```

## Checks

1. Project config discovery (`nyl.toml`)
2. TOML parse validity
3. Existence of each `project.components_search_paths` entry
4. Existence of each `project.helm_chart_search_paths` entry
5. Git-visible GitOps resources and references when the project is in Git
6. Environments, their groups, and their state locations
7. Unit envelopes: a literal `apiVersion`, `kind`, `metadata.name`, and
   `metadata.labels`, and names unique across unit kinds
8. Which Environment places each DeploymentTarget, and the `fromUnit` bindings
   of every placed target
9. A complete target compilation for every configured DeploymentTarget
10. Each Environment's units, with the checks `nyl plan` and `nyl reconcile`
    run before reading state: specs, `env.secrets`, overlapping resources,
    `dependsOn` and references to unknown units, cycles, references to
    undeclared or sensitive outputs, and evidence requirements

## Examples

```bash
nyl validate
nyl validate --strict
nyl validate /path/to/project
```

## Output

`validate` prints each passed check, then each warning once, then the verdict.
Search paths appear as `nyl.toml` writes them, relative to its directory; the
configuration file is shown relative to the current directory.

```console
$ nyl validate
✓ Found project config: nyl.toml
✓ Helm chart search path exists: .
✓ GitOps configuration is valid (4 resource(s), 0 unit(s), 2 YAML file(s))
⚠ Components search path does not exist: components

✓ Validation passed with warnings
```

Warnings leave the exit status at 0. `--strict` fails the run instead:

```console
$ nyl validate --strict
✓ Found project config: nyl.toml
✓ Helm chart search path exists: .
✓ GitOps configuration is valid (4 resource(s), 0 unit(s), 2 YAML file(s))
⚠ Components search path does not exist: components

✗ Validation failed in strict mode (warnings treated as errors)
```

When a check fails with an error, the checks that passed are printed before the
error, and the command exits with a non-zero status.

GitOps validation discovers all Git-visible control resources, applies
strict per-kind validation, resolves repository and project references, and
rejects overlapping target prefixes on the same repository revision.

## Units and placed targets

Validation renders every Environment's selected units with its effective
values from the worktree as it is, the same render `nyl plan` and
`nyl reconcile` use at their source commit, and runs the unit checks they run
before reading state, with the same codes: `NYL-UNIT-INVALID`,
`NYL-UNIT-KIND-UNKNOWN`, `NYL-UNIT-SECRETS-UNSUPPORTED`,
`NYL-UNIT-OWNERSHIP-OVERLAP`, `NYL-DEPENDENCY-UNKNOWN`, `NYL-DEPENDENCY-CYCLE`,
`NYL-REFERENCE-INVALID` for a reference to an output its producer does not
declare or marks `sensitive`, and `NYL-ATTESTATIONS-UNSUPPORTED`. An `artifact`
reference between units is checked only for the producer's existence. A
`KubernetesPublication` unit places the DeploymentTarget its `spec.target`
names into that Environment. A target placed by publication units of two
Environments fails with `NYL-TARGET-ENVIRONMENT-CONFLICT`.

A placed target compiles with a placeholder of the declared type for each
`fromUnit` and `fromPromotion` binding, because only a run resolves their
values. Each `fromUnit` binding must instead name a unit the Environment
selects and enables; an `output` reference must name an output the unit
declares and does not mark `sensitive`. An `artifact` reference is checked
only for the unit's existence. Every problem names its binding, such as
`spec.releaseInputs."web/web".databaseHost.fromUnit`.

A publication unit with `enabled: false` still places its target, because
the target belongs to the Environment until its teardown completes. Its
bindings are not checked, so a stack can be disabled together, and its
target may already be removed.

A target no publication unit places compiles as `render-tree` would, so a
`fromUnit` or `fromPromotion` binding on it fails and names the binding.
`render-tree`, `publish-tree`, `diff-tree`, and the direct commands never
resolve these bindings.

A minimal project outside Git receives project validation only. A project with
`gitops.yaml` must belong to a Git worktree.
