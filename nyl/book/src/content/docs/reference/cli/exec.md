---
title: 'exec'
---

Runs the native operator tool of a held unit. The command dispatches through
the unit driver's `exec` capability; Terraform and OpenTofu prepare their
configuration and native backend before running the arguments after `--`.

```bash
nyl hold -e dev --unit database --reason 'repair native state'
nyl exec -e dev --unit database --reason 'inspect state' -- state list
nyl exec -e dev --unit database --worktree --reason 'move backend' -- init -migrate-state
nyl resume -e dev --unit database
nyl plan -e dev --unit database -o json
nyl reconcile -e dev --unit database --approve database=sha256:…
```

Without `--worktree`, the tool uses the deployed desired spec and its pinned
source files. With it, Nyl renders the current checkout, resolves references
from current receipts, and copies Git-visible files into a private snapshot.
Uncommitted and untracked files are included; ignored files and symlinks are
excluded or refused. Explicit source pins continue to read their pinned trees.
The native tool cannot write into the operator's checkout.

The unit must be held and its kind must support `exec`. Nyl takes the
environment lease and refuses a spec whose resource scopes overlap another
unit's. Credentials come from the unit's admitted environment and secrets.
`--path` selects the project; `--timeout` overrides the unit's timeout, and
`--wait-lease` waits for its lease. `--author` selects the Git commit identity.

Before starting the tool, Nyl commits the repair marker so a lost runner cannot
leave an unrecorded native change. A second observed transition records the
tool's status, arguments, operator, reason, spec digest, changed field paths,
and source snapshot metadata. Tool output and source file contents stay out
of these records. Native stdout is forwarded, so `state pull` can be redirected
to a backup file. An interactive tool receives the terminal only when stdin
is a terminal.

While held, consumers may continue reading the existing receipt. After resume,
the next execution waits for explicit approval even if its execution key is
unchanged. A kind with plan digests requires the digest shown by `plan`; a
successful execution clears the repair marker.

Refusals before a tool starts exit **125**, with a stable code such as
`NYL-EXEC-NOT-HELD`, `NYL-EXEC-UNSUPPORTED`, or `NYL-EXEC-SCOPE-CONFLICT`.
After the native tool runs, the command returns its exit status, or 128 plus
its terminating signal. A failed audit write is also reported on stderr; the
marker committed before the tool remains in place.

