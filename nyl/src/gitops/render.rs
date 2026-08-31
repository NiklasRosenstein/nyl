//! Target-aware, offline rendering for rendered GitOps trees.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::commands::render::{
    deduplicate_manifests, generate_resource, is_renderable_resource, load_resources, needs_helm_rendering,
    prepare_manifests_for_output, resolve_offline_kubernetes_target, resolve_strip_empty_metadata_labels_mode,
    select_profile_from_project,
};
use crate::config::ProjectConfig;
use crate::postprocess::apply_kyverno_policies;
use crate::profiles::{deep_merge_value, Profile};
use crate::resources::{
    extract_all_kyverno_policies, extract_application_generators, extract_nyl_release, GitOpsTarget, KyvernoScope,
    NylRelease,
};
use crate::secrets::SecretsConfig;
use crate::template::TemplateContext;
use crate::{NylError, Result};

/// Immutable rendering state shared by every release in one GitOps target.
pub struct RenderSession {
    project_root: PathBuf,
    project_config: ProjectConfig,
    profile: Profile,
    profile_name: String,
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
    pub fn for_target(project_root: &Path, target: &GitOpsTarget) -> Result<Self> {
        Self::build(project_root, target, true, false)
    }

    /// Build with the central project configuration but without secrets or
    /// process environment for independently controlled source manifests.
    pub fn for_untrusted_source(project_root: &Path, target: &GitOpsTarget) -> Result<Self> {
        Self::build(project_root, target, false, false)
    }

    /// Build a restricted remote-source session. Remote projects cannot load a
    /// secrets provider from their checkout.
    pub fn for_remote_target(project_root: &Path, target: &GitOpsTarget) -> Result<Self> {
        Self::build(project_root, target, false, true)
    }

    fn build(project_root: &Path, target: &GitOpsTarget, load_secrets: bool, restrict_checkout: bool) -> Result<Self> {
        target.validate()?;
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
        let (mut profile, profile_name) = select_profile_from_project(&project_config, Some(&target.spec.profile))?;

        let profile_values = serde_json::to_value(&profile.values)?;
        let target_values = serde_json::to_value(&target.spec.values)?;
        let effective_values = deep_merge_value(Some(profile_values), target_values);
        profile.values = serde_json::from_value(effective_values)
            .map_err(|error| NylError::config(format!("Failed to build effective target values: {error}")))?;

        let target_context = serde_json::json!({
            "name": target.metadata.name,
            "labels": target.metadata.labels,
            "profile": target.spec.profile,
            "destination": target.spec.destination,
        });
        let template_context = if load_secrets {
            let secrets = SecretsConfig::load_from_dir(None, Some(&project_root))?;
            TemplateContext::build(&profile, &secrets, &profile_name)?
        } else {
            TemplateContext {
                values: serde_json::to_value(&profile.values)?,
                secrets: serde_json::json!({}),
                profile: profile_name.clone(),
                env: serde_json::Map::new(),
                target: None,
            }
        }
        .with_target(target_context);

        Ok(Self {
            project_root,
            project_config,
            profile,
            profile_name,
            template_context,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
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
            resolve_offline_kubernetes_target(&self.project_config, &self.profile_name, None, &[])?
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
                "profile": "production",
                "values": {"nested": {"target": true}},
                "destination": {
                    "repository": {"repoURL": "https://example.invalid/deploy.git"},
                    "revision": "deploy/production"
                }
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn target_values_and_context_are_visible() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("nyl.toml"),
            r"[profile.production.values]
[profile.production.values.nested]
profile = true
",
        )
        .unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            r#"apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: {{ target.name }}
  namespace: production
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: context
data:
  profile: "{{ values.nested.profile }}"
  target: "{{ values.nested.target }}"
  environment: "{{ target.labels.environment }}"
"#,
        )
        .unwrap();

        let session = RenderSession::for_target(temp.path(), &target()).unwrap();
        let rendered = session.render_release_file(Path::new("app.yaml")).await.unwrap();
        assert_eq!(rendered.release.unwrap().metadata.name, "production");
        assert_eq!(rendered.manifests[0]["data"]["profile"], "true");
        assert_eq!(rendered.manifests[0]["data"]["target"], "true");
        assert_eq!(rendered.manifests[0]["data"]["environment"], "production");
    }

    #[tokio::test]
    async fn rejects_cmp_application_generator() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[profile.production]\n").unwrap();
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

        let session = RenderSession::for_target(temp.path(), &target()).unwrap();
        let error = session.render_release_file(Path::new("app.yaml")).await.unwrap_err();
        assert!(error.to_string().contains("ApplicationGenerator"));
    }

    #[test]
    fn remote_renderer_context_exposes_neither_secrets_nor_process_environment() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[profile.production]\n").unwrap();

        let session = RenderSession::for_remote_target(temp.path(), &target()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));

        let session = RenderSession::for_untrusted_source(temp.path(), &target()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));
    }

    #[cfg(unix)]
    #[test]
    fn remote_renderer_rejects_symlinks_in_its_checkout() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[profile.production]\n").unwrap();
        symlink("/tmp", temp.path().join("components")).unwrap();

        let error = RenderSession::for_remote_target(temp.path(), &target()).err().unwrap();
        assert!(error.to_string().contains("symbolic links"));
    }
}
