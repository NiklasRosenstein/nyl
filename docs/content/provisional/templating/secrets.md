# Secret injection

!!! danger
    When using secret injection with Nyl, you must make sure that you are aware of the risk profile for unintentionally
    revealing a secret in ArgoCD, which only masks out the data for actual Kubernetes `Secret` resources. Any other
    resource that contains the secret will be rendered in plain text.

Secrets are only available at the deployment level and need to be propagated further down.

```yaml
apiVersion: nyl/v1
kind: Application
metadata:
  name: my-app
spec:
  package: ./path/to/package
  values:
    theSecret: {{ Secrets.default.get("my-secret") }}
```
