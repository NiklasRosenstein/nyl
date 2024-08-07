# Packages

A package is similar to a Helm chart in that it is a source for Kubernetes resources that can make use of
templating to conditionally render resources and inject values into them. Resources generated by packages
may make use of Nyl-specific resource kinds that are only available time of resource generation (see the
[Templating > Resource Kinds](#resource-kinds) section below).

Nyl packages can be checked into a source repository in a DRY form, but can be compiled to a bundle (e.g.
including other referenced packages or Helm charts) for deployment when needed (e.g. for shipping to an
air-gapped environment).

Packages may have a `nyl-package.yaml` file that defines a schema for the package parameters and additional
metadata. A package without this file may still accept and use parameters in its templates.

## Example package

Let's consider a somewhat contrived example package that generates a stateful secret, passes that value into a Helm
chart, and then further passes another secret created by the Helm chart into another Helm chart. This demonstrates
how Nyl can be used to manage the lifecycle of resources across multiple packages.

=== "Directory structure"

    ```
    my-package/
    ├── nyl-package.yaml
    └── app.yaml
    ```

=== "nyl-package.yaml"

    ```yaml
    # todo
    ```

=== "app.yaml"

    ```yaml
    apiVersion: nyl.io/v1
    kind: StatefulSecret
    name: password
    stringData:
      PASSWORD: {{ randhex(32) }}
    ---
    apiVersion: nyl.io/v1
    kind: HelmChart
    name: my-chart
    chart:
      repository: https://charts.example.com
      name: my-chart
      version: 1.0.0
    values:
      password: {{ ref("Secret", "password", "PASSWORD") }}
    ---
    apiVersion: nyl.io/v1
    kind: HelmChart
    name: my-other-chart
    chart:
      repository: https://charts.example.com
      name: my-other-chart
      version: 1.0.0
    values:
      sharedSecret: {{ ref("Secret:shared-secret.SECRET") }}
    ```

In this example, the `StatefulSecret` resource generates a Kubernetes `Secret` resource with a random 32-character
password. If this secret already exists, it will not be updated (only new keys will be added and removed keys will
be deleted).

We reference that generated secret in the `my-chart` Helm chart. Until this secret is available in the cluster, the
Helm chart will not be rendered.

Finally, we reference a secret that we expect to be generated by the `my-chart` Helm chart in the `my-other-chart` Helm
chart. Again, this will not be rendered until the secret is available in the cluster.

??? todo "Placeholder resources"
    Should Nyl generate a placeholder resource to indicate that there's a dependency on a resource that doesn't exist
    yet? This would allow for a better way to introspect the reconciliation state of the package.
