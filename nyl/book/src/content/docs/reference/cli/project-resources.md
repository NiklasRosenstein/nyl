---
title: 'get and update'
---

## Inspect project resources

`nyl get` reports GitOps control resources, Environments, and units discovered
in project source:

```bash
nyl get repositories
nyl get clusters
nyl get argocd-instances
nyl get targets
nyl get app-projects
nyl get application-groups
nyl get environments
nyl get units
nyl get cluster primary
```

Plural resource names are canonical and singular aliases select the same kind.
An optional name filters the result. Output contains the resource name, source
file and document, and concise kind-specific coordinates. The command does not
render workloads, resolve remote sources, or contact Kubernetes.

Each kind prints one table row per resource:

```console
$ nyl get clusters
NAME     DESTINATION                       CONTEXT        VERSION  FILE
primary  https://primary.example.com:6443  admin@primary  1.32.2   config/clusters/primary.yaml#document-1
staging  https://staging.example.com:6443  -              1.31.4   config/clusters/staging.yaml#document-1
$ nyl get targets
NAME        CLUSTER  PUBLICATION      PATH-PREFIX  FILE
production  primary  platform@deploy  production   config/targets/production.yaml#document-1
staging     staging  platform@deploy  staging      config/targets/staging.yaml#document-1
$ nyl get application-groups
NAME        PROJECT                 SOURCE                   FILE
monitoring  monitoring (generated)  applications/monitoring  applications/monitoring/group.yaml#document-1
web         web (generated)         applications/web         applications/web/group.yaml#document-1
```

`CONTEXT` is the Cluster's `spec.live.context`, `PUBLICATION` the target's
repository and revision, and `PROJECT` the ApplicationGroup's AppProject, marked
`(generated)` when Nyl derives it.

## Inspect environment state

With `-e <environment>`, `nyl get` reads that environment's orchestration
state instead of source. It only reads: it needs read access to the state
repository, never takes the lease, and needs no commit identity.

```bash
nyl get units -e dev                          # NAME KIND STATE RECEIPT LAST RUN DETAIL
nyl get units -e dev -o wide                  # ... and TEARDOWN READINESS
nyl get unit migrate -e dev -o yaml           # the persisted desired and observed documents
nyl get unit migrate -e dev -o json --observed
nyl get output database/host -e dev           # resolves like {fromUnit: {unit: database, output: host}}
nyl get artifacts -e dev                      # UNIT NAME KIND SUMMARY
nyl get artifact web/image -e dev --pointer /reference
nyl get units -e dev --state-revision 41f0c2e
nyl get units -e dev --local                  # this clone's local view
```

- `get units -e` joins desired and observed state into one row per unit:
  its kind, its [state](/nyl/reference/cli/status/), whether its latest
  receipt is current, the run of its latest receipt or condition, and a
  detail such as what a blocked unit waits for or the changes a held unit
  keeps pending. A held marker, which has no kind, shows `-` as its kind,
  and a held unit with a condition shows it, such as `held (uncertain)`.
  `-o wide` adds the teardown readiness of units whose kind reports
  teardown settings, such as a `KubernetesPublication`: `ready`, or
  `incomplete:` with the unmet requirements, and `-` for other kinds.
- `get unit <name> -e <env> -o yaml|json` prints exactly the persisted
  `{desired, observed}` documents, never a synthesized merge. `--desired` or
  `--observed` prints only one of them.
- `get output <unit>/<output>` and `get artifact <unit>/<name> --pointer …`
  print one value for scripts and resolve exactly like the matching
  `fromUnit` reference: the pointer is relative to the artifact's `spec`, and
  only a current receipt counts. When the reference would not resolve, they
  print nothing and exit 1. `--allow-stale` prints the latest receipt's value
  with the warning `NYL-REFERENCE-STALE` instead.
- `--state-revision <commit>` reads state as of an earlier state commit.
- `--local` reads the configured local state repository without contacting the
  remote. It cannot be combined with `--state-revision`.
- `-o` selects `table` (default), `wide`, `yaml`, `json`, or `name`.

Every form takes the project directory, or a path beneath it, as its last
argument, default `.`. Document and table forms exit 0 when they can read the
requested state and 1 otherwise, for example with `NYL-STATE-MISSING`.

## Update cluster capabilities

Refresh `Cluster.spec.kubernetes` from the live cluster:

```bash
nyl capture cluster primary
nyl capture cluster primary --context admin@primary
nyl capture cluster primary --check
```

The explicit context wins over `Cluster.spec.live.context`. The update changes
only the selected Cluster document and preserves the rest of a shared YAML
file. `--check` reports drift without writing.

## Update source locks

Resolve remote ApplicationGroup sources, `fromGit` Release inputs, Environment
sources, and native unit sources (including named image contexts), and update
their full commit locks:

```bash
nyl update source-locks
nyl update source-locks workloads
nyl update source-locks --target production
nyl update source-locks --check
```

A group name selects that ApplicationGroup and `--target` selects one
DeploymentTarget's `fromGit` locks; without either, every lock is updated. The
filters choose which locks move, never where: each lock's destination depends
only on the lock, so a filtered update agrees with an unfiltered `--check`.
Every lock moves to the head of its revision. Each repository is fetched once
per run. Locks are edited in place by their
position, so write source locks and `fromGit` bindings in block style. Unit
source locks require literal repository coordinates, revisions, and commits.
Nyl discovers source fields through each registered driver and warns when a unit
cannot render without an environment; its locks are skipped. Unfiltered updates
include renderable units, while group and target filters select only
the named control resources. A revision without a commit follows its tip and
has no lock to refresh.

Mutable revisions must refresh successfully before a lock is reported current
or updated. Immutable commit rendering can use an existing local cache.
