# Nyl Examples

This directory contains practical examples demonstrating how to use Nyl for various Kubernetes deployment scenarios.

## Examples Overview

### 1. [simple-app](./simple-app/)
Basic example showing:
- Simple Kubernetes manifests (Deployment, Service, ConfigMap)
- Template variables with profiles
- Multi-environment rendering with `--profile`

More examples are planned and will be added here as they are published.

## Running Examples

Each example includes its own README with specific instructions. Generally:

```bash
# Navigate to example directory
cd examples/simple-app

# Validate the project
nyl validate

# Render manifests for development
nyl render --profile dev

# Render for production
nyl render --profile prod

# See diff against cluster
nyl diff --profile dev

# Apply to cluster
nyl apply --profile dev
```

## Prerequisites

- Nyl installed (`cargo install --path .` from repository root)
- kubectl configured (for diff/apply commands)
- Helm installed (for Helm chart examples)

## Learning Path

1. Start with **simple-app** to understand basic concepts
2. Extend it by adding a HelmChart resource
3. Refactor repeated manifests into reusable components
4. Add more profiles and profile values in `nyl.toml`

## Contributing

Feel free to contribute additional examples! Useful examples include:
- ArgoCD ApplicationSet integration
- GitOps workflows
- Custom post-processors
- Advanced templating patterns
