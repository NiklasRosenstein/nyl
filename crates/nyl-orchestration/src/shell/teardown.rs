//! `nyl teardown -e <env> (--unit <u>… | --all) [--hold] [--confirm-removed]`
//! and `nyl state delete -e <env> [--teardown]`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Explicit teardown" and holds, [Instances](../../../../design/orchestration-core.md#instances)
//! (`teardown --all`, `state delete`, decommissioning a declared
//! environment), [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (`--confirm-removed`, the signal path), and [Commands](../../../../design/orchestration-core.md#commands).
//!
//! `teardown` runs on the shared run path ([`super::run`]) with
//! [`RunMode::Teardown`](super::run::RunMode::Teardown): it takes the lease
//! (and takes a broken lease over, importing its run), acts on the stored
//! desired documents, writes one transition commit per state ref with
//! operation `teardown`, and reports a `TeardownReport`. A held unit is not
//! torn down: the run records the request on its held file and reports it
//! held with a pending teardown (exit 0); after `nyl resume` the next
//! teardown-capable run carries it out. `--all` and `state delete
//! --teardown` honour `deletionPolicy: Retain`: Retain units and retained
//! tombstones are released and listed in the commit.
//!
//! `state delete` refuses while a unit has an incarnation
//! (`NYL-STATE-DELETE-INCARNATIONS`) and otherwise writes one commit per
//! state ref removing the environment's other state files
//! ([`TransitionChanges::clear`](nyl_core::orchestration::ports::TransitionChanges::clear))
//! and leaving `state.yaml` with a `state-deleted` event as the deleted
//! marker; with `--teardown` it runs `teardown --all` first, in the same
//! command. A plain `nyl state init` on a deleted location refuses with
//! `NYL-STATE-DELETED`, read from the marker.

use super::run::{RunContext, RunRequest};
use crate::report::StateOperationReport;
use crate::run_report::RunReport;

/// `nyl teardown`: `request.mode` is [`RunMode::Teardown`](super::run::RunMode::Teardown).
pub fn teardown(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let _ = (context, request);
    todo!("operator")
}

/// `nyl state delete`, with `teardown` for `--teardown`. `request` carries
/// the lifecycle options, `--source`, `--reason`, `--wait-lease`, and the
/// cancellation.
pub fn state_delete(context: &RunContext<'_>, request: &RunRequest, teardown: bool) -> StateOperationReport {
    let _ = (context, request, teardown);
    todo!("operator")
}
