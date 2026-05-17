---
title: 'Kyverno Policies'
---

Nyl supports applying [Kyverno](https://kyverno.io/) policies to Kubernetes manifests at render time. This allows you to mutate and validate resources before they are applied to the cluster.

## Overview

Kyverno policies in Nyl are standard Kyverno CRDs annotated with a scope that determines when and where they are applied during rendering. Policies without the scope annotation are treated as normal Kubernetes resources and passed through to the output.

## Quick Start

```yaml
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: add-managed-by-label
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps', 'services']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: >-
          Object{metadata: Object.metadata{labels: Object.metadata.labels{managedBy: "nyl"}}}
```

## Supported Policy Types

Nyl supports standard Kyverno policy CRDs:

### Working Policy Types

These policy types work with `kyverno apply` in offline mode:

- **ClusterPolicy** (`kyverno.io/v1`) - Cluster-wide policies using traditional Kyverno format
- **MutatingPolicy** (`policies.kyverno.io/v1`) - Namespace or cluster-scoped mutation policies
- **ValidatingPolicy** (`policies.kyverno.io/v1`) - Namespace or cluster-scoped validation policies

### Detected but Not Evaluable

These policy types are detected but require a live Kubernetes cluster:

- **GeneratingPolicy** (`policies.kyverno.io/v1`) - Generate new resources
- **DeletingPolicy** (`policies.kyverno.io/v1`) - Delete resources
- **ImageValidatingPolicy** (`policies.kyverno.io/v1`) - Validate container images

## Scope Annotation

Policies must include the `nyl.niklasrosenstein.github.com/apply-policy-scope` annotation to be processed by Nyl.

### Available Scopes

#### Global (Currently Supported)

Applies to all resources in the rendered file. Since Nyl processes single files, this means all resources from the manifest being rendered.

```yaml
metadata:
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
```

**Use cases:**
- Organization-wide labeling standards
- Security policies applied to all resources
- Compliance requirements

#### Future Scopes (Not Yet Implemented)

- **Subtree**: Applies to siblings and all descendant resources
- **Immediate**: Applies to sibling resources only

Policies with these scopes will trigger a warning but won't cause errors.

## Policy Examples

### Mutation Policy

Add labels to all ConfigMaps and Secrets:

```yaml
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: add-labels
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps', 'secrets']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: >-
          Object{
            metadata: Object.metadata{
              labels: Object.metadata.labels{
                environment: "production",
                managedBy: "nyl"
              }
            }
          }
```

### Validation Policy

Require specific labels on all resources:

```yaml
apiVersion: policies.kyverno.io/v1
kind: ValidatingPolicy
metadata:
  name: require-labels
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['*']
        apiVersions: ['*']
        operations: ['CREATE']
        resources: ['*']
  validations:
    - expression: >-
        has(object.metadata.labels) &&
        'environment' in object.metadata.labels &&
        'team' in object.metadata.labels
      message: "All resources must have 'environment' and 'team' labels"
```

### ClusterPolicy (Traditional Format)

Using the older `kyverno.io/v1` ClusterPolicy format:

```yaml
apiVersion: kyverno.io/v1
kind: ClusterPolicy
metadata:
  name: add-default-network-policy
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  rules:
    - name: add-network-policy-annotation
      match:
        any:
          - resources:
              kinds:
                - Namespace
      mutate:
        patchStrategicMerge:
          metadata:
            annotations:
              networking.policy: "default-deny"
```

### Service Load Balancer Configuration

Automatically configure LoadBalancer services:

```yaml
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: configure-loadbalancer
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['services']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: >-
          object.spec.type == 'LoadBalancer'
            ? Object{
                spec: Object.spec{
                  loadBalancerClass: "ngrok",
                  allocateLoadBalancerNodePorts: false
                }
              }
            : object
```

## Unannotated Policies

Policies without the scope annotation are treated as normal Kubernetes resources and included in the output. This allows you to:

1. Deploy policies to the cluster along with your applications
2. Manage policies as part of your GitOps workflow
3. Use Nyl for some policies (with annotations) and deploy others normally

```yaml
# This policy will be included in output but NOT applied by Nyl
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: cluster-policy  # No scope annotation
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['apps']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['deployments']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: "object"
```

## Policy Processing

### Processing Order

1. **Policy Detection**: Nyl scans all manifests for Kyverno policy CRDs
2. **Scope Extraction**: Policies with the scope annotation are extracted and grouped by scope
3. **Policy Application**: Global policies are applied to all non-policy resources
4. **Output**: Mutated resources are output; annotated policy resources are excluded

### Policy Exclusion

Policy resources themselves are automatically excluded from mutation by other policies. This prevents circular mutations and ensures policies remain unchanged.

### Validation Failures

If a validation policy fails, the `nyl render` command will fail with an error:

```bash
$ nyl render
ERROR: Configuration error: Kyverno apply failed with exit code Some(1):
...
Validation error: ConfigMap must have 'environment' label
```

## CEL Expressions

Kyverno policies use [Common Expression Language (CEL)](https://github.com/google/cel-spec) for mutations and validations.

### Common Patterns

**Check if field exists:**
```text
has(object.metadata.labels)
```

**Check if key exists in map:**
```text
'environment' in object.metadata.labels
```

**Conditional mutation:**
```text
object.spec.type == 'LoadBalancer'
  ? Object{spec: Object.spec{loadBalancerClass: "ngrok"}}
  : object
```

**Add or merge labels:**
```text
Object{
  metadata: Object.metadata{
    labels: Object.metadata.labels{
      newLabel: "value"
    }
  }
}
```

**Set deeply nested field:**
```text
Object{
  spec: Object.spec{
    template: Object.spec.template{
      spec: Object.spec.template.spec{
        securityContext: Object{runAsNonRoot: true}
      }
    }
  }
}
```

## Requirements

- **Kyverno CLI**: Must be installed and available in PATH
  ```bash
  # Install on Linux
  curl -LO https://github.com/kyverno/kyverno/releases/download/v1.17.0/kyverno-cli_v1.17.0_linux_x86_64.tar.gz
  tar -xzf kyverno-cli_v1.17.0_linux_x86_64.tar.gz
  sudo mv kyverno /usr/local/bin/

  # Install on macOS
  brew install kyverno

  # Verify installation
  kyverno version
  ```

## Best Practices

1. **Use Global scope sparingly**: Global policies affect all resources and can have wide-reaching effects
2. **Test policies locally**: Use `nyl render` to verify policy behavior before deployment
3. **Provide clear error messages**: Include descriptive messages in validation policies
4. **Document policy intent**: Use metadata annotations to document why policies exist
5. **Prefer immutable labels**: Add labels rather than modifying existing ones
6. **Version your policies**: Include version information in policy names or labels

## Troubleshooting

### Policy Not Applied

**Issue**: Policy appears in output but isn't being applied

**Solution**: Check that the policy has the scope annotation:
```yaml
metadata:
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
```

### Kyverno CLI Not Found

**Issue**: `Kyverno CLI is not installed but Kyverno policies were found`

**Solution**: Install the Kyverno CLI (see Requirements section above)

### Validation Policy Passes But Shouldn't

**Issue**: Resources that should fail validation are passing

**Solution**: Verify the CEL expression logic and matchConstraints are correct. Test with `kyverno apply` directly:
```bash
kyverno apply policy.yaml --resource resource.yaml
```

### Multiple Policies Conflict

**Issue**: Multiple policies are mutating the same field differently

**Solution**: Ensure policies are orthogonal (affect different fields) or use proper scoping to control application order

## See Also

- [Kyverno Documentation](https://kyverno.io/docs/)
- [CEL Language Definition](https://github.com/google/cel-spec)
- [Kyverno CLI Documentation](https://kyverno.io/docs/kyverno-cli/)
- [Nyl Commands](../commands/)
