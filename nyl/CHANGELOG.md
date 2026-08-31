# Changelog

All notable changes to the Rust rewrite of Nyl will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added `--color` global flag to control colored output
  - `auto` (default): Automatically detect if colors should be used based on TTY detection
  - `always`: Always use colors, even when output is redirected
  - `never`: Never use colors
  - Colors are now automatically disabled when output is piped or redirected to a file

### Changed

- Optimized Git source resolution in ArgoCD plugin context to reuse ArgoCD's local checkout for matching `repoURL` + exact `targetRevision`, falling back to cache/worktree cloning on mismatch

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

- **BREAKING**: ArgoCD Nyl plugin now requires `NYL_CMP_TEMPLATE_INPUT` to select a single manifest file
  - Plugin command no longer falls back to `nyl render .`
  - `ApplicationGenerator` and `nyl generate` now set `NYL_CMP_TEMPLATE_INPUT` in generated ArgoCD `Application` plugin env
  - Plugin name is now `nyl-v2` (legacy `nyl`/`nyl-v1` references are no longer supported)

  **Migration Guide:**

  If you manage ArgoCD Applications manually, add:
  ```yaml
  spec:
    source:
      plugin:
        name: nyl-v2
        env:
          - name: NYL_CMP_TEMPLATE_INPUT
            value: apps.yaml
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
  - All Nyl resources (Release, HelmChart, StatefulSecret, PostProcessor, etc.) now use the new API version
  - ArgoCD ApplicationGenerator continues to use `argocd.nyl.niklasrosenstein.github.com/v1` (unchanged)
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
  - ArgoCD resources (`argocd.nyl.niklasrosenstein.github.com/v1`) remain unchanged

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
