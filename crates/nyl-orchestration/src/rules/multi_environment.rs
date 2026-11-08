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

/// The instances of EnvironmentTemplates that a rendered unit names as the
/// producer of a reference or a `dependsOn` entry. Instances are consumers
/// only: they are too short-lived to depend on, so a producer is always a
/// declared environment.
pub fn instance_producers(spec: &Value, instances: &BTreeSet<EnvironmentName>) -> BTreeSet<EnvironmentName> {
    dependencies(spec)
        .into_iter()
        .filter(|environment| instances.contains(environment))
        .collect()
}

/// The refusal of a reference to an instance, with the fix.
pub fn instance_producer_finding(consumer: &str, producer: &EnvironmentName) -> Finding {
    Finding::new(
        Code::ReferenceInstanceProducer,
        format!(
            "{consumer} reads a unit of {producer}, which is an instance of an EnvironmentTemplate; \
             instances are consumers only, because they are too short-lived to depend on"
        ),
        "read from a declared Environment, whose units persist, and move what the instance needs there",
    )
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

/// The result of validating the environments' sources together: their order,
/// producers first, and the findings that belong to one consumer environment.
/// A consumer's bad reference stops that environment, never the others: a
/// preview that names an instance, or a unit its producer no longer selects,
/// cannot hold dev back.
#[derive(Debug)]
pub struct SourceValidation {
    pub order: Vec<EnvironmentName>,
    pub errors: BTreeMap<EnvironmentName, Finding>,
}

/// Validate the environment graph and cross-environment declarations before
/// receipts exist, failing on the first finding of any environment, as `nyl
/// validate` does.
pub fn validate_sources(
    sources: &BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
    instances: &BTreeSet<EnvironmentName>,
) -> Result<Vec<EnvironmentName>, Finding> {
    let validation = validate_available_sources(sources, &BTreeSet::new(), instances)?;
    match validation.errors.into_values().next() {
        Some(finding) => Err(finding),
        None => Ok(validation.order),
    }
}

/// Validate all available declarations, retaining failed environments as graph
/// nodes that cannot execute; one unavailable source does not stop independent
/// runs. A cycle between environments is the only failure of the whole.
pub fn validate_available_sources(
    sources: &BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
    unavailable: &BTreeSet<EnvironmentName>,
    instances: &BTreeSet<EnvironmentName>,
) -> Result<SourceValidation, Finding> {
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
                    // An instance is no producer: its reference is refused below, not ordered.
                    .filter(|other| other != environment && !instances.contains(other))
                    .collect(),
            )
        })
        .collect();
    graph.extend(unavailable.iter().map(|name| (name.clone(), BTreeSet::new())));
    let order = order(&graph)?;
    let mut errors = BTreeMap::new();
    for (consumer_environment, source) in sources {
        for (consumer, unit) in source.units.iter().filter(|(_, unit)| unit.selected) {
            let Some(spec) = &unit.spec else {
                continue;
            };
            if spec.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            let at = format!("{consumer_environment}/{consumer}");
            if let Err(finding) = check_references(sources, unavailable, instances, consumer_environment, &at, spec) {
                errors.entry(consumer_environment.clone()).or_insert(finding);
            }
        }
    }
    Ok(SourceValidation { order, errors })
}

/// The references and `dependsOn` entries of one consumer unit, `at`
/// `<environment>/<unit>`, against the producers' declarations.
fn check_references(
    sources: &BTreeMap<EnvironmentName, nyl_core::orchestration::ports::RenderedSource>,
    unavailable: &BTreeSet<EnvironmentName>,
    instances: &BTreeSet<EnvironmentName>,
    consumer_environment: &EnvironmentName,
    at: &str,
    spec: &Value,
) -> Result<(), Finding> {
    if let Some(producer) = instance_producers(spec, instances).into_iter().next() {
        return Err(instance_producer_finding(at, &producer));
    }
    let producer_environment = |environment: &str| {
        EnvironmentName::new(environment).map_err(|error| {
            Finding::new(
                Code::EnvironmentUnknown,
                error.to_string(),
                "name a declared Environment",
            )
        })
    };
    if let Ok(references) = nyl_core::bindings::unit_references("", spec) {
        for reference in references {
            let Some(environment) = reference.source.environment.as_deref() else {
                continue;
            };
            if environment == consumer_environment.as_str() {
                continue;
            }
            let name = producer_environment(environment)?;
            if unavailable.contains(&name) {
                continue;
            }
            let producer = nyl_core::orchestration::ids::UnitName::new(&reference.source.unit).ok();
            let Some(producer_spec) = producer_spec(sources, &name, &reference.source.unit) else {
                return Err(Finding::new(
                    Code::ReferenceInvalid,
                    format!(
                        "{at} references unit {environment}/{}, which is outside its ownership set",
                        reference.source.unit
                    ),
                    "reference an enabled unit the producer Environment selects",
                ));
            };
            super::references::check_spec_declaration(producer.as_ref().unwrap(), producer_spec, &reference.source)
                .map_err(|error| {
                    Finding::new(
                        Code::ReferenceInvalid,
                        format!("{at}: {error}"),
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
            let name = producer_environment(environment)?;
            if unavailable.contains(&name) {
                continue;
            }
            if producer_spec(sources, &name, entry.unit()).is_none() {
                return Err(Finding::new(
                    Code::DependencyUnknown,
                    format!(
                        "{at} depends on unit {environment}/{}, which is outside its ownership set",
                        entry.unit()
                    ),
                    "depend on an enabled unit the producer Environment selects",
                ));
            }
        }
    }
    Ok(())
}

/// Inputs wait for selected producers whose run failed or is waiting, or whose preceding
/// plan would change the referenced unit. Preview outputs never become inputs.
pub fn blocked_references(
    spec: &Value,
    planned: &BTreeMap<EnvironmentName, BTreeSet<nyl_core::orchestration::ids::UnitName>>,
    unavailable: &BTreeSet<EnvironmentName>,
) -> Vec<nyl_core::orchestration::state::BlockedReason> {
    use nyl_core::orchestration::blocked::Blocker;
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
        reasons.push(BlockedReason::new(pointer, Blocker::other(reason)));
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
    /// Instances are consumers only: a reference or a `dependsOn` entry whose
    /// producer is an instance is found, whichever form names it, and
    /// refused with its stable code and the fix. Catches an instance read as a
    /// producer, which would make a unit depend on something that expires.
    #[test]
    fn test_instance_producers_are_found_in_references_and_depends_on() {
        let instances = BTreeSet::from([name("pr-1")]);
        let by_reference =
            json!({"values": {"vpc": {"fromUnit": {"environment": "pr-1", "unit": "network", "output": "id"}}}});
        let by_depends_on = json!({"dependsOn": [{"environment": "pr-1", "unit": "seed"}]});
        let declared = json!({"dependsOn": [{"environment": "dev", "unit": "seed"}]});
        assert_eq!(
            instance_producers(&by_reference, &instances),
            BTreeSet::from([name("pr-1")])
        );
        assert_eq!(
            instance_producers(&by_depends_on, &instances),
            BTreeSet::from([name("pr-1")])
        );
        assert!(instance_producers(&declared, &instances).is_empty());
        let finding = instance_producer_finding("dev/web", &name("pr-1"));
        assert_eq!(finding.code, Code::ReferenceInstanceProducer);
        assert!(finding.message.contains("dev/web") && finding.message.contains("pr-1"));
        assert!(finding.fix.contains("declared Environment"));
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
