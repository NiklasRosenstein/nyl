//! Instances of an EnvironmentTemplate.
//!
//! Contract: [Environment templates and previews](../../../../design/orchestration-core.md#environment-templates-and-previews).
//!
//! An instance is an environment whose definition lives in state: the name of
//! its template and the parameters recorded in `state.yaml`. Every entry
//! point sees an instance as these two facts plus the template as source
//! declares it, never as anything ambient. [`InstanceDescriptor`] is that
//! pair; [`InstanceTerms`] is what the template, read at an instance's source
//! commit, says about the instance's lifecycle.

use std::collections::BTreeMap;

use serde_json::Value;

use super::time::Duration;
use crate::resources::units::DeletionPolicy;

/// A known instance: its template and its recorded parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceDescriptor {
    /// Name of the EnvironmentTemplate.
    pub template: String,
    /// The instance's parameters, bound against the template's declarations.
    pub params: BTreeMap<String, Value>,
}

/// What an EnvironmentTemplate says about an instance's lifecycle, as read at
/// the instance's source commit.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceTerms {
    pub template: String,
    pub params: BTreeMap<String, Value>,
    /// `Teardown` overrides every unit's own policy for kinds that support teardown.
    pub deletion_policy: Option<DeletionPolicy>,
    /// Whether omission teardown needs no `--allow-teardown`.
    pub allow_teardown: bool,
    /// Whether the instance's latest source commit is kept fetchable.
    pub keep_source: bool,
    /// Time to live without activity.
    pub ttl: Option<Duration>,
    /// The most instances of the template that may exist at once.
    pub max_instances: Option<u32>,
}
