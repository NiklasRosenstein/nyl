---
title: 'Pod template checksums'
---

A workload rolls out again only when its Pod template changes. Changing a
ConfigMap or Secret that a Deployment mounts does not, so Pods keep running the
old configuration until something touches the template. A literal digest in a
Pod annotation fixes that, but it duplicates derived state, and it goes stale the
day someone edits the ConfigMap and forgets the digest.

Set `k8s.nyl/checksum` on the Pod template and Nyl writes the digest for you, as
`k8s.nyl/checksum-sha256`, from the ConfigMaps and Secrets the same
[Release](/nyl/kubernetes-gitops/resource-guides/release/) renders. The ConfigMap stays an
ordinary resource: you do not inline it, hoist its content into a template
variable, or maintain a digest.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: glance-config
data:
  glance.yml: |
    server:
      port: 8080
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: glance
spec:
  selector:
    matchLabels:
      app: glance
  template:
    metadata:
      labels:
        app: glance
      annotations:
        k8s.nyl/checksum: auto
    spec:
      containers:
        - name: glance
          image: glanceapp/glance:v0.7.0
          volumeMounts:
            - name: config
              mountPath: /app/config
      volumes:
        - name: config
          configMap:
            name: glance-config
```

```console
$ nyl render glance.yaml --offline
...
  template:
    metadata:
      annotations:
        k8s.nyl/checksum: auto
        k8s.nyl/checksum-sha256: f64f17eaef82f5a51004e73a3ff47404143e2c52bb2d2c6a375bd430c58a27aa
```

Change `glance.yml` and the digest changes, so the Deployment rolls out. Reorder
the keys of `data`, edit the ConfigMap's labels, or reformat the YAML, and it
does not.

Rendering the output again is safe: a Pod template that already holds
`k8s.nyl/checksum-sha256` next to `k8s.nyl/checksum` gets the value recomputed.
Nyl owns the key, so a document that sets it without `k8s.nyl/checksum` fails.

What a rollout means depends on the workload. A Job's `spec.template` is
immutable, so a changed digest makes the apply or sync fail unless the Job is
recreated, for example with a hook delete policy or Argo CD's `Replace=true`. A
ReplicaSet does not replace running Pods when its template changes, so the
digest only reaches Pods that are created afterwards; use a Deployment to roll
Pods out.

## Choosing what is hashed

`k8s.nyl/checksum` goes in the annotations of the Pod template: `spec.template`
of a Deployment, StatefulSet, DaemonSet, ReplicaSet, or Job, and
`spec.jobTemplate.spec.template` of a CronJob. Its value is one of two forms.

### `auto`

`auto` hashes every ConfigMap and Secret the Pod spec already references, so
there is no list to keep in step with the volumes and environment variables:

| Reference in the Pod spec | What is hashed |
| --- | --- |
| `envFrom[].configMapRef`, `envFrom[].secretRef` | every key of the object |
| `env[].valueFrom.configMapKeyRef`, `env[].valueFrom.secretKeyRef` | that key |
| `volumes[].configMap`, `volumes[].secret` | the keys in `items`, else every key |
| `volumes[].projected.sources[].configMap`, `.secret` | the keys in `items`, else every key |

The references of `containers`, `initContainers`, and `ephemeralContainers` all
count. Image pull Secrets and volume types that use a Secret to reach storage
are not read: they carry no content that the workload's configuration depends on.

`auto` hashes what the Release renders. A reference to an object the Release
does not render (a ConfigMap another team owns, say) or to a key that the
rendered object lacks is skipped, not an error: Nyl never reads the cluster, and
adding the key later changes the digest. When nothing is selected, the
annotation still gets the digest of the empty selection, and Nyl warns with
`NYL-KUBERNETES-CHECKSUM-EMPTY`, since the Pod spec probably references nothing
the Release renders.

### An explicit list

A comma-separated list names exactly what to hash. Each entry is `Kind/name`,
with an optional `#key` that selects one key, and the kind is `ConfigMap` or
`Secret`:

```yaml
annotations:
  k8s.nyl/checksum: "ConfigMap/glance-config#glance.yml,Secret/app-env"
```

Here only `glance.yml` of the ConfigMap, and every key of the Secret, can roll
the Pods. Every entry must resolve to a rendered object, and a `#key` to a key
it holds, or the render fails with a [stable error](#errors).
The order of entries does not matter, and neither does repeating one.

`auto` cannot be combined with entries; use one form or the other.

## What the digest covers

Only the bytes a Pod can read:

- a ConfigMap's `data`, as UTF-8, and `binaryData`, base64-decoded;
- a Secret's `data`, base64-decoded, and `stringData`. For a key in both,
  `stringData` wins, as it does when Kubernetes stores the Secret.

Metadata, labels, annotations, `type`, `immutable`, and the order and formatting
of keys never matter. A value is its exact bytes, so a trailing newline counts: a
block scalar `|` and a stripped `|-` are different values. A value reads the same
whether it was written in `data` or as `binaryData`, because a Pod sees the same
file.

### Serialization

The digest is the SHA-256, in lowercase hex, of this input:

1. The text `k8s.nyl/checksum/v1` and a zero byte, which name the format.
2. For each selected key, in ascending byte order of `(kind, name, key)`, four
   fields: the kind (`ConfigMap` or `Secret`), the object's name, the key, and the
   value. Each field is its length in bytes as an unsigned 64-bit big-endian
   integer, then the bytes.

Because every field carries its length, two different selections never produce
the same input: moving a character from a key into its value, or from a name into
a key, changes the digest. The selection is a set of keys, so referencing an
object twice, or whole and by key, counts each key once, and the order of
references in the Pod spec or in the annotation never matters. The namespace is
not hashed.

## Namespaces

A reference resolves in the workload's namespace, as Kubernetes resolves it: the
workload's `metadata.namespace`, or the namespace the Release is deployed to when
it sets none. That is the `destinationNamespace` of the
[ApplicationGroup](/nyl/kubernetes-gitops/resource-guides/application-group/) that
renders the Release, else the Release's own namespace. A file without a Release
has none of its own; `nyl apply` and `nyl diff` then use `--namespace`, and
`nyl render` leaves documents without a namespace in no namespace.

A ConfigMap or Secret without a namespace is in the deployed namespace too, so it
makes no difference whether the documents spell the namespace out. A resource in
another namespace is not the object the Pod mounts, and cannot be hashed.

If a ConfigMap is rendered twice in the namespace, once with `metadata.namespace`
and once without, the reference is ambiguous and fails.

## Where it runs

The step runs once per Release, on the final documents: after templates,
components, Helm charts, [Kyverno policies](/nyl/kubernetes-rendering/validation/kyverno-policies/),
and deduplication. It reads no cluster and no file outside those documents, and
it does not depend on their order. `nyl render`, `nyl diff`, `nyl apply`,
`nyl render-tree`, and `nyl publish-tree` take the same
[rendering pipeline](/nyl/kubernetes-rendering/pipeline/), so they write the same
digest, and the published tree holds the annotation as any other manifest field.
The render cache stores it with the rendered Release, keyed by the same inputs as
the rest of the render plus the destination namespace.

`--only-source-kind` renders a part of the Release, which may lack the documents a
checksum reads, and a digest of a part would differ from the one every other
command writes. A render with that flag fails with
`NYL-KUBERNETES-CHECKSUM-PARTIAL` when a remaining document sets
`k8s.nyl/checksum`; drop the flag.

## Secrets

A Secret can be hashed like a ConfigMap, and Nyl neither refuses nor warns about
it. The digest covers only resources rendered in the same Release, and those are
published in the same output as the Secret itself, in the rendered tree or on
standard output. A digest of data that is in that output reveals nothing the
output does not already hold.

Nyl does not mask Secret data in its output: `nyl render` prints it and `nyl diff`
shows it. Keep a Secret that
must not appear in rendered output out of the Release, for example by letting an
in-cluster operator create it, and reference it from the Pod spec: `auto` skips
what the Release does not render.

## Errors

Every error carries a stable code and the fix, and names the source document of
the workload:

```console
$ nyl render bad.yaml --offline
ERROR nyl: Render error:
Source: bad.yaml (document 3)
Resource: apps/v1 Deployment glance
Caused by: NYL-KUBERNETES-CHECKSUM-NOT-FOUND: Deployment glance/glance: k8s.nyl/checksum lists ConfigMap/glance-settings, but the Release renders no such resource in namespace "glance"
Hint: Render the resource in the same Release, in the workload's namespace, or correct its name. Nyl hashes only rendered documents and never reads the cluster; use `auto` to skip references the Release does not render.
```

| Code | Cause |
| --- | --- |
| `NYL-KUBERNETES-CHECKSUM-MALFORMED` | The value is not `auto` or a list of `Kind/name[#key]` with the kind `ConfigMap` or `Secret`, has an empty entry, or is not a string. |
| `NYL-KUBERNETES-CHECKSUM-NOT-FOUND` | An explicit entry names a ConfigMap or Secret that the Release does not render in the workload's namespace, or a key it lacks. |
| `NYL-KUBERNETES-CHECKSUM-AMBIGUOUS` | A reference matches more than one rendered document, or a ConfigMap key is in both `data` and `binaryData`. |
| `NYL-KUBERNETES-CHECKSUM-NO-POD-TEMPLATE` | `k8s.nyl/checksum` is on a document that has no Pod template, or on a workload's own `metadata.annotations` (a CronJob's `spec.jobTemplate.metadata.annotations` too) instead of its Pod template's. |
| `NYL-KUBERNETES-CHECKSUM-RESERVED` | A document sets `k8s.nyl/checksum-sha256` without `k8s.nyl/checksum` on the same Pod template, or outside a Pod template. Nyl owns the key; remove it. |
| `NYL-KUBERNETES-CHECKSUM-INVALID-DATA` | A referenced object has data that cannot be read: a value that is not a string, or base64 text that does not decode. Base64 may contain line breaks, as Kubernetes allows. |
| `NYL-KUBERNETES-CHECKSUM-PARTIAL` | `--only-source-kind` is set and a remaining document sets `k8s.nyl/checksum`. Drop the flag. |
| `NYL-KUBERNETES-CHECKSUM-EMPTY` | A warning, not an error: `auto` found no ConfigMap or Secret the Release renders, so the digest covers nothing. |
