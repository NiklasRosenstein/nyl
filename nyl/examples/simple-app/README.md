# Simple App Example

This example demonstrates a basic web application deployment with Nyl.

## Structure

```
simple-app/
├── nyl.toml                  # Project path configuration
├── nyl-profiles.yaml         # Profile values (dev/staging/prod)
├── manifests/
│   ├── deployment.yaml       # Application Deployment
│   ├── service.yaml          # ClusterIP Service
│   └── configmap.yaml        # Application configuration
└── README.md                 # This file
```

## Features Demonstrated

- **Template Variables**: Using `{{ }}` syntax for dynamic values
- **Profiles**: Different configurations for dev, staging, and prod
- **Resource Management**: CPU and memory settings per environment
- **Environment-Specific Values**: Replica counts, image tags, debug flags

## Usage

### Validate Configuration

```bash
nyl validate
```

### Render Manifests

```bash
# Development environment (1 replica, debug mode)
nyl render --environment dev

# Staging environment (2 replicas)
nyl render --environment staging

# Production environment (3 replicas, optimized resources)
nyl render --environment prod
```

### Apply to Cluster

```bash
# Deploy to development
nyl apply --environment dev

# Deploy to production
nyl apply --environment prod
```

### View Differences

```bash
# See what would change
nyl diff --environment dev
```

## Profile Differences

| Setting | Dev | Staging | Prod |
|---------|-----|---------|------|
| Replicas | 1 | 2 | 3 |
| Image Tag | dev-latest | staging-v1.0.0 | v1.0.0 |
| Debug Mode | true | false | false |
| CPU Request | 100m | 200m | 500m |
| Memory Request | 128Mi | 256Mi | 512Mi |

## Customization

To customize this example:

1. Edit `nyl-profiles.yaml` to add more profiles or change values
2. Modify manifests in `manifests/` to add resources (Ingress, PVC, etc.)
3. Add environment-specific overrides using profile values

## Next Steps

After mastering this example:
- Try the [helm-charts](../helm-charts/) example for Helm integration
- Explore [components](../components/) for reusable patterns
- Study [multi-env](../multi-env/) for advanced configurations
