---
title: 'Kubernetes GitOps Resources'
sidebar:
  label: 'Resources'
---

Kubernetes GitOps uses shared repository configuration in `gitops.nyl/v1` and
Kubernetes-specific configuration in `k8s.gitops.nyl/v1`. These resources are
compiler inputs; they are not installed as Nyl custom resources in a cluster.

The [resource catalog](/nyl/reference/resources/) lists every kind, its purpose,
and its schema-generated field reference.

## Common envelope

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: <resource-kind>
metadata:
  name: <local-name>
  labels: {}
spec: {}
```

`metadata.name` is a Kubernetes DNS subdomain and forms a project-local identity
with `kind`. `metadata.labels` is an optional string map. The API version, kind,
and name must remain static so Nyl can discover resources without evaluating
templates.

Files can live anywhere visible to Git discovery. The
[recommended project structure](/nyl/rendered-gitops/project-structure/)
groups them under `config/` for readability.

## Validation and schemas

Validate the complete reference graph with:

```bash
nyl validate
```

Kind-specific JSON schemas are available from the
[Nyl Resource Schemas](/nyl/reference/editor-schemas/). Scaffolded resources include the
appropriate YAML language-server schema URL.

See the [Rendered Manifest Pattern](/nyl/rendered-gitops/)
for the operational workflow.
