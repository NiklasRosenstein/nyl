# Nyl CI image

The Nyl image is a shell-friendly rendering environment for CI jobs. It contains:

- Nyl
- Helm
- Git
- SOPS
- Kyverno CLI
- Bash and CA certificates

The image has no entrypoint, so CI systems can run any included command. Its
default command prints Nyl's help.

```bash
docker run --rm ghcr.io/niklasrosenstein/nyl:TAG nyl --version
docker run --rm -v "$PWD:/workspace" ghcr.io/niklasrosenstein/nyl:TAG \
  nyl render-tree /workspace --target production --output-dir /workspace/deploy
```

## Building

The Nyl binary is built by CI and must be provided in the Docker build context as `nyl-amd64` or `nyl-arm64`.

Example for local testing:

```bash
# Build a Linux binary matching the image architecture
cd nyl
cargo build --release --target x86_64-unknown-linux-musl
cd ..

# Copy binary to build context
cp target/x86_64-unknown-linux-musl/release/nyl ./nyl-amd64

# Build Docker image
docker buildx build --platform linux/amd64 --load -t nyl:test -f docker/Dockerfile .
```

CI builds the binaries separately and adds them to the build context.

## Image Details

- Base image: `debian:bookworm-slim`
- Working directory: `/workspace`
- User: `root`, allowing CI jobs to install or write workspace tooling as needed
- Nyl binary: statically linked
