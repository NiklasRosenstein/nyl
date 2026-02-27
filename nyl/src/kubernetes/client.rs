use async_trait::async_trait;
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams},
    discovery::{ApiCapabilities, ApiResource, Discovery, Scope},
    Client, ResourceExt,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::{
    kubernetes::resource::{ApplyOutcome, GroupVersionKind, ResourceKey},
    profiles::{KubeconfigSource, Profile},
    NylError, Result,
};

/// Trait for Kubernetes client operations
#[async_trait]
pub trait KubeClient: Send + Sync {
    /// Get a resource from the cluster
    async fn get_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<DynamicObject>>;

    /// Apply a resource to the cluster using server-side apply
    async fn apply_resource(
        &self,
        resource: &DynamicObject,
        field_manager: &str,
        dry_run: bool,
    ) -> Result<ApplyOutcome>;

    /// Get the Kubernetes server version string (e.g., "1.28.0")
    async fn get_server_version(&self) -> Result<String>;

    /// Get all available API versions from the cluster
    async fn get_api_versions(&self) -> Result<Vec<String>>;

    /// Return true if the resource kind is namespaced.
    async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool>;

    /// Return the default namespace from kubeconfig/context.
    fn default_namespace(&self) -> &str;

    /// Delete a resource from the cluster
    async fn delete_resource(&self, gvk: &GroupVersionKind, namespace: Option<&str>, name: &str) -> Result<()>;

    /// Get server-normalized version of a resource using dry-run apply
    ///
    /// Sends the resource to the server with dry-run enabled to:
    /// - Apply server-side defaults
    /// - Run validation and admission webhooks
    /// - Return normalized resource without persisting
    async fn get_normalized_resource(&self, resource: &DynamicObject, field_manager: &str) -> Result<DynamicObject>;
}

/// Production Kubernetes client using kube-rs
pub struct KubeRsClient {
    client: Client,
    discovery: Arc<Discovery>,
    api_resources: HashMap<GroupVersionKind, (ApiResource, ApiCapabilities)>,
}

impl KubeRsClient {
    /// Build kube config from explicit path/context options.
    ///
    /// Notes for exec-based auth plugins:
    /// - kube-client is responsible for spawning the exec plugin
    /// - when supported by kube-client, plugin stderr should be visible in CLI stderr
    pub async fn load_kube_config(path: Option<&Path>, context: Option<&str>) -> Result<kube::Config> {
        tracing::debug!(
            kubeconfig_path = %path.map_or_else(|| "<default>".to_string(), |p| p.display().to_string()),
            kubeconfig_context = %context.unwrap_or("<default>"),
            "Starting Kubernetes config load (may invoke kubeconfig exec auth plugin)"
        );

        if let Some(ctx) = context {
            let kubeconfig = if let Some(path) = path {
                kube::config::Kubeconfig::read_from(path)?
            } else {
                kube::config::Kubeconfig::read()?
            };

            return kube::Config::from_custom_kubeconfig(
                kubeconfig,
                &kube::config::KubeConfigOptions {
                    context: Some(ctx.to_string()),
                    ..Default::default()
                },
            )
            .await
            .inspect(|_| tracing::debug!("Finished Kubernetes config load from custom kubeconfig"))
            .map_err(Into::into);
        }

        if let Some(path) = path {
            return kube::Config::from_custom_kubeconfig(
                kube::config::Kubeconfig::read_from(path)?,
                &kube::config::KubeConfigOptions::default(),
            )
            .await
            .inspect(|_| tracing::debug!("Finished Kubernetes config load from kubeconfig path"))
            .map_err(Into::into);
        }

        kube::Config::infer()
            .await
            .inspect(|_| tracing::debug!("Finished Kubernetes config load via inferred kubeconfig"))
            .map_err(Into::into)
    }

    /// Build kube config from a Nyl profile and optional context override.
    pub async fn load_kube_config_from_profile(
        profile: &Profile,
        context_override: Option<&str>,
    ) -> Result<kube::Config> {
        let kubeconfig = &profile.kubeconfig;

        if matches!(kubeconfig, KubeconfigSource::Ssh { .. }) {
            return Err(NylError::Config(
                "SSH kubeconfig is not yet supported. This feature is planned for Phase 5.".to_string(),
            ));
        }

        let KubeconfigSource::Local { path, context } = kubeconfig else {
            unreachable!()
        };

        let resolved_context = context_override.or(context.as_deref());
        Self::load_kube_config(path.as_deref(), resolved_context).await
    }

    /// Create a new Kubernetes client from a profile
    pub async fn from_profile(profile: &Profile, context_override: Option<&str>) -> Result<Self> {
        tracing::debug!(
            has_context_override = context_override.is_some(),
            "Initializing Kubernetes client from profile"
        );
        let config = Self::load_kube_config_from_profile(profile, context_override).await?;
        tracing::debug!("Creating kube-rs client from loaded config");
        let client = Client::try_from(config)?;
        tracing::debug!("Running Kubernetes API discovery");
        let discovery = Arc::new(Discovery::new(client.clone()).run().await?);
        let api_resources = Self::build_api_resource_index(&discovery);
        tracing::debug!("Kubernetes client initialization complete");

        Ok(Self {
            client,
            discovery,
            api_resources,
        })
    }

    /// Create a new Kubernetes client from an existing kube::Client
    pub async fn from_client(client: Client) -> Result<Self> {
        let discovery = Arc::new(Discovery::new(client.clone()).run().await?);
        let api_resources = Self::build_api_resource_index(&discovery);
        Ok(Self {
            client,
            discovery,
            api_resources,
        })
    }

    fn build_api_resource_index(discovery: &Discovery) -> HashMap<GroupVersionKind, (ApiResource, ApiCapabilities)> {
        let mut index = HashMap::new();

        for group in discovery.groups() {
            for (ar, caps) in group.recommended_resources() {
                let gvk = GroupVersionKind {
                    group: ar.group.clone(),
                    version: ar.version.clone(),
                    kind: ar.kind.clone(),
                };
                index.entry(gvk).or_insert_with(|| (ar.clone(), caps.clone()));
            }
        }

        index
    }

    /// Discover the API resource for a given GVK
    fn discover_api_resource(&self, gvk: &GroupVersionKind) -> Result<(ApiResource, ApiCapabilities)> {
        if let Some((ar, caps)) = self.api_resources.get(gvk) {
            return Ok((ar.clone(), caps.clone()));
        }

        if let Some((ar, caps)) = self.find_api_resource_by_group_kind(gvk) {
            tracing::debug!(
                requested_group = %gvk.group,
                requested_version = %gvk.version,
                requested_kind = %gvk.kind,
                resolved_group = %ar.group,
                resolved_version = %ar.version,
                resolved_kind = %ar.kind,
                "Falling back to discovered API resource with matching group/kind but different version"
            );
            return Ok((ar, caps));
        }

        let group_version = if gvk.group.is_empty() {
            gvk.version.clone()
        } else {
            format!("{}/{}", gvk.group, gvk.version)
        };

        let available_versions: Vec<String> = self
            .api_resources
            .keys()
            .filter(|known| known.group == gvk.group && known.kind == gvk.kind)
            .map(|known| known.version.clone())
            .collect();

        let versions_hint = if available_versions.is_empty() {
            String::new()
        } else {
            format!(" (available versions for this kind: {})", available_versions.join(", "))
        };

        Err(NylError::Config(format!(
            "API resource not found for {}/{}{}",
            group_version, gvk.kind, versions_hint
        )))
    }

    fn find_api_resource_by_group_kind(&self, gvk: &GroupVersionKind) -> Option<(ApiResource, ApiCapabilities)> {
        find_api_resource_by_group_kind_in_index(&self.api_resources, gvk)
    }

    async fn try_resolve_scope_from_crd(&self, gvk: &GroupVersionKind) -> Result<Option<bool>> {
        // Core API resources cannot be represented by CRDs.
        if gvk.group.is_empty() {
            return Ok(None);
        }

        let crd_api: Api<DynamicObject> = Api::all_with(self.client.clone(), &crd_api_resource());
        let crds = crd_api.list(&Default::default()).await?;

        for crd in crds.items {
            if let Some(namespaced) = scope_from_crd_for_gvk(&crd, gvk) {
                return Ok(Some(namespaced));
            }
        }

        Ok(None)
    }
}

fn find_api_resource_by_group_kind_in_index(
    api_resources: &HashMap<GroupVersionKind, (ApiResource, ApiCapabilities)>,
    gvk: &GroupVersionKind,
) -> Option<(ApiResource, ApiCapabilities)> {
    api_resources
        .iter()
        .find(|(known, _)| known.group == gvk.group && known.kind == gvk.kind)
        .map(|(_, value)| value.clone())
}

#[async_trait]
impl KubeClient for KubeRsClient {
    async fn get_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<DynamicObject>> {
        let (ar, caps) = self.discover_api_resource(gvk)?;

        let api: Api<DynamicObject> = if caps.scope == Scope::Namespaced {
            let ns = namespace
                .ok_or_else(|| NylError::Config(format!("Namespace required for namespaced resource {}", gvk.kind)))?;
            Api::namespaced_with(self.client.clone(), ns, &ar)
        } else {
            Api::all_with(self.client.clone(), &ar)
        };

        match api.get(name).await {
            Ok(obj) => Ok(Some(obj)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn apply_resource(
        &self,
        resource: &DynamicObject,
        field_manager: &str,
        dry_run: bool,
    ) -> Result<ApplyOutcome> {
        // Extract metadata
        let name = resource.name_any();
        let namespace = resource.namespace();

        // Extract GVK from resource - use data field which is a serde_json::Value
        let resource_json = serde_json::to_value(resource)?;
        let gvk = crate::kubernetes::resource::extract_gvk(&resource_json)?;

        // Create ResourceKey
        let resource_key = ResourceKey {
            gvk: gvk.clone(),
            namespace: namespace.clone(),
            name: name.clone(),
        };

        let (ar, caps) = self.discover_api_resource(&gvk)?;

        // Create API client
        let api: Api<DynamicObject> = if caps.scope == Scope::Namespaced {
            let ns = namespace
                .as_deref()
                .ok_or_else(|| NylError::Config(format!("Namespace required for namespaced resource {}", gvk.kind)))?;
            Api::namespaced_with(self.client.clone(), ns, &ar)
        } else {
            Api::all_with(self.client.clone(), &ar)
        };

        // Check if resource exists and get its current resourceVersion
        let existing = self.get_resource(&gvk, namespace.as_deref(), &name).await?;
        let old_resource_version = existing.as_ref().and_then(|r| r.metadata.resource_version.clone());

        // Setup patch parameters
        let mut patch_params = PatchParams::apply(field_manager).force();
        if dry_run {
            patch_params = patch_params.dry_run();
        }

        // Apply the resource (with or without dry-run) to validate and get server response
        let patch = Patch::Apply(resource);
        let updated = api.patch(&name, &patch_params, &patch).await?;

        // Determine outcome
        let base_outcome = if dry_run {
            // In dry-run mode, resourceVersion won't change even if resource would be modified
            // So we need to compare resources to detect actual changes
            if let Some(ref existing_resource) = existing {
                // Resource exists - check if it would change using diff comparison
                let desired_json = serde_json::to_value(resource)?;
                let existing_json = serde_json::to_value(existing_resource)?;

                // Use DiffEngine to check if resources are equivalent (ignoring server-managed fields)
                let would_change = !crate::kubernetes::diff::DiffEngine::are_equivalent(&desired_json, &existing_json)?;

                if would_change {
                    ApplyOutcome::Updated {
                        resource_key: resource_key.clone(),
                    }
                } else {
                    ApplyOutcome::Unchanged {
                        resource_key: resource_key.clone(),
                    }
                }
            } else {
                // Resource doesn't exist - would be created
                ApplyOutcome::Created {
                    resource_key: resource_key.clone(),
                }
            }
        } else {
            // In normal mode, check resourceVersion to detect changes
            let new_resource_version = updated.metadata.resource_version.clone();

            if existing.is_none() {
                // Resource didn't exist - it was created
                ApplyOutcome::Created {
                    resource_key: resource_key.clone(),
                }
            } else if old_resource_version != new_resource_version {
                // Resource existed and resourceVersion changed - it was updated
                ApplyOutcome::Updated {
                    resource_key: resource_key.clone(),
                }
            } else {
                // Resource existed but resourceVersion didn't change - it was unchanged
                ApplyOutcome::Unchanged {
                    resource_key: resource_key.clone(),
                }
            }
        };

        Ok(if dry_run {
            ApplyOutcome::DryRun {
                would_be: Box::new(base_outcome),
            }
        } else {
            base_outcome
        })
    }

    async fn get_server_version(&self) -> Result<String> {
        let version = self.client.apiserver_version().await?;
        // Format as "major.minor.patch" (e.g., "1.28.3")
        let version_str = if version.git_version.starts_with('v') {
            version.git_version[1..].to_string()
        } else {
            version.git_version
        };
        Ok(version_str)
    }

    async fn get_api_versions(&self) -> Result<Vec<String>> {
        use std::collections::HashSet;

        let mut api_versions = HashSet::new();

        // Iterate through all discovered API groups and resources
        for group in self.discovery.groups() {
            for (ar, _caps) in group.recommended_resources() {
                // Format as {api_version}/{kind} (e.g., "apps/v1/Deployment", "v1/Pod")
                api_versions.insert(format!("{}/{}", ar.api_version, ar.kind));
            }
        }

        // Convert to sorted Vec for consistent output
        let mut result: Vec<String> = api_versions.into_iter().collect();
        result.sort();
        Ok(result)
    }

    async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool> {
        if is_known_cluster_scoped_kind(&gvk.kind) {
            return Ok(false);
        }
        match self.discover_api_resource(gvk) {
            Ok((_ar, caps)) => Ok(caps.scope == Scope::Namespaced),
            Err(err) => {
                if is_api_resource_not_found_config_error(&err) {
                    if let Some(namespaced) = self.try_resolve_scope_from_crd(gvk).await? {
                        return Ok(namespaced);
                    }
                }
                Err(err)
            }
        }
    }

    fn default_namespace(&self) -> &str {
        self.client.default_namespace()
    }

    async fn delete_resource(&self, gvk: &GroupVersionKind, namespace: Option<&str>, name: &str) -> Result<()> {
        let (ar, caps) = self.discover_api_resource(gvk)?;

        // Create API client
        let api: Api<DynamicObject> = if caps.scope == Scope::Namespaced {
            let ns = namespace
                .ok_or_else(|| NylError::Config(format!("Namespace required for namespaced resource {}", gvk.kind)))?;
            Api::namespaced_with(self.client.clone(), ns, &ar)
        } else {
            Api::all_with(self.client.clone(), &ar)
        };

        // Delete the resource
        match api.delete(name, &kube::api::DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Resource already deleted or doesn't exist - not an error
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn get_normalized_resource(&self, resource: &DynamicObject, field_manager: &str) -> Result<DynamicObject> {
        // Extract metadata
        let name = resource.name_any();
        let namespace = resource.namespace();

        // Extract GVK from resource
        let resource_json = serde_json::to_value(resource)?;
        let gvk = crate::kubernetes::resource::extract_gvk(&resource_json)?;
        let (ar, caps) = self.discover_api_resource(&gvk)?;

        // Create API client
        let api: Api<DynamicObject> = if caps.scope == Scope::Namespaced {
            let ns = namespace
                .as_deref()
                .ok_or_else(|| NylError::Config(format!("Namespace required for namespaced resource {}", gvk.kind)))?;
            Api::namespaced_with(self.client.clone(), ns, &ar)
        } else {
            Api::all_with(self.client.clone(), &ar)
        };

        // Apply with dry-run to get server-normalized version
        let patch_params = PatchParams::apply(field_manager).force().dry_run();
        let patch = Patch::Apply(resource);
        let normalized = api.patch(&name, &patch_params, &patch).await?;

        Ok(normalized)
    }
}

/// Mock Kubernetes client for testing
pub struct MockKubeClient {
    resources: Arc<Mutex<HashMap<ResourceKey, DynamicObject>>>,
    kind_scope_overrides: Arc<Mutex<HashMap<String, bool>>>,
    default_namespace: String,
}

impl MockKubeClient {
    /// Create a new mock client
    pub fn new() -> Self {
        Self::with_default_namespace("default")
    }

    /// Create a new mock client with a specific default namespace.
    pub fn with_default_namespace(default_namespace: impl Into<String>) -> Self {
        Self {
            resources: Arc::new(Mutex::new(HashMap::new())),
            kind_scope_overrides: Arc::new(Mutex::new(HashMap::new())),
            default_namespace: default_namespace.into(),
        }
    }

    /// Add a resource to the mock store
    pub fn add_resource(&self, key: ResourceKey, resource: DynamicObject) {
        let mut store = self.resources.lock().unwrap();
        store.insert(key, resource);
    }

    /// Get all resources in the mock store
    pub fn get_all_resources(&self) -> HashMap<ResourceKey, DynamicObject> {
        let store = self.resources.lock().unwrap();
        store.clone()
    }

    /// Set namespaced scope for a given Kind for testing.
    pub fn set_kind_scope(&self, kind: &str, namespaced: bool) {
        let mut overrides = self.kind_scope_overrides.lock().unwrap();
        overrides.insert(kind.to_string(), namespaced);
    }

    fn default_scope_for_kind(kind: &str) -> bool {
        !is_known_cluster_scoped_kind(kind)
    }
}

fn is_known_cluster_scoped_kind(kind: &str) -> bool {
    matches!(
        kind,
        "APIService"
            | "ClusterRole"
            | "ClusterRoleBinding"
            | "CustomResourceDefinition"
            | "MutatingWebhookConfiguration"
            | "Namespace"
            | "Node"
            | "PersistentVolume"
            | "PriorityClass"
            | "RuntimeClass"
            | "StorageClass"
            | "ValidatingWebhookConfiguration"
            | "VolumeAttachment"
    )
}

fn crd_api_resource() -> ApiResource {
    ApiResource {
        group: "apiextensions.k8s.io".to_string(),
        version: "v1".to_string(),
        api_version: "apiextensions.k8s.io/v1".to_string(),
        kind: "CustomResourceDefinition".to_string(),
        plural: "customresourcedefinitions".to_string(),
    }
}

fn scope_from_crd_for_gvk(crd: &DynamicObject, gvk: &GroupVersionKind) -> Option<bool> {
    let value = serde_json::to_value(crd).ok()?;
    let spec = value.get("spec")?;
    let group = spec.get("group")?.as_str()?;
    let kind = spec.get("names")?.get("kind")?.as_str()?;
    let scope = spec.get("scope")?.as_str()?;
    let versions = spec.get("versions")?.as_array()?;

    if group != gvk.group || kind != gvk.kind {
        return None;
    }

    // Mirror Kubernetes behavior: only served versions are requestable.
    let version_is_served = versions.iter().any(|v| {
        let name = v.get("name").and_then(serde_json::Value::as_str);
        let served = v.get("served").and_then(serde_json::Value::as_bool).unwrap_or(false);
        name == Some(gvk.version.as_str()) && served
    });
    if !version_is_served {
        return None;
    }

    match scope {
        "Namespaced" => Some(true),
        "Cluster" => Some(false),
        _ => None,
    }
}

fn is_api_resource_not_found_config_error(err: &NylError) -> bool {
    matches!(err, NylError::Config(message) if message.starts_with("API resource not found for "))
}

impl Default for MockKubeClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl KubeClient for MockKubeClient {
    async fn get_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<DynamicObject>> {
        let key = ResourceKey {
            gvk: gvk.clone(),
            namespace: namespace.map(|s| s.to_string()),
            name: name.to_string(),
        };

        let store = self.resources.lock().unwrap();
        Ok(store.get(&key).cloned())
    }

    async fn apply_resource(
        &self,
        resource: &DynamicObject,
        _field_manager: &str,
        dry_run: bool,
    ) -> Result<ApplyOutcome> {
        // Extract metadata
        let name = resource.name_any();
        let namespace = resource.namespace();

        // Extract GVK - use data field which is a serde_json::Value
        let resource_json = serde_json::to_value(resource)?;
        let gvk = crate::kubernetes::resource::extract_gvk(&resource_json)?;
        let key = ResourceKey {
            gvk,
            namespace: namespace.clone(),
            name: name.clone(),
        };

        let mut store = self.resources.lock().unwrap();
        let existing = store.get(&key).cloned();

        // Check if resource actually changed by comparing the data
        let changed = existing.as_ref().is_none_or(|old| {
            // Compare the full resource data (including metadata)
            let old_json = serde_json::to_value(old).unwrap_or_default();
            let new_json = serde_json::to_value(resource).unwrap_or_default();
            old_json != new_json
        });

        // Only store if not dry run
        if !dry_run {
            store.insert(key.clone(), resource.clone());
        }

        let base_outcome = if existing.is_none() {
            // Resource didn't exist - created
            ApplyOutcome::Created {
                resource_key: key.clone(),
            }
        } else if changed {
            // Resource existed and changed - updated
            ApplyOutcome::Updated {
                resource_key: key.clone(),
            }
        } else {
            // Resource existed but didn't change - unchanged
            ApplyOutcome::Unchanged {
                resource_key: key.clone(),
            }
        };

        Ok(if dry_run {
            ApplyOutcome::DryRun {
                would_be: Box::new(base_outcome),
            }
        } else {
            base_outcome
        })
    }

    async fn get_server_version(&self) -> Result<String> {
        // Return a mock version for testing
        Ok("1.28.0".to_string())
    }

    async fn get_api_versions(&self) -> Result<Vec<String>> {
        // Return common API versions with kinds for testing
        Ok(vec![
            "apps/v1/Deployment".to_string(),
            "batch/v1/Job".to_string(),
            "v1/Pod".to_string(),
            "v1/Service".to_string(),
        ])
    }

    async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool> {
        let overrides = self.kind_scope_overrides.lock().unwrap();
        Ok(overrides
            .get(&gvk.kind)
            .copied()
            .unwrap_or_else(|| Self::default_scope_for_kind(&gvk.kind)))
    }

    fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    async fn delete_resource(&self, gvk: &GroupVersionKind, namespace: Option<&str>, name: &str) -> Result<()> {
        let key = ResourceKey {
            gvk: gvk.clone(),
            namespace: namespace.map(|s| s.to_string()),
            name: name.to_string(),
        };

        let mut store = self.resources.lock().unwrap();
        // Remove returns the old value if it existed, None if it didn't
        // Either way, we consider the operation successful
        store.remove(&key);
        Ok(())
    }

    async fn get_normalized_resource(&self, resource: &DynamicObject, _field_manager: &str) -> Result<DynamicObject> {
        // For testing, simply return the resource as-is
        // Tests can inject pre-normalized resources if needed
        Ok(resource.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_mock_client_get_missing() {
        let client = MockKubeClient::new();
        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        };

        let result = client.get_resource(&gvk, Some("default"), "test").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_mock_client_apply_create() {
        let client = MockKubeClient::new();

        let json_data = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        let outcome = client.apply_resource(&resource, "nyl", false).await.unwrap();

        match &outcome {
            ApplyOutcome::Created { .. } => {
                assert_eq!(outcome.kind(), "ConfigMap");
                assert_eq!(outcome.name(), "test");
                assert_eq!(outcome.namespace(), Some("default"));
            }
            _ => panic!("Expected Created outcome"),
        }
    }

    #[tokio::test]
    async fn test_mock_client_apply_update() {
        let client = MockKubeClient::new();

        let json_data = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "value1"
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        // First apply
        client.apply_resource(&resource, "nyl", false).await.unwrap();

        // Second apply with different data (update)
        let updated_json = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "value2"  // Changed value
            }
        });

        let updated_resource: DynamicObject = serde_json::from_value(updated_json).unwrap();
        let outcome = client.apply_resource(&updated_resource, "nyl", false).await.unwrap();

        match &outcome {
            ApplyOutcome::Updated { .. } => {
                assert_eq!(outcome.kind(), "ConfigMap");
                assert_eq!(outcome.name(), "test");
                assert_eq!(outcome.namespace(), Some("default"));
            }
            _ => panic!("Expected Updated outcome"),
        }
    }

    #[tokio::test]
    async fn test_mock_client_apply_unchanged() {
        let client = MockKubeClient::new();

        let json_data = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "value"
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        // First apply
        client.apply_resource(&resource, "nyl", false).await.unwrap();

        // Second apply with same data (unchanged)
        let outcome = client.apply_resource(&resource, "nyl", false).await.unwrap();

        match &outcome {
            ApplyOutcome::Unchanged { .. } => {
                assert_eq!(outcome.kind(), "ConfigMap");
                assert_eq!(outcome.name(), "test");
                assert_eq!(outcome.namespace(), Some("default"));
            }
            _ => panic!("Expected Unchanged outcome, got {:?}", outcome),
        }
    }

    #[tokio::test]
    async fn test_mock_client_dry_run() {
        let client = MockKubeClient::new();

        let json_data = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        let outcome = client.apply_resource(&resource, "nyl", true).await.unwrap();

        match outcome {
            ApplyOutcome::DryRun { would_be } => match *would_be {
                ApplyOutcome::Created { .. } => {}
                _ => panic!("Expected Created in DryRun"),
            },
            _ => panic!("Expected DryRun outcome"),
        }

        // Verify resource was not stored
        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        };
        let result = client.get_resource(&gvk, Some("default"), "test").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_mock_client_delete() {
        let client = MockKubeClient::new();

        let json_data = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        // Apply resource first
        client.apply_resource(&resource, "nyl", false).await.unwrap();

        // Verify it exists
        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        };
        let result = client.get_resource(&gvk, Some("default"), "test").await.unwrap();
        assert!(result.is_some());

        // Delete the resource
        client.delete_resource(&gvk, Some("default"), "test").await.unwrap();

        // Verify it's gone
        let result = client.get_resource(&gvk, Some("default"), "test").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_mock_client_delete_nonexistent() {
        let client = MockKubeClient::new();

        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        };

        // Delete a resource that doesn't exist - should not error
        let result = client.delete_resource(&gvk, Some("default"), "test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mock_client_is_namespaced_override() {
        let client = MockKubeClient::new();
        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ServiceAccount".to_string(),
        };

        assert!(client.is_namespaced(&gvk).await.unwrap());
        client.set_kind_scope("ServiceAccount", false);
        assert!(!client.is_namespaced(&gvk).await.unwrap());
    }

    #[test]
    fn test_mock_client_default_namespace() {
        let client = MockKubeClient::with_default_namespace("custom");
        assert_eq!(client.default_namespace(), "custom");
    }

    #[test]
    fn test_find_api_resource_by_group_kind_falls_back_to_other_version() {
        let mut api_resources = HashMap::new();

        let known_gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v2".to_string(),
            kind: "ClusterPolicy".to_string(),
        };
        let ar = ApiResource {
            group: "kyverno.io".to_string(),
            version: "v2".to_string(),
            api_version: "kyverno.io/v2".to_string(),
            kind: "ClusterPolicy".to_string(),
            plural: "clusterpolicies".to_string(),
        };
        let caps = ApiCapabilities {
            scope: Scope::Cluster,
            subresources: vec![],
            operations: vec![],
        };
        api_resources.insert(known_gvk, (ar.clone(), caps.clone()));

        let requested = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };

        let (resolved_ar, resolved_caps) =
            find_api_resource_by_group_kind_in_index(&api_resources, &requested).unwrap();
        assert_eq!(resolved_ar.version, "v2");
        assert_eq!(resolved_ar.kind, "ClusterPolicy");
        assert_eq!(resolved_caps.scope, Scope::Cluster);
    }

    #[test]
    fn test_scope_from_crd_for_gvk_cluster() {
        let crd: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "clusterpolicies.kyverno.io"},
            "spec": {
                "group": "kyverno.io",
                "names": {"kind": "ClusterPolicy"},
                "scope": "Cluster",
                "versions": [
                    {"name": "v1", "served": true}
                ]
            }
        }))
        .unwrap();

        let gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };

        assert_eq!(scope_from_crd_for_gvk(&crd, &gvk), Some(false));
    }

    #[test]
    fn test_scope_from_crd_for_gvk_namespaced() {
        let crd: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.example.com"},
            "spec": {
                "group": "example.com",
                "names": {"kind": "Widget"},
                "scope": "Namespaced",
                "versions": [
                    {"name": "v1", "served": true}
                ]
            }
        }))
        .unwrap();

        let gvk = GroupVersionKind {
            group: "example.com".to_string(),
            version: "v1".to_string(),
            kind: "Widget".to_string(),
        };

        assert_eq!(scope_from_crd_for_gvk(&crd, &gvk), Some(true));
    }

    #[test]
    fn test_scope_from_crd_for_gvk_rejects_unserved_version() {
        let crd: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "clusterpolicies.kyverno.io"},
            "spec": {
                "group": "kyverno.io",
                "names": {"kind": "ClusterPolicy"},
                "scope": "Cluster",
                "versions": [
                    {"name": "v1", "served": false},
                    {"name": "v2beta1", "served": true}
                ]
            }
        }))
        .unwrap();

        let gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };

        assert_eq!(scope_from_crd_for_gvk(&crd, &gvk), None);
    }

    #[test]
    fn test_scope_from_crd_for_gvk_rejects_missing_version() {
        let crd: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "clusterpolicies.kyverno.io"},
            "spec": {
                "group": "kyverno.io",
                "names": {"kind": "ClusterPolicy"},
                "scope": "Cluster",
                "versions": [
                    {"name": "v2beta1", "served": true}
                ]
            }
        }))
        .unwrap();

        let gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };

        assert_eq!(scope_from_crd_for_gvk(&crd, &gvk), None);
    }
}
