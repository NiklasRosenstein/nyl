---
title: 'validate'
---

Validate project configuration and search paths.

## Synopsis

```bash
nyl validate [path] [--strict]
nyl validate gitops [path]
```

## Checks

1. Project config discovery (`nyl.toml`)
2. TOML parse validity
3. Existence of each `project.components_search_paths` entry
4. Existence of each `project.helm_chart_search_paths` entry

## Examples

```bash
nyl validate
nyl validate --strict
nyl validate /path/to/project
```

`nyl validate gitops` discovers all Git-visible control resources, applies
strict per-kind validation, resolves repository and project references, and
rejects overlapping target prefixes on the same repository revision.
