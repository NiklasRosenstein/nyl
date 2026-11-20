//! The value sources an environment's values carry, and where they land.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md#contexts),
//! "Carried sources". An EnvironmentTemplate's or a declared environment's
//! `values` resolve none of `$unit`, `$promotion`, `$file`, `$git`, and
//! `$publication`: they carry them to the unit documents that read the values
//! with `$environment`, whose context checks and resolves them there, so a
//! source that lands where it is not valid fails that unit's render. A
//! carried source that no selected unit's rendered spec holds landed nowhere
//! and never resolves: [`unread_sources`] reports it, for `nyl validate`,
//! once per environment and per sample instance of each template.

use nyl_core::bindings::{find_clauses, Context, Source};
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ports::RenderedSource;
use serde_json::Value;

/// One warning for each value source in the environment's values that no
/// selected unit's rendered spec holds. `$literal` is left out: it unwraps
/// where it lands, so it cannot be traced. Malformed values are reported
/// where the values are computed, not here.
pub fn unread_sources(source: &RenderedSource) -> Vec<Finding> {
    let values = Value::Object(source.environment.spec.values.clone().into_iter().collect());
    let Ok(found) = find_clauses("spec.values", "", &values, Context::EnvironmentValues.into()) else {
        return Vec::new();
    };
    let specs = source
        .units
        .values()
        .filter(|unit| unit.selected)
        .filter_map(|unit| unit.spec.as_ref())
        .collect::<Vec<_>>();
    found
        .into_iter()
        .filter(|found| found.clause.source() != Source::Literal)
        .filter_map(|found| {
            let written = values.pointer(&found.pointer)?;
            (!specs.iter().any(|spec| holds(spec, written))).then(|| {
                Finding::new(
                    Code::ValuesSourceUnread,
                    format!(
                        "{} of {} {} holds a {} source that no unit of the environment reads: a source in values resolves only where a unit reads it with $environment",
                        found.field,
                        if source.instance.is_some() { "instance" } else { "Environment" },
                        source.environment.metadata.name,
                        found.clause.source().key(),
                    ),
                    "read the value in a unit with $environment, or remove it from the values",
                )
            })
        })
        .collect()
}

/// Whether `value` is `needle` or holds it at any depth.
fn holds(value: &Value, needle: &Value) -> bool {
    value == needle
        || match value {
            Value::Object(fields) => fields.values().any(|child| holds(child, needle)),
            Value::Array(items) => items.iter().any(|child| holds(child, needle)),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nyl_core::constants::API_VERSION_UNITS;
    use nyl_core::orchestration::ids::CommitId;
    use nyl_core::orchestration::ports::RenderedUnit;
    use nyl_core::resources::units::{UnitEnvelope, UnitMetadata};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::name;

    fn source(values: &Value, specs: Vec<(&str, bool, Value)>) -> RenderedSource {
        RenderedSource {
            repositories: BTreeMap::new(),
            source_commit: CommitId::new("a".repeat(40)).unwrap(),
            project_dir: "/nyl".to_owned(),
            environment: serde_json::from_value(json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "Environment",
                "metadata": {"name": "dev"},
                "spec": {"values": values},
            }))
            .unwrap(),
            instance: None,
            units: specs
                .into_iter()
                .map(|(unit, selected, spec)| {
                    (
                        name(unit),
                        RenderedUnit {
                            envelope: UnitEnvelope {
                                api_version: API_VERSION_UNITS.to_owned(),
                                kind: "Test".to_owned(),
                                metadata: UnitMetadata {
                                    name: unit.to_owned(),
                                    labels: BTreeMap::new(),
                                },
                            },
                            source_path: "/nyl/units.yaml".to_owned(),
                            selected,
                            spec: Some(spec),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Catches a carried source that lands nowhere passing `nyl validate`
    /// silently, and one that a unit reads (through `$environment` or Jinja,
    /// either way its rendered spec holds it) reported.
    #[test]
    fn test_a_carried_source_no_unit_reads_is_reported() {
        let image = json!({"$unit": {"unit": "build", "artifact": "image"}});
        let values = json!({
            "target": {"inline": {"releaseInputs": {"web/web": {"image": image}}}},
            "unused": {"$file": {"path": "config.yaml"}},
            "unselected": {"$promotion": {"value": "image"}},
            "escaped": {"$literal": {"a": 1}},
        });
        let source = source(
            &values,
            vec![
                (
                    "publication",
                    true,
                    json!({"target": {"inline": {"releaseInputs": {"web/web": {"image": image}}}}}),
                ),
                ("other", false, json!({"image": {"$promotion": {"value": "image"}}})),
            ],
        );
        let findings = unread_sources(&source);
        assert_eq!(
            findings.iter().map(|finding| finding.code).collect::<Vec<_>>(),
            [Code::ValuesSourceUnread, Code::ValuesSourceUnread]
        );
        assert!(
            findings[0].message.starts_with("spec.values.unselected of Environment dev holds a $promotion source that no unit of the environment reads"),
            "{}",
            findings[0].message
        );
        assert!(
            findings[1].message.starts_with("spec.values.unused"),
            "{}",
            findings[1].message
        );
    }
}
