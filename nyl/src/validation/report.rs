//! Stable validation results and destination-specific presentation.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::render::{Provenance, ProvenanceFrame};
use crate::util::ansi_style;
use crate::{NylError, Result};

/// A document's location within the rendered artifact, with zero-based List item indexes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLocation {
    pub path: String,
    /// One-based YAML document number.
    pub document: usize,
    pub items: Vec<usize>,
}

impl std::fmt::Display for ResourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (document {})", self.path, self.document)?;
        for item in &self.items {
            write!(f, " · items[{item}]")?;
        }
        Ok(())
    }
}

/// Kubernetes identity without conflating namespace, name, and API version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceIdentity {
    pub api_version: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

impl ResourceIdentity {
    pub(crate) fn from_manifest(value: &Value) -> Self {
        let string = |path| value.pointer(path).and_then(Value::as_str).map(str::to_owned);
        Self {
            api_version: string("/apiVersion").unwrap_or_default(),
            kind: string("/kind").unwrap_or_default(),
            namespace: string("/metadata/namespace"),
            name: string("/metadata/name"),
        }
    }
}

/// An individual constraint violation; path is the validator's instance path when available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub path: Option<String>,
    pub message: String,
}

/// The schema selected for a resource, separate from authoring provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SchemaOrigin {
    Captured { cluster: String, digest: String },
    Desired { crd: String, digest: String },
    Local { path: String, digest: String },
    Builtin { url: String, digest: Option<String> },
}

/// Outcome of checking one resource with one validator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceStatus {
    Valid,
    Invalid,
    Error,
    Skipped,
    NotChecked,
}

/// One resource result; several findings still count as one invalid resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceResult {
    pub validator: String,
    pub destination: String,
    pub resource: ResourceIdentity,
    pub rendered_location: ResourceLocation,
    pub status: ResourceStatus,
    pub findings: Vec<Finding>,
    pub provenance: Provenance,
    pub schema_origin: Option<SchemaOrigin>,
}

/// Resource counts across the report. Operation errors are recorded separately.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub valid: usize,
    pub invalid: usize,
    pub errors: usize,
    pub skipped: usize,
    pub not_checked: usize,
}

/// Destination contract used for validation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    pub name: String,
    pub kubernetes_version: String,
    pub schema_source: Option<String>,
}

/// An operation failure that prevented complete, trustworthy validation.
#[derive(Debug, Serialize, Deserialize)]
pub struct OperationError {
    pub destination: Option<String>,
    pub message: String,
}

/// Version-one validation report. Invalid resources do not make a report incomplete.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationReport {
    pub version: u32,
    pub status: String,
    pub complete: bool,
    pub summary: Summary,
    pub destinations: Vec<Destination>,
    pub resources: Vec<ResourceResult>,
    pub operation_errors: Vec<OperationError>,
}

impl Default for ValidationReport {
    fn default() -> Self {
        Self {
            version: 1,
            status: "valid".into(),
            complete: true,
            summary: Summary::default(),
            destinations: Vec::new(),
            resources: Vec::new(),
            operation_errors: Vec::new(),
        }
    }
}

impl ValidationReport {
    pub(crate) fn finish(&mut self) {
        self.resources.sort_by(|a, b| {
            (
                &a.destination,
                &a.rendered_location.path,
                a.rendered_location.document,
                &a.rendered_location.items,
                &a.resource.api_version,
                &a.resource.kind,
                &a.resource.namespace,
                &a.resource.name,
            )
                .cmp(&(
                    &b.destination,
                    &b.rendered_location.path,
                    b.rendered_location.document,
                    &b.rendered_location.items,
                    &b.resource.api_version,
                    &b.resource.kind,
                    &b.resource.namespace,
                    &b.resource.name,
                ))
        });
        self.summary = Summary::default();
        for resource in &mut self.resources {
            resource
                .findings
                .sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
            match resource.status {
                ResourceStatus::Valid => self.summary.valid += 1,
                ResourceStatus::Invalid => self.summary.invalid += 1,
                ResourceStatus::Error => self.summary.errors += 1,
                ResourceStatus::Skipped => self.summary.skipped += 1,
                ResourceStatus::NotChecked => self.summary.not_checked += 1,
            }
        }
        self.complete = self.operation_errors.is_empty() && self.summary.not_checked == 0;
        self.status = if !self.complete || self.summary.errors > 0 {
            "error"
        } else if self.summary.invalid > 0 {
            "invalid"
        } else {
            "valid"
        }
        .into();
    }

    pub(crate) fn destination_text(&self, destination: &Destination, color: bool) -> String {
        let mut output = String::new();
        let failures = self
            .resources
            .iter()
            .filter(|r| {
                r.destination == destination.name && matches!(r.status, ResourceStatus::Invalid | ResourceStatus::Error)
            })
            .collect::<Vec<_>>();
        if failures.is_empty() {
            return output;
        }
        let _ = writeln!(
            output,
            "{}\n",
            ansi_style(
                format!(
                    "kubeconform · {} · Kubernetes {}",
                    destination.name, destination.kubernetes_version
                ),
                "1;36",
                color
            )
        );
        for result in failures {
            let label = if result.status == ResourceStatus::Invalid {
                "FAIL"
            } else {
                "ERROR"
            };
            let namespace = result
                .resource
                .namespace
                .as_deref()
                .map_or(String::new(), |ns| format!("{ns}/"));
            let _ = writeln!(
                output,
                "  {}  {}  {}\n",
                ansi_style(label, "1;31", color),
                ansi_style(
                    format!(
                        "{} {namespace}{}",
                        result.resource.kind,
                        result.resource.name.as_deref().unwrap_or("<unnamed>")
                    ),
                    "1",
                    color
                ),
                ansi_style(format!("({})", result.resource.api_version), "2", color)
            );
            for finding in &result.findings {
                let path = finding
                    .path
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .map_or(String::new(), |p| format!("{}: ", ansi_style(p, "1", color)));
                let _ = writeln!(output, "    {path}{}", finding.message);
            }
            output.push('\n');
            write_details(&mut output, result, color);
            output.push('\n');
        }
        output
    }

    pub(crate) fn summary_text(&self) -> String {
        let s = &self.summary;
        let mut text = format!(
            "Validation: {} valid, {} invalid, {} errors, {} skipped",
            s.valid, s.invalid, s.errors, s.skipped
        );
        if !self.complete {
            let _ = write!(text, ", {} not checked (incomplete)", s.not_checked);
        }
        text.push('\n');
        text
    }

    pub(crate) fn text(&self) -> String {
        let mut output = self
            .destinations
            .iter()
            .map(|d| self.destination_text(d, false))
            .collect::<String>();
        for error in &self.operation_errors {
            let _ = writeln!(output, "ERROR: {}", error.message);
        }
        output.push_str(&self.summary_text());
        output
    }

    pub(crate) fn export(&self, destinations: &[ValidationOutput]) -> Result<()> {
        for output in destinations {
            let bytes = match output.format {
                ReportFormat::Json => super::store::json_bytes(self)?,
                ReportFormat::Text => self.text().into_bytes(),
            };
            if output.path == Path::new("-") {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            } else {
                super::store::atomic_write(&output.path, &bytes)?;
            }
        }
        Ok(())
    }
}

fn write_details(output: &mut String, result: &ResourceResult, color: bool) {
    let location = &result.rendered_location;
    let mut document = format!("(document {})", location.document);
    for item in &location.items {
        let _ = write!(document, " · items[{item}]");
    }
    let mut rows = vec![("Rendered:", location.path.clone(), Some(document))];
    for frame in &result.provenance.0 {
        rows.push(match frame {
            ProvenanceFrame::Source { path, document } => (
                "Source:",
                path.display().to_string().replace('\\', "/"),
                Some(format!("(document {document})")),
            ),
            ProvenanceFrame::Resource { identity } => {
                let (value, suffix) = identity.split_once(' ').map_or_else(
                    || (identity.clone(), None),
                    |(api, resource)| (resource.to_owned(), Some(format!("({api})"))),
                );
                ("Expanded from:", value, suffix)
            }
            ProvenanceFrame::Generated { operation } => ("Generated:", operation.clone(), None),
            ProvenanceFrame::Remote { repository, revision } => {
                ("Repository:", format!("{repository} @ {revision}"), None)
            }
        });
    }
    let width = rows
        .iter()
        .filter(|(_, _, suffix)| suffix.is_some())
        .map(|(_, value, _)| value.width())
        .max()
        .unwrap_or_default();
    for (label, value, suffix) in rows {
        let _ = write!(output, "    {} {value}", ansi_style(format!("{label:<14}"), "2", color));
        if let Some(suffix) = suffix {
            let padding = " ".repeat(width - value.width() + 2);
            let _ = write!(output, "{padding}{}", ansi_style(suffix, "2", color));
        }
        output.push('\n');
    }
}

/// Export format independent of the terminal presentation.
#[derive(Debug, Clone, Copy)]
pub enum ReportFormat {
    Text,
    Json,
}

/// Explicit destination for one complete validation report.
#[derive(Debug, Clone)]
pub struct ValidationOutput {
    pub format: ReportFormat,
    pub path: PathBuf,
}

impl FromStr for ValidationOutput {
    type Err = String;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (format, path) = value.split_once(':').ok_or("expected FORMAT:PATH")?;
        let format = match format {
            "json" => ReportFormat::Json,
            "text" => ReportFormat::Text,
            _ => return Err("validation report format must be text or json".into()),
        };
        if path.is_empty() {
            return Err("validation report path must not be empty".into());
        }
        Ok(Self {
            format,
            path: path.into(),
        })
    }
}

/// Resolve aliases through existing parents without requiring a destination to exist.
pub(crate) fn output_identity(path: &Path) -> Result<PathBuf> {
    let mut absolute = std::env::current_dir()?;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                absolute.pop();
            }
            _ => {
                absolute.push(component.as_os_str());
                if let Ok(resolved) = absolute.canonicalize() {
                    absolute = resolved;
                }
            }
        }
    }
    Ok(absolute)
}

pub(crate) fn validate_outputs(
    outputs: &[ValidationOutput],
    stdout_allowed: bool,
    protected: &[PathBuf],
    trees: &[PathBuf],
) -> Result<()> {
    let mut seen = BTreeSet::new();
    #[cfg(unix)]
    let mut identities = BTreeSet::new();
    for output in outputs {
        if output.path == Path::new("-") {
            if !stdout_allowed || !seen.insert(PathBuf::from("-")) {
                return Err(NylError::config(
                    "Validation report stdout is supported only by render-tree, with one stdout destination",
                ));
            }
            continue;
        }
        let identity = output_identity(&output.path)?;
        if identity
            .ancestors()
            .skip(1)
            .any(|parent| parent.join("_nyl/index.json").is_file())
        {
            return Err(NylError::config(
                "Validation reports must be outside managed rendered output trees",
            ));
        }
        if identity.is_dir() || !seen.insert(identity.clone()) {
            return Err(NylError::config(
                "Validation report destinations must be distinct files",
            ));
        }
        if protected
            .iter()
            .any(|p| output_identity(p).is_ok_and(|p| p == identity || same_file(p.as_path(), &output.path)))
            || trees
                .iter()
                .any(|p| output_identity(p).is_ok_and(|p| identity.starts_with(p)))
        {
            return Err(NylError::config(format!(
                "Validation report would overwrite source or managed output: {}",
                output.path.display()
            )));
        }
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&output.path) {
            use std::os::unix::fs::MetadataExt as _;
            if !identities.insert((meta.dev(), meta.ino())) {
                return Err(NylError::config("Validation reports refer to the same file"));
            }
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn findings_distinguish_the_validated_resource_from_its_expansion_trace() {
        let destination = Destination {
            name: "kasoku".into(),
            kubernetes_version: "1.34.5".into(),
            schema_source: None,
        };
        let mut report = ValidationReport {
            resources: vec![ResourceResult {
                validator: "kubeconform".into(),
                destination: "kasoku".into(),
                resource: ResourceIdentity {
                    api_version: "postgresql.cnpg.io/v1".into(),
                    kind: "Cluster".into(),
                    namespace: Some("rise".into()),
                    name: Some("rise-db".into()),
                },
                rendered_location: ResourceLocation {
                    path: "cloud/rise/resources.yaml".into(),
                    document: 21,
                    items: Vec::new(),
                },
                status: ResourceStatus::Invalid,
                findings: vec![Finding {
                    path: Some("/spec/affinity".into()),
                    message: "got null, want object".into(),
                }],
                provenance: Provenance(vec![
                    ProvenanceFrame::Source {
                        path: "applications/cloud/rise.yaml".into(),
                        document: 4,
                    },
                    ProvenanceFrame::Resource {
                        identity: "components.k8s.nyl/v1 PostgresDb rise/rise-db".into(),
                    },
                    ProvenanceFrame::Resource {
                        identity: "nyl.niklasrosenstein.github.com/v1 HelmChart rise/postgres".into(),
                    },
                ]),
                schema_origin: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            report.destination_text(&destination, false),
            concat!(
                "kubeconform · kasoku · Kubernetes 1.34.5\n\n",
                "  FAIL  Cluster rise/rise-db  (postgresql.cnpg.io/v1)\n\n",
                "    /spec/affinity: got null, want object\n\n",
                "    Rendered:      cloud/rise/resources.yaml     (document 21)\n",
                "    Source:        applications/cloud/rise.yaml  (document 4)\n",
                "    Expanded from: PostgresDb rise/rise-db       (components.k8s.nyl/v1)\n",
                "    Expanded from: HelmChart rise/postgres       (nyl.niklasrosenstein.github.com/v1)\n\n",
            )
        );
        let styled = report.destination_text(&destination, true);
        assert!(styled.contains("resources.yaml     \x1b[2m(document 21)\x1b[0m"));
        assert!(styled.contains("rise.yaml  \x1b[2m(document 4)\x1b[0m"));
        assert!(styled.contains("PostgresDb rise/rise-db       \x1b[2m(components.k8s.nyl/v1)\x1b[0m"));
        report.resources[0].status = ResourceStatus::Error;
        assert!(report
            .destination_text(&destination, true)
            .contains("\x1b[1;31mERROR\x1b[0m"));
    }

    #[test]
    fn report_output_rejects_aliases_and_managed_tree_paths() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source.json");
        std::fs::write(&source, "{}").unwrap();
        let alias = directory.path().join("alias.json");
        std::fs::hard_link(&source, &alias).unwrap();
        let output = ValidationOutput {
            format: ReportFormat::Json,
            path: alias,
        };
        assert!(validate_outputs(std::slice::from_ref(&output), false, &[source], &[]).is_err());
        let tree = directory.path().join("rendered");
        std::fs::create_dir_all(tree.join("_nyl")).unwrap();
        std::fs::write(tree.join("_nyl/index.json"), "{}").unwrap();
        let output = ValidationOutput {
            format: ReportFormat::Json,
            path: tree.join("report.json"),
        };
        assert!(validate_outputs(&[output], false, &[], &[]).is_err());
    }

    #[test]
    fn report_export_fails_when_destination_cannot_be_written() {
        let directory = TempDir::new().unwrap();
        let parent = directory.path().join("file");
        std::fs::write(&parent, "not a directory").unwrap();
        let output = ValidationOutput {
            format: ReportFormat::Json,
            path: parent.join("report.json"),
        };
        assert!(ValidationReport::default().export(&[output]).is_err());
    }
}
