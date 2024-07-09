# Applications

An application is an instance of one package that is deployed to a Kubernetes cluster and are in turn defined
also in a Nyl package. Packages instantiated as applications may not produce other applications. Applications
are usually accompanied by a `nyl-deployment.yaml` file that defines the top-level templating context for the
package(s), such as the secret store.

When deploying a package as an application, the package must not generated resources other than applications,
as all deployed resources must be owned by an application.
