# Simple App Example

This example demonstrates a basic web application deployment with Nyl.

## Structure

```
simple-app/
├── nyl.toml                  # Project + profile values configuration
├── manifests/
│   ├── deployment.yaml       # Application Deployment
│   ├── service.yaml          # ClusterIP Service
│   └── configmap.yaml        # Application configuration
└── README.md                 # This file
```

## Features Demonstrated

- **Template Variables**: Using `{{ }}` syntax for dynamic values
- **Profile Selection**: `--profile` drives environment behavior
- **Conditional Rendering**: Most environment differences come from `{% if profile == ... %}` blocks
- **Minimal Profile Values**: `nyl.toml` only stores profile-specific image tags

## Usage

### Validate Configuration

```bash
nyl validate
```

### Render Manifests

```bash
# Development environment (1 replica, debug mode)
nyl render --profile dev

# Staging environment (2 replicas)
nyl render --profile staging

# Production environment (3 replicas, optimized resources)
nyl render --profile prod
```

### Apply to Cluster

```bash
# Deploy to development
nyl apply --profile dev

# Deploy to production
nyl apply --profile prod
```

### View Differences

```bash
# See what would change
nyl diff --profile dev
```

## Profile Differences

| Setting | Dev | Staging | Prod |
|---------|-----|---------|------|
| Replicas | 1 | 2 | 3 |
| Image Tag | dev-latest | staging-v1.0.0 | v1.0.0 |
| Debug Mode | true | false | false |
| Resource Requests | 100m/128Mi | 200m/256Mi | 500m/512Mi |

## Customization

To customize this example:

1. Edit `nyl.toml` under `[profile.values.<name>]` to add or change profiles
2. Modify manifests in `manifests/` to add resources (Ingress, PVC, etc.)
3. Adjust profile-specific conditionals (`{% if profile == ... %}`) in manifests

## Next Steps

After mastering this example:
1. Add a `HelmChart` resource to this project and render it with different profiles.
2. Extract repeated manifest fragments into local components.
3. Add `profile.values` entries in `nyl.toml` beyond `image_tag` and consume them via `values.*`.
