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
6. A complete target compilation for every configured DeploymentTarget

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
✓ GitOps configuration is valid (4 resource(s), 2 YAML file(s))
⚠ Components search path does not exist: components

✓ Validation passed with warnings
```

Warnings leave the exit status at 0. `--strict` fails the run instead:

```console
$ nyl validate --strict
✓ Found project config: nyl.toml
✓ Helm chart search path exists: .
✓ GitOps configuration is valid (4 resource(s), 2 YAML file(s))
⚠ Components search path does not exist: components

✗ Validation failed in strict mode (warnings treated as errors)
```

When a check fails with an error, the checks that passed are printed before the
error, and the command exits with a non-zero status.

GitOps validation discovers all Git-visible control resources, applies
strict per-kind validation, resolves repository and project references, and
rejects overlapping target prefixes on the same repository revision.

A minimal project outside Git receives project validation only. A project with
`gitops.yaml` must belong to a Git worktree.
