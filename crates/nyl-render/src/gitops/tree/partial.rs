//! Scoped publication over a pinned, ownership-verified destination snapshot.
//!
//! Contract: `design/partial-publication.md`. Selection controls replacement,
//! while the complete candidate remains the input of target-wide checks.
use super::*;
use crate::gitops::inputs::{GitBlobSource, PublicationBase};
use crate::gitops::reconcile::{parse_index, RenderIndex, DEFAULT_INDEX_PATH};

/// Explicit replacement authority inside one DeploymentTarget.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationSelection {
    pub application_groups: BTreeSet<String>,
    /// Group-qualified `<applicationGroup>/<release>` identities.
    pub releases: BTreeSet<String>,
}

impl PublicationSelection {
    /// Whether the invocation replaces the complete target.
    pub fn is_full(&self) -> bool {
        self.application_groups.is_empty() && self.releases.is_empty()
    }

    pub fn includes(&self, group: &str, release: &str) -> bool {
        self.is_full()
            || self.application_groups.contains(group)
            || self.releases.contains(&format!("{group}/{release}"))
    }

    pub(super) fn touches_group(&self, group: &str) -> bool {
        self.is_full()
            || self.application_groups.contains(group)
            || self
                .releases
                .iter()
                .any(|key| key.split_once('/').is_some_and(|(name, _)| name == group))
    }

    pub(super) fn validate(&self) -> Result<()> {
        for name in &self.application_groups {
            validate_path_segment("ApplicationGroup selector", name)?;
        }
        for key in &self.releases {
            ReleaseKey::parse(key)?;
        }
        Ok(())
    }
}

/// Changes and replacement authority of one scoped compilation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationReport {
    pub selection: PublicationSelection,
    pub baseline_commit: Option<String>,
    pub rendered_releases: BTreeSet<String>,
    pub retained_releases: BTreeSet<String>,
    pub removed_releases: BTreeSet<String>,
    pub changed_shared_files: BTreeSet<PathBuf>,
}

/// Versioned metadata carried in version 3 ownership indexes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationSnapshot {
    pub version: u32,
    /// Digest of target-wide configuration, excluding per-Release bindings.
    pub control_digest: String,
    pub groups: BTreeMap<String, String>,
    pub releases: BTreeMap<String, PublishedRelease>,
    pub bindings: BTreeMap<String, Vec<crate::resources::release_inputs::PublicationInputSource>>,
}

/// One Release's effective policy, ownership, and original provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishedRelease {
    workload: PendingWorkload,
    pub files: BTreeSet<PathBuf>,
    origins: Vec<(crate::kubernetes::ResourceKey, crate::render::Provenance)>,
    release_origin: Option<crate::render::Provenance>,
    pub source_commit: Option<String>,
    pub dirty: bool,
    pub inputs: BTreeMap<String, String>,
}

pub(super) struct Baseline {
    pub index: RenderIndex,
    pub snapshot: PublicationSnapshot,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

fn full_publish(detail: impl std::fmt::Display) -> NylError {
    NylError::config(format!(
        "Cannot partially publish: {detail}. Publish the full target first"
    ))
}

fn digest(value: &impl Serialize) -> Result<String> {
    Ok(nyl_core::digest::sha256_hex(&serde_json::to_vec(value)?))
}

pub(super) fn control_digest(
    inventory: &GitOpsInventory,
    target: &DeploymentTarget,
    cluster: &Cluster,
    argocd: &EffectiveArgoCDInstance,
    repository: &InlineGitRepository,
) -> Result<String> {
    // ProjectConfig expands declared paths beneath the config file's directory.
    // Publish the policy's identity independently of the checkout location (or
    // a lexical alias such as macOS's /var and /private/var).
    let mut config = inventory.project_config.config.clone();
    if let Some(root) = inventory.project_config.file.as_deref().and_then(Path::parent) {
        let portable = |path: &Path| {
            PathBuf::from(
                path.strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        };
        config.project.components_search_paths = config
            .project
            .components_search_paths
            .iter()
            .map(|path| portable(path))
            .collect();
        config.project.helm_chart_search_paths = config
            .project
            .helm_chart_search_paths
            .iter()
            .map(|path| portable(path))
            .collect();
        config.project.gitops_scaffold_path = portable(&config.project.gitops_scaffold_path);
        if let Some(vendor) = &mut config.vendor {
            vendor.path = portable(&vendor.path);
        }
    }
    let mut target = target.clone();
    target.spec.release_inputs.clear();
    let mut cluster = cluster.clone();
    cluster.spec.live = None;
    let mut control_cluster = argocd.cluster.clone();
    control_cluster.spec.live = None;
    digest(&(config, target, cluster, &argocd.resource, control_cluster, repository))
}

pub(super) fn group_digest(group: &ApplicationGroup, project: &EffectiveProject) -> Result<String> {
    digest(&(group, project.cache_input()))
}

pub(super) fn load_baseline(
    git: &dyn GitBlobSource,
    base: &PublicationBase,
    target: &DeploymentTarget,
    cluster: &Cluster,
    repository: &str,
) -> Result<Baseline> {
    let commit = base
        .commit
        .as_deref()
        .ok_or_else(|| full_publish("the destination branch is missing"))?;
    let read = |path: &str| -> Result<Vec<u8>> {
        crate::resources::validate_relative_path("published path", path, false, false)?;
        let path = if base.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", base.prefix)
        };
        git.read_publication_blob(&base.url, commit, &path)?
            .ok_or_else(|| full_publish(format!("indexed file {path:?} is missing")))
    };
    let index = parse_index(&read(DEFAULT_INDEX_PATH)?, DEFAULT_INDEX_PATH)?;
    if index.target != target.metadata.name
        || index.cluster != cluster.metadata.name
        || index.publication.repository != repository
        || index.publication.revision != target.spec.publication.revision
        || index.publication.path_prefix != target.publication_path_prefix()
    {
        return Err(full_publish("destination ownership does not match the target"));
    }
    let snapshot: PublicationSnapshot = serde_json::from_value(
        index
            .compilation
            .clone()
            .ok_or_else(|| full_publish("the destination has a legacy ownership index"))?,
    )
    .map_err(|error| full_publish(format!("invalid compilation metadata: {error}")))?;
    if snapshot.version != 1 {
        return Err(full_publish("unsupported compilation metadata version"));
    }
    let mut files = BTreeMap::new();
    for (path, expected) in &index.files {
        let bytes = read(path)?;
        if nyl_core::digest::sha256_hex(&bytes) != *expected {
            return Err(full_publish(format!("indexed file {path:?} was modified outside Nyl")));
        }
        files.insert(PathBuf::from(path), bytes);
    }
    let mut claimed = BTreeSet::new();
    for (key, release) in &snapshot.releases {
        let workload = &release.workload;
        crate::resources::relative_path_to_posix("published Release directory", &workload.release_directory)?;
        validate_path_segment("published Application name", &workload.application_name)?;
        if *key != format!("{}/{}", workload.group.metadata.name, workload.release.metadata.name) {
            return Err(full_publish("Release metadata has an inconsistent identity"));
        }
        for path in &release.files {
            if !path.starts_with(&workload.release_directory) || !files.contains_key(path) || !claimed.insert(path) {
                return Err(full_publish("Release metadata has invalid file ownership"));
            }
        }
    }
    Ok(Baseline { index, snapshot, files })
}

impl Baseline {
    pub(super) fn retained_workloads(&self, selection: &PublicationSelection) -> Result<Vec<PendingWorkload>> {
        self.snapshot
            .releases
            .values()
            .filter(|release| {
                !selection.includes(
                    &release.workload.group.metadata.name,
                    &release.workload.release.metadata.name,
                )
            })
            .map(|release| {
                let mut workload = release.workload.clone();
                for path in &release.files {
                    workload.manifests.extend(
                        serde_saphyr::from_multiple::<Value>(
                            std::str::from_utf8(&self.files[path]).map_err(full_publish)?,
                        )
                        .map_err(NylError::Yaml)?,
                    );
                }
                workload.manifest_provenance = release.origins.iter().cloned().collect();
                workload.release_provenance.clone_from(&release.release_origin);
                Ok(workload)
            })
            .collect()
    }

    /// Retaining a workload cannot change any of its workload or Application bytes.
    pub(super) fn verify_retention(
        &self,
        selection: &PublicationSelection,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<()> {
        for (key, release) in &self.snapshot.releases {
            if selection.includes(
                &release.workload.group.metadata.name,
                &release.workload.release.metadata.name,
            ) {
                continue;
            }
            let actual = release_paths(files, &release.workload.release_directory);
            if actual != release.files
                || actual
                    .iter()
                    .any(|path| !same_documents(&files[path], &self.files[path]))
            {
                return Err(NylError::config(format!(
                    "Partial publication would modify retained Release {key}; select it as well"
                )));
            }
            for path in &release.files {
                files.insert(path.clone(), self.files[path].clone());
            }
            let application = serde_json::json!({
                "apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
                "metadata": {"name": release.workload.application_name, "namespace": release.workload.group.spec.application_namespace}
            });
            let key = crate::kubernetes::ResourceKey::from_json_value(&application)?;
            let path = PathBuf::from(CATALOG_DIRECTORY).join(crate::gitops::resource_path(&key)?);
            if !self.files.contains_key(&path) || files.get(&path) != self.files.get(&path) {
                return Err(NylError::config(format!(
                    "Partial publication would modify retained Application of {key}; select its Release as well"
                )));
            }
        }
        Ok(())
    }
}

pub(super) fn snapshot_release(workload: &PendingWorkload, files: &BTreeMap<PathBuf, Vec<u8>>) -> PublishedRelease {
    let mut origins = workload
        .manifest_provenance
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    origins.sort_by_cached_key(|entry| serde_json::to_string(&entry.0).expect("ResourceKey serializes"));
    let mut metadata = workload.clone();
    metadata.manifests.clear();
    metadata.manifest_provenance.clear();
    metadata.release_provenance = None;
    // Retention needs namespace/Application policy, never input values or defaults.
    metadata.release.spec.inputs.clear();
    metadata.release.spec.include.clear();
    PublishedRelease {
        workload: metadata,
        origins,
        release_origin: workload.release_provenance.clone(),
        files: release_paths(files, &workload.release_directory),
        source_commit: None,
        dirty: false,
        inputs: workload
            .input_paths
            .iter()
            .map(|path| (path.to_string_lossy().replace('\\', "/"), String::new()))
            .collect(),
    }
}

/// Stamp only newly rendered Releases; retained entries keep their evidence.
pub fn stamp_snapshot(
    snapshot: &PublicationSnapshot,
    source_commit: Option<&str>,
    dirty: bool,
    inputs: &BTreeMap<String, String>,
) -> Result<Value> {
    let mut snapshot = snapshot.clone();
    for (key, release) in &mut snapshot.releases {
        if release.source_commit.is_some() {
            continue;
        }
        release.source_commit = source_commit.map(ToOwned::to_owned);
        release.dirty = dirty;
        release.inputs.retain(|path, digest| {
            if let Some(value) = inputs.get(path) {
                digest.clone_from(value);
                true
            } else {
                false
            }
        });
        let prefix = format!("{}{}{}", crate::gitops::inputs::INDEX_INPUT_PREFIX, key, "/");
        release.inputs.extend(
            inputs
                .range(prefix.clone()..)
                .take_while(|(name, _)| name.starts_with(&prefix))
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        for source in snapshot.bindings.get(key).into_iter().flatten() {
            for prefix in [
                crate::gitops::inputs::INDEX_PUBLICATION_PREFIX,
                crate::gitops::inputs::INDEX_CARRIED_PREFIX,
            ] {
                let name = format!("{prefix}{}", source.path);
                if let Some(digest) = inputs.get(&name) {
                    release.inputs.insert(name, digest.clone());
                }
            }
        }
    }
    Ok(serde_json::to_value(snapshot)?)
}

pub(super) fn report(
    selection: &PublicationSelection,
    baseline: Option<&Baseline>,
    base: Option<&PublicationBase>,
    snapshot: &PublicationSnapshot,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> PublicationReport {
    let mut report = PublicationReport {
        selection: selection.clone(),
        baseline_commit: base.and_then(|base| base.commit.clone()),
        ..PublicationReport::default()
    };
    for (key, release) in &snapshot.releases {
        if selection.includes(
            &release.workload.group.metadata.name,
            &release.workload.release.metadata.name,
        ) {
            report.rendered_releases.insert(key.clone());
        } else {
            report.retained_releases.insert(key.clone());
        }
    }
    if let Some(base) = baseline {
        report.removed_releases = base
            .snapshot
            .releases
            .keys()
            .filter(|key| !snapshot.releases.contains_key(*key))
            .cloned()
            .collect();
        report.changed_shared_files = base
            .files
            .keys()
            .chain(files.keys())
            .filter(|path| {
                (path.starts_with(CATALOG_DIRECTORY) || path.starts_with("_nyl/namespaces"))
                    && base.files.get(*path) != files.get(*path)
            })
            .cloned()
            .collect();
    }
    report
}

fn same_documents(left: &[u8], right: &[u8]) -> bool {
    let parse = |bytes: &[u8]| {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| serde_saphyr::from_multiple::<Value>(text).ok())
    };
    match (parse(left), parse(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn release_paths(files: &BTreeMap<PathBuf, Vec<u8>>, directory: &Path) -> BTreeSet<PathBuf> {
    files
        .range(directory.to_path_buf()..)
        .take_while(|(path, _)| path.starts_with(directory))
        .map(|(path, _)| path.clone())
        .collect()
}
