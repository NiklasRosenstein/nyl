# Troubleshooting Components

## Component Not Found

Symptom:

- Render/diff/apply fails to resolve a component kind.

Checks:

1. Confirm `apiVersion` is exactly `components.nyl.niklasrosenstein.github.com/v1` for Component resources.
2. Confirm `kind` matches `<apiVersion>/<kind>` directory layout.
3. Confirm `Chart.yaml` exists at `<components_search_path>/<kind>/Chart.yaml`.
4. Confirm `components_search_paths` are valid from the `nyl.toml` location.

Example expected path for `kind: example/v1/Nginx`:

```text
components/example/v1/Nginx/Chart.yaml
```

## Wrong Chart Selected

Symptom:

- Nyl resolves a shared component when repo-local component was expected.

Cause:

- Search paths are ordered; first match wins.

Fix:

- Put preferred path earlier in `project.components_search_paths`.

## Alias Not Applied

Symptom:

- Resource is treated as a normal Kubernetes resource instead of a chart-backed resource.

Checks:

1. Alias key is exact: `<apiVersion>/<kind>`.
2. Manifest `apiVersion` and `kind` exactly match key case and spelling.
3. Alias target uses a valid local path or remote shortcut syntax.

## Filtering Doesn’t Show Generated Resources

Symptom:

- `-c ConfigMap` misses ConfigMaps produced by Helm rendering.

Cause:

- Filtering runs before expansion.

Fix:

- Filter top-level chart resources (`-c HelmChart` or matching component resources), then inspect rendered output.

## Fast Validation Loop

Use:

```bash
nyl validate --strict
nyl render <manifest-file>
```

This catches path/config issues before cluster-facing commands.
