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

/// How to decommission or rename a shared catalog, which an error that finds a second one names.
pub const DECOMMISSION_FIX: &str = "To rename or decommission a shared catalog, first delete its Argo CD Application with its resources kept (non-cascading), \
     then remove `_nyl/shared/<old>/` from the branch in one commit, and only then publish the new one";

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
            delivery: rendered.delivery.clone(),
            workload: rendered.workload.clone(),
            shared_catalog: None,
        })
    }
}

/// [`validate_shared_catalogs`] over `inventory` as `nyl validate` sees it:
/// with a sample instance of every EnvironmentTemplate and the inline targets
/// of every environment's publication units, none of which a plain inventory
/// holds. State-free, so the tree commands (`render-tree`, `publish-tree`,
/// `diff-tree`) that compile a target from a plain inventory enforce the same
/// rules, such as that a revision carrying a shared catalog holds nothing else.
pub fn validate_with_samples(inventory: &GitOpsInventory) -> Result<()> {
    // Rendering every environment's units is only worth it, and only a new way
    // to fail, for a project that can declare a shared catalog somewhere the
    // plain inventory does not show.
    if !may_declare_shared_catalog(inventory) {
        return validate_shared_catalogs(inventory);
    }
    let samples = super::instances::templates(inventory)
        .into_iter()
        .flat_map(super::instances::sample_instances)
        .collect();
    let extended = super::units::with_inline_targets(&inventory.clone().with_instances(samples))?;
    validate_shared_catalogs(&extended)
}

/// Whether a shared catalog may be declared where only expansion shows it.
/// Structural and closed: false only when the inventory provably has none of
/// these.
///
/// - a parsed DeploymentTarget declares `catalogApplication.shared`;
/// - a DeploymentTarget could not be parsed statically, because its spec is
///   deferred;
/// - the project has an EnvironmentTemplate, whose instances render units;
/// - a publication unit declares its target inline, or could do so: its
///   document is not statically parseable (templated), or its `target` is
///   not a plain name.
fn may_declare_shared_catalog(inventory: &GitOpsInventory) -> bool {
    let targets = inventory
        .resources
        .values()
        .filter(|discovered| discovered.identity.kind == crate::resources::GitOpsResourceKind::DeploymentTarget);
    for discovered in targets {
        match discovered.resource.as_ref() {
            Some(GitOpsResource::DeploymentTarget(target)) if target.spec.catalog_application.shared.is_none() => {}
            _ => return true,
        }
    }
    if !super::instances::templates(inventory).is_empty() {
        return true;
    }
    let Ok(units) = super::units::discover_units(inventory) else {
        return true;
    };
    units
        .values()
        .any(super::units::DiscoveredUnit::may_declare_inline_target)
}

/// A target that takes part in the checks across targets.
struct Member<'a> {
    target: &'a DeploymentTarget,
    /// The publication repository and branch, as the checks compare them.
    repository: (String, String),
    shared: Option<&'a SharedCatalogApplication>,
    /// The Application the target renders for `shared`.
    manifest: Option<SharedCatalogManifest>,
}

fn conflict(message: String) -> NylError {
    NylError::config(format!("{SHARED_CATALOG_CONFLICT_CODE}: {message}"))
}

/// The rules that span targets, checked wherever targets are validated
/// together. Each failure carries [`SHARED_CATALOG_CONFLICT_CODE`] and names
/// the targets and the fix.
///
/// - No target's prefix is, contains, or lies beneath the reserved prefix
///   `_nyl/shared`.
/// - Targets that name one shared catalog publish to one repository and
///   revision, and describe it identically, because one manifest is rendered
///   for all of them.
/// - A repository revision that carries a shared catalog holds nothing else
///   than targets sharing it: its include pattern would sync the catalog
///   directory of a target that does not share it, or shares another one, and
///   give that target's Applications two owners.
pub fn validate_shared_catalogs(inventory: &GitOpsInventory) -> Result<()> {
    let mut members = Vec::new();
    for discovered in inventory.resources.values() {
        let Some(GitOpsResource::DeploymentTarget(target)) = discovered.resource.as_ref() else {
            continue;
        };
        // A direct target's tree has no catalog directory for a shared
        // catalog's include pattern to sync, so it neither shares one nor
        // conflicts with one.
        if target.is_direct() {
            continue;
        }
        let (_, repository, _) = resolve_git_publication(inventory, &target.spec.publication)?;
        members.push(Member {
            target,
            repository: (
                crate::git::normalize_git_url_for_equality(&repository.repo_url),
                normalize_branch_revision(&target.spec.publication.revision).to_owned(),
            ),
            shared: target.spec.catalog_application.shared.as_deref(),
            manifest: super::tree::shared_manifest_of(inventory, target)?,
        });
    }
    check_reserved_prefix(&members)?;
    check_catalog_descriptions(&members)?;
    check_revisions(&members)
}

/// No target publishes into the reserved prefix of shared catalogs, and a
/// target that shares one has a prefix beside the others.
fn check_reserved_prefix(members: &[Member<'_>]) -> Result<()> {
    let reserved = Path::new(SHARED_ROOT);
    for member in members {
        let prefix = member.target.publication_path_prefix();
        if let Some(shared) = member.shared.filter(|_| prefix.is_empty()) {
            return Err(conflict(format!(
                "DeploymentTarget {:?} shares catalog {:?} and publishes to the repository root; give it a pathPrefix beside the other targets",
                member.target.metadata.name, shared.name
            )));
        }
        let own = Path::new(prefix);
        if own.starts_with(reserved) || (!prefix.is_empty() && reserved.starts_with(own)) {
            return Err(conflict(format!(
                "DeploymentTarget {:?} publishes to {prefix:?}, which overlaps the reserved prefix {SHARED_ROOT}/ of shared catalogs; choose another pathPrefix",
                member.target.metadata.name
            )));
        }
    }
    Ok(())
}

/// Targets that name one catalog describe it identically, on one revision.
fn check_catalog_descriptions(members: &[Member<'_>]) -> Result<()> {
    let mut first: BTreeMap<&str, &Member<'_>> = BTreeMap::new();
    for member in members {
        let Some(shared) = member.shared else { continue };
        let Some(first) = first.get(shared.name.as_str()) else {
            first.insert(shared.name.as_str(), member);
            continue;
        };
        if first.repository != member.repository {
            // Another branch is another catalog in Git but the same Application in Argo CD.
            return Err(conflict(format!(
                "DeploymentTargets {:?} and {:?} name the shared catalog {:?} on different repositories or revisions; targets sharing a catalog publish to one revision, and a catalog name is one Argo CD Application",
                first.target.metadata.name, member.target.metadata.name, shared.name
            )));
        }
        // The rendered manifest covers what the declared block does not: the
        // ArgoCDInstance (namespace, destination, catalog defaults) and the repository.
        if first.manifest != member.manifest {
            return Err(conflict(format!(
                "DeploymentTargets {:?} and {:?} share catalog {:?} but render different manifests (another shared block, ArgoCDInstance, or catalog defaults); one manifest is rendered for every target that shares it, so declare the same shared block, or give them different catalogs",
                first.target.metadata.name, member.target.metadata.name, shared.name
            )));
        }
    }
    Ok(())
}

/// A revision that carries a shared catalog carries one, and only targets that share it.
fn check_revisions(members: &[Member<'_>]) -> Result<()> {
    let mut carried: BTreeMap<&(String, String), std::collections::BTreeSet<&str>> = BTreeMap::new();
    for member in members {
        if let Some(shared) = member.shared {
            carried.entry(&member.repository).or_default().insert(&shared.name);
        }
    }
    for member in members {
        let Some(names) = carried.get(&member.repository) else {
            continue;
        };
        let listed = names.iter().copied().collect::<Vec<_>>().join(", ");
        if member.shared.is_none() {
            return Err(conflict(format!(
                "DeploymentTarget {:?} publishes to {}@{}, a revision that carries the shared catalog {listed}, but does not share it; the catalog's include pattern would sync its catalog directory too and give its Applications two owners. Share the catalog, or publish to another branch",
                member.target.metadata.name, member.repository.0, member.target.spec.publication.revision
            )));
        }
        if names.len() > 1 {
            return Err(conflict(format!(
                "the revision {}@{} carries several shared catalogs ({listed}); each syncs every target's catalog directory, which would give the Applications two owners. Share one catalog per branch",
                member.repository.0, member.repository.1
            )));
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
  reconciler: {{argocd: {{}}}}
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
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("render different manifests"),
            "{error}"
        );

        // The same block over another ArgoCDInstance renders another Application.
        let instances = "---\napiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata: {name: one}\nspec: {clusterRef: {name: dev}, namespace: argocd}\n---\napiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata: {name: two}\nspec: {clusterRef: {name: dev}, namespace: elsewhere}\n";
        let with_instance = |name: &str, instance: &str| {
            target(name, "previews", name, PREVIEWS).replace(
                "  reconciler: {argocd: {}}\n",
                &format!("  reconciler: {{argocd: {{instanceRef: {{name: {instance}}}}}}}\n"),
            )
        };
        let across = [
            instances.to_owned(),
            with_instance("pr-1", "one"),
            with_instance("pr-2", "two"),
        ];
        let error = validate(&across).unwrap_err();
        assert!(
            error.contains(SHARED_CATALOG_CONFLICT_CODE) && error.contains("render different manifests"),
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

    /// The gate of the tree commands is structural and closed: a project
    /// is expanded when a parsed target shares a catalog, a template exists, or a
    /// publication unit may declare its target inline (a mapping, a templated
    /// name, or a document that is not statically parseable); only a project
    /// with static targets and no such unit is left alone, so it keeps its
    /// exit codes. Catches a templated or inline shared declaration skipping
    /// the rules, and a plain project being rendered for nothing.
    #[test]
    fn test_the_gate_expands_only_what_may_declare_a_shared_catalog() {
        let gate = |files: &[(&str, &str)]| {
            let directory = tempfile::TempDir::new().unwrap();
            let root = directory.path();
            std::fs::write(root.join("nyl.toml"), "[project]\n").unwrap();
            std::fs::write(
                root.join("platform.yaml"),
                format!("{BASE}{}", target("static", "deploy", "static", "{enabled: true}")),
            )
            .unwrap();
            for (name, text) in files {
                std::fs::write(root.join(name), text).unwrap();
            }
            git2::Repository::init(root).unwrap();
            let inventory = discover_gitops_inventory(root, None, crate::config::test_catalogue()).unwrap();
            may_declare_shared_catalog(&inventory)
        };
        let unit = |spec: &str| {
            format!("apiVersion: units.gitops.nyl/v1\nkind: KubernetesPublication\nmetadata: {{name: kubernetes}}\nspec:\n{spec}")
        };
        // Static targets only, and publication units that name a static target.
        assert!(!gate(&[]));
        assert!(!gate(&[("units.yaml", &unit("  target: static\n"))]));
        // A parsed target that shares, a template, an inline target, a templated name,
        // and a document that is not YAML before rendering.
        assert!(gate(&[("shared.yaml", &target("pr", "previews", "pr", PREVIEWS))]));
        assert!(gate(&[(
            "template.yaml",
            "apiVersion: gitops.nyl/v1\nkind: EnvironmentTemplate\nmetadata: {name: preview}\nspec:\n  unitSelector: {matchLabels: {preview: 'true'}}\n  state: {path: '{{ environment.name }}'}\n"
        )]));
        assert!(gate(&[(
            "units.yaml",
            &unit("  target:\n    inline: {clusterRef: {name: dev}, publication: {repositoryRef: {name: platform}, revision: x}}\n")
        )]));
        assert!(gate(&[("units.yaml", &unit("  target: '{{ values.target }}'\n"))]));
        assert!(gate(&[(
            "units.yaml",
            &unit("  target: {{ values.target | tojson }}\n")
        )]));
    }
}
