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
//! - `refs/nyl-keep/<project>/<instance>/<commit>`: one ref per commit that is
//!   never moved. A new commit is kept first, the record that names it is
//!   written, and only then are the instance's other refs removed, so the
//!   recorded commit is never unkept.
//! - `<project>` is a digest of the project's directory in the source
//!   repository, so two projects of one monorepo with state in different
//!   repositories cannot collide on an instance name.
//! - The namespace is independent of `coordinationRefPrefix`, so moving an
//!   instance's state never strands its refs, and reserved: a state ref under
//!   it is a conflict ([`reserved_conflict`]).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use nyl_core::orchestration::state::StateLocation;

/// The namespace of every keep ref.
pub const KEEP_NAMESPACE: &str = "refs/nyl-keep/";

/// The prefix of `environment`'s keep refs in the project at `project_dir`,
/// the worktree-rooted directory of its `nyl.toml`.
pub fn keep_prefix(project_dir: &str, environment: &EnvironmentName) -> String {
    let digest = nyl_core::digest::sha256_hex(project_dir.trim_matches('/').as_bytes());
    format!("{KEEP_NAMESPACE}{}/{environment}", &digest[..12])
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

    /// Catches two projects of one source repository sharing keep refs for an
    /// instance of the same name, and a project whose refs change with its
    /// state location.
    #[test]
    fn test_keep_refs_are_named_by_project_instance_and_commit() {
        let commit = CommitId::new("a".repeat(40)).unwrap();
        let first = keep_prefix("/team-a", &name("pr-1"));
        let second = keep_prefix("/team-b", &name("pr-1"));
        assert_ne!(first, second);
        assert!(first.starts_with("refs/nyl-keep/") && first.ends_with("/pr-1"));
        assert_eq!(
            keep_prefix("team-a/", &name("pr-1")),
            first,
            "the slashes around the directory do not matter"
        );
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
