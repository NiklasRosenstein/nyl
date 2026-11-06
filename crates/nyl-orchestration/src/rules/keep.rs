//! The keep ref of an instance's source commit.
//!
//! Contract: [Instances](../../../../design/orchestration-core.md#instances),
//! `keepSource`: with `keepSource` (the default) Nyl keeps each instance's
//! latest source commit fetchable with a keep ref in the source repository,
//! `refs/nyl/<instance>/keep` under the default `coordinationRefPrefix`,
//! moved whenever the instance's recorded source commit moves and removed by
//! `state delete`, so teardown after the pull request branch is deleted or
//! squash-merged still finds its source.
//!
//! The ref is named after the instance exactly like its lease and run refs,
//! which is why environment names are unique across declared Environments and
//! instances. It never collides with them: its last segment is `keep`.

use nyl_core::orchestration::ids::EnvironmentName;

/// The keep ref of `environment` under the state's `coordination_ref_prefix`.
pub fn keep_ref(coordination_ref_prefix: &str, environment: &EnvironmentName) -> String {
    format!("{coordination_ref_prefix}{environment}/keep")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a keep ref outside the instance's own coordination namespace,
    /// which would make branch protection and cleanup rules miss it.
    #[test]
    fn test_the_keep_ref_lives_beside_the_instances_lease_under_the_prefix() {
        let environment = EnvironmentName::new("pr-123").unwrap();
        assert_eq!(keep_ref("refs/nyl/", &environment), "refs/nyl/pr-123/keep");
        assert_eq!(keep_ref("refs/ci/", &environment), "refs/ci/pr-123/keep");
    }
}
