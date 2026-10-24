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
//! error, and so is a cycle, which names its units in order.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::graph::ExecutionGraph;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::state::DesiredUnit;

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
    let _ = (environment, source_commit, units);
    todo!("references-graph")
}

/// Units in dependency order, producers first, ties by name.
pub fn topological_order(graph: &ExecutionGraph) -> Vec<UnitName> {
    let _ = graph;
    todo!("references-graph")
}

/// The producers `unit` depends on directly.
pub fn producers<'a>(graph: &'a ExecutionGraph, unit: &UnitName) -> BTreeSet<&'a UnitName> {
    let _ = (graph, unit);
    todo!("references-graph")
}
