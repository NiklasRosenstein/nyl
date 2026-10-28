//! Environment dependency order and qualified unit selection.
//!
//! Contract: [Multi-environment runs](../../../../design/orchestration-core.md#multi-environment-runs)
//! and [Cross-environment references](../../../../design/orchestration-core.md#cross-environment-references).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use serde_json::Value;

/// Environments explicitly referenced by a rendered unit's bindings or dependencies.
pub fn dependencies(spec: &Value) -> BTreeSet<EnvironmentName> {
    if spec.get("enabled") == Some(&Value::Bool(false)) {
        return BTreeSet::new();
    }
    let mut result = BTreeSet::new();
    if let Ok(references) = nyl_core::bindings::unit_references("", spec) {
        for reference in references {
            if let Some(environment) = reference.source.environment {
                if let Ok(name) = EnvironmentName::new(environment) {
                    result.insert(name);
                }
            }
        }
    }
    if let Ok(entries) = super::references::depends_on(spec) {
        for entry in entries {
            if let Some(environment) = entry.environment() {
                if let Ok(name) = EnvironmentName::new(environment) {
                    result.insert(name);
                }
            }
        }
    }
    result
}

/// Producers before consumers, ties by name. Validate the entire declared graph,
/// including environments outside the invocation's selection.
pub fn order(graph: &BTreeMap<EnvironmentName, BTreeSet<EnvironmentName>>) -> Result<Vec<EnvironmentName>, Finding> {
    fn visit(
        name: &EnvironmentName,
        graph: &BTreeMap<EnvironmentName, BTreeSet<EnvironmentName>>,
        path: &mut Vec<EnvironmentName>,
        done: &mut BTreeSet<EnvironmentName>,
        result: &mut Vec<EnvironmentName>,
    ) -> Result<(), Finding> {
        if done.contains(name) {
            return Ok(());
        }
        if let Some(index) = path.iter().position(|item| item == name) {
            let mut cycle = path[index..].iter().map(ToString::to_string).collect::<Vec<_>>();
            cycle.push(name.to_string());
            return Err(Finding::new(
                Code::DependencyCycle,
                format!("environments depend on each other in a cycle: {}", cycle.join(" -> ")),
                "remove a cross-environment reference in the cycle",
            ));
        }
        let Some(dependencies) = graph.get(name) else {
            return Err(Finding::new(
                Code::EnvironmentUnknown,
                format!("references undeclared environment {name}"),
                "declare the producer Environment or correct the reference",
            ));
        };
        path.push(name.clone());
        for dependency in dependencies {
            visit(dependency, graph, path, done, result)?;
        }
        path.pop();
        done.insert(name.clone());
        result.push(name.clone());
        Ok(())
    }
    let mut result = Vec::new();
    let mut remaining = graph.keys().cloned().collect::<BTreeSet<_>>();
    // Kahn's order uses name order among all currently ready environments.
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .find(|name| graph[*name].iter().all(|dependency| !remaining.contains(dependency)))
            .cloned();
        let Some(name) = ready else {
            visit(
                remaining.first().unwrap(),
                graph,
                &mut Vec::new(),
                &mut BTreeSet::new(),
                &mut Vec::new(),
            )?;
            unreachable!("a graph with no ready node has a cycle");
        };
        for dependency in &graph[&name] {
            if !graph.contains_key(dependency) {
                return Err(Finding::new(
                    Code::EnvironmentUnknown,
                    format!("references undeclared environment {dependency}"),
                    "declare the producer Environment or correct the reference",
                ));
            }
        }
        remaining.remove(&name);
        result.push(name);
    }
    Ok(result)
}

/// Select qualified unit options for one environment; never silently discard a typo.
pub fn units(
    flag: &str,
    values: &[String],
    environment: &str,
    selected: &[String],
    multi: bool,
) -> Result<Vec<String>, Finding> {
    let mut result = Vec::new();
    for value in values {
        if let Some((owner, unit)) = value.split_once('/') {
            if !selected.iter().any(|name| name == owner) {
                return Err(Finding::new(
                    Code::EnvironmentUnknown,
                    format!("{flag} {value:?} names an environment outside the selection"),
                    "qualify units with a selected environment",
                ));
            }
            if owner == environment {
                result.push(unit.to_owned());
            }
        } else if multi {
            return Err(Finding::new(
                Code::UnitUnknown,
                format!("{flag} {value:?} is unqualified in a multi-environment run"),
                "name the unit as <environment>/<unit>",
            ));
        } else {
            result.push(value.clone());
        }
    }
    Ok(result)
}

/// Validate the environment graph and cross-environment declarations before
/// receipts exist. Shared by source validation and command selection.
pub fn validate_sources(
    sources: &BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
) -> Result<Vec<EnvironmentName>, Finding> {
    validate_available_sources(sources, &BTreeSet::new())
}

/// Validate all available declarations, retaining failed environments as graph
/// nodes that cannot execute; one unavailable source does not stop independent runs.
pub fn validate_available_sources(
    sources: &BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
    unavailable: &BTreeSet<EnvironmentName>,
) -> Result<Vec<EnvironmentName>, Finding> {
    let mut graph: BTreeMap<EnvironmentName, BTreeSet<EnvironmentName>> = sources
        .iter()
        .map(|(environment, source)| {
            (
                environment.clone(),
                source
                    .units
                    .values()
                    .filter(|unit| unit.selected)
                    .filter_map(|unit| unit.spec.as_ref())
                    .filter(|spec| spec.get("enabled") != Some(&Value::Bool(false)))
                    .flat_map(dependencies)
                    .filter(|other| other != environment)
                    .collect(),
            )
        })
        .collect();
    graph.extend(unavailable.iter().map(|name| (name.clone(), BTreeSet::new())));
    let ordered = order(&graph)?;
    for (consumer_environment, source) in sources {
        for (consumer, unit) in source.units.iter().filter(|(_, unit)| unit.selected) {
            let Some(spec) = &unit.spec else {
                continue;
            };
            if spec.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            if let Ok(references) = nyl_core::bindings::unit_references("", spec) {
                for reference in references {
                    let Some(environment) = reference.source.environment.as_deref() else {
                        continue;
                    };
                    if environment == consumer_environment.as_str() {
                        continue;
                    }
                    let name = EnvironmentName::new(environment).map_err(|error| {
                        Finding::new(
                            Code::EnvironmentUnknown,
                            error.to_string(),
                            "name a declared Environment",
                        )
                    })?;
                    if unavailable.contains(&name) {
                        continue;
                    }
                    let producer = nyl_core::orchestration::ids::UnitName::new(&reference.source.unit).ok();
                    let producer_spec = producer_spec(sources, &name, &reference.source.unit);
                    let Some(producer_spec) = producer_spec else {
                        return Err(Finding::new(Code::ReferenceInvalid, format!("{consumer_environment}/{consumer} references unit {environment}/{}, which is outside its ownership set", reference.source.unit), "reference an enabled unit the producer Environment selects"));
                    };
                    super::references::check_spec_declaration(
                        producer.as_ref().unwrap(),
                        producer_spec,
                        &reference.source,
                    )
                    .map_err(|error| {
                        Finding::new(
                            Code::ReferenceInvalid,
                            format!("{consumer_environment}/{consumer}: {error}"),
                            "reference a declared, non-sensitive producer output",
                        )
                    })?;
                }
            }
            if let Ok(entries) = super::references::depends_on(spec) {
                for entry in entries {
                    let Some(environment) = entry.environment() else {
                        continue;
                    };
                    if environment == consumer_environment.as_str() {
                        continue;
                    }
                    let name = EnvironmentName::new(environment).map_err(|error| {
                        Finding::new(
                            Code::EnvironmentUnknown,
                            error.to_string(),
                            "name a declared Environment",
                        )
                    })?;
                    if unavailable.contains(&name) {
                        continue;
                    }
                    if producer_spec(sources, &name, entry.unit()).is_none() {
                        return Err(Finding::new(Code::DependencyUnknown, format!("{consumer_environment}/{consumer} depends on unit {environment}/{}, which is outside its ownership set", entry.unit()), "depend on an enabled unit the producer Environment selects"));
                    }
                }
            }
        }
    }
    Ok(ordered)
}

/// Inputs wait for selected producers whose run failed or is waiting, or whose preceding
/// plan would change the referenced unit. Preview outputs never become inputs.
pub fn blocked_references(
    spec: &Value,
    planned: &BTreeMap<EnvironmentName, BTreeSet<nyl_core::orchestration::ids::UnitName>>,
    unavailable: &BTreeSet<EnvironmentName>,
) -> Vec<nyl_core::orchestration::state::BlockedReason> {
    use nyl_core::orchestration::ids::UnitName;
    use nyl_core::orchestration::state::BlockedReason;
    let mut reasons = Vec::new();
    let mut reference = |environment: Option<&str>, unit: &str, pointer: String| {
        let (Some(environment), Ok(unit)) = (environment, UnitName::new(unit)) else {
            return;
        };
        let Ok(environment) = EnvironmentName::new(environment) else {
            return;
        };
        let reason = if unavailable.contains(&environment) {
            format!("{environment}/{unit} waits because its producer environment failed or is waiting")
        } else if planned.get(&environment).is_some_and(|units| units.contains(&unit)) {
            format!("{environment}/{unit} has a planned change; waiting for its new receipt")
        } else {
            return;
        };
        reasons.push(BlockedReason { pointer, reason });
    };
    if let Ok(references) = nyl_core::bindings::unit_references("", spec) {
        for binding in references {
            reference(
                binding.source.environment.as_deref(),
                &binding.source.unit,
                binding.pointer,
            );
        }
    }
    if let Ok(entries) = super::references::depends_on(spec) {
        for (index, entry) in entries.iter().enumerate() {
            reference(entry.environment(), entry.unit(), format!("/dependsOn/{index}"));
        }
    }
    reasons
}

/// The enabled producer declaration selected by its own environment.
fn producer_spec<'a>(
    sources: &'a BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
    environment: &EnvironmentName,
    unit: &str,
) -> Option<&'a Value> {
    let unit = nyl_core::orchestration::ids::UnitName::new(unit).ok()?;
    sources
        .get(environment)?
        .units
        .get(&unit)
        .filter(|unit| unit.selected)?
        .spec
        .as_ref()
        .filter(|spec| spec.get("enabled") != Some(&Value::Bool(false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn name(value: &str) -> EnvironmentName {
        EnvironmentName::new(value).unwrap()
    }
    #[test]
    fn test_order_producers_first_ties_by_name_and_rejects_cycles() {
        let mut graph = BTreeMap::from([
            (name("a"), BTreeSet::from([name("z")])),
            (name("b"), BTreeSet::new()),
            (name("z"), BTreeSet::new()),
        ]);
        assert_eq!(order(&graph).unwrap(), [name("b"), name("z"), name("a")]);
        graph.get_mut(&name("z")).unwrap().insert(name("a"));
        assert!(order(&graph).unwrap_err().message.contains("a -> z -> a"));
    }
    #[test]
    fn test_dependencies_and_qualified_units() {
        assert_eq!(
            dependencies(
                &json!({"values": {"vpc": {"fromUnit": {"environment": "infra", "unit": "network", "output": "id"}}}, "dependsOn": [{"environment": "dev", "unit": "seed"}]})
            ),
            BTreeSet::from([name("dev"), name("infra")])
        );
        assert!(dependencies(&json!({"enabled": false, "dependsOn": [{"environment": "infra", "unit": "network"}], "values": {"id": {"fromUnit": {"environment": "infra", "unit": "network", "output": "id"}}}})).is_empty());
        let selected = vec!["dev".to_owned(), "prod".to_owned()];
        assert_eq!(
            units(
                "--unit",
                &["dev/api".to_owned(), "prod/db".to_owned()],
                "dev",
                &selected,
                true
            )
            .unwrap(),
            ["api"]
        );
        assert!(units("--unit", &["api".to_owned()], "dev", &selected, true).is_err());
        assert!(units("--unit", &["typo/api".to_owned()], "dev", &selected, true).is_err());
    }
}
