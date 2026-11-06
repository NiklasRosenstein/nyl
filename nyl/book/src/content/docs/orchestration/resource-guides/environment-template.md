---
title: 'Using EnvironmentTemplate'
---

See the [EnvironmentTemplate resource reference](/nyl/reference/resources/gitops.nyl/v1/environment-template/) for the API, example, and field definitions.

An EnvironmentTemplate describes what a preview environment looks like, such
as one environment per pull request. Source holds the template; each
**instance** is created from it with parameters and exists only in state, in
the `state.yaml` that `nyl state init --template` writes. Afterwards an
instance behaves like a declared Environment for every command: `plan`,
`reconcile`, `status`, `get`, `attest`, `observe`, `teardown`, `state delete`,
and `build`.

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata:
  name: preview
spec:
  parameters:
    pr: {type: integer}
  unitSelector:
    matchLabels: {preview: 'true'}
  values:
    hostname: 'pr-{{ params.pr }}.preview.example.com'
  state:
    repositoryRef: {name: platform-state}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
```

## Creating and updating an instance

```console
$ nyl state init -e pr-123 --template preview --param pr=123
$ nyl reconcile -e pr-123 --template preview --param pr=123
```

`state init` writes the instance's record without reconciling it;
`reconcile --template` is what a pull request job runs: it creates or updates
the instance from its own checkout and then reconciles it. Both record the
template name, the bound parameters (defaults included), the entry
checkout's commit as the instance's source, and an `expiresAt` of now plus
`ttl`. Run again, they update the parameters. `--param name=value` parses the
value as YAML and checks it against the declaration, so `--param pr=123` is an
integer. A parameter without a `default` is required.

Parameters are stored in plain text in the instance's `state.yaml`, which
anyone who can read the state ref can read, so never pass a secret as a
parameter; give units secrets through their `env`.

The name must not belong to a declared Environment or to an instance of
another template (`NYL-INSTANCE-NAME-TAKEN`), because leases, run refs, and
state directories are named after it. A deleted instance refuses with
`NYL-STATE-DELETED` until `nyl state init --fresh` starts it over.

Every later command names the instance with `-e`:

```console
$ nyl reconcile -e pr-123
$ nyl get environments
```

An instance is found only while its template exists at the entry checkout: a
checkout that no longer declares the template reports
`NYL-INSTANCE-TEMPLATE-UNKNOWN`. Renaming a template, or changing where its
`state` puts instances, goes through [`nyl state move
--template`](#moving-and-renaming-a-template).

A plain `reconcile -e pr-123` from any checkout renders the instance at the
source commit its record names, not at the checkout, and records which commit it
used; only `reconcile --template`, entered from the pull request's own
checkout, moves the recorded commit. `get environments` lists the declared
environments and the live instances with their expiry, marking an expired one.

## What renders with the parameters

Every string of `values`, `source`, and `protectedRefs` that contains template
syntax renders once, when an instance is created or loaded, with `params` and
`environment.name`. A unit then sees the rendered values, so a `values.target`
object that declares a publication's inline target can name its prefix after
the instance, and a unit places it with `target: {{ values.target | tojson }}`.
A document that is not YAML before rendering needs a block-style envelope; see
[units](/nyl/orchestration/environments-and-state/).

`state` is different: it locates the `state.yaml` that holds the parameters,
so it renders with `environment.name` alone. With `path: '{{ environment.name }}'`
every instance of the template, and every declared environment that names the
same refs, shares one ref and keeps its own directory, which also lets Nyl
list the instances by listing the ref. A template whose refs or repository
depend on the name works for every command that names the instance, but its
instances are not listed.

## Deletion policy and teardown

`deletionPolicy: Teardown` overrides every unit's own policy, an explicit
`Retain` included, for units whose kind supports teardown. Units of other
kinds, such as `OciImage`, are left in place and dropped from state when the
instance is removed. `allowTeardown: true` lets a reconcile tear down a unit
that left the template or its selection without `--allow-teardown`, because
nobody is there to pass it for a preview.

Remove an instance with `nyl state delete -e pr-123 --teardown`: it tears
down every unit, dependents first, and removes the instance's files from the
shared ref, leaving only the deleted marker in its `state.yaml`. See
[Deletion](/nyl/orchestration/deletion/).

## Validation

Which instances exist is state, which `nyl validate` does not read. It checks
each template through sample instances, with parameters taken from `enum`
values or generated by type, so a template whose instances would collide
fails before any exists: two instances sharing a state location, publication
prefixes that overlap, or generated Argo CD Application and AppProject names
that match each other or another target's. Every publication render of every
environment also includes the inline targets of the instances state lists, so
a collision with a real instance is found whichever command renders. Template
the names with `environment.name` or `target.metadata.name`, for example
`applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'`.

A direct `nyl build --push` refuses to publish a target an instance owns. If a
template's state cannot be read, its instances are unknown, so a teardown, an `exec`,
or a run of a unit that claims native resources refuses with `NYL-STATE-UNAVAILABLE`
instead of checking without them. A deleted instance's name still answers the
owner fence of the prefix it published, as an owner that no longer exists.

## Expiry, limits, and the fleet

Nyl has no daemon, so expiry works through the commands people already run:

```yaml
spec:
  ttl: 7d
  maxInstances: 20
  allowTeardown: true
```

- **Activity extends.** `state init --template`, the pull request job's
  `reconcile -e pr-123 --template preview --param pr=123`, and every other
  `reconcile -e pr-123` move `expiresAt` to now plus `ttl` when they start,
  whether or not the run succeeds, so a pull request that keeps pushing keeps
  its preview even while the preview fails. `plan`, `status`, `get`, and
  `teardown` never extend it, and `reconcile -e pr-123 --no-extend` is
  maintenance that does not.
- **Expired instances are removed by the next reconcile that reaches them**:
  a plain or `--no-extend` `reconcile -e pr-123`, or the scheduled
  `reconcile --template preview`, which maintains the live instances at their
  own recorded commits, never extends any, and removes the expired. Removal is
  `teardown --all` and then `state delete`, so nothing is deleted while a unit
  still has resources. `allowTeardown: true` authorizes the teardown;
  without it, or `--allow-teardown`, the run records the units as pending
  teardown and exits 2. A removal that waits (a publication's `teardownWait`,
  an approval, unmet readiness) or fails leaves the state and exits like the
  teardown does, and a later run continues it. `--renew` keeps an expired
  instance, as does the pull request job's `reconcile --template`: updating an
  active pull request never removes its preview.
- **`maxInstances`** counts the template's instances, expired ones until they
  are removed, when one is created. At the limit `state init --template` and
  `reconcile --template` exit 2 (`NYL-INSTANCE-LIMIT`) and write nothing, naming
  the expired instances that could go: removing them is the scheduled job's
  work, not a side effect of another pull request's job. `--make-room` opts
  in: it removes the expired instances with the oldest expiry first, only as
  many as needed and never a live one, and then creates the instance; too few
  expired instances, or a removal that does not finish, still refuse. Nyl counts
  by listing, so `maxInstances` needs `state.path: '{{ environment.name }}'`
  and refs and a repository that do not read the name (`nyl validate` rejects
  the template otherwise). Two jobs creating at once can exceed the limit by
  one.
- **The fleet job.** A scheduled job runs `nyl reconcile --template preview`
  from the default branch. It needs the same preview credentials as the pull
  request jobs. An instance whose lease another run holds is reported as
  `busy` and does not fail the job, and a later run removes it if it is
  expired. Removing an expired instance renders nothing from source, so a pull
  request branch that is gone does not keep it alive; the teardown of a unit
  that needs its files does, until `--source <revision>` supplies them.

A removal waits for each publication's `teardownWait`, and the default
`manual` waits for a person to confirm with `--confirm-removed`. A template
whose previews should expire unattended sets `observe` or `delay` on its
publications; `state init --template` warns (`NYL-TEARDOWN-WAIT-MANUAL`)
about a `manual` wait and about every unmet teardown requirement of the units
it can resolve before their producers ran, such as a catalog that does not
prune (`NYL-TEARDOWN-CATALOG-MANUAL-SYNC`). `plan` shows the rest once the units
resolve.

## Source keep refs

Teardown needs the instance's source files, and a closed pull request's branch
may be gone. With `keepSource`, the default, each instance's recorded source
commit is kept fetchable through the ref `refs/nyl/<instance>/keep` (under the
state's `coordinationRefPrefix`) in the source repository. `state init
--template` and `reconcile --template` move it before they record the commit
and refuse when the push is refused (`NYL-INSTANCE-KEEP-FAILED`), so the preview
job needs push access to `refs/nyl/*`. Every command that needs the recorded
commit fetches it from the ref when the clone lacks it, and `state delete`
removes the ref once the instance is gone. `keepSource: false` keeps nothing;
the pipeline must then remove instances before their branches disappear, or
tear them down with `--source <revision>`. A run with `--local` pushes no
ref.

## Moving and renaming a template

An instance is found at the location its template names, under the template's
name. After changing the template's `state` or renaming it, move the instances
with one command (see [`nyl state move`](/nyl/reference/cli/state/#moving-every-instance-of-a-template)):

```console
$ nyl state move --template preview --state-path 'previews/{{ environment.name }}'
$ nyl state move --template previews --from-template preview
```

## Not yet implemented

- `catalogApplication.shared`, one catalog for every instance: each instance
  publishes a catalog Application of its own.
- Cross-environment references to declared environments.

## See also

- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)
- [EnvironmentGroup](/nyl/reference/resources/gitops.nyl/v1/environment-group/)
