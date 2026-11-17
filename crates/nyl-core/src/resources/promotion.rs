//! The PromotionPath resource.
//!
//! Contract: [PromotionPath](../../../../design/promotion.md#promotionpath).
//! Like an Environment, a PromotionPath has a static envelope and a literal
//! spec. This module owns what can be checked from the resource alone:
//! its shape, selector forms, pointer syntax, and the forms M6 does not
//! support yet ([`PromotionPath::unsupported`]). What needs the project (the
//! environments, their units, and the bindings that read the values) is
//! checked by `nyl_orchestration::rules::promotion_paths`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::gitops::{validate_static_required, GitOpsResourceMetadata};
use crate::constants::API_VERSION_GITOPS;
use crate::orchestration::codes::{Code, Finding};
use crate::orchestration::ids::PromotionPathName;
use crate::orchestration::promotion::PromotionSelect;
use crate::orchestration::time::Duration;
use crate::{CoreError, Result};

/// Kind name of [`PromotionPath`].
pub const KIND_PROMOTION_PATH: &str = "PromotionPath";

/// The explicit link between source selectors and target bindings, along
/// which `nyl promote` moves a proven source commit and selected values from
/// one environment's recorded state into another environment's desired state.
///
/// A path says which environment is the source and which the target, how
/// strong the evidence must be, and which source fields become named values
/// that `fromPromotion` bindings read. Source and target may differ in unit
/// names, input names, and document shape; the path states the mapping.
///
/// ## When needed
///
/// To promote what one environment proved into another: a source commit, an
/// image digest, or any value a unit's input or a Release input should take
/// from another environment instead of rebuilding or retyping it.
///
/// ## If omitted
///
/// Nothing is promoted between environments, and no environment may use a
/// `fromPromotion` source or binding.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::promotion_path_example())]
pub struct PromotionPath {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata. The name is a DNS label and names the record file.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: PromotionPathSpec,
}

/// The spec of a [`PromotionPath`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromotionPathSpec {
    /// The source: a declared Environment, never a template instance.
    pub from: PromotionEndpoint,
    /// The target environment, whose desired state holds the records.
    pub to: PromotionEndpoint,
    /// The level every covered unit and the source must reach: `published` (the source produced the value, as a receipt) or `attested` (published, and every attestation the covered units and the source environment declare passes for that state). Default `published`.
    #[serde(default)]
    pub evidence: PathEvidence,
    /// Replaces the attestations required per scope, for `attested`, or adds requirements, for `published`. A scope the path does not name keeps the level's default.
    #[serde(default, skip_serializing_if = "PathAttestations::is_empty")]
    pub attestations: PathAttestations,
    /// Attestations older than this, such as `1h`, do not count. Omitted, attestations never age.
    #[serde(rename = "maxAttestationAge", default, skip_serializing_if = "Option::is_none")]
    pub max_attestation_age: Option<Duration>,
    /// `none` writes the record directly. `pullRequest` opens a reviewable change instead; it needs M11, and validation rejects it with `NYL-PROMOTE-UNSUPPORTED` until then.
    #[serde(rename = "changeGate", default)]
    pub change_gate: ChangeGate,
    /// Check that every promoted artifact still exists before recording. Default `true`.
    #[serde(rename = "verifyArtifacts", default = "default_true")]
    pub verify_artifacts: bool,
    /// Per unit that runs as several deployments, such as a KubernetesPublication, which of them count toward its running-scoped attestations for this path: keyed by the unit's name in the source environment. `nyl validate` and `nyl plan` check every unit, group, and deployment named here against what the unit's kind lists for it. Omitted, every deployment of every unit counts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub coverage: BTreeMap<String, UnitCoverage>,
    /// The promoted values by name; the names are what `fromPromotion` bindings read.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, PromotionValue>,
}

fn default_true() -> bool {
    true
}

/// One end of a path. Set exactly one of `environment` and `target`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(transform = endpoint_schema)]
pub struct PromotionEndpoint {
    /// A declared Environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// A DeploymentTarget outside every environment (M11). Validation rejects it with `NYL-PROMOTE-UNSUPPORTED` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

fn endpoint_schema(schema: &mut schemars::Schema) {
    super::schema::exclusive_fields(
        schema,
        &[
            ("environment", "A declared Environment."),
            ("target", "A DeploymentTarget (M11)."),
        ],
        None,
    );
}

/// Evidence level of a path.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PathEvidence {
    /// The source produced the value: a current receipt.
    #[default]
    Published,
    /// Published, and every required attestation passes for that state.
    Attested,
}

/// Whether a promotion is recorded directly or through a reviewable change.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ChangeGate {
    /// `nyl promote` writes the record.
    #[default]
    #[serde(rename = "none")]
    None,
    /// `nyl promote` opens a pull request instead (M11).
    #[serde(rename = "pullRequest")]
    PullRequest,
}

/// The attestations a path requires, per scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathAttestations {
    /// Per covered unit, the attestation names required of it; an empty list asks for its receipt only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub units: BTreeMap<String, Vec<String>>,
    /// The environment-wide attestations required; omitted, every one the source environment declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Vec<String>>,
}

impl PathAttestations {
    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.environment.is_none()
    }
}

/// Which deployments of one unit a path counts, by the names the unit's kind
/// lists for them (for a KubernetesPublication, a deployment is an Argo CD
/// Application, or a Release on a direct target, and a group is an
/// ApplicationGroup).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnitCoverage {
    /// Narrows the deployments that count toward a source promotion to those of these groups; it never narrows what moves, and a deployment in no group (such as a catalog) then does not count. Omitted, every group. A path that promotes only values judges each value for its own consumer, so validation refuses `groups` there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Deployments knowingly accepted as falling short: they leave the unit's aggregate for this path, and each record lists them. A value consumed by an excluded deployment must still come from a publication it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Deployments that must meet the path's required attestations at whatever they run, without contributing values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require: Vec<String>,
}

impl UnitCoverage {
    /// Whether the entry narrows which deployments count: it names groups or exclusions.
    pub fn narrows(&self) -> bool {
        !self.groups.is_empty() || !self.exclude.is_empty()
    }

    /// Whether `deployment`, in `group`, counts toward a whole-source judgement:
    /// in a named group, when the entry names groups, and not excluded.
    pub fn counts(&self, deployment: &str, group: Option<&str>) -> bool {
        (self.groups.is_empty() || group.is_some_and(|group| self.groups.iter().any(|named| named == group)))
            && !self.excludes(deployment)
    }

    /// Whether the entry excludes `deployment`.
    pub fn excludes(&self, deployment: &str) -> bool {
        self.exclude.iter().any(|name| name == deployment)
    }
}

impl PromotionPathSpec {
    /// The coverage of `unit`, when the path declares one.
    pub fn unit_coverage(&self, unit: &str) -> Option<&UnitCoverage> {
        self.coverage.get(unit)
    }

    /// The excluded deployments by unit, as a record lists them: each unit's
    /// `exclude`, sorted, each name once; a unit that excludes none is left
    /// out.
    pub fn excluded_deployments(&self) -> BTreeMap<String, Vec<String>> {
        self.coverage
            .iter()
            .filter(|(_, coverage)| !coverage.exclude.is_empty())
            .map(|(unit, coverage)| {
                let names: std::collections::BTreeSet<&String> = coverage.exclude.iter().collect();
                (unit.clone(), names.into_iter().cloned().collect())
            })
            .collect()
    }
}

/// A promoted value: which source field it reads.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromotionValue {
    /// The one source field the value reads: `{unit, input: <pointer>}`, `{unit, output, pointer?}`, or `{unit, artifact, kind?, pointer}`.
    pub select: PromotionSelect,
}

/// The shape of a selector, once its form is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectForm {
    /// The source unit's resolved spec.
    Input,
    /// A declared output of the unit's receipt.
    Output,
    /// A field of an artifact the receipt lists.
    Artifact,
    /// A Release input of a target source (M11): no unit.
    TargetInput,
}

impl PromotionSelect {
    /// Which of the four forms the selector is, or why it is none of them.
    pub fn form(&self) -> std::result::Result<SelectForm, String> {
        let kinds = [
            (self.input.is_some(), "input"),
            (self.output.is_some(), "output"),
            (self.artifact.is_some(), "artifact"),
        ];
        let set = kinds
            .iter()
            .filter(|(set, _)| *set)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>();
        let [kind] = set.as_slice() else {
            return Err(if set.is_empty() {
                "names none of input, output, and artifact".to_owned()
            } else {
                format!(
                    "sets {} together; set exactly one of input, output, and artifact",
                    set.join(" and ")
                )
            });
        };
        match (*kind, self.unit.is_some()) {
            ("input", false) => {
                if self.pointer.is_some() || self.kind.is_some() {
                    return Err("`input` without a unit takes no pointer or kind".to_owned());
                }
                Ok(SelectForm::TargetInput)
            }
            (_, false) => Err(format!("`{kind}` needs the `unit` it reads")),
            ("input", true) => {
                if self.pointer.is_some() || self.kind.is_some() {
                    Err("`input` takes no `pointer` or `kind`; its value is the pointer".to_owned())
                } else {
                    Ok(SelectForm::Input)
                }
            }
            ("output", true) => {
                if self.kind.is_some() {
                    Err("`output` takes no `kind`".to_owned())
                } else {
                    Ok(SelectForm::Output)
                }
            }
            (_, true) => {
                if self.pointer.is_none() {
                    Err("`artifact` needs a `pointer` into the artifact's spec".to_owned())
                } else {
                    Ok(SelectForm::Artifact)
                }
            }
        }
    }

    /// Why the selector is malformed: no single form, or a pointer that is
    /// not RFC 6901. Reported as `NYL-PROMOTE-SELECTOR-INVALID` by the path
    /// rules, so a malformed selector parses and `nyl get` still lists the path.
    pub fn shape_problem(&self) -> Option<String> {
        if let Err(reason) = self.form() {
            return Some(format!("the selector {reason}"));
        }
        for (name, pointer) in [("input", &self.input), ("pointer", &self.pointer)] {
            if let Some(pointer) = pointer {
                if let Err(error) = crate::json_pointer::validate(&format!("select.{name}"), pointer) {
                    return Some(error.to_string());
                }
            }
        }
        None
    }
}

impl PromotionPath {
    /// The validated name.
    pub fn name(&self) -> Result<PromotionPathName> {
        PromotionPathName::new(&self.metadata.name)
            .map_err(|error| CoreError::config(format!("PromotionPath metadata.name: {error}")))
    }

    /// What is checked from the resource alone: envelope, name, endpoints, and
    /// value names. Unsupported forms ([`Self::unsupported`]) and malformed
    /// selectors ([`PromotionSelect::shape_problem`]) are valid here and
    /// reported as findings with their codes, so `nyl get promotion-paths`
    /// lists the path.
    pub fn validate(&self) -> Result<()> {
        if self.api_version != API_VERSION_GITOPS {
            return Err(CoreError::config(format!(
                "{KIND_PROMOTION_PATH} apiVersion must be {API_VERSION_GITOPS:?}"
            )));
        }
        if self.kind != KIND_PROMOTION_PATH {
            return Err(CoreError::config(format!(
                "Expected kind {KIND_PROMOTION_PATH:?}, got {:?}",
                self.kind
            )));
        }
        validate_static_required("metadata.name", &self.metadata.name)?;
        self.name()?;
        for (field, endpoint) in [("spec.from", &self.spec.from), ("spec.to", &self.spec.to)] {
            match (&endpoint.environment, &endpoint.target) {
                (Some(name), None) | (None, Some(name)) => validate_static_required(field, name)?,
                _ => {
                    return Err(CoreError::config(format!(
                        "{field} must set exactly one of environment and target"
                    )))
                }
            }
        }
        if self.spec.max_attestation_age.is_some_and(|age| age.as_secs() == 0) {
            return Err(CoreError::config(
                "spec.maxAttestationAge must be longer than zero; omit it for no limit",
            ));
        }
        for name in self.spec.values.keys() {
            if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c == '/') {
                return Err(CoreError::config(format!(
                    "spec.values key {name:?} must be a name without whitespace or '/'"
                )));
            }
        }
        Ok(())
    }

    /// The forms M6 does not support: `changeGate: pullRequest` and a
    /// DeploymentTarget in `from` or `to`, each `NYL-PROMOTE-UNSUPPORTED`
    /// naming M11.
    ///
    /// Contract: [Exit categories](../../../../design/promotion.md#exit-categories)
    /// and the [M6 plan](../../../../design/m6-plan.md#deferred-to-m11).
    pub fn unsupported(&self) -> Vec<Finding> {
        let name = &self.metadata.name;
        let mut findings = Vec::new();
        let mut add = |what: String, fix: &str| {
            findings.push(Finding::new(
                Code::PromoteUnsupported,
                format!("PromotionPath {name} uses {what}, which M11 (promotion for non-orchestrated targets and change gates) delivers"),
                fix.to_owned(),
            ));
        };
        if self.spec.change_gate == ChangeGate::PullRequest {
            add(
                "`changeGate: pullRequest`".to_owned(),
                "remove changeGate (or set it to `none`): `nyl promote` records the promotion directly until M11",
            );
        }
        for (field, endpoint) in [("from", &self.spec.from), ("to", &self.spec.to)] {
            if let Some(target) = &endpoint.target {
                add(
                    format!("a DeploymentTarget ({target}) in `{field}`"),
                    "name a declared Environment in from and to; promotion through DeploymentTargets arrives with M11",
                );
            }
        }
        findings
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn path(spec: serde_json::Value) -> Result<PromotionPath> {
        let path: PromotionPath = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "PromotionPath",
            "metadata": {"name": "dev-to-prod"}, "spec": spec,
        }))?;
        path.validate()?;
        Ok(path)
    }

    fn base() -> serde_json::Value {
        json!({"from": {"environment": "dev"}, "to": {"environment": "prod"}})
    }

    #[test]
    fn test_minimal_path_takes_the_documented_defaults() {
        let path = path(base()).unwrap();
        assert_eq!(path.spec.evidence, PathEvidence::Published);
        assert_eq!(path.spec.change_gate, ChangeGate::None);
        assert!(path.spec.verify_artifacts);
        assert!(path.spec.values.is_empty());
        assert!(path.unsupported().is_empty());
    }

    /// Every endpoint names exactly one of environment and target.
    #[test]
    fn test_endpoint_names_exactly_one_kind() {
        for from in [json!({}), json!({"environment": "dev", "target": "t"})] {
            let mut spec = base();
            spec["from"] = from;
            let error = path(spec).unwrap_err().to_string();
            assert!(error.contains("spec.from must set exactly one"), "{error}");
        }
    }

    /// A malformed selector still parses; its shape problem names why.
    #[test]
    fn test_selector_forms_and_pointers_are_checked() {
        let value = |select: serde_json::Value| {
            let mut spec = base();
            spec["values"] = json!({"v": {"select": select}});
            path(spec).unwrap().spec.values["v"].select.shape_problem()
        };
        for good in [
            json!({"unit": "k8s", "input": "/releases/a~1b/image"}),
            json!({"unit": "db", "output": "connection", "pointer": "/host"}),
            json!({"unit": "img", "artifact": "image", "kind": "container-image", "pointer": "/reference"}),
        ] {
            assert_eq!(value(good), None);
        }
        for (bad, message) in [
            (json!({"unit": "u"}), "names none of input, output, and artifact"),
            (
                json!({"unit": "u", "input": "/a", "output": "o"}),
                "sets input and output together",
            ),
            (json!({"unit": "u", "artifact": "a"}), "needs a `pointer`"),
            (json!({"unit": "u", "input": "a"}), "select.input"),
            (json!({"output": "o"}), "needs the `unit`"),
        ] {
            let problem = value(bad).unwrap_or_default();
            assert!(problem.contains(message), "{message}: {problem}");
        }
    }

    /// Coverage is per unit, with each deployment counted by its own unit's
    /// entry, and the record lists each unit's exclusions under that unit,
    /// sorted and each name once, leaving out a unit that excludes nothing.
    /// Catches an exclusion of one unit leaking into another's aggregate.
    #[test]
    fn test_coverage_is_per_unit() {
        let mut spec = base();
        spec["coverage"] = json!({
            "kubernetes": {"groups": ["web"], "exclude": ["web-batch"], "require": ["web-gateway"]},
            "edge": {"exclude": ["web-batch", "edge-canary", "web-batch"]},
            "gateway": {"require": ["gateway"]},
        });
        let path = path(spec).unwrap();
        let kubernetes = path.spec.unit_coverage("kubernetes").unwrap();
        assert!(kubernetes.narrows());
        assert!(kubernetes.counts("web-api", Some("web")));
        assert!(!kubernetes.counts("web-batch", Some("web")));
        assert!(!kubernetes.counts("jobs-worker", Some("jobs")));
        assert!(!kubernetes.counts("catalog", None));
        let edge = path.spec.unit_coverage("edge").unwrap();
        assert!(edge.counts("catalog", None));
        assert!(!edge.counts("edge-canary", None));
        assert_eq!(path.spec.unit_coverage("other"), None);
        assert_eq!(
            path.spec.excluded_deployments(),
            BTreeMap::from([
                (
                    "edge".to_owned(),
                    vec!["edge-canary".to_owned(), "web-batch".to_owned()]
                ),
                ("kubernetes".to_owned(), vec!["web-batch".to_owned()]),
            ])
        );
    }

    /// The unreleased flat shape is refused rather than ignored, so a path
    /// written for it cannot silently count every deployment.
    #[test]
    fn test_flat_coverage_fields_are_refused() {
        let mut spec = base();
        spec["coverage"] = json!({"excludeApplications": ["web"]});
        let error = path(spec).unwrap_err().to_string();
        assert!(
            error.contains("unknown field") || error.contains("invalid type"),
            "{error}"
        );
    }

    /// `changeGate: pullRequest` and a target endpoint parse, so they can be
    /// listed, and each is reported as unsupported with M11 named.
    #[test]
    fn test_unsupported_forms_name_m11() {
        let mut spec = base();
        spec["changeGate"] = json!("pullRequest");
        spec["to"] = json!({"target": "production"});
        let findings = path(spec).unwrap().unsupported();
        assert_eq!(findings.len(), 2);
        for finding in &findings {
            assert_eq!(finding.code, Code::PromoteUnsupported);
            assert!(finding.message.contains("M11"), "{}", finding.message);
        }
        assert!(findings[0].message.contains("changeGate"));
        assert!(findings[1].message.contains("`to`"));
    }
}
