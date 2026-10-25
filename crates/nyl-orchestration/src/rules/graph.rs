//! The dependency graph: extraction, cycles, and order.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units)
//! (`dependsOn`: names or `{unit, evidence?, attestations?}`),
//! [References](../../../../design/orchestration-core.md#references)
//! ("References add edges implicitly"), and the
//! [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (its dependencies are its target's `fromUnit` bindings, which the renderer
//! places under `spec.releases`).
//!
//! Edges come from rendered specs, so they exist before any value does. A
//! `dependsOn` or reference naming a unit outside the ownership set is an
//! error, and so is a cycle, which names its units in order. A reference
//! must also pass the resolver's state-free checks
//! ([`super::references::check_declaration`]): it reads an output its
//! producer declares as non-sensitive. M3 restriction: a reference or
//! `dependsOn` entry requiring `evidence: attested` or naming attestations is
//! rejected with `NYL-ATTESTATIONS-UNSUPPORTED`, since no kind records
//! attestation results yet.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::unit_references;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::graph::{EdgeSource, ExecutionGraph, GraphEdge, GraphNode, GRAPH_FORMAT_VERSION};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, JsonPointer, UnitName};
use nyl_core::orchestration::state::DesiredUnit;
use nyl_core::resources::units::{DependsOn, Evidence};
use serde_json::Value;

use super::references::{check_declaration, unsupported_depends_on, unsupported_evidence};

/// The graph of a run's desired units. Errors use
/// [`Code::DependencyUnknown`](nyl_core::orchestration::codes::Code::DependencyUnknown),
/// [`Code::DependencyCycle`](nyl_core::orchestration::codes::Code::DependencyCycle), and
/// [`Code::ReferenceInvalid`](nyl_core::orchestration::codes::Code::ReferenceInvalid)
/// for malformed reference objects.
pub fn extract(
    environment: &EnvironmentName,
    source_commit: &CommitId,
    units: &BTreeMap<UnitName, DesiredUnit>,
) -> Result<ExecutionGraph, Vec<Finding>> {
    let mut findings = Vec::new();
    let mut edges = Vec::new();
    for (consumer, unit) in units {
        depends_on_edges(environment, consumer, unit, units, &mut edges, &mut findings);
        reference_edges(consumer, unit, units, &mut edges, &mut findings);
    }
    edges.sort_by(|a, b| (&a.consumer, &a.producer, &a.source).cmp(&(&b.consumer, &b.producer, &b.source)));
    edges.dedup();
    let graph = ExecutionGraph {
        format_version: GRAPH_FORMAT_VERSION,
        environment: environment.clone(),
        source_commit: source_commit.clone(),
        nodes: units
            .iter()
            .map(|(name, unit)| {
                (
                    name.clone(),
                    GraphNode {
                        api_version: unit.unit.api_version.clone(),
                        kind: unit.unit.kind.clone(),
                        uid: unit.unit.uid.clone(),
                    },
                )
            })
            .collect(),
        edges,
    };
    if findings.is_empty() {
        if let Some(cycle) = find_cycle(&graph) {
            findings.push(Finding::new(
                Code::DependencyCycle,
                format!(
                    "units of environment {environment} depend on each other in a cycle: {}",
                    cycle.iter().map(UnitName::as_str).collect::<Vec<_>>().join(" -> ")
                ),
                "remove one dependsOn entry or fromUnit reference of the cycle",
            ));
        }
    }
    if findings.is_empty() {
        Ok(graph)
    } else {
        Err(findings)
    }
}

fn depends_on_edges(
    environment: &EnvironmentName,
    consumer: &UnitName,
    unit: &DesiredUnit,
    units: &BTreeMap<UnitName, DesiredUnit>,
    edges: &mut Vec<GraphEdge>,
    findings: &mut Vec<Finding>,
) {
    let entries: Vec<DependsOn> = match unit.spec.get("dependsOn") {
        None | Some(Value::Null) => return,
        Some(entries) => match serde_json::from_value(entries.clone()) {
            Ok(entries) => entries,
            Err(error) => {
                findings.push(Finding::new(
                    Code::UnitInvalid,
                    format!("unit {consumer}: spec.dependsOn is invalid: {error}"),
                    "write each dependsOn entry as a unit name or {unit, evidence, attestations}",
                ));
                return;
            }
        },
    };
    for (index, entry) in entries.iter().enumerate() {
        if let Some(error) = unsupported_depends_on(entry) {
            findings.push(attestations_unsupported(
                consumer,
                &format!("spec.dependsOn[{index}] {error}"),
            ));
            continue;
        }
        if let Some(other) = entry.environment() {
            findings.push(Finding::new(
                Code::DependencyUnknown,
                format!(
                    "unit {consumer}: spec.dependsOn[{index}] names unit {:?} of environment {other:?}; cross-environment dependencies are not supported yet",
                    entry.unit()
                ),
                "depend on a unit of the same environment",
            ));
            continue;
        }
        let producer = match UnitName::new(entry.unit()) {
            Ok(producer) if units.contains_key(&producer) => producer,
            _ => {
                findings.push(Finding::new(
                    Code::DependencyUnknown,
                    format!(
                        "unit {consumer}: spec.dependsOn[{index}] names unit {:?}, which is outside the ownership set of environment {environment}",
                        entry.unit()
                    ),
                    "name a unit the environment selects and enables, or remove the entry",
                ));
                continue;
            }
        };
        let evidence = match entry {
            DependsOn::Name(_) => Evidence::default(),
            DependsOn::Entry(entry) => entry.evidence.unwrap_or_default(),
        };
        edges.push(GraphEdge {
            producer,
            consumer: consumer.clone(),
            source: EdgeSource::DependsOn { evidence },
        });
    }
}

fn reference_edges(
    consumer: &UnitName,
    unit: &DesiredUnit,
    units: &BTreeMap<UnitName, DesiredUnit>,
    edges: &mut Vec<GraphEdge>,
    findings: &mut Vec<Finding>,
) {
    let references = match unit_references("", &unit.spec) {
        Ok(references) => references,
        Err(issues) => {
            for issue in issues {
                findings.push(Finding::new(
                    Code::ReferenceInvalid,
                    format!("unit {consumer}: {issue}"),
                    "write a reference as {fromUnit: {unit, output | artifact, kind?, pointer?}} alone in its object",
                ));
            }
            return;
        }
    };
    for reference in references {
        let source = reference.source;
        if let Some(error) = unsupported_evidence(&source) {
            findings.push(attestations_unsupported(
                consumer,
                &format!("the reference at {:?} {error}", reference.pointer),
            ));
            continue;
        }
        if let Some(other) = &source.environment {
            findings.push(Finding::new(
                Code::ReferenceInvalid,
                format!(
                    "unit {consumer}: the reference at {:?} reads unit {:?} of environment {other:?}; cross-environment references are not supported yet",
                    reference.pointer, source.unit
                ),
                "reference a unit of the same environment",
            ));
            continue;
        }
        let producer = match UnitName::new(source.unit.as_str()) {
            Ok(producer) if units.contains_key(&producer) => producer,
            _ => {
                findings.push(Finding::new(
                    Code::ReferenceInvalid,
                    format!(
                        "unit {consumer}: the reference at {:?} reads unit {:?}, which is outside the ownership set",
                        reference.pointer, source.unit
                    ),
                    "reference a unit the environment selects and enables",
                ));
                continue;
            }
        };
        if let Err(error) = check_declaration(&producer, &units[&producer], &source) {
            findings.push(Finding::new(
                Code::ReferenceInvalid,
                format!("unit {consumer}: the reference at {:?} {error}", reference.pointer),
                "reference an output the producer declares and does not mark sensitive",
            ));
            continue;
        }
        let pointer = match JsonPointer::new(reference.pointer.as_str()) {
            Ok(pointer) => pointer,
            Err(error) => {
                findings.push(Finding::new(
                    Code::ReferenceInvalid,
                    format!("unit {consumer}: the reference at {:?}: {error}", reference.pointer),
                    "rename the field holding the reference",
                ));
                continue;
            }
        };
        edges.push(GraphEdge {
            producer,
            consumer: consumer.clone(),
            source: EdgeSource::Reference {
                pointer,
                output: source.output,
                artifact: source.artifact,
            },
        });
    }
}

/// `NYL-ATTESTATIONS-UNSUPPORTED` for one reference or `dependsOn` entry.
fn attestations_unsupported(consumer: &UnitName, message: &str) -> Finding {
    Finding::new(
        Code::AttestationsUnsupported,
        format!("unit {consumer}: {message}"),
        "remove evidence: attested and attestations; requiring attestations arrives in a later milestone",
    )
}

/// Direct producers of every node.
fn producer_map(graph: &ExecutionGraph) -> BTreeMap<&UnitName, BTreeSet<&UnitName>> {
    let mut map: BTreeMap<&UnitName, BTreeSet<&UnitName>> =
        graph.nodes.keys().map(|name| (name, BTreeSet::new())).collect();
    for edge in &graph.edges {
        map.entry(&edge.consumer).or_default().insert(&edge.producer);
    }
    map
}

/// One cycle, in dependency order from its smallest unit back to it, or
/// `None` for an acyclic graph.
fn find_cycle(graph: &ExecutionGraph) -> Option<Vec<UnitName>> {
    let ordered = topological_order(graph).into_iter().collect::<BTreeSet<_>>();
    let producers = producer_map(graph);
    // Every unit left out of the order is on a cycle or downstream of one.
    // Walking producers from a remaining unit stays among remaining units and
    // must revisit one; the revisited stretch is a cycle.
    let start = graph.nodes.keys().find(|name| !ordered.contains(*name))?;
    let mut path: Vec<&UnitName> = vec![start];
    loop {
        let current = *path.last().expect("the path is never empty");
        let next = producers[current]
            .iter()
            .find(|producer| !ordered.contains(**producer))
            .copied()
            .expect("a unit left out of the order has a producer left out too");
        if let Some(position) = path.iter().position(|seen| *seen == next) {
            // `path` runs consumer to producer; a cycle reads producer first.
            let mut cycle = path[position..]
                .iter()
                .rev()
                .map(|name| (*name).clone())
                .collect::<Vec<_>>();
            let smallest = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, name)| *name)
                .map(|(index, _)| index)
                .expect("a cycle is never empty");
            cycle.rotate_left(smallest);
            cycle.push(cycle[0].clone());
            return Some(cycle);
        }
        path.push(next);
    }
}

/// Units in dependency order, producers first, ties by name. Units are
/// ordered in waves: a unit follows every unit of the waves before its own,
/// so `network, web-image` precede `database` even though `database` sorts
/// earlier. Units on a cycle are left out; [`extract`] rejects cycles.
pub fn topological_order(graph: &ExecutionGraph) -> Vec<UnitName> {
    let mut waiting = producer_map(graph)
        .into_iter()
        .map(|(unit, producers)| (unit, producers.len()))
        .collect::<BTreeMap<_, _>>();
    let mut consumers: BTreeMap<&UnitName, BTreeSet<&UnitName>> = BTreeMap::new();
    for edge in &graph.edges {
        consumers.entry(&edge.producer).or_default().insert(&edge.consumer);
    }
    // Kahn's algorithm, one wave at a time: every unit whose producers are
    // all ordered joins the next wave, and a wave is ordered by name.
    let mut wave = waiting
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(unit, _)| *unit)
        .collect::<BTreeSet<_>>();
    let mut order = Vec::new();
    while !wave.is_empty() {
        let mut next = BTreeSet::new();
        for unit in wave {
            order.push(unit.clone());
            for consumer in consumers.get(unit).into_iter().flatten() {
                let count = waiting.get_mut(consumer).expect("every consumer is a node");
                *count -= 1;
                if *count == 0 {
                    next.insert(*consumer);
                }
            }
        }
        wave = next;
    }
    order
}

/// The producers `unit` depends on directly.
pub fn producers<'a>(graph: &'a ExecutionGraph, unit: &UnitName) -> BTreeSet<&'a UnitName> {
    graph
        .edges
        .iter()
        .filter(|edge| &edge.consumer == unit)
        .map(|edge| &edge.producer)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::references::fixtures::*;
    use super::*;

    fn units(specs: &[(&str, &str, Value)]) -> BTreeMap<UnitName, DesiredUnit> {
        specs
            .iter()
            .map(|(unit, kind, spec)| (name(unit), desired(unit, kind, spec.clone(), unit)))
            .collect()
    }

    fn graph(specs: &[(&str, &str, Value)]) -> Result<ExecutionGraph, Vec<Finding>> {
        extract(&env(), &commit(COMMIT), &units(specs))
    }

    /// network, web-image, database (network output), and a publication
    /// whose releases read database and web-image.
    fn platform() -> Vec<(&'static str, &'static str, Value)> {
        vec![
            ("network", "Command", network_spec()),
            ("web-image", "OciImage", json!({})),
            (
                "database",
                "Command",
                json!({
                    "values": {"vpc": {"fromUnit": {"unit": "network", "output": "vpcId"}}},
                    "outputs": {"host": {"type": "string"}},
                }),
            ),
            (
                "kubernetes",
                "KubernetesPublication",
                json!({
                    "target": "dev",
                    "releases": {
                        "platform/web": {
                            "image": {"fromUnit": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}},
                            "dbHost": {"fromUnit": {"unit": "database", "output": "host"}},
                        },
                    },
                    "dependsOn": ["database"],
                }),
            ),
        ]
    }

    #[test]
    fn test_topological_order_runs_producers_in_dependency_waves() {
        let graph = graph(&platform()).unwrap();
        assert_eq!(
            topological_order(&graph),
            [name("network"), name("web-image"), name("database"), name("kubernetes")]
        );
        assert_eq!(
            producers(&graph, &name("kubernetes")),
            BTreeSet::from([&name("database"), &name("web-image")])
        );
        let publication = graph
            .edges
            .iter()
            .filter(|edge| edge.consumer == name("kubernetes"))
            .map(|edge| edge.source.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            publication,
            [
                EdgeSource::DependsOn {
                    evidence: Evidence::default()
                },
                EdgeSource::Reference {
                    pointer: JsonPointer::new("/releases/platform~1web/dbHost").unwrap(),
                    output: Some("host".to_owned()),
                    artifact: None,
                },
                EdgeSource::Reference {
                    pointer: JsonPointer::new("/releases/platform~1web/image").unwrap(),
                    output: None,
                    artifact: Some("image".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn test_extract_sorts_and_deduplicates_edges() {
        let graph = graph(&[
            ("a", "Command", json!({})),
            ("b", "Command", json!({"dependsOn": ["a", "a"]})),
        ])
        .unwrap();
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.format_version, GRAPH_FORMAT_VERSION);
    }

    #[test]
    fn test_extract_names_a_cycle_in_order() {
        let findings = graph(&[
            ("a", "Command", json!({"dependsOn": ["c"]})),
            (
                "b",
                "Command",
                json!({"dependsOn": ["a"], "outputs": {"y": {"type": "string"}}}),
            ),
            ("c", "Command", json!({"x": {"fromUnit": {"unit": "b", "output": "y"}}})),
            ("d", "Command", json!({"dependsOn": ["a"]})),
        ])
        .unwrap_err();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, Code::DependencyCycle);
        assert!(
            findings[0].message.ends_with(": a -> b -> c -> a"),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn test_extract_rejects_a_unit_depending_on_itself() {
        let findings = graph(&[("a", "Command", json!({"dependsOn": ["a"]}))]).unwrap_err();
        assert_eq!(findings[0].code, Code::DependencyCycle);
        assert!(findings[0].message.ends_with(": a -> a"), "{}", findings[0].message);
    }

    #[test]
    fn test_extract_rejects_unknown_dependencies() {
        let findings = graph(&[(
            "a",
            "Command",
            json!({
                "dependsOn": ["missing", {"environment": "infra", "unit": "network"}],
                "x": {"fromUnit": {"unit": "gone", "output": "y"}},
                "y": {"fromUnit": {"unit": "gone"}},
            }),
        )])
        .unwrap_err();
        let codes = findings.iter().map(|finding| finding.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            [Code::DependencyUnknown, Code::DependencyUnknown, Code::ReferenceInvalid],
            "{findings:?}"
        );
        assert!(findings[0].message.contains("\"missing\""));
        assert!(findings[1].message.contains("cross-environment"));
        assert!(findings[2].message.contains("\"/y\""), "{}", findings[2].message);
    }

    /// Catches a reference to an output its producer does not declare, or
    /// declares sensitive, passing the state-free checks `nyl validate`
    /// shares with runs.
    #[test]
    fn test_extract_rejects_undeclared_and_sensitive_outputs() {
        let findings = graph(&[
            (
                "db",
                "Command",
                json!({"outputs": {"host": {"type": "string"}, "password": {"type": "string", "sensitive": true}}}),
            ),
            (
                "app",
                "Command",
                json!({
                    "a": {"fromUnit": {"unit": "db", "output": "port"}},
                    "b": {"fromUnit": {"unit": "db", "output": "password"}},
                    "c": {"fromUnit": {"unit": "db", "output": "host"}},
                    "d": {"fromUnit": {"unit": "db", "artifact": "anything"}},
                }),
            ),
        ])
        .unwrap_err();
        let codes = findings.iter().map(|finding| finding.code).collect::<Vec<_>>();
        assert_eq!(codes, [Code::ReferenceInvalid, Code::ReferenceInvalid], "{findings:?}");
        assert!(
            findings[0].message.contains("does not declare"),
            "{}",
            findings[0].message
        );
        assert!(findings[1].message.contains("sensitive"), "{}", findings[1].message);
    }

    /// Catches a consumer requiring attestations running on a bare receipt:
    /// no kind records attestations yet, so the requirement is rejected.
    #[test]
    fn test_extract_rejects_evidence_requirements_until_attestations_exist() {
        let findings = graph(&[
            ("db", "Command", json!({"outputs": {"host": {"type": "string"}}})),
            (
                "app",
                "Command",
                json!({
                    "dependsOn": [{"unit": "db", "attestations": ["healthy"]}, {"unit": "db", "evidence": "published"}],
                    "x": {"fromUnit": {"unit": "db", "output": "host", "evidence": "attested"}},
                }),
            ),
        ])
        .unwrap_err();
        let codes = findings.iter().map(|finding| finding.code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            [Code::AttestationsUnsupported, Code::AttestationsUnsupported],
            "{findings:?}"
        );
        assert!(findings[0].message.contains("healthy"), "{}", findings[0].message);
        assert!(findings[1].message.contains("\"attested\""), "{}", findings[1].message);
    }
}
