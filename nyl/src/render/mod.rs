//! Authoritative Kubernetes manifest rendering engine.

pub(crate) mod artifact;
mod bundle;
pub mod cache;
mod expand;
mod postprocess;
mod provenance;
mod session;

#[cfg(test)]
pub(crate) use bundle::load_release_bundle;
pub(crate) use bundle::{load_release_bundle_with_root, static_release_envelope};
pub(crate) use expand::*;
pub(crate) use postprocess::*;
pub(crate) use provenance::{RenderProvenance, RenderResource};
pub use session::{RenderPathMode, RenderRequest, RenderSession, RenderedBundle};

use glob::{glob, Pattern};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{
    config::StripEmptyMetadataLabelsMode,
    kubernetes::ResourceKey,
    resources::{
        extract_release, is_supported_application_array_field_path, is_supported_application_field_path,
        join_field_path_segments, path_matches_glob, Release,
    },
    template::{TemplateContext, TemplateEngine},
    util::deep_merge_value,
    NylError, Result,
};

#[cfg(test)]
use crate::config::ProjectConfig;
#[cfg(test)]
use crate::constants::API_VERSION_ARGOCD;
#[cfg(test)]
use crate::resources::RemoteManifest;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StripEmptyMetadataLabelsMode;
    use crate::constants::API_VERSION_GITOPS;
    use crate::resources::{
        ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
        ApplicationSource, ReleaseArgoCdSpec, ReleaseCustomizationPolicy, ReleaseMetadata, ReleaseSpec,
    };
    use git2::{Repository, RepositoryInitOptions, Signature};
    use std::sync::{Arc, Mutex, MutexGuard};
    use tempfile::TempDir;

    static APPGEN_OVERRIDE_ENV_LOCK: Mutex<()> = Mutex::new(());
    static PWD_CWD_LOCK: Mutex<()> = Mutex::new(());

    fn lock_appgen_override_env() -> MutexGuard<'static, ()> {
        APPGEN_OVERRIDE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_pwd_cwd() -> MutexGuard<'static, ()> {
        PWD_CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_project_config() -> ProjectConfig {
        ProjectConfig {
            file: None,
            config: crate::config::ProjectFile::default(),
        }
    }

    fn create_test_worktree_paths() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = TempDir::new().unwrap();
        let source_root = temp.path().join("worktree");
        let file_path = source_root.join("clusters/default/addons/nginx.yaml");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\n").unwrap();
        (temp, source_root, file_path)
    }

    fn test_release_with_override(override_value: serde_json::Value) -> Release {
        use crate::resources::{ReleaseArgoCdSpec, ReleaseMetadata, ReleaseSpec};

        Release {
            api_version: API_VERSION_GITOPS.to_string(),
            kind: "Release".to_string(),
            metadata: ReleaseMetadata {
                name: "nginx".to_string(),
                namespace: "web".to_string(),
            },
            spec: ReleaseSpec {
                strip_empty_metadata_labels: None,
                argocd: Some(ReleaseArgoCdSpec {
                    application_override: Some(serde_json::from_value(override_value).unwrap()),
                }),
                ..Default::default()
            },
        }
    }

    fn test_application_generator(
        sync_policy: Option<crate::resources::SyncPolicy>,
        release_customization: Option<crate::resources::ReleaseCustomizationPolicy>,
    ) -> crate::resources::ApplicationGenerator {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;

        ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "HEAD".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization,
            },
        }
    }

    struct PwdCwdGuard {
        pwd: Option<String>,
        cwd: std::path::PathBuf,
    }

    impl PwdCwdGuard {
        fn new() -> Self {
            Self {
                pwd: std::env::var("PWD").ok(),
                cwd: std::env::current_dir().unwrap(),
            }
        }
    }

    impl Drop for PwdCwdGuard {
        fn drop(&mut self) {
            match &self.pwd {
                Some(pwd) => std::env::set_var("PWD", pwd),
                None => std::env::remove_var("PWD"),
            }
            let _ = std::env::set_current_dir(&self.cwd);
        }
    }

    fn create_test_application_generator(
        repo_url: &str,
        target_revision: &str,
    ) -> crate::resources::ApplicationGenerator {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;

        ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: repo_url.to_string(),
                    target_revision: target_revision.to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        }
    }

    fn create_local_git_repo(branch: &str, remote_url: &str) -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let mut init = RepositoryInitOptions::new();
        init.initial_head(branch);
        let repo = Repository::init_opts(temp.path(), &init).unwrap();

        std::fs::write(temp.path().join("README.md"), "test\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).unwrap();
        drop(tree);

        let remote = repo.remote("origin", remote_url).unwrap();
        drop(remote);

        (temp, repo)
    }

    fn test_application_generator_for_warning() -> crate::resources::ApplicationGenerator {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;

        ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "main".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        }
    }

    #[test]
    fn test_missing_release_warning_message_includes_counts_and_generator_name() {
        let generator = test_application_generator_for_warning();
        let msg = missing_release_warning_message(
            &generator,
            2,
            5,
            &[
                "clusters/default/a.yaml".to_string(),
                "clusters/default/b.yaml".to_string(),
            ],
        );
        assert!(msg.contains("ApplicationGenerator apps"));
        assert!(msg.contains("repoURL=https://github.com/example/repo.git"));
        assert!(msg.contains("targetRevision=main"));
        assert!(msg.contains("source paths=clusters/default"));
        assert!(msg.contains("skipped 2/5 file(s)"));
        assert!(msg.contains("no Release was found"));
        assert!(msg.contains("clusters/default/a.yaml"));
        assert!(msg.contains("clusters/default/b.yaml"));
        assert!(msg.contains("Skipped files:"));
    }

    #[test]
    fn test_missing_release_warning_message_lists_all_skipped_files() {
        let generator = test_application_generator_for_warning();
        let msg = missing_release_warning_message(
            &generator,
            4,
            4,
            &[
                "a.yaml".to_string(),
                "b.yaml".to_string(),
                "c.yaml".to_string(),
                "d.yaml".to_string(),
            ],
        );
        assert!(msg.contains("a.yaml"));
        assert!(msg.contains("b.yaml"));
        assert!(msg.contains("c.yaml"));
        assert!(msg.contains("d.yaml"));
        assert!(!msg.contains("Examples:"));
    }

    #[test]
    fn test_missing_release_warning_message_without_examples() {
        let generator = test_application_generator_for_warning();
        let msg = missing_release_warning_message(&generator, 1, 1, &[]);
        assert!(msg.contains("ApplicationGenerator apps"));
        assert!(msg.contains("skipped 1/1 file(s)"));
        assert!(!msg.contains("Skipped files:"));
    }

    #[test]
    fn test_parse_yaml_documents_single() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test
";
        let source_ctx = crate::util::SourceContext::new(std::path::PathBuf::from("test.yaml"));
        let docs = source_ctx.parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_parse_yaml_documents_multiple() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: Service
metadata:
  name: test2
";
        let source_ctx = crate::util::SourceContext::new(std::path::PathBuf::from("test.yaml"));
        let docs = source_ctx.parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_filter_resources_no_filter() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources.clone(), None).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_resources_by_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources, Some("ConfigMap")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_filter_resources_by_api_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment"
            }),
        ];

        let filtered = filter_resources(resources, Some("apps/v1/Deployment")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "Deployment");
    }

    #[test]
    fn test_create_argocd_application_from_generator_sets_plugin_env() {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource, ReleaseMetadata, ReleaseSpec,
        };
        use std::collections::{BTreeMap, HashMap};

        let release = Release {
            api_version: API_VERSION_GITOPS.to_string(),
            kind: "Release".to_string(),
            metadata: ReleaseMetadata {
                name: "nginx".to_string(),
                namespace: "web".to_string(),
            },
            spec: ReleaseSpec::default(),
        };

        let generator = ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "HEAD".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: BTreeMap::from([
                        ("ENVIRONMENT".to_string(), "production".to_string()),
                        ("REGION".to_string(), "eu-central-1".to_string()),
                    ]),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        };

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, Some("prod"))
                .unwrap();

        assert_eq!(app["spec"]["source"]["path"], "clusters/default/addons");
        assert_eq!(app["spec"]["source"]["plugin"]["name"], "nyl-v2");

        let env = app["spec"]["source"]["plugin"]["env"].as_array().unwrap();
        let template_input = env
            .iter()
            .find(|v| v["name"] == "NYL_CMP_TEMPLATE_INPUT")
            .and_then(|v| v["value"].as_str())
            .unwrap();
        assert_eq!(template_input, "nginx.yaml");
        assert_eq!(
            env.iter()
                .find(|v| v["name"] == "NYL_CMP_TARGET")
                .and_then(|v| v["value"].as_str()),
            Some("prod")
        );
        assert_eq!(
            env.iter()
                .find(|v| v["name"] == "ENVIRONMENT")
                .and_then(|v| v["value"].as_str()),
            Some("production")
        );
        assert_eq!(
            env.iter()
                .find(|v| v["name"] == "REGION")
                .and_then(|v| v["value"].as_str()),
            Some("eu-central-1")
        );
    }

    fn make_test_release(override_map: serde_json::Value) -> Release {
        Release {
            api_version: API_VERSION_GITOPS.to_string(),
            kind: "Release".to_string(),
            metadata: ReleaseMetadata {
                name: "nginx".to_string(),
                namespace: "web".to_string(),
            },
            spec: ReleaseSpec {
                strip_empty_metadata_labels: None,
                argocd: Some(ReleaseArgoCdSpec {
                    application_override: Some(serde_json::from_value(override_map).unwrap()),
                }),
                ..Default::default()
            },
        }
    }

    fn make_test_generator(release_customization: Option<ReleaseCustomizationPolicy>) -> ApplicationGenerator {
        use std::collections::HashMap;
        ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "HEAD".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization,
            },
        }
    }

    #[test]
    fn test_release_customization_appends_warning_to_existing_info_entries() {
        let release = make_test_release(serde_json::json!({
            "spec": {
                "info": [
                    {"name": "team-note", "value": "kept"}
                ],
                "syncPolicy": {
                    "automated": {
                        "prune": true
                    }
                }
            }
        }));

        let generator = make_test_generator(Some(ReleaseCustomizationPolicy {
            allowed_paths: Some(vec!["spec.info.**".to_string(), "spec.syncPolicy.**".to_string()]),
            denied_paths: vec!["spec.syncPolicy.automated.prune".to_string()],
        }));

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert!(app["spec"]["syncPolicy"]["automated"]["prune"].is_null());
        let info = app["spec"]["info"].as_array().unwrap();
        assert!(info.iter().any(|entry| entry["name"] == "team-note"));
        assert!(info.iter().any(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME));
    }

    #[test]
    fn test_release_customization_plus_sync_options_uses_canonical_path_for_denies() {
        use crate::resources::{ReleaseCustomizationPolicy, SyncPolicy};

        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "+syncOptions": ["RespectIgnoreDifferences=false"]
                }
            }
        }));
        let generator = test_application_generator(
            Some(SyncPolicy {
                automated: None,
                sync_options: vec!["ServerSideApply=true".to_string()],
            }),
            Some(ReleaseCustomizationPolicy {
                allowed_paths: Some(vec!["spec.syncPolicy.**".to_string()]),
                denied_paths: vec!["spec.syncPolicy.syncOptions".to_string()],
            }),
        );

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(
            app["spec"]["syncPolicy"]["syncOptions"],
            serde_json::json!(["ServerSideApply=true"])
        );
        let warning = app["spec"]["info"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME)
            .and_then(|entry| entry["value"].as_str())
            .unwrap();
        assert!(warning.contains("+syncOptions"));
    }

    #[test]
    fn test_release_customization_plus_sync_options_with_non_array_value_warns_and_ignores() {
        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "+syncOptions": "RespectIgnoreDifferences=false"
                }
            }
        }));
        let generator = test_application_generator(None, None);

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert!(app["spec"]["syncPolicy"].is_null());
        let warning = app["spec"]["info"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME)
            .and_then(|entry| entry["value"].as_str())
            .unwrap();
        assert!(warning.contains("invalid"));
        assert!(warning.contains("+syncOptions"));
    }

    #[test]
    fn test_release_customization_plus_non_array_field_warns_and_ignores() {
        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "+automated": [{"prune": true}]
                }
            }
        }));
        let generator = test_application_generator(None, None);

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert!(app["spec"]["syncPolicy"]["automated"].is_null());
        let warning = app["spec"]["info"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME)
            .and_then(|entry| entry["value"].as_str())
            .unwrap();
        assert!(warning.contains("+automated"));
    }

    #[test]
    fn test_default_allowed_paths_permit_ignore_differences_and_sync_policy() {
        let release = make_test_release(serde_json::json!({
            "spec": {
                "ignoreDifferences": [
                    {
                        "kind": "Deployment",
                        "jsonPointers": ["/spec/replicas"]
                    }
                ],
                "syncPolicy": {
                    "automated": {
                        "selfHeal": true
                    }
                }
            }
        }));

        let generator = make_test_generator(Some(ReleaseCustomizationPolicy {
            allowed_paths: None,
            denied_paths: Vec::new(),
        }));

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(app["spec"]["ignoreDifferences"][0]["kind"], "Deployment");
        assert_eq!(app["spec"]["ignoreDifferences"][0]["jsonPointers"][0], "/spec/replicas");
        assert_eq!(app["spec"]["syncPolicy"]["automated"]["selfHeal"], true);
    }

    #[test]
    fn test_default_allowed_paths_apply_when_customization_policy_omitted() {
        let release = make_test_release(serde_json::json!({
            "spec": {
                "ignoreDifferences": [
                    {
                        "group": "apps",
                        "kind": "Deployment",
                        "jsonPointers": ["/spec/replicas"]
                    }
                ],
                "syncPolicy": {
                    "automated": {
                        "selfHeal": true
                    }
                }
            }
        }));

        let generator = make_test_generator(None);

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(app["spec"]["ignoreDifferences"][0]["group"], "apps");
        assert_eq!(app["spec"]["ignoreDifferences"][0]["kind"], "Deployment");
        assert_eq!(app["spec"]["syncPolicy"]["automated"]["selfHeal"], true);
    }

    #[test]
    fn test_release_customization_plus_sync_options_uses_canonical_path_for_policy_checks() {
        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "+syncOptions": ["RespectIgnoreDifferences=false"]
                }
            }
        }));
        let generator = test_application_generator(
            Some(crate::resources::SyncPolicy {
                automated: None,
                sync_options: vec!["ServerSideApply=true".to_string()],
            }),
            Some(crate::resources::ReleaseCustomizationPolicy {
                allowed_paths: Some(vec!["spec.syncPolicy.syncOptions".to_string()]),
                denied_paths: Vec::new(),
            }),
        );

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(
            app["spec"]["syncPolicy"]["syncOptions"],
            serde_json::json!(["ServerSideApply=true", "RespectIgnoreDifferences=false"])
        );
        assert!(app["spec"]["info"].is_null());
    }

    #[test]
    fn test_release_customization_invalid_plus_sync_options_warns_and_ignores_override() {
        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "+syncOptions": {
                        "bad": "value"
                    }
                }
            }
        }));
        let generator = test_application_generator(
            Some(crate::resources::SyncPolicy {
                automated: None,
                sync_options: vec!["ServerSideApply=true".to_string()],
            }),
            None,
        );

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(
            app["spec"]["syncPolicy"]["syncOptions"],
            serde_json::json!(["ServerSideApply=true"])
        );
        let warning = app["spec"]["info"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME)
            .unwrap();
        let warning_value = warning["value"].as_str().unwrap();
        assert!(warning_value.contains("invalid=1"));
        assert!(warning_value.contains("+syncOptions"));
    }

    #[test]
    fn test_release_customization_plus_sync_policy_warns_when_target_is_not_a_list() {
        let release = test_release_with_override(serde_json::json!({
            "spec": {
                "+syncPolicy": [
                    {"syncOptions": ["RespectIgnoreDifferences=false"]}
                ]
            }
        }));
        let generator = test_application_generator(
            Some(crate::resources::SyncPolicy {
                automated: None,
                sync_options: vec!["ServerSideApply=true".to_string()],
            }),
            None,
        );

        let (_temp, source_root, file_path) = create_test_worktree_paths();
        let app =
            create_argocd_application_from_generator(&release, &file_path, &source_root, &generator, None).unwrap();

        assert_eq!(
            app["spec"]["syncPolicy"]["syncOptions"],
            serde_json::json!(["ServerSideApply=true"])
        );
        let warning = app["spec"]["info"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == NYL_CUSTOMIZATION_WARNING_NAME)
            .unwrap();
        let warning_value = warning["value"].as_str().unwrap();
        assert!(warning_value.contains("invalid=1"));
        assert!(warning_value.contains("+syncPolicy"));
    }

    #[test]
    fn test_resolve_application_generator_source_path_uses_local_override() {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;
        use std::fs;
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("clusters/default");
        fs::create_dir_all(&source_dir).unwrap();
        std::env::set_var("NYL_APPGEN_REPO_PATH_OVERRIDE", temp.path());

        let generator = ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "main".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        };

        let resolved = resolve_application_generator_source_path(&generator, None, None).unwrap();
        assert_eq!(resolved, temp.path());

        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");
    }

    #[test]
    fn test_resolve_application_generator_source_path_reuses_local_git_repo_for_head() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();
        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");

        let (repo_dir, _repo) = create_local_git_repo("main", "git@gitlab.com:NiklasRosenstein/config.git");
        let _pwd_guard = PwdCwdGuard::new();
        std::env::set_current_dir(repo_dir.path()).unwrap();
        std::env::set_var("PWD", repo_dir.path());

        let generator = create_test_application_generator("git@gitlab.com:NiklasRosenstein/config.git", "HEAD");

        let resolved = resolve_application_generator_source_path(&generator, None, None).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            repo_dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_resolve_application_generator_source_path_reuses_local_git_repo_for_current_branch() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();
        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");

        let (repo_dir, _repo) = create_local_git_repo("main", "git@github.com:example/repo.git");
        let _pwd_guard = PwdCwdGuard::new();
        std::env::set_current_dir(repo_dir.path()).unwrap();
        std::env::set_var("PWD", repo_dir.path());

        let generator = create_test_application_generator("ssh://git@github.com/example/repo", "main");

        let resolved = resolve_application_generator_source_path(&generator, None, None).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            repo_dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_repo_root_path_returns_bare_repo_path() {
        let temp = TempDir::new().unwrap();
        let bare_repo_path = temp.path().join("repo.git");
        let repo = Repository::init_bare(&bare_repo_path).unwrap();

        assert_eq!(
            repo_root_path(&repo).unwrap().canonicalize().unwrap(),
            bare_repo_path.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_try_resolve_application_generator_source_from_local_git_repo_skips_on_repo_mismatch() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();

        let (repo_dir, _repo) = create_local_git_repo("main", "git@github.com:example/repo.git");
        let _pwd_guard = PwdCwdGuard::new();
        std::env::set_current_dir(repo_dir.path()).unwrap();
        std::env::set_var("PWD", repo_dir.path());

        let generator = create_test_application_generator("git@github.com:example/other.git", "HEAD");

        let resolved = try_resolve_application_generator_source_from_local_git_repo(&generator);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_try_resolve_application_generator_source_from_local_git_repo_skips_on_target_revision_mismatch() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();

        let (repo_dir, _repo) = create_local_git_repo("main", "git@github.com:example/repo.git");
        let _pwd_guard = PwdCwdGuard::new();
        std::env::set_current_dir(repo_dir.path()).unwrap();
        std::env::set_var("PWD", repo_dir.path());

        let generator = create_test_application_generator("git@github.com:example/repo.git", "develop");

        let resolved = try_resolve_application_generator_source_from_local_git_repo(&generator);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_try_resolve_application_generator_source_from_local_git_repo_skips_on_detached_head() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();

        let (repo_dir, repo) = create_local_git_repo("main", "git@github.com:example/repo.git");
        let _pwd_guard = PwdCwdGuard::new();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.set_head_detached(head_commit.id()).unwrap();
        std::env::set_current_dir(repo_dir.path()).unwrap();
        std::env::set_var("PWD", repo_dir.path());

        let generator = create_test_application_generator("git@github.com:example/repo.git", "main");

        let resolved = try_resolve_application_generator_source_from_local_git_repo(&generator);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_try_resolve_application_generator_source_from_local_git_repo_skips_outside_git_repo() {
        let _guard = lock_appgen_override_env();
        let _cwd_lock = lock_pwd_cwd();

        let temp = TempDir::new().unwrap();
        let _pwd_guard = PwdCwdGuard::new();
        std::env::set_current_dir(temp.path()).unwrap();
        std::env::set_var("PWD", temp.path());

        let generator = create_test_application_generator("git@github.com:example/repo.git", "HEAD");

        let resolved = try_resolve_application_generator_source_from_local_git_repo(&generator);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_resolve_application_generator_source_path_errors_when_override_root_missing() {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;

        let _guard = lock_appgen_override_env();
        std::env::set_var("NYL_APPGEN_REPO_PATH_OVERRIDE", "/definitely/not/a/real/path");

        let generator = ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "main".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        };

        let err = resolve_application_generator_source_path(&generator, None, None).unwrap_err();
        assert!(err.to_string().contains("NYL_APPGEN_REPO_PATH_OVERRIDE"));

        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");
    }

    #[test]
    fn test_find_yaml_files_filtered_errors_when_source_selector_missing_under_override() {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use std::collections::HashMap;
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        let temp = TempDir::new().unwrap();
        std::env::set_var("NYL_APPGEN_REPO_PATH_OVERRIDE", temp.path());

        let generator = ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "main".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        };

        let source_root = resolve_application_generator_source_path(&generator, None, None).unwrap();
        let selectors = application_generator_source_selectors(&generator);
        let err = find_yaml_files_filtered(
            &source_root,
            &selectors,
            &generator.spec.source.include,
            &generator.spec.source.exclude,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"));

        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");
    }

    #[test]
    fn test_resolve_application_generator_source_path_falls_back_to_git_resolution() {
        use crate::resources::{
            ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata, ApplicationGeneratorSpec,
            ApplicationSource,
        };
        use git2::Repository;
        use std::collections::HashMap;
        use std::fs;
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        std::env::remove_var("NYL_APPGEN_REPO_PATH_OVERRIDE");

        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        fs::create_dir_all(repo_dir.path().join("clusters/default")).unwrap();
        fs::write(
            repo_dir.path().join("clusters/default/example.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\n",
        )
        .unwrap();

        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).unwrap();

        let generator = ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "apps".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: repo_dir.path().to_string_lossy().to_string(),
                    target_revision: "HEAD".to_string(),
                    path: Some("clusters/default".to_string()),
                    paths: None,
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                    plugin_env: std::collections::BTreeMap::default(),
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
                release_customization: None,
            },
        };

        let resolved = resolve_application_generator_source_path(&generator, None, None).unwrap();
        assert!(resolved.exists());
        assert!(resolved.join("clusters/default/example.yaml").exists());
    }

    #[test]
    fn test_resolve_override_root_path_prefers_pwd_for_relative_paths() {
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::env::set_var("PWD", temp.path());

        let resolved = resolve_override_root_path("NYL_APPGEN_REPO_PATH_OVERRIDE", "repo").unwrap();
        assert_eq!(resolved.canonicalize().unwrap(), repo.canonicalize().unwrap());
        std::env::remove_var("PWD");
    }

    #[test]
    fn test_resolve_override_root_path_at_git_resolves_repo_root_from_pwd() {
        use git2::Repository;
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("repo");
        let nested = repo_root.join("nested/path");
        std::fs::create_dir_all(&nested).unwrap();
        Repository::init(&repo_root).unwrap();
        std::env::set_var("PWD", &nested);

        let resolved = resolve_override_root_path("NYL_APPGEN_REPO_PATH_OVERRIDE", "@git").unwrap();
        assert_eq!(resolved.canonicalize().unwrap(), repo_root.canonicalize().unwrap());
        std::env::remove_var("PWD");
    }

    #[test]
    fn test_resolve_override_root_path_at_git_errors_when_pwd_not_in_repo() {
        use tempfile::TempDir;

        let _guard = lock_appgen_override_env();
        let temp = TempDir::new().unwrap();
        std::env::set_var("PWD", temp.path());

        let err = resolve_override_root_path("NYL_APPGEN_REPO_PATH_OVERRIDE", "@git").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("NYL_APPGEN_REPO_PATH_OVERRIDE"));
        assert!(msg.contains("@git"));
        std::env::remove_var("PWD");
    }

    #[cfg(unix)]
    #[test]
    fn test_find_yaml_files_filtered_ignores_broken_symlink_entries() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let good = root.join("good.yaml");
        std::fs::write(&good, "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\n").unwrap();

        let broken_target = root.join("does-not-exist");
        let broken_link = root.join("broken-link");
        symlink(&broken_target, &broken_link).unwrap();

        let files = find_yaml_files_filtered(root, &[".".to_string()], &["*.yaml".to_string()], &[]).unwrap();
        assert!(files.contains(&good));
    }

    #[test]
    fn test_find_yaml_files_filtered_directory_selector_is_non_recursive() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let root = temp.path();
        std::fs::write(root.join("top.yaml"), "apiVersion: v1\nkind: ConfigMap\n").unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/deep.yaml"), "apiVersion: v1\nkind: ConfigMap\n").unwrap();

        let files = find_yaml_files_filtered(root, &[".".to_string()], &["*.yaml".to_string()], &[]).unwrap();
        assert!(files.contains(&root.join("top.yaml")));
        assert!(!files.contains(&root.join("nested/deep.yaml")));
    }

    #[test]
    fn test_find_yaml_files_filtered_applies_relative_exclude_pattern() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let root = temp.path();
        std::fs::write(root.join("top.yaml"), "apiVersion: v1\nkind: ConfigMap\n").unwrap();
        std::fs::create_dir_all(root.join(".nyl/cache")).unwrap();
        std::fs::write(
            root.join(".nyl/cache/ignored.yaml"),
            "apiVersion: v1\nkind: ConfigMap\n",
        )
        .unwrap();

        let files = find_yaml_files_filtered(
            root,
            &["**/*.yaml".to_string()],
            &["*.yaml".to_string()],
            &[".nyl/**".to_string()],
        )
        .unwrap();
        assert!(files.contains(&root.join("top.yaml")));
        assert!(!files.contains(&root.join(".nyl/cache/ignored.yaml")));
    }

    #[test]
    fn test_path_normalization_posix() {
        use std::path::Path;

        let rel_dir = Path::new("subdir/nested");
        let rel_dir_normalized = normalize_relative_path_to_posix(rel_dir);
        assert_eq!(rel_dir_normalized, "subdir/nested");
    }

    #[test]
    fn test_path_normalization_with_join() {
        use std::path::Path;

        // Test with platform-native path construction
        let rel_dir = Path::new("subdir").join("nested");
        let rel_dir_normalized = normalize_relative_path_to_posix(&rel_dir);

        // Should always produce POSIX-style paths regardless of platform
        assert_eq!(rel_dir_normalized, "subdir/nested");
    }

    #[test]
    fn test_path_normalization_root() {
        use std::path::Path;

        // Test empty path handling
        let rel_dir = Path::new("");
        let rel_dir_normalized = normalize_relative_path_to_posix(rel_dir);

        assert_eq!(rel_dir_normalized, "");
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "abd"), 1);
        assert_eq!(levenshtein_distance("abc", "abcd"), 1);
        assert_eq!(levenshtein_distance("abc", "def"), 3);
        assert_eq!(
            levenshtein_distance("nyl.niklasrosenstein.github.com", "nyl.niklasrosenstein.github.com"),
            0
        );
        assert_eq!(
            levenshtein_distance("nyl.niklasrosenstein.github.com", "nyl.nikolasrosenstein.github.com"),
            1
        );
    }

    #[test]
    fn test_add_parent_annotations() {
        use crate::constants::{
            ANNOTATION_PARENT_API_VERSION, ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE,
        };

        // Test adding annotations to a manifest
        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "test-pod",
                "namespace": "default"
            },
            "spec": {
                "containers": []
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "HelmChart",
            "my-chart",
            Some("default"),
        );

        // Verify annotations were added
        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_KIND).unwrap().as_str().unwrap(),
            "HelmChart"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAME).unwrap().as_str().unwrap(),
            "my-chart"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAMESPACE).unwrap().as_str().unwrap(),
            "default"
        );
    }

    #[test]
    fn test_is_nyl_like_api_version_exact_match() {
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com/v1"));
        assert!(is_nyl_like_api_version("components.nyl.niklasrosenstein.github.com/v1"));
        assert!(is_nyl_like_api_version("argocd.nyl.niklasrosenstein.github.com/v1"));
    }

    #[test]
    fn test_is_nyl_like_api_version_contains() {
        // Should match anything containing the domain
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com/v2"));
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com"));
        assert!(is_nyl_like_api_version("foo.nyl.niklasrosenstein.github.com/v1"));
    }

    #[test]
    fn test_is_nyl_like_api_version_similar() {
        // Typos within Levenshtein distance of 3
        assert!(is_nyl_like_api_version("nyl.nikolasrosenstein.github.com/v1")); // one character difference
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.co/v1")); // missing 'm'
    }

    #[test]
    fn test_is_nyl_like_api_version_not_similar() {
        // Standard Kubernetes API versions should not match
        assert!(!is_nyl_like_api_version("v1"));
        assert!(!is_nyl_like_api_version("apps/v1"));
        assert!(!is_nyl_like_api_version("batch/v1"));
        assert!(!is_nyl_like_api_version("argoproj.io/v1alpha1"));
    }

    #[test]
    fn test_is_known_nyl_resource_helm_chart() {
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "HelmChart",
            "metadata": {"name": "test"},
            "spec": {"chart": {"name": "nginx"}}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_component() {
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/MyComponent",
            "metadata": {"name": "test"},
            "spec": {}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_release() {
        let resource = serde_json::json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {"name": "test", "namespace": "default"}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_remote_manifest() {
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "test"},
            "spec": {"url": "https://example.com/manifests.yaml"}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_application_generator() {
        let resource = serde_json::json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {"name": "test"},
            "spec": {
                "destination": {"server": "https://k8s", "namespace": "argocd"},
                "source": {"repoURL": "https://github.com/test/repo", "path": "apps"}
            }
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_unknown() {
        // Unknown Nyl-like resource
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "UnknownKind",
            "metadata": {"name": "test"}
        });
        assert!(!is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_standard_k8s() {
        // Standard Kubernetes resource
        let resource = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        assert!(!is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_add_parent_annotations_without_namespace() {
        use crate::constants::{
            ANNOTATION_PARENT_API_VERSION, ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE,
        };

        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test-config"
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "Component",
            "my-component",
            None,
        );

        // Verify annotations were added (except namespace)
        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_KIND).unwrap().as_str().unwrap(),
            "Component"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAME).unwrap().as_str().unwrap(),
            "my-component"
        );
        // Namespace annotation should not be present
        assert!(annotations.get(ANNOTATION_PARENT_NAMESPACE).is_none());
    }

    #[test]
    fn test_add_parent_annotations_preserves_existing() {
        use crate::constants::ANNOTATION_PARENT_API_VERSION;

        // Test that existing annotations are preserved
        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": "test-service",
                "annotations": {
                    "existing-annotation": "existing-value"
                }
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "HelmChart",
            "my-chart",
            None,
        );

        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        // Original annotation should still be there
        assert_eq!(
            annotations.get("existing-annotation").unwrap().as_str().unwrap(),
            "existing-value"
        );
        // New annotation should also be there
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
    }

    #[test]
    fn test_apply_parent_tracking_annotations_remote_manifest() {
        use crate::constants::{ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE};

        let manifests = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "cm"}
        })];

        let manifests = apply_parent_tracking_annotations(
            manifests,
            true,
            "nyl.niklasrosenstein.github.com/v1",
            "RemoteManifest",
            "remote-a",
            Some("apps"),
        );

        let annotations = manifests[0]["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_KIND).unwrap().as_str().unwrap(),
            "RemoteManifest"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAME).unwrap().as_str().unwrap(),
            "remote-a"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAMESPACE).unwrap().as_str().unwrap(),
            "apps"
        );
    }

    #[test]
    fn test_needs_helm_rendering_ignores_remote_manifest() {
        let config = test_project_config();
        let resources = vec![serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "https://example.com/manifest.yaml"}
        })];

        assert!(!needs_helm_rendering(&resources, &config));
    }

    #[test]
    fn test_needs_helm_rendering_detects_helm_chart() {
        let config = test_project_config();
        let resources = vec![serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "HelmChart",
            "metadata": {"name": "chart"},
            "spec": {"chart": {"name": "nginx"}}
        })];

        assert!(needs_helm_rendering(&resources, &config));
    }

    #[test]
    fn test_is_renderable_resource_helm_chart() {
        let config = test_project_config();
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "HelmChart",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource, &config));
    }

    #[test]
    fn test_is_renderable_resource_component() {
        let config = test_project_config();
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource, &config));
    }

    #[test]
    fn test_is_renderable_resource_component_shortcut() {
        let config = test_project_config();
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "https://charts.example.com/repo#nginx@1.0.0",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource, &config));
    }

    #[test]
    fn test_is_renderable_resource_remote_manifest() {
        let config = test_project_config();
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "test"},
            "spec": {"url": "https://example.com/manifests.yaml"}
        });
        assert!(is_renderable_resource(&resource, &config));
    }

    #[test]
    fn test_is_renderable_resource_alias() {
        let mut config = test_project_config();
        config.config.project.aliases.insert(
            "myapi.io/v1/MyKind".to_string(),
            "oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4".to_string(),
        );
        let resource = serde_json::json!({
            "apiVersion": "myapi.io/v1",
            "kind": "MyKind",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource, &config));
    }

    #[test]
    fn test_is_renderable_resource_plain_k8s() {
        let config = test_project_config();
        let resource = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        assert!(!is_renderable_resource(&resource, &config));
    }

    #[tokio::test]
    async fn test_generate_resource_remote_manifest_rejects_http_url() {
        let temp = TempDir::new().unwrap();
        let config = test_project_config();
        let artifact_resolver = artifact::ArtifactResolver::new(temp.path(), &config, None).unwrap();
        let context = TemplateContext {
            values: serde_json::json!({}),
            secrets: serde_json::json!({}),
            env: serde_json::Map::new(),
            cluster: None,
            target: None,
        };
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "http://example.com/manifests.yaml"}
        });

        let result = generate_resource(
            &resource,
            &context,
            &config,
            "",
            &[],
            None,
            false,
            None,
            &artifact_resolver,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("https://"));
    }

    #[tokio::test]
    async fn test_fetch_remote_manifest_documents_fetches_urls_in_order_and_overrides_namespaces() {
        let remote_manifest = RemoteManifest::from_value(&serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote", "namespace": "target"},
            "spec": {
                "overrideNamespace": true,
                "params": {"version": "1.5.1"},
                "urls": [
                    "https://example.com/v{version}/a.yaml",
                    "https://example.com/v{version}/b.yaml"
                ]
            }
        }))
        .unwrap();
        remote_manifest.validate().unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_fetcher = Arc::clone(&calls);
        let manifests = fetch_remote_manifest_documents_with_fetcher(&remote_manifest, move |url| {
            let calls = Arc::clone(&calls_for_fetcher);
            async move {
                calls.lock().unwrap().push(url.clone());
                let name = if url.ends_with("/a.yaml") { "a" } else { "b" };
                Ok(vec![serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": name, "namespace": "source"}
                })])
            }
        })
        .await
        .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "https://example.com/v1.5.1/a.yaml".to_string(),
                "https://example.com/v1.5.1/b.yaml".to_string(),
            ]
        );
        assert_eq!(manifests[0]["metadata"]["name"], "a");
        assert_eq!(manifests[1]["metadata"]["name"], "b");
        assert_eq!(manifests[0]["metadata"]["namespace"], "target");
        assert_eq!(manifests[1]["metadata"]["namespace"], "target");
    }

    #[tokio::test]
    async fn test_fetch_remote_manifest_documents_stops_on_first_failed_url() {
        let remote_manifest = RemoteManifest::from_value(&serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "urls": [
                    "https://example.com/a.yaml",
                    "https://example.com/b.yaml",
                    "https://example.com/c.yaml"
                ]
            }
        }))
        .unwrap();
        remote_manifest.validate().unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_fetcher = Arc::clone(&calls);
        let err = fetch_remote_manifest_documents_with_fetcher(&remote_manifest, move |url| {
            let calls = Arc::clone(&calls_for_fetcher);
            async move {
                calls.lock().unwrap().push(url.clone());
                if url.ends_with("/b.yaml") {
                    return Err(NylError::Process(format!("fetch failed for {url}")));
                }
                Ok(vec![serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": "ok"}
                })])
            }
        })
        .await
        .unwrap_err();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "https://example.com/a.yaml".to_string(),
                "https://example.com/b.yaml".to_string(),
            ]
        );
        assert!(err.to_string().contains("https://example.com/b.yaml"));
    }

    #[test]
    fn test_override_fetched_manifest_namespaces_overwrites_existing_namespace() {
        let mut manifests = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "cm", "namespace": "old"}
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "dep"}
            }),
        ];

        override_fetched_manifest_namespaces(&mut manifests, Some("target"));

        assert_eq!(manifests[0]["metadata"]["namespace"], "target");
        assert!(manifests[1]["metadata"]["namespace"].is_null());
    }

    #[test]
    fn test_override_fetched_manifest_namespaces_does_not_add_metadata() {
        let mut manifests = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap"
        })];

        override_fetched_manifest_namespaces(&mut manifests, Some("target"));

        assert!(manifests[0].get("metadata").is_none());
    }

    #[test]
    fn test_override_fetched_manifest_namespaces_no_namespace_hint_is_noop() {
        let original = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "cm", "namespace": "old"}
        });
        let mut manifests = vec![original.clone()];

        override_fetched_manifest_namespaces(&mut manifests, None);

        assert_eq!(manifests[0], original);
    }

    #[test]
    fn test_override_fetched_manifest_namespaces_rewrites_cluster_role_binding_subject_namespaces() {
        let mut manifests = vec![serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRoleBinding",
            "metadata": {"name": "bind"},
            "subjects": [
                {"kind": "ServiceAccount", "name": "sa-a", "namespace": "old-a"},
                {"kind": "ServiceAccount", "name": "sa-b"},
                {"kind": "User", "name": "alice"}
            ],
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "view"}
        })];

        override_fetched_manifest_namespaces(&mut manifests, Some("target"));
        let subjects = manifests[0]["subjects"].as_array().unwrap();
        assert_eq!(subjects[0]["namespace"], "target");
        assert_eq!(subjects[1]["namespace"], "target");
        assert!(subjects[2]["namespace"].is_null());
    }

    #[test]
    fn test_override_fetched_manifest_namespaces_rewrites_role_binding_subject_namespaces() {
        let mut manifests = vec![serde_json::json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "RoleBinding",
            "metadata": {"name": "bind", "namespace": "source"},
            "subjects": [
                {"kind": "ServiceAccount", "name": "sa-a", "namespace": "old-a"},
                {"kind": "ServiceAccount", "name": "sa-b"},
                {"kind": "User", "name": "alice"}
            ],
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "view"}
        })];

        override_fetched_manifest_namespaces(&mut manifests, Some("target"));
        let subjects = manifests[0]["subjects"].as_array().unwrap();
        assert_eq!(subjects[0]["namespace"], "target");
        assert_eq!(subjects[1]["namespace"], "target");
        assert!(subjects[2]["namespace"].is_null());
    }

    #[test]
    fn test_normalize_emitted_manifests_strips_empty_metadata_labels() {
        let mut manifests = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": {}
            }
        })];

        normalize_emitted_manifests(&mut manifests);

        let metadata = manifests[0]["metadata"].as_object().unwrap();
        assert!(!metadata.contains_key("labels"));
    }

    #[test]
    fn test_normalize_emitted_manifests_preserves_non_empty_metadata_labels() {
        let mut manifests = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": {"app": "demo"}
            }
        })];

        normalize_emitted_manifests(&mut manifests);

        assert_eq!(manifests[0]["metadata"]["labels"]["app"], "demo");
    }

    #[test]
    fn test_normalize_emitted_manifests_leaves_missing_metadata_unchanged() {
        let original = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap"
        });
        let mut manifests = vec![original.clone()];

        normalize_emitted_manifests(&mut manifests);

        assert_eq!(manifests[0], original);
    }

    #[test]
    fn test_normalize_emitted_manifests_leaves_non_object_labels_unchanged() {
        let original = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": "unexpected"
            }
        });
        let mut manifests = vec![original.clone()];

        normalize_emitted_manifests(&mut manifests);

        assert_eq!(manifests[0], original);
    }

    #[test]
    fn test_normalize_emitted_manifests_removes_empty_labels_from_emitted_yaml() {
        let original = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": {}
            }
        })];

        let manifests = prepare_manifests_for_output(&original, true);

        let yaml = crate::yaml::serialize_yaml_document(&manifests[0]).unwrap();
        assert!(!yaml.contains("labels: {}"));
        assert!(!yaml.contains("labels:\n"));
        assert!(yaml.contains("name: cm"));
        assert_eq!(original[0]["metadata"]["labels"], serde_json::json!({}));
    }

    #[test]
    fn test_prepare_manifests_for_output_preserves_original_manifests() {
        let original = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": {}
            }
        })];

        let emitted = prepare_manifests_for_output(&original, true);

        assert_eq!(original[0]["metadata"]["labels"], serde_json::json!({}));
        assert!(emitted[0]["metadata"]["labels"].is_null());
    }

    #[test]
    fn test_prepare_manifests_for_output_preserves_empty_labels_when_disabled() {
        let original = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "cm",
                "labels": {}
            }
        })];

        let emitted = prepare_manifests_for_output(&original, false);

        assert_eq!(emitted[0]["metadata"]["labels"], serde_json::json!({}));
    }

    #[test]
    fn test_resolve_strip_empty_metadata_labels_mode_uses_project_default() {
        assert_eq!(
            resolve_strip_empty_metadata_labels_mode(StripEmptyMetadataLabelsMode::Argocd, None),
            StripEmptyMetadataLabelsMode::Argocd
        );
    }

    #[test]
    fn test_resolve_strip_empty_metadata_labels_mode_release_override_takes_precedence() {
        let release = Release {
            api_version: API_VERSION_GITOPS.to_string(),
            kind: "Release".to_string(),
            metadata: ReleaseMetadata {
                name: "nginx".to_string(),
                namespace: "web".to_string(),
            },
            spec: ReleaseSpec {
                strip_empty_metadata_labels: Some(StripEmptyMetadataLabelsMode::Never),
                argocd: Some(ReleaseArgoCdSpec {
                    application_override: None,
                }),
                ..Default::default()
            },
        };

        assert_eq!(
            resolve_strip_empty_metadata_labels_mode(StripEmptyMetadataLabelsMode::Always, Some(&release)),
            StripEmptyMetadataLabelsMode::Never
        );
    }

    #[test]
    fn test_strip_empty_metadata_labels_mode_should_strip_respects_argocd_environment() {
        assert!(StripEmptyMetadataLabelsMode::Always.should_strip(false));
        assert!(!StripEmptyMetadataLabelsMode::Never.should_strip(true));
        assert!(StripEmptyMetadataLabelsMode::Argocd.should_strip(true));
        assert!(!StripEmptyMetadataLabelsMode::Argocd.should_strip(false));
    }

    #[test]
    fn test_split_yaml_documents_single() {
        let raw = "apiVersion: v1\nkind: ConfigMap\n";
        let docs = split_yaml_documents(raw);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].contains("ConfigMap"));
    }

    #[test]
    fn test_split_yaml_documents_multiple() {
        let raw = "apiVersion: v1\nkind: ConfigMap\n---\napiVersion: v1\nkind: Service\n";
        let docs = split_yaml_documents(raw);
        assert_eq!(docs.len(), 2);
        assert!(docs[0].contains("ConfigMap"));
        assert!(docs[1].contains("Service"));
    }

    #[test]
    fn test_split_yaml_documents_leading_separator() {
        let raw = "---\napiVersion: v1\nkind: ConfigMap\n";
        let docs = split_yaml_documents(raw);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].contains("ConfigMap"));
    }

    #[test]
    fn test_best_effort_parse_yaml_documents_valid() {
        let raw = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\n";
        let docs = best_effort_parse_yaml_documents(raw);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_best_effort_parse_yaml_documents_skips_jinja() {
        let raw = r"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: my-app
  namespace: default
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ values.config_name }}
data:
  key: {{ values.some_value }}
";
        let docs = best_effort_parse_yaml_documents(raw);
        // The Release should parse, the ConfigMap with Jinja should be skipped
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "Release");
        assert_eq!(docs[0]["metadata"]["name"], "my-app");
    }

    #[test]
    fn test_best_effort_parse_yaml_documents_all_invalid() {
        let raw = "key: {{ values.foo }}\n---\nother: {{ values.bar }}\n";
        let docs = best_effort_parse_yaml_documents(raw);
        assert!(docs.is_empty());
    }

    #[test]
    fn release_bundle_expands_sorted_deduplicated_includes() {
        let temporary = TempDir::new().unwrap();
        std::fs::create_dir(temporary.path().join("manifests")).unwrap();
        let entry = temporary.path().join("release.yaml");
        std::fs::write(
            &entry,
            r"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: example
  namespace: example
spec:
  include:
    - manifests/*.yaml
    - manifests/config-*.yaml
",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("manifests/config-one.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: one\n",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("manifests/config-two.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: two\n",
        )
        .unwrap();
        let context = TemplateContext {
            values: serde_json::json!({}),
            secrets: serde_json::json!({}),
            env: serde_json::Map::new(),
            cluster: None,
            target: None,
        };

        let bundle = load_release_bundle_with_root(&entry, &context, Some(temporary.path())).unwrap();
        assert_eq!(bundle.resources.len(), 3);
        assert_eq!(bundle.inputs.len(), 3);
        assert_eq!(bundle.resources[1]["metadata"]["name"], "one");
        assert_eq!(bundle.resources[2]["metadata"]["name"], "two");
        assert_eq!(
            bundle.resources[0].provenance.to_string(),
            "Source: release.yaml (document 1)"
        );
        assert_eq!(
            bundle.resources[1].provenance.to_string(),
            "Source: manifests/config-one.yaml (document 1)"
        );
    }

    #[test]
    fn release_bundle_rejects_unmatched_and_nested_includes() {
        let temporary = TempDir::new().unwrap();
        let entry = temporary.path().join("release.yaml");
        std::fs::write(
            &entry,
            "apiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: example\n  namespace: example\nspec:\n  include: ['missing/*.yaml']\n",
        )
        .unwrap();
        let context = TemplateContext {
            values: serde_json::json!({}),
            secrets: serde_json::json!({}),
            env: serde_json::Map::new(),
            cluster: None,
            target: None,
        };
        assert!(load_release_bundle(&entry, &context)
            .unwrap_err()
            .to_string()
            .contains("matched no additional"));

        std::fs::write(
            &entry,
            "apiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: example\n  namespace: example\nspec:\n  include: ['nested.yaml']\n",
        )
        .unwrap();
        std::fs::write(
            temporary.path().join("nested.yaml"),
            "apiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: nested\n  namespace: nested\n",
        )
        .unwrap();
        assert!(load_release_bundle(&entry, &context)
            .unwrap_err()
            .to_string()
            .contains("another Release"));
    }

    #[test]
    fn static_release_discovery_requires_a_literal_document() {
        let temporary = TempDir::new().unwrap();
        let structural = temporary.path().join("structural.yaml");
        std::fs::write(
            &structural,
            "{% if values.enabled %}\napiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: example\n  namespace: example\n{% endif %}\n",
        )
        .unwrap();
        assert!(static_release_envelope(&structural).unwrap().is_none());

        let value_templated = temporary.path().join("value.yaml");
        std::fs::write(
            &value_templated,
            "apiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: example\n  namespace: '{{ values.namespace }}'\n",
        )
        .unwrap();
        assert_eq!(
            static_release_envelope(&value_templated)
                .unwrap()
                .and_then(|release| release.name),
            Some("example".to_string())
        );
    }

    #[test]
    fn test_disable_automated_sync() {
        let mut app = serde_json::json!({
            "spec": {
                "syncPolicy": {
                    "automated": {
                        "prune": true,
                        "selfHeal": true
                    },
                    "syncOptions": ["CreateNamespace=true"]
                }
            }
        });
        disable_automated_sync(&mut app);
        assert!(app["spec"]["syncPolicy"]["automated"].is_null());
        assert_eq!(app["spec"]["syncPolicy"]["syncOptions"][0], "CreateNamespace=true");
    }

    #[test]
    fn test_disable_automated_sync_no_sync_policy() {
        let mut app = serde_json::json!({
            "spec": {}
        });
        disable_automated_sync(&mut app);
        // Should not panic or error
        assert!(app["spec"]["syncPolicy"].is_null());
    }

    #[test]
    fn test_append_render_error_info() {
        let mut app = serde_json::json!({
            "spec": {}
        });
        let file_path = "test/file.yaml";
        append_render_error_info(&mut app, file_path, "undefined variable 'foo'").unwrap();
        let info = app["spec"]["info"].as_array().unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0]["name"], "nyl-render-error");
        assert!(info[0]["value"].as_str().unwrap().contains("undefined variable"));
        assert!(info[0]["value"].as_str().unwrap().contains("test/file.yaml"));
    }

    #[test]
    fn test_append_render_error_info_preserves_existing() {
        let mut app = serde_json::json!({
            "spec": {
                "info": [
                    {"name": "existing", "value": "entry"}
                ]
            }
        });
        let file_path = "test/file.yaml";
        append_render_error_info(&mut app, file_path, "error").unwrap();
        let info = app["spec"]["info"].as_array().unwrap();
        assert_eq!(info.len(), 2);
        assert_eq!(info[0]["name"], "existing");
        assert_eq!(info[1]["name"], "nyl-render-error");
    }
}
