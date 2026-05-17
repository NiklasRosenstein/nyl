---
title: 'new'
---

Create new nyl projects and components.

## Synopsis

```bash
nyl new project <dir>
nyl new component <api-version> <kind>
```

## `nyl new project`

Creates:

```text
<dir>/
├── nyl.toml
└── components/
```

Generated `nyl.toml`:

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
```

## `nyl new component`

Creates component chart files under:

```text
components/<api-version>/<kind>/
├── Chart.yaml
├── values.yaml
├── values.schema.json
└── templates/deployment.yaml
```
