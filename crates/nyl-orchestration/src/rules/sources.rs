//! The source commit S, pinned Git sources, and reachability.
//!
//! Contract: [Source](../../../../design/orchestration-core.md#source),
//! [Units](../../../../design/orchestration-core.md#units) ("Git source
//! shape"), and [Pinned commits](../../../../design/orchestration-core.md#pinned-commits).
//!
//! - S is the entry worktree's commit when the Environment's `source` is
//!   omitted, the tip of `source.revision`, or `source.commit` when both are
//!   set; `--source` overrides it for one invocation, recorded as `override`.
//!   (`fromPromotion` is M6: [`source_request`] cannot express it, and the
//!   shell rejects it first with [`unsupported_source`].)
//! - S must be reachable from `protectedRefs` (default: the repository's
//!   default branch) unless `allowUnprotectedSource`; an override follows the
//!   same rule ([`run_source_refs`]). A locked commit (a unit source's or the
//!   Environment source's `revision` + `commit`) must be reachable from
//!   `protectedRefs` or its own revision, and `allowUnprotectedSource` never
//!   waives that ([`pin_refs`]).
//! - Each Git source field reads its own repository and pinned commit,
//!   exposed to drivers through source_roots. The main worktree stays at S.
//! - Resolution writes the resolved commit and the worktree-rooted path into
//!   every Git source field of a resolved spec; without `revision` a source
//!   is S.
//!
//! The shell performs the Git reads these rules ask for through
//! `SourceResolver`; the rules only decide.

use std::collections::BTreeMap;

use nyl_core::local_path::validate_local_path;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::LocalPathField;
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::SourceRepository;
use nyl_core::orchestration::state::SourceChoice;
use nyl_core::resources::environment::Environment;
use nyl_core::resources::GitSource;
use serde_json::Value;

/// How to find S.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRequest {
    /// The entry worktree's commit.
    Entry(CommitId),
    /// The tip of a revision of this repository.
    Revision(String),
    /// A commit, with the revision it must be reachable from.
    Commit { revision: String, commit: CommitId },
    /// `--source <rev>`: a revision or commit for this invocation only.
    Override(String),
}

impl SourceRequest {
    /// How the environment record names the choice.
    pub fn choice(&self) -> SourceChoice {
        match self {
            Self::Entry(_) => SourceChoice::Entry,
            Self::Revision(revision) => SourceChoice::Revision {
                revision: revision.clone(),
            },
            Self::Commit { revision, .. } => SourceChoice::Commit {
                revision: revision.clone(),
            },
            Self::Override(revision) => SourceChoice::Override {
                revision: revision.clone(),
            },
        }
    }
}

/// The request for S from the entry worktree's Environment, its commit, and
/// an optional `--source` override.
///
/// `source.fromPromotion` is M6: it yields a revision only where one is set
/// (validation excludes that), and otherwise the entry commit, so the shell
/// must reject it beforehand with [`unsupported_source`].
pub fn source_request(entry: &Environment, entry_commit: &CommitId, override_revision: Option<&str>) -> SourceRequest {
    if let Some(revision) = override_revision {
        return SourceRequest::Override(revision.to_owned());
    }
    let Some(source) = &entry.spec.source else {
        return SourceRequest::Entry(entry_commit.clone());
    };
    let Some(revision) = &source.git.revision else {
        return SourceRequest::Entry(entry_commit.clone());
    };
    match source.git.commit.as_deref().map(CommitId::new) {
        Some(Ok(commit)) => SourceRequest::Commit {
            revision: revision.clone(),
            commit,
        },
        // Environment validation rejects a malformed lock; without one the revision's tip is S.
        Some(Err(_)) | None => SourceRequest::Revision(revision.clone()),
    }
}

/// Refuse source forms the entry-repository resolver cannot represent.
pub fn unsupported_source(entry: &Environment) -> Option<Finding> {
    let source = entry.spec.source.as_ref()?;
    if source.git.is_remote() {
        return Some(Finding::new(
            Code::SourceUnavailable,
            format!(
                "Environment {} names a remote source repository, which is not supported yet",
                entry.metadata.name
            ),
            "remove spec.source.repository and repositoryRef, and use a revision or commit from the entry repository",
        ));
    }
    source.from_promotion.as_ref()?;
    Some(Finding::new(
        Code::SourceUnavailable,
        format!("Environment {} takes its source from a promotion, which is not supported yet", entry.metadata.name),
        "set spec.source.revision (optionally with commit), or omit spec.source to run from the entry worktree's commit",
    ))
}

/// The effective `protectedRefs`, defaulting to the repository's default branch.
fn protected_refs(entry: &Environment, default_branch: &str) -> Vec<String> {
    match &entry.spec.protected_refs {
        Some(refs) if !refs.is_empty() => refs.clone(),
        _ => vec![default_branch.to_owned()],
    }
}

/// The refs the run source S must be reachable from, or `None` when
/// `allowUnprotectedSource` waives the check. `entry` is the defaults-applied
/// entry Environment ([`nyl_core::resources::environment::effective_environment`]);
/// `default_branch` is the repository's default branch, the default of
/// `protectedRefs`. The waiver covers S only (including a `--source`
/// override), never pinned unit sources ([`pin_refs`]).
pub fn run_source_refs(entry: &Environment, default_branch: &str) -> Option<Vec<String>> {
    if entry.spec.allow_unprotected_source == Some(true) {
        return None;
    }
    Some(protected_refs(entry, default_branch))
}

/// The refs a pinned commit must be reachable from, whatever
/// `allowUnprotectedSource` says ("every commit Nyl pins must stay
/// fetchable"): the effective `protectedRefs` (default the repository's
/// default branch) plus `own_revision`, the revision a lock or an
/// Environment `source.revision`+`commit` names.
pub fn pin_refs(entry: &Environment, default_branch: &str, own_revision: Option<&str>) -> Vec<String> {
    let mut refs = protected_refs(entry, default_branch);
    if let Some(revision) = own_revision {
        if !refs.iter().any(|existing| existing == revision) {
            refs.push(revision.to_owned());
        }
    }
    refs
}

/// The finding for a commit that none of `refs` reaches: it names the
/// commit and the refs searched
/// ([`Code::SourceUnreachable`](nyl_core::orchestration::codes::Code::SourceUnreachable)).
pub fn unreachable(commit: &CommitId, refs: &[String]) -> Finding {
    Finding::new(
        Code::SourceUnreachable,
        format!("commit {commit} is not reachable from any of {}", refs.join(", ")),
        "push or merge the commit to a protected ref, or set allowUnprotectedSource: true on the Environment \
         (which only applies to the run's source commit, never to pinned sources)",
    )
}

/// A Git source field naming a revision, which the shell resolves (or, with
/// `commit`, checks for reachability) before [`pin_source_fields`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLock {
    /// The field's JSON Pointer, such as `/source`.
    pub field: String,
    pub repository: SourceRepository,
    pub revision: String,
    /// A locked commit (`revision` + `commit`): used as is once the shell has
    /// checked it is reachable from [`pin_refs`] with this revision.
    pub commit: Option<CommitId>,
}

/// The Git source at `field` of `spec`, or `None` when the field is absent.
pub fn git_source(spec: &Value, field: &str) -> Result<Option<GitSource>, String> {
    match spec.pointer(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("{field}: {error}")),
    }
}

fn locked_commit(field: &str, source: &GitSource) -> Result<Option<CommitId>, String> {
    source
        .commit
        .as_deref()
        .map(|commit| CommitId::new(commit).map_err(|error| format!("{field}/commit: {error}")))
        .transpose()
}

/// Replace named repository references with the declared read URL in a resolved spec.
pub fn resolve_repository_refs(
    spec: &mut Value,
    fields: &[String],
    repositories: &BTreeMap<String, String>,
) -> Result<(), String> {
    for field in fields {
        let Some(source) = git_source(spec, field)? else {
            continue;
        };
        let Some(reference) = source.repository_ref else {
            continue;
        };
        if source.repository.is_some() {
            return Err(format!("{field}: repository and repositoryRef are mutually exclusive"));
        }
        let url = repositories
            .get(&reference.name)
            .ok_or_else(|| format!("{field}: GitRepository {:?} is not declared", reference.name))?;
        let object = spec
            .pointer_mut(field)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("{field} must be an object"))?;
        object.remove("repositoryRef");
        object.insert("repository".to_owned(), serde_json::json!({"repoURL": url}));
    }
    Ok(())
}

/// The repository identity of a source; named references must first be resolved.
pub fn source_repository(source: &GitSource) -> Result<SourceRepository, String> {
    if source.repository_ref.is_some() {
        return Err("resolve repositoryRef to a declared GitRepository before choosing a source".to_owned());
    }
    Ok(source.repository.as_ref().map_or(SourceRepository::This, |repository| {
        SourceRepository::Url(repository.repo_url.clone())
    }))
}

/// Each resolved source field's repository and pinned commit.
pub fn source_checkouts(
    spec: &Value,
    fields: &[String],
    source_commit: &CommitId,
) -> Result<BTreeMap<String, (SourceRepository, CommitId)>, String> {
    let mut result = BTreeMap::new();
    for field in fields {
        let Some(source) = git_source(spec, field)? else {
            continue;
        };
        result.insert(
            field.clone(),
            (
                source_repository(&source)?,
                locked_commit(field, &source)?.unwrap_or_else(|| source_commit.clone()),
            ),
        );
    }
    Ok(result)
}

/// The revisions and locks the Git source fields of `spec` name. `fields`
/// are the driver's source_fields; remote sources require their own revision.
pub fn source_revisions(spec: &Value, fields: &[String]) -> Result<Vec<SourceLock>, String> {
    let mut locks = Vec::new();
    for field in fields {
        let Some(source) = git_source(spec, field)? else {
            continue;
        };
        let repository = source_repository(&source)?;
        if source.is_remote() && source.revision.is_none() {
            return Err(format!("{field}: revision is required for a remote source"));
        }
        let Some(revision) = source.revision.as_ref().or(source.commit.as_ref()) else {
            continue;
        };
        locks.push(SourceLock {
            field: field.clone(),
            repository,
            revision: revision.clone(),
            commit: locked_commit(field, &source)?,
        });
    }
    Ok(locks)
}

/// Write each Git source field's resolved `commit` and its worktree-rooted
/// `path`, resolved against `project_dir` by the local path rule, into
/// `resolved_spec`. The commit is S without `revision`, the lock's `commit`
/// when one is set, else the revision's tip from `revisions`.
pub fn pin_source_fields(
    resolved_spec: &mut Value,
    fields: &[String],
    project_dir: &str,
    source_commit: &CommitId,
    revisions: &BTreeMap<(SourceRepository, String), CommitId>,
) -> Result<(), String> {
    for field in fields {
        let Some(source) = git_source(resolved_spec, field)? else {
            continue;
        };
        let repository = source_repository(&source)?;
        if source.is_remote() && source.revision.is_none() {
            return Err(format!("{field}: revision is required for a remote source"));
        }
        let commit = match (&source.revision, locked_commit(field, &source)?) {
            (_, Some(locked)) => locked,
            (None, None) => source_commit.clone(),
            (Some(revision), None) => revisions
                .get(&(repository, revision.clone()))
                .cloned()
                .ok_or_else(|| format!("{field}: revision {revision:?} was not resolved"))?,
        };
        let path = source
            .path
            .as_deref()
            .map(|path| {
                worktree_path(
                    &format!("{field}/path"),
                    if source.is_remote() { "/" } else { project_dir },
                    path,
                )
            })
            .transpose()?;
        let object = resolved_spec
            .pointer_mut(field)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("{field} must be an object"))?;
        object.insert("commit".to_owned(), Value::String(commit.to_string()));
        if let Some(path) = path {
            object.insert("path".to_owned(), Value::String(path));
        }
    }
    Ok(())
}

/// Rewrite each local path field of `resolved_spec` (a string, or an array
/// of glob strings) into its worktree-rooted form by the same rule as Git
/// source paths, and record an absent field that defaults to the `nyl.toml`
/// directory as that directory. The desired document and the execution key
/// then mean the same wherever `nyl.toml` lives.
pub fn root_local_paths(resolved_spec: &mut Value, fields: &[LocalPathField], project_dir: &str) -> Result<(), String> {
    for field in fields {
        let name = format!("spec{}", field.pointer.replace('/', "."));
        let rooted = match resolved_spec.pointer(&field.pointer) {
            None | Some(Value::Null) if field.defaults_to_project_dir => {
                Value::String(format!("/{}", project_segments(project_dir).join("/")))
            }
            None | Some(Value::Null) => continue,
            Some(Value::String(path)) => Value::String(worktree_path(&name, project_dir, path)?),
            Some(Value::Array(paths)) => Value::Array(
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| {
                        let path = path
                            .as_str()
                            .ok_or_else(|| format!("{name}[{index}] must be a string"))?;
                        worktree_path(&format!("{name}[{index}]"), project_dir, path).map(Value::String)
                    })
                    .collect::<Result<_, String>>()?,
            ),
            Some(_) => return Err(format!("{name} must be a string or a list of strings")),
        };
        let (parent, key) = field
            .pointer
            .rsplit_once('/')
            .ok_or_else(|| format!("{} is not a JSON Pointer", field.pointer))?;
        let object = if parent.is_empty() {
            resolved_spec.as_object_mut()
        } else {
            resolved_spec.pointer_mut(parent).and_then(Value::as_object_mut)
        }
        .ok_or_else(|| format!("{name}: its parent must be an object"))?;
        object.insert(key.to_owned(), rooted);
    }
    Ok(())
}

/// The segments of the worktree-rooted `project_dir`; `/` joins them into
/// its normalized form, `/` or `/a/b`.
fn project_segments(project_dir: &str) -> Vec<&str> {
    project_dir.split('/').filter(|segment| !segment.is_empty()).collect()
}

/// A local path in its worktree-rooted form: relative paths resolve against
/// the worktree-rooted `project_dir`, `/` names the worktree root, and the
/// result never leaves the worktree.
fn worktree_path(field: &str, project_dir: &str, path: &str) -> Result<String, String> {
    if path == "/" {
        return Ok(path.to_owned());
    }
    validate_local_path(field, path).map_err(|error| error.to_string())?;
    if path.starts_with('/') {
        return Ok(path.to_owned());
    }
    let mut segments = project_segments(project_dir);
    for segment in path.split('/') {
        if segment == ".." {
            if segments.pop().is_none() {
                return Err(format!(
                    "{field} {path:?} leaves the Git worktree from the nyl.toml directory {project_dir}"
                ));
            }
        } else {
            segments.push(segment);
        }
    }
    Ok(format!("/{}", segments.join("/")))
}

/// The main worktree remains at the environment's source commit. Every Git
/// source field has an independent checkout in the driver's source_roots.
pub fn effective_commit(
    resolved_spec: &Value,
    fields: &[String],
    source_commit: &CommitId,
) -> Result<CommitId, String> {
    source_checkouts(resolved_spec, fields, source_commit)?;
    Ok(source_commit.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn commit(digit: char) -> CommitId {
        CommitId::new(digit.to_string().repeat(40)).unwrap()
    }

    fn command_paths() -> Vec<LocalPathField> {
        vec![
            LocalPathField {
                pointer: "/workingDir".to_owned(),
                defaults_to_project_dir: true,
            },
            LocalPathField {
                pointer: "/files".to_owned(),
                defaults_to_project_dir: false,
            },
        ]
    }

    /// Catches a Command's execution key changing, and the unit running
    /// again, when only the `nyl.toml` location moves while its working
    /// directory and files stay the same.
    #[test]
    fn test_root_local_paths_keep_the_key_when_nyl_toml_moves() {
        let key = |spec: Value, project_dir: &str| {
            let mut spec = spec;
            root_local_paths(&mut spec, &command_paths(), project_dir).unwrap();
            let key = crate::rules::execution_key::execution_key(&crate::rules::execution_key::KeyMaterial {
                api_version: "units.gitops.nyl/v1",
                kind: "Command",
                behavior_version: 1,
                resolved_spec: &spec,
                source_fields: &[],
                inputs: &nyl_core::orchestration::driver::KeyInputs::default(),
            })
            .unwrap();
            (spec, key)
        };
        let (at_nyl, key_at_nyl) = key(
            json!({"command": ["./run.sh"], "workingDir": "../scripts/seed", "files": ["../scripts/seed/**", "/tools/*.sh"]}),
            "/nyl",
        );
        let (moved, key_moved) = key(
            json!({"command": ["./run.sh"], "workingDir": "../../scripts/seed", "files": ["../../scripts/seed/**", "/tools/*.sh"]}),
            "/deploy/nyl",
        );
        assert_eq!(at_nyl["workingDir"], "/scripts/seed");
        assert_eq!(at_nyl["files"], json!(["/scripts/seed/**", "/tools/*.sh"]));
        assert_eq!(at_nyl, moved);
        assert_eq!(key_at_nyl, key_moved);
    }

    /// Catches an omitted working directory staying implicit, which would
    /// make the desired document depend on where `nyl.toml` lives.
    #[test]
    fn test_root_local_paths_records_the_default_working_directory() {
        let mut spec = json!({"command": ["./run.sh"]});
        root_local_paths(&mut spec, &command_paths(), "/nyl").unwrap();
        assert_eq!(spec, json!({"command": ["./run.sh"], "workingDir": "/nyl"}));
        let mut at_root = json!({"command": ["./run.sh"], "workingDir": "/"});
        root_local_paths(&mut at_root, &command_paths(), "/").unwrap();
        assert_eq!(at_root["workingDir"], "/");
        let mut escaping = json!({"files": ["../../x"]});
        assert!(root_local_paths(&mut escaping, &command_paths(), "/nyl").is_err());
    }

    fn environment(spec: &Value) -> Environment {
        serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Environment",
            "metadata": {"name": "dev"},
            "spec": spec
        }))
        .unwrap()
    }

    fn fields(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Catches a run choosing the wrong S, or recording the wrong choice in
    /// the environment record, for any `source` form or an override.
    #[test]
    fn test_source_request_chooses_s_for_every_form_and_an_override() {
        let entry = commit('e');
        let lock = "c".repeat(40);
        let cases = [
            (
                json!({}),
                None,
                SourceRequest::Entry(entry.clone()),
                SourceChoice::Entry,
            ),
            (
                json!({"source": {"revision": "release/prod"}}),
                None,
                SourceRequest::Revision("release/prod".to_owned()),
                SourceChoice::Revision {
                    revision: "release/prod".to_owned(),
                },
            ),
            (
                json!({"source": {"revision": "release/prod", "commit": lock}}),
                None,
                SourceRequest::Commit {
                    revision: "release/prod".to_owned(),
                    commit: commit('c'),
                },
                SourceChoice::Commit {
                    revision: "release/prod".to_owned(),
                },
            ),
            (
                json!({"source": {"revision": "release/prod", "commit": lock}}),
                Some("hotfix"),
                SourceRequest::Override("hotfix".to_owned()),
                SourceChoice::Override {
                    revision: "hotfix".to_owned(),
                },
            ),
        ];
        for (spec, override_revision, request, choice) in cases {
            let actual = source_request(&environment(&spec), &entry, override_revision);
            assert_eq!(actual, request, "{spec}");
            assert_eq!(actual.choice(), choice, "{spec}");
        }
    }

    /// Catches a promoted source silently running from the entry commit.
    #[test]
    fn test_promoted_source_is_reported_unsupported() {
        let promoted = environment(&json!({"source": {"fromPromotion": {"path": "dev-to-prod"}}}));
        assert_eq!(unsupported_source(&promoted).unwrap().code, Code::SourceUnavailable);
        assert!(unsupported_source(&environment(&json!({"source": {"revision": "main"}}))).is_none());
    }

    #[test]
    fn test_remote_environment_source_is_not_resolved_from_entry_repository() {
        for source in [
            json!({"repositoryRef": {"name": "other"}, "revision": "main"}),
            json!({"repository": {"repoURL": "https://example.com/other.git"}, "revision": "main"}),
        ] {
            let finding = unsupported_source(&environment(&json!({"source": source}))).unwrap();
            assert_eq!(finding.code, Code::SourceUnavailable);
            assert!(finding.fix.contains("entry repository"));
        }
    }

    /// Catches `allowUnprotectedSource` leaking into pinned commits, which
    /// would let a lock name a commit that later disappears.
    #[test]
    fn test_waiver_covers_the_run_source_but_never_pinned_commits() {
        let waived = environment(&json!({"allowUnprotectedSource": true}));
        assert_eq!(run_source_refs(&waived, "main"), None);
        assert_eq!(pin_refs(&waived, "main", Some("deploy")), ["main", "deploy"]);

        let protected = environment(&json!({"protectedRefs": ["main", "release/*"]}));
        assert_eq!(
            run_source_refs(&protected, "trunk"),
            Some(fields(&["main", "release/*"]))
        );
        assert_eq!(
            pin_refs(&protected, "trunk", Some("deploy")),
            ["main", "release/*", "deploy"]
        );
        let empty = environment(&json!({"protectedRefs": []}));
        assert_eq!(run_source_refs(&empty, "trunk"), Some(fields(&["trunk"])));
        assert_eq!(pin_refs(&empty, "trunk", None), ["trunk"]);
    }

    /// Catches an unreachable-commit error that does not tell the user which
    /// commit and refs were checked.
    #[test]
    fn test_unreachable_names_the_commit_and_every_ref() {
        let finding = unreachable(&commit('d'), &fields(&["main", "deploy"]));
        assert_eq!(finding.code, Code::SourceUnreachable);
        assert!(finding.message.contains(&"d".repeat(40)));
        assert!(finding.message.contains("main") && finding.message.contains("deploy"));
    }

    /// Catches a lock being overridden by the revision tip (which would
    /// silently move a reviewed pin) and paths recorded relative to
    /// `nyl.toml`, which change meaning when the project moves.
    #[test]
    fn test_pinning_uses_locks_tips_and_s_and_roots_paths_at_the_worktree() {
        let lock = "c".repeat(40);
        let spec = json!({
            "source": {"path": "../infra/network"},
            "locked": {"path": "/infra/db", "revision": "deploy", "commit": lock},
            "tip": {"path": "modules", "revision": "main"}
        });
        let field_names = fields(&["/source", "/locked", "/tip", "/absent"]);
        let locks = source_revisions(&spec, &field_names).unwrap();
        assert_eq!(
            locks,
            [
                SourceLock {
                    field: "/locked".to_owned(),
                    repository: SourceRepository::This,
                    revision: "deploy".to_owned(),
                    commit: Some(commit('c')),
                },
                SourceLock {
                    field: "/tip".to_owned(),
                    repository: SourceRepository::This,
                    revision: "main".to_owned(),
                    commit: None,
                },
            ]
        );
        let revisions = BTreeMap::from([
            ((SourceRepository::This, "deploy".to_owned()), commit('f')),
            ((SourceRepository::This, "main".to_owned()), commit('b')),
        ]);
        let mut resolved = spec.clone();
        pin_source_fields(&mut resolved, &field_names, "/nyl", &commit('a'), &revisions).unwrap();
        assert_eq!(
            resolved["source"],
            json!({"path": "/infra/network", "commit": "a".repeat(40)})
        );
        assert_eq!(resolved["locked"]["commit"], json!(lock));
        assert_eq!(resolved["locked"]["path"], json!("/infra/db"));
        assert_eq!(resolved["tip"]["commit"], json!("b".repeat(40)));
        assert_eq!(resolved["tip"]["path"], json!("/nyl/modules"));
    }

    /// Catches a source path escaping the repository checkout.
    #[test]
    fn test_pinning_rejects_paths_that_leave_the_worktree() {
        let mut spec = json!({"source": {"path": "../../outside"}});
        let error =
            pin_source_fields(&mut spec, &fields(&["/source"]), "/nyl", &commit('a'), &BTreeMap::new()).unwrap_err();
        assert!(error.contains("leaves the Git worktree"), "{error}");
        let mut root = json!({"source": {"path": "../outside"}});
        assert!(pin_source_fields(&mut root, &fields(&["/source"]), "/", &commit('a'), &BTreeMap::new()).is_err());
    }

    #[test]
    fn test_remote_sources_resolve_declared_repository_and_root_paths_at_that_checkout() {
        let mut spec =
            json!({"source": {"repositoryRef": {"name": "platform"}, "revision": "main", "path": "modules/network"}});
        let fields = fields(&["/source"]);
        assert!(resolve_repository_refs(&mut spec, &fields, &BTreeMap::new()).is_err());
        resolve_repository_refs(
            &mut spec,
            &fields,
            &BTreeMap::from([("platform".to_owned(), "https://example.com/platform.git".to_owned())]),
        )
        .unwrap();
        let repository = SourceRepository::Url("https://example.com/platform.git".to_owned());
        let locks = source_revisions(&spec, &fields).unwrap();
        assert_eq!(locks[0].repository, repository);
        let revisions = BTreeMap::from([((repository.clone(), "main".to_owned()), commit('b'))]);
        pin_source_fields(&mut spec, &fields, "/nyl", &commit('a'), &revisions).unwrap();
        assert_eq!(spec["source"]["path"], "/modules/network");
        assert_eq!(
            source_checkouts(&spec, &fields, &commit('a')).unwrap()["/source"],
            (repository, commit('b'))
        );
        spec["source"].as_object_mut().unwrap().remove("revision");
        assert!(source_revisions(&spec, &fields)
            .unwrap_err()
            .contains("revision is required"));
    }

    #[test]
    fn test_commit_without_revision_stays_pinned_and_requires_reachability() {
        let mut spec = json!({"source": {"path": "../infra", "commit": "b".repeat(40)}});
        let fields = fields(&["/source"]);
        let locks = source_revisions(&spec, &fields).unwrap();
        assert_eq!(locks[0].commit, Some(commit('b')));
        assert_eq!(locks[0].revision, "b".repeat(40));
        pin_source_fields(&mut spec, &fields, "/nyl", &commit('a'), &BTreeMap::new()).unwrap();
        assert_eq!(spec["source"]["commit"], "b".repeat(40));
    }

    #[test]
    fn test_source_fields_keep_independent_commits_and_main_worktree_stays_at_s() {
        let spec = json!({"a": {"commit": "c".repeat(40)}, "b": {"commit": "d".repeat(40)}});
        let fields = fields(&["/a", "/b"]);
        let roots = source_checkouts(&spec, &fields, &commit('a')).unwrap();
        assert_eq!(roots["/a"], (SourceRepository::This, commit('c')));
        assert_eq!(roots["/b"], (SourceRepository::This, commit('d')));
        assert_eq!(effective_commit(&spec, &fields, &commit('a')).unwrap(), commit('a'));
    }
}
