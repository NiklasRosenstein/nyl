//! The API versions a lookup render declares to Helm.
//!
//! Helm in server mode reads `.Capabilities.APIVersions` from discovery and
//! ignores `--api-versions`. To keep a chart's capability checks the way an
//! offline render answers them, discovery lists exactly what an offline render
//! would: Helm's own default set for the pinned binary, the Cluster's declared
//! API versions, and the API versions of the CustomResourceDefinitions the chart
//! ships in `crds/`, which `--include-crds` renders alongside it. Objects the
//! snapshot holds add their API versions, because Helm must map them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use super::model::{split_api_version, KindInfo};
use crate::{NylError, Result};

const PROBE_CHART: &str = "apiVersion: v2\nname: nyl-probe\nversion: 0.0.0\n";
const PROBE_TEMPLATE: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: probe\ndata:\n  versions: {{ .Capabilities.APIVersions | toJson | quote }}\n";

/// The API versions this Helm binary reports without being told any: what an
/// offline `helm template` of a chart that prints `.Capabilities.APIVersions`
/// shows. Probed once per binary, which the render cache already fingerprints.
pub fn helm_default_api_versions() -> Result<Arc<BTreeSet<String>>> {
    static PROBED: OnceLock<Mutex<BTreeMap<String, Arc<BTreeSet<String>>>>> = OnceLock::new();
    let fingerprint = crate::render::cache::helm_fingerprint();
    let probed = PROBED.get_or_init(Mutex::default);
    if let Some(versions) = probed
        .lock()
        .expect("the probe cache is not poisoned")
        .get(&fingerprint)
    {
        return Ok(versions.clone());
    }
    let versions = Arc::new(probe_default_api_versions()?);
    probed
        .lock()
        .expect("the probe cache is not poisoned")
        .insert(fingerprint, versions.clone());
    Ok(versions)
}

fn probe_default_api_versions() -> Result<BTreeSet<String>> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("templates"))?;
    std::fs::write(directory.path().join("Chart.yaml"), PROBE_CHART)?;
    std::fs::write(directory.path().join("templates/versions.yaml"), PROBE_TEMPLATE)?;
    let output = Command::new("helm")
        .arg("template")
        .arg("nyl-probe")
        .arg(directory.path())
        .output()
        .map_err(|error| NylError::process(format!("Failed to execute helm: {error}")))?;
    if !output.status.success() {
        return Err(NylError::helm_chart(format!(
            "helm could not report its default API versions: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let documents = serde_saphyr::from_multiple::<Value>(&String::from_utf8_lossy(&output.stdout))?;
    let versions = documents
        .iter()
        .find_map(|document| document.pointer("/data/versions").and_then(Value::as_str))
        .and_then(|versions| serde_json::from_str::<BTreeSet<String>>(versions).ok())
        .filter(|versions| !versions.is_empty())
        .ok_or_else(|| NylError::helm_chart("helm reported no default API versions"))?;
    Ok(versions)
}

/// The `group/version` of a declared API version entry, and its kind when the
/// entry is `group/version/Kind`.
pub fn parse_declared_api_version(entry: &str) -> (String, Option<String>) {
    let parts = entry.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        // `v1/Secret`: a core version, then a kind.
        [version, kind] if is_version(version) => ((*version).to_owned(), Some((*kind).to_owned())),
        [group, version, kind] => (format!("{group}/{version}"), Some((*kind).to_owned())),
        _ => (entry.to_owned(), None),
    }
}

fn is_version(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('v') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && {
        let suffix = &rest[digits..];
        suffix.is_empty()
            || ["alpha", "beta"].iter().any(|stage| {
                suffix
                    .strip_prefix(stage)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
    }
}

/// The kinds of the CustomResourceDefinitions in the chart's `crds/` directory
/// and those of its unpacked subcharts, one per served version.
pub fn chart_crd_kinds(chart: &Path) -> Result<BTreeSet<KindInfo>> {
    let mut versions = BTreeSet::new();
    for entry in walkdir::WalkDir::new(chart).follow_links(false).sort_by_file_name() {
        let entry = entry
            .map_err(|error| NylError::helm_chart(format!("Failed to inspect chart {}: {error}", chart.display())))?;
        if !entry.file_type().is_file() || !is_crd_file(entry.path(), chart) {
            continue;
        }
        let text = std::fs::read_to_string(entry.path())?;
        collect_crd_kinds(&text, &mut versions);
    }
    Ok(versions)
}

/// A manifest in a `crds/` directory of the chart or one of its subcharts.
fn is_crd_file(path: &Path, chart: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(chart) else {
        return false;
    };
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let in_crds = components.iter().rev().skip(1).any(|component| *component == "crds");
    let extension = path.extension().and_then(|extension| extension.to_str());
    in_crds && matches!(extension, Some("yaml" | "yml" | "json"))
}

fn collect_crd_kinds(text: &str, kinds: &mut BTreeSet<KindInfo>) {
    // A CRD file that does not parse is Helm's to report; the render fails on it.
    let Ok(documents) = crate::yaml::parse_yaml_documents_k8s_compatible(text) else {
        return;
    };
    for document in documents {
        if document.get("kind").and_then(Value::as_str) != Some("CustomResourceDefinition") {
            continue;
        }
        let (Some(group), Some(kind)) = (
            document.pointer("/spec/group").and_then(Value::as_str),
            document.pointer("/spec/names/kind").and_then(Value::as_str),
        ) else {
            continue;
        };
        let namespaced = document.pointer("/spec/scope").and_then(Value::as_str) != Some("Cluster");
        let served = document
            .pointer("/spec/versions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|version| version.get("name").and_then(Value::as_str))
            .chain(document.pointer("/spec/version").and_then(Value::as_str))
            .collect::<Vec<_>>();
        for version in served {
            kinds.insert(KindInfo::new(&format!("{group}/{version}"), kind, namespaced));
        }
    }
}

/// Whether `api_version` is declared among `declared` entries.
pub fn declares_api_version(declared: &BTreeSet<String>, api_version: &str) -> bool {
    let (group, version) = split_api_version(api_version);
    declared.contains(api_version) || (group.is_empty() && declared.contains(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_declared_entries_split_into_versions_and_kinds() {
        assert_eq!(parse_declared_api_version("apps/v1"), ("apps/v1".to_owned(), None));
        assert_eq!(parse_declared_api_version("v1"), ("v1".to_owned(), None));
        assert_eq!(
            parse_declared_api_version("v1/Secret"),
            ("v1".to_owned(), Some("Secret".to_owned()))
        );
        assert_eq!(
            parse_declared_api_version("monitoring.coreos.com/v1/ServiceMonitor"),
            ("monitoring.coreos.com/v1".to_owned(), Some("ServiceMonitor".to_owned()))
        );
        assert_eq!(parse_declared_api_version("autoscaling/v2beta1").1, None);
    }

    #[test]
    fn test_chart_crds_declare_every_served_version_of_their_kind() {
        let chart = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(chart.path().join("crds")).unwrap();
        std::fs::create_dir_all(chart.path().join("charts/sub/crds")).unwrap();
        std::fs::create_dir_all(chart.path().join("templates")).unwrap();
        let crd = |group: &str, versions: &str| {
            format!("apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: widgets.{group}\nspec:\n  group: {group}\n  names: {{kind: Widget}}\n  scope: Cluster\n  versions: {versions}\n")
        };
        std::fs::write(
            chart.path().join("crds/widgets.yaml"),
            crd(
                "foo.example.com",
                "[{name: v1, served: true}, {name: v1beta1, served: true}]",
            ),
        )
        .unwrap();
        std::fs::write(
            chart.path().join("charts/sub/crds/gadgets.yaml"),
            crd("bar.example.com", "[{name: v2}]"),
        )
        .unwrap();
        // A CRD-shaped file outside `crds/` is a template, not a shipped CRD.
        std::fs::write(
            chart.path().join("templates/not-shipped.yaml"),
            crd("baz.example.com", "[{name: v1}]"),
        )
        .unwrap();
        let kinds = chart_crd_kinds(chart.path()).unwrap();
        assert_eq!(
            kinds
                .iter()
                .map(|kind| (kind.api_version(), kind.kind.as_str(), kind.namespaced))
                .collect::<Vec<_>>(),
            [
                ("bar.example.com/v2".to_owned(), "Widget", false),
                ("foo.example.com/v1".to_owned(), "Widget", false),
                ("foo.example.com/v1beta1".to_owned(), "Widget", false),
            ]
        );
    }
}
