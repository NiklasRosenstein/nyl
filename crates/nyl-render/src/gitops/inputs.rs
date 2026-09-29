//! Resolution of Release inputs from DeploymentTarget bindings.
//!
//! Contract: [Release inputs and bindings](../../../../design/release-inputs.md).
//!
//! - [Bindings](../../../../design/release-inputs.md#bindings): the key table and
//!   `effective input = target binding, otherwise Release default`.
//! - [Binding kinds](../../../../design/release-inputs.md#binding-kinds): `value`,
//!   `fromFile`, and `fromGit` resolve here; `fromUnit` and `fromPromotion`
//!   need orchestrated execution and are rejected. `fromGit` reads only its
//!   locked commit and never resolves `revision`.
//! - [Provenance, caching, and validation](../../../../design/release-inputs.md#provenance-caching-and-validation):
//!   failures are reported together per target, before any Release renders,
//!   and each resolved input is digested for the ownership index.
//!
//! The declaration types and their pure rules live in
//! [`nyl_core::resources::release_inputs`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::resources::release_inputs::{BindingKind, InputBinding, InputDeclaration, ReleaseKey};
use crate::resources::{DeploymentTarget, InlineGitRepository};
use crate::util::project_path::ProjectPaths;
use crate::{NylError, Result};

/// Ownership-index key prefix of resolved input digests.
pub const INDEX_INPUT_PREFIX: &str = "@input/";

/// Ownership-index key prefix of `fromGit` blob digests.
pub const INDEX_GIT_PREFIX: &str = "@git/";

/// Where resolution may read binding sources from.
pub struct InputSources<'a> {
    /// Local path rule of this project, for `fromFile`.
    pub paths: &'a ProjectPaths,
    /// Git-visible YAML and JSON files, relative to the worktree root. A
    /// `fromFile` binding reads only these, so published output reproduces
    /// from committed source.
    pub visible_files: &'a BTreeSet<PathBuf>,
    /// Resolves a `fromGit` `repositoryRef` to the GitRepository it names and
    /// that resource's absolute source file, through
    /// [`GitOpsInventory::resolve_git_repository`](super::GitOpsInventory::resolve_git_repository).
    pub repositories: &'a RepositoryResolver<'a>,
    /// Reader of files at locked commits, for `fromGit`.
    pub git: &'a dyn GitBlobSource,
}

/// Resolves a GitRepository name to the repository and its source file.
pub type RepositoryResolver<'a> =
    dyn Fn(&crate::resources::LocalReference) -> Result<(InlineGitRepository, PathBuf)> + 'a;

/// Reads a file at an immutable commit.
pub trait GitBlobSource {
    /// The bytes of `path` at `commit` of `url`, or `None` when the commit has
    /// no such path.
    fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>>;
}

/// [`GitBlobSource`] over the project's remote artifacts: the vendor snapshot,
/// then the exact source cache, then the shared bare-repository cache.
///
/// Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds).
/// A locked file is a remote renderer input like a remote group source, so
/// `nyl vendor` captures it, `nyl vendor --check` requires it, and vendor mode
/// `required` never reads it from the network.
pub struct CachedGitBlobSource {
    manager: RefCell<Option<crate::git::GitManager>>,
    project_root: PathBuf,
    project_config: crate::config::ProjectConfig,
    cache: Option<crate::render::cache::RenderCache>,
    /// Created on first use, so targets without `fromGit` bindings never
    /// load the vendor lock.
    artifacts: std::cell::OnceCell<crate::render::artifact::ArtifactResolver>,
}

impl CachedGitBlobSource {
    /// Reuse `manager` when one exists; otherwise one is created on first use
    /// in the cache's external root, or the default Git cache.
    pub fn new(
        manager: Option<crate::git::GitManager>,
        project_root: &Path,
        project_config: &crate::config::ProjectConfig,
        cache: Option<crate::render::cache::RenderCache>,
    ) -> Self {
        Self {
            manager: RefCell::new(manager),
            project_root: project_root.to_path_buf(),
            project_config: project_config.clone(),
            cache,
            artifacts: std::cell::OnceCell::new(),
        }
    }

    fn artifacts(&self) -> Result<&crate::render::artifact::ArtifactResolver> {
        if let Some(artifacts) = self.artifacts.get() {
            return Ok(artifacts);
        }
        let artifacts = crate::render::artifact::ArtifactResolver::new(
            &self.project_root,
            &self.project_config,
            self.cache.clone(),
        )?;
        Ok(self.artifacts.get_or_init(|| artifacts))
    }

    fn read_from_git(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mut manager = self.manager.borrow_mut();
        if manager.is_none() {
            *manager = Some(crate::git::GitManager::for_cache(self.cache.as_ref())?);
        }
        Ok(manager
            .as_mut()
            .expect("manager was created above")
            .read_blob(url, commit, path)?)
    }
}

impl GitBlobSource for CachedGitBlobSource {
    fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        use crate::render::artifact::{ArtifactFormat, ArtifactRequest};
        let request = ArtifactRequest::GitBlob {
            repository: credential_free_url(url),
            commit: commit.to_owned(),
            path: path.to_owned(),
        };
        let artifacts = self.artifacts()?;
        if let Some(artifact) = artifacts.lookup(&request)? {
            return Ok(Some(std::fs::read(&artifact.path)?));
        }
        let Some(bytes) = self.read_from_git(url, commit, path)? else {
            return Ok(None);
        };
        let staged = tempfile::NamedTempFile::new()?;
        std::fs::write(staged.path(), &bytes)?;
        artifacts.store(
            &request,
            staged.path(),
            ArtifactFormat::GitBlob,
            Some(commit.to_owned()),
        )?;
        Ok(Some(bytes))
    }
}

pub use crate::util::credential_free_url;

/// A Release the target renders, with its literal input declarations.
pub struct ReleaseDeclaration<'a> {
    /// `<applicationGroup>/<release>` on the target.
    pub key: ReleaseKey,
    /// Literal `spec.inputs`; empty when the Release declares none.
    pub declarations: &'a BTreeMap<String, InputDeclaration>,
    /// Release entry file, for messages.
    pub source: &'a Path,
}

/// Where one effective input value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOrigin {
    /// The Release `default`.
    Default,
    /// An inline `value` binding.
    Value,
    /// A `fromFile` binding reading this project file.
    File(PathBuf),
    /// A `fromGit` binding reading `path` at `commit` of `url`.
    Git {
        /// Repository URL without credentials.
        url: String,
        commit: String,
        path: String,
        /// SHA-256 of the file bytes.
        blob_digest: String,
        /// Source file of the referenced GitRepository resource.
        repository_source: Option<PathBuf>,
    },
    /// A `--input` or `--inputs` override of a direct command.
    Override,
}

/// One effective input.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInput {
    pub value: Value,
    pub origin: InputOrigin,
}

/// Effective inputs of one Release that declares inputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedReleaseInputs {
    pub inputs: BTreeMap<String, ResolvedInput>,
}

impl ResolvedReleaseInputs {
    /// The `inputs` template variable.
    pub fn values(&self) -> serde_json::Map<String, Value> {
        self.inputs
            .iter()
            .map(|(name, input)| (name.clone(), input.value.clone()))
            .collect()
    }
}

/// Effective inputs of every Release a target renders that declares inputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedTargetInputs {
    pub releases: BTreeMap<ReleaseKey, ResolvedReleaseInputs>,
}

impl ResolvedTargetInputs {
    /// Project files the bindings read: `fromFile` files and the resources of
    /// GitRepositories that `fromGit` names.
    pub fn files(&self) -> BTreeSet<PathBuf> {
        self.releases
            .values()
            .flat_map(|release| release.inputs.values())
            .filter_map(|input| match &input.origin {
                InputOrigin::File(path)
                | InputOrigin::Git {
                    repository_source: Some(path),
                    ..
                } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Ownership-index entries: `@input/<group>/<release>/<input>` to the digest
    /// of the canonical JSON value.
    pub fn index_entries(&self) -> Result<BTreeMap<String, String>> {
        let mut entries = BTreeMap::new();
        for (key, release) in &self.releases {
            for (name, input) in &release.inputs {
                entries.insert(format!("{INDEX_INPUT_PREFIX}{key}/{name}"), value_digest(&input.value)?);
                if let InputOrigin::Git {
                    url,
                    commit,
                    path,
                    blob_digest,
                    ..
                } = &input.origin
                {
                    entries.insert(format!("{INDEX_GIT_PREFIX}{url}@{commit}/{path}"), blob_digest.clone());
                }
            }
        }
        Ok(entries)
    }
}

/// Digest of an input's canonical JSON value.
pub fn value_digest(value: &Value) -> Result<String> {
    Ok(nyl_core::digest::sha256_hex(&nyl_core::digest::canonical_json_bytes(
        value,
    )?))
}

/// Resolve the inputs of every rendered Release of `target`.
///
/// `releases` lists the Releases of selected, enabled groups; keys naming a
/// group in `disabled_groups` are ignored so `enabled` can still toggle a
/// group. Every other problem is collected and reported together.
pub fn resolve_target_inputs(
    target: &DeploymentTarget,
    releases: &[ReleaseDeclaration<'_>],
    disabled_groups: &BTreeSet<String>,
    sources: &InputSources<'_>,
) -> Result<ResolvedTargetInputs> {
    let target_name = &target.metadata.name;
    let mut issues = Vec::new();
    let mut declared = BTreeMap::new();
    for release in releases {
        if let Some(previous) = declared.insert(release.key.clone(), release) {
            if !release.declarations.is_empty() || !previous.declarations.is_empty() {
                issues.push(format!(
                    "{} is declared by both {} and {}; Releases that declare inputs need unique names within their ApplicationGroup",
                    release.key,
                    previous.source.display(),
                    release.source.display()
                ));
            }
        }
    }

    let mut bound = BTreeMap::new();
    for (key_text, bindings) in &target.spec.release_inputs {
        let key = ReleaseKey::parse(key_text)?;
        if disabled_groups.contains(&key.group) {
            continue;
        }
        let Some(release) = declared.get(&key) else {
            issues.push(format!(
                "spec.releaseInputs.{key_text:?} names no Release this target renders; keys are <applicationGroup>/<release> of a selected ApplicationGroup"
            ));
            continue;
        };
        if release.declarations.is_empty() {
            issues.push(format!(
                "spec.releaseInputs.{key_text:?} binds Release {key}, which declares no spec.inputs ({})",
                release.source.display()
            ));
            continue;
        }
        for name in bindings.keys() {
            if !release.declarations.contains_key(name) {
                issues.push(format!(
                    "spec.releaseInputs.{key_text:?}.{name} binds an input Release {key} does not declare; declared inputs: {}",
                    release.declarations.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        bound.insert(key, bindings);
    }

    let mut resolved = ResolvedTargetInputs::default();
    for (key, release) in &declared {
        if release.declarations.is_empty() {
            continue;
        }
        let bindings = bound.get(key).copied();
        let inputs = resolve_release_inputs(
            &format!("spec.releaseInputs.{:?}", key.to_string()),
            &key.to_string(),
            release.declarations,
            bindings,
            &BTreeMap::new(),
            sources,
            &mut issues,
        );
        resolved.releases.insert(key.clone(), inputs);
    }

    if issues.is_empty() {
        Ok(resolved)
    } else {
        Err(NylError::config(format!(
            "DeploymentTarget {target_name:?} has invalid Release inputs:\n{}",
            issues
                .iter()
                .map(|issue| format!("  - {issue}"))
                .collect::<Vec<_>>()
                .join("\n")
        )))
    }
}

/// Resolve one Release's declared inputs.
///
/// `effective input = override, otherwise binding, otherwise default`.
/// Overrides come from direct commands only. Problems are appended to
/// `issues`; the returned inputs then omit the failing names.
pub fn resolve_release_inputs(
    field_prefix: &str,
    release: &str,
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
    overrides: &BTreeMap<String, Value>,
    sources: &InputSources<'_>,
    issues: &mut Vec<String>,
) -> ResolvedReleaseInputs {
    let mut resolved = ResolvedReleaseInputs::default();
    for (name, declaration) in declarations {
        let field = format!("{field_prefix}.{name}");
        let binding = bindings.and_then(|bindings| bindings.get(name));
        let candidate = if let Some(value) = overrides.get(name) {
            Ok(Some(ResolvedInput {
                value: value.clone(),
                origin: InputOrigin::Override,
            }))
        } else if let Some(binding) = binding {
            resolve_binding(&field, binding, sources).map(Some)
        } else {
            Ok(declaration.default.clone().map(|value| ResolvedInput {
                value,
                origin: InputOrigin::Default,
            }))
        };
        match candidate {
            Ok(Some(input)) => match declaration.check(&input.value) {
                Ok(()) => {
                    resolved.inputs.insert(name.clone(), input);
                }
                Err(reason) => issues.push(format!(
                    "{} for input {name:?} of Release {release} is invalid: {reason}",
                    describe_origin(&field, &input.origin)
                )),
            },
            Ok(None) => issues.push(format!(
                "Release {release} requires input {name:?}{}, but it has no binding and no default; bind it in {field_prefix}",
                declaration
                    .description
                    .as_deref()
                    .map(|description| format!(" ({description})"))
                    .unwrap_or_default()
            )),
            Err(error) => issues.push(error),
        }
    }
    resolved
}

fn describe_origin(field: &str, origin: &InputOrigin) -> String {
    match origin {
        InputOrigin::Default => "The default".to_owned(),
        InputOrigin::Value => format!("{field}.value"),
        InputOrigin::File(path) => format!("{field}.fromFile ({})", path.display()),
        InputOrigin::Git { url, commit, path, .. } => format!("{field}.fromGit ({url}@{commit}/{path})"),
        InputOrigin::Override => "The --input/--inputs override".to_owned(),
    }
}

fn resolve_binding(
    field: &str,
    binding: &InputBinding,
    sources: &InputSources<'_>,
) -> std::result::Result<ResolvedInput, String> {
    let kind = binding.kind(field).map_err(|error| error.to_string())?;
    match kind {
        BindingKind::Value => Ok(ResolvedInput {
            value: binding.value.clone().expect("kind agrees with the set field"),
            origin: InputOrigin::Value,
        }),
        BindingKind::FromFile => {
            let source = binding.from_file.as_ref().expect("kind agrees with the set field");
            let path = sources
                .paths
                .resolve(&format!("{field}.fromFile.path"), &source.path)
                .map_err(|error| error.to_string())?;
            let visible = path
                .strip_prefix(&sources.paths.worktree_root)
                .is_ok_and(|relative| sources.visible_files.contains(relative));
            if !visible {
                return Err(format!(
                    "{field}.fromFile.path {:?} names no Git-visible YAML or JSON file of this repository; the file must exist and must not be ignored by Git or lie in the output or vendor subtree",
                    source.path
                ));
            }
            let document = read_single_document(&path).map_err(|reason| format!("{field}.fromFile: {reason}"))?;
            let value = select(&document, &source.pointer)
                .map_err(|reason| format!("{field}.fromFile: {} {reason}", source.path))?;
            Ok(ResolvedInput {
                value,
                origin: InputOrigin::File(path),
            })
        }
        BindingKind::FromGit => {
            let source = binding.from_git.as_ref().expect("kind agrees with the set field");
            let (repository, repository_source) = match (&source.repository, &source.repository_ref) {
                (Some(repository), _) => (repository.clone(), None),
                (None, Some(reference)) => {
                    let (repository, path) = (sources.repositories)(reference)
                        .map_err(|error| format!("{field}.fromGit.repositoryRef: {error}"))?;
                    (repository, Some(path))
                }
                (None, None) => unreachable!("validated fromGit names a repository"),
            };
            let bytes = sources
                .git
                .read_blob(&repository.repo_url, &source.commit, &source.path)
                .map_err(|error| match error {
                    NylError::Git(_) => format!(
                        "{field}.fromGit cannot read {} at locked commit {} of {}: {}. Rendering reads only the locked commit and fetches it by ID; offline, it must already be in the local Git cache",
                        source.path,
                        source.commit,
                        crate::util::sanitize_url(&repository.repo_url),
                        crate::util::redact_url_credentials(&error.to_string(), &repository.repo_url)
                    ),
                    // Vendor policy errors carry their own fix.
                    NylError::Config(message) => format!(
                        "{field}.fromGit: {}",
                        crate::util::redact_url_credentials(&message, &repository.repo_url)
                    ),
                    other => format!(
                        "{field}.fromGit: {}",
                        crate::util::redact_url_credentials(&other.to_string(), &repository.repo_url)
                    ),
                })?
                .ok_or_else(|| {
                    format!(
                        "{field}.fromGit: commit {} of {} has no file {}",
                        source.commit,
                        crate::util::sanitize_url(&repository.repo_url),
                        source.path
                    )
                })?;
            let document = parse_single_document(&bytes).map_err(|reason| format!("{field}.fromGit: {} {reason}", source.path))?;
            let value = select(&document, &source.pointer).map_err(|reason| format!("{field}.fromGit: {} {reason}", source.path))?;
            Ok(ResolvedInput {
                value,
                origin: InputOrigin::Git {
                    url: credential_free_url(&repository.repo_url),
                    commit: source.commit.clone(),
                    path: source.path.clone(),
                    blob_digest: nyl_core::digest::sha256_hex(&bytes),
                    repository_source,
                },
            })
        }
        BindingKind::FromUnit | BindingKind::FromPromotion => Err(format!(
            "{field}.{} needs orchestrated execution, which resolves it into a pinned input snapshot; render-tree, publish-tree, diff-tree, and direct commands never resolve it",
            kind.field()
        )),
    }
}

/// After rendering, a Release's `spec.inputs` must equal its static
/// declaration: templating cannot add, remove, or change declarations.
pub fn verify_rendered_declarations(
    path: &Path,
    declared: &BTreeMap<String, InputDeclaration>,
    release: &crate::resources::Release,
) -> Result<()> {
    if &release.spec.inputs == declared {
        Ok(())
    } else {
        Err(NylError::config(format!(
            "Release {:?} in {} renders spec.inputs that differ from its literal declaration; declare spec.inputs literally, without templating",
            release.metadata.name,
            path.display()
        )))
    }
}

/// Parse a YAML or JSON file holding exactly one document.
pub fn read_single_document(path: &Path) -> std::result::Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse_single_document(&bytes).map_err(|reason| format!("{} {reason}", path.display()))
}

/// Parse YAML or JSON bytes holding exactly one document.
pub fn parse_single_document(bytes: &[u8]) -> std::result::Result<Value, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not valid UTF-8".to_owned())?;
    let mut documents = crate::yaml::parse_yaml_documents_k8s_compatible(text)
        .map_err(|error| format!("is not valid YAML or JSON: {error}"))?;
    match documents.len() {
        1 => Ok(documents.remove(0)),
        0 => Err("contains no document".to_owned()),
        count => Err(format!(
            "contains {count} YAML documents; an input file must hold exactly one"
        )),
    }
}

/// Select `pointer` inside `document`.
pub fn select(document: &Value, pointer: &str) -> std::result::Result<Value, String> {
    nyl_core::json_pointer::resolve(document, pointer)
        .cloned()
        .ok_or_else(|| format!("has no value at JSON Pointer {pointer:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn target(release_inputs: Value) -> DeploymentTarget {
        serde_json::from_value(json!({
            "apiVersion": "k8s.gitops.nyl/v1",
            "kind": "DeploymentTarget",
            "metadata": {"name": "dev"},
            "spec": {
                "publication": {"repository": {"repoURL": "https://example.invalid/deploy.git"}, "revision": "deploy", "pathPrefix": "dev"},
                "releaseInputs": release_inputs,
            }
        }))
        .unwrap()
    }

    fn declarations(value: Value) -> BTreeMap<String, InputDeclaration> {
        serde_json::from_value(value).unwrap()
    }

    fn resolve(
        target: &DeploymentTarget,
        declared: &[(&str, &BTreeMap<String, InputDeclaration>)],
        disabled: &[&str],
        root: &Path,
    ) -> Result<ResolvedTargetInputs> {
        let paths = ProjectPaths::new(root.to_path_buf(), root.to_path_buf());
        let visible_files = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| PathBuf::from(entry.unwrap().file_name()))
            .filter(|name| !name.to_string_lossy().starts_with("ignored"))
            .collect::<BTreeSet<_>>();
        let releases = declared
            .iter()
            .map(|(key, declarations)| ReleaseDeclaration {
                key: ReleaseKey::parse(key).unwrap(),
                declarations,
                source: Path::new("release.yaml"),
            })
            .collect::<Vec<_>>();
        resolve_target_inputs(
            target,
            &releases,
            &disabled.iter().map(|name| (*name).to_owned()).collect(),
            &InputSources {
                paths: &paths,
                repositories: &|reference| {
                    Err(NylError::config(format!(
                        "GitRepository {:?} was not found",
                        reference.name
                    )))
                },
                git: &NoGit,
                visible_files: &visible_files,
            },
        )
    }

    struct NoGit;

    impl GitBlobSource for NoGit {
        fn read_blob(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
            Err(NylError::config("no Git in this test"))
        }
    }

    #[test]
    fn test_bindings_replace_defaults_and_files_resolve_pointers() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("db.yaml"),
            "database:\n  host: db.internal\n  port: 5432\n",
        )
        .unwrap();
        let declared = declarations(json!({
            "image": {"type": "string"},
            "replicas": {"type": "integer", "default": 2},
            "database": {"type": "object"},
            "host": {"type": "string"},
        }));
        let target = target(json!({"platform/web": {
            "image": {"value": "registry.example/web@sha256:1"},
            "database": {"fromFile": {"path": "db.yaml", "pointer": "/database"}},
            "host": {"fromFile": {"path": "db.yaml", "pointer": "/database/host"}},
        }}));
        let resolved = resolve(&target, &[("platform/web", &declared)], &[], temp.path()).unwrap();
        let web = &resolved.releases[&ReleaseKey::parse("platform/web").unwrap()];
        assert_eq!(
            Value::Object(web.values()),
            json!({"image": "registry.example/web@sha256:1", "replicas": 2, "database": {"host": "db.internal", "port": 5432}, "host": "db.internal"})
        );
        assert_eq!(web.inputs["replicas"].origin, InputOrigin::Default);
        assert_eq!(resolved.files(), BTreeSet::from([temp.path().join("db.yaml")]));
        let entries = resolved.index_entries().unwrap();
        assert_eq!(
            entries["@input/platform/web/replicas"],
            nyl_core::digest::sha256_hex(b"2\n")
        );
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn test_disabled_group_keys_are_ignored() {
        let temp = TempDir::new().unwrap();
        let target = target(json!({"optional/web": {"image": {"value": "x"}}}));
        let resolved = resolve(&target, &[], &["optional"], temp.path()).unwrap();
        assert!(resolved.releases.is_empty());
    }

    #[test]
    fn test_problems_are_reported_together() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("two.yaml"), "a: 1\n---\na: 2\n").unwrap();
        let web = declarations(json!({
            "image": {"type": "string", "description": "Immutable image reference"},
            "replicas": {"type": "integer"},
            "tier": {"type": "string"},
            "seed": {"type": "string"},
        }));
        let plain = BTreeMap::new();
        let target = target(json!({
            "platform/missing": {"image": {"value": "x"}},
            "platform/plain": {"image": {"value": "x"}},
            "platform/web": {
                "unknown": {"value": 1},
                "replicas": {"value": 2.5},
                "tier": {"fromFile": {"path": "two.yaml"}},
                "seed": {"fromUnit": {"unit": "seed", "output": "version"}},
            },
        }));
        let error = resolve(
            &target,
            &[("platform/web", &web), ("platform/plain", &plain)],
            &[],
            temp.path(),
        )
        .unwrap_err()
        .to_string();
        for expected in [
            "DeploymentTarget \"dev\" has invalid Release inputs",
            "\"platform/missing\" names no Release this target renders",
            "binds Release platform/plain, which declares no spec.inputs",
            "binds an input Release platform/web does not declare",
            "requires input \"image\" (Immutable image reference)",
            "expected integer, got number",
            "contains 2 YAML documents",
            "fromUnit needs orchestrated execution",
        ] {
            assert!(error.contains(expected), "missing {expected:?} in:\n{error}");
        }
    }

    struct FakeGit(BTreeMap<(String, String, String), Vec<u8>>);

    impl GitBlobSource for FakeGit {
        fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .get(&(url.to_owned(), commit.to_owned(), path.to_owned()))
                .cloned())
        }
    }

    #[test]
    fn test_from_git_reads_the_locked_commit_and_records_the_blob() {
        let temp = TempDir::new().unwrap();
        let commit = "3f1c9a0000000000000000000000000000000000";
        let bytes = b"web:\n  tier: large\n".to_vec();
        let git = FakeGit(BTreeMap::from([(
            (
                "https://git.example.com/state.git".to_owned(),
                commit.to_owned(),
                "staging/sizing.yaml".to_owned(),
            ),
            bytes.clone(),
        )]));
        let repositories = BTreeMap::from([(
            "state".to_owned(),
            (
                InlineGitRepository {
                    repo_url: "https://git.example.com/state.git".to_owned(),
                    publish_url: None,
                },
                PathBuf::from("config/state.yaml"),
            ),
        )]);
        let declared = declarations(json!({"tier": {"type": "string"}, "missing": {"type": "string", "default": "x"}}));
        let target = target(json!({"platform/web": {
            "tier": {"fromGit": {"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "staging/sizing.yaml", "pointer": "/web/tier"}},
            "missing": {"fromGit": {"repository": {"repoURL": "https://git.example.com/state.git"}, "revision": "main", "commit": commit, "path": "absent.yaml"}},
        }}));
        let paths = ProjectPaths::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        let releases = [ReleaseDeclaration {
            key: ReleaseKey::parse("platform/web").unwrap(),
            declarations: &declared,
            source: Path::new("release.yaml"),
        }];
        let sources = InputSources {
            paths: &paths,
            visible_files: &BTreeSet::new(),
            repositories: &|reference| {
                repositories
                    .get(&reference.name)
                    .cloned()
                    .ok_or_else(|| NylError::config(format!("GitRepository {:?} was not found", reference.name)))
            },
            git: &git,
        };
        let error = resolve_target_inputs(&target, &releases, &BTreeSet::new(), &sources)
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no file absent.yaml"), "{error}");

        let target = super::tests::target(json!({"platform/web": {
            "tier": {"fromGit": {"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "staging/sizing.yaml", "pointer": "/web/tier"}},
        }}));
        let resolved = resolve_target_inputs(&target, &releases, &BTreeSet::new(), &sources).unwrap();
        let web = &resolved.releases[&ReleaseKey::parse("platform/web").unwrap()];
        assert_eq!(web.inputs["tier"].value, json!("large"));
        assert_eq!(resolved.files(), BTreeSet::from([PathBuf::from("config/state.yaml")]));
        let entries = resolved.index_entries().unwrap();
        assert_eq!(
            entries[&format!("@git/https://git.example.com/state.git@{commit}/staging/sizing.yaml")],
            nyl_core::digest::sha256_hex(&bytes)
        );
    }

    #[test]
    fn test_credential_free_url_drops_userinfo() {
        assert_eq!(
            credential_free_url("https://user:token@git.example.com/state.git"),
            "https://git.example.com/state.git"
        );
        assert_eq!(
            credential_free_url("https://token@Git.Example.com/state.git"),
            credential_free_url("https://Git.Example.com/state.git")
        );
        assert_eq!(
            credential_free_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }

    #[test]
    fn test_from_file_reads_only_git_visible_files() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("ignored.yaml"), "port: 1\n").unwrap();
        let declared = declarations(json!({"port": {"type": "integer"}}));
        for path in ["ignored.yaml", "missing.yaml"] {
            let target = target(json!({"platform/web": {"port": {"fromFile": {"path": path, "pointer": "/port"}}}}));
            let error = resolve(&target, &[("platform/web", &declared)], &[], temp.path())
                .unwrap_err()
                .to_string();
            assert!(error.contains("names no Git-visible YAML or JSON file"), "{error}");
        }
    }

    #[test]
    fn test_missing_pointer_is_an_error() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("db.json"), "{\"host\": \"db\"}").unwrap();
        let declared = declarations(json!({"port": {"type": "integer"}}));
        let target = target(json!({"platform/web": {"port": {"fromFile": {"path": "db.json", "pointer": "/port"}}}}));
        let error = resolve(&target, &[("platform/web", &declared)], &[], temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no value at JSON Pointer \"/port\""), "{error}");
    }
}
