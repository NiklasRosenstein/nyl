# nyl

This directory builds a container image that contains Nyl (Rust version) and the [ArgoCD CMP Server][1] with the following tools:

  [1]: https://argo-cd.readthedocs.io/en/stable/operator-manual/config-management-plugins/#register-the-plugin-sidecar

* **Nyl** (Rust binary from CI build artifacts)
* **ArgoCD CMP Server** (v3.2.5)
* **Helm** (v3.19.5)
* **SOPS** (v3.11.0) - included for future use (not yet implemented in Rust version)
* **Kyverno** (v1.16.2)

## Building

The Rust binary is built by CI and must be provided in the Docker build context as `nyl-amd64` or `nyl-arm64`.

**Example** (for local testing):
```bash
# Build the Rust binary first
cd nyl
cargo build --release
cd ..

# Copy binary to build context
cp nyl/target/release/nyl ./nyl-amd64

# Build Docker image
docker build -t nyl:test -f docker/Dockerfile . --build-arg TARGETARCH=amd64
```

**Note**: In CI, the binary is built separately and added to the build context automatically.

## Image Details

- **Base image**: `debian:bookworm-slim` (changed from `python:3.14-slim`)
- **Binary**: Static Rust binary (~8.5MB)
- **Total size**: <100MB (down from ~200MB+ with Python)
- **No runtime dependencies**: No Python, no venv, pure static binary

## Migration Notes

This image uses the **Rust version** of Nyl with the following changes:
- Command changed: `nyl template` → `nyl render`
- SOPS support not yet implemented (planned for future release)
- See [MOVE_TO_RUST.md](../MOVE_TO_RUST.md) for complete migration guide
