//! The catalog Application shared by many targets (`catalogApplication.shared`).
//!
//! Contract: [Catalog ownership](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//! A target with a shared catalog stays an ordinary target in its own prefix,
//! with today's catalog layout, index, and reconciliation, except that its
//! `_nyl/catalog` holds no catalog Application: one Application, rendered here
//! from the `shared` block and the ArgoCDInstance's catalog defaults, syncs
//! every target's catalog directory and its own manifest.
//!
//! The manifest lives under the reserved prefix `_nyl/shared/<name>/`, with
//! its own ownership index whose owner is `catalog:<name>`, so no target ever
//! writes outside its own prefix. [`RenderedTarget::shared_catalog`] is that
//! tree, ready for the publication functions of
//! [`super::publication`]; [`validate_shared_catalogs`] holds the rules that
//! span targets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::argocd::{build_directory_application, DirectoryApplicationInput};
use super::publication::{PublicationDestination, RenderedTarget};
use super::reconcile::{RenderIndex, RenderIndexOwner, RenderIndexPublication, DEFAULT_INDEX_PATH};
use super::tree::{
    normalize_branch_revision, resolve_git_publication, set_catalog_prune_option, EffectiveArgoCDInstance,
};
use super::GitOpsInventory;
use crate::resources::{DeploymentTarget, GitOpsResource, SharedCatalogApplication};
use crate::{NylError, Result};
use nyl_core::resources::InlineGitRepository;

/// The stable code of every shared-catalog rule that spans targets.
pub const SHARED_CATALOG_CONFLICT_CODE: &str = "NYL-KUBERNETES-SHARED-CATALOG-CONFLICT";

/// The reserved directory that holds the shared catalogs of a publication branch.
pub const SHARED_ROOT: &str = "_nyl/shared";

/// The file of a shared catalog's tree that holds its Application.
pub const SHARED_MANIFEST: &str = "catalog.yaml";

/// The publication prefix of the shared catalog `name`.
pub fn shared_prefix(name: &str) -> String {
    format!("{SHARED_ROOT}/{name}")
}

/// The `directory.include` pattern of a shared catalog Application: its own
/// manifest and every target's catalog directory, and nothing else. Argo CD
/// matches it against each file's path relative to the source path with gobwas
/// globs compiled without separators, so `*` also matches across `/`.
pub fn include_pattern(name: &str) -> String {
    format!("{{{}/*.yaml,*/_nyl/catalog/*}}", shared_prefix(name))
}

/// The shared catalog a compiled target declares: the Application Argo CD
/// runs, and what the index of its tree records. Compiled with the target, so
/// everything it was rendered from enters the render's input record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedCatalogManifest {
    pub name: String,
    /// The Application manifest, as Argo CD reads it.
    pub application: Value,
    /// The Argo CD control-plane Cluster, which the catalog's index records.
    pub cluster: String,
    /// The publication repository's identity in the index.
    pub repository: String,
    pub revision: String,
}

/// Render the shared catalog Application of `target`.
pub(super) fn manifest(
    shared: &SharedCatalogApplication,
    target: &DeploymentTarget,
    repository: &InlineGitRepository,
    argocd: &EffectiveArgoCDInstance,
) -> Result<SharedCatalogManifest> {
    let defaults = &argocd.resource.spec.catalog_application_defaults;
    let project = shared.project.clone().unwrap_or_else(|| defaults.project.clone());
    let sync_policy = shared
        .sync_policy
        .clone()
        .unwrap_or_else(|| defaults.sync_policy.clone());
    let deletion_policy = shared
        .application_deletion_policy
        .unwrap_or(defaults.application_deletion_policy);
    let self_prune_policy = shared.self_prune_policy.unwrap_or(defaults.self_prune_policy);
    let mut labels = defaults.labels.clone();
    labels.extend(shared.labels.clone());
    let mut annotations = defaults.annotations.clone();
    annotations.extend(shared.annotations.clone());
    set_catalog_prune_option(&mut annotations, self_prune_policy);
    let mut application = build_directory_application(&DirectoryApplicationInput {
        name: shared.name.clone(),
        application_namespace: argocd.resource.spec.namespace.clone(),
        project,
        repo_url: repository.repo_url.clone(),
        revision: target.spec.publication.revision.clone(),
        // The builder takes a directory below the root; the shared catalog
        // sources the root itself, set below.
        rendered_path: shared_prefix(&shared.name),
        destination: argocd.cluster.spec.destination.clone(),
        destination_namespace: argocd.resource.spec.namespace.clone(),
        sync_policy: Some(sync_policy),
        deletion_policy,
        labels,
        annotations,
    })?;
    application["spec"]["source"]["path"] = Value::String(".".to_owned());
    application["spec"]["source"]["directory"]["include"] = Value::String(include_pattern(&shared.name));
    Ok(SharedCatalogManifest {
        name: shared.name.clone(),
        application,
        cluster: argocd.cluster.metadata.name.clone(),
        repository: repository.repo_url.clone(),
        revision: normalize_branch_revision(&target.spec.publication.revision).to_owned(),
    })
}

impl SharedCatalogManifest {
    /// The tree of this catalog on the publication branch of `rendered`, the
    /// target's own render: the manifest and an ownership index that names
    /// the owner `catalog:<name>`. The index records no source commit, so the
    /// tree is the same whichever commit rendered it, and a tree that differs
    /// differs in its manifest.
    pub(super) fn tree(&self, rendered: &RenderedTarget) -> Result<RenderedTarget> {
        let mut text = serde_saphyr::to_string(&self.application)
            .map_err(|error| NylError::config(format!("cannot render shared catalog {}: {error}", self.name)))?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let manifest = text.into_bytes();
        let prefix = shared_prefix(&self.name);
        let digest = nyl_core::digest::sha256_hex(&manifest);
        let mut index = RenderIndex::new(
            format!("catalog:{}", self.name),
            self.cluster.clone(),
            RenderIndexPublication {
                repository: self.repository.clone(),
                revision: self.revision.clone(),
                path_prefix: prefix.clone(),
            },
            None,
            false,
            BTreeMap::new(),
        )
        .owned_by(RenderIndexOwner::catalog(&self.name));
        index.version = super::reconcile::COMPILED_RENDER_INDEX_VERSION;
        index.compilation = Some(serde_json::to_value(super::tree::partial::PublicationSnapshot {
            version: super::tree::partial::COMPILATION_METADATA_VERSION,
            control_digest: digest.clone(),
            groups: BTreeMap::new(),
            releases: BTreeMap::new(),
            bindings: BTreeMap::new(),
        })?);
        index.files = BTreeMap::from([(SHARED_MANIFEST.to_owned(), digest)]);
        let mut index_bytes = serde_json::to_vec_pretty(&index)?;
        index_bytes.push(b'\n');
        let files = BTreeMap::from([
            (PathBuf::from(SHARED_MANIFEST), manifest),
            (PathBuf::from(DEFAULT_INDEX_PATH), index_bytes),
        ]);
        Ok(RenderedTarget {
            destination: PublicationDestination {
                path_prefix: prefix,
                ..rendered.destination.clone()
            },
            report: super::tree::partial::PublicationReport::default(),
            files,
            input_record: BTreeMap::new(),
            cacheable: true,
            reconcile: super::reconcile::ReconcileOptions::default(),
            publication_base: None,
            cluster_kubernetes: None,
            argocd: rendered.argocd.clone(),
            workload: rendered.workload.clone(),
            shared_catalog: None,
        })
    }
}

/// The rules that span targets, checked wherever targets are validated
/// together. Each failure carries [`SHARED_CATALOG_CONFLICT_CODE`] and names
/// the targets and the fix.
///
/// - Targets that name one shared catalog publish to one repository and
///   revision, and describe it identically, because one manifest is rendered
///   for all of them.
/// - A repository revision that carries a shared catalog holds nothing else
///   than targets sharing it: its include pattern would sync the catalog
///   directory of a target that does not share it, or shares another one, and
///   give that target's Applications two owners.
/// - No target's prefix is, contains, or lies beneath the reserved prefix
///   `_nyl/shared`.
pub fn validate_shared_catalogs(inventory: &GitOpsInventory) -> Result<()> {
    struct Member<'a> {
        target: &'a DeploymentTarget,
        repository: (String, String),
        shared: Option<&'a SharedCatalogApplication>,
    }
    let mut members = Vec::new();
    for discovered in inventory.resources.values() {
        let Some(GitOpsResource::DeploymentTarget(target)) = discovered.resource.as_ref() else {
            continue;
        };
        let (_, repository, _) = resolve_git_publication(inventory, &target.spec.publication)?;
        members.push(Member {
            target,
            repository: (
                crate::git::normalize_git_url_for_equality(&repository.repo_url),
                normalize_branch_revision(&target.spec.publication.revision).to_owned(),
            ),
            shared: target.spec.catalog_application.shared.as_ref(),
        });
    }
    let conflict =
        |message: String| -> NylError { NylError::config(format!("{SHARED_CATALOG_CONFLICT_CODE}: {message}")) };
    for member in &members {
        let prefix = member.target.publication_path_prefix();
        if let Some(shared) = member.shared {
            if member.target.publication_path_prefix().is_empty() {
                return Err(conflict(format!(
                    "DeploymentTarget {:?} shares catalog {:?} and publishes to the repository root; give it a pathPrefix beside the other targets",
                    member.target.metadata.name, shared.name
                )));
            }
        }
        let reserved = Path::new(SHARED_ROOT);
        let own = Path::new(prefix);
        if own.starts_with(reserved) || reserved.starts_with(own) && !prefix.is_empty() {
            return Err(conflict(format!(
                "DeploymentTarget {:?} publishes to {prefix:?}, which overlaps the reserved prefix {SHARED_ROOT}/ of shared catalogs; choose another pathPrefix",
                member.target.metadata.name
            )));
        }
    }
    let mut catalogs: BTreeMap<(&(String, String), &str), &Member<'_>> = BTreeMap::new();
    let mut carried: BTreeMap<&(String, String), Vec<&Member<'_>>> = BTreeMap::new();
    for member in &members {
        if let Some(shared) = member.shared {
            carried.entry(&member.repository).or_default().push(member);
            if let Some(first) = catalogs.get(&(&member.repository, shared.name.as_str())) {
                if first.shared != member.shared {
                    return Err(conflict(format!(
                        "DeploymentTargets {:?} and {:?} share catalog {:?} but describe it differently; one manifest is rendered for every target that shares it, so declare the same shared block, or give them different catalogs",
                        first.target.metadata.name, member.target.metadata.name, shared.name
                    )));
                }
            } else {
                catalogs.insert((&member.repository, shared.name.as_str()), member);
            }
        }
    }
    // The same catalog name on another repository revision is another catalog
    // in Git but the same Application in Argo CD, so the name must be unique.
    let mut by_name: BTreeMap<&str, &Member<'_>> = BTreeMap::new();
    for ((repository, name), member) in &catalogs {
        match by_name.get(name) {
            Some(first) if &&first.repository != repository => {
                return Err(conflict(format!(
                    "DeploymentTargets {:?} and {:?} name the shared catalog {name:?} on different repositories or revisions; targets sharing a catalog publish to one revision, and a catalog name is one Argo CD Application",
                    first.target.metadata.name, member.target.metadata.name
                )));
            }
            Some(_) => {}
            None => {
                by_name.insert(name, member);
            }
        }
    }
    for (repository, carrying) in &carried {
        let names = carrying
            .iter()
            .filter_map(|member| member.shared.map(|shared| shared.name.as_str()))
            .collect::<std::collections::BTreeSet<_>>();
        for member in members.iter().filter(|member| &&member.repository == repository) {
            let own = member.shared.map(|shared| shared.name.as_str());
            if names.len() > 1 || own.is_none() {
                let (name, _) = carrying
                    .first()
                    .map(|first| (first.shared.expect("a carrier shares").name.as_str(), first))
                    .expect("a revision carries a catalog");
                let message = if own.is_none() {
                    format!(
                        "DeploymentTarget {:?} publishes to {}@{}, a revision that carries the shared catalog {name:?}, but does not share it; the catalog's include pattern would sync its catalog directory too and give its Applications two owners. Share the catalog, or publish to another branch",
                        member.target.metadata.name, member.target.spec.publication.revision, repository.0
                    )
                } else {
                    format!(
                        "the revision {}@{} carries several shared catalogs ({}); each syncs every target's catalog directory, which would give the Applications two owners. Share one catalog per branch",
                        repository.0,
                        repository.1,
                        names.iter().copied().collect::<Vec<_>>().join(", ")
                    )
                };
                return Err(conflict(message));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitops::{discover_gitops_inventory, validate_gitops_inventory};

    /// The include pattern is the contract's, so an Argo CD that matches it
    /// syncs the shared manifest and every target's catalog directory.
    #[test]
    fn test_include_pattern_names_the_manifest_and_every_catalog_directory() {
        assert_eq!(
            include_pattern("previews"),
            "{_nyl/shared/previews/*.yaml,*/_nyl/catalog/*}"
        );
        assert_eq!(shared_prefix("previews"), "_nyl/shared/previews");
    }

    const BASE: &str = r"apiVersion: gitops.nyl/v1
kind: GitRepository
metadata: {name: platform}
spec: {repoURL: 'https://example.invalid/platform.git'}
---
apiVersion: k8s.nyl/v1
kind: Cluster
metadata: {name: dev}
spec:
  destination: {server: https://kubernetes.default.svc}
  kubernetes: {kubeVersion: 1.31.4, apiVersions: [v1]}
---
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata: {name: web, labels: {app: web}}
spec:
  applicationNamespace: argocd
  source: {path: ../applications/web}
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
  projectTemplate: {name: '{{ target.metadata.name }}-web', destinations: [{server: '*', namespace: web}]}
";

    fn target(name: &str, revision: &str, prefix: &str, catalog: &str) -> String {
        format!(
            r"---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {{name: {name}}}
spec:
  clusterRef: {{name: dev}}
  applicationGroupSelector: {{matchLabels: {{app: web}}}}
  catalogApplication: {catalog}
  publication: {{repositoryRef: {{name: platform}}, revision: {revision}, pathPrefix: '{prefix}'}}
"
        )
    }

    /// Validate a project holding `targets`, with the error text if any.
    fn validate(targets: &[String]) -> std::result::Result<(), String> {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        std::fs::write(root.join("nyl.toml"), "[project]\n").unwrap();
        std::fs::write(root.join("platform.yaml"), format!("{BASE}{}", targets.concat())).unwrap();
        git2::Repository::init(root).unwrap();
        let inventory = discover_gitops_inventory(root, None, crate::config::test_catalogue())
            .map_err(|error| error.to_string())?;
        validate_gitops_inventory(&inventory).map_err(|error| error.to_string())
    }

    const PREVIEWS: &str = "{shared: {name: previews}}";

    /// Catches the rules that span targets going unchecked: each failure
    /// carries the stable code and names the targets, and valid layouts pass.
    #[test]
    fn test_validation_checks_the_rules_across_targets_sharing_a_catalog() {
        // Two targets sharing a catalog on one revision are valid.
        let shared = [
            target("pr-1", "previews", "pr-1", PREVIEWS),
            target("pr-2", "previews", "pr-2", PREVIEWS),
        ];
        validate(&shared).unwrap();

        // A target on the revision that does not share it would be synced twice.
        let alien = [shared.concat(), target("dev", "previews", "dev", "{enabled: true}")];
        let error = validate(&alien).unwrap_err();
        assert!(error.contains(SHARED_CATALOG_CONFLICT_CODE), "{error}");
        assert!(error.contains("\"dev\"") && error.contains("two owners"), "{error}");

        // The same revision may not carry two catalogs.
        let two = [
            target("pr-1", "previews", "pr-1", PREVIEWS),
            target("pr-2", "previews", "pr-2", "{shared: {name: other}}"),
        ];
        let error = validate(&two).unwrap_err();
        assert!(
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("several shared catalogs"),
            "{error}"
        );

        // Targets that name one catalog describe it the same way.
        let different = [
            target("pr-1", "previews", "pr-1", PREVIEWS),
            target(
                "pr-2",
                "previews",
                "pr-2",
                "{shared: {name: previews, syncPolicy: {automated: {prune: true}}}}",
            ),
        ];
        let error = validate(&different).unwrap_err();
        assert!(
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("describe it differently"),
            "{error}"
        );

        // Another branch is another catalog in Git but the same Application in Argo CD.
        let branches = [
            target("pr-1", "previews", "pr-1", PREVIEWS),
            target("pr-2", "staging", "pr-2", PREVIEWS),
        ];
        let error = validate(&branches).unwrap_err();
        assert!(
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("different repositories or revisions"),
            "{error}"
        );

        // The reserved prefix belongs to the catalogs.
        let reserved = [target("pr-1", "previews", "_nyl/shared/pr-1", PREVIEWS)];
        let error = validate(&reserved).unwrap_err();
        assert!(
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("reserved prefix"),
            "{error}"
        );

        // A per-target catalog Application may not take the shared one's name.
        let named = [
            target("pr-1", "previews", "pr-1", PREVIEWS),
            target("dev", "deploy", "dev", "{name: previews}"),
        ];
        let error = validate(&named).unwrap_err();
        assert!(
            error.contains("shared catalog") && error.contains("previews"),
            "{error}"
        );

        // A shared catalog replaces the target's own settings, so declaring both is a mistake.
        let both = [target(
            "pr-1",
            "previews",
            "pr-1",
            "{name: mine, shared: {name: previews}}",
        )];
        let error = validate(&both).unwrap_err();
        assert!(error.contains("belong in the shared block"), "{error}");
    }
}
