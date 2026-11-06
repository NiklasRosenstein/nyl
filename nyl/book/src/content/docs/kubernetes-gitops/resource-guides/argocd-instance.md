---
title: 'Using ArgoCDInstance'
---

See the [ArgoCDInstance resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/argocd-instance/) for the API, example, and field definitions.

## Catalog defaults

Every target emits a parent Application named `<target>-catalog` unless
`DeploymentTarget.spec.catalogApplication.enabled` is false. The parent recursively
syncs `<pathPrefix>/_nyl/catalog` and therefore manages the generated child
Applications, AppProjects, and its own manifest.

Nyl's catalog defaults apply without any explicit policy configuration:

- Manual synchronization.
- `ApplyOutOfSyncOnly=true` and `ServerSideApply=true` for synchronization.
- `applicationDeletionPolicy: Foreground`, so deleting the parent cascades to
  catalog resources.
- `selfPrunePolicy: Confirm`, which annotates the parent with `Prune=confirm`.

Set `ArgoCDInstance.spec.catalogApplicationDefaults` to customize these defaults
for its targets. An individual DeploymentTarget can override the name, project,
sync policy, deletion policy, self-prune policy, labels, and annotations under
`spec.catalogApplication`.

Enable automated catalog synchronization explicitly when the publication
workflow provides the required approval boundary:

```yaml
spec:
  catalogApplicationDefaults:
    syncPolicy:
      automated:
        enabled: true
        prune: false
        selfHeal: true
      syncOptions:
        - ApplyOutOfSyncOnly=true
        - ServerSideApply=true
```

When no ArgoCDInstance resources exist, each target gets an implicit local
instance using its workload Cluster, the `argocd` namespace, and these defaults.
`--verbose` logs that choice. Once any explicit instance exists, every target must set
`spec.argocdRef.name`; this prevents accidental attachment to the wrong control
plane.

The `default` AppProject must exist before the catalog can manage itself. After
rendering and publishing a target for the first time, seed its parent once:

```bash
kubectl apply -f deploy/<target>/_nyl/catalog/application.argoproj.io/argocd/<target>-catalog.yaml
```

Use the actual rendered output path and configured namespace when they differ.

## Reading Argo CD

Rendering and publishing never read Argo CD. A
[`KubernetesPublication` unit in observe mode](/nyl/orchestration/resource-guides/kubernetes-publication/#observing-argo-cd)
does, to attest that Argo CD accepted a publication and that its Applications
are healthy, and to wait for Applications to disappear when it tears a target
down. `spec.access` says how, once, for every target on the instance. It never
affects rendered bytes, so changing it republishes nothing.

Without `access`, Nyl reads the Application objects from the Kubernetes API of
the instance's control-plane Cluster:

```yaml
apiVersion: k8s.nyl/v1
kind: Cluster
metadata:
  name: control-plane
spec:
  destination: {server: https://kubernetes.default.svc}
  live: {context: argocd-admin}      # the kubeconfig context that reaches this cluster
---
apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata:
  name: prod
spec:
  clusterRef: {name: control-plane}
  namespace: argocd
```

The Cluster's `spec.live.context` names the kubeconfig context, and the unit
admits the kubeconfig file as `KUBECONFIG` (`env: {passthrough: [KUBECONFIG]}`).
Nyl never falls back to the kubeconfig's current context, `~/.kube/config`, or
in-cluster configuration, so a run reads the same cluster wherever it runs. The
context's user needs `list` on `applications.argoproj.io` in the Application
namespaces.

To read the Argo CD API server instead, declare its address and the name of the
variable that holds an API token:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata:
  name: prod
spec:
  clusterRef: {name: control-plane}
  namespace: argocd
  access:
    method: api
    api:
      server: https://argocd.example.com
      tokenEnv: ARGOCD_AUTH_TOKEN      # the default
```

The unit admits the token under that name, from the runner with
`env.passthrough` or from the project's secrets provider with `env.secrets`
(`env: {secrets: {ARGOCD_AUTH_TOKEN: argocd-token}}`). The token needs `get` on
the instance's Applications: an Argo CD account or a project role with a
read-only policy is enough. The address must use `https`, except for a loopback
address, so the token is never sent in the clear.

| | `kubernetes` (default) | `api` |
| --- | --- | --- |
| Declared in Git | The control-plane Cluster's `spec.live.context` | `spec.access.api.server` and `tokenEnv` |
| Admitted by the unit | `KUBECONFIG` | the token variable, and optionally `HTTPS_PROXY` |
| Needs | Access to the control-plane Cluster's API | The API server reachable from the runner |
| Permission | `list` on `applications.argoproj.io` | `get` on Applications |

The token also needs `can-i` (it asks Argo CD whether it may `get` an
Application that its list does not show, so it never concludes an Application is
gone from a list that hides what it may not read).

A credential that is not admitted is an error that names the variable and the
`env` entry that admits it. The process's own environment, including its proxy
settings, is never read: the API method uses an `HTTPS_PROXY` the unit admits,
and the Kubernetes method a `proxy-url` its kubeconfig declares.

One exception: a kubeconfig's `exec` plugin or auth provider is started with the
whole process environment, as `kubectl` does, because the Kubernetes client
cannot restrict it. What it reads is the kubeconfig's own declaration; use the
`api` method where the process environment must stay out of the reading.

Editing `access`, or moving the control-plane Cluster's `live.context`,
republishes nothing, re-executes no unit, and resets no attestation. Adding
`access` to an instance, or removing it, changes the instance file's digest in
published ownership indexes once.
