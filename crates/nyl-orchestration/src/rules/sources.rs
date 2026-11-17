//! The source commit S, pinned Git sources, and reachability.
//!
//! Contract: [Source](../../../../design/orchestration-core.md#source),
//! [Units](../../../../design/orchestration-core.md#units) ("Git source
//! shape"), and [Pinned commits](../../../../design/orchestration-core.md#pinned-commits).
//!
//! - S is the entry worktree's commit when the Environment's `source` is
//!   omitted, the tip of `source.revision`, or `source.commit` when both are
//!   set; `--source` overrides it for one invocation, recorded as `override`.
//!   A `source.fromPromotion` Environment takes S from the newest
//!   PromotionRecord on its path or paths ([`promoted_record`]), recorded as
//!   `sourceChosenBy: promotion` with the record's `sequence`; a path with no
//!   record yet is `NYL-PROMOTION-MISSING`.
//! - With `record: source` the record of the Environment's path is the
//!   `source.fromPromotion.promoted` block of the Environment as the entry
//!   worktree declares it, in place of any record in state
//!   ([`promotion_records`]); every reader takes the records through this
//!   module. A block with a lower `sequence` than the one a run applied is
//!   `NYL-PROMOTION-SEQUENCE-OLDER`, so an older checkout never rolls the
//!   environment back: rolling back is a new promotion. Readers of state
//!   that do not run anything see the block the last run applied, and a
//!   newer one in the entry worktree as not applied yet ([`recorded_promotions`]).
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
use nyl_core::orchestration::ids::{CommitId, PromotionPathName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, SourceRepository};
use nyl_core::orchestration::promotion::PromotionRecord;
use nyl_core::orchestration::state::SourceChoice;
use nyl_core::resources::environment::{Environment, PromotedSource};
use nyl_core::resources::GitSource;
use serde_json::Value;

/// How to find S.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRequest {
    /// An explicitly captured immutable snapshot of working files.
    Worktree(nyl_core::orchestration::ports::WorktreeSnapshot),
    /// The entry worktree's commit.
    Entry(CommitId),
    /// An instance's recorded source commit.
    Recorded(CommitId),
    /// The tip of a revision of this repository.
    Revision(String),
    /// A commit, with the revision it must be reachable from.
    Commit { revision: String, commit: CommitId },
    /// `--source <rev>`: a revision or commit for this invocation only.
    Override(String),
    /// The source commit of the newest PromotionRecord on the Environment's
    /// promotion path or paths.
    Promotion {
        path: PromotionPathName,
        sequence: u64,
        commit: CommitId,
    },
}

impl SourceRequest {
    /// How the environment record names the choice.
    pub fn choice(&self) -> SourceChoice {
        match self {
            Self::Worktree(snapshot) => SourceChoice::Override {
                revision: snapshot.revision.clone(),
            },
            Self::Entry(_) => SourceChoice::Entry,
            Self::Recorded(_) => SourceChoice::Recorded,
            Self::Revision(revision) => SourceChoice::Revision {
                revision: revision.clone(),
            },
            Self::Commit { revision, .. } => SourceChoice::Commit {
                revision: revision.clone(),
            },
            Self::Override(revision) => SourceChoice::Override {
                revision: revision.clone(),
            },
            Self::Promotion { path, sequence, .. } => SourceChoice::Promotion {
                path: path.to_string(),
                sequence: *sequence,
            },
        }
    }
}

/// The request for S from the entry worktree's Environment, its commit (an
/// instance's recorded commit when `recorded`), an optional `--source`
/// override, and the PromotionRecords of the environment's desired state.
///
/// A `source.fromPromotion` Environment takes S from [`promoted_record`]; the
/// override still wins for one invocation.
pub fn source_request(
    entry: &Environment,
    entry_commit: &CommitId,
    override_revision: Option<&str>,
    recorded: bool,
    promotions: &BTreeMap<PromotionPathName, PromotionRecord>,
) -> Result<SourceRequest, Finding> {
    if let Some(revision) = override_revision {
        return Ok(SourceRequest::Override(revision.to_owned()));
    }
    // Without a source of its own, an instance not entered from its own
    // checkout renders at the commit its record names.
    let without_source = || {
        if recorded {
            SourceRequest::Recorded(entry_commit.clone())
        } else {
            SourceRequest::Entry(entry_commit.clone())
        }
    };
    let Some(source) = &entry.spec.source else {
        return Ok(without_source());
    };
    if let Some(promoted) = &source.from_promotion {
        let record = promoted_record(&entry.metadata.name, promoted, promotions)?;
        return Ok(SourceRequest::Promotion {
            path: record.path.clone(),
            sequence: record.sequence,
            commit: record
                .source_commit
                .clone()
                .expect("promoted_record returns records with a source commit"),
        });
    }
    let Some(revision) = &source.git.revision else {
        return Ok(without_source());
    };
    Ok(match source.git.commit.as_deref().map(CommitId::new) {
        Some(Ok(commit)) => SourceRequest::Commit {
            revision: revision.clone(),
            commit,
        },
        // Environment validation rejects a malformed lock; without one the revision's tip is S.
        Some(Err(_)) | None => SourceRequest::Revision(revision.clone()),
    })
}

/// The record that supplies a promoted source: the newest, by `sequence`,
/// on `paths` that carries a source commit. Every reader that asks which
/// record supplies the source takes it here.
pub fn source_supplier<'a, P: AsRef<str>>(
    records: impl IntoIterator<Item = &'a PromotionRecord>,
    paths: &[P],
) -> Option<&'a PromotionRecord> {
    records
        .into_iter()
        .filter(|record| {
            record.source_commit.is_some() && paths.iter().any(|path| path.as_ref() == record.path.as_str())
        })
        .max_by_key(|record| record.sequence)
}

/// The newest record, by `sequence`, on the paths `promoted` names that
/// carries a source commit ([`source_supplier`]). None is
/// `NYL-PROMOTION-MISSING`, whose fix names `nyl promote <path>`.
pub fn promoted_record<'a>(
    environment: &str,
    promoted: &nyl_core::resources::environment::PromotedSource,
    promotions: &'a BTreeMap<PromotionPathName, PromotionRecord>,
) -> Result<&'a PromotionRecord, Finding> {
    let paths: Vec<&str> = promoted
        .path
        .iter()
        .map(String::as_str)
        .chain(promoted.paths.iter().map(String::as_str))
        .collect();
    source_supplier(promotions.values(), &paths).ok_or_else(|| {
        Finding::new(
            Code::PromotionMissing,
            format!(
                "Environment {environment} takes its source from promotion path {}, which has no record yet",
                paths.join(", ")
            ),
            format!(
                "promote a state into it with `nyl promote {}`",
                paths.first().copied().unwrap_or_default()
            ),
        )
    })
}

/// The record a `fromPromotion` binding reads among an environment's
/// `records`, or `None` when none supplies it yet. Every reader of a
/// binding takes it here: the binding provider of the shared resolver and
/// `nyl get promotions`.
///
/// - With `path`, that path's record, when it carries the value.
/// - Without `path`, in an environment whose source is promoted along
///   `source_paths`: the record that supplies the source commit (the newest
///   on those paths that carries one, as [`promoted_record`] chooses it), so
///   the source commit and the values proven with it move together. A value
///   it does not carry is not taken from an older record of another path.
/// - Without `path` otherwise, the newest record that carries the value.
pub fn binding_supplier<'a>(
    records: &'a BTreeMap<PromotionPathName, PromotionRecord>,
    source_paths: Option<&std::collections::BTreeSet<String>>,
    binding: &nyl_core::bindings::PromotionInputSource,
) -> Option<&'a PromotionRecord> {
    let carries = |record: &&PromotionRecord| record.values.contains_key(&binding.value);
    if let Some(path) = &binding.path {
        return records
            .values()
            .filter(|record| record.path.as_str() == path)
            .filter(carries)
            .max_by_key(|record| record.sequence);
    }
    if let Some(paths) = source_paths {
        let paths: Vec<&String> = paths.iter().collect();
        return source_supplier(records.values(), &paths).filter(carries);
    }
    records.values().filter(carries).max_by_key(|record| record.sequence)
}

/// The promotion paths that supply an environment's source, from its
/// Environment spec (the declared one or the one an environment record
/// keeps): `None` unless `source.fromPromotion` is set. A `fromPromotion`
/// binding without `path` reads only records on these paths, so every
/// resolution of references, run, `get`, or direct command, derives them
/// here.
pub fn promoted_source_paths(environment_spec: &Value) -> Option<std::collections::BTreeSet<String>> {
    let promoted = environment_spec.pointer("/source/fromPromotion")?;
    let single = promoted.get("path").and_then(Value::as_str).map(str::to_owned);
    let several = promoted
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned);
    Some(single.into_iter().chain(several).collect())
}

/// Refuse source forms the entry-repository resolver cannot represent.
pub fn unsupported_source(entry: &Environment) -> Option<Finding> {
    let source = entry.spec.source.as_ref()?;
    source.git.is_remote().then(|| {
        Finding::new(
            Code::SourceUnavailable,
            format!(
                "Environment {} names a remote source repository, which is not supported yet",
                entry.metadata.name
            ),
            "remove spec.source.repository and repositoryRef, and use a revision or commit from the entry repository",
        )
    })
}

/// The `source.fromPromotion` of an Environment that keeps its record in its
/// source (`record: source`).
pub fn record_in_source(promoted: Option<&PromotedSource>) -> Option<&PromotedSource> {
    promoted.filter(|promoted| promoted.record.as_deref() == Some("source"))
}

/// The `source.fromPromotion` of an Environment spec as JSON, such as the
/// one an environment record keeps.
pub fn promoted_source(environment_spec: &Value) -> Option<PromotedSource> {
    environment_spec
        .pointer("/source/fromPromotion")
        .and_then(|promoted| serde_json::from_value(promoted.clone()).ok())
}

/// What an environment's runs applied, as its environment record keeps it:
/// the highest promotion sequence, and the `record: source` block the last
/// run applied (the record's `spec.source.fromPromotion.promoted`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub sequence: Option<u64>,
    pub block: Option<PromotionRecord>,
}

impl Applied {
    /// What `snapshot`'s environment record says was applied.
    pub fn of(snapshot: &EnvironmentSnapshot) -> Self {
        let Some(record) = snapshot.environment.as_ref() else {
            return Self::default();
        };
        Self {
            sequence: record.applied_promotion_sequence,
            block: record_in_source(promoted_source(&record.spec).as_ref())
                .and_then(|promoted| promoted.promoted.clone()),
        }
    }
}

/// The records [`promotion_records`] reads, and the `record: source` block
/// that must still be checked against the source state it names: none when
/// there is no block, or when it is the block a run already applied (which
/// was checked then).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceRecords {
    pub records: BTreeMap<PromotionPathName, PromotionRecord>,
    pub check: Option<PromotionRecord>,
}

/// The finding for a `record: source` block that cannot be the record.
pub fn invalid_block(environment: &str, problem: impl std::fmt::Display) -> Finding {
    Finding::new(
        Code::PromotionRecordInvalid,
        format!("the promoted block of Environment {environment} {problem}"),
        "restore the block `nyl promote` wrote, or promote again; the block is written by `nyl promote`, never by hand",
    )
}

/// The PromotionRecords an environment's source and its `fromPromotion`
/// bindings read: the records in its state, and with `record: source` the
/// `promoted` block of `promoted` (the Environment's `source.fromPromotion`)
/// in place of any record in state on the paths it names, which never
/// count. No block is no record, which [`promoted_record`] reports as
/// `NYL-PROMOTION-MISSING`.
///
/// The block must name a path of `promoted`, an environment source state,
/// and a source commit (`NYL-PROMOTION-RECORD-INVALID`). Against `applied`:
/// a block below the applied sequence is `NYL-PROMOTION-SEQUENCE-OLDER`, so
/// an older checkout or a reverted promotion commit never rolls the
/// environment back (rolling back is a new promotion, whose fix this names);
/// a block at the applied sequence must be the applied block, because a
/// sequence is never reused, such as by another branch's promotion
/// (`NYL-PROMOTION-RECORD-INVALID`), and needs no new check; a newer block
/// is returned in [`SourceRecords::check`].
pub fn promotion_records(
    environment: &str,
    promoted: Option<&PromotedSource>,
    state: &BTreeMap<PromotionPathName, PromotionRecord>,
    applied: &Applied,
) -> Result<SourceRecords, Finding> {
    let Some(promoted) = record_in_source(promoted) else {
        return Ok(SourceRecords {
            records: state.clone(),
            check: None,
        });
    };
    let paths: Vec<&str> = promoted
        .path
        .iter()
        .chain(&promoted.paths)
        .map(String::as_str)
        .collect();
    let mut records: BTreeMap<_, _> = state
        .iter()
        .filter(|(name, _)| !paths.contains(&name.as_str()))
        .map(|(name, record)| (name.clone(), record.clone()))
        .collect();
    let Some(block) = &promoted.promoted else {
        return Ok(SourceRecords { records, check: None });
    };
    let invalid = |problem: String| invalid_block(environment, problem);
    if !paths.contains(&block.path.as_str()) {
        return Err(invalid(format!(
            "names path {}, which is not among the paths its source is promoted along ({})",
            block.path,
            paths.join(", ")
        )));
    }
    let Some(source_commit) = &block.source_commit else {
        return Err(invalid("carries no source commit".to_owned()));
    };
    if !matches!(
        block.from,
        nyl_core::orchestration::promotion::PromotionFrom::Environment(_)
    ) {
        return Err(invalid(
            "names a target as its source state, which M6 does not promote".to_owned(),
        ));
    }
    let check = match applied.sequence {
        Some(sequence) if block.sequence < sequence => {
            return Err(Finding::new(
                Code::PromotionSequenceOlder,
                format!(
                    "the promoted block of Environment {environment} has sequence {}, below sequence {sequence} that a run already applied: this checkout is older than what the environment runs",
                    block.sequence
                ),
                format!(
                    "run from the current branch tip; to roll back, promote the older state as a new promotion: `nyl promote {} --revision {source_commit}`",
                    block.path
                ),
            ));
        }
        Some(sequence) if block.sequence == sequence => {
            if applied.block.as_ref() != Some(block) {
                return Err(invalid(format!(
                    "has sequence {sequence}, which a run already applied with another record; a sequence is never reused, such as by a promotion on another branch"
                )));
            }
            None
        }
        _ => Some(block.clone()),
    };
    records.insert(block.path.clone(), block.clone());
    Ok(SourceRecords { records, check })
}

/// The records a reader of `snapshot` that runs nothing shows (`status`,
/// `nyl get promotions`, `promotion-candidates`, `nyl get` value forms): the
/// block the environment record keeps, which its last run applied, and then
/// `entry`'s block (the Environment as the entry worktree declares it) when
/// that one is newer, promoted but not applied yet. An `entry` that keeps
/// its record in state reads state alone, even when the last run applied a
/// block. Never refuses and never checks a block against its state: a block
/// that is invalid or older is left out here, and a run refuses it. This is
/// a deliberate difference from the runs' [`promotion_records`]: these
/// readers show what is recorded, and change nothing.
pub fn recorded_promotions(
    snapshot: &EnvironmentSnapshot,
    entry: Option<&PromotedSource>,
) -> BTreeMap<PromotionPathName, PromotionRecord> {
    if entry.is_some() && record_in_source(entry).is_none() {
        return snapshot.promotions.clone();
    }
    let environment = snapshot
        .environment
        .as_ref()
        .map(|record| record.environment.to_string())
        .unwrap_or_default();
    let applied = snapshot
        .environment
        .as_ref()
        .and_then(|record| promoted_source(&record.spec));
    let mut records = promotion_records(
        &environment,
        applied.as_ref(),
        &snapshot.promotions,
        &Applied::default(),
    )
    .map_or_else(|_| snapshot.promotions.clone(), |read| read.records);
    if let Some(block) = record_in_source(entry).and_then(|entry| {
        promotion_records(&environment, Some(entry), &BTreeMap::new(), &Applied::default())
            .ok()?
            .records
            .into_values()
            .next()
    }) {
        if records
            .get(&block.path)
            .is_none_or(|current| current.sequence < block.sequence)
        {
            records.insert(block.path.clone(), block);
        }
    }
    records
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

/// Each resolved source field's repository and commit.
///
/// A field is checked out at its pinned commit, except one that only followed
/// the unit's own source commit: no `revision`, this repository, and a pin
/// equal to `recorded`, the unit's recorded source commit. It follows
/// `source_commit` instead, so a teardown with `--source <revision>` (whose
/// job commit differs from the recorded one) takes that revision's files for
/// it as for every other file of the unit, while the stored spec, and with it
/// every digest and approval, stays unchanged. Explicit `revision` and
/// `commit` locks, and other repositories, are never replaced. See
/// [Teardown](../../../../design/orchestration-core.md#deletion).
pub fn source_checkouts(
    spec: &Value,
    fields: &[String],
    source_commit: &CommitId,
    recorded: Option<&CommitId>,
) -> Result<BTreeMap<String, (SourceRepository, CommitId)>, String> {
    let mut result = BTreeMap::new();
    for field in fields {
        let Some(source) = git_source(spec, field)? else {
            continue;
        };
        let repository = source_repository(&source)?;
        let commit = match locked_commit(field, &source)? {
            Some(locked)
                if source.revision.is_none() && repository == SourceRepository::This && Some(&locked) == recorded =>
            {
                source_commit.clone()
            }
            Some(locked) => locked,
            None => source_commit.clone(),
        };
        result.insert(field.clone(), (repository, commit));
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
    source_checkouts(resolved_spec, fields, source_commit, None)?;
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
            let actual =
                source_request(&environment(&spec), &entry, override_revision, false, &BTreeMap::new()).unwrap();
            assert_eq!(actual, request, "{spec}");
            assert_eq!(actual.choice(), choice, "{spec}");
        }
    }

    /// Catches an instance not entered from its own checkout following the
    /// checkout: it renders at its record's commit and records that choice,
    /// unless its template names a revision or the run overrides the source.
    #[test]
    fn test_a_recorded_source_is_used_only_without_a_source_of_its_own() {
        let recorded = commit('e');
        let request = |spec: &Value, override_revision: Option<&str>| {
            source_request(&environment(spec), &recorded, override_revision, true, &BTreeMap::new()).unwrap()
        };
        let plain = request(&json!({}), None);
        assert_eq!(plain, SourceRequest::Recorded(recorded.clone()));
        assert_eq!(plain.choice(), SourceChoice::Recorded);
        assert_eq!(
            request(&json!({"source": {"revision": "release/prod"}}), None),
            SourceRequest::Revision("release/prod".to_owned())
        );
        assert_eq!(
            request(&json!({}), Some("hotfix")),
            SourceRequest::Override("hotfix".to_owned())
        );
    }

    fn record(path: &str, sequence: u64, source: Option<char>) -> PromotionRecord {
        serde_json::from_value(json!({
            "path": path, "sequence": sequence,
            "from": {"environment": "dev", "run": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90", "desiredCommit": "3".repeat(40)},
            "sourceCommit": source.map(|c| c.to_string().repeat(40)),
            "values": {}, "evidence": {"level": "published", "attestations": [], "override": null},
            "ignoredAttestations": [], "excludedDeployments": {}, "superseded": [],
            "artifactsVerified": true,
            "requested": {"by": "a", "identity": "local", "at": "2026-09-25T16:41:00Z"}
        }))
        .unwrap()
    }

    fn records(list: &[PromotionRecord]) -> BTreeMap<PromotionPathName, PromotionRecord> {
        list.iter().map(|r| (r.path.clone(), r.clone())).collect()
    }

    /// Catches a promoted source running from the entry commit instead of the
    /// newest record's source commit, or an environment without a record
    /// running at all: it is `NYL-PROMOTION-MISSING`, naming `nyl promote`.
    #[test]
    fn test_promoted_source_is_the_newest_records_commit_or_missing() {
        let promoted = environment(&json!({"source": {"fromPromotion": {"paths": ["dev-to-prod", "hotfix-to-prod"]}}}));
        let entry = commit('e');
        let missing = source_request(&promoted, &entry, None, false, &BTreeMap::new()).unwrap_err();
        assert_eq!(missing.code, Code::PromotionMissing);
        assert!(missing.fix.contains("nyl promote dev-to-prod"));
        let all = records(&[
            record("dev-to-prod", 1, Some('a')),
            record("hotfix-to-prod", 2, Some('b')),
            record("other", 9, Some('c')),
        ]);
        let request = source_request(&promoted, &entry, None, false, &all).unwrap();
        assert_eq!(
            request,
            SourceRequest::Promotion {
                path: PromotionPathName::new("hotfix-to-prod").unwrap(),
                sequence: 2,
                commit: commit('b')
            }
        );
        assert_eq!(
            request.choice(),
            SourceChoice::Promotion {
                path: "hotfix-to-prod".to_owned(),
                sequence: 2
            }
        );
        // A record of a values-only path carries no source commit and never supplies one.
        let values_only = records(&[record("dev-to-prod", 1, None)]);
        assert_eq!(
            source_request(&promoted, &entry, None, false, &values_only)
                .unwrap_err()
                .code,
            Code::PromotionMissing
        );
        // --source still wins for one invocation.
        assert_eq!(
            source_request(&promoted, &entry, Some("main"), false, &all).unwrap(),
            SourceRequest::Override("main".to_owned())
        );
    }

    fn in_source(block: Option<PromotionRecord>) -> PromotedSource {
        PromotedSource {
            path: Some("dev-to-prod".to_owned()),
            record: Some("source".to_owned()),
            promoted: block,
            ..PromotedSource::default()
        }
    }

    fn applied(sequence: u64, block: Option<PromotionRecord>) -> Applied {
        Applied {
            sequence: Some(sequence),
            block,
        }
    }

    /// With `record: source` the block is the path's record, in place of one
    /// in state, and supplies the source; a block below the applied sequence
    /// is refused with the reviewed way back (a new promotion), never applied.
    /// Catches an older checkout rolling the environment back, a stale state
    /// record shadowing the block, or a block applied for a path it does not
    /// belong to.
    #[test]
    fn test_record_source_block_is_the_record_and_never_older_than_applied() {
        let state = records(&[record("dev-to-prod", 5, Some('b')), record("other", 3, Some('c'))]);
        let block = record("dev-to-prod", 3, Some('a'));
        let promoted = in_source(Some(block.clone()));
        let read = promotion_records("prod", Some(&promoted), &state, &applied(2, None)).unwrap();
        assert_eq!(
            read.records[&block.path], block,
            "the block replaces the state's record"
        );
        assert_eq!(read.records[&PromotionPathName::new("other").unwrap()].sequence, 3);
        assert_eq!(
            read.check.as_ref(),
            Some(&block),
            "a newer block is checked against its state"
        );
        let environment = environment(&json!({"source": {"fromPromotion": serde_json::to_value(&promoted).unwrap()}}));
        assert_eq!(
            source_request(&environment, &commit('e'), None, false, &read.records).unwrap(),
            SourceRequest::Promotion {
                path: block.path.clone(),
                sequence: 3,
                commit: commit('a')
            }
        );

        let older = promotion_records("prod", Some(&promoted), &state, &applied(4, None)).unwrap_err();
        assert_eq!(older.code, Code::PromotionSequenceOlder);
        assert!(
            older
                .fix
                .contains(&format!("nyl promote dev-to-prod --revision {}", commit('a'))),
            "{}",
            older.fix
        );

        // No block yet: no record, so the source is NYL-PROMOTION-MISSING.
        let none = promotion_records("prod", Some(&in_source(None)), &state, &applied(5, None)).unwrap();
        assert!(!none.records.contains_key(&block.path));

        for invalid in [record("other", 7, Some('a')), record("dev-to-prod", 7, None)] {
            let finding =
                promotion_records("prod", Some(&in_source(Some(invalid))), &state, &Applied::default()).unwrap_err();
            assert_eq!(finding.code, Code::PromotionRecordInvalid);
        }

        // `record: state` reads state alone.
        let in_state = PromotedSource {
            record: None,
            ..in_source(None)
        };
        assert_eq!(
            promotion_records("prod", Some(&in_state), &state, &applied(9, None))
                .unwrap()
                .records,
            state
        );
    }

    /// A block at the applied sequence is the applied block, which needs no
    /// second check; any other block with that sequence is refused, because
    /// a sequence is never reused. Catches a promotion on another branch
    /// that took the same sequence being run as if it were the applied one.
    #[test]
    fn test_a_block_at_the_applied_sequence_must_be_the_applied_block() {
        let block = record("dev-to-prod", 3, Some('a'));
        let same = promotion_records(
            "prod",
            Some(&in_source(Some(block.clone()))),
            &BTreeMap::new(),
            &applied(3, Some(block.clone())),
        )
        .unwrap();
        assert_eq!(same.check, None);
        assert_eq!(same.records[&block.path], block);
        let reused = record("dev-to-prod", 3, Some('b'));
        let finding = promotion_records(
            "prod",
            Some(&in_source(Some(reused))),
            &BTreeMap::new(),
            &applied(3, Some(block)),
        )
        .unwrap_err();
        assert_eq!(finding.code, Code::PromotionRecordInvalid);
    }

    /// `nyl promote` reads the records as a run does: after switching from
    /// `record: state` there is no block yet, and the state's record on the
    /// block's path does not count; a block older than what was applied is
    /// refused, as the run refuses it. Catches a promotion reported
    /// `unchanged` against an old checkout's block, and one that never
    /// writes its first block after switching.
    #[test]
    fn test_promote_reads_a_block_as_a_run_would() {
        let path = PromotionPathName::new("dev-to-prod").unwrap();
        let state = records(&[record("dev-to-prod", 1, Some('b'))]);
        let switched = promotion_records("prod", Some(&in_source(None)), &state, &applied(1, None)).unwrap();
        assert!(!switched.records.contains_key(&path));
        let old = in_source(Some(record("dev-to-prod", 2, Some('a'))));
        assert_eq!(
            promotion_records("prod", Some(&old), &state, &applied(3, None))
                .unwrap_err()
                .code,
            Code::PromotionSequenceOlder
        );
        assert_eq!(
            promotion_records("prod", Some(&old), &state, &applied(1, None))
                .unwrap()
                .records[&path]
                .sequence,
            2
        );
    }

    /// Readers that run nothing show the block the last run applied, and a
    /// newer one in the entry worktree as the path's record not applied yet;
    /// an older entry block never replaces what ran. Catches `status` showing
    /// an old checkout's block as current.
    #[test]
    fn test_recorded_promotions_prefer_the_newer_block() {
        use nyl_core::orchestration::state::{EnvironmentRecord, EnvironmentRecordKind, StateV1};
        let applied = in_source(Some(record("dev-to-prod", 2, Some('a'))));
        let snapshot = EnvironmentSnapshot {
            environment: Some(EnvironmentRecord {
                api_version: StateV1,
                kind: EnvironmentRecordKind,
                environment: nyl_core::orchestration::ids::EnvironmentName::new("prod").unwrap(),
                spec: json!({"source": {"fromPromotion": serde_json::to_value(&applied).unwrap()}}),
                source_commit: commit('a'),
                source_chosen_by: SourceChoice::Promotion {
                    path: "dev-to-prod".to_owned(),
                    sequence: 2,
                },
                applied_promotion_sequence: Some(2),
                ownership: BTreeMap::new(),
            }),
            ..EnvironmentSnapshot::default()
        };
        let path = PromotionPathName::new("dev-to-prod").unwrap();
        assert_eq!(recorded_promotions(&snapshot, None)[&path].sequence, 2);
        let newer = in_source(Some(record("dev-to-prod", 3, Some('b'))));
        assert_eq!(recorded_promotions(&snapshot, Some(&newer))[&path].sequence, 3);
        let older = in_source(Some(record("dev-to-prod", 1, Some('c'))));
        assert_eq!(recorded_promotions(&snapshot, Some(&older))[&path].sequence, 2);
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
            source_checkouts(&spec, &fields, &commit('a'), None).unwrap()["/source"],
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
        let roots = source_checkouts(&spec, &fields, &commit('a'), None).unwrap();
        assert_eq!(roots["/a"], (SourceRepository::This, commit('c')));
        assert_eq!(roots["/b"], (SourceRepository::This, commit('d')));
        assert_eq!(effective_commit(&spec, &fields, &commit('a')).unwrap(), commit('a'));
    }

    /// A teardown's `--source` supplies the files of a field that only followed
    /// the unit's source commit, and of nothing explicit: a revision, a commit
    /// lock that is not the recorded commit, and another repository stay put.
    /// Catches a `--source` teardown that still checks out the lost commit.
    #[test]
    fn test_a_source_override_replaces_only_the_pin_that_followed_the_recorded_commit() {
        let recorded = commit('a');
        let mut spec = json!({
            "followed": {"path": "../infra"},
            "locked": {"path": "../infra", "commit": "c".repeat(40)},
            "tracked": {"path": "../infra", "revision": "main"},
            "remote": {"path": "modules", "repository": {"repoURL": "https://example.org/repo.git"}, "revision": "main"},
        });
        let fields = fields(&["/followed", "/locked", "/tracked", "/remote"]);
        let revisions = BTreeMap::from([(
            (
                SourceRepository::Url("https://example.org/repo.git".to_owned()),
                "main".to_owned(),
            ),
            commit('e'),
        )]);
        let mut tips = revisions.clone();
        tips.insert((SourceRepository::This, "main".to_owned()), commit('a'));
        pin_source_fields(&mut spec, &fields, "/nyl", &recorded, &tips).unwrap();
        let before = spec.clone();
        // Without an override the job commit is the recorded one: nothing moves.
        let same = source_checkouts(&spec, &fields, &recorded, Some(&recorded)).unwrap();
        assert_eq!(same["/followed"], (SourceRepository::This, recorded.clone()));
        assert_eq!(same["/locked"], (SourceRepository::This, commit('c')));
        // With `--source`, the job commit differs from the recorded one.
        let main = commit('f');
        let roots = source_checkouts(&spec, &fields, &main, Some(&recorded)).unwrap();
        assert_eq!(roots["/followed"], (SourceRepository::This, main.clone()));
        assert_eq!(roots["/locked"], (SourceRepository::This, commit('c')));
        assert_eq!(roots["/tracked"], (SourceRepository::This, recorded.clone()));
        assert_eq!(roots["/remote"].1, commit('e'));
        // The stored spec, and so every digest and approval, is unchanged.
        assert_eq!(spec, before);
        // Without the recorded commit nothing is replaced.
        let none = source_checkouts(&spec, &fields, &main, None).unwrap();
        assert_eq!(none["/followed"], (SourceRepository::This, recorded));
    }
}
