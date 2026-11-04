---
title: 'Hand Over to Argo CD'
description: 'Why a sync can fail after other tools applied a resource, and how to hand field ownership to Argo CD.'
---

Generated Applications sync with server-side apply as the field manager
`argocd-controller`. `nyl apply` also uses server-side apply, as `nyl` unless told
otherwise. This page explains when the two get in each other's way and how to
resolve it.

## Why syncs fail after another tool applied

Server-side apply records an owner for every field in the object's
`metadata.managedFields`. When a manager stops applying a field, the API server
removes it only if no other manager still owns it. A field that someone else
also owns stays in the object after you remove it from Git.

That leftover field causes the typical symptoms:

- A field removed from the source stays on the live object, and the Application
  remains in sync.
- A sync fails because the leftover field contradicts its replacement. For example,
  switching a container's `env` entry from `valueFrom` to `value` fails with
  `only one of value and valueFrom may be specified`.

The other owner is usually one of these:

- `nyl`, after `nyl apply` without `--handoff` bootstrapped, iterated on, or
  repaired the resource.
- `kubectl edit`, `kubectl set`, `kubectl scale`, or a client-side `kubectl apply`.
- Helm, or Argo CD itself before server-side apply was enabled.

## Find the owners

List the managers and the fields each owns:

```bash
kubectl get deployment web -n web -o yaml --show-managed-fields
```

Each entry in `metadata.managedFields` names a `manager`, an `operation`
(`Apply` for server-side apply, `Update` for everything else), and the fields it
owns under `fieldsV1`.

## Hand over from `nyl apply`

Before enabling or re-enabling the Application's sync, apply the Release once
more with `--handoff`:

```bash
nyl apply --target production --handoff applications/web/release.yaml
```

Nyl applies every resource as `argocd-controller`, then releases the fields the
`nyl` manager owns. Fields in the manifest stay, now owned by Argo CD's manager,
and fields only `nyl` still owned are removed. A later sync can then remove
anything dropped from the source. Each resource whose `nyl` fields were released
prints a `↳` line under its outcome, and the summary counts them as
`released from nyl`. A resource whose fields could not be released is reported
as failed, and the command exits non-zero.

Use `--handoff` for every `nyl apply` of a resource Argo CD manages, including a
break-glass fix while its Application is paused. The fix then belongs to Argo CD's
manager, and the next sync converges instead of failing. `--handoff=<MANAGER>`
hands over to another field manager, for example when Argo CD is configured with a
different one. [`nyl release rollback`](/nyl/reference/cli/release/#rollback) takes
the same option.

## Hand over from other managers

- **`Update` operations** (client-side apply, `kubectl edit`, Helm): Argo CD's
  `ClientSideApplyMigration` sync option, enabled by default with server-side
  apply, moves the ownership of one such manager to `argocd-controller` during
  sync. Set the `argocd.argoproj.io/client-side-apply-migration-manager`
  annotation on the Application to choose which manager it migrates.
- **Another `Apply` manager**: release its fields by applying a configuration
  that names only the object as that manager. Fields Argo CD's manager also owns
  stay; fields only that manager owned are removed:

  ```bash
  kubectl apply --server-side --field-manager=<MANAGER> -f - <<'EOF'
  apiVersion: apps/v1
  kind: Deployment
  metadata:
    name: web
    namespace: web
  EOF
  ```

## Why not `Replace=true`

The `Replace=true` sync option makes Argo CD replace the whole object. That
discards every other owner's fields, so the conflict disappears, but so does
everything else nobody wrote to Git. Nyl does not add it, and setting it per
resource is a last resort:

- A Deployment scaled by a HorizontalPodAutoscaler omits `spec.replicas`. Replacing
  it resets the field to `1`, so every sync scales the workload down until the
  autoscaler reacts. A Deployment that does set `replicas` overrides the
  autoscaler on every sync instead.
- Finalizers, labels, and annotations other controllers set are dropped, and so
  is the `kubectl rollout restart` annotation in a Pod template, which starts
  another rollout.
- Immutable fields, such as a Deployment's `selector` or a Job's `template`,
  still fail. Only `Force=true,Replace=true` gets past them by deleting and
  recreating the object.

Prefer handing ownership over. Keep `Replace=true` for resources that no other
controller writes to.
