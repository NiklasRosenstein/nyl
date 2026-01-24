# Migration Strategy

## Overview

This document outlines the strategy for migrating from Python Nyl to Rust Nyl, including compatibility considerations, migration tooling, and adoption path.

## Migration Philosophy

### Clean Slate Approach
- **No backward compatibility guarantee** for configuration format
- Opportunity to simplify and improve the API
- Migration guide and tooling provided
- Side-by-side installation supported during transition

### Versioning Strategy
- Python version: Continue as `nyl` (v0.10.x → v0.11.x)
- Rust version: Released as `nyl-rs` initially (v0.1.0-rust)
- After maturity: Rust becomes `nyl` (v1.0.0), Python archived

## Configuration Changes

### Simplified Configuration

**Python (nyl-project.yaml)**:
```yaml
settings:
  generate_applysets: true
  on_lookup_failure: CreatePlaceholder
  components_path: components
  search_path: [.]
profiles:
  default:
    # ... complex profile config
secrets:
  default:
    # ... secrets provider config
```

**Rust (nyl-project.yaml)** - MVP version:
```yaml
settings:
  componentsPath: components  # camelCase for consistency
  searchPath: [.]

# profiles and secrets deferred to later versions
```

### Breaking Changes

| Feature | Python | Rust MVP | Rust Future |
|---------|--------|----------|-------------|
| Config format | YAML/TOML/JSON | YAML/JSON | YAML/JSON/TOML |
| Field naming | snake_case | camelCase | camelCase |
| Profiles | Supported | Not supported | v0.3.0+ |
| Secrets | Supported | Not supported | v0.2.0+ |
| ApplySets | Supported | Not supported | v0.4.0+ |
| Post-processing | Supported | Not supported | v0.5.0+ |
| Lookup failures | Configurable | Error only | v0.2.0+ |

### Compatibility Matrix

| Python Feature | Rust MVP Support | Workaround |
|----------------|------------------|------------|
| Template rendering | ✅ Full | N/A |
| Helm charts | ✅ Full | N/A |
| Components | ✅ Basic | Advanced features later |
| Custom functions | ⚠️ Partial | Core functions only |
| Secrets | ❌ None | Use env vars for now |
| Profiles | ❌ None | Use manual kubeconfig |
| SSH tunnels | ❌ None | Set up manually |
| kubectl apply | ❌ None | Pipe to kubectl |
| ApplySets | ❌ None | Manual management |
| lookup() | ❌ None | Placeholder refs |

## Migration Tooling

### Config Converter

Provide a conversion tool to migrate configs:

```bash
# Convert Python config to Rust config
nyl-migrate-config nyl-project.yaml > nyl-project-rust.yaml
```

**Implementation**:
```rust
// src/migrate/config.rs

pub fn convert_config(python_config: &str) -> Result<String> {
    // 1. Parse Python config
    let py_config: PythonConfig = serde_yaml::from_str(python_config)?;

    // 2. Convert to Rust config
    let rust_config = RustConfig {
        settings: Settings {
            components_path: py_config.settings.components_path,
            search_path: py_config.settings.search_path,
        },
    };

    // 3. Add warnings for unsupported features
    let mut warnings = Vec::new();
    if py_config.profiles.is_some() {
        warnings.push("# WARNING: Profiles not yet supported in Rust version");
    }
    if py_config.secrets.is_some() {
        warnings.push("# WARNING: Secrets not yet supported, use environment variables");
    }

    // 4. Serialize to YAML with warnings
    let mut output = warnings.join("\n");
    output.push_str("\n\n");
    output.push_str(&serde_yaml::to_string(&rust_config)?);

    Ok(output)
}
```

### Manifest Validator

Validate manifests work with both versions:

```bash
# Check if manifest is compatible with Rust version
nyl-rs validate manifests/*.yaml
```

**Checks**:
- ✅ HelmChart syntax supported
- ⚠️ Template functions used (warn if unsupported)
- ❌ Features not yet implemented (error)

### Side-by-Side Comparison

Tool to compare output from both versions:

```bash
# Compare outputs
nyl template manifests/*.yaml > python-output.yaml
nyl-rs template manifests/*.yaml > rust-output.yaml
diff -u python-output.yaml rust-output.yaml
```

## Adoption Path

### Phase 1: Experimentation (v0.1.0-rust)
**Target**: Early adopters, simple use cases

**Capabilities**:
- Basic template rendering
- Helm chart generation
- Simple projects without secrets/profiles

**Adoption**:
- Install alongside Python version: `cargo install nyl-rs`
- Binary named `nyl-rs` to avoid conflicts
- Documentation clearly marks MVP limitations

**User Journey**:
1. Install nyl-rs
2. Run `nyl-migrate-config` on existing project
3. Test with `nyl-rs template`
4. Compare output with Python version
5. Report issues/feedback

### Phase 2: Feature Parity (v0.2.0 - v0.6.0)
**Target**: Broader adoption as features mature

**Capabilities**:
- Secrets management (v0.2.0)
- Profiles and SSH (v0.3.0)
- kubectl integration (v0.4.0)
- Post-processing (v0.5.0)
- ArgoCD support (v0.6.0)

**Adoption**:
- Still side-by-side installation
- More users migrating critical workloads
- Python version in maintenance mode

### Phase 3: Full Migration (v1.0.0)
**Target**: Complete migration

**Capabilities**:
- Feature parity achieved
- Production-proven stability
- Comprehensive documentation
- Migration guide finalized

**Adoption**:
- Rust version becomes default `nyl` binary
- Python version archived/deprecated
- Homebrew/package manager defaults to Rust version

## Migration Guide (User-Facing)

### For Simple Projects

**Before (Python)**:
```bash
pip install nyl
nyl template manifests/*.yaml
```

**After (Rust)**:
```bash
cargo install nyl-rs
nyl-migrate-config nyl-project.yaml > nyl-project-rust.yaml
nyl-rs template manifests/*.yaml
```

### For Projects with Secrets

**Before (Python)**:
```yaml
# nyl-secrets.yaml
default:
  type: sops
  path: secrets.yaml
```

```yaml
# manifest.yaml
stringData:
  password: ${{ secrets.db_password }}
```

**After (Rust MVP - workaround)**:
```bash
# Export secrets as env vars
export NYL_DB_PASSWORD=$(sops -d secrets.yaml | yq .db_password)
```

```yaml
# manifest.yaml
stringData:
  password: ${{ env.NYL_DB_PASSWORD }}
```

**After (Rust v0.2.0+)**:
```yaml
# nyl-project.yaml
secrets:
  providers:
    - type: sops
      path: secrets.yaml
```

### For Projects with Profiles

**Before (Python)**:
```bash
nyl profile activate staging
nyl template manifests/*.yaml --apply
```

**After (Rust MVP - workaround)**:
```bash
# Manually set kubeconfig
export KUBECONFIG=~/.kube/staging-config
nyl-rs template manifests/*.yaml | kubectl apply -f -
```

**After (Rust v0.3.0+)**:
```bash
nyl-rs profile activate staging
nyl-rs template manifests/*.yaml --apply
```

## Deprecation Timeline

### Python Version Support

| Version | Status | Support Level | Timeline |
|---------|--------|---------------|----------|
| v0.10.x | Current | Full support | Ongoing |
| v0.11.x | Maintenance | Bug fixes only | After Rust v0.1.0 |
| v0.12.x | Deprecated | Security only | After Rust v1.0.0 |
| v1.x | End of Life | None | 6 months after Rust v1.0.0 |

### Communication Plan

1. **v0.1.0-rust Release**:
   - Blog post announcing Rust rewrite
   - Clear MVP limitations
   - Invitation for early adopters
   - Side-by-side installation guide

2. **Feature Parity Releases (v0.2.0+)**:
   - Release notes for each feature
   - Migration examples
   - Performance comparisons

3. **v1.0.0 Release**:
   - Major announcement
   - Complete migration guide
   - Deprecation notice for Python version
   - Support timeline

4. **Python EOL**:
   - 6-month notice before EOL
   - Final release with deprecation warnings
   - Archive repository

## Rollback Plan

If Rust rewrite encounters critical issues:

1. **Continue Python development** in parallel
2. **Delay migration** until issues resolved
3. **Community feedback** drives priorities
4. **No forced migration** - users choose when ready

## Success Metrics

### Technical Metrics
- ✅ 90%+ feature parity with Python
- ✅ 5-10x performance improvement
- ✅ <20MB binary size
- ✅ 80%+ test coverage

### Adoption Metrics
- 20% users trying Rust version within 3 months
- 50% users migrated within 6 months of v1.0.0
- 80% users migrated within 12 months of v1.0.0

### Quality Metrics
- <10 critical bugs per release
- <5 days average time to fix critical bugs
- >90% user satisfaction in surveys

## Documentation Updates

### Required Documentation

1. **Migration Guide** (this document)
2. **API Comparison Table** (Python vs Rust)
3. **Feature Roadmap** (what's coming when)
4. **FAQ** (common migration questions)
5. **Troubleshooting Guide** (common issues)
6. **Performance Guide** (optimization tips)

### Examples Repository

Create `nyl-examples` repository with:
- Simple manifest examples
- Helm chart examples
- Complex multi-component projects
- Both Python and Rust versions maintained

## Community Engagement

### Feedback Channels
- GitHub Discussions for questions
- GitHub Issues for bugs
- Discord/Slack for real-time help
- Monthly community calls

### Beta Testing Program
- Recruit early adopters
- Provide dedicated support
- Fast-track feature requests
- Recognition for contributors

## Risks and Mitigations

### Risk: Slow Adoption
**Mitigation**:
- Clear performance benefits
- Excellent documentation
- Smooth migration path
- Community support

### Risk: Missing Features
**Mitigation**:
- Clear roadmap
- Workarounds documented
- Fast feature development
- Priority based on feedback

### Risk: Incompatibilities
**Mitigation**:
- Comprehensive testing
- Migration validation tools
- Side-by-side comparison
- Quick bug fixes

### Risk: Performance Regressions
**Mitigation**:
- Continuous benchmarking
- Performance regression tests
- Optimization priority
- Transparent metrics

## Conclusion

The migration from Python to Rust is a significant undertaking but offers substantial benefits. By taking a clean slate approach with clear migration tooling and documentation, we can ensure a smooth transition while delivering a superior product.
