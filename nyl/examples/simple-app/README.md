# Simple App Example

This example renders one web application for development, staging, and
production targets.

## Structure

```text
simple-app/
├── nyl.toml
├── config/
│   ├── clusters/local.yaml
│   └── targets/{dev,staging,prod}.yaml
└── manifests/
    ├── deployment.yaml
    ├── service.yaml
    └── configmap.yaml
```

The `local` Cluster contains Kubernetes capabilities and cluster facts. Each
DeploymentTarget references it and supplies deployment values such as environment,
namespace, image, replicas, and resources. Target values win over Cluster
values at conflicting leaves.

## Render

```bash
nyl render --target dev manifests/deployment.yaml
nyl render --target staging manifests/deployment.yaml
nyl render --target prod manifests/deployment.yaml
```

The committed Cluster capabilities make these renders deterministic without
Kubernetes discovery.

## Diff and apply

The example Cluster uses kube context `kind-kind`. Change `spec.live.context`
or pass `--context` for another local cluster.

```bash
nyl diff --target dev manifests/deployment.yaml
nyl apply --target dev manifests/deployment.yaml
nyl apply --target prod --context admin@production manifests/deployment.yaml
```

## What differs

| Setting | Dev | Staging | Prod |
|---|---:|---:|---:|
| Replicas | 1 | 2 | 3 |
| Image tag | dev-latest | staging-v1.0.0 | v1.0.0 |
| Debug | true | false | false |
| CPU request | 100m | 200m | 500m |

Edit the target resources to change deployment intent. Edit the Cluster when a
fact or Kubernetes capability of the concrete destination changes.
