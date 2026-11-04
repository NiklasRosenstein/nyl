//! The generated Argo CD Applications of a rendered target tree and the
//! files each one deploys.
//!
//! `diff-tree --application` selects Applications by these views, and a
//! publication commit lists the Applications whose files it changes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{resource_path, DesiredFiles, CATALOG_DIRECTORY};
use crate::{NylError, Result};

/// One generated Argo CD Application in a rendered tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationView {
    /// The catalog file holding the Application manifest, relative to the prefix.
    pub catalog_file: PathBuf,
    /// The directory the Application deploys, relative to the prefix.
    pub payload_path: PathBuf,
    /// Whether this is the catalog Application, which deploys `_nyl/catalog`.
    pub catalog_application: bool,
}

/// The generated Applications in `files`, keyed by `NAMESPACE/NAME`.
///
/// Only Applications in [`CATALOG_DIRECTORY`] are generated ones. Their
/// source paths must lie beneath `publication_path_prefix`, and workload
/// payload paths must not overlap.
pub fn derive_application_views(
    files: &DesiredFiles<'_>,
    publication_path_prefix: &str,
) -> Result<BTreeMap<String, ApplicationView>> {
    let mut views = BTreeMap::new();
    for (path, bytes) in files.iter().filter(|(path, _)| path.starts_with(CATALOG_DIRECTORY)) {
        let text = std::str::from_utf8(bytes).map_err(|error| {
            NylError::config(format!(
                "Generated catalog file {} is not UTF-8: {error}",
                path.display()
            ))
        })?;
        let manifest = crate::yaml::parse_yaml_value_k8s_compatible(text).map_err(|error| {
            NylError::config(format!(
                "Failed to parse generated catalog file {}: {error}",
                path.display()
            ))
        })?;
        if manifest.get("apiVersion").and_then(serde_json::Value::as_str) != Some("argoproj.io/v1alpha1")
            || manifest.get("kind").and_then(serde_json::Value::as_str) != Some("Application")
        {
            continue;
        }
        let namespace = required_application_string(&manifest, "/metadata/namespace", path)?;
        let name = required_application_string(&manifest, "/metadata/name", path)?;
        let identity = format!("{namespace}/{name}");
        let key = crate::kubernetes::ResourceKey::from_json_value(&manifest)?;
        let expected = Path::new(CATALOG_DIRECTORY).join(resource_path(&key)?);
        // Publications rendered before the per-resource catalog layout stored
        // Applications at applications/<namespace>/<name>.yaml; they remain
        // readable as a comparison baseline.
        let legacy = Path::new(CATALOG_DIRECTORY)
            .join("applications")
            .join(namespace)
            .join(format!("{name}.yaml"));
        if *path != expected && *path != legacy {
            return Err(NylError::config(format!(
                "Generated Argo CD Application {identity:?} is at {}, expected {}",
                nyl_core::resources::relative_path_to_posix("generated catalog path", path)?,
                nyl_core::resources::relative_path_to_posix("generated catalog path", &expected)?
            )));
        }
        let rendered_path = required_application_string(&manifest, "/spec/source/path", path)?;
        nyl_core::resources::validate_relative_path(
            "generated Application spec.source.path",
            rendered_path,
            false,
            false,
        )?;
        let payload_path = strip_publication_prefix(rendered_path, publication_path_prefix, path)?;
        let view = ApplicationView {
            catalog_file: path.to_path_buf(),
            catalog_application: payload_path == Path::new(CATALOG_DIRECTORY),
            payload_path,
        };
        if views.insert(identity.clone(), view).is_some() {
            return Err(NylError::config(format!(
                "Generated Argo CD Application identity {identity:?} occurs more than once"
            )));
        }
    }
    validate_application_payloads(&views)?;
    Ok(views)
}

fn required_application_string<'a>(application: &'a serde_json::Value, pointer: &str, path: &Path) -> Result<&'a str> {
    application
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            NylError::config(format!(
                "Generated Argo CD Application {} field {} must be a string",
                path.display(),
                pointer.trim_start_matches('/')
            ))
        })
}

fn strip_publication_prefix(rendered_path: &str, path_prefix: &str, catalog_file: &Path) -> Result<PathBuf> {
    let rendered_path = Path::new(rendered_path);
    let payload = if path_prefix.is_empty() {
        rendered_path
    } else {
        rendered_path.strip_prefix(path_prefix).map_err(|_| {
            NylError::config(format!(
                "Generated Argo CD Application {} source path {} is outside publication path prefix {:?}",
                catalog_file.display(),
                rendered_path.display(),
                path_prefix
            ))
        })?
    };
    if payload.as_os_str().is_empty() {
        return Err(NylError::config(format!(
            "Generated Argo CD Application {} source path resolves to the publication root",
            catalog_file.display()
        )));
    }
    nyl_core::resources::relative_path_to_posix("generated Application payload path", payload)?;
    Ok(payload.to_path_buf())
}

fn validate_application_payloads(views: &BTreeMap<String, ApplicationView>) -> Result<()> {
    let workloads = views
        .iter()
        .filter(|(_, view)| !view.catalog_application)
        .collect::<Vec<_>>();
    for (index, (left_identity, left)) in workloads.iter().enumerate() {
        for (right_identity, right) in workloads.iter().skip(index + 1) {
            if left.payload_path.starts_with(&right.payload_path) || right.payload_path.starts_with(&left.payload_path)
            {
                return Err(NylError::config(format!(
                    "Generated Argo CD Applications {left_identity:?} and {right_identity:?} have overlapping payload paths {} and {}",
                    left.payload_path.display(),
                    right.payload_path.display()
                )));
            }
        }
    }
    Ok(())
}

/// The generated Applications whose files differ between `base` and
/// `desired`, two trees beneath the same publication prefix, as sorted
/// `NAMESPACE/NAME` identities.
///
/// A workload Application changes when its catalog manifest or a file beneath
/// its payload path is added, removed, or modified. The catalog Application
/// changes when another catalog file does, such as an AppProject; workload
/// Application manifests count only for their own Application. Files outside
/// every Application, such as state files, change none.
///
/// The list describes a change, so a tree whose Applications cannot be
/// derived, such as a publication written by another tool, contributes no
/// Applications instead of failing.
pub fn changed_applications(base: &DesiredFiles<'_>, desired: &DesiredFiles<'_>, prefix: &str) -> Vec<String> {
    let views = |files: &DesiredFiles<'_>| {
        derive_application_views(files, prefix).unwrap_or_else(|error| {
            tracing::debug!(%error, "Rendered tree has no derivable Applications");
            BTreeMap::new()
        })
    };
    let (base_views, desired_views) = (views(base), views(desired));
    let changed_paths = base
        .keys()
        .chain(desired.keys())
        .copied()
        .filter(|path| base.get(path) != desired.get(path))
        .collect::<BTreeSet<_>>();
    let workload_manifests = base_views
        .values()
        .chain(desired_views.values())
        .filter(|view| !view.catalog_application)
        .map(|view| view.catalog_file.as_path())
        .collect::<BTreeSet<_>>();
    base_views
        .iter()
        .chain(&desired_views)
        .filter(|(_, view)| {
            changed_paths.iter().any(|path| {
                if view.catalog_application {
                    path.starts_with(CATALOG_DIRECTORY) && !workload_manifests.contains(path)
                } else {
                    *path == view.catalog_file || path.starts_with(&view.payload_path)
                }
            })
        })
        .map(|(identity, _)| identity.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn application_yaml(namespace: &str, name: &str, path: &str) -> Vec<u8> {
        crate::yaml::serialize_yaml_document(&serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Application",
            "metadata": {"namespace": namespace, "name": name},
            "spec": {"source": {"path": path}}
        }))
        .unwrap()
        .into_bytes()
    }

    fn borrowed(files: &BTreeMap<PathBuf, Vec<u8>>) -> DesiredFiles<'_> {
        files
            .iter()
            .map(|(path, bytes)| (path.as_path(), bytes.as_slice()))
            .collect()
    }

    fn catalog_tree() -> BTreeMap<PathBuf, Vec<u8>> {
        BTreeMap::from([
            (
                PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/api.yaml"),
                application_yaml("argocd", "api", "production/workloads/api"),
            ),
            (
                PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/web.yaml"),
                application_yaml("argocd", "web", "production/workloads/web"),
            ),
            (
                PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/production-catalog.yaml"),
                application_yaml("argocd", "production-catalog", "production/_nyl/catalog"),
            ),
            (
                PathBuf::from("_nyl/catalog/appproject.argoproj.io/argocd/workloads.yaml"),
                b"kind: AppProject\n".to_vec(),
            ),
            (
                PathBuf::from("workloads/api/configmap/api/api.yaml"),
                b"kind: ConfigMap\n".to_vec(),
            ),
            (
                PathBuf::from("workloads/web/configmap/web/web.yaml"),
                b"kind: ConfigMap\n".to_vec(),
            ),
        ])
    }

    #[test]
    fn test_derive_application_views_from_generated_catalog() {
        let files = catalog_tree();
        let views = derive_application_views(&borrowed(&files), "production").unwrap();
        assert_eq!(views["argocd/api"].payload_path, Path::new("workloads/api"));
        assert!(!views["argocd/api"].catalog_application);
        assert!(views["argocd/production-catalog"].catalog_application);
    }

    #[test]
    fn test_derive_application_views_accepts_legacy_baselines_and_rejects_misplaced_applications() {
        let legacy = BTreeMap::from([(
            PathBuf::from("_nyl/catalog/applications/argocd/api.yaml"),
            application_yaml("argocd", "api", "production/workloads/api"),
        )]);
        let views = derive_application_views(&borrowed(&legacy), "production").unwrap();
        assert_eq!(views["argocd/api"].payload_path, Path::new("workloads/api"));

        let misplaced = BTreeMap::from([(
            PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/web.yaml"),
            application_yaml("argocd", "api", "production/workloads/api"),
        )]);
        let error = derive_application_views(&borrowed(&misplaced), "production").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected _nyl/catalog/application.argoproj.io/argocd/api.yaml"),
            "{error}"
        );
    }

    #[test]
    fn test_derive_application_views_rejects_ambiguous_or_escaping_payloads() {
        let overlapping = BTreeMap::from([
            (
                PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/parent.yaml"),
                application_yaml("argocd", "parent", "production/workloads"),
            ),
            (
                PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/child.yaml"),
                application_yaml("argocd", "child", "production/workloads/child"),
            ),
        ]);
        let error = derive_application_views(&borrowed(&overlapping), "production").unwrap_err();
        assert!(error.to_string().contains("overlapping payload paths"));

        let escaping = BTreeMap::from([(
            PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/api.yaml"),
            application_yaml("argocd", "api", "another-target/workloads/api"),
        )]);
        let error = derive_application_views(&borrowed(&escaping), "production").unwrap_err();
        assert!(error.to_string().contains("outside publication path prefix"));
    }

    #[test]
    fn test_changed_applications_lists_applications_whose_payload_changed() {
        let base = catalog_tree();
        let mut desired = base.clone();
        desired.insert(
            PathBuf::from("workloads/api/configmap/api/api.yaml"),
            b"kind: ConfigMap\ndata: {}\n".to_vec(),
        );
        desired.insert(PathBuf::from("_nyl/state/values.json"), b"{}\n".to_vec());

        assert_eq!(
            changed_applications(&borrowed(&base), &borrowed(&desired), "production"),
            ["argocd/api"]
        );
        assert!(changed_applications(&borrowed(&base), &borrowed(&base), "production").is_empty());
    }

    #[test]
    fn test_changed_applications_lists_added_and_removed_applications() {
        let base = catalog_tree();
        let mut desired = base.clone();
        desired.remove(Path::new("_nyl/catalog/application.argoproj.io/argocd/web.yaml"));
        desired.remove(Path::new("workloads/web/configmap/web/web.yaml"));
        desired.insert(
            PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/jobs.yaml"),
            application_yaml("argocd", "jobs", "production/workloads/jobs"),
        );
        desired.insert(
            PathBuf::from("workloads/jobs/configmap/jobs/jobs.yaml"),
            b"kind: ConfigMap\n".to_vec(),
        );

        assert_eq!(
            changed_applications(&borrowed(&base), &borrowed(&desired), "production"),
            ["argocd/jobs", "argocd/web"]
        );
    }

    #[test]
    fn test_changed_applications_attributes_other_catalog_files_to_the_catalog_application() {
        let base = catalog_tree();
        let mut desired = base.clone();
        desired.insert(
            PathBuf::from("_nyl/catalog/appproject.argoproj.io/argocd/workloads.yaml"),
            b"kind: AppProject\nspec: {}\n".to_vec(),
        );

        assert_eq!(
            changed_applications(&borrowed(&base), &borrowed(&desired), "production"),
            ["argocd/production-catalog"]
        );
    }

    #[test]
    fn test_changed_applications_tolerates_an_underivable_base() {
        let base = BTreeMap::from([(
            PathBuf::from("_nyl/catalog/application.argoproj.io/argocd/api.yaml"),
            b"not: [yaml".to_vec(),
        )]);
        let desired = catalog_tree();

        assert_eq!(
            changed_applications(&borrowed(&base), &borrowed(&desired), "production"),
            ["argocd/api", "argocd/production-catalog", "argocd/web"]
        );
    }
}
