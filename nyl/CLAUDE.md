- Commit at regular intervals.
- Always run `mise pre-commit` before comitting. Run `cargo fmt` to format the code.

## Testing Guidelines

### Parallel Test Execution

All tests must be able to run in parallel. This is enforced by the CI pipeline
and `mise pre-commit` checks.

**Requirements:**
- Tests MUST NOT rely on global state (environment variables, global statics)
- Tests MUST NOT modify global state (env::set_var, static mut)
- Use dependency injection for external dependencies (cache dirs, config paths)
- Each test should use isolated temporary directories via `tempfile::TempDir`

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
