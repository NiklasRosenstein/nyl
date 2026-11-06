//! What an observer of a published target needs: where its Argo CD instance
//! is and which Applications the target generates (the declaration), and the
//! publication history that matches a running revision to a publication
//! (the Git side).
//!
//! Contract: [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications)
//! (observer, running revision, matching a publication, recorded commit).
//! Nothing here reads Argo CD: reading the Applications and judging health
//! belongs to the publication driver, which asks this module only for
//! declared facts ([`ArgoCdObservation`]) and Git answers
//! ([`PublicationHistory`]).
//!
//! - [`ArgoCdObservation`] is what a driver records at resolution: the
//!   instance (its namespace, control-plane Cluster, local context, and
//!   declared access) and the Applications the target generates, so that
//!   `attest` and an `observe` teardown wait need no render.
//! - [`PublicationHistory`] answers the matching questions over the
//!   publication branch. [`running_publication`] applies the rule: the
//!   publication commits that are the running revision or its ancestors and
//!   whose Application directory tree and recorded inputs equal those at the
//!   running revision form a run, and the recorded commit is the oldest of
//!   the unbroken run ending at it. [`GitPublicationHistory`] implements the
//!   trait over a fetched copy of the branch.

use std::collections::BTreeMap;
use std::path::Path;

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::publication::{PublicationDestination, RenderedTarget};
use super::reconcile::{parse_index, DEFAULT_INDEX_PATH};
use crate::resources::ArgoCDAccess;
use crate::{NylError, Result};

/// Where a target's Argo CD instance is and how Nyl reads it, from the
/// declaration (the instance, its control-plane Cluster, and the target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgoCdLocation {
    /// The ArgoCDInstance's name, or `implicit:<target>` without explicit instances.
    pub instance: String,
    /// The namespace holding the instance's Applications and AppProjects.
    pub namespace: String,
    /// The control-plane Cluster.
    pub cluster: String,
    /// The Cluster's declared local kubeconfig context (`spec.live.context`), if any.
    pub context: Option<String>,
    /// The declared access, the Kubernetes API of the Cluster by default.
    pub access: ArgoCDAccess,
}

/// Where a target's rendered resources live: its workload Cluster, with that
/// Cluster's declared local context (`spec.live.context`). It may differ from
/// the Argo CD control-plane Cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadCluster {
    /// The Cluster's name.
    pub cluster: String,
    /// The Cluster's declared local kubeconfig context, if any.
    pub context: Option<String>,
}

/// The part an Application plays in a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplicationRole {
    /// The Application sourcing `_nyl/catalog`, which creates the others.
    Catalog,
    /// Any other generated Application: a Release's or a dedicated Namespace Application.
    Workload,
}

/// An Argo CD Application the target generates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedApplication {
    pub role: ApplicationRole,
    pub name: String,
    /// The namespace the Application object lives in.
    pub namespace: String,
    /// The directory it sources, from the publication repository's root.
    pub path: String,
    /// The Argo CD project it belongs to (`spec.project`), which Argo CD's
    /// policy names it by. Empty in a document recorded before it was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// `<applicationGroup>/<release>` for a Release's Application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The generated `spec.source` without `targetRevision` (the revision is
    /// what an observer reads, not a setting to compare): the repository, the
    /// directory, and how Argo CD renders it (`directory.recurse`, and any
    /// `exclude`, `include`, or tool block). An Application edited in the
    /// cluster to render the directory differently no longer matches it.
    /// Null in a document recorded before it was.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub source: Value,
}

/// Where the target publishes, as a reader of the branch needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservedPublication {
    /// The URL the branch is read from (the publication URL; never carries credentials).
    pub url: String,
    pub branch: String,
    pub path_prefix: String,
}

/// Everything an observer needs from the declaration, recorded by the
/// publication driver at resolution (a persisted format whose camelCase keys
/// a test pins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgoCdObservation {
    #[serde(flatten)]
    pub location: ArgoCdLocation,
    pub publication: ObservedPublication,
    /// Every Application of the target, the catalog first.
    pub applications: Vec<GeneratedApplication>,
}

impl ArgoCdObservation {
    /// The observation facts of a rendered target.
    pub fn of(rendered: &RenderedTarget) -> Result<Self> {
        let PublicationDestination {
            push_url,
            branch,
            path_prefix,
            ..
        } = &rendered.destination;
        Ok(Self {
            location: rendered.argocd.clone(),
            publication: ObservedPublication {
                url: push_url.clone(),
                branch: branch.clone(),
                path_prefix: path_prefix.clone(),
            },
            applications: generated_applications(rendered)?,
        })
    }

    /// The Applications that are not the catalog: what a teardown's first
    /// phase removes.
    pub fn workload_applications(&self) -> impl Iterator<Item = &GeneratedApplication> {
        self.applications
            .iter()
            .filter(|application| application.role == ApplicationRole::Workload)
    }
}

/// The Applications a rendered target generates, read from its catalog
/// files as Argo CD will see them: the one sourcing `_nyl/catalog` is the
/// catalog, every other one is a workload Application. The catalog sorts
/// first, the rest by name.
pub fn generated_applications(rendered: &RenderedTarget) -> Result<Vec<GeneratedApplication>> {
    let prefix = rendered.destination.path_prefix.trim_matches('/');
    let catalog_source = if prefix.is_empty() {
        super::CATALOG_DIRECTORY.to_owned()
    } else {
        format!("{prefix}/{}", super::CATALOG_DIRECTORY)
    };
    let releases = release_applications(rendered)?;
    let mut applications = Vec::new();
    for (path, bytes) in &rendered.files {
        if !path.starts_with(super::CATALOG_DIRECTORY) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| NylError::config(format!("generated catalog file {} is not UTF-8", path.display())))?;
        for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
            if manifest.get("kind").and_then(Value::as_str) != Some("Application") {
                continue;
            }
            let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
            let (Some(name), Some(namespace), Some(source)) = (
                text("/metadata/name"),
                text("/metadata/namespace"),
                text("/spec/source/path"),
            ) else {
                return Err(NylError::config(format!(
                    "generated Application in {} lacks a name, namespace, or source path",
                    path.display()
                )));
            };
            let mut generated_source = manifest.pointer("/spec/source").cloned().unwrap_or(Value::Null);
            if let Some(object) = generated_source.as_object_mut() {
                object.remove("targetRevision");
            }
            let project = text("/spec/project").unwrap_or_default();
            applications.push(GeneratedApplication {
                role: if source == catalog_source {
                    ApplicationRole::Catalog
                } else {
                    ApplicationRole::Workload
                },
                release: releases.get(&name).cloned(),
                name,
                namespace,
                path: source,
                project,
                source: generated_source,
            });
        }
    }
    applications.sort_by(|a, b| (a.role, &a.name, &a.namespace).cmp(&(b.role, &b.name, &b.namespace)));
    Ok(applications)
}

/// A Kubernetes resource as the Kubernetes API names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRef {
    pub api_version: String,
    pub kind: String,
    /// The namespace of a namespaced resource; absent for a cluster-scoped one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
}

impl std::fmt::Display for ResourceRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(namespace) => write!(formatter, "{} {namespace}/{} ({})", self.kind, self.name, self.api_version),
            None => write!(formatter, "{} {} ({})", self.kind, self.name, self.api_version),
        }
    }
}

/// The workload Application of `rendered` whose directory renders `resource`,
/// `None` when the target renders no such resource: the check that keeps a
/// driver from reading anything the declaration does not own.
///
/// A manifest without `metadata.namespace` belongs to the namespace its
/// Application deploys to (`spec.destination.namespace`), as Argo CD applies
/// it. Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// observed values.
pub fn owning_application(rendered: &RenderedTarget, resource: &ResourceRef) -> Result<Option<GeneratedApplication>> {
    let applications = generated_applications(rendered)?;
    let destinations = destination_namespaces(rendered)?;
    owning_application_in(
        &rendered.files,
        &rendered.destination.path_prefix,
        &applications,
        &destinations,
        resource,
    )
}

/// The namespace each generated Application deploys to, by its namespace and name.
fn destination_namespaces(rendered: &RenderedTarget) -> Result<BTreeMap<(String, String), String>> {
    let mut destinations = BTreeMap::new();
    for (path, bytes) in &rendered.files {
        if !path.starts_with(super::CATALOG_DIRECTORY) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| NylError::config(format!("generated catalog file {} is not UTF-8", path.display())))?;
        for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
            if manifest.get("kind").and_then(Value::as_str) != Some("Application") {
                continue;
            }
            let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
            if let (Some(name), Some(namespace), Some(destination)) = (
                text("/metadata/name"),
                text("/metadata/namespace"),
                text("/spec/destination/namespace"),
            ) {
                destinations.insert((namespace, name), destination);
            }
        }
    }
    Ok(destinations)
}

fn owning_application_in(
    files: &BTreeMap<std::path::PathBuf, Vec<u8>>,
    prefix: &str,
    applications: &[GeneratedApplication],
    destinations: &BTreeMap<(String, String), String>,
    resource: &ResourceRef,
) -> Result<Option<GeneratedApplication>> {
    let prefix = prefix.trim_matches('/');
    for application in applications
        .iter()
        .filter(|application| application.role == ApplicationRole::Workload)
    {
        let directory = application.path.trim_matches('/');
        let directory = if prefix.is_empty() {
            directory
        } else {
            directory.strip_prefix(prefix).unwrap_or(directory).trim_matches('/')
        };
        let destination = destinations.get(&(application.namespace.clone(), application.name.clone()));
        for (path, bytes) in files {
            let manifests = path.starts_with(directory)
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yaml" | "yml" | "json")
                );
            if !manifests {
                continue;
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| NylError::config(format!("rendered file {} is not UTF-8", path.display())))?;
            for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
                let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str);
                if text("/apiVersion") != Some(resource.api_version.as_str())
                    || text("/kind") != Some(resource.kind.as_str())
                    || text("/metadata/name") != Some(resource.name.as_str())
                {
                    continue;
                }
                let namespace = match (text("/metadata/namespace"), &resource.namespace) {
                    (Some(namespace), _) => Some(namespace),
                    // Applied into the namespace the Application deploys to.
                    (None, Some(_)) => destination.map(String::as_str),
                    (None, None) => None,
                };
                if namespace == resource.namespace.as_deref() {
                    return Ok(Some(application.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// `<group>/<release>` by generated Application name, from the ownership
/// index's compilation record; empty for an index without one.
fn release_applications(rendered: &RenderedTarget) -> Result<BTreeMap<String, String>> {
    let Some(bytes) = rendered.files.get(Path::new(DEFAULT_INDEX_PATH)) else {
        return Ok(BTreeMap::new());
    };
    let index = parse_index(bytes, DEFAULT_INDEX_PATH)?;
    Ok(release_applications_of(index.compilation.as_ref()))
}

fn release_applications_of(compilation: Option<&Value>) -> BTreeMap<String, String> {
    compilation
        .and_then(|compilation| compilation.get("releases"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, release)| {
            let name = release.pointer("/workload/application_name")?.as_str()?;
            Some((name.to_owned(), key.clone()))
        })
        .collect()
}

/// A revision of the publication branch, as [`PublicationHistory`] names
/// commits: the full hexadecimal object id.
pub type Revision = String;

/// What the matching rules ask of the publication branch. Every answer is
/// about the branch as it was fetched; an error is an unreadable repository,
/// never a missing commit.
pub trait PublicationHistory {
    /// Whether `revision` names a commit of the fetched history.
    fn contains(&self, revision: &str) -> Result<bool>;

    /// The identity of the directory `path` (from the repository root) at
    /// `revision`, `None` when it does not exist there. Equal identities mean
    /// equal contents, everything Argo CD would read.
    fn directory(&self, revision: &str, path: &str) -> Result<Option<String>>;

    /// The recorded inputs of `release` in the ownership index at
    /// `revision`: its `compilation` record's `inputs` for the Release, an
    /// empty map when the index records none, and `None` when the commit
    /// has no index.
    fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>>;

    /// The publication commits (those that change the ownership index) that
    /// are `revision` or its ancestors along first parents, newest first, at
    /// most `limit`; the flag says whether older ones were left out.
    fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)>;
}

/// How many publication commits a match walks back at most.
pub const MATCH_WALK_LIMIT: usize = 1_000;

/// An Application's directory and Release, the identity the matching rules
/// compare.
#[derive(Debug, Clone, Copy)]
pub struct ApplicationTree<'a> {
    pub path: &'a str,
    pub release: Option<&'a str>,
}

/// Whether two revisions hold the same Application: the same directory tree
/// and, for a Release's Application, the same recorded inputs.
pub fn equivalent(
    history: &dyn PublicationHistory,
    application: ApplicationTree<'_>,
    a: &str,
    b: &str,
) -> Result<bool> {
    if history.directory(a, application.path)? != history.directory(b, application.path)? {
        return Ok(false);
    }
    match application.release {
        Some(release) => Ok(history.recorded_inputs(a, release)? == history.recorded_inputs(b, release)?),
        None => Ok(true),
    }
}

/// The publication an Application runs when Argo CD reports `revision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningPublication {
    /// The commit that introduced what runs: the oldest publication commit of
    /// the unbroken run of equivalent ones ending at the running revision.
    pub recorded: Revision,
    /// The newest publication commit in the run, which is at or before the
    /// running revision.
    pub newest: Revision,
    /// The run reaches the oldest commit the walk examined, so it may start earlier.
    pub truncated: bool,
}

/// What matching a running revision found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningMatch {
    /// The revision is not on the fetched branch.
    NotOnBranch,
    /// No publication commit holds the Application as the revision does:
    /// something other than a publication, such as another tool's commit
    /// inside the Application's directory, is what runs.
    NoPublication,
    Matched(RunningPublication),
}

/// Match the revision an Application runs to the target's publication
/// commits. A revision that is another target's commit, or a write-back
/// outside the Application's directory, matches the newest publication at or
/// before it, whose directory tree and recorded inputs it shares; one that
/// changed the directory without publishing matches none, even when it
/// restored the tree of an older publication.
pub fn running_publication(
    history: &dyn PublicationHistory,
    application: ApplicationTree<'_>,
    running: &str,
    limit: usize,
) -> Result<RunningMatch> {
    if !history.contains(running)? {
        return Ok(RunningMatch::NotOnBranch);
    }
    let (commits, more) = history.publication_commits(running, limit)?;
    let mut equal = Vec::new();
    for commit in &commits {
        equal.push(equivalent(history, application, commit, running)?);
    }
    // The newest publication at or before the running revision is what Nyl
    // says the revision holds, so it must be the one that matches: a commit
    // that restores an older tree by hand, or edits the directory, matches
    // none, however well an older publication would fit.
    if !equal.first().copied().unwrap_or(false) {
        return Ok(RunningMatch::NoPublication);
    }
    let newest = 0;
    // The run: consecutive equivalent commits from the newest match back.
    let mut oldest = newest;
    while oldest + 1 < commits.len() && equal[oldest + 1] {
        oldest += 1;
    }
    Ok(RunningMatch::Matched(RunningPublication {
        recorded: commits[oldest].clone(),
        newest: commits[newest].clone(),
        truncated: more && oldest + 1 == commits.len(),
    }))
}

/// [`PublicationHistory`] over a fetched copy of the publication branch
/// beneath one path prefix.
pub struct GitPublicationHistory {
    repository: Repository,
    index_path: String,
    /// Keeps the scratch repository alive.
    _scratch: Option<tempfile::TempDir>,
}

impl GitPublicationHistory {
    /// Fetch `branch` of `url` into a scratch repository, authenticating as
    /// the project's `[git]` settings say (variables read only through
    /// `variable`), and read it beneath `path_prefix`. A branch the remote
    /// does not have reads as an empty history.
    pub fn fetch(
        publication: &ObservedPublication,
        project_dir: &Path,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let auth = super::publication::project_auth(project_dir, variable)?;
        let scratch = tempfile::TempDir::new()?;
        let repository = git(Repository::init_bare(scratch.path()))?;
        crate::git::transport::fetch_branch(
            &repository,
            &publication.url,
            &publication.branch,
            "refs/nyl/observed",
            &auth,
        )
        .map_err(|error| {
            NylError::config(format!(
                "cannot read publication branch {} of {}: {error}",
                publication.branch,
                nyl_git::sanitize_url(&publication.url)
            ))
        })?;
        Ok(Self::over(repository, &publication.path_prefix, Some(scratch)))
    }

    /// Read `repository`, which already holds the history, beneath `path_prefix`.
    pub fn over(repository: Repository, path_prefix: &str, scratch: Option<tempfile::TempDir>) -> Self {
        let prefix = path_prefix.trim_matches('/');
        Self {
            repository,
            index_path: if prefix.is_empty() {
                DEFAULT_INDEX_PATH.to_owned()
            } else {
                format!("{prefix}/{DEFAULT_INDEX_PATH}")
            },
            _scratch: scratch,
        }
    }

    fn commit(&self, revision: &str) -> Result<Option<git2::Commit<'_>>> {
        let Ok(oid) = Oid::from_str(revision) else {
            return Ok(None);
        };
        match self.repository.find_commit(oid) {
            Ok(commit) => Ok(Some(commit)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }

    fn index_blob(&self, commit: &git2::Commit<'_>) -> Result<Option<Oid>> {
        match git(commit.tree())?.get_path(Path::new(&self.index_path)) {
            Ok(entry) => Ok(Some(entry.id())),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }
}

impl PublicationHistory for GitPublicationHistory {
    fn contains(&self, revision: &str) -> Result<bool> {
        Ok(self.commit(revision)?.is_some())
    }

    fn directory(&self, revision: &str, path: &str) -> Result<Option<String>> {
        let Some(commit) = self.commit(revision)? else {
            return Ok(None);
        };
        match git(commit.tree())?.get_path(Path::new(path.trim_matches('/'))) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) => Ok(Some(entry.id().to_string())),
            // A file where the directory should be is no Application directory.
            Ok(_) => Ok(None),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }

    fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>> {
        let Some(commit) = self.commit(revision)? else {
            return Ok(None);
        };
        let Some(blob) = self.index_blob(&commit)? else {
            return Ok(None);
        };
        let blob = git(self.repository.find_blob(blob))?;
        let index = parse_index(blob.content(), DEFAULT_INDEX_PATH)?;
        let inputs = index
            .compilation
            .as_ref()
            .and_then(|compilation| compilation.pointer(&format!("/releases/{}/inputs", escape_pointer(release))))
            .and_then(Value::as_object)
            .map(|inputs| {
                inputs
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(inputs))
    }

    fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)> {
        let mut commits = Vec::new();
        let mut current = self.commit(revision)?;
        while let Some(commit) = current {
            let parent = commit.parents().next();
            let index = self.index_blob(&commit)?;
            let parent_index = match &parent {
                Some(parent) => self.index_blob(parent)?,
                None => None,
            };
            if index.is_some() && index != parent_index {
                if commits.len() == limit {
                    return Ok((commits, true));
                }
                commits.push(commit.id().to_string());
            }
            current = parent;
        }
        Ok((commits, false))
    }
}

fn git<T>(result: std::result::Result<T, git2::Error>) -> Result<T> {
    result.map_err(|error| crate::git::GitError::from(error).into())
}

/// A JSON Pointer reference token: `~` and `/` escaped.
fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::*;

    /// A history scripted by commit: each holds directory identities, the
    /// release inputs, and whether it publishes; parents are given oldest
    /// first as a chain.
    #[derive(Default)]
    struct Script {
        chain: Vec<&'static str>,
        directories: BTreeMap<(&'static str, &'static str), &'static str>,
        inputs: BTreeMap<(&'static str, &'static str), Vec<(&'static str, &'static str)>>,
        publishes: BTreeSet<&'static str>,
        asked: RefCell<usize>,
    }

    impl PublicationHistory for Script {
        fn contains(&self, revision: &str) -> Result<bool> {
            Ok(self.chain.contains(&revision))
        }
        fn directory(&self, revision: &str, path: &str) -> Result<Option<String>> {
            *self.asked.borrow_mut() += 1;
            Ok(self
                .directories
                .iter()
                .find(|((commit, directory), _)| *commit == revision && *directory == path)
                .map(|(_, tree)| (*tree).to_owned()))
        }
        fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>> {
            Ok(self
                .inputs
                .iter()
                .find(|((commit, name), _)| *commit == revision && *name == release)
                .map(|(_, inputs)| {
                    inputs
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect()
                }))
        }
        fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)> {
            let Some(position) = self.chain.iter().position(|commit| *commit == revision) else {
                return Ok((Vec::new(), false));
            };
            let all = self.chain[..=position]
                .iter()
                .rev()
                .filter(|commit| self.publishes.contains(*commit))
                .map(|commit| (*commit).to_owned())
                .collect::<Vec<_>>();
            let more = all.len() > limit;
            Ok((all.into_iter().take(limit).collect(), more))
        }
    }

    const WEB: ApplicationTree<'static> = ApplicationTree {
        path: "dev/web/web",
        release: Some("web/web"),
    };

    fn script(web_trees: &[(&'static str, &'static str)]) -> Script {
        let mut script = Script::default();
        for (commit, tree) in web_trees {
            script.chain.push(commit);
            script.directories.insert((commit, "dev/web/web"), tree);
            script.publishes.insert(commit);
            script
                .inputs
                .insert((commit, "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        }
        script
    }

    /// Catches a commit that did not change the Application ending the run
    /// early: a tree that stays while other Applications publish is one run,
    /// whose recorded commit is the first of them, so a promotion names the
    /// commit that introduced what runs.
    #[test]
    fn test_recorded_commit_is_the_oldest_of_the_unbroken_run() {
        let history = script(&[
            ("p1", "tree-a"),
            ("p2", "tree-a"),
            ("p3", "tree-b"),
            ("p4", "tree-b"),
            ("p5", "tree-b"),
        ]);
        let matched = running_publication(&history, WEB, "p5", 10).unwrap();
        assert_eq!(
            matched,
            RunningMatch::Matched(RunningPublication {
                recorded: "p3".to_owned(),
                newest: "p5".to_owned(),
                truncated: false
            })
        );
    }

    /// Catches a tree that went A, B, A recording the first A: the change
    /// that was reverted starts a new run, so the second A is recorded.
    #[test]
    fn test_a_reverted_tree_starts_a_new_run() {
        let history = script(&[("p1", "tree-a"), ("p2", "tree-b"), ("p3", "tree-a")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, "p3");
    }

    /// Catches an Application synced to a commit of another target (or a
    /// write-back) not matching the publication whose directory it equals:
    /// the running revision is not a publication commit itself.
    #[test]
    fn test_a_foreign_commit_matches_the_publication_with_the_same_directory() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-b")]);
        // `other` is another target's commit after p2, leaving dev/web/web alone.
        history.chain.push("other");
        history.directories.insert(("other", "dev/web/web"), "tree-b");
        history
            .inputs
            .insert(("other", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "other", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.newest.as_str()), ("p2", "p2"));
    }

    /// Catches a hand edit that restores an older publication's tree being
    /// credited to that publication: the newest publication before the
    /// revision holds another tree, so the revision is no publication.
    #[test]
    fn test_a_hand_edited_revert_matches_no_publication() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-b")]);
        history.chain.push("revert");
        history.directories.insert(("revert", "dev/web/web"), "tree-a");
        history
            .inputs
            .insert(("revert", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        assert_eq!(
            running_publication(&history, WEB, "revert", 10).unwrap(),
            RunningMatch::NoPublication
        );
    }

    /// Catches a commit that edited the Application's directory without
    /// publishing being credited to the publication before it, which would
    /// let a hand-edited tree attest as the published one.
    #[test]
    fn test_a_commit_that_changes_the_directory_without_publishing_matches_none() {
        let mut history = script(&[("p1", "tree-a")]);
        history.chain.push("edit");
        history.directories.insert(("edit", "dev/web/web"), "tree-edited");
        history
            .inputs
            .insert(("edit", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        assert_eq!(
            running_publication(&history, WEB, "edit", 10).unwrap(),
            RunningMatch::NoPublication
        );
        assert_eq!(
            running_publication(&history, WEB, "unknown", 10).unwrap(),
            RunningMatch::NotOnBranch
        );
    }

    /// Catches equal manifests with different recorded inputs counting as one
    /// publication: values come from the publication that recorded them, so
    /// a changed input starts a new run even when the bytes did not change.
    #[test]
    fn test_changed_recorded_inputs_break_the_run() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-a")]);
        history
            .inputs
            .insert(("p2", "web/web"), vec![("@input/web/web/image", "sha256:2")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p2", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, "p2");
    }

    /// Catches a long history being walked without bound, and a run that
    /// reaches the walk's end claiming to start there.
    #[test]
    fn test_a_run_reaching_the_walk_limit_is_reported_truncated() {
        let history = script(&[("p1", "tree-a"), ("p2", "tree-a"), ("p3", "tree-a")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 2).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.truncated), ("p2", true));
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 3).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.truncated), ("p1", false));
    }

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn index(inputs: &str) -> String {
        format!(
            r#"{{"version":3,"target":"dev","cluster":"dev","publication":{{"repository":"r","revision":"deploy","pathPrefix":"dev"}},"dirty":false,"inputs":{{}},"files":{{}},"compilation":{{"releases":{{"web/web":{{"inputs":{inputs}}}}}}}}}"#
        )
    }

    /// Catches the Git side disagreeing with the rules: publication commits
    /// are those that change the index (another tool's commit is not one),
    /// directory identity follows content, and recorded inputs come from the
    /// index's compilation record, all through a real repository.
    #[test]
    fn test_git_history_answers_the_matching_questions() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        git(root, &["init", "--quiet", "--initial-branch=deploy"]);
        let write = |path: &str, text: &str| {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        let commit = |message: &str| {
            git(root, &["add", "--all"]);
            git(root, &["commit", "--quiet", "--message", message]);
            git(root, &["rev-parse", "HEAD"])
        };
        write("dev/web/web/deployment.yaml", "a: 1\n");
        write("dev/_nyl/index.json", &index(r#"{"@input/web/web/image":"sha256:1"}"#));
        let p1 = commit("publish 1");
        write("other/file.yaml", "x: 1\n");
        let other = commit("another target");
        write("dev/web/web/deployment.yaml", "a: 2\n");
        write("dev/_nyl/index.json", &index(r#"{"@input/web/web/image":"sha256:2"}"#));
        let p2 = commit("publish 2");
        write("dev/web/web/note.yaml", "edited: true\n");
        let edit = commit("edit inside the application");

        let history = GitPublicationHistory::over(Repository::open(root).unwrap(), "dev", None);
        assert!(history.contains(&p1).unwrap());
        assert!(!history.contains(&"0".repeat(40)).unwrap());
        assert_eq!(
            history.publication_commits(&edit, 10).unwrap(),
            (vec![p2.clone(), p1.clone()], false)
        );
        assert_eq!(
            history.publication_commits(&other, 10).unwrap(),
            (vec![p1.clone()], false)
        );
        assert_eq!(history.publication_commits(&p2, 1).unwrap(), (vec![p2.clone()], true));
        assert_eq!(
            history.directory(&p1, "dev/web/web").unwrap(),
            history.directory(&other, "dev/web/web").unwrap()
        );
        assert_ne!(
            history.directory(&p2, "dev/web/web").unwrap(),
            history.directory(&edit, "dev/web/web").unwrap()
        );
        assert_eq!(history.directory(&p1, "dev/missing").unwrap(), None);
        assert_eq!(
            history.recorded_inputs(&p2, "web/web").unwrap(),
            Some(BTreeMap::from([(
                "@input/web/web/image".to_owned(),
                "sha256:2".to_owned()
            )]))
        );
        assert_eq!(
            history.recorded_inputs(&p2, "web/other").unwrap(),
            Some(BTreeMap::new())
        );

        // The edit is inside the Application's directory and publishes nothing.
        assert_eq!(
            running_publication(
                &history,
                ApplicationTree {
                    path: "dev/web/web",
                    release: Some("web/web")
                },
                &edit,
                10
            )
            .unwrap(),
            RunningMatch::NoPublication
        );
        // Another target's commit leaves the first publication's directory as it was.
        let RunningMatch::Matched(matched) = running_publication(
            &history,
            ApplicationTree {
                path: "dev/web/web",
                release: Some("web/web"),
            },
            &other,
            10,
        )
        .unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, p1);
    }

    fn application(name: &str, path: &str, role: ApplicationRole) -> GeneratedApplication {
        GeneratedApplication {
            role,
            name: name.to_owned(),
            namespace: "argocd".to_owned(),
            path: path.to_owned(),
            project: String::new(),
            release: None,
            source: Value::Null,
        }
    }

    fn service(namespace: Option<&str>, name: &str) -> ResourceRef {
        ResourceRef {
            api_version: "v1".to_owned(),
            kind: "Service".to_owned(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    /// Catches a driver reading a resource the target does not render (the
    /// cluster would be asked about anything), a resource attributed to the
    /// Application of another directory (the wrong Application's revision
    /// would gate the read), and a manifest without a namespace not
    /// belonging to the namespace its Application deploys to.
    #[test]
    fn test_owning_application_names_the_directory_that_renders_the_resource() {
        let files = BTreeMap::from([
            (
                std::path::PathBuf::from("web/web/service.yaml"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: web, namespace: web}\n---\napiVersion: v1\nkind: Service\nmetadata: {name: bare}\n"
                    .to_vec(),
            ),
            (
                std::path::PathBuf::from("api/api/service.yaml"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: web, namespace: api}\n".to_vec(),
            ),
            (
                std::path::PathBuf::from("web/web/notes.txt"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: ignored, namespace: web}\n".to_vec(),
            ),
        ]);
        let applications = [
            application("catalog", "dev/_nyl/catalog", ApplicationRole::Catalog),
            application("web", "dev/web/web", ApplicationRole::Workload),
            application("api", "dev/api/api", ApplicationRole::Workload),
        ];
        let destinations = BTreeMap::from([
            (("argocd".to_owned(), "web".to_owned()), "web".to_owned()),
            (("argocd".to_owned(), "api".to_owned()), "api".to_owned()),
        ]);
        let owner = |resource: &ResourceRef| {
            owning_application_in(&files, "dev", &applications, &destinations, resource)
                .unwrap()
                .map(|application| application.name)
        };
        assert_eq!(owner(&service(Some("web"), "web")).as_deref(), Some("web"));
        assert_eq!(owner(&service(Some("api"), "web")).as_deref(), Some("api"));
        // No namespace in the manifest: the Application's destination namespace.
        assert_eq!(owner(&service(Some("web"), "bare")).as_deref(), Some("web"));
        assert_eq!(owner(&service(Some("api"), "bare")), None);
        // Not rendered: another name, kind, namespace, or a file that is no manifest.
        assert_eq!(owner(&service(Some("web"), "missing")), None);
        assert_eq!(owner(&service(Some("other"), "web")), None);
        assert_eq!(owner(&service(Some("web"), "ignored")), None);
        let mut deployment = service(Some("web"), "web");
        deployment.kind = "Deployment".to_owned();
        assert_eq!(owner(&deployment), None);
        // A resource declared without a namespace is one the manifest leaves
        // without, such as a cluster-scoped kind; a namespaced one never matches it.
        assert_eq!(owner(&service(None, "bare")).as_deref(), Some("web"));
        assert_eq!(owner(&service(None, "web")), None);
    }

    #[test]
    fn test_release_applications_map_names_to_release_keys() {
        let compilation = serde_json::json!({"releases": {
            "web/web": {"workload": {"application_name": "web-web"}},
            "web/api": {"workload": {"application_name": "web-api"}},
        }});
        let map = release_applications_of(Some(&compilation));
        assert_eq!(map["web-web"], "web/web");
        assert_eq!(map["web-api"], "web/api");
        assert!(release_applications_of(None).is_empty());
    }

    /// Pins the recorded format: it is persisted in desired documents.
    #[test]
    fn test_observation_facts_serialize_with_camel_case_keys() {
        let facts = ArgoCdObservation {
            location: ArgoCdLocation {
                instance: "prod".to_owned(),
                namespace: "argocd".to_owned(),
                cluster: "control".to_owned(),
                context: Some("control-admin".to_owned()),
                access: ArgoCDAccess::kubernetes(),
            },
            publication: ObservedPublication {
                url: "https://example.invalid/deploy.git".to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            applications: vec![GeneratedApplication {
                role: ApplicationRole::Workload,
                name: "web".to_owned(),
                namespace: "argocd".to_owned(),
                path: "dev/web/web".to_owned(),
                project: "default".to_owned(),
                release: Some("web/web".to_owned()),
                source: serde_json::json!({"repoURL": "https://example.invalid/deploy.git", "path": "dev/web/web"}),
            }],
        };
        let value = serde_json::to_value(&facts).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "instance": "prod", "namespace": "argocd", "cluster": "control", "context": "control-admin",
                "access": {"method": "kubernetes"},
                "publication": {"url": "https://example.invalid/deploy.git", "branch": "deploy", "pathPrefix": "dev"},
                "applications": [{"role": "workload", "name": "web", "namespace": "argocd", "path": "dev/web/web", "project": "default", "release": "web/web",
                                  "source": {"repoURL": "https://example.invalid/deploy.git", "path": "dev/web/web"}}],
            })
        );
        assert_eq!(serde_json::from_value::<ArgoCdObservation>(value).unwrap(), facts);
    }
}
