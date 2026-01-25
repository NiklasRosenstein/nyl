use async_trait::async_trait;
use kube::{
    api::{Api, DynamicObject, Patch, PatchParams},
    discovery::{ApiCapabilities, ApiResource, Discovery, Scope},
    Client, ResourceExt,
};
use std::collections::HashMap;
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
}

impl KubeRsClient {
    /// Create a new Kubernetes client from a profile
    pub async fn from_profile(profile: &Profile, context_override: Option<&str>) -> Result<Self> {
        let kubeconfig = &profile.kubeconfig;

        // Check for SSH kubeconfig (not supported yet)
        if matches!(kubeconfig, KubeconfigSource::Ssh { .. }) {
            return Err(NylError::Config(
                "SSH kubeconfig is not yet supported. This feature is planned for Phase 5.".to_string(),
            ));
        }

        // Use Local kubeconfig
        let KubeconfigSource::Local { path, context } = kubeconfig else {
            unreachable!()
        };

        let mut config = if let Some(path) = path {
            kube::Config::from_custom_kubeconfig(
                kube::config::Kubeconfig::read_from(path)?,
                &kube::config::KubeConfigOptions::default(),
            )
            .await?
        } else {
            kube::Config::infer().await?
        };

        // Apply context override if provided
        if let Some(ctx) = context_override.or(context.as_deref()) {
            let kubeconfig = if let Some(path) = path {
                kube::config::Kubeconfig::read_from(path)?
            } else {
                kube::config::Kubeconfig::read()?
            };

            config = kube::Config::from_custom_kubeconfig(
                kubeconfig,
                &kube::config::KubeConfigOptions {
                    context: Some(ctx.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        }

        let client = Client::try_from(config)?;
        let discovery = Arc::new(Discovery::new(client.clone()).run().await?);

        Ok(Self { client, discovery })
    }

    /// Discover the API resource for a given GVK
    fn discover_api_resource(&self, gvk: &GroupVersionKind) -> Result<(ApiResource, ApiCapabilities)> {
        // Search through all groups for matching resource
        for group in self.discovery.groups() {
            for (ar, caps) in group.recommended_resources() {
                // Match by kind and version
                if ar.kind == gvk.kind && ar.version == gvk.version {
                    // For core resources (empty group), ar.group is also empty
                    // For other resources, check group matches
                    if (gvk.group.is_empty() && ar.group.is_empty()) || ar.group == gvk.group {
                        return Ok((ar, caps));
                    }
                }
            }
        }

        let group_version = if gvk.group.is_empty() {
            gvk.version.clone()
        } else {
            format!("{}/{}", gvk.group, gvk.version)
        };

        Err(NylError::Config(format!(
            "API resource not found for {}/{}",
            group_version, gvk.kind
        )))
    }
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

        // Check if resource exists
        let exists = self.get_resource(&gvk, namespace.as_deref(), &name).await?.is_some();

        // Setup patch parameters
        let mut patch_params = PatchParams::apply(field_manager).force();
        if dry_run {
            patch_params = patch_params.dry_run();
        }

        // Apply the resource using server-side apply
        let patch = Patch::Apply(resource);
        api.patch(&name, &patch_params, &patch).await?;

        // Determine outcome
        let base_outcome = if exists {
            ApplyOutcome::Updated {
                name: name.clone(),
                namespace: namespace.clone(),
            }
        } else {
            ApplyOutcome::Created {
                name: name.clone(),
                namespace: namespace.clone(),
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
}

impl MockKubeClient {
    /// Create a new mock client
    pub fn new() -> Self {
        Self {
            resources: Arc::new(Mutex::new(HashMap::new())),
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
        let exists = store.contains_key(&key);

        // Only store if not dry run
        if !dry_run {
            store.insert(key, resource.clone());
        }

        let base_outcome = if exists {
            ApplyOutcome::Updated {
                name: name.clone(),
                namespace: namespace.clone(),
            }
        } else {
            ApplyOutcome::Created {
                name: name.clone(),
                namespace: namespace.clone(),
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

        match outcome {
            ApplyOutcome::Created { name, namespace } => {
                assert_eq!(name, "test");
                assert_eq!(namespace, Some("default".to_string()));
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
            }
        });

        let resource: DynamicObject = serde_json::from_value(json_data).unwrap();

        // First apply
        client.apply_resource(&resource, "nyl", false).await.unwrap();

        // Second apply (update)
        let outcome = client.apply_resource(&resource, "nyl", false).await.unwrap();

        match outcome {
            ApplyOutcome::Updated { name, namespace } => {
                assert_eq!(name, "test");
                assert_eq!(namespace, Some("default".to_string()));
            }
            _ => panic!("Expected Updated outcome"),
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
}
