//! Resource-level rendering provenance.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;

use super::render_resource_identity;

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
    pub(crate) fn gitops_provenance_display(&self) -> Result<String> {
        self.provenance.gitops_display()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RenderProvenance(Arc<RenderProvenanceNode>);

#[derive(Debug)]
struct RenderProvenanceNode {
    frame: RenderProvenanceFrame,
    parent: Option<RenderProvenance>,
}
#[derive(Debug)]
enum RenderProvenanceFrame {
    Source { path: PathBuf, document: usize },
    Resource(String),
}

impl RenderProvenance {
    pub(crate) fn source(path: PathBuf, document: usize) -> Self {
        Self(Arc::new(RenderProvenanceNode {
            frame: RenderProvenanceFrame::Source { path, document },
            parent: None,
        }))
    }

    pub(crate) fn resource(&self, value: &serde_json::Value) -> Self {
        Self(Arc::new(RenderProvenanceNode {
            frame: RenderProvenanceFrame::Resource(render_resource_identity(value)),
            parent: Some(self.clone()),
        }))
    }

    fn gitops_display(&self) -> Result<String> {
        let mut frames = Vec::new();
        let mut current = Some(self);
        while let Some(provenance) = current {
            frames.push(&provenance.0.frame);
            current = provenance.0.parent.as_ref();
        }

        let mut output = String::new();
        for (index, frame) in frames.into_iter().rev().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            match frame {
                RenderProvenanceFrame::Source { path, document } => {
                    let path = crate::resources::relative_path_to_posix("rendered provenance path", path)?;
                    let _ = write!(output, "Source: {path} (document {document})");
                }
                RenderProvenanceFrame::Resource(identity) => {
                    output.push_str("Resource: ");
                    output.push_str(identity);
                }
            }
        }
        Ok(output)
    }
}

impl std::fmt::Display for RenderProvenance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut frames = Vec::new();
        let mut current = Some(self);
        while let Some(provenance) = current {
            frames.push(&provenance.0.frame);
            current = provenance.0.parent.as_ref();
        }
        for (index, frame) in frames.into_iter().rev().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            match frame {
                RenderProvenanceFrame::Source { path, document } => {
                    let path = path.display().to_string().replace(std::path::MAIN_SEPARATOR, "/");
                    write!(formatter, "Source: {path} (document {document})")?;
                }
                RenderProvenanceFrame::Resource(identity) => write!(formatter, "Resource: {identity}")?,
            }
        }
        Ok(())
    }
}
