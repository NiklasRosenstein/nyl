# Package

Use this resource to instantiate another package.

```yaml
apiVersion: templating.nyl/v1
kind: Package
spec:
  package: ./path/to/package
  values: {}
```

__Spec__

* `package` (string): The path to the package to instantiate. Must be prefixed with `./` to be resolved relative
  to the package that references it. Otherwise, it will be resolved in a search path that is defined in the
  `nyl-deployment.yaml` file.
* `values` (object): The values to pass to the package.
