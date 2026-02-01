# Release Workflow Testing Guide

This guide explains how to test the release workflow without creating actual releases.

## Quick Reference

| Tag Pattern | Result |
|-------------|--------|
| `v0.1.0-rc.1` | Prerelease: dry-run publish, create GitHub release |
| `v0.1.0-test` | Prerelease: dry-run publish, create GitHub release |
| `v0.1.0-alpha.1` | Prerelease: dry-run publish, create GitHub release |
| `v0.1.0` | Full release: publish to crates.io, create GitHub release |

## Testing Methods

### Method 1: Use Prerelease Tags (Recommended)

Create a prerelease tag to test the entire workflow without publishing:

```bash
# Test the workflow with a prerelease tag
git tag v0.1.0-rc.1
git push origin v0.1.0-rc.1

# What happens:
# ✅ Workflow runs end-to-end
# ✅ Binaries are built for all platforms
# ✅ cargo publish --dry-run validates package
# ✅ GitHub release is created (marked as prerelease)
# ❌ Does NOT publish to crates.io
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
# 1. Verify cargo package is valid
cd nyl
cargo publish --dry-run

# 2. Verify binary builds
cargo build --release

# 3. Check dist plan
cargo install cargo-dist
dist plan

# 4. Test dist build locally
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

- [ ] `cargo publish --dry-run` succeeds
- [ ] All tests pass: `cargo test --all-features`
- [ ] Clippy is clean: `cargo clippy --all-targets --all-features`
- [ ] Formatting is correct: `cargo fmt --check`
- [ ] Version number updated in `Cargo.toml`
- [ ] CHANGELOG.md updated with release notes
- [ ] Documentation builds: `mdbook build book`
- [ ] Test with prerelease tag (e.g., `v0.1.0-rc.1`)
- [ ] Verify GitHub release artifacts are correct
- [ ] Verify binary sizes are acceptable (<20MB)

## Workflow Behavior

### On Prerelease Tags (e.g., v0.1.0-rc.1)

1. **plan** job: Determines this is a prerelease
2. **build-local-artifacts** job: Builds binaries for all platforms
3. **build-global-artifacts** job: Creates checksums and archives
4. **host** job: Uploads artifacts to GitHub release (marked as prerelease)
5. **publish-crates-io** job: Runs `cargo publish --dry-run` (validation only)
6. **announce** job: Finalizes release

### On Stable Tags (e.g., v0.1.0)

1. **plan** job: Determines this is a stable release
2. **build-local-artifacts** job: Builds binaries for all platforms
3. **build-global-artifacts** job: Creates checksums and archives
4. **host** job: Uploads artifacts to GitHub release (stable)
5. **publish-crates-io** job: Runs `cargo publish` (publishes to crates.io)
6. **announce** job: Finalizes release

## Crates.io Publishing

The workflow uses **GitHub OIDC trusted publishing** to publish to crates.io:

- **No tokens required** in GitHub secrets
- **Configured once** in crates.io account settings
- **Automatic verification** via GitHub Actions OIDC

### Setup (One-time)

1. Go to https://crates.io/settings/tokens
2. Navigate to "Trusted Publishing" section
3. Add GitHub Actions publisher:
   - **Repository**: `helsing-ai/nyl` (or your fork)
   - **Workflow**: `release.yml`
   - **Job**: `publish-crates-io`
   - **Environment**: (leave empty)

Once configured, the workflow will automatically authenticate using OIDC.

## Common Issues

### "package appears to have no version"

**Cause**: Cargo.toml version doesn't match the tag.

**Fix**:
```bash
# Ensure Cargo.toml version matches tag
cd nyl
# Edit Cargo.toml: version = "0.1.0"
git add Cargo.toml
git commit -m "chore: bump version to 0.1.0"
git tag v0.1.0
```

### "crate is not authorized"

**Cause**: OIDC trusted publishing not configured.

**Fix**: Follow the "Setup (One-time)" steps above.

### "Binary size exceeds 20MB limit"

**Cause**: Release binary is too large.

**Fix**:
```bash
# Check binary size
ls -lh nyl/target/release/nyl

# Reduce size with strip
strip nyl/target/release/nyl

# Or update size limit in .github/workflows/rust.yaml
```

## Recommended Release Process

1. **Prepare release**:
   ```bash
   # Update version in Cargo.toml
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
   git tag v0.1.0
   git push origin v0.1.0
   # Workflow publishes to crates.io and creates GitHub release
   ```

4. **Verify**:
   - Check GitHub release: https://github.com/helsing-ai/nyl/releases
   - Check crates.io: https://crates.io/crates/nyl
   - Test installation: `cargo install nyl`

## Emergency Rollback

If a release goes wrong:

### GitHub Release
```bash
# Delete release from GitHub
gh release delete v0.1.0 --yes

# Delete tag
git tag -d v0.1.0
git push origin :refs/tags/v0.1.0
```

### Crates.io
**Cannot be undone** - crate versions on crates.io are immutable.

**Options**:
1. Yank the version (still installable with exact version, but not recommended):
   ```bash
   cargo yank --vers 0.1.0
   ```
2. Publish a new patch version (e.g., v0.1.1) with fixes

This is why testing with prereleases is critical!
