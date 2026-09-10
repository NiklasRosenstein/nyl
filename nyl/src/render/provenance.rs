//! Resource-level rendering provenance shared by comments and validation reports.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;

use super::render_resource_identity;

/// Ordered authoring and expansion frames for one rendered resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Provenance(pub Vec<ProvenanceFrame>);

/// One recorded step in producing a resource; this is not a field-level source map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ProvenanceFrame {
    Source { path: PathBuf, document: usize },
    Resource { identity: String },
    Generated { operation: String },
    Remote { repository: String, revision: String },
}

impl Provenance {
    pub(crate) fn source(path: PathBuf, document: usize) -> Self {
        Self(vec![ProvenanceFrame::Source { path, document }])
    }

    pub(crate) fn resource(&self, value: &serde_json::Value) -> Self {
        let mut result = self.clone();
        result.0.push(ProvenanceFrame::Resource {
            identity: render_resource_identity(value),
        });
        result
    }

    pub(crate) fn generated(mut self, operation: impl Into<String>) -> Self {
        self.0.push(ProvenanceFrame::Generated {
            operation: operation.into(),
        });
        self
    }

    pub(crate) fn remote(&mut self, repository: &str, revision: &str) {
        self.0.insert(
            0,
            ProvenanceFrame::Remote {
                repository: crate::util::sanitize_url(repository),
                revision: revision.to_owned(),
            },
        );
    }

    fn normalized(&self) -> Result<Self> {
        let mut result = self.clone();
        for frame in &mut result.0 {
            if let ProvenanceFrame::Source { path, .. } = frame {
                *path = crate::resources::relative_path_to_posix("rendered provenance path", path)?.into();
            }
        }
        Ok(result)
    }
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, frame) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            match frame {
                ProvenanceFrame::Source { path, document } => write!(
                    formatter,
                    "Source: {} (document {document})",
                    path.display().to_string().replace('\\', "/")
                )?,
                ProvenanceFrame::Resource { identity } => write!(formatter, "Resource: {identity}")?,
                ProvenanceFrame::Generated { operation } => write!(formatter, "Generated: {operation}")?,
                ProvenanceFrame::Remote { repository, revision } => {
                    write!(formatter, "Repository: {repository} @ {revision}")?;
                }
            }
        }
        Ok(())
    }
}

pub(crate) type RenderProvenance = Provenance;

#[derive(Debug, Clone)]
pub(crate) struct RenderResource {
    pub value: serde_json::Value,
    pub(crate) provenance: RenderProvenance,
}

impl std::ops::Deref for RenderResource {
    type Target = serde_json::Value;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl RenderResource {
    pub(crate) fn gitops_provenance(&self) -> Result<Provenance> {
        self.provenance.normalized()
    }
}
