---
title: 'validate'
---

Validate project configuration and search paths.

## Synopsis

```bash
nyl validate [path] [--strict]
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
