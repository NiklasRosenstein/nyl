---
title: Python client
description: Render and inspect Kubernetes manifests from Python tests.
---

The `nyl` Python package exposes `NylClient` for rendering one source manifest
through an installed Nyl executable and inspecting its resources. It requires
Python 3.11+ and works independently of pytest.

```bash
uv add nyl
# Or: pip install nyl
```

[Install the Nyl executable](/nyl/start/getting-started/) separately. Put it on
`PATH` or pass its path as `executable`. Installing or importing the Python package
does not require the executable or Rust. Integration tests build the executable
from the same checkout, currently CLI 0.5.9; matching version strings alone do not
establish compatibility with other binaries.

## Test a rendered Release

```python
from nyl import NylClient

def test_web():
    client = NylClient(project="/path/to/infrastructure", timeout=120)
    manifests = client.render(
        "applications/workloads/web.yaml", target="development"
    )
    deployment = manifests.one("Deployment", "web")
    assert deployment.namespace == "web"
    for container in deployment.containers(include_init=True):
        assert container.security_context["allowPrivilegeEscalation"] is False
        assert container.env.value("PGHOST") == "database-rw"
        assert container.env.secret_ref("PGPASSWORD") == ("database-app", "password")
```

Rust owns configuration, validation, rendering, and caching. Every `render()`
call invokes the CLI; target-aware Releases use the compiled target and the
project's configured validation defaults. `offline=True` and
`allow_secret_inputs=True` forward the corresponding
[`render` flags](/nyl/reference/cli/render/). A nonzero exit, including validation
failure, raises an error even if the CLI emits manifests.

## Select resources and inspect declarations

`manifests.select(kind=None, name=None, *, namespace=..., api_version=None)` returns
an iterable `ManifestSet` in document order. `one(kind, name=None, *, namespace=...,
api_version=None)` requires exactly one match. Omit the namespace to search all
namespaces; pass `namespace=None` to select resources with no namespace. Namespace
properties reflect declarations; the client does not infer a destination namespace.
Each resource exposes `raw`, `api_version`, `kind`, `name`, and `namespace`.

Container helpers support Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets,
ReplicationControllers, Jobs, and CronJobs. They inspect literal values and Secret
key references without resolving Secrets, ConfigMaps, `envFrom`, inherited security
contexts, or live cluster data. Missing, duplicate, and incompatible declarations
raise `DeclarationError`. `security_context` is an empty dictionary when omitted.

`ManifestSet.from_yaml()` also safely parses multi-document YAML and skips empty
documents. It preserves the CLI's quoted string scalars without YAML 1.1 coercion.

## Errors and scope

Errors derive from `NylError`. `ExecutableNotFoundError`, `RenderTimeoutError`, and
`RenderError` retain command arguments, stderr diagnostics, and an exit code when
available. `InvalidOutputError` reports parsing failures without manifest excerpts.
Timeouts terminate Nyl and its renderer children: a process group on POSIX, or a
process tree through Windows' built-in `taskkill`.
`ResourceNotFoundError` and `AmbiguousResourceError` show resource identities.
Representations omit manifest bodies, and execution errors omit stdout; stderr
may still contain sensitive CLI diagnostics.

The client supports single-file rendering. Full-tree loading requires a documented
manifest inventory because publication trees contain ownership indexes and may
contain non-manifest state. The package does not download Nyl or run mutating CLI
commands. See the [Python package README](https://github.com/NiklasRosenstein/nyl/tree/develop/python)
for the API, compatibility, development commands, and Cargo-derived release versions.
