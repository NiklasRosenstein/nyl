//! Local state branches and `--local` runs.
//!
//! Contract: [Local runs](../../../../design/orchestration-core.md#local-runs)
//! and [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves).
//!
//! - A `--local` run reads and writes the clone's local branches of the
//!   remote state refs, the local view of the location ([`view`]), and marks
//!   its commits `Nyl-Local: true`.
//! - A run against remote state refuses when a remote state ref's tip is a
//!   `Nyl-Local` commit, naming the last commit without the trailer
//!   ([`check_tip`], `NYL-STATE-LOCAL-TIP`).
//! - Diverged local branches are reported, such as "local state has 2 local
//!   transitions and is 3 behind the remote" ([`divergence_warning`]).
//! - `state reset --local` lists the units whose local receipts the remote
//!   does not reflect and proceeds only with `--discard <unit>…` or
//!   `--discard-all` ([`unreflected`], [`check_discard`]).
//! - `state restore --to <commit>` writes forward commits restoring that
//!   commit's content and never restores `admission.yaml`
//!   ([`restore_changes`]); it records the local transitions it rejects
//!   ([`rejected`]).
//! - A `--local` run refuses while the remote holds run refs of dead runs
//!   that remote state has not taken over yet ([`check_leftovers`],
//!   `NYL-LOCAL-RUN-LEFTOVERS`), because it never imports remote run refs;
//!   the lease it takes at the remote lists no units, since remote runs do
//!   not notice local effects.
//! - `state restore --to` pairs the other state ref through the transition
//!   trailers ([`restore_tips`]).
//! - A local run warns about every unit it executes, naming the remote
//!   command that records it again ([`unrecorded_warning`]), and runs units
//!   whose recovery policy is not `converge` only when named with
//!   `--approve` (part 6; until then they are skipped,
//!   `NYL-LOCAL-RUN-NOT-CONVERGE`).

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry, StateSide, StateTips, TransitionChanges,
};
use nyl_core::orchestration::state::StateLocation;
use nyl_core::orchestration::time::Timestamp;

/// The local view of a remote location: the same refs, path, and prefix in
/// the clone itself. `--local-lease` takes the lease here too.
pub fn view(location: &StateLocation) -> Result<StateLocation, Finding> {
    let _ = location;
    todo!("local")
}

/// Whether a commit message carries the `Nyl-Local: true` trailer.
pub fn is_local_commit(message: &str) -> bool {
    let _ = message;
    todo!("local")
}

/// Refuse when the newest entry of a remote state ref's `log` is a
/// `Nyl-Local` commit, naming the last commit without the trailer.
pub fn check_tip(environment: &EnvironmentName, side: StateSide, log: &[LogEntry]) -> Result<(), Finding> {
    let _ = (environment, side, log);
    todo!("local")
}

/// The local transitions on top of a remote state ref, newest first: what
/// `state restore` records as rejected.
pub fn rejected(log: &[LogEntry]) -> Vec<CommitId> {
    let _ = log;
    todo!("local")
}

/// The warning for diverged local branches; `None` when they follow the remote.
pub fn divergence_warning(environment: &EnvironmentName, divergence: &LocalDivergence) -> Option<Finding> {
    let _ = (environment, divergence);
    todo!("local")
}

/// Units whose receipt in the local view differs from the remote's: what
/// `state reset --local` would forget.
pub fn unreflected(local: &EnvironmentSnapshot, remote: &EnvironmentSnapshot) -> Vec<UnitName> {
    let _ = (local, remote);
    todo!("local")
}

/// `--discard <unit>…` or `--discard-all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discard {
    Units(BTreeSet<UnitName>),
    All,
}

/// Refuse a reset that would forget unreflected units not discarded
/// (`NYL-STATE-RESET-UNDISCARDED`).
pub fn check_discard(
    environment: &EnvironmentName,
    unreflected: &[UnitName],
    discard: Option<&Discard>,
) -> Result<(), Finding> {
    let _ = (environment, unreflected, discard);
    todo!("local")
}

/// The changes that make `current` hold `target`'s content: every document
/// of `target` put, every document absent from it removed. `state.yaml` is
/// carried forward, never restored.
pub fn restore_changes(current: &EnvironmentSnapshot, target: &EnvironmentSnapshot) -> TransitionChanges {
    let _ = (current, target);
    todo!("local")
}

/// `NYL-LOCAL-RUN-UNRECORDED` for a unit a `--local` run executed.
pub fn unrecorded_warning(environment: &EnvironmentName, unit: &UnitName) -> Finding {
    let _ = (environment, unit);
    todo!("local")
}

/// Refuse a `--local` run (`NYL-LOCAL-RUN-LEFTOVERS`) when `coordination`
/// holds a foreign (remote) run ref of another run that is finished or past
/// its deadline: remote state has not taken it over yet, and a local run
/// never imports remote run refs. The fix names `nyl reconcile -e <env>`
/// against remote state. Live foreign run refs block through the lease rule.
pub fn check_leftovers(
    environment: &EnvironmentName,
    run: &RunId,
    coordination: &Coordination,
    now: Timestamp,
) -> Result<(), Finding> {
    let _ = (environment, run, coordination, now);
    todo!("local")
}

/// The tips `nyl state restore --to <to>` restores: `to` is a commit of
/// one side's log (newest first, as [`StateStore::log`](nyl_core::orchestration::ports::StateStore::log)
/// reads it). The other side is the commit carrying the same `Nyl-Run-Id`
/// trailer, else the one `to`'s `Nyl-Read-Desired`/`Nyl-Read-Observed`
/// trailer names. `NYL-STATE-RESTORE-INVALID` when `to` is in neither log,
/// when either chosen commit is a `Nyl-Local` commit, or when the other side
/// cannot be paired. With one shared ref, both tips are `to`.
pub fn restore_tips(
    environment: &EnvironmentName,
    to: &CommitId,
    desired: &[LogEntry],
    observed: &[LogEntry],
    shared: bool,
) -> Result<StateTips, Finding> {
    let _ = (environment, to, desired, observed, shared);
    todo!("local")
}
