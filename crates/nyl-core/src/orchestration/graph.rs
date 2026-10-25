//! The versioned dependency graph of one environment at one source commit.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units)
//! (`dependsOn`), [References](../../../../design/orchestration-core.md#references),
//! and [Execution](../../../../design/orchestration-core.md#execution) (waves);
//! the CI boundary of `design/implementation-architecture.md`.
//!
//! The graph is data: `nyl-orchestration`'s `rules::graph` extracts it from
//! desired documents and orders it. Edges exist before values do, so a
//! reference whose producer has no current receipt is still an edge, and a
//! later CI export can carry unresolved data edges. The format is versioned:
//! a change adds a version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ids::{CommitId, EnvironmentName, JsonPointer, Uid, UnitName};
use crate::resources::units::Evidence;

/// The graph format this build writes.
pub const GRAPH_FORMAT_VERSION: u32 = 1;

/// The units of one environment and the edges between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionGraph {
    pub format_version: u32,
    pub environment: EnvironmentName,
    pub source_commit: CommitId,
    pub nodes: BTreeMap<UnitName, GraphNode>,
    /// Sorted by consumer, then producer, then source, so equal graphs serialize equally.
    pub edges: Vec<GraphEdge>,
}

/// One unit of the ownership set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphNode {
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uid>,
}

/// The consumer needs the producer's current receipt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphEdge {
    pub producer: UnitName,
    pub consumer: UnitName,
    pub source: EdgeSource,
}

/// Why an edge exists.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EdgeSource {
    /// An explicit `dependsOn` entry.
    DependsOn {
        #[serde(default)]
        evidence: Evidence,
    },
    /// A `fromUnit` reference at `pointer` of the consumer's spec.
    #[serde(rename_all = "camelCase")]
    Reference {
        pointer: JsonPointer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<String>,
    },
}
