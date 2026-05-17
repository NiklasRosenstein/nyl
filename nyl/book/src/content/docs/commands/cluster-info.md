---
title: 'cluster-info'
---

Display Kubernetes server version and API versions discovered from the selected cluster.

## Synopsis

```bash
nyl cluster-info [OPTIONS]
```

## Description

The `cluster-info` command is useful when preparing `nyl render --offline` inputs. It connects to Kubernetes using the selected Nyl profile and prints the Kubernetes version plus API versions that Helm should see during offline rendering.

## Options

- `-p, --profile <PROFILE>` - Profile to use for connecting to the cluster.
- `--output <FORMAT>` - Output format: `text`, `json`, `yaml`, or `csv` (default: `text`).

## Examples

Human-readable output:

```bash
nyl cluster-info -p prod
```

CSV output for copying into `[project.kubernetes]` or `[profile.<name>.kubernetes]`:

```bash
nyl cluster-info -p prod --output csv
```

The first CSV line is the Kubernetes version. The second line is a comma-separated list of API versions.
