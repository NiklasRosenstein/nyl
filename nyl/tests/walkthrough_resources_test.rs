//! The contract walkthroughs under `tests/scenarios/walkthroughs` describe their
//! projects with Nyl resources. This test keeps every resource whose kind has
//! a Rust type in step with that type, so a schema change cannot silently
//! invalidate a walkthrough. Kinds that later milestones add are listed and
//! skipped.

use std::collections::BTreeSet;
use std::path::Path;

use nyl::resources::environment::Environment;
use nyl::resources::units::{CommandSpec, UnitEnvelope, KIND_COMMAND};
use nyl::resources::{parse_gitops_resource, Release};
use serde_json::Value;
use walkdir::WalkDir;

const NYL_API_VERSIONS: [&str; 3] = ["gitops.nyl/v1", "k8s.gitops.nyl/v1", "units.gitops.nyl/v1"];

/// Kinds of later milestones that have no Rust type yet. Remove a kind from
/// this list when its type lands, so its walkthrough documents are checked.
const FUTURE_KINDS: [(&str, &str); 4] = [
    ("gitops.nyl/v1", "PromotionPath"),
    ("units.gitops.nyl/v1", "KubernetesPublication"),
    ("units.gitops.nyl/v1", "OciImage"),
    ("units.gitops.nyl/v1", "OpenTofu"),
];

#[test]
fn test_walkthrough_projects_parse_as_current_resource_types() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs");
    let mut checked = 0;
    let mut skipped = BTreeSet::new();
    let mut failures = Vec::new();
    for entry in WalkDir::new(&root).sort_by_file_name() {
        let entry = entry.unwrap();
        let path = entry.path();
        // scenario.yaml files hold the scenario format, not project resources.
        if path.extension().is_none_or(|extension| extension != "yaml") || path.ends_with("scenario.yaml") {
            continue;
        }
        let relative = path.strip_prefix(&root).unwrap().display().to_string();
        let text = std::fs::read_to_string(path).unwrap();
        let documents = serde_saphyr::from_multiple::<Value>(&text)
            .unwrap_or_else(|error| panic!("{relative} is not valid YAML: {error}"));
        for document in documents {
            let field = |name: &str| {
                document
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let (api_version, kind) = (field("apiVersion"), field("kind"));
            if !NYL_API_VERSIONS.contains(&api_version.as_str()) {
                continue;
            }
            let result = if api_version == "gitops.nyl/v1" && kind == "Environment" {
                serde_json::from_value::<Environment>(document.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|environment| environment.validate().map_err(|error| error.to_string()))
            } else if api_version == "units.gitops.nyl/v1" && kind == KIND_COMMAND {
                parse_command(&document)
            } else if Release::is_release(&document) {
                Release::from_value(&document)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            } else if nyl::resources::gitops_resource_kind(&document).is_some() {
                parse_gitops_resource(&document)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            } else {
                skipped.insert((api_version, kind));
                continue;
            };
            checked += 1;
            if let Err(error) = result {
                failures.push(format!("{relative}: {kind}: {error}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "walkthrough resources do not parse:\n{}",
        failures.join("\n")
    );
    assert!(checked >= 10, "only {checked} walkthrough resources were checked");
    let future = FUTURE_KINDS
        .iter()
        .map(|(api_version, kind)| ((*api_version).to_owned(), (*kind).to_owned()))
        .collect::<BTreeSet<_>>();
    let unknown = skipped.difference(&future).collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "walkthroughs use Nyl kinds that are neither implemented nor listed as future kinds: {unknown:?}"
    );
}

/// A Command document whose spec contains no template, as in the
/// walkthroughs: its envelope and its spec as the kind's type reads them.
fn parse_command(document: &Value) -> Result<(), String> {
    let envelope: UnitEnvelope = serde_json::from_value(document.clone()).map_err(|error| error.to_string())?;
    envelope.name().map_err(|error| error.to_string())?;
    let spec: CommandSpec =
        serde_json::from_value(document.get("spec").cloned().unwrap_or_default()).map_err(|error| error.to_string())?;
    spec.validate().map_err(|error| error.to_string())
}
