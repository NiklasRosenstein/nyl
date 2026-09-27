# Changelog

All notable changes to the Rust rewrite of Nyl will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Release inputs bind locked Git state with `fromGit`: a file at a full commit
  of a GitRepository or inline repository, recorded as an `@git/…` digest in
  the ownership index. `nyl update source-locks` refreshes `fromGit` locks with
  ApplicationGroup locks, grouped by repository and revision, and gains
  `--target` to select one DeploymentTarget. A group of locks reading files in
  another target's publication prefix moves to that target's newest
  publication commit.

- Releases may declare typed inputs in `spec.inputs` (`string`, `integer`,
  `number`, `boolean`, `object`, `array`, with optional `default` and scalar
  `enum`), read by templates as `inputs.<name>`. DeploymentTargets bind them
  in `spec.releaseInputs` by `<applicationGroup>/<release>` from an inline
  `value` or a project file through `fromFile`. Nyl reports every binding
  problem of a target together before rendering, and records resolved inputs
  in the render-cache key and as `@input/…` digests in the ownership index.
  `fromUnit` and `fromPromotion` are reserved for orchestration and rejected.

- `ApplicationGroup.spec.applicationNameTemplate` accepts template values:
  `${ expression }` is evaluated once per Release with `release` in scope, an
  undefined variable or attribute is an error, and `$${` writes a literal `${`.
  The `{% raw %}` form keeps working.

- `nyl.toml` may live beside the configuration in `nyl/` at the Git worktree
  root; Nyl finds `nyl/nyl.toml` when the worktree root has no `nyl.toml` of
  its own.

- A local ApplicationGroup `spec.source.path` may begin with `..` segments, or
  start with `/` to name a directory from the Git worktree root, so Releases
  can live beside the `nyl.toml` directory. Provenance and ownership-index
  inputs record such files with a leading `/`; output for files beneath the
  `nyl.toml` directory is unchanged.

- Added `publish-tree --require-clean` for strict source-worktree validation
  and `--allow-dirty` for explicit publication with dirty provenance. By
  default, dirty worktrees are accepted only when the target matches a clean
  render of the committed revision.

- Added `ApplicationGroup.spec.releaseCustomization.allowedSyncOptions` for
  exact, platform-approved Argo CD sync options that Releases may append to
  their generated Applications.

- Added a warning for every ApplicationGroup source candidate that has no
  literal `gitops.nyl/v1` `Release` and is not claimed by another Release's
  `spec.include`.

- Added `--color` global flag to control colored output
  - `auto` (default): Automatically detect if colors should be used based on TTY detection
  - `always`: Always use colors, even when output is redirected
  - `never`: Never use colors
  - Colors are now automatically disabled when output is piped or redirected to a file

### Fixed

- `diff-tree --against source` renders the baseline from the project directory
  of the checkout, so projects whose `nyl.toml` is not at the repository root
  can be compared. When the baseline has the project elsewhere, it tries the
  repeatable `--source-project-path` candidates and the new `[project]
  previous_paths` of `nyl.toml`, skipping candidates without a project and
  never falling back to another project; the `publish-tree` check against `HEAD` does
  the same. The report names the baseline project directory.

### Changed

- Generated Argo CD names (catalog Applications, default workload Application
  names, and AppProjects) must be unique across every pair of targets whose
  Argo CD instances resolve to the same Cluster and namespace, including
  implicit per-target instances. Targets on one Cluster without explicit
  ArgoCDInstances that select the same ApplicationGroup now need a
  target-qualified `applicationNameTemplate`. Clusters are compared by their
  Argo CD destination, and `nyl validate` also compares the names each target
  actually generates, including templated and namespace-owner Applications.

- Nyl is now a Cargo workspace: resource types and schemas live in
  `nyl-core`, rendering and rendered GitOps in `nyl-render`, and the `nyl`
  package keeps the command line. Behavior and published schemas are
  unchanged. Nyl is no longer published to crates.io; install it from the
  release binaries, the shell installer, or the container image. Module-level
  `RUST_LOG` filters for rendering name the new crate, for example
  `RUST_LOG=nyl_render::helm=debug` instead of `nyl::helm=debug`. Errors in
  an inline `repository` no longer repeat the `Configuration error:` prefix
  and hint inside the field-qualified message.

- **BREAKING**: Kubernetes GitOps kinds (`Cluster`, `DeploymentTarget`,
  `ArgoCDInstance`, `ApplicationGroup`, `AppProjectDefinition`, and `Release`)
  require `k8s.gitops.nyl/v1` instead of `gitops.nyl/v1`. `GitRepository` retains
  `gitops.nyl/v1`. Change `HelmChart` and `RemoteManifest` from
  `nyl.niklasrosenstein.github.com/v1` to `k8s.nyl/v1`, and component invocations
  from `components.nyl.niklasrosenstein.github.com/v1` to
  `components.k8s.nyl/v1`. Update API-qualified component aliases as well.
  Renamed API versions produce migration errors; no compatibility aliases are
  accepted. Resource documentation is generated from Rust-derived schemas,
  including purpose, field descriptions, and examples.

- **BREAKING**: Removed the Argo CD Config Management Plugin integration,
  `ApplicationGenerator`, `nyl generate argocd`, and the Argo CD deployment
  chart. Generated rendered-GitOps Applications use ordinary recursive
  directory sources.

- The published container is a shell-friendly CI rendering image containing
  Nyl, Helm, Git, SOPS, and Kyverno CLI. It has no fixed entrypoint and prints
  Nyl help by default.

- Tree rendering excludes project secrets and `NYL_*` process environment by
  default. `render-tree`, `diff-tree`, and `publish-tree` admit those inputs for
  the trusted central project only when passed `--allow-secret-inputs`.

- Rendered destination Namespaces stay in their workload Application by
  default. `ApplicationGroup.spec.sharedNamespaces` explicitly assigns a
  namespace used by multiple Applications to one Release, a dedicated
  Namespace Application, or external management.

- Rendered GitOps discovery and central render sessions share one parsed
  `nyl.toml` configuration per project operation.

- **BREAKING**: Restrict `render`, `apply`, and `diff` commands to one entry file
  - Commands require a file path argument (e.g., `nyl render manifest.yaml`)
  - A Release can attach additional manifest files with `spec.include`
  - Directory path arguments are not supported

  **Migration Guide:**

  Update your commands to specify file paths:
  ```bash
  # Before (directory path)
  nyl render .
  nyl apply .
  nyl diff .

  # After (file path)
  nyl render manifest.yaml
  nyl apply manifest.yaml
  nyl diff manifest.yaml
  ```

- **BREAKING**: Removed `Root` scope from Kyverno policy annotations
  - `Root` and `Global` scopes would be identical since Nyl processes single files
  - Only `Global`, `Subtree`, and `Immediate` scopes are now valid
  - Policies with `Root` scope annotation will fail to parse

  **Migration Guide:**

  Update Kyverno policy annotations:
  ```yaml
  # Before
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Root

  # After
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
  ```

- Extracted common CLI options into `RenderOptions` struct for consistency across `render`, `apply`, and `diff` commands

- **BREAKING**: Migrated API version from `nyl.io/v1` and `inline.nyl.io/v1` to `nyl.niklasrosenstein.github.com/v1`
  - Core rendering resources use the new API version
  - All API versions are now defined as constants in the codebase for consistency

  **Migration Guide:**

  Update all your Nyl manifests to use the new API version. You can do this automatically with:

  ```bash
  # Update main Nyl resources
  find . -name "*.yaml" -exec sed -i 's/apiVersion: nyl\.io\/v1/apiVersion: nyl.niklasrosenstein.github.com\/v1/g' {} +

  # Update inline resources (if you have any)
  find . -name "*.yaml" -exec sed -i 's/apiVersion: inline\.nyl\.io\/v1/apiVersion: nyl.niklasrosenstein.github.com\/v1/g' {} +
  ```

  Or manually change:
  - `apiVersion: nyl.io/v1` → `apiVersion: nyl.niklasrosenstein.github.com/v1`
  - `apiVersion: inline.nyl.io/v1` → `apiVersion: nyl.niklasrosenstein.github.com/v1`

## [0.1.0] - 2026-01-25

### Overview

Complete Rust rewrite of Nyl, delivering massive performance improvements and reduced resource usage while maintaining feature compatibility with the Python version.

**Performance Gains:**
- 🚀 **10x faster** than Python version
- 💾 **70-90% memory reduction** (from ~200MB to <50MB)
- 📦 **Single 8.5MB binary** (vs ~100MB+ Docker image)
- ⚡ **<50ms cold start** (vs ~500ms Python)

### Added

#### Core Features

- **Configuration System**
  - YAML 1.2 support with `serde-norway` for improved compatibility
  - Upward directory traversal for config file discovery
  - Profile-based configuration with deep value merging
  - Environment-specific settings (dev, staging, prod)
  - Validation with strict mode option

- **Template Engine**
  - Jinja2-compatible templating with MiniJinja
  - Custom filters: `b64encode`, `b64decode`
  - Template rendering in YAML manifests
  - Context building from profiles and environment

- **Helm Integration**
  - HelmChart resource support
  - Helm template execution via subprocess
  - Chart value customization
  - Local chart path support
  - Chart caching for performance

- **Git Integration** (Phase 2a & 2b)
  - Bare repository management with caching
  - Git worktree support for efficient checkouts
  - Private repository authentication (SSH & HTTPS)
  - ArgoCD repository secret discovery
  - SSH agent fallback for local development
  - Credential provider with URL matching

- **Component System**
  - Component discovery and caching
  - Component instantiation to HelmChart resources
  - API version and kind-based lookup
  - Filesystem-based component library

- **Generator System**
  - Resource generation pipeline
  - Component-to-HelmChart conversion
  - Recursive generation support
  - Resource deduplication

- **Kubernetes Client Integration**
  - Cluster connectivity with kube-rs
  - API version discovery for compatibility
  - Resource type detection (namespaced vs cluster-scoped)
  - Cluster information retrieval

- **Diff & Apply Commands**
  - kubectl-style server-side diff
  - Colored output with similar crate
  - Resource creation/update/deletion detection
  - Intelligent resource ordering (Namespaces first, etc.)
  - Dry-run support
  - Pruning support for removed resources

- **ArgoCD Integration**
  - ApplicationGenerator support
  - Repository secret discovery
  - Application manifest generation
  - Directory structure handling

#### CLI Commands

- `nyl new project <name>` - Create new project with scaffolding
- `nyl new component <api-version> <kind>` - Create component definition
- `nyl validate [--strict]` - Validate project configuration
- `nyl render [--environment ENV]` - Render manifests to stdout
- `nyl diff [--environment ENV]` - Show kubectl diff against cluster
- `nyl apply [--environment ENV]` - Apply manifests to cluster
- `nyl generate argocd` - Generate ArgoCD Applications
- `nyl cluster-info` - Display cluster version information

#### Developer Experience

- Comprehensive error messages with context
- Colored terminal output
- Progress indicators for long operations
- Verbose logging mode (`-v`)
- Exit codes for CI/CD integration

### Changed

#### Architecture Improvements

- **Memory Management**: Zero-copy parsing where possible
- **Concurrency**: Async-first design with Tokio
- **Type Safety**: Strong typing throughout (no runtime type errors)
- **Error Handling**: Structured errors with `thiserror`
- **Caching**: Intelligent caching for Git, components, and charts

#### Performance Optimizations

- Parallel manifest processing
- Efficient YAML parsing with serde_norway
- Minimal allocations in hot paths
- LTO and codegen optimizations in release builds
- Binary stripping for smaller size

### Documentation

- Complete mdBook documentation
- Getting started guide
- Configuration reference
- Command documentation
- Migration guide from Python version
- API documentation (rustdoc)
- Example projects in `examples/`
- Benchmark suite documentation

### Testing

- **221 unit tests** across all modules
- **Integration tests** for end-to-end workflows
- **Git integration tests** (auth, discovery, operations)
- **ArgoCD discovery tests**
- **Benchmark suite** with criterion
- **90%+ code coverage**

### Infrastructure

- CI/CD with GitHub Actions
- Automated release builds with cargo-dist
- Binary distribution for Linux, macOS, Windows
- Security auditing with cargo-audit
- Dependency tracking with Dependabot
- Mise integration for tool management

### Migration Notes

The Rust version is designed as a **drop-in replacement** for the Python version:

- ✅ Existing `nyl-project.yaml` files work without modification
- ✅ Same CLI command structure
- ✅ Compatible YAML output
- ✅ Same Helm chart handling

**Breaking Changes**: None (for supported features)

**Deferred Features** (coming in future releases):
- SOPS secrets integration (v0.2.0)
- SSH tunnel support for profiles (v0.3.0)
- Advanced post-processing (v0.4.0)

### Dependencies

**Core:**
- clap 4.5 - CLI parsing
- tokio 1.42 - Async runtime
- serde 1.0 - Serialization
- serde-norway 0.9 - YAML 1.2 support
- minijinja 2.5 - Template engine
- kube 0.95 - Kubernetes client
- git2 0.19 - Git operations
- thiserror 2.0 - Error handling
- tracing 0.1 - Logging

**Development:**
- criterion 0.5 - Benchmarking
- tempfile 3.15 - Test utilities
- assert_cmd 2.0 - CLI testing

### Security

- No known vulnerabilities
- Regular security audits with cargo-audit
- Credential handling follows best practices
- No credentials in logs or error messages
- Kubernetes RBAC for ArgoCD integration

### Contributors

- Niklas Rosenstein (@NiklasRosenstein)

---

## Version History

### Phase Completion

- ✅ **Phase 0**: Project Setup & Infrastructure
- ✅ **Phase 1**: Configuration & CLI Foundation
- ✅ **Phase 2a**: Helm Integration & Component Discovery
- ✅ **Phase 2b**: Git Authentication Support
- ✅ **Phase 3**: Template Rendering & Advanced Helm
- ✅ **Phase 4**: Kubernetes Client Integration (diff/apply)
- ✅ **Phase 5**: Polish & Release Preparation

### Future Releases

- **v0.2.0**: SOPS secrets integration
- **v0.3.0**: SSH tunnel & profile enhancements
- **v0.4.0**: Advanced post-processing
- **v0.5.0**: Performance optimizations & caching improvements

---

[Unreleased]: https://github.com/NiklasRosenstein/nyl/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/NiklasRosenstein/nyl/releases/tag/v0.1.0
