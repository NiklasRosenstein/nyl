# Development Instructions for Nyl

This file provides guidelines for AI assistants (Claude, GitHub Copilot) working on the Nyl project.

## Project Overview

Nyl is a fast Kubernetes manifest generator written in Rust. It supports:
- Jinja2-compatible templating with MiniJinja
- Helm chart integration
- Multi-environment configurations (profiles)
- Git repository support with authentication
- Kubernetes client integration (kubectl diff/apply)
- ArgoCD Application generation

**Primary Language:** Rust 1.93.0+  
**Build System:** Cargo  
**Task Runner:** mise  
**Documentation:** mdbook  

## Repository Structure

```
/
├── nyl/               # Main Rust crate (the nyl tool)
│   ├── src/          # Source code
│   ├── tests/        # Integration tests
│   ├── benches/      # Benchmarks
│   ├── book/         # mdbook documentation
│   └── examples/     # Example projects
├── docker/           # Docker image for ArgoCD CMP
├── chart/            # Helm chart for ArgoCD
├── examples/         # Top-level examples
└── .github/          # CI/CD workflows
```

## Development Workflow

### Prerequisites
- Rust 1.93.0 or newer
- kubectl (for diff/apply commands)
- Helm (for Helm chart rendering)
- mise (recommended for task management)

### Essential Commands

```bash
# Format code (REQUIRED before commit)
mise run fmt              # or: cd nyl && cargo fmt && cargo clippy --all-targets --fix --allow-dirty

# Lint code
mise run lint             # or: cd nyl && cargo clippy -- -D warnings

# Run tests
mise run test             # or: cd nyl && cargo test

# Pre-commit checks (REQUIRED)
mise run pre-commit       # Runs: fmt-check, lint, test

# Build release binary
mise run build            # or: cd nyl && cargo build --release

# Run benchmarks
mise run bench            # or: cd nyl && cargo bench

# Documentation
mise run docs-serve       # Serve mdbook docs
mise run docs-rustdoc     # Generate API docs
```

### Commit Guidelines

1. **Always run `mise pre-commit` before committing**
2. Commit at regular intervals with descriptive messages
3. Run `cargo fmt` to format the code
4. Ensure all tests pass
5. Update documentation if changing public APIs

## Testing Guidelines

### Parallel Test Execution

**CRITICAL:** All tests MUST run in parallel. This is enforced by CI and `mise pre-commit`.

**Requirements:**
- Tests MUST NOT rely on global state (environment variables, global statics)
- Tests MUST NOT modify global state (env::set_var, static mut)
- Use dependency injection for external dependencies (cache dirs, config paths)
- Each test should use isolated temporary directories via `tempfile::TempDir`
- Tests MUST NOT hardcode fixed temp paths like `/tmp/...`; always create per-test paths from `TempDir`

**Example - DON'T:**
```rust
#[test]
fn test_something() {
    env::set_var("NYL_CACHE_DIR", "/tmp/test");  // ❌ Global state!
    // ... test code
}
```

**Example - DO:**
```rust
#[test]
fn test_something() {
    let cache_dir = TempDir::new().unwrap();
    let manager = GitManager::with_cache_dir(cache_dir.path());  // ✓ Injected!
    // ... test code
}
```

### Integration Tests

When testing components that use `GitManager`:
- Use `HelmChartResolver::with_cache_dir()` instead of `new()`
- Pass `Some(cache_dir.path().to_path_buf())` as the cache directory
- Never use `env::set_var("NYL_CACHE_DIR", ...)` in tests

### Test Coverage

Current status:
- 221+ unit tests
- 34 integration tests  
- 90%+ code coverage

Run tests with coverage:
```bash
mise run coverage  # Generates HTML report in nyl/coverage/
```

### Test Organization and Patterns

**Test structure:**
- Unit tests: In the same file as the code using `#[cfg(test)] mod tests`
- Integration tests: In `tests/` directory
- Test fixtures: Place in `tests/fixtures/` or create in `TempDir`

**Common patterns:**
```rust
// Arrange-Act-Assert pattern
#[test]
fn test_parse_config_with_valid_yaml() {
    // Arrange
    let yaml = "key: value";
    
    // Act
    let result = parse_config(yaml);
    
    // Assert
    assert!(result.is_ok());
    assert_eq!(result.unwrap().key, "value");
}

// Using Result<()> for cleaner error handling in tests
#[test]
fn test_operation() -> Result<()> {
    let value = may_fail()?;
    assert_eq!(value, expected);
    Ok(())
}
```

**Mock and stub guidelines:**
- Use dependency injection to allow test doubles
- Create test-specific implementations of traits
- Prefer real implementations over mocks when practical
- Use `TempDir` for filesystem operations instead of mocking


## Code Style and Architecture

### Style Guidelines
- Follow Rust standard formatting (enforced by `cargo fmt`)
- Use `clippy` warnings as guidance (zero warnings policy)
- Prefer explicit error handling with `Result<T, E>` and `?` operator
- Use descriptive variable and function names
- Add documentation comments (`///`) for public APIs

### Naming Conventions
- **Functions and methods:** `snake_case` (e.g., `render_manifest`, `get_config`)
- **Types and structs:** `PascalCase` (e.g., `HelmChart`, `GitManager`)
- **Constants:** `SCREAMING_SNAKE_CASE` (e.g., `DEFAULT_CACHE_DIR`, `MAX_RETRIES`)
- **Test functions:** `test_<function>_<scenario>` (e.g., `test_render_with_profile`, `test_git_clone_fails`)
- **Modules:** `snake_case` (e.g., `helm_resolver`, `template_engine`)
- **Trait names:** Descriptive nouns or adjectives in `PascalCase` (e.g., `Resolver`, `Renderable`)

### Clippy Policy
- Zero warnings enforced with `-D warnings` flag
- All clippy warnings treated as errors in CI
- Use `#[allow(...)]` sparingly and always with a comment explaining why
- Common acceptable suppressions (with justification):
  - `#[allow(clippy::too_many_arguments)]` - when builder pattern is impractical
  - `#[allow(clippy::module_inception)]` - for module re-exports

### Error Handling
- Use custom error types in `src/error.rs`
- Provide context with `.context()` or `.with_context()`
- Return `Result<T, NylError>` from functions that can fail
- Use `anyhow::Result` sparingly, prefer typed errors

### API Design Guidelines
- **Return types:**
  - Use `Result<T, E>` for operations that can fail
  - Use `Option<T>` for values that may be absent (not errors)
  - Return references (`&T`) when ownership transfer is not needed
  - Return owned types (`T`) when the caller needs ownership
- **Function parameters:**
  - Take references (`&T`, `&str`) for read-only access
  - Use `impl Trait` for parameters to accept any type implementing a trait
  - Prefer `&str` over `&String` and `&[T]` over `&Vec<T>`
- **Public API stability:**
  - Mark unstable APIs with `#[doc(hidden)]` or feature flags
  - Use semantic versioning: breaking changes require major version bump
  - Add deprecation warnings before removing public APIs: `#[deprecated(since = "x.y.z", note = "use X instead")]`
- **Trait design:**
  - Keep traits focused and composable (single responsibility)
  - Provide default implementations where sensible
  - Use associated types for type relationships
  - Consider `Send + Sync` bounds for concurrent usage

### Architecture Patterns
- **Dependency Injection:** Pass dependencies (cache dirs, config paths) explicitly
- **Builder Pattern:** Use for complex object construction (e.g., `HelmChartResolver`)
- **Trait Composition:** Define traits for testability and extensibility
- **Immutability:** Prefer immutable data structures where possible

### Module Organization
```
src/
├── cli/          # Command-line interface (clap)
├── config/       # Configuration loading and validation
├── template/     # Template engine (MiniJinja wrapper)
├── generator/    # Manifest generation pipeline
├── kubernetes/   # Kubernetes client (kube-rs)
├── resources/    # HelmChart, Component resources
├── git/          # Git repository management
├── helm/         # Helm chart processing
├── components/   # Component discovery and registry
├── profiles/     # Profile management
├── secrets/      # Secrets provider framework
└── util/         # Shared utilities
```

## Common Tasks

### Adding a New CLI Command
1. Create command module in `src/cli/commands/`
2. Define command struct with `clap` derives
3. Implement command logic in the module
4. Register command in `src/cli/mod.rs`
5. Add integration test in `tests/`
6. Update documentation in `book/src/commands/`

### Adding a Template Filter
1. Add filter function in `src/template/filters.rs`
2. Register in `TemplateEngine::new()`
3. Add unit test
4. Document in `book/src/templating.md`

### Adding a New Resource Type
1. Define resource struct in `src/resources/`
2. Implement `Serialize`, `Deserialize` traits
3. Add parsing logic in `src/config/`
4. Add validation in `src/config/validate.rs`
5. Add tests
6. Document in `book/src/reference/resources/`

### Updating Dependencies
1. Run `cargo update` to update within semver bounds
2. For major updates, edit `Cargo.toml` manually
3. Run `cargo check` and `cargo test` to verify
4. Check for deprecation warnings with `cargo clippy`
5. Update `Cargo.lock` (committed to repo)

### Adding New Dependencies
- **Evaluation criteria:**
  - Active maintenance and community support
  - Minimal transitive dependencies
  - Compatible license (MIT, Apache-2.0)
  - Good test coverage and documentation
- **Process:**
  1. Check if functionality can be implemented without external dependency
  2. Review crate on crates.io for download count, recent updates, and open issues
  3. Add dependency with minimal features: `dependency = { version = "x.y", default-features = false, features = ["needed-feature"] }`
  4. Document why the dependency is needed in commit message or code comments
  5. Run `cargo tree` to check for dependency bloat
- **Avoid:** Dependencies with long build times, large binary size impact, or many transitive deps

## Performance Considerations

- **Minimize allocations:** Use `&str` instead of `String` where possible
- **Lazy initialization:** Use `OnceCell` or `LazyLock` for expensive initialization
- **Parallel processing:** Use `rayon` for data-parallel operations
- **Caching:** Cache expensive operations (Git clones, Helm downloads)
- **Profile before optimizing:** Use `cargo bench` to measure performance

See `BENCHMARKS.md` for detailed performance analysis.

## Documentation

### Types of Documentation
1. **Code comments:** For implementation details
2. **Doc comments (`///`):** For public APIs (generates rustdoc)
3. **mdbook (`book/`):** User-facing documentation
4. **README.md:** Quick start and overview
5. **IMPLEMENTATION.md:** Development status and technical details

### Writing Documentation
- Document all public functions and types
- Include examples in doc comments
- Use markdown formatting
- Link to related items with `[Item]` or `[crate::path::to::Item]`
- Test examples with `cargo test --doc`

### Building Documentation
```bash
# User documentation (mdbook)
mise run docs-serve        # Opens in browser at http://localhost:3000

# API documentation (rustdoc)
mise run docs-rustdoc      # Opens in browser
```

## CI/CD Pipelines

### Workflows
- **ci.yaml:** Unified CI for quality checks, cross-platform tests, image/chart packaging, and develop integration tests
- **docs.yaml:** Deploy documentation to GitHub Pages
- **release.yml:** Binary releases with cargo-dist

### Release Process
1. Update version in `Cargo.toml`
2. Update `CHANGELOG.md`
3. Commit: `chore: bump version to X.Y.Z`
4. Tag: `git tag vX.Y.Z`
5. Push tag: `git push --tags`
6. GitHub Actions builds and publishes binaries

See `.github/RELEASE_TESTING.md` for release testing checklist.

## Troubleshooting

### Common Issues

**Tests fail with "Address already in use":**
- Ensure tests don't use hardcoded ports
- Use `port: 0` for dynamic port allocation

**Git authentication fails in tests:**
- Mock Git operations or use local test repositories
- Don't rely on external Git services in unit tests

**Cargo build is slow:**
- Use `cargo build` (debug) for development
- Use `sccache` for caching (optional)
- Incremental compilation is enabled by default

**Binary size is large:**
- Use `cargo build --release` for optimized builds
- Profile includes `strip = true` to remove debug info
- LTO (Link Time Optimization) is enabled in release profile

## Security

- Never commit secrets or credentials
- Use `SOPS` for encrypting sensitive data
- Validate all user input in CLI commands
- Use `cargo audit` to check for vulnerabilities:
  ```bash
  mise run security-audit
  ```

## Additional Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [MiniJinja Documentation](https://docs.rs/minijinja/)
- [kube-rs Documentation](https://docs.rs/kube/)
- [clap Documentation](https://docs.rs/clap/)

## Getting Help

- Check existing issues: https://github.com/NiklasRosenstein/nyl/issues
- Review documentation: https://niklasrosenstein.github.io/nyl/
- Read implementation notes: `nyl/IMPLEMENTATION.md`
- Check benchmarks: `nyl/BENCHMARKS.md`

---

**Remember:** Always run `mise pre-commit` before pushing code!
