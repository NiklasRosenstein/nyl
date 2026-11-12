//! Helm `lookup` from the target's last publication.
//!
//! A chart that declares `spec.lookup` reads the objects of its Release as the
//! target last published them: the files the ownership index
//! (`<prefix>/_nyl/index.json`) lists beneath `<group output>/<release>/` at the
//! publication base commit, the same commit `fromPublication` reads. Every
//! command that renders the target builds its lookups here, so `render`, `diff`,
//! `apply`, `render-tree`, and `publish-tree` agree.
//!
//! - A publication that does not exist yet, or does not hold the Release yet,
//!   holds nothing: the lookups are absent and the chart generates.
//! - A publication that cannot be read, or whose files do not match its index,
//!   is an error, never absence, so a CA is never silently regenerated.
//! - The snapshot is read lazily, at the first lookup that needs it, so a target
//!   whose charts declare no `lookup` never reads its publication.
//!
//! The resolved base is reported back ([`PublicationLookup::resolved_base`]) so
//! `publish-tree` verifies that the branch did not move under what rendered
//! from it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use super::discovery::GitOpsInventory;
use super::inputs::{CachedGitBlobSource, GitBlobSource, PublicationBase, PublicationBaseOrigin, PublicationRead};
use super::reconcile::{parse_index, DEFAULT_INDEX_PATH};
use crate::helm::lookup::{
    snapshot_unavailable, KindInfo, LookupBackend, LookupConfig, ObjectRef, ObjectSet, ObjectStore, ReleaseScope,
    ScopeSource, StoreError,
};
use crate::kubernetes::ScopeResolver;
use crate::render::cache::RenderCache;
use crate::resources::{ApplicationGroup, Cluster, ClusterKubernetesCapabilities, DeploymentTarget, ResourceScope};
use crate::{NylError, Result};

/// The target's last publication, as the source of its Releases' lookups.
#[derive(Clone)]
pub struct PublicationLookup {
    inner: Arc<Inner>,
}

struct Inner {
    git: Mutex<CachedGitBlobSource>,
    url: String,
    branch: String,
    prefix: String,
    read: PublicationRead,
    cluster: Option<ClusterKubernetesCapabilities>,
    base: Mutex<Option<std::result::Result<PublicationBase, StoreError>>>,
    /// The documents beneath a Release's directory, read once per directory.
    documents: Mutex<HashMap<String, Loaded<Vec<Value>>>>,
    /// The objects of a Release, by its directory and the namespace its objects default to.
    releases: Mutex<HashMap<(String, String), Loaded<ObjectSet>>>,
}

/// What a lazily read part of the publication holds, or why it cannot be read.
type Loaded<T> = std::result::Result<Arc<T>, StoreError>;

impl PublicationLookup {
    /// The publication of `target`, read as `read` says.
    pub fn for_target(
        inventory: &GitOpsInventory,
        target: &DeploymentTarget,
        cluster: &Cluster,
        read: PublicationRead,
        cache: Option<&RenderCache>,
        base: Option<&PublicationBase>,
    ) -> Result<Self> {
        let (_, repository, _) = super::tree::resolve_git_publication(inventory, &target.spec.publication)?;
        let url = repository.publish_url.as_deref().unwrap_or(&repository.repo_url);
        let (branch, prefix) = (&target.spec.publication.revision, target.publication_path_prefix());
        let git = CachedGitBlobSource::new(None, &inventory.project_root, &inventory.project_config, cache.cloned());
        let lookup = Self::new(git, url, branch, prefix, read, cluster.spec.kubernetes.clone());
        // A base the caller already resolved for this publication is the one
        // everything reads, so the state `fromPublication` read is the state
        // the lookups read.
        if let Some(base) = base.filter(|base| base.is_for(url, branch, prefix)) {
            *lookup.inner.base.lock().expect("the base is not poisoned") = Some(Ok(base.clone()));
        }
        Ok(lookup)
    }

    pub fn new(
        git: CachedGitBlobSource,
        url: &str,
        branch: &str,
        prefix: &str,
        read: PublicationRead,
        cluster: Option<ClusterKubernetesCapabilities>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                git: Mutex::new(git),
                url: url.to_owned(),
                branch: branch.to_owned(),
                prefix: prefix.to_owned(),
                read,
                cluster,
                base: Mutex::new(None),
                documents: Mutex::new(HashMap::new()),
                releases: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Read the publication at `base`, when it is this publication's and no lookup
    /// resolved one yet. The state a render reads from the publication, through
    /// `fromPublication` or a lookup, is then read at one commit.
    pub fn adopt_base(&self, base: Option<&PublicationBase>) {
        let Some(base) = base.filter(|base| base.is_for(&self.inner.url, &self.inner.branch, &self.inner.prefix))
        else {
            return;
        };
        self.inner
            .base
            .lock()
            .expect("the base is not poisoned")
            .get_or_insert_with(|| Ok(base.clone()));
    }

    /// The publication base the lookups read, when any lookup needed it.
    pub fn resolved_base(&self) -> Option<PublicationBase> {
        self.inner
            .base
            .lock()
            .expect("the base is not poisoned")
            .as_ref()
            .and_then(|base| base.as_ref().ok().cloned())
    }
}

impl LookupBackend for PublicationLookup {
    fn describe(&self) -> String {
        format!(
            "the last publication of {}@{}",
            nyl_git::sanitize_url(&self.inner.url),
            self.inner.branch
        )
    }

    fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore> {
        Arc::new(ReleaseSnapshot {
            inner: self.inner.clone(),
            scope: scope.clone(),
        })
    }
}

/// The objects of one Release in the publication, read at first use.
struct ReleaseSnapshot {
    inner: Arc<Inner>,
    scope: ReleaseScope,
}

impl ReleaseSnapshot {
    fn objects(&self) -> std::result::Result<Arc<ObjectSet>, StoreError> {
        if self.scope.is_none() {
            return Ok(Arc::default());
        }
        let mut releases = self.inner.releases.lock().expect("the releases are not poisoned");
        releases
            .entry((self.scope.directory.clone(), self.scope.namespace.clone()))
            .or_insert_with(|| self.inner.load(&self.scope).map(Arc::new))
            .clone()
    }
}

impl ObjectStore for ReleaseSnapshot {
    fn kinds(&self) -> std::result::Result<Vec<KindInfo>, StoreError> {
        self.objects()?.kinds()
    }

    fn get(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        name: &str,
    ) -> std::result::Result<Option<Value>, StoreError> {
        self.objects()?.get(kind, namespace, name)
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> std::result::Result<Vec<Value>, StoreError> {
        self.objects()?.list(kind, namespace)
    }
}

impl Inner {
    fn base(&self) -> std::result::Result<PublicationBase, StoreError> {
        let mut base = self.base.lock().expect("the base is not poisoned");
        base.get_or_insert_with(|| {
            let git = self.git.lock().expect("the Git source is not poisoned");
            PublicationBase::resolve(&*git, &self.url, &self.branch, &self.prefix, self.read).map_err(|error| {
                snapshot_unavailable(format!(
                    "the publication that a chart's lookup reads cannot be resolved: {error}"
                ))
            })
        })
        .clone()
    }

    fn describe(&self) -> String {
        format!("{}@{}", nyl_git::sanitize_url(&self.url), self.branch)
    }

    /// The objects of the Release `scope` names, from the publication base.
    fn load(&self, scope: &ReleaseScope) -> std::result::Result<ObjectSet, StoreError> {
        let documents = self
            .documents
            .lock()
            .expect("the documents are not poisoned")
            .entry(scope.directory.clone())
            .or_insert_with(|| self.read_documents(&scope.directory).map(Arc::new))
            .clone()?;
        let resolver = ScopeResolver::new(self.cluster.as_ref(), documents.iter());
        ObjectSet::from_documents(
            documents.as_ref().clone(),
            &|group, kind| {
                resolver
                    .scope(group, kind)
                    .map(|scope| scope == ResourceScope::Namespaced)
            },
            &scope.namespace,
        )
        .map_err(|error| {
            snapshot_unavailable(format!(
                "the publication {} is corrupt beneath {}: {error}",
                self.describe(),
                scope.directory
            ))
        })
    }

    /// The documents of the files beneath `directory` in the publication base.
    fn read_documents(&self, directory: &str) -> std::result::Result<Vec<Value>, StoreError> {
        let base = self.base()?;
        let Some(commit) = &base.commit else {
            // A branch that was never fetched cannot be told apart from one that
            // does not exist, so only a read that bootstraps may call it empty.
            return if base.origin == PublicationBaseOrigin::Unavailable && self.read != PublicationRead::FreshOrCached {
                Err(snapshot_unavailable(format!(
                    "the publication {} is not in the local Git cache and was not refreshed, so what the Release {} \
                     published is unknown; render once with network access (without --offline) so the publication is \
                     cached, then use --offline",
                    self.describe(),
                    directory
                )))
            } else {
                Ok(Vec::new())
            };
        };
        let git = self.git.lock().expect("the Git source is not poisoned");
        let read = |path: &str| {
            git.read_publication_blob(&base.url, commit, &repository_path(&base.prefix, path))
                .map_err(|error| {
                    snapshot_unavailable(format!(
                        "cannot read {path} of the publication {} at {commit}: {error}",
                        self.describe()
                    ))
                })
        };
        let Some(index) = read(DEFAULT_INDEX_PATH)? else {
            // The branch holds no ownership index under the prefix: nothing is published there yet.
            return Ok(Vec::new());
        };
        let index = parse_index(
            &index,
            &format!(
                "{DEFAULT_INDEX_PATH} of the publication {} at {commit}",
                self.describe()
            ),
        )
        .map_err(|error| snapshot_unavailable(error.to_string()))?;
        let directory = format!("{directory}/");
        let mut documents = Vec::new();
        for (path, digest) in index
            .files
            .iter()
            .filter(|(path, _)| path.starts_with(&directory) && is_manifest_path(path))
        {
            let Some(bytes) = read(path)? else {
                return Err(snapshot_unavailable(format!(
                    "the publication {} at {commit} is corrupt: {path} is in its ownership index but not in the tree",
                    self.describe()
                )));
            };
            if &nyl_core::digest::sha256_hex(&bytes) != digest {
                return Err(snapshot_unavailable(format!(
                    "the publication {} at {commit} is corrupt: {path} does not match its digest in the ownership index",
                    self.describe()
                )));
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                snapshot_unavailable(format!(
                    "the publication {} holds {path}, which is not UTF-8",
                    self.describe()
                ))
            })?;
            documents.extend(serde_saphyr::from_multiple::<Value>(text).map_err(|error| {
                snapshot_unavailable(format!(
                    "the publication {} holds {path}, which is not valid YAML: {error}",
                    self.describe()
                ))
            })?);
        }
        Ok(documents)
    }
}

fn is_manifest_path(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|extension| extension.to_str()),
        Some("yaml" | "yml")
    )
}

fn repository_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

/// Where a target's Release is published, for the lookups of its charts: the
/// directory of the Release in the ApplicationGroup's output, and the namespace
/// its Application deploys unnamespaced objects into.
pub fn release_scope(group: &ApplicationGroup, release: &str, release_namespace: &str) -> ReleaseScope {
    let output = group.spec.output_path.as_deref().unwrap_or(&group.metadata.name);
    ReleaseScope {
        directory: format!("{output}/{release}"),
        namespace: group
            .spec
            .destination_namespace
            .clone()
            .unwrap_or_else(|| release_namespace.to_owned()),
    }
}

/// How a render of one ApplicationGroup answers lookups: where each of its
/// Releases is published.
pub fn group_scope(group: &ApplicationGroup) -> ScopeSource {
    let group = group.clone();
    ScopeSource::new(move |release| Ok(Some(release_scope(&group, &release.name, &release.namespace))))
}

/// A new error with the text, and the kind callers act on, of `error`: the code a message
/// begins with, or the one a hint names, survives being reported more than once.
fn same_error(error: &NylError) -> NylError {
    match error {
        NylError::Config(message) => NylError::Config(message.clone()),
        other => NylError::Other(other.to_string()),
    }
}

/// The lookups of a direct command (`render`, `diff`, `apply`) with a target.
pub struct DirectLookup {
    pub config: LookupConfig,
    pub scope: ScopeSource,
}

/// Build the lookups of a direct command for the Release file `file`.
///
/// The Release is published in the ApplicationGroup that renders the file, or
/// the one `--application-group` names, which is decided the first time a
/// chart's lookup needs it, never for a file that declares none. A render
/// outside every group (`--defaults-only`) has no published Release and finds
/// nothing. `base` is the publication base the command's `fromPublication`
/// inputs already read, which the lookups then read too: one commit, one fetch.
#[allow(clippy::too_many_arguments)]
pub fn direct_lookup(
    inventory: &GitOpsInventory,
    target: &DeploymentTarget,
    cluster: &Cluster,
    file: &Path,
    application_group: Option<&str>,
    defaults_only: bool,
    read: PublicationRead,
    cache: Option<&RenderCache>,
    refresh: BTreeSet<ObjectRef>,
    base: Option<&PublicationBase>,
) -> Result<DirectLookup> {
    let publication = PublicationLookup::for_target(inventory, target, cluster, read, cache, base)?;
    let config = LookupConfig::new(Arc::new(publication), refresh);
    if defaults_only {
        return Ok(DirectLookup {
            config,
            scope: ScopeSource::new(|_| Ok(None)),
        });
    }
    let (inventory, target) = (inventory.clone(), target.clone());
    let (file, application_group) = (PathBuf::from(file), application_group.map(ToOwned::to_owned));
    let group: OnceLock<std::result::Result<Option<ApplicationGroup>, NylError>> = OnceLock::new();
    let scope = ScopeSource::new(move |release| {
        let group = group
            .get_or_init(|| {
                super::direct::rendering_group(&inventory, &target, &file, application_group.as_deref())
                    .map(|selected| selected.map(|(group, _)| group))
            })
            .as_ref()
            .map_err(same_error)?;
        Ok(group
            .as_ref()
            .map(|group| release_scope(group, &release.name, &release.namespace)))
    });
    Ok(DirectLookup { config, scope })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::ApplicationGroup;

    fn group(output: Option<&str>, namespace: Option<&str>) -> ApplicationGroup {
        let mut spec = serde_json::json!({"projectRef": "workloads", "applicationNamespace": "argocd"});
        if let Some(output) = output {
            spec["outputPath"] = output.into();
        }
        if let Some(namespace) = namespace {
            spec["destinationNamespace"] = namespace.into();
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "k8s.gitops.nyl/v1",
            "kind": "ApplicationGroup",
            "metadata": {"name": "platform"},
            "spec": spec,
        }))
        .unwrap()
    }

    #[test]
    fn test_a_release_is_published_beneath_its_groups_output_and_destination() {
        let scope = release_scope(&group(None, None), "cilium", "kube-system");
        assert_eq!(
            (scope.directory.as_str(), scope.namespace.as_str()),
            ("platform/cilium", "kube-system")
        );
        let scope = release_scope(&group(Some("infra"), Some("system")), "cilium", "kube-system");
        assert_eq!(
            (scope.directory.as_str(), scope.namespace.as_str()),
            ("infra/cilium", "system")
        );
    }
}
