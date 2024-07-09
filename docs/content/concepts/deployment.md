# Deployment

A deployment is where things get together: This is the place where you define the applications that should be
deployed to a Kubernetes cluster, which cluster they are being deployed to, as well as sourcing secrets from
a secret store. The configuration for a deployment is defined in a `nyl-deployment.yaml` file.

```yaml
apiVersion: nyl/v1
kind: Deployment
spec:
  secretStores:
    default:
      type: Sops
      path: secrets.yaml
```

__Spec__

* `secrets` (array): The secret stores to make available in the templating context of the deployment. Any secrets
  need to be injected from these stores into the application values at templating time.
