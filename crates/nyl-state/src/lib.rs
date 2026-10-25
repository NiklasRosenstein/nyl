#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]

//! Nyl orchestration state.
//!
//! This crate implements [`StateStore`](nyl_core::orchestration::ports::StateStore):
//! the state layout on the desired and observed refs, transition commits,
//! leases, run checkpoints, and compare-and-swap. It knows nothing about
//! drivers, rendering, or orchestration decisions. See
//! `design/implementation-architecture.md`.
//!
//! [`git::GitStateStore`] keeps state in Git repositories, remote or the
//! clone itself; [`memory::MemoryStateStore`] is the in-memory store the
//! contract walkthroughs run against. [`source::GitSourceResolver`]
//! implements the source port: revisions, reachability, and pinned worktrees.

pub mod git;
pub mod memory;
pub mod source;

use nyl_core::orchestration::ports::{StateError, StateResult};
use nyl_core::orchestration::state::StateLocation;

/// Refuse a location whose state refs have no local branches.
fn require_local_branches(location: &StateLocation) -> StateResult<()> {
    for configured in [&location.desired_ref, &location.observed_ref] {
        if !StateLocation::is_branch_ref(configured) {
            return Err(StateError::Unavailable(format!(
                "state ref {configured} is a full ref name outside refs/heads/, which has no local branch in the clone"
            )));
        }
    }
    Ok(())
}
