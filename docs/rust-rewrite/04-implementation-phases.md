# Implementation Phases

## Overview

The implementation is divided into 5 major phases, each building on the previous. This ensures incremental progress with testable milestones.

## Phase 0: Project Setup & Infrastructure

**Goal**: Establish project structure, CI/CD, and development environment

### Tasks

1. **Repository Setup**
   - Create new `nyl-rs` directory or branch
   - Initialize Cargo project
   - Set up module structure (cli, config, template, generator, etc.)
   - Create initial `Cargo.toml` with dependencies

2. **CI/CD Pipeline**
   - GitHub Actions workflow for:
     - Build and test on Linux/macOS/Windows
     - Clippy linting
     - Format checking with rustfmt
     - Security audit with cargo-audit
   - Set up Dependabot for dependency updates
   - Configure release automation with cargo-dist

3. **Development Tools**
   - Create `justfile` or `Makefile` for common tasks
   - Set up pre-commit hooks
   - Configure VSCode/RustRover settings
   - Create CONTRIBUTING.md

4. **Testing Infrastructure**
   - Set up test fixtures directory
   - Create integration test framework
   - Set up benchmark suite with criterion
   - Define performance baselines

5. **Documentation**
   - Initialize rustdoc comments
   - Create initial README.md
   - Set up changelog

**Deliverables**:
- ✅ Compilable empty project
- ✅ CI/CD passing
- ✅ Test infrastructure ready
- ✅ Documentation skeleton

**Duration**: 1-2 weeks (can be done in parallel with design)

---

## Phase 1: Configuration & CLI Foundation

**Goal**: Load configuration, parse CLI arguments, basic file I/O

### Tasks

1. **CLI Argument Parsing**
   - Implement `Cli` struct with clap
   - Implement `template`, `new`, `validate` command stubs
   - Add global flags (verbose, log-level)
   - Set up logging with tracing/tracing-subscriber

2. **Configuration Loading**
   - Implement `ProjectConfig` struct
   - Implement config file discovery (upward search)
   - Support YAML and JSON parsing
   - Add config validation

3. **File Discovery**
   - Implement manifest file discovery
   - Support glob patterns
   - Recursive directory traversal with walkdir

4. **New Command**
   - Implement project scaffolding
   - Generate default `nyl-project.yaml`
   - Create directory structure
   - Add example manifests (optional)

5. **Validate Command**
   - Implement config file validation
   - Check for schema errors
   - Print validation results

**Tests**:
- Unit tests for config parsing
- Integration tests for CLI commands
- Test config file discovery
- Test new project creation

**Deliverables**:
- ✅ `nyl new <name>` creates project
- ✅ `nyl validate` validates config
- ✅ Config loading works with YAML/JSON (using serde-norway)
- ✅ Logging configured and working

**Duration**: 2-3 weeks

---

## Phase 2: Template Engine

**Goal**: Implement Jinja2-compatible template rendering

### Tasks

1. **MiniJinja Integration**
   - Create `TemplateEngine` wrapper
   - Configure MiniJinja environment
   - Test basic template rendering

2. **Custom Functions**
   - Implement `random_password()`
   - Implement `randhex()`
   - Implement `b64encode()` / `b64decode()`
   - Implement `ref()` (placeholder)

3. **Custom Filters**
   - Implement common filters (if needed)
   - Test filter functionality

4. **YAML Template Rendering**
   - Implement recursive YAML template evaluation
   - Support `${{ expression }}` syntax
   - Parse multi-document YAML
   - Handle template errors gracefully

5. **Context Building**
   - Merge values from CLI args
   - Merge values from files
   - Environment variable expansion
   - Test context precedence

**Tests**:
- Unit tests for each template function
- Integration tests for YAML rendering
- Test multi-document YAML
- Test template error handling
- Test context merging

**Deliverables**:
- ✅ Template engine renders Jinja2 templates
- ✅ Custom functions working
- ✅ YAML with templates works
- ✅ Context from CLI and files

**Duration**: 2-3 weeks

---

## Phase 3: Basic Manifest Generation

**Goal**: Render simple manifests without Helm/generators

### Tasks

1. **Kubernetes Resource Types**
   - Define generic `Resource` struct
   - Define `Metadata` struct
   - Implement serialization/deserialization
   - Support common K8s types

2. **Manifest Loading**
   - Load YAML/JSON manifests
   - Parse into Resource structs
   - Handle multi-document files

3. **Render Command (Basic)**
   - Implement basic `nyl render` command flow:
     1. Load config
     2. Discover manifests
     3. Render templates
     4. Output YAML to stdout
   - No generators yet, just pass-through

4. **Diff and Apply Commands (Stubs)**
   - Implement `nyl diff` - pipe to kubectl diff
   - Implement `nyl apply` - pipe to kubectl apply
   - Shared rendering logic with render command

5. **Output Formatting**
   - YAML multi-document output
   - Proper formatting with serde-norway
   - Output to stdout or file
   - Pretty printing option

6. **Namespace Defaulting**
   - Backfill default namespace
   - Respect resource namespaced/cluster-scoped

**Tests**:
- Test manifest loading
- Test template rendering in manifests
- Test output formatting
- End-to-end integration tests

**Deliverables**:
- ✅ `nyl render` renders simple manifests to stdout
- ✅ `nyl diff` shows differences with cluster
- ✅ `nyl apply` applies manifests to cluster
- ✅ Templates in YAML work
- ✅ Output to stdout/file
- ✅ Basic manifests can be applied to cluster

**Duration**: 2-3 weeks

---

## Phase 4: Helm Chart Generation

**Goal**: Implement HelmChart resource and generator

### Tasks

1. **HelmChart Resource**
   - Define `HelmChart` struct matching Nyl spec
   - Support repository, OCI, local path
   - Support values and release configuration

2. **Helm Binary Integration**
   - Call `helm template` as subprocess
   - Parse helm output (YAML)
   - Handle errors from helm

3. **Chart Repository Support**
   - HTTP repository download
   - OCI repository download
   - Local path support

4. **Chart Caching**
   - Cache downloaded charts
   - Cache key: repo + name + version
   - Implement cache cleanup

5. **HelmChartGenerator**
   - Implement Generator trait
   - Integrate with dispatcher
   - Test generation

6. **Generator Dispatcher**
   - Implement generator registry
   - Route resources to generators
   - Support multiple generators

7. **Resource Reconciliation**
   - Implement recursive generation
   - Deduplication with stable hashing
   - Queue-based processing

**Tests**:
- Unit tests for HelmChartGenerator
- Test chart downloading
- Test caching
- Integration tests with real Helm charts
- Test reconciliation

**Deliverables**:
- ✅ HelmChart resources generate manifests
- ✅ Chart caching works
- ✅ Reconciliation deduplicates
- ✅ Multi-level generation works

**Duration**: 3-4 weeks

---

## Phase 5: Polish & Release Preparation

**Goal**: Optimize, document, and prepare for release

### Tasks

1. **Performance Optimization**
   - Profile with cargo-flamegraph
   - Optimize hot paths
   - Add parallel processing where beneficial
   - Reduce allocations

2. **Error Messages**
   - Improve error messages
   - Add context to errors
   - Pretty-print errors

3. **Documentation**
   - Complete API documentation (rustdoc)
   - Write user guide
   - Create examples directory
   - Write migration guide from Python

4. **Testing**
   - Achieve >80% code coverage
   - Add more integration tests
   - Test edge cases
   - Cross-platform testing

5. **Benchmarking**
   - Compare with Python version
   - Document performance improvements
   - Create benchmark suite

6. **Binary Distribution**
   - Set up cargo-dist
   - Build for Linux/macOS/Windows
   - Create release artifacts
   - Test installation

7. **Final Touches**
   - Update README
   - Create CHANGELOG
   - Tag v0.1.0-rust release

**Tests**:
- Comprehensive integration test suite
- Performance regression tests
- Installation tests

**Deliverables**:
- ✅ Optimized binary (<20MB)
- ✅ Complete documentation
- ✅ 5-10x faster than Python
- ✅ Ready for beta release

**Duration**: 2-3 weeks

---

## Phase 6+ (Future): Advanced Features

These features are deferred to post-MVP releases:

### Phase 6: Secrets Management (v0.2.0)
- SOPS integration
- Environment variable provider
- Kubernetes secret provider
- Secret injection in templates

### Phase 7: Profile Management (v0.3.0)
- Profile configuration
- Kubeconfig management
- SSH tunnel support
- Profile-specific values

### Phase 8: ApplySet & kubectl Integration (v0.4.0)
- ApplySet resource generation
- kubectl apply integration
- kubectl diff support
- Pruning support

### Phase 9: Post-Processing (v0.5.0)
- Kyverno policy integration
- Custom post-processors
- Mutation and validation

### Phase 10: ArgoCD Integration (v0.6.0)
- ConfigManagementPlugin support
- ArgoCD environment variable handling
- ArgoCD-specific features

---

## Milestones Summary

| Phase | Duration | Deliverable |
|-------|----------|-------------|
| 0 | 1-2 weeks | Project setup complete |
| 1 | 2-3 weeks | CLI and config working |
| 2 | 2-3 weeks | Template engine complete |
| 3 | 2-3 weeks | Basic manifest generation |
| 4 | 3-4 weeks | Helm chart support |
| 5 | 2-3 weeks | v0.1.0-rust release |
| **Total** | **12-18 weeks** | **Production-ready MVP** |

---

## Parallel Work Opportunities

Some tasks can be done in parallel:

**Phase 0 + Phase 1**: Setup and CLI can overlap
**Phase 2 + Phase 3**: Template engine and resource types can be developed concurrently
**Testing**: Can be ongoing throughout all phases

---

## Risk Mitigation

1. **Helm Integration Complexity**
   - Prototype early in Phase 4
   - Test with variety of charts
   - Consider helm-sys bindings if subprocess is problematic

2. **Performance Goals**
   - Continuous benchmarking from Phase 3 onwards
   - Profile early and often
   - May need optimization phase between Phase 4 and 5

3. **Feature Creep**
   - Strict adherence to phase scope
   - Defer non-essential features to post-MVP
   - Regular scope reviews

---

## Success Criteria

Each phase must meet these criteria before moving to next:

1. **Code Quality**
   - All tests passing
   - No clippy warnings
   - Code formatted with rustfmt

2. **Documentation**
   - Public APIs documented
   - Examples provided
   - CHANGELOG updated

3. **Performance**
   - Benchmarks show no regression
   - Memory usage within targets

4. **Review**
   - Code review completed
   - Design review for significant changes
