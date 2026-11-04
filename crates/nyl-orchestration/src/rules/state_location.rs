//! Where an environment's state lives, and whether `state.yaml` agrees.
//!
//! Contract: [Environment](../../../../design/orchestration-core.md#environment),
//! [Environment groups](../../../../design/orchestration-core.md#environment-groups),
//! and [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves).
//!
//! - The repository is the Environment's `state.repositoryRef` or
//!   `state.repository`, else its group's, else the checkout's `origin`, else
//!   the local repository.
//! - Refs default to `nyl/<environment>/desired` and `nyl/<environment>/observed`,
//!   the coordination prefix to `refs/nyl/`, the path to the root.
//! - A full `refs/…` state ref other than a branch cannot be protected by
//!   forges: `NYL-STATE-UNPROTECTED`.
//! - `state.yaml` must exist (`NYL-STATE-MISSING`), name this environment
//!   (`NYL-STATE-ENVIRONMENT-MISMATCH`), not have moved away
//!   (`NYL-STATE-MOVED`), record the configured location
//!   (`NYL-STATE-LOCATION-MISMATCH`), and not carry the deleted marker of
//!   `nyl state delete` (`NYL-STATE-DELETED`, read from the record's last
//!   event, never from history; [Instances](../../../../design/orchestration-core.md#instances)).
//! - Refs of all environments in one repository must be names Git can hold
//!   together: no ref may be a directory of another (`NYL-STATE-REF-CONFLICT`).

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{StateEvent, StateLocation, StateRecord};
use nyl_core::resources::environment::{EnvironmentGroupState, EnvironmentState};
use nyl_core::resources::{InlineGitRepository, LocalReference};

/// The default coordination prefix.
pub const DEFAULT_COORDINATION_PREFIX: &str = "refs/nyl/";

/// What resolving an environment's state location needs.
pub struct LocationInputs<'a> {
    pub environment: &'a EnvironmentName,
    pub state: &'a EnvironmentState,
    /// The state defaults of the EnvironmentGroup with shared fields that selects it.
    pub group: Option<&'a EnvironmentGroupState>,
    /// The credential-free read URL of a project GitRepository, by name.
    pub repositories: &'a dyn Fn(&LocalReference) -> Result<InlineGitRepository, String>,
    /// The credential-free URL of the checkout's `origin` remote.
    pub origin: Option<&'a str>,
}

/// The resolved location, with warnings about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocation {
    pub location: StateLocation,
    /// The write URL when the GitRepository names a `publishURL`.
    pub publish_url: Option<String>,
    pub warnings: Vec<Finding>,
}

pub fn resolve(inputs: &LocationInputs<'_>) -> Result<ResolvedLocation, String> {
    let environment = inputs.environment;
    let state = inputs.state;
    let explicit = if let Some(reference) = &state.repository_ref {
        Some((inputs.repositories)(reference)?)
    } else if let Some(repository) = &state.repository {
        Some(repository.clone())
    } else if let Some(group) = inputs.group {
        match (&group.repository_ref, &group.repository) {
            (Some(reference), _) => Some((inputs.repositories)(reference)?),
            (None, Some(repository)) => Some(repository.clone()),
            (None, None) => None,
        }
    } else {
        None
    };
    let (repository, publish_url) = match explicit {
        Some(repository) => (repository.repo_url, repository.publish_url),
        None => match inputs.origin {
            Some(origin) => (origin.to_owned(), None),
            None => (StateLocation::LOCAL_REPOSITORY.to_owned(), None),
        },
    };
    let location = StateLocation {
        repository,
        desired_ref: state
            .desired_ref
            .clone()
            .unwrap_or_else(|| format!("nyl/{environment}/desired")),
        observed_ref: state
            .observed_ref
            .clone()
            .unwrap_or_else(|| format!("nyl/{environment}/observed")),
        path: state.path.trim_matches('/').to_owned(),
        coordination_ref_prefix: state
            .coordination_ref_prefix
            .clone()
            .unwrap_or_else(|| DEFAULT_COORDINATION_PREFIX.to_owned()),
    };
    let warnings = [&location.desired_ref, &location.observed_ref]
        .into_iter()
        .filter(|reference| !StateLocation::is_branch_ref(reference))
        .map(|reference| {
            Finding::new(
                Code::StateUnprotected,
                format!("state ref {reference} of {environment} is not a branch, so forges cannot protect it"),
                "use a branch name for desiredRef and observedRef, and protect it against force pushes and deletion",
            )
        })
        .collect::<Vec<_>>();
    Ok(ResolvedLocation {
        location,
        publish_url,
        warnings,
    })
}

/// Check the state record at the configured location before using it:
/// [`check_location`], and the deleted marker ([`deleted`]).
pub fn check_record(
    record: Option<&StateRecord>,
    environment: &EnvironmentName,
    configured: &StateLocation,
) -> Result<(), Finding> {
    check_location(record, environment, configured)?;
    match record.and_then(StateRecord::deleted) {
        Some(_) => Err(deleted(environment)),
        None => Ok(()),
    }
}

/// `NYL-STATE-DELETED`: the location's `state.yaml` carries the deleted
/// marker of `nyl state delete`. Every command for the environment fails
/// with it, naming both ways forward, so a pipeline that still runs it
/// fails visibly instead of re-creating anything.
pub fn deleted(environment: &EnvironmentName) -> Finding {
    Finding::new(
        Code::StateDeleted,
        format!("the state of {environment} was deleted with `nyl state delete`"),
        format!(
            "remove the Environment {environment} from source to decommission it, or start it over with `nyl state init -e {environment} --fresh`"
        ),
    )
    .with_title(format!("State for environment '{environment}' was deleted"))
    .with_action("Start over", format!("nyl state init -e {environment} --fresh"))
    .with_action("Decommission", format!("remove Environment {environment} from source"))
}

/// Check that the state record exists, belongs to `environment`, and
/// records the configured location; a deleted location passes (`state init
/// --fresh` starts it over).
pub fn check_location(
    record: Option<&StateRecord>,
    environment: &EnvironmentName,
    configured: &StateLocation,
) -> Result<(), Finding> {
    let Some(record) = record else {
        return Err(Finding::new(
            Code::StateMissing,
            format!(
                "{environment} has no state at {}: no state.yaml on {}",
                describe(configured),
                configured.desired_ref
            ),
            format!(
                "run `nyl state init -e {environment}` to create it, or `nyl state move -e {environment} --from <old location>` if the configuration moved it; a decommissioned environment is removed from source, or started over with `nyl state init -e {environment} --fresh`"
            ),
        ));
    };
    if &record.environment != environment {
        return Err(Finding::new(
            Code::StateEnvironmentMismatch,
            format!(
                "the state at {} belongs to {}, not {environment}",
                describe(configured),
                record.environment
            ),
            "give each environment its own refs or state.path",
        ));
    }
    if let Some(StateEvent::StateMoved { to, .. }) = record.events.last() {
        if to.as_ref() != configured {
            return Err(Finding::new(
                Code::StateMoved,
                format!(
                    "the state of {environment} moved from {} to {}",
                    describe(configured),
                    describe(to)
                ),
                format!("point {environment}'s state configuration at {}", describe(to)),
            ));
        }
    }
    if &record.location != configured {
        return Err(Finding::new(
            Code::StateLocationMismatch,
            format!(
                "the state of {environment} records the location {}, but the configuration names {}",
                describe(&record.location),
                describe(configured)
            ),
            format!(
                "run `nyl state move -e {environment}` with the recorded location's options (--from, --desired-ref, --observed-ref, --path, --coordination-refs) to move it to the configured one"
            ),
        ));
    }
    Ok(())
}

/// Human description of a location.
pub fn describe(location: &StateLocation) -> String {
    let refs = if location.desired_ref == location.observed_ref {
        location.desired_ref.clone()
    } else {
        format!("{} and {}", location.desired_ref, location.observed_ref)
    };
    let path = if location.path.is_empty() {
        String::new()
    } else {
        format!(" at path {}", location.path)
    };
    format!(
        "{} ({refs}{path}, coordination {})",
        location.repository, location.coordination_ref_prefix
    )
}

/// Ref names of environments in one repository that Git cannot hold
/// together, such as a branch `nyl/previews` and an environment `previews`
/// whose desired ref is `nyl/previews/desired`.
pub fn ref_conflicts(locations: &[(EnvironmentName, StateLocation)]) -> Vec<Finding> {
    let mut names: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (environment, location) in locations {
        let prefix = &location.coordination_ref_prefix;
        for name in [
            StateLocation::full_name(&location.desired_ref),
            StateLocation::full_name(&location.observed_ref),
            format!("{prefix}{environment}/lease"),
            format!("{prefix}{environment}/runs/*"),
            format!("{prefix}{environment}/signals/*"),
            format!("{prefix}{environment}/keep"),
        ] {
            let owners = names.entry((location.repository.clone(), name)).or_default();
            if !owners.contains(&environment.to_string()) {
                owners.push(environment.to_string());
            }
        }
    }
    let mut findings = Vec::new();
    for ((repository, name), owners) in &names {
        for ((other_repository, other), other_owners) in &names {
            if repository == other_repository && other.starts_with(&format!("{name}/")) {
                findings.push(Finding::new(
                    Code::StateRefConflict,
                    format!(
                        "{name} ({}) and {other} ({}) in {repository} cannot both exist in Git",
                        owners.join(", "),
                        other_owners.join(", ")
                    ),
                    "rename one environment or configure other state refs or coordinationRefPrefix",
                ));
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::fixtures::env as dev;
    use nyl_core::orchestration::state::{StateRecordKind, StateV1};

    fn no_repositories(reference: &LocalReference) -> Result<InlineGitRepository, String> {
        Err(format!("no GitRepository {}", reference.name))
    }

    #[test]
    fn test_defaults_fall_back_from_environment_to_group_to_origin_to_local() {
        let state = EnvironmentState::default();
        let resolve_with = |group: Option<&EnvironmentGroupState>, origin: Option<&str>| {
            resolve(&LocationInputs {
                environment: &dev(),
                state: &state,
                group,
                repositories: &no_repositories,
                origin,
            })
            .unwrap()
            .location
        };
        let local = resolve_with(None, None);
        assert_eq!(local.repository, StateLocation::LOCAL_REPOSITORY);
        assert_eq!(local.desired_ref, "nyl/dev/desired");
        assert_eq!(local.observed_ref, "nyl/dev/observed");
        assert_eq!(local.coordination_ref_prefix, "refs/nyl/");
        assert_eq!(
            resolve_with(None, Some("https://git.example/p.git")).repository,
            "https://git.example/p.git"
        );
        let group = EnvironmentGroupState {
            repository_ref: None,
            repository: Some(InlineGitRepository {
                repo_url: "https://git.example/state.git".to_owned(),
                publish_url: Some("git@git.example:state.git".to_owned()),
            }),
        };
        assert_eq!(
            resolve_with(Some(&group), Some("https://git.example/p.git")).repository,
            "https://git.example/state.git"
        );
    }

    #[test]
    fn test_unprotectable_state_refs_warn() {
        let state = EnvironmentState {
            desired_ref: Some("refs/nyl-state/dev".to_owned()),
            observed_ref: Some("refs/nyl-state/dev".to_owned()),
            ..EnvironmentState::default()
        };
        let resolved = resolve(&LocationInputs {
            environment: &dev(),
            state: &state,
            group: None,
            repositories: &no_repositories,
            origin: None,
        })
        .unwrap();
        assert!(resolved
            .warnings
            .iter()
            .all(|warning| warning.code == Code::StateUnprotected));
        assert_eq!(resolved.warnings.len(), 2);
    }

    #[test]
    fn test_the_record_must_exist_belong_to_the_environment_and_match_the_location() {
        let location = StateLocation {
            repository: "local".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        assert_eq!(
            check_record(None, &dev(), &location).unwrap_err().code,
            Code::StateMissing
        );
        let mut record = StateRecord {
            api_version: StateV1,
            kind: StateRecordKind,
            environment: EnvironmentName::new("prod").unwrap(),
            template: None,
            params: BTreeMap::new(),
            expires_at: None,
            location: location.clone(),
            events: Vec::new(),
        };
        assert_eq!(
            check_record(Some(&record), &dev(), &location).unwrap_err().code,
            Code::StateEnvironmentMismatch
        );
        record.environment = dev();
        check_record(Some(&record), &dev(), &location).unwrap();
        let mut elsewhere = location.clone();
        elsewhere.coordination_ref_prefix = "refs/heads/nyl-run/".to_owned();
        assert_eq!(
            check_record(Some(&record), &dev(), &elsewhere).unwrap_err().code,
            Code::StateLocationMismatch
        );
    }

    /// Q15: a deleted location refuses every command, naming both ways
    /// forward; a later fresh initialization ends the marker.
    #[test]
    fn test_a_deleted_location_refuses_until_initialized_fresh() {
        let location = StateLocation {
            repository: "local".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        let at = "2026-09-24T10:00:00Z".parse().unwrap();
        let mut record = StateRecord {
            api_version: StateV1,
            kind: StateRecordKind,
            environment: dev(),
            template: None,
            params: BTreeMap::new(),
            expires_at: None,
            location: location.clone(),
            events: vec![StateEvent::StateDeleted {
                at,
                by: "alice".to_owned(),
                reason: None,
            }],
        };
        let finding = check_record(Some(&record), &dev(), &location).unwrap_err();
        assert_eq!(finding.code, Code::StateDeleted);
        assert_eq!(
            finding.presentation.title.as_deref(),
            Some("State for environment 'dev' was deleted")
        );
        assert_eq!(
            finding
                .presentation
                .actions
                .iter()
                .map(|field| (field.label.as_str(), field.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Start over", "nyl state init -e dev --fresh"),
                ("Decommission", "remove Environment dev from source"),
            ]
        );
        assert!(finding.fix.contains("remove the Environment") && finding.fix.contains("--fresh"));
        check_location(Some(&record), &dev(), &location).unwrap();
        record.events.push(StateEvent::StateInitialized {
            at,
            by: "alice".to_owned(),
            reason: None,
            fresh: true,
        });
        check_record(Some(&record), &dev(), &location).unwrap();
    }

    #[test]
    fn test_a_shared_branch_cannot_be_the_directory_of_another_environments_refs() {
        let location = |desired: &str| StateLocation {
            repository: "https://git.example/p.git".to_owned(),
            desired_ref: desired.to_owned(),
            observed_ref: desired.to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        let conflicts = ref_conflicts(&[
            (EnvironmentName::new("pr-1").unwrap(), location("nyl/previews")),
            (
                EnvironmentName::new("previews").unwrap(),
                location("nyl/previews/desired"),
            ),
        ]);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].message.contains("pr-1") && conflicts[0].message.contains("previews"));
        // Environments sharing one ref through state.path do not conflict.
        assert!(ref_conflicts(&[
            (EnvironmentName::new("pr-1").unwrap(), location("nyl/previews")),
            (EnvironmentName::new("pr-2").unwrap(), location("nyl/previews")),
        ])
        .is_empty());
    }
}
