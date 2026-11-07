//! File-backed Cluster and DeploymentTarget values, resolved once during inventory discovery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::resources::ValuesFromFile;
use crate::util::project_path::ProjectPaths;
use crate::{NylError, Result};

use super::DiscoveredGitOpsResource;

/// Materialize each resource's imports before any consumer builds a target context.
pub(super) fn resolve_resource_values(
    paths: &ProjectPaths,
    visible_files: &BTreeSet<PathBuf>,
    discovered: &mut DiscoveredGitOpsResource,
) -> Result<()> {
    let (sources, values) = match discovered.resource.as_mut() {
        Some(crate::resources::GitOpsResource::Cluster(cluster)) => {
            (&cluster.spec.values_from, &mut cluster.spec.values)
        }
        Some(crate::resources::GitOpsResource::DeploymentTarget(target)) => {
            (&target.spec.values_from, &mut target.spec.values)
        }
        _ => return Ok(()),
    };
    if sources.is_empty() {
        return Ok(());
    }
    let source_paths = ProjectPaths::new(
        paths
            .project_root
            .join(discovered.source_path.parent().unwrap_or_else(|| Path::new(""))),
        paths.worktree_root.clone(),
    );
    let mut merged = serde_json::json!({});
    for (index, source) in sources.iter().enumerate() {
        let context = format!(
            "{} document {} ({}/{}), spec.valuesFrom[{index}].file {:?}",
            discovered.source_path.display(),
            discovered.document_index,
            discovered.identity.kind.as_str(),
            discovered.identity.name,
            source.file,
        );
        let (path, imported) = load(paths, &source_paths, visible_files, source)
            .map_err(|error| NylError::config(format!("{context}: {error}")))?;
        discovered
            .values_inputs
            .insert(paths.key(&path).expect("import is inside the worktree"));
        merged = crate::util::deep_merge_value(Some(merged), imported);
    }
    let inline = serde_json::to_value(&*values)?;
    *values = serde_json::from_value(crate::util::deep_merge_value(Some(merged), inline))?;
    Ok(())
}

fn load(
    paths: &ProjectPaths,
    source_paths: &ProjectPaths,
    visible_files: &BTreeSet<PathBuf>,
    source: &ValuesFromFile,
) -> Result<(PathBuf, serde_json::Value)> {
    let path = source_paths.resolve("spec.valuesFrom[].file", &source.file)?;
    let relative = path
        .strip_prefix(&paths.worktree_root)
        .expect("import is inside the worktree");
    if !visible_files.contains(relative) || !path.is_file() || path.starts_with(paths.project_root.join(".nyl/cache")) {
        return Err(NylError::config(format!(
            "Values file {} is missing or is not an eligible Git-visible regular file; add the file to Git or remove its ignore rule, and keep it outside submodules and the project's .nyl/cache directory",
            path.display()
        )));
    }
    let content = std::fs::read_to_string(&path).map_err(|error| {
        NylError::config(format!(
            "Cannot read values file {}: {error}; provide a readable YAML mapping file",
            path.display()
        ))
    })?;
    // Parse exactly one document, without resource API canonicalization or templating.
    let mapping: BTreeMap<String, serde_json::Value> =
        crate::util::SourceContext::new(path.clone()).parse_yaml(&content)?;
    let value = serde_json::to_value(mapping)?;
    let imported = match &source.key {
        Some(key) => serde_json::Value::Object(serde_json::Map::from_iter([(key.clone(), value)])),
        None => value,
    };
    Ok((path, imported))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{GitOpsResource, GitOpsResourceKind};
    use serde_json::json;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("nyl/config")).unwrap();
        std::fs::create_dir_all(temp.path().join("shared")).unwrap();
        std::fs::write(temp.path().join("nyl/nyl.toml"), "").unwrap();
        write_resource(&temp, "Cluster", json!({}));
        write_resource(&temp, "DeploymentTarget", json!({}));
        temp
    }

    fn write_resource(temp: &TempDir, kind: &str, overlay: serde_json::Value) {
        let (api, spec) = if kind == "Cluster" {
            (
                "k8s.nyl/v1",
                json!({"destination":{"name":"dev"}, "kubernetes":{"kubeVersion":"1.31.4", "apiVersions":["v1"]}}),
            )
        } else {
            (
                "k8s.gitops.nyl/v1",
                json!({"reconciler":{"argocd":{}}, "publication":{"repository":{"repoURL":"https://example.invalid/deploy.git"}, "revision":"deploy"}}),
            )
        };
        let value = json!({"apiVersion":api, "kind":kind, "metadata":{"name":"dev"},
            "spec":crate::util::deep_merge_value(Some(spec), overlay)});
        std::fs::write(
            temp.path().join(format!("nyl/config/{kind}.yaml")),
            crate::yaml::serialize_yaml_document(&value).unwrap(),
        )
        .unwrap();
    }

    fn discover(temp: &TempDir, nested: bool) -> Result<super::super::GitOpsInventory> {
        super::super::discover_gitops_inventory(
            &temp.path().join(if nested { "nyl/config" } else { "nyl" }),
            None,
            crate::config::test_catalogue(),
        )
    }

    #[test]
    fn test_file_values_merge_in_resource_and_target_order_from_any_invocation_directory() {
        let temp = fixture();
        std::fs::write(temp.path().join("shared/base.yaml"), "platform:\n  imported: true\n  order: cluster-file\n  clusterInline: file\n  targetImport: cluster\n  targetInline: cluster\narray: [base]\nscalar: base\nnullValue: base\nemptyObject: {retained: true}\n").unwrap();
        std::fs::write(temp.path().join("shared/platform.yaml"), "order: first\nteam: engineering\nbackup: {retentionDays: 30}\nliteral: '{{ env.SECRET }}'\nvaluesFrom: [{file: missing.yaml}]\n").unwrap();
        std::fs::write(temp.path().join("shared/overlay.yaml"), "platform:\n  order: later-file\n  targetImport: file\n  targetInline: file\narray: []\nscalar: ''\nnullValue: null\nemptyObject: {}\n").unwrap();
        write_resource(
            &temp,
            "Cluster",
            json!({"valuesFrom":[{"file":"../../shared/base.yaml"}],
            "values":{"platform":{"clusterInline":"inline", "targetImport":"cluster-inline", "targetInline":"cluster-inline"}}}),
        );
        write_resource(
            &temp,
            "DeploymentTarget",
            json!({"valuesFrom":[
            {"file":"../../shared/platform.yaml", "key":"platform"}, {"file":"/shared/overlay.yaml"}],
            "values":{"platform":{"targetInline":"inline", "backup":{"retentionDays":14}}}}),
        );
        let inventory = discover(&temp, false).unwrap();
        assert_eq!(discover(&temp, true).unwrap(), inventory);
        let Some(GitOpsResource::Cluster(cluster)) =
            &inventory.get(GitOpsResourceKind::Cluster, "dev").unwrap().resource
        else {
            panic!()
        };
        let target_resource = inventory.get(GitOpsResourceKind::DeploymentTarget, "dev").unwrap();
        let Some(GitOpsResource::DeploymentTarget(target)) = &target_resource.resource else {
            panic!()
        };
        let session = crate::render::RenderSession::for_target(
            &inventory.project_root,
            &inventory.project_config,
            target,
            cluster,
        )
        .unwrap();
        assert_eq!(
            session.template_context().values,
            json!({
                "platform":{"imported":true, "order":"later-file", "clusterInline":"inline", "targetImport":"file", "targetInline":"inline",
                    "team":"engineering", "backup":{"retentionDays":14}, "literal":"{{ env.SECRET }}", "valuesFrom":[{"file":"missing.yaml"}]},
                "array":[], "scalar":"", "nullValue":null, "emptyObject":{"retained":true}
            })
        );
        assert_eq!(
            target_resource.values_inputs,
            [
                PathBuf::from("/shared/platform.yaml"),
                PathBuf::from("/shared/overlay.yaml")
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn test_file_values_reject_invalid_data_with_import_and_resource_context() {
        let temp = fixture();
        for (contents, expected) in [
            (None, "missing or is not an eligible Git-visible regular file"),
            (Some("broken: ["), "YAML parse error"),
            (Some("- item"), "Type mismatch"),
            (Some("scalar"), "Type mismatch"),
            (Some("null"), "Type mismatch"),
            (Some(""), "Type mismatch"),
            (Some("a: 1\n---\nb: 2\n"), "YAML parse error"),
            (Some("a: 1\n---\nnull\n"), "YAML parse error"),
        ] {
            if let Some(contents) = contents {
                std::fs::write(temp.path().join("shared/data.yaml"), contents).unwrap();
            }
            write_resource(
                &temp,
                "DeploymentTarget",
                json!({"valuesFrom":[{"file":"../../shared/data.yaml"}]}),
            );
            let error = discover(&temp, false).unwrap_err().to_string().replace('\\', "/");
            for part in [
                "config/DeploymentTarget.yaml document 1",
                "DeploymentTarget/dev",
                "spec.valuesFrom[0].file",
                "shared/data.yaml",
                expected,
            ] {
                assert!(error.contains(part), "{contents:?}: expected {part:?} in {error}");
            }
        }
    }

    #[test]
    fn test_file_values_cannot_leave_the_worktree() {
        let temp = fixture();
        write_resource(
            &temp,
            "Cluster",
            json!({"valuesFrom":[{"file":"../../../outside.yaml"}]}),
        );
        let error = discover(&temp, false).unwrap_err().to_string();
        assert!(error.contains("outside the Git worktree"), "{error}");
    }

    #[test]
    fn test_file_values_validate_static_declarations_with_source_context() {
        let temp = fixture();
        for (source, expected) in [
            (
                json!({"file":"./data.yaml"}),
                "normalized path relative to the resource's source directory",
            ),
            (json!({"file":"../../.git/data.yaml"}), "must not access Git metadata"),
            (json!({"file":"{{ env.FILE }}"}), "must be a static value"),
            (
                json!({"file":"data.yaml", "key":""}),
                "spec.valuesFrom[].key must not be empty",
            ),
        ] {
            write_resource(&temp, "Cluster", json!({"valuesFrom":[source]}));
            let error = discover(&temp, false).unwrap_err().to_string().replace('\\', "/");
            assert!(error.contains("config/Cluster.yaml#document-1"), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn test_file_values_require_git_visibility_regardless_of_extension() {
        let temp = fixture();
        std::fs::write(temp.path().join(".gitignore"), "shared/config.data\n").unwrap();
        std::fs::write(temp.path().join("shared/config.data"), "region: west\n").unwrap();
        write_resource(
            &temp,
            "Cluster",
            json!({"valuesFrom":[{"file":"../../shared/config.data"}]}),
        );
        let error = discover(&temp, false).unwrap_err().to_string();
        assert!(error.contains("not an eligible Git-visible regular file"), "{error}");

        std::fs::create_dir_all(temp.path().join("nyl/.nyl/cache")).unwrap();
        std::fs::write(temp.path().join("nyl/.nyl/cache/config.data"), "region: cached\n").unwrap();
        write_resource(
            &temp,
            "Cluster",
            json!({"valuesFrom":[{"file":"/nyl/.nyl/cache/config.data"}]}),
        );
        let error = discover(&temp, false).unwrap_err().to_string();
        assert!(error.contains("project's .nyl/cache directory"), "{error}");

        let repository = git2::Repository::open(temp.path()).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("shared/config.data")).unwrap();
        index.write().unwrap();
        write_resource(
            &temp,
            "Cluster",
            json!({"valuesFrom":[{"file":"../../shared/config.data"}]}),
        );
        let inventory = discover(&temp, false).unwrap();
        let Some(GitOpsResource::Cluster(cluster)) =
            &inventory.get(GitOpsResourceKind::Cluster, "dev").unwrap().resource
        else {
            panic!()
        };
        assert_eq!(cluster.spec.values["region"], "west");

        std::fs::remove_file(temp.path().join("shared/config.data")).unwrap();
        let error = discover(&temp, false).unwrap_err().to_string();
        assert!(error.contains("not an eligible Git-visible regular file"), "{error}");
        std::fs::create_dir(temp.path().join("shared/config.data")).unwrap();
        let error = discover(&temp, false).unwrap_err().to_string();
        assert!(error.contains("not an eligible Git-visible regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn test_file_values_reject_symlinked_files_and_directories() {
        let temp = fixture();
        std::fs::write(temp.path().join("shared/data.yaml"), "a: 1\n").unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("shared/data.yaml"),
            temp.path().join("shared/link.yaml"),
        )
        .unwrap();
        std::os::unix::fs::symlink(temp.path().join("shared"), temp.path().join("linked")).unwrap();
        for path in ["../../shared/link.yaml", "../../linked/data.yaml"] {
            write_resource(&temp, "Cluster", json!({"valuesFrom":[{"file":path}]}));
            let error = discover(&temp, false).unwrap_err().to_string();
            assert!(error.contains("symbolic link"), "{error}");
        }
    }
}
