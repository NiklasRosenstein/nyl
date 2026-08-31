//! Target-aware, offline rendering for rendered GitOps trees.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::commands::render::{
    deduplicate_manifests, generate_resource, is_renderable_resource, load_resources, needs_helm_rendering,
    prepare_manifests_for_output, resolve_strip_empty_metadata_labels_mode,
};
use crate::config::ProjectConfig;
use crate::postprocess::apply_kyverno_policies;
use crate::resources::{
    extract_all_kyverno_policies, extract_application_generators, extract_nyl_release, Cluster, GitOpsTarget,
    KyvernoScope, NylRelease,
};
use crate::secrets::SecretsConfig;
use crate::template::TemplateContext;
use crate::{NylError, Result};

/// Immutable rendering state shared by every release in one GitOps target.
pub struct RenderSession {
    project_root: PathBuf,
    project_config: ProjectConfig,
    target_name: String,
    kube_version: String,
    api_versions: Vec<String>,
    template_context: TemplateContext,
}

/// The result of rendering one possible Nyl release.
#[derive(Debug)]
pub struct RenderedRelease {
    /// The release declaration. `None` means target templating omitted it.
    pub release: Option<NylRelease>,
    /// Fully expanded, policy-processed and deduplicated Kubernetes manifests.
    pub manifests: Vec<Value>,
}

impl RenderSession {
    /// Build an offline rendering session from a project root and effective target.
    pub fn for_target(project_root: &Path, target: &GitOpsTarget, cluster: &Cluster) -> Result<Self> {
        Self::build(project_root, target, cluster, true, false)
    }

    /// Build with the central project configuration but without secrets or
    /// process environment for independently controlled source manifests.
    pub fn for_untrusted_source(project_root: &Path, target: &GitOpsTarget, cluster: &Cluster) -> Result<Self> {
        Self::build(project_root, target, cluster, false, false)
    }

    /// Build a restricted remote-source session. Remote projects cannot load a
    /// secrets provider from their checkout.
    pub fn for_remote_target(project_root: &Path, target: &GitOpsTarget, cluster: &Cluster) -> Result<Self> {
        Self::build(project_root, target, cluster, false, true)
    }

    fn build(
        project_root: &Path,
        target: &GitOpsTarget,
        cluster: &Cluster,
        load_secrets: bool,
        restrict_checkout: bool,
    ) -> Result<Self> {
        target.validate()?;
        cluster.validate()?;
        let (kube_version, api_versions) = required_cluster_capabilities(cluster)?;
        let project_root = project_root
            .canonicalize()
            .map_err(|error| NylError::config(format!("Failed to resolve project root: {error}")))?;
        let project_config = ProjectConfig::load_from_dir(None, Some(&project_root))?;
        if restrict_checkout {
            let config_file = project_config.file.as_ref().ok_or_else(|| {
                NylError::config(format!(
                    "Remote renderer mode requires {}/nyl.toml",
                    project_root.display()
                ))
            })?;
            if config_file.parent().map(Path::to_path_buf) != Some(project_root.clone()) {
                return Err(NylError::config(format!(
                    "Remote renderer configuration must be rooted at {}",
                    project_root.display()
                )));
            }
            for entry in walkdir::WalkDir::new(&project_root).follow_links(false) {
                let entry = entry.map_err(|error| {
                    NylError::config(format!("Failed to inspect remote renderer checkout: {error}"))
                })?;
                if entry.file_type().is_symlink() {
                    return Err(NylError::config(format!(
                        "Remote renderer checkout must not contain symbolic links: {}",
                        entry.path().display()
                    )));
                }
            }
            for path in project_config
                .get_components_search_paths()
                .iter()
                .chain(project_config.get_helm_chart_search_paths())
            {
                let relative = path.strip_prefix(&project_root).map_err(|_| {
                    NylError::config(format!(
                        "Remote renderer path {} escapes {}",
                        path.display(),
                        project_root.display()
                    ))
                })?;
                if relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(NylError::config(format!(
                        "Remote renderer path {} escapes {}",
                        path.display(),
                        project_root.display()
                    )));
                }
                if path.exists() && !path.canonicalize()?.starts_with(&project_root) {
                    return Err(NylError::config(format!(
                        "Remote renderer path {} resolves outside {}",
                        path.display(),
                        project_root.display()
                    )));
                }
            }
        }
        let cluster_values = serde_json::to_value(&cluster.spec.values)?;
        let target_values = serde_json::to_value(&target.spec.values)?;
        let effective_values = crate::util::deep_merge_value(Some(cluster_values), target_values);

        let mut cluster_context = serde_json::to_value(cluster)?;
        cluster_context
            .get_mut("spec")
            .and_then(Value::as_object_mut)
            .expect("serialized Cluster spec is an object")
            .remove("live");
        let target_context = target_template_context(target, load_secrets)?;
        let template_context = if load_secrets {
            let secrets = SecretsConfig::load_from_dir(None, Some(&project_root))?;
            TemplateContext::build(effective_values.clone(), &secrets)?
        } else {
            TemplateContext {
                values: effective_values,
                secrets: serde_json::json!({}),
                env: serde_json::Map::new(),
                cluster: None,
                target: None,
            }
        }
        .with_gitops_context(cluster_context, target_context);

        Ok(Self {
            project_root,
            project_config,
            target_name: target.metadata.name.clone(),
            kube_version,
            api_versions,
            template_context,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    pub fn template_context(&self) -> &TemplateContext {
        &self.template_context
    }

    /// Render one source file without contacting a Kubernetes cluster.
    pub async fn render_release_file(&self, path: &Path) -> Result<RenderedRelease> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        let path_text = path
            .to_str()
            .ok_or_else(|| NylError::config(format!("Release path is not valid UTF-8: {}", path.display())))?;

        let resources = load_resources(path_text, &self.template_context)?;
        let (kube_version, api_versions) = if needs_helm_rendering(&resources, &self.project_config) {
            (self.kube_version.clone(), self.api_versions.clone())
        } else {
            (String::new(), Vec::new())
        };

        let mut manifests = Vec::new();
        let mut pending = resources;
        for _ in 0..10 {
            let mut next = Vec::new();
            for resource in pending {
                for manifest in generate_resource(
                    &resource,
                    &self.template_context,
                    &self.project_config,
                    &kube_version,
                    &api_versions,
                    None,
                    false,
                )
                .await?
                {
                    if is_renderable_resource(&manifest, &self.project_config) {
                        next.push(manifest);
                    } else {
                        manifests.push(manifest);
                    }
                }
            }
            pending = next;
            if pending.is_empty() {
                break;
            }
        }
        manifests.extend(pending);

        let (release, manifests) = extract_nyl_release(&manifests)?;
        let strip_mode = resolve_strip_empty_metadata_labels_mode(
            self.project_config.get_strip_empty_metadata_labels_mode(),
            release.as_ref(),
        );

        let (generators, manifests) = extract_application_generators(&manifests)?;
        if !generators.is_empty() {
            return Err(NylError::config(format!(
                "ApplicationGenerator is not supported in rendered GitOps source {}",
                path.display()
            )));
        }

        let (policies, manifests) = extract_all_kyverno_policies(&manifests)?;
        let global = policies.get(&KyvernoScope::Global).cloned().unwrap_or_default();
        let manifests = if global.is_empty() {
            manifests
        } else {
            apply_kyverno_policies(&manifests, &global)?
        };
        let (manifests, _) = deduplicate_manifests(manifests)?;
        let manifests = prepare_manifests_for_output(&manifests, strip_mode.should_strip(false));

        Ok(RenderedRelease { release, manifests })
    }
}

fn target_template_context(target: &GitOpsTarget, trusted_source: bool) -> Result<Value> {
    let mut context = serde_json::to_value(target)?;
    if !trusted_source {
        let publication = context
            .get_mut("spec")
            .and_then(Value::as_object_mut)
            .and_then(|spec| spec.get_mut("publication"))
            .and_then(Value::as_object_mut)
            .expect("serialized GitOpsTarget publication is an object");
        publication.remove("repository");
    }
    Ok(context)
}

fn required_cluster_capabilities(cluster: &Cluster) -> Result<(String, Vec<String>)> {
    let kube_version = cluster.spec.kubernetes.kube_version.clone().ok_or_else(|| {
        NylError::config(format!(
            "Cluster {:?} requires spec.kubernetes.kubeVersion for target rendering",
            cluster.metadata.name
        ))
    })?;
    if cluster.spec.kubernetes.api_versions.is_empty() {
        return Err(NylError::config(format!(
            "Cluster {:?} requires non-empty spec.kubernetes.apiVersions for target rendering",
            cluster.metadata.name
        )));
    }
    Ok((kube_version, cluster.spec.kubernetes.api_versions.clone()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::constants::API_VERSION_GITOPS;

    fn target() -> GitOpsTarget {
        serde_json::from_value(serde_json::json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": "GitOpsTarget",
            "metadata": {"name": "production", "labels": {"environment": "production"}},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "values": {"nested": {"target": true}},
                "publication": {
                    "repository": {"repoURL": "https://example.invalid/deploy.git"},
                    "revision": "deploy/production"
                }
            }
        }))
        .unwrap()
    }

    fn cluster() -> Cluster {
        serde_json::from_value(serde_json::json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": "Cluster",
            "metadata": {"name": "kasoku", "labels": {"region": "fsn1"}},
            "spec": {
                "destination": {"server": "https://kubernetes.default.svc"},
                "kubernetes": {"kubeVersion": "1.31.4", "apiVersions": ["v1", "apps/v1"]},
                "values": {"nested": {"cluster": true}},
                "live": {"context": "kasoku-admin"}
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn target_values_and_context_are_visible() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            r#"apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: {{ target.metadata.name }}
  namespace: production
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: context
data:
  cluster: "{{ values.nested.cluster }}"
  target: "{{ values.nested.target }}"
  environment: "{{ target.metadata.labels.environment }}"
  region: "{{ cluster.metadata.labels.region }}"
  clusterName: "{{ cluster.metadata.name }}"
"#,
        )
        .unwrap();

        let session = RenderSession::for_target(temp.path(), &target(), &cluster()).unwrap();
        let rendered = session.render_release_file(Path::new("app.yaml")).await.unwrap();
        assert_eq!(rendered.release.unwrap().metadata.name, "production");
        assert_eq!(rendered.manifests[0]["data"]["cluster"], "true");
        assert_eq!(rendered.manifests[0]["data"]["target"], "true");
        assert_eq!(rendered.manifests[0]["data"]["environment"], "production");
        assert_eq!(rendered.manifests[0]["data"]["region"], "fsn1");
        assert_eq!(rendered.manifests[0]["data"]["clusterName"], "kasoku");
        let context = session.template_context().to_json();
        assert!(context.get("profile").is_none());
        assert!(context["cluster"]["spec"].get("live").is_none());
    }

    #[tokio::test]
    async fn rejects_cmp_application_generator() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            r"apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: legacy
spec: {}
",
        )
        .unwrap();

        let session = RenderSession::for_target(temp.path(), &target(), &cluster()).unwrap();
        let error = session.render_release_file(Path::new("app.yaml")).await.unwrap_err();
        assert!(error.to_string().contains("ApplicationGenerator"));
    }

    #[test]
    fn remote_renderer_context_exposes_neither_secrets_nor_process_environment() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();

        let session = RenderSession::for_remote_target(temp.path(), &target(), &cluster()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));
        assert!(context["target"]["spec"]["publication"].get("repository").is_none());

        let session = RenderSession::for_untrusted_source(temp.path(), &target(), &cluster()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));
        assert!(context["target"]["spec"]["publication"].get("repository").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn remote_renderer_rejects_symlinks_in_its_checkout() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        symlink("/tmp", temp.path().join("components")).unwrap();

        let error = RenderSession::for_remote_target(temp.path(), &target(), &cluster())
            .err()
            .unwrap();
        assert!(error.to_string().contains("symbolic links"));
    }
}
