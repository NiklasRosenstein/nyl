# Nyl Examples

This directory contains practical examples demonstrating how to use Nyl for various Kubernetes deployment scenarios.

## Examples Overview

### 1. [simple-app](./simple-app/)
Basic example showing:
- Simple Kubernetes manifests (Deployment, Service, ConfigMap)
- Template variables with profiles
- Multi-environment configuration

### 2. [helm-charts](./helm-charts/)
Demonstrates Helm chart usage:
- Using HelmChart resources
- Customizing chart values
- Local chart references
- Chart composition

### 3. [components](./components/)
Shows component-based architecture:
- Creating reusable components
- Component instantiation
- Component discovery
- Building component libraries

### 4. [multi-env](./multi-env/)
Production-grade multi-environment setup:
- Development, staging, production profiles
- Environment-specific values
- Secret management patterns
- Resource customization per environment

## Running Examples

Each example includes its own README with specific instructions. Generally:

```bash
# Navigate to example directory
cd examples/simple-app

# Validate the project
nyl validate

# Render manifests for development
nyl render --environment dev

# Render for production
nyl render --environment prod

# See diff against cluster
nyl diff --environment dev

# Apply to cluster
nyl apply --environment dev
```

## Prerequisites

- Nyl installed (`cargo install --path .` from repository root)
- kubectl configured (for diff/apply commands)
- Helm installed (for Helm chart examples)

## Learning Path

1. Start with **simple-app** to understand basic concepts
2. Move to **helm-charts** to learn chart integration
3. Explore **components** for reusable patterns
4. Study **multi-env** for production setups

## Contributing

Feel free to contribute additional examples! Useful examples include:
- ArgoCD ApplicationSet integration
- GitOps workflows
- Custom post-processors
- Advanced templating patterns
