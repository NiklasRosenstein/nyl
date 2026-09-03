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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProjectConfig, StripEmptyMetadataLabelsMode};
    use crate::constants::API_VERSION_GITOPS;
    use crate::resources::{Release, ReleaseArgoCdSpec, ReleaseMetadata, ReleaseSpec, RemoteManifest};
    use crate::template::TemplateContext;
    use crate::NylError;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn test_project_config() -> ProjectConfig {
        ProjectConfig {
            file: None,
            config: crate::config::ProjectFile::default(),
        }
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
}
