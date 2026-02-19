# Renovate Integration

Nyl ships a reusable Renovate preset for Helm chart version bumps in `HelmChart`, `Component` shortcuts, and `project.aliases`.

## Use The Shared Preset

In your project's `renovate.json`:

```json
{
  "extends": [
    "config:recommended",
    "github>NiklasRosenstein/nyl:.github/renovate/nyl-helm-components.json5"
  ]
}
```

## What It Matches

The preset adds regex managers for:

- `HelmChart` resources (`apiVersion: nyl.niklasrosenstein.github.com/v1`)
  - `spec.chart.repository` + `spec.chart.name` + `spec.chart.version`
- `Component` shortcut resources (`apiVersion: components.nyl.niklasrosenstein.github.com/v1`)
  - `kind: https://...#<chart>@<version>`
  - `kind: oci://...@<version>`
  - `kind: git+...#<path>@<ref>`
- `nyl.toml` alias targets in `[project.aliases]`
  - `"myapi/v1/Kind" = "https://...#<chart>@<version>"`
  - `"myapi/v1/Kind" = "oci://...@<version>"`
  - `"myapi/v1/Kind" = "git+...#<path>@<ref>"`

## Datasource Mapping

- `https://...` shortcuts and Helm repositories -> Renovate `helm` datasource
- `oci://...` shortcuts and repositories -> Renovate `docker` datasource
- `git+...` shortcuts and repositories -> Renovate `git-tags` datasource

## Notes And Limitations

- Local component kinds (for example `example/v1/Nginx`) are intentionally ignored.
- Matches expect canonical field names (`repository`, `name`, `version`) in `HelmChart` specs.
- `git+...@<ref>` updates depend on available Git tags for the referenced repository.

## Troubleshooting

- If a dependency is not detected, first verify it includes an explicit `@<version>` (or `spec.chart.version` for `HelmChart`).
- Run `renovate-config-validator` against your final config to verify schema and templates.

## Validate Matcher Coverage

Nyl includes fixture-based matcher tests for the shared preset:

```bash
mise run test-renovate-matchers
```

The test runs the official `renovate/renovate` Docker image and validates extracted dependencies from Renovate's own logs.
Docker daemon access is required.

Fixtures are in `integrationtests/fixtures/renovate-matchers/`:

- `positive/`: inputs that must match
- `negative/`: inputs that must not match
- `expected.json`: expected extracted dependencies (`datasource`, `depName`, `currentValue`) per fixture file

The validation is extraction-focused: it asserts matching behavior and extracted dependency identity, and does not require successful network datasource lookups.

When changing `.github/renovate/nyl-helm-components.json5`, update fixtures and `expected.json` in the same change to keep matcher behavior explicit.
