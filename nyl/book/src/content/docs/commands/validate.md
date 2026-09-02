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

GitOps validation discovers all Git-visible control resources, applies
strict per-kind validation, resolves repository and project references, and
rejects overlapping target prefixes on the same repository revision.

A minimal project outside Git receives project validation only. A project with
`gitops.yaml` must belong to a Git worktree.
