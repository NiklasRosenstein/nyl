/// Resource definitions (HelmChart, Component, etc.)
///
/// This module will handle:
/// - HelmChart resource type
/// - Component definitions
/// - Resource dependencies
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelmChart {
    pub name: String,
    pub version: String,
    pub repository: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub name: String,
    #[serde(rename = "type")]
    pub component_type: String,
}
