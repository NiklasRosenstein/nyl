# nyl

This directory builds a container image that contains Nyl and the [ArgoCD CMP Server][1] with the following tools:

  [1]: https://argo-cd.readthedocs.io/en/stable/operator-manual/config-management-plugins/#register-the-plugin-sidecar

* **Nyl** (static binary from CI build artifacts)
* **ArgoCD CMP Server** (v3.2.5)
* **Helm** (v3.19.5)
* **SOPS** (v3.11.0) - included for future use
* **Kyverno** (v1.16.2)

## Building

The Nyl binary is built by CI and must be provided in the Docker build context as `nyl-amd64` or `nyl-arm64`.

**Example** (for local testing):
```bash
# Build the binary first
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

- **Base image**: `debian:bookworm-slim`
- **Binary**: Static binary
- **Runtime dependencies**: none for Nyl itself
