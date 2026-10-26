---
title: 'Using EnvironmentGroup'
---

See the [EnvironmentGroup resource reference](/nyl/reference/resources/gitops.nyl/v1/environment-group/) for the API, example, and field definitions.

## Sharing defaults

An EnvironmentGroup gives the Environments its `environmentSelector` matches
shared defaults. Its `values` sit beneath each member's own `values`, and the
member wins. Its `state` fields are the default state repository of the
members. `protectedRefs`, `allowUnprotectedSource`, `admission`, and
`driftRepair` apply to every member that omits them.

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentGroup
metadata:
  name: production
spec:
  environmentSelector:
    matchLabels:
      stage: production
  values:
    cluster: prod-1
  driftRepair: manual
```

Here every production environment leaves drift repair to an operator's
`nyl reconcile --unit <unit> --reapply --reason <text>`, unless it sets
`driftRepair` itself. See
[drift repair](/nyl/orchestration/resource-guides/environment/#drift-repair).

At most one group with shared fields may select an Environment; `nyl validate`
reports an overlap as `NYL-ENVIRONMENT-GROUP-OVERLAP`.

## See also

- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)
