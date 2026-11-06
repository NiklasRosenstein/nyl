//! The keep refs of an instance's source commit.
//!
//! Contract: [Instances](../../../../design/orchestration-core.md#instances),
//! `keepSource`: with `keepSource` (the default) Nyl keeps each instance's
//! recorded source commit fetchable with a keep ref in the source repository,
//! so teardown after the pull request branch is deleted or squash-merged
//! still finds its source.
//!
//! The names are what keeps two instances, projects, and Nyl versions from
//! disturbing each other:
//!
//! - `refs/nyl-keep/<keepId>/<commit>`: one ref per commit that is never
//!   moved. A new commit is kept first, the record that names it is written,
//!   and only then are the instance's other refs removed, so the recorded
//!   commit is never unkept.
//! - `<keepId>` is minted from the injected id generator when an instance
//!   first keeps a commit and recorded in its `state.yaml`. It is unique
//!   across projects of one source repository, and survives moves of the
//!   state and of the project directory and renames of the template, because
//!   nothing about the name is derived from where the instance lives.
//! - The namespace is reserved: a state ref under it is a conflict
//!   ([`reserved_conflict`]).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, Uid};
use nyl_core::orchestration::state::StateLocation;

/// The namespace of every keep ref.
pub const KEEP_NAMESPACE: &str = "refs/nyl-keep/";

/// The prefix of the keep refs of the instance whose record holds `keep_id`.
pub fn keep_prefix(keep_id: &Uid) -> String {
    format!("{KEEP_NAMESPACE}{keep_id}")
}

/// The keep ref of `commit` under `prefix`.
pub fn keep_ref(prefix: &str, commit: &CommitId) -> String {
    format!("{prefix}/{commit}")
}

/// The commit a keep ref under `prefix` is for, from its name.
pub fn kept_commit<'a>(prefix: &str, reference: &'a str) -> Option<&'a str> {
    reference
        .strip_prefix(prefix)?
        .strip_prefix('/')
        .filter(|commit| !commit.contains('/'))
}

/// Why a state location may not use the keep namespace: its refs would
/// collide with keep refs wherever the source repository is the state's.
pub fn reserved_conflict(environment: &EnvironmentName, location: &StateLocation) -> Option<Finding> {
    let used = [
        StateLocation::full_name(&location.desired_ref),
        StateLocation::full_name(&location.observed_ref),
        location.coordination_ref_prefix.clone(),
    ]
    .into_iter()
    .find(|name| name.starts_with(KEEP_NAMESPACE))?;
    Some(Finding::new(
        Code::StateRefConflict,
        format!(
            "{environment} uses {used}, which is inside {KEEP_NAMESPACE}, the namespace of instances' source keep refs"
        ),
        format!("configure state refs and a coordinationRefPrefix outside {KEEP_NAMESPACE}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> EnvironmentName {
        EnvironmentName::new(text).unwrap()
    }

    /// Catches keep refs that depend on where an instance lives, which would
    /// strand them when its state or project moves, or that collide between
    /// instances.
    #[test]
    fn test_keep_refs_are_named_by_keep_id_and_commit() {
        let commit = CommitId::new("a".repeat(40)).unwrap();
        let first = keep_prefix(&Uid::new("00000000-0000-7000-8000-000000000001").unwrap());
        let second = keep_prefix(&Uid::new("00000000-0000-7000-8000-000000000002").unwrap());
        assert_ne!(first, second);
        assert_eq!(first, "refs/nyl-keep/00000000-0000-7000-8000-000000000001");
        let reference = keep_ref(&first, &commit);
        assert_eq!(reference, format!("{first}/{commit}"));
        assert_eq!(kept_commit(&first, &reference), Some(commit.as_str()));
        assert_eq!(kept_commit(&first, &format!("{second}/{commit}")), None);
        assert_eq!(kept_commit(&first, &format!("{reference}/more")), None);
    }

    /// Catches a state location inside the namespace the keep refs own.
    #[test]
    fn test_state_refs_inside_the_keep_namespace_conflict() {
        let location = |desired: &str, prefix: &str| StateLocation {
            repository: "local".to_owned(),
            desired_ref: desired.to_owned(),
            observed_ref: desired.to_owned(),
            path: String::new(),
            coordination_ref_prefix: prefix.to_owned(),
        };
        assert!(reserved_conflict(&name("dev"), &location("nyl/dev", "refs/nyl/")).is_none());
        assert!(reserved_conflict(&name("dev"), &location("refs/nyl-keep/x", "refs/nyl/")).is_some());
        assert!(reserved_conflict(&name("dev"), &location("nyl/dev", "refs/nyl-keep/")).is_some());
    }
}
