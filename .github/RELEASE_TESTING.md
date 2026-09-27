# Release Workflow Testing Guide

This guide explains how to test the release workflow without creating actual releases.

Nyl ships as release binaries and container images. Its crates (`nyl`,
`nyl-core`, and `nyl-render`) are not published to crates.io.

## Quick Reference

| Tag Pattern | Result |
|-------------|--------|
| `v0.1.0-rc.1` | Prerelease: build artifacts, create GitHub release |
| `v0.1.0-test` | Prerelease: build artifacts, create GitHub release |
| `v0.1.0-alpha.1` | Prerelease: build artifacts, create GitHub release |
| `v0.1.0` | Full release: build artifacts, create GitHub release |

## Testing Methods

### Method 1: Use Prerelease Tags (Recommended)

Create a prerelease tag to test the entire workflow:

```bash
# Test the workflow with a prerelease tag
git tag v0.1.0-rc.1
git push origin v0.1.0-rc.1

# What happens:
# ✅ Workflow runs end-to-end
# ✅ Binaries are built for all platforms
# ✅ GitHub release is created (marked as prerelease)
```

**Cleanup after testing:**
```bash
# Delete the test release from GitHub UI or:
gh release delete v0.1.0-rc.1 --yes
git tag -d v0.1.0-rc.1
git push origin :refs/tags/v0.1.0-rc.1
```

### Method 2: Local Validation

Test components locally before pushing tags:

```bash
# 1. Verify binary builds
cargo build --release -p nyl

# 2. Check dist plan
cargo install cargo-dist
dist plan

# 3. Test dist build locally
dist build
```

### Method 3: Check GitHub Actions Locally

Use [act](https://github.com/nektos/act) to test workflows locally:

```bash
# Install act
# brew install act  # macOS
# See: https://github.com/nektos/act#installation

# Test the release workflow (requires Docker)
act -W .github/workflows/release.yml -j build-local-artifacts

# Note: act has limitations and may not perfectly replicate GitHub Actions
```

## Testing Checklist

Before creating a real release, verify:

- [ ] All tests pass: `cargo test --workspace --all-features`
- [ ] Clippy is clean: `cargo clippy --workspace --all-targets --all-features`
- [ ] Formatting is correct: `cargo fmt --all --check`
- [ ] Workspace version updated in the root `Cargo.toml` (`scripts/release.sh` does this)
- [ ] CHANGELOG.md updated with release notes
- [ ] Documentation builds: `mise run docs-build`
- [ ] `dist plan` lists the `nyl` binary
- [ ] Test with prerelease tag (e.g., `v0.1.0-rc.1`)
- [ ] Verify GitHub release artifacts are correct
- [ ] Verify binary sizes are acceptable (<20MB)

## Workflow Behavior

1. **plan** job: Determines whether this is a prerelease
2. **build-local-artifacts** job: Builds binaries for all platforms
3. **build-global-artifacts** job: Creates checksums and archives
4. **host** job: Uploads artifacts to the GitHub release (marked as prerelease for prerelease tags)
5. **announce** job: Finalizes release

## Common Issues

### "package appears to have no version"

**Cause**: The workspace version in the root `Cargo.toml` doesn't match the tag.

**Fix**:
```bash
# Bumps the shared workspace version, commits, tags, and pushes
scripts/release.sh 0.1.0
```

### `dist plan` does not list `nyl`

**Cause**: The workspace packages set `publish = false`, which cargo-dist
treats as "do not distribute" unless the package opts in.

**Fix**: Keep `[package.metadata.dist] dist = true` in `nyl/Cargo.toml`.

## Recommended Release Process

1. **Prepare release**:
   ```bash
   # Update CHANGELOG.md
   git commit -am "chore: prepare v0.1.0 release"
   git push
   ```

2. **Test with prerelease**:
   ```bash
   git tag v0.1.0-rc.1
   git push origin v0.1.0-rc.1
   # Wait for workflow, verify everything works
   ```

3. **Create stable release**:
   ```bash
   scripts/release.sh 0.1.0
   # Workflow creates the GitHub release
   ```

4. **Verify**:
   - Check GitHub release: https://github.com/NiklasRosenstein/nyl/releases
   - Test installation with the shell installer or container image

## Emergency Rollback

If a release goes wrong:

```bash
# Delete release from GitHub
gh release delete v0.1.0 --yes

# Delete tag
git tag -d v0.1.0
git push origin :refs/tags/v0.1.0
```

Then publish a new patch version (e.g., v0.1.1) with fixes.
