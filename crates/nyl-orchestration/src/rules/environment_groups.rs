//! Which EnvironmentGroup supplies an environment's shared defaults.
//!
//! Contract: [Environment groups](../../../../design/orchestration-core.md#environment-groups).
//! Groups that only select may overlap. An environment selected by more
//! than one group that carries shared fields is an error naming the groups,
//! because the order of their defaults would be arbitrary.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::resources::environment::{Environment, EnvironmentGroup};

/// The group whose shared fields apply to `environment`, if any.
pub fn sharing_group<'a>(
    environment: &Environment,
    groups: &'a [EnvironmentGroup],
) -> Result<Option<&'a EnvironmentGroup>, Finding> {
    let sharing = groups
        .iter()
        .filter(|group| group.has_shared_fields() && group.selects(&environment.metadata.labels))
        .collect::<Vec<_>>();
    match sharing.as_slice() {
        [] => Ok(None),
        [group] => Ok(Some(group)),
        several => Err(Finding::new(
            Code::EnvironmentGroupOverlap,
            format!(
                "Environment {} is selected by EnvironmentGroups {} that all carry shared fields",
                environment.metadata.name,
                several
                    .iter()
                    .map(|group| group.metadata.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "narrow the groups' environmentSelector, or keep shared fields in one of them",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(name: &str, spec: &serde_json::Value) -> EnvironmentGroup {
        serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "EnvironmentGroup", "metadata": {"name": name}, "spec": spec
        }))
        .unwrap()
    }

    #[test]
    fn test_only_one_sharing_group_may_select_an_environment() {
        let environment: Environment = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Environment",
            "metadata": {"name": "prod", "labels": {"stage": "production", "cluster": "prod-1"}}
        }))
        .unwrap();
        let selecting = group(
            "all-production",
            &json!({"environmentSelector": {"matchLabels": {"stage": "production"}}}),
        );
        let sharing = group(
            "production",
            &json!({"environmentSelector": {"matchLabels": {"stage": "production"}}, "values": {"tier": "prod"}}),
        );
        let groups = [selecting.clone(), sharing.clone()];
        assert_eq!(
            sharing_group(&environment, &groups).unwrap().unwrap().metadata.name,
            "production"
        );
        let other = group(
            "prod-1",
            &json!({"environmentSelector": {"matchLabels": {"cluster": "prod-1"}}, "protectedRefs": ["main"]}),
        );
        let finding = sharing_group(&environment, &[selecting, sharing, other]).unwrap_err();
        assert_eq!(finding.code, Code::EnvironmentGroupOverlap);
        assert!(finding.message.contains("production, prod-1"), "{finding}");
    }
}
