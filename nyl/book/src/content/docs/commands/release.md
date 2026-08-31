---
title: 'release'
---

Inspect and manage releases tracked by `nyl apply`.

## Synopsis

```bash
nyl release <SUBCOMMAND> [OPTIONS]
```

## Description

Every `nyl apply` records a release revision, storing the full rendered manifest
and the set of applied resources in a Kubernetes Secret in the release namespace.
The `release` subcommands let you inspect that history and roll back to a previous
revision.

## Subcommands

- [`list`](#list) - List all releases
- [`show`](#show) - Show details of a specific release
- [`history`](#history) - Show the revision history for a release
- [`rollback`](#rollback) - Roll back a release to a previous revision
- [`delete`](#delete) - Delete release(s)

### list

```bash
nyl release list [OPTIONS]
```

List tracked releases, optionally filtered by namespace.

### show

```bash
nyl release show <NAME> --namespace <NAMESPACE> [OPTIONS]
```

Show the details of a release. By default the latest revision is shown; pass
`--revision <N>` for a specific revision and `--manifest` to include the full
rendered manifest.

### history

```bash
nyl release history <NAME> --namespace <NAMESPACE> [OPTIONS]
```

Show the revision history of a release, including each revision's status and
timestamps.

### rollback

```bash
nyl release rollback <NAME> --namespace <NAMESPACE> [OPTIONS]
```

Re-apply the manifest stored for a previous revision and record it as a **new**
revision. Rollback reuses the same apply path as `nyl apply`: it applies the
stored manifest with server-side apply, marks the previously deployed revision
`Superseded`, and prunes resources that existed in the superseded revision but are
not part of the rolled-back revision.

For example, if revision 4 is currently deployed, rolling back to revision 2
creates revision 5 whose content is identical to revision 2, marks revision 4 as
`Superseded`, and removes any resources that revision 4 added on top of revision 2.

#### Options

- `-n, --namespace <NAMESPACE>` - Release namespace (required)
- `-r, --revision <REVISION>` - Revision to roll back to. Defaults to the revision
  immediately before the latest one (i.e. undo the most recent deployment).
- `-y, --yes` - Skip the confirmation prompt
- `--context <CONTEXT>` - Kubernetes context to use

#### Examples

```bash
# Roll back to the revision before the current one (with confirmation)
nyl release rollback my-app --namespace default

# Roll back to a specific revision, skipping the confirmation prompt
nyl release rollback my-app --namespace default --revision 2 --yes
```

#### Notes

- Rollback always creates a new revision rather than mutating history, so the full
  audit trail is preserved and a rollback can itself be rolled back.
- Pruning matches `nyl apply` semantics: resources present in the previously
  deployed revision but absent from the rolled-back revision are deleted.
- A clear error is returned if the release or the requested revision does not
  exist, or if there is no previous revision to roll back to.

### delete

```bash
nyl release delete <NAME>... --namespace <NAMESPACE> [OPTIONS]
```

Delete one or more releases (or a specific revision) and, by default, the
resources they created.

## See Also

- [`apply`](/nyl/commands/apply/) - Apply manifests and record release revisions
- [Release resource](/nyl/reference/resources/release/) - Release metadata
