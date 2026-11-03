# Nyl Python client

`nyl` is a small Python 3.11+ library for rendering and inspecting Kubernetes
manifests. Install it with `uv add nyl` or `pip install nyl`.

Install the [Nyl CLI](https://niklasrosenstein.github.io/nyl/start/getting-started/)
separately and put it on `PATH`, or pass its executable path. Installing and
importing this package does not require the CLI, Rust, or pytest.

```python
from nyl import NylClient

client = NylClient(project="/path/to/infrastructure", executable="nyl", timeout=120)
manifests = client.render("applications/workloads/web.yaml", target="development")
deployment = manifests.one("Deployment", "web")

assert deployment.namespace == "web"
for container in deployment.containers(include_init=True):
    assert container.security_context["allowPrivilegeEscalation"] is False
    assert container.env.value("PGHOST") == "database-rw"
    assert container.env.secret_ref("PGPASSWORD") == ("database-app", "password")
```

Use this code in ordinary Python or in your own tests; no pytest plugin is needed.
`namespace` is the authored namespace. Resources without one return `None`,
even when the Release or Argo CD Application has a destination namespace.

## Rendering

`NylClient(project, executable="nyl", timeout=120)` fixes the invocation directory
and requires a positive, finite timeout in seconds. `render(source, *, target=None,
offline=False, allow_secret_inputs=False)` invokes only `nyl render`. Relative
source paths are relative to `project`; absolute paths are passed through.
Calls leave Python's working directory and environment unchanged and use no shell.
Timeouts terminate the invocation's process group on POSIX and its process tree
with Windows' built-in `taskkill`; diagnostics are collected after termination.

Rust owns configuration resolution, validation, rendering, and caching. Target-aware
Releases use the compiled target's rendering rules. Each call runs the CLI again;
Nyl's cache may reuse valid artifacts. Project-configured validation remains enabled
according to the CLI's defaults. A validation failure yields an exception even if
the CLI emits manifests. `offline=True` forwards `--offline` (cached Git references
and publication inputs); it is not a universal network prohibition for validators
or other external tools. `allow_secret_inputs=True` explicitly forwards the CLI's
opt-in for project secrets and admitted environment inputs.

Only single-file render entry points are supported. Full-tree loading needs a
documented manifest inventory: a published tree can also contain ownership indexes
and non-manifest state. This package neither downloads the executable nor applies,
publishes, or reconciles resources, and has no persistent Python cache.

## Inspecting declarations

`ManifestSet` is iterable, has a length, and preserves document order.
`select(kind=None, name=None, *, namespace=..., api_version=None)` returns another
`ManifestSet`. `one(kind, name=None, *, namespace=..., api_version=None)` requires
exactly one match. Omit `namespace` to search all namespaces; explicitly pass
`namespace=None` to select resources with no declared namespace. Selection errors
show resource identities without manifest bodies. `ManifestSet.from_yaml(text)`
safely reads multi-document YAML 1.2, ignores empty documents, and rejects invalid
resource envelopes. Nyl's emitted quoting preserves strings such as `"on"`, `"yes"`,
and `"001"`; no YAML 1.1 boolean coercion is applied by Python.

Resources expose `raw`, `api_version`, `kind`, `name`, and `namespace`.
`containers(include_init=False)` handles Pods, Deployments, StatefulSets, DaemonSets,
ReplicaSets, ReplicationControllers, Jobs, and CronJobs. It yields regular containers
in order, followed by init containers when requested. Unsupported kinds raise
`DeclarationError`.

Containers expose `raw`, `name`, `security_context` (an empty dictionary when
omitted), and `env`. `env.value(name)` requires an explicit literal string;
`env.secret_ref(name)` requires one Secret key reference and returns `(name, key)`.
Missing, duplicate, malformed, and incompatible declarations raise `DeclarationError`.
These helpers do not resolve Secrets, ConfigMaps, `envFrom`, Pod-level security
context inheritance, or live cluster data. Raw dictionaries remain mutable.

## Errors and compatibility

All library errors derive from `NylError` and are exported from `nyl`:

| Error | Meaning |
| --- | --- |
| `ExecutableNotFoundError` | Install the CLI or correct its executable path |
| `RenderTimeoutError` | The subprocess exceeded its deadline |
| `RenderError` | A nonzero exit status or an OS execution error |
| `InvalidOutputError` | stdout is not valid rendered resource YAML |
| `ResourceNotFoundError` | `one()` found zero matches |
| `AmbiguousResourceError` | `one()` found several matches |
| `DeclarationError` | A helper cannot interpret the declaration |

Execution errors derive from `ExecutionError` and retain `command` (an argument
tuple), `exit_code` (`None` when unavailable), and `diagnostics` (stderr).
They do not retain or automatically print stdout. YAML errors omit input excerpts;
resource and container representations include only identities. CLI diagnostics
can contain sensitive information and should be handled accordingly.

Compatibility is tested against a binary built from the same checkout, currently
CLI **0.5.9**. Version strings alone do not establish compatibility with independently
built binaries. CI runs the synthetic render without Kubernetes, remote charts, or
production credentials. The package's version is available through
`importlib.metadata.version("nyl")`.

## Development and releases

From the repository root:

```bash
mise run python-check  # Ruff, type checking, parallel tests, checkout-built CLI
mise run python-build # Stamp Cargo-derived version, then build wheel and sdist
mise run pre-commit   # Required Rust and Python checks
```

Or inside `python/`:

```bash
uv sync --locked
uv run python scripts/version.py
uv run ruff check .
uv run ruff format --check .
uv run mypy
uv run pytest
uv build
```

The test fixture builds the CLI and reads Cargo's executable artifact, honoring
Cargo's configured target and output directory. Set `NYL_TEST_EXECUTABLE` to use
a binary you built separately.

`scripts/version.py` derives the release version from root `Cargo.toml` and stamps
`src/nyl/_version.py` before building. For CLI `0.MINOR.PATCH`, Python uses
`0.2MINOR.PATCH` (concatenation): `0.5.9` → `0.25.9`, `0.10.3` → `0.210.3`.
From CLI 1.x, versions match. Strict SemVer prereleases `alpha.N`, `beta.N`, and
`rc.N` map to PEP 440 `aN`, `bN`, and `rcN`. Other prereleases and build metadata
are rejected. The stamped file is build metadata, not a separately maintained
release counter. Stamp after synchronizing the development environment;
`uv run --no-sync` can use that environment when preparing a newer native release.
The source distribution includes its stamped version and builds without the Rust
repository, Cargo, or Nyl.

The handwritten `.github/workflows/python.yaml` tests and builds artifacts on
relevant pull requests, `develop` changes, and native `v*` tags. The tag must match
the Cargo version. Its publish job consumes the tested artifacts and uses PyPI
Trusted Publishing from the `pypi` environment for `NiklasRosenstein/nyl`.
Release through the repository's native release command (`scripts/release.sh X.Y.Z`);
no separate Python counter or PyPI API token is needed.
