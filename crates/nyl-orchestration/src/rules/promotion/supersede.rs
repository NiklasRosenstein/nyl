//! The supersede check: what a promotion on one path may replace of what
//! other paths promoted into the same environment.
//!
//! Contract: [Several paths into one environment](../../../../../design/promotion.md#several-paths-into-one-environment)
//! ("Rule", "Source commit of a value", "Refusal", "Override", "Record",
//! "Which records") and decision D5 of the
//! [M6 plan](../../../../../design/m6-plan.md#open-decisions).
//!
//! - **Rule.** Before writing an item on path P (the target's source commit,
//!   or a value), every record for that item that another path wrote since P
//!   last promoted it ([`since`]) must be contained in the item's source
//!   commits: per repository, [`SourceResolver::missing_commits`] (`git
//!   cherry`) of the new item's commit against the record's lists nothing.
//!   [`plan`] lists these obligations, pairing commits by the source field
//!   they came from; [`Plan::prune`] drops a record whose commit a newer
//!   record of the same path contains by ancestry; the shell answers each
//!   distinct [`Check`] once.
//! - **Source commits.** The source item has the state's source commit; a
//!   value has the commits its producing unit's desired document resolved
//!   into its Git source fields ([`value_commits`]). A value without one is
//!   unchecked (`NYL-PROMOTE-UNCHECKED`, a warning), and so is a record whose
//!   value's state cannot be read or whose commit cannot be fetched.
//! - **Override.** `--supersede [<repository>:]<commit or range>[=<reason>]`
//!   ([`SupersedeRequest`]) accepts missing commits as replaced; an entry
//!   with neither its own reason nor `--reason` is refused
//!   ([`require_reasons`], `NYL-PROMOTE-SUPERSEDE-REASON`, exit 1). An
//!   unqualified entry belongs to the one repository whose missing commits
//!   it names ([`match_entry`]). Commits it names that are not missing are
//!   ignored. An entry naming the commit of a record that cannot be fetched
//!   supersedes that record.
//! - **Refusal.** A missing commit no entry covers refuses the promotion
//!   ([`judge`], `NYL-PROMOTE-SUPERSEDES`, exit 2), listing per record the
//!   missing commits with their subjects and the entries to copy.
//! - **Record.** Each superseded commit is recorded on its own, with its
//!   repository ([`RepositoryNames`]), reason, and who gave it, never the range.
//!
//! [`SourceResolver::missing_commits`]: nyl_core::orchestration::ports::SourceResolver::missing_commits

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, PromotionPathName, UnitName};
use nyl_core::orchestration::ports::{MissingCommit, SourceRepository};
use nyl_core::orchestration::promotion::{PromotionSelect, SupersededCommit};
use nyl_core::orchestration::state::{DesiredUnit, Provenance};

/// What a promotion writes that the supersede check covers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Item {
    /// The target environment's source commit.
    Source,
    /// A promoted value, by name.
    Value(String),
}

impl fmt::Display for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => formatter.write_str("source"),
            Self::Value(name) => formatter.write_str(name),
        }
    }
}

/// One source commit of an item: the source field it came from (empty for
/// the environment's source commit), its repository, and the commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceCommit {
    pub field: String,
    pub repository: SourceRepository,
    pub commit: CommitId,
}

/// The source commits of one item.
pub type Commits = BTreeSet<SourceCommit>;

/// The environment's source commit as an item's commits.
pub fn source_commits(commit: &CommitId) -> Commits {
    [SourceCommit {
        field: String::new(),
        repository: SourceRepository::This,
        commit: commit.clone(),
    }]
    .into()
}

/// A record into the target environment with the source commits of each item
/// it carries; `None` when an item's commits are not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordCommits {
    pub path: PromotionPathName,
    pub sequence: u64,
    pub items: BTreeMap<Item, Option<Commits>>,
}

/// One containment question: whether `value` contains `record` in `repository`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Check {
    pub repository: SourceRepository,
    pub value: CommitId,
    pub record: CommitId,
}

/// What the shell found for a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The record's commits the value lacks, oldest first; empty when it
    /// contains the record.
    Missing(Vec<MissingCommit>),
    /// The record's commit cannot be fetched, so nothing can be compared.
    Unavailable,
}

/// A commit of another path's record that an item must contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub item: Item,
    /// The other path and the sequence of its record.
    pub path: PromotionPathName,
    pub sequence: u64,
    pub repository: SourceRepository,
    pub record: CommitId,
    /// One check per source commit the item has for the same field in
    /// `repository`, or for any field there when none matches; any one that
    /// finds nothing missing satisfies the obligation.
    pub checks: Vec<Check>,
}

/// What the check must ask, and what it cannot check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub obligations: Vec<Obligation>,
    /// `NYL-PROMOTE-UNCHECKED` warnings.
    pub unchecked: Vec<Finding>,
}

impl Plan {
    /// Every distinct check, for the shell to answer once each.
    pub fn checks(&self) -> BTreeSet<Check> {
        self.obligations
            .iter()
            .flat_map(|obligation| obligation.checks.iter().cloned())
            .collect()
    }

    /// Drop each obligation whose record commit a newer record of the same
    /// path contains by ancestry (`is_ancestor(repository, older, newer)`):
    /// whatever contains the newer one contains the older one, so it needs
    /// no check of its own.
    pub fn prune(&mut self, is_ancestor: &dyn Fn(&SourceRepository, &CommitId, &CommitId) -> bool) {
        let all = self.obligations.clone();
        self.obligations.retain(|older| {
            !all.iter().any(|newer| {
                newer.item == older.item
                    && newer.path == older.path
                    && newer.repository == older.repository
                    && newer.sequence > older.sequence
                    && is_ancestor(&older.repository, &older.record, &newer.record)
            })
        });
    }
}

/// Since when other paths' records count for `item` on `path`: the newest
/// record on `path` that carries the item, among every record known (current
/// and from history), or 0.
pub fn since(path: &PromotionPathName, item: &Item, records: &[RecordCommits]) -> u64 {
    records
        .iter()
        .filter(|record| record.path == *path && record.items.contains_key(item))
        .map(|record| record.sequence)
        .max()
        .unwrap_or(0)
}

fn unchecked(message: String) -> Finding {
    Finding::new(
        Code::PromoteUnchecked,
        message,
        "check by hand that the promotion keeps what the other path promoted, such as by its image's build inputs",
    )
}

/// The obligations of a promotion on `path` that writes `writes`, against
/// `records`: every record into the target (current, and from the target's
/// history), on any path. Records on `path` itself only say since when
/// another path's records count ([`since`]).
pub fn plan(path: &PromotionPathName, writes: &BTreeMap<Item, Option<Commits>>, records: &[RecordCommits]) -> Plan {
    let mut plan = Plan::default();
    let mut messages = BTreeSet::new();
    for (item, written) in writes {
        let since = since(path, item, records);
        let mut others: Vec<&RecordCommits> = records
            .iter()
            .filter(|record| record.path != *path && record.sequence > since)
            .collect();
        others.sort_by_key(|record| record.sequence);
        for record in others {
            let Some(theirs) = record.items.get(item) else {
                continue;
            };
            let Some(theirs) = theirs else {
                messages.insert(format!(
                    "{item} of record {} #{} has no known source commit, so path {path} cannot check that it keeps it",
                    record.path, record.sequence
                ));
                continue;
            };
            let Some(written) = written else {
                messages.insert(format!(
                    "{item} has no known source commit, so path {path} cannot check that it keeps what record {} #{} promoted",
                    record.path, record.sequence
                ));
                continue;
            };
            for theirs in theirs {
                let in_repository: Vec<&SourceCommit> = written
                    .iter()
                    .filter(|ours| ours.repository == theirs.repository)
                    .collect();
                let same_field: Vec<&SourceCommit> = in_repository
                    .iter()
                    .copied()
                    .filter(|ours| ours.field == theirs.field)
                    .collect();
                let paired = if same_field.is_empty() {
                    in_repository
                } else {
                    same_field
                };
                if paired.is_empty() {
                    messages.insert(format!(
                        "{item} has no source commit in {} (repository of record {} #{}'s {}), so path {path} cannot check that it keeps it",
                        describe(&theirs.repository),
                        record.path,
                        record.sequence,
                        theirs.commit
                    ));
                    continue;
                }
                plan.obligations.push(Obligation {
                    item: item.clone(),
                    path: record.path.clone(),
                    sequence: record.sequence,
                    repository: theirs.repository.clone(),
                    record: theirs.commit.clone(),
                    checks: paired
                        .into_iter()
                        .map(|ours| Check {
                            repository: theirs.repository.clone(),
                            value: ours.commit.clone(),
                            record: theirs.commit.clone(),
                        })
                        .collect(),
                });
            }
        }
    }
    plan.unchecked = messages.into_iter().map(unchecked).collect();
    plan
}

fn describe(repository: &SourceRepository) -> String {
    match repository {
        SourceRepository::This => "this repository".to_owned(),
        SourceRepository::Url(url) => normalize_url(url),
    }
}

/// The source commits of a value read with `select` from a state whose
/// desired documents are `desired`: the commits the producing unit's desired
/// document resolved into its Git source fields (`source_fields` is its
/// driver's [`nyl_core::orchestration::driver::Driver::source_fields`]). An
/// `input` selector whose pointer a unit reference filled has the referenced
/// producer's commits; one that reads a Git source field has that field's
/// commit. `None` when nothing names a commit, such as a command output or an
/// input written in the spec.
pub fn value_commits(
    select: &PromotionSelect,
    desired: &BTreeMap<UnitName, DesiredUnit>,
    source_fields: &dyn Fn(&DesiredUnit) -> Vec<String>,
) -> Option<Commits> {
    let unit = desired.get(select.unit.as_ref()?)?;
    let Some(input) = &select.input else {
        return unit_commits(unit, None, source_fields);
    };
    // A reference at the pointer or above it names the producer.
    let cited = unit
        .provenance
        .iter()
        .filter(|(pointer, _)| is_within(input, pointer.as_str()))
        .max_by_key(|(pointer, _)| pointer.as_str().len())
        .map(|(_, provenance)| provenance);
    match cited {
        Some(Provenance::Receipt(receipt)) if receipt.environment.is_none() => {
            unit_commits(desired.get(&receipt.unit)?, None, source_fields)
        }
        Some(_) => None,
        None => unit_commits(unit, Some(input), source_fields),
    }
}

/// Whether `pointer` is `ancestor` or lies beneath it.
fn is_within(pointer: &str, ancestor: &str) -> bool {
    pointer == ancestor || pointer.strip_prefix(ancestor).is_some_and(|rest| rest.starts_with('/'))
}

/// The commits a unit's resolved Git source fields name; with `input`, only
/// the field that holds it.
fn unit_commits(
    unit: &DesiredUnit,
    input: Option<&str>,
    source_fields: &dyn Fn(&DesiredUnit) -> Vec<String>,
) -> Option<Commits> {
    let spec = unit.resolved_spec.as_ref()?;
    let mut commits = Commits::new();
    for field in source_fields(unit) {
        if input.is_some_and(|input| !is_within(input, &field)) {
            continue;
        }
        let Ok(Some(source)) = crate::rules::sources::git_source(spec, &field) else {
            continue;
        };
        let (Ok(repository), Some(commit)) = (
            crate::rules::sources::source_repository(&source),
            source.commit.as_deref().and_then(|commit| CommitId::new(commit).ok()),
        ) else {
            continue;
        };
        commits.insert(SourceCommit {
            field,
            repository,
            commit,
        });
    }
    (!commits.is_empty()).then_some(commits)
}

/// A repository URL in the form names are matched and recorded by: without
/// credentials, scheme, a trailing `.git` or `/`, with `git@host:org/r` as
/// `host/org/r` and the host lowercased. `https://Git.example/org/r.git`,
/// `ssh://git@git.example/org/r`, and `git@git.example:org/r` are all
/// `git.example/org/r`. A local path keeps its form.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let (rest, scp) = match url.split_once("://") {
        Some((_, rest)) => (rest, false),
        None => (url, url.contains(':') && !url.starts_with('/') && !url.contains(":\\")),
    };
    let rest = match rest.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest,
        _ => rest,
    };
    let rest = if scp {
        rest.replacen(':', "/", 1)
    } else {
        rest.to_owned()
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest).trim_end_matches('/');
    match rest.split_once('/') {
        Some((host, path)) if !host.is_empty() => format!("{}/{path}", host.to_ascii_lowercase()),
        _ => rest.to_owned(),
    }
}

/// How repositories are named in refusals, `--supersede` qualifiers, and
/// the record's `superseded[].repository`: the name of the GitRepository
/// whose URL matches (after [`normalize_url`]), else the normalized URL; the
/// project's own repository matches by its `origin` URL, and without a
/// GitRepository is its normalized `origin` URL, or `project` without one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryNames {
    /// Each GitRepository's name and normalized URL.
    declared: Vec<(String, String)>,
    /// The project's `origin` URL, normalized.
    origin: Option<String>,
}

impl RepositoryNames {
    /// From the project's GitRepositories (name, URL) and its `origin` URL.
    pub fn new(declared: &[(String, String)], origin: Option<&str>) -> Self {
        Self {
            declared: declared
                .iter()
                .map(|(name, url)| (name.clone(), normalize_url(url)))
                .collect(),
            origin: origin.map(normalize_url),
        }
    }

    fn url_of(&self, repository: &SourceRepository) -> Option<String> {
        match repository {
            SourceRepository::This => self.origin.clone(),
            SourceRepository::Url(url) => Some(normalize_url(url)),
        }
    }

    /// The name of `repository`.
    pub fn name_of(&self, repository: &SourceRepository) -> String {
        let Some(url) = self.url_of(repository) else {
            return "project".to_owned();
        };
        self.declared
            .iter()
            .find(|(_, declared)| *declared == url)
            .map_or(url, |(name, _)| name.clone())
    }

    /// Whether a `--supersede` qualifier names `repository`: by its name, or
    /// by a URL that normalizes to its URL.
    pub fn names(&self, repository: &SourceRepository, qualifier: &str) -> bool {
        self.name_of(repository) == qualifier
            || self
                .url_of(repository)
                .is_some_and(|url| url == normalize_url(qualifier))
    }
}

/// `--supersede [<repository>:]<commit or range>[=<reason>]`, as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeRequest {
    /// The repository qualifier: a GitRepository name, or the name a refusal prints.
    pub repository: Option<String>,
    /// A commit, or a Git range `<from>..<to>`.
    pub range: String,
    /// The entry's own reason.
    pub reason: Option<String>,
}

impl SupersedeRequest {
    /// Parse an entry. The reason follows the first `=` and may contain any
    /// text; a qualifier ends at the last `:` before it, since a commit or
    /// range never contains one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let malformed = || format!("--supersede {text:?} is not [<repository>:]<commit or range>[=<reason>]");
        let (target, reason) = match text.split_once('=') {
            Some((_, reason)) if reason.trim().is_empty() => return Err(malformed()),
            Some((target, reason)) => (target, Some(reason.trim().to_owned())),
            None => (text, None),
        };
        let (repository, range) = match target.rsplit_once(':') {
            Some((repository, range)) if !repository.is_empty() => (Some(repository.trim().to_owned()), range),
            Some(_) => return Err(malformed()),
            None => (None, target),
        };
        let range = range.trim();
        if range.is_empty() || range.contains(char::is_whitespace) || range.starts_with('-') {
            return Err(malformed());
        }
        Ok(Self {
            repository,
            range: range.to_owned(),
            reason,
        })
    }

    /// The entry as given, for messages.
    pub fn text(&self) -> String {
        let qualifier = self
            .repository
            .as_ref()
            .map(|repository| format!("{repository}:"))
            .unwrap_or_default();
        format!("{qualifier}{}", self.range)
    }
}

/// Every entry must carry a reason, its own or `--reason`: otherwise
/// `NYL-PROMOTE-SUPERSEDE-REASON` (exit 1), naming the entries without one.
pub fn require_reasons(entries: &[SupersedeRequest], reason: Option<&str>) -> Result<(), Finding> {
    if reason.is_some_and(|reason| !reason.trim().is_empty()) {
        return Ok(());
    }
    let without: Vec<String> = entries
        .iter()
        .filter(|entry| entry.reason.is_none())
        .map(SupersedeRequest::text)
        .collect();
    if without.is_empty() {
        return Ok(());
    }
    Err(Finding::new(
        Code::PromoteSupersedeReason,
        format!(
            "--supersede {} gives no reason, and the promotion has no --reason: a superseded commit is recorded with why it no longer needs to reach the target",
            without.join(", ")
        ),
        format!(
            "append the reason to the entry, such as `--supersede {}=<why>`, or pass --reason",
            without[0]
        ),
    ))
}

/// The repository a qualified entry names, among `repositories` (the ones
/// the obligations compare in).
pub fn qualified(
    entry: &SupersedeRequest,
    repositories: &BTreeSet<SourceRepository>,
    names: &RepositoryNames,
) -> Result<Option<SourceRepository>, Finding> {
    let Some(wanted) = &entry.repository else {
        return Ok(None);
    };
    repositories
        .iter()
        .find(|repository| names.names(repository, wanted))
        .cloned()
        .map(Some)
        .ok_or_else(|| {
            Finding::new(
                Code::SourceUnavailable,
                format!(
                    "--supersede {} names repository {wanted:?}, which no missing commit is in",
                    entry.text()
                ),
                format!(
                    "qualify the entry with one of: {}",
                    repositories
                        .iter()
                        .map(|repository| names.name_of(repository))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
}

/// A `--supersede` entry resolved to the commits it names in its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeEntry {
    pub repository: SourceRepository,
    pub commits: BTreeSet<CommitId>,
    pub reason: String,
}

/// The commits per repository a `--supersede` entry can cover: the missing
/// commits of every check, and the commit of a record that cannot be fetched.
pub fn wanted(plan: &Plan, answers: &BTreeMap<Check, Answer>) -> BTreeMap<SourceRepository, BTreeSet<CommitId>> {
    let mut wanted: BTreeMap<SourceRepository, BTreeSet<CommitId>> = BTreeMap::new();
    for check in plan.checks() {
        let commits = wanted.entry(check.repository.clone()).or_default();
        match answers.get(&check) {
            Some(Answer::Missing(missing)) => commits.extend(missing.iter().map(|missing| missing.commit.clone())),
            Some(Answer::Unavailable) => {
                commits.insert(check.record.clone());
            }
            None => {}
        }
    }
    wanted
}

/// The repository an entry belongs to, from the commits it names in each
/// candidate repository (`resolved`; all repositories share one object
/// database, so a commit ID resolves in each) and the commits each could
/// cover (`wanted`): a qualified entry its own; an unqualified one the only
/// repository where it names a commit to cover, and with several it must be
/// qualified. `None` when it names nothing to cover, which is ignored.
pub fn match_entry(
    entry: &SupersedeRequest,
    reason: String,
    resolved: &BTreeMap<SourceRepository, Vec<CommitId>>,
    wanted: &BTreeMap<SourceRepository, BTreeSet<CommitId>>,
    names: &RepositoryNames,
) -> Result<Option<SupersedeEntry>, Finding> {
    let covering: Vec<(&SourceRepository, BTreeSet<CommitId>)> = resolved
        .iter()
        .map(|(repository, commits)| {
            let wanted = wanted.get(repository);
            let hit: BTreeSet<CommitId> = commits
                .iter()
                .filter(|commit| wanted.is_some_and(|wanted| wanted.contains(*commit)))
                .cloned()
                .collect();
            (repository, hit)
        })
        .filter(|(_, hit)| !hit.is_empty())
        .collect();
    match covering.as_slice() {
        [] => Ok(None),
        [(repository, commits)] => Ok(Some(SupersedeEntry {
            repository: (*repository).clone(),
            commits: commits.clone(),
            reason,
        })),
        several => Err(Finding::new(
            Code::SourceUnavailable,
            format!(
                "--supersede {} names missing commits in several repositories: {}",
                entry.text(),
                several
                    .iter()
                    .map(|(repository, _)| names.name_of(repository))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "qualify the entry with its repository, `--supersede <repository>:<range>=<reason>`",
        )),
    }
}

/// An obligation no check satisfies, with the commits still missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsatisfied {
    pub path: PromotionPathName,
    pub sequence: u64,
    pub repository: SourceRepository,
    /// The new item's commit the check compared.
    pub value: CommitId,
    /// The other record's commit.
    pub record: CommitId,
    /// The items that must contain it.
    pub items: Vec<Item>,
    /// Missing commits no entry covers, oldest first.
    pub missing: Vec<MissingCommit>,
    /// The `--supersede` entries (without reason) that would cover them,
    /// qualified when the refusal involves several repositories or another
    /// repository than the project's.
    pub supersede: Vec<String>,
}

/// Why the promotion is refused, with what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeRefusal {
    pub finding: Finding,
    pub unsatisfied: Vec<Unsatisfied>,
}

/// What the check accepted: the superseded commits the record lists, and
/// records that could not be compared (`NYL-PROMOTE-UNCHECKED`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Accepted {
    pub superseded: Vec<SupersededCommit>,
    pub unchecked: Vec<Finding>,
}

/// What [`judge`] reads besides the answers.
pub struct Judged<'a> {
    pub path: &'a PromotionPathName,
    pub source: &'a EnvironmentName,
    pub target: &'a EnvironmentName,
    /// Who gives the `--supersede` entries.
    pub by: &'a str,
    pub names: &'a RepositoryNames,
}

/// Decide the check. An obligation is satisfied by a check that finds
/// nothing missing, else by one whose missing commits the entries cover
/// entirely, and the record lists the covered ones; a record whose commit
/// cannot be fetched is unchecked, unless an entry names that commit, which
/// supersedes it. Otherwise `NYL-PROMOTE-SUPERSEDES` (exit 2) lists every
/// obligation still unsatisfied.
pub fn judge(
    context: &Judged<'_>,
    plan: &Plan,
    answers: &BTreeMap<Check, Answer>,
    entries: &[SupersedeEntry],
) -> Result<Accepted, SupersedeRefusal> {
    let covering = |repository: &SourceRepository, commit: &CommitId| {
        entries
            .iter()
            .find(|entry| entry.repository == *repository && entry.commits.contains(commit))
    };
    let mut accepted = Accepted::default();
    let mut unchecked = BTreeSet::new();
    let supersede = |repository: &SourceRepository, commit: &CommitId, accepted: &mut Accepted| {
        let entry = covering(repository, commit).expect("covered");
        record_superseded(accepted, context, repository, commit, &entry.reason);
    };
    let mut unsatisfied: Vec<Unsatisfied> = Vec::new();
    for obligation in &plan.obligations {
        let answered: Vec<(&Check, &Answer)> = obligation
            .checks
            .iter()
            .filter_map(|check| answers.get(check).map(|answer| (check, answer)))
            .collect();
        let missing_of = |answer: &Answer| match answer {
            Answer::Missing(missing) => Some(missing.clone()),
            Answer::Unavailable => None,
        };
        // Nothing missing first, then everything missing covered.
        if answered
            .iter()
            .any(|(_, answer)| missing_of(answer).is_some_and(|missing| missing.is_empty()))
        {
            continue;
        }
        let covered = answered.iter().find_map(|(check, answer)| {
            missing_of(answer).filter(|missing| {
                missing
                    .iter()
                    .all(|commit| covering(&check.repository, &commit.commit).is_some())
            })
        });
        if let Some(missing) = covered {
            for commit in &missing {
                supersede(&obligation.repository, &commit.commit, &mut accepted);
            }
            continue;
        }
        if answered.iter().all(|(_, answer)| **answer == Answer::Unavailable) {
            if covering(&obligation.repository, &obligation.record).is_some() {
                supersede(&obligation.repository, &obligation.record, &mut accepted);
            } else {
                unchecked.insert(format!(
                    "commit {} of record {} #{} ({}) cannot be fetched, so path {} cannot check that it keeps it; fetch it, or pass `--supersede {}=<reason>` to record that this promotion replaces it",
                    obligation.record,
                    obligation.path,
                    obligation.sequence,
                    obligation.item,
                    context.path,
                    obligation.record
                ));
            }
            continue;
        }
        let Some((check, missing)) = answered
            .iter()
            .filter_map(|(check, answer)| {
                missing_of(answer).map(|missing| {
                    let uncovered: Vec<MissingCommit> = missing
                        .into_iter()
                        .filter(|commit| covering(&check.repository, &commit.commit).is_none())
                        .collect();
                    (*check, uncovered)
                })
            })
            .min_by_key(|(_, uncovered)| uncovered.len())
        else {
            continue;
        };
        add_unsatisfied(&mut unsatisfied, obligation, check, missing);
    }
    accepted.unchecked = unchecked.into_iter().map(unchecked_finding).collect();
    if unsatisfied.is_empty() {
        return Ok(accepted);
    }
    with_entries(context.names, &mut unsatisfied);
    Err(SupersedeRefusal {
        finding: refusal_finding(context, &unsatisfied),
        unsatisfied,
    })
}

/// Add an obligation no check satisfies, merged with one for the same
/// record and commits that another item already listed.
fn add_unsatisfied(
    unsatisfied: &mut Vec<Unsatisfied>,
    obligation: &Obligation,
    check: &Check,
    missing: Vec<MissingCommit>,
) {
    if let Some(known) = unsatisfied.iter_mut().find(|known| {
        known.path == obligation.path
            && known.sequence == obligation.sequence
            && known.repository == check.repository
            && known.value == check.value
            && known.record == check.record
    }) {
        if !known.items.contains(&obligation.item) {
            known.items.push(obligation.item.clone());
        }
        return;
    }
    unsatisfied.push(Unsatisfied {
        path: obligation.path.clone(),
        sequence: obligation.sequence,
        repository: check.repository.clone(),
        value: check.value.clone(),
        record: check.record.clone(),
        items: vec![obligation.item.clone()],
        missing,
        supersede: Vec::new(),
    });
}

/// Fill each unsatisfied obligation's `--supersede` entries, qualified when
/// the refusal involves several repositories or another repository than
/// the project's.
fn with_entries(names: &RepositoryNames, unsatisfied: &mut [Unsatisfied]) {
    let qualify = unsatisfied
        .iter()
        .map(|unsatisfied| &unsatisfied.repository)
        .collect::<BTreeSet<_>>()
        .len()
        > 1
        || unsatisfied
            .iter()
            .any(|unsatisfied| unsatisfied.repository != SourceRepository::This);
    for unsatisfied in unsatisfied.iter_mut() {
        let qualifier = if qualify {
            format!("{}:", names.name_of(&unsatisfied.repository))
        } else {
            String::new()
        };
        unsatisfied.supersede = entries_for(unsatisfied)
            .into_iter()
            .map(|entry| format!("{qualifier}{entry}"))
            .collect();
    }
}

/// Add a superseded commit to what the record lists, once per repository.
fn record_superseded(
    accepted: &mut Accepted,
    context: &Judged<'_>,
    repository: &SourceRepository,
    commit: &CommitId,
    reason: &str,
) {
    let recorded = context.names.name_of(repository);
    if !accepted
        .superseded
        .iter()
        .any(|known| known.commit == *commit && known.repository == recorded)
    {
        accepted.superseded.push(SupersededCommit {
            repository: recorded,
            commit: commit.clone(),
            reason: reason.to_owned(),
            by: context.by.to_owned(),
        });
    }
}

fn unchecked_finding(message: String) -> Finding {
    Finding::new(
        Code::PromoteUnchecked,
        message,
        "fetch the commit (the branch that carries it) and promote again, or record that this promotion replaces it with --supersede",
    )
}

/// The `--supersede` entries that cover what is missing: the range from the
/// parent of the oldest missing commit to the record's commit, with full
/// commit IDs; when the oldest is a root commit, which has no parent, each
/// missing commit on its own.
pub fn entries_for(unsatisfied: &Unsatisfied) -> Vec<String> {
    match unsatisfied.missing.first() {
        Some(oldest) if oldest.root => unsatisfied
            .missing
            .iter()
            .map(|commit| commit.commit.to_string())
            .collect(),
        Some(oldest) => vec![format!("{}^..{}", oldest.commit, unsatisfied.record)],
        None => vec![unsatisfied.record.to_string()],
    }
}

/// 12 characters of a commit, as messages abbreviate it.
fn short(commit: &CommitId) -> &str {
    &commit.as_str()[..12]
}

fn refusal_finding(context: &Judged<'_>, unsatisfied: &[Unsatisfied]) -> Finding {
    let several = unsatisfied
        .iter()
        .map(|unsatisfied| &unsatisfied.repository)
        .collect::<BTreeSet<_>>()
        .len()
        > 1;
    let options = |unsatisfied: &Unsatisfied| {
        unsatisfied
            .supersede
            .iter()
            .map(|entry| format!("--supersede {entry}=<reason>"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let parts: Vec<String> = unsatisfied
        .iter()
        .map(|unsatisfied| {
            let count = unsatisfied.missing.len();
            let commits = unsatisfied
                .missing
                .iter()
                .map(|commit| format!("{} {}", short(&commit.commit), commit.subject))
                .collect::<Vec<_>>()
                .join(", ");
            let repository = if several {
                format!(" in {}", context.names.name_of(&unsatisfied.repository))
            } else {
                String::new()
            };
            format!(
                "{} is missing {count} commit{} from {} #{} ({}){repository}: {commits}",
                short(&unsatisfied.value),
                if count == 1 { "" } else { "s" },
                unsatisfied.path,
                unsatisfied.sequence,
                unsatisfied
                    .items
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })
        .collect();
    let all: Vec<String> = unsatisfied.iter().map(options).collect();
    Finding::new(
        Code::PromoteSupersedes,
        format!(
            "path {} would replace what other paths promoted into {}: {}; merge them or pass {}",
            context.path,
            context.target,
            parts.join("; "),
            all.join(" ")
        ),
        format!(
            "merge the missing commits into the source {} runs, reconcile it, and promote again; or record that this promotion replaces them with {}",
            context.source,
            all.join(" ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{desired, name};

    const WEB: &str = "https://git.example.invalid/web.git";

    fn commit(fill: char) -> CommitId {
        CommitId::new(fill.to_string().repeat(40)).unwrap()
    }

    fn path(name: &str) -> PromotionPathName {
        PromotionPathName::new(name).unwrap()
    }

    fn web() -> SourceRepository {
        SourceRepository::Url(WEB.to_owned())
    }

    fn at(field: &str, repository: &SourceRepository, fill: char) -> SourceCommit {
        SourceCommit {
            field: field.to_owned(),
            repository: repository.clone(),
            commit: commit(fill),
        }
    }

    /// A record that carries the source commit and `webImage`, both built
    /// from the same commit, as a source path's records do.
    fn source_record(on: &str, sequence: u64, fill: char) -> RecordCommits {
        RecordCommits {
            path: path(on),
            sequence,
            items: [
                (Item::Source, Some(source_commits(&commit(fill)))),
                (
                    Item::Value("webImage".to_owned()),
                    Some([at("/context", &SourceRepository::This, fill)].into()),
                ),
            ]
            .into(),
        }
    }

    fn writes(fill: char) -> BTreeMap<Item, Option<Commits>> {
        [
            (Item::Source, Some(source_commits(&commit(fill)))),
            (
                Item::Value("webImage".to_owned()),
                Some([at("/context", &SourceRepository::This, fill)].into()),
            ),
        ]
        .into()
    }

    fn missing(fill: char, subject: &str) -> MissingCommit {
        MissingCommit {
            commit: commit(fill),
            subject: subject.to_owned(),
            root: false,
        }
    }

    fn names() -> RepositoryNames {
        RepositoryNames::new(&[("web".to_owned(), WEB.to_owned())], None)
    }

    fn judge_with(
        plan: &Plan,
        answers: &BTreeMap<Check, Answer>,
        entries: &[SupersedeEntry],
    ) -> Result<Accepted, SupersedeRefusal> {
        let names = names();
        let context = Judged {
            path: &path("dev-to-prod"),
            source: &EnvironmentName::new("dev").unwrap(),
            target: &EnvironmentName::new("prod").unwrap(),
            by: "release-bot",
            names: &names,
        };
        judge(&context, plan, answers, entries)
    }

    fn entry(repository: SourceRepository, commits: &[char], reason: &str) -> SupersedeEntry {
        SupersedeEntry {
            repository,
            commits: commits.iter().map(|fill| commit(*fill)).collect(),
            reason: reason.to_owned(),
        }
    }

    /// Only records of other paths written after this path's newest record
    /// count, each item for itself, this path's records from history
    /// included; one check answers the source and a value built from the
    /// same commit. Catches a check against what this path already promoted
    /// past, or "since" taken from the current record alone.
    #[test]
    fn test_plan_checks_other_paths_since_this_path_last_promoted() {
        let records = vec![
            source_record("hotfix-to-prod", 1, 'a'),
            source_record("dev-to-prod", 2, 'b'),
            source_record("hotfix-to-prod", 3, 'c'),
        ];
        let plan = plan(&path("dev-to-prod"), &writes('d'), &records);
        assert_eq!(plan.obligations.len(), 2, "{plan:?}");
        assert!(plan.obligations.iter().all(|obligation| obligation.sequence == 3));
        assert_eq!(
            plan.checks(),
            [Check {
                repository: SourceRepository::This,
                value: commit('d'),
                record: commit('c'),
            }]
            .into()
        );
        assert!(plan.unchecked.is_empty());
        assert_eq!(since(&path("dev-to-prod"), &Item::Source, &records), 2);

        // Without a record of its own, every other path's record counts.
        let plan = super::plan(&path("dev-to-prod"), &writes('d'), &records[..1]);
        assert_eq!(plan.obligations.len(), 2);
    }

    /// The rule is symmetric: the hotfix path is checked against dev's
    /// newer record exactly as dev is against the hotfix's. Catches a check
    /// that only guards one direction.
    #[test]
    fn test_plan_is_symmetric() {
        let records = vec![
            source_record("hotfix-to-prod", 4, 'a'),
            source_record("dev-to-prod", 5, 'b'),
        ];
        let plan = plan(&path("hotfix-to-prod"), &writes('c'), &records);
        assert!(plan
            .obligations
            .iter()
            .all(|obligation| obligation.path == path("dev-to-prod") && obligation.record == commit('b')));
        assert_eq!(plan.obligations.len(), 2);
    }

    /// A record's commit is compared with the commit of the same source
    /// field, and with any commit of its repository only when no field
    /// matches. Catches an image's library context compared with its main
    /// context, which would report the library's whole history as missing.
    #[test]
    fn test_plan_pairs_commits_by_source_field() {
        let image = |main: char, lib: char| -> Commits {
            [
                at("/context", &SourceRepository::This, main),
                at("/contexts/lib", &SourceRepository::This, lib),
            ]
            .into()
        };
        let theirs = RecordCommits {
            path: path("hotfix-to-prod"),
            sequence: 2,
            items: [(Item::Value("webImage".to_owned()), Some(image('a', 'b')))].into(),
        };
        let ours: BTreeMap<Item, Option<Commits>> =
            [(Item::Value("webImage".to_owned()), Some(image('c', 'd')))].into();
        let plan = plan(&path("dev-to-prod"), &ours, std::slice::from_ref(&theirs));
        let pairs: BTreeSet<(CommitId, CommitId)> = plan
            .checks()
            .into_iter()
            .map(|check| (check.value, check.record))
            .collect();
        assert_eq!(pairs, [(commit('c'), commit('a')), (commit('d'), commit('b'))].into());

        // A field the new value lacks falls back to every commit of the repository.
        let ours: BTreeMap<Item, Option<Commits>> = [(
            Item::Value("webImage".to_owned()),
            Some([at("/context", &SourceRepository::This, 'c')].into()),
        )]
        .into();
        let plan = super::plan(&path("dev-to-prod"), &ours, &[theirs]);
        assert_eq!(plan.checks().len(), 2);
        assert!(plan.checks().iter().all(|check| check.value == commit('c')));
    }

    /// A record whose commit a newer record of the same path descends from
    /// needs no check of its own. Catches a promotion that compares every
    /// hotfix of a long-lived branch one by one.
    #[test]
    fn test_prune_drops_records_a_newer_one_of_the_same_path_contains() {
        let records = vec![
            source_record("hotfix-to-prod", 2, 'a'),
            source_record("hotfix-to-prod", 4, 'b'),
        ];
        let mut plan = plan(&path("dev-to-prod"), &writes('d'), &records);
        assert_eq!(plan.obligations.len(), 4);
        plan.prune(&|_, older, newer| *older == commit('a') && *newer == commit('b'));
        assert_eq!(plan.obligations.len(), 2);
        assert!(plan.obligations.iter().all(|obligation| obligation.sequence == 4));
    }

    /// A value without a known source commit, or another record's value
    /// without one, is listed as unchecked instead of passing silently or
    /// refusing. Catches an image from outside Nyl blocking every promotion.
    #[test]
    fn test_plan_lists_unknown_source_commits_as_unchecked() {
        let mut theirs = source_record("hotfix-to-prod", 2, 'a');
        theirs.items.insert(Item::Value("webImage".to_owned()), None);
        let mut ours = writes('d');
        let plan = plan(&path("dev-to-prod"), &ours, std::slice::from_ref(&theirs));
        assert_eq!(plan.obligations.len(), 1, "the source commit is still checked");
        assert_eq!(plan.unchecked.len(), 1);
        assert_eq!(plan.unchecked[0].code, Code::PromoteUnchecked);

        ours.insert(Item::Value("webImage".to_owned()), None);
        let plan = super::plan(&path("dev-to-prod"), &ours, &[source_record("hotfix-to-prod", 2, 'a')]);
        assert_eq!(plan.unchecked.len(), 1);
        assert!(plan.unchecked[0]
            .message
            .contains("webImage has no known source commit"));
    }

    fn one_plan() -> (Plan, Check) {
        let plan = plan(
            &path("dev-to-prod"),
            &writes('d'),
            &[source_record("hotfix-to-prod", 2, 'c')],
        );
        let check = plan.checks().into_iter().next().unwrap();
        (plan, check)
    }

    /// Missing commits refuse with `NYL-PROMOTE-SUPERSEDES` (exit 2),
    /// naming the other path, every missing commit by subject, and a range
    /// with full commit IDs to pass to `--supersede`; nothing missing passes
    /// with no superseded commits. Catches a refusal a user cannot act on.
    #[test]
    fn test_judge_refuses_missing_commits_with_the_range_to_supersede() {
        let (plan, check) = one_plan();
        let answers = BTreeMap::from([(
            check.clone(),
            Answer::Missing(vec![missing('a', "Fix pool size"), missing('c', "Bump timeout")]),
        )]);
        let refusal = judge_with(&plan, &answers, &[]).unwrap_err();
        assert_eq!(refusal.finding.code, Code::PromoteSupersedes);
        assert_eq!(crate::run_report::ExitCategory::of_finding(&refusal.finding).code(), 2);
        let range = format!("{}^..{}", commit('a'), commit('c'));
        for expected in [
            "hotfix-to-prod #2",
            "Fix pool size",
            "Bump timeout",
            &format!("--supersede {range}=<reason>"),
        ] {
            assert!(
                refusal.finding.message.contains(expected),
                "{}",
                refusal.finding.message
            );
        }
        assert_eq!(refusal.unsatisfied[0].supersede, [range]);
        assert_eq!(
            refusal.unsatisfied[0].items,
            [Item::Source, Item::Value("webImage".to_owned())]
        );

        let none = BTreeMap::from([(check, Answer::Missing(Vec::new()))]);
        assert_eq!(judge_with(&plan, &none, &[]).unwrap(), Accepted::default());
    }

    /// A root commit has no parent to start a range at, so each missing
    /// commit is its own entry; in another repository than the project's,
    /// every entry carries its qualifier. Catches a hint Git cannot resolve.
    #[test]
    fn test_judge_offers_entries_for_a_root_and_qualifies_other_repositories() {
        let theirs = RecordCommits {
            path: path("hotfix-to-prod"),
            sequence: 2,
            items: [(
                Item::Value("webImage".to_owned()),
                Some([at("/context", &web(), 'c')].into()),
            )]
            .into(),
        };
        let ours: BTreeMap<Item, Option<Commits>> = [(
            Item::Value("webImage".to_owned()),
            Some([at("/context", &web(), 'd')].into()),
        )]
        .into();
        let plan = plan(&path("dev-to-prod"), &ours, &[theirs]);
        let check = plan.checks().into_iter().next().unwrap();
        let mut root = missing('a', "Initial");
        root.root = true;
        let answers = BTreeMap::from([(check, Answer::Missing(vec![root, missing('c', "Fix")]))]);
        let refusal = judge_with(&plan, &answers, &[]).unwrap_err();
        assert_eq!(
            refusal.unsatisfied[0].supersede,
            [format!("web:{}", commit('a')), format!("web:{}", commit('c'))]
        );
        assert!(refusal
            .finding
            .fix
            .contains(&format!("--supersede web:{}=<reason>", commit('a'))));
    }

    /// A check that finds nothing missing satisfies the obligation before
    /// one whose missing commits an entry covers, so no commit is recorded
    /// as superseded that the promotion keeps. Catches a record listing
    /// superseded commits the promoted image still contains.
    #[test]
    fn test_judge_prefers_a_check_with_nothing_missing() {
        let theirs = RecordCommits {
            path: path("hotfix-to-prod"),
            sequence: 2,
            items: [(
                Item::Value("webImage".to_owned()),
                Some([at("/lib", &SourceRepository::This, 'c')].into()),
            )]
            .into(),
        };
        let ours: BTreeMap<Item, Option<Commits>> = [(
            Item::Value("webImage".to_owned()),
            Some(
                [
                    at("/context", &SourceRepository::This, 'd'),
                    at("/other", &SourceRepository::This, 'e'),
                ]
                .into(),
            ),
        )]
        .into();
        let plan = plan(&path("dev-to-prod"), &ours, &[theirs]);
        let answers: BTreeMap<Check, Answer> = plan
            .checks()
            .into_iter()
            .map(|check| {
                let answer = if check.value == commit('d') {
                    Answer::Missing(vec![missing('c', "Fix")])
                } else {
                    Answer::Missing(Vec::new())
                };
                (check, answer)
            })
            .collect();
        let accepted = judge_with(&plan, &answers, &[entry(SourceRepository::This, &['c'], "x")]).unwrap();
        assert!(accepted.superseded.is_empty());
    }

    /// An entry that covers only some missing commits keeps the promotion
    /// refused, naming only the rest; entries that cover all pass and the
    /// record lists each missing commit with its repository, reason, and
    /// who gave it, oldest first, never the range or commits that were not
    /// missing. Catches a partial supersede passing, or a range recorded.
    #[test]
    fn test_judge_records_each_superseded_commit() {
        let (plan, check) = one_plan();
        let answers = BTreeMap::from([(
            check,
            Answer::Missing(vec![missing('a', "Fix cache"), missing('c', "Raise cache size")]),
        )]);
        let partial = judge_with(&plan, &answers, &[entry(SourceRepository::This, &['a'], "squashed")]).unwrap_err();
        assert!(partial.finding.message.contains("Raise cache size"));
        assert!(!partial.finding.message.contains("Fix cache,"));
        assert_eq!(partial.unsatisfied[0].missing, [missing('c', "Raise cache size")]);

        let accepted = judge_with(
            &plan,
            &answers,
            &[entry(SourceRepository::This, &['b', 'c', 'a'], "squashed")],
        )
        .unwrap();
        assert_eq!(
            accepted
                .superseded
                .iter()
                .map(|commit| (commit.commit.clone(), commit.reason.as_str(), commit.by.as_str()))
                .collect::<Vec<_>>(),
            [
                (commit('a'), "squashed", "release-bot"),
                (commit('c'), "squashed", "release-bot")
            ]
        );
        assert_eq!(accepted.superseded[0].repository, "project");

        // An entry in another repository covers nothing here.
        assert!(judge_with(&plan, &answers, &[entry(web(), &['a', 'c'], "other")]).is_err());
    }

    /// A record whose commit cannot be fetched is unchecked, never a
    /// refusal of every later promotion; an entry naming its commit
    /// supersedes it. Catches a deleted hotfix branch blocking prod.
    #[test]
    fn test_judge_leaves_an_unfetchable_record_unchecked_unless_superseded() {
        let (plan, check) = one_plan();
        let answers = BTreeMap::from([(check, Answer::Unavailable)]);
        let accepted = judge_with(&plan, &answers, &[]).unwrap();
        assert!(accepted.superseded.is_empty());
        assert_eq!(accepted.unchecked.len(), 2, "{:?}", accepted.unchecked);
        assert!(accepted.unchecked[0].message.contains("hotfix-to-prod #2"));
        let superseded = judge_with(&plan, &answers, &[entry(SourceRepository::This, &['c'], "gone")]).unwrap();
        assert!(superseded.unchecked.is_empty());
        assert_eq!(superseded.superseded.len(), 1);
        assert_eq!(superseded.superseded[0].commit, commit('c'));
    }

    /// Entries parse with an optional repository qualifier and reason, and
    /// one without a reason needs `--reason`
    /// (`NYL-PROMOTE-SUPERSEDE-REASON`, exit 1). Catches a reason-less
    /// supersede recorded, or a reason split at a `:` it contains.
    #[test]
    fn test_supersede_entries_parse_and_need_a_reason() {
        let entry = SupersedeRequest::parse("web:a1b2^..c3d4=merged as one: squash").unwrap();
        assert_eq!(entry.repository.as_deref(), Some("web"));
        assert_eq!(entry.range, "a1b2^..c3d4");
        assert_eq!(entry.reason.as_deref(), Some("merged as one: squash"));
        let bare = SupersedeRequest::parse("a1b2..c3d4").unwrap();
        assert_eq!((bare.repository, bare.reason), (None, None));
        let url = SupersedeRequest::parse("https://git.example.invalid/web.git:a1b2").unwrap();
        assert_eq!(url.repository.as_deref(), Some(WEB));
        for malformed in ["", "a1b2=", ":a1b2", "a b", "--all=x"] {
            assert!(SupersedeRequest::parse(malformed).is_err(), "{malformed}");
        }

        let entries = [
            SupersedeRequest::parse("a..b").unwrap(),
            SupersedeRequest::parse("c=why").unwrap(),
        ];
        let refused = require_reasons(&entries, None).unwrap_err();
        assert_eq!(refused.code, Code::PromoteSupersedeReason);
        assert_eq!(crate::run_report::ExitCategory::of_finding(&refused).code(), 1);
        assert!(refused.message.contains("a..b") && !refused.message.contains("c,"));
        assert!(require_reasons(&entries, Some("Release 3")).is_ok());
        assert!(require_reasons(&entries[1..], None).is_ok());
    }

    /// URLs are compared after normalization, so a GitRepository names the
    /// repository however its URL is spelled, and the project's own
    /// repository takes the name of the GitRepository of its `origin`.
    /// Catches a record naming one repository two ways.
    #[test]
    fn test_repository_names_match_normalized_urls() {
        for url in [
            "https://Git.Example.invalid/org/web.git",
            "https://user:token@git.example.invalid/org/web/",
            "ssh://git@git.example.invalid/org/web.git",
            "git@git.example.invalid:org/web.git",
        ] {
            assert_eq!(normalize_url(url), "git.example.invalid/org/web", "{url}");
        }
        assert_eq!(normalize_url("/srv/git/web.git"), "/srv/git/web");
        let declared = [("platform".to_owned(), "git@git.example.invalid:org/platform".to_owned())];
        let names = RepositoryNames::new(&declared, Some("https://git.example.invalid/org/platform.git"));
        assert_eq!(names.name_of(&SourceRepository::This), "platform");
        assert_eq!(
            names.name_of(&SourceRepository::Url(
                "ssh://git@git.example.invalid/org/platform".to_owned()
            )),
            "platform"
        );
        assert_eq!(
            names.name_of(&SourceRepository::Url(
                "https://git.example.invalid/org/lib.git".to_owned()
            )),
            "git.example.invalid/org/lib"
        );
        let unnamed = RepositoryNames::new(&[], Some("https://git.example.invalid/org/app.git"));
        assert_eq!(unnamed.name_of(&SourceRepository::This), "git.example.invalid/org/app");
        assert_eq!(RepositoryNames::default().name_of(&SourceRepository::This), "project");
        assert!(names.names(&SourceRepository::This, "https://git.example.invalid/org/platform"));
    }

    /// A qualifier names one of the repositories the check compares in, by
    /// its name or URL; another is refused with the names to use.
    #[test]
    fn test_qualified_entries_name_a_compared_repository() {
        let repositories = BTreeSet::from([SourceRepository::This, web()]);
        let names = names();
        let resolve = |text: &str| qualified(&SupersedeRequest::parse(text).unwrap(), &repositories, &names);
        assert_eq!(resolve("web:a1b2").unwrap(), Some(web()));
        assert_eq!(resolve("https://git.example.invalid/web:a1b2").unwrap(), Some(web()));
        assert_eq!(resolve("a1b2").unwrap(), None);
        let unknown = resolve("api:a1b2").unwrap_err();
        assert!(
            unknown.fix.contains("project") && unknown.fix.contains("web"),
            "{}",
            unknown.fix
        );
    }

    /// With two repositories, an entry belongs to the repository whose
    /// missing commits it names: a qualified short range in its own, and an
    /// unqualified full ID that resolves in both (they share one object
    /// database) in the one it is missing from; one that names missing
    /// commits in both must be qualified. Catches every unqualified ID
    /// refused as ambiguous, or one applied to the wrong repository.
    #[test]
    fn test_match_entry_uses_each_repository_s_missing_commits() {
        let wanted = BTreeMap::from([
            (SourceRepository::This, BTreeSet::from([commit('a')])),
            (web(), BTreeSet::from([commit('b'), commit('c')])),
        ]);
        let names = names();
        let short = SupersedeRequest::parse("web:bbbbbbb^..ccccccc").unwrap();
        let matched = match_entry(
            &short,
            "r".to_owned(),
            &BTreeMap::from([(web(), vec![commit('c'), commit('b')])]),
            &wanted,
            &names,
        )
        .unwrap()
        .unwrap();
        assert_eq!(matched.repository, web());
        assert_eq!(matched.commits, BTreeSet::from([commit('b'), commit('c')]));

        let full = SupersedeRequest::parse(commit('b').as_str()).unwrap();
        let everywhere = BTreeMap::from([(SourceRepository::This, vec![commit('b')]), (web(), vec![commit('b')])]);
        let matched = match_entry(&full, "r".to_owned(), &everywhere, &wanted, &names)
            .unwrap()
            .unwrap();
        assert_eq!(matched.repository, web());

        let both = BTreeMap::from([
            (SourceRepository::This, vec![commit('a'), commit('b')]),
            (web(), vec![commit('a'), commit('b')]),
        ]);
        let ambiguous = match_entry(&full, "r".to_owned(), &both, &wanted, &names).unwrap_err();
        assert!(ambiguous.message.contains("project") && ambiguous.message.contains("web"));

        let elsewhere = BTreeMap::from([(SourceRepository::This, vec![commit('e')])]);
        assert_eq!(
            match_entry(&full, "r".to_owned(), &elsewhere, &wanted, &names).unwrap(),
            None
        );
    }

    /// A value's source commits are its producer's resolved Git source
    /// fields, every repository on its own; an input filled by a reference
    /// follows it to the producer; a unit with no source field has none.
    /// Catches an image checked by the environment's source commit when its
    /// context pins another repository.
    #[test]
    fn test_value_commits_follow_the_producer_s_source_fields() {
        let mut image = desired("web-image", "OciImage", json!({}), "image");
        image.resolved_spec = Some(json!({
            "context": {"path": "/services/web", "commit": "a".repeat(40)},
            "contexts": {"lib": {"path": "/", "commit": "b".repeat(40), "repository": {"repoURL": "https://git.example.invalid/lib.git"}}},
        }));
        let mut publication = desired("kubernetes", "KubernetesPublication", json!({}), "pub");
        publication.resolved_spec = Some(json!({"releases": {"web/web": {"image": "registry/web@sha256:1"}}}));
        publication.provenance.insert(
            nyl_core::orchestration::ids::JsonPointer::new("/releases/web~1web/image").unwrap(),
            Provenance::Receipt(nyl_core::orchestration::state::ReceiptProvenance {
                environment: None,
                unit: name("web-image"),
                uid: image.unit.uid.clone().unwrap(),
                execution_key: crate::rules::fixtures::key("image"),
                observed_commit: None,
                run: None,
                observation: None,
                observed_at: None,
            }),
        );
        let desired = BTreeMap::from([(name("web-image"), image), (name("kubernetes"), publication)]);
        let fields = |unit: &DesiredUnit| {
            if unit.unit.name.as_str() == "web-image" {
                vec!["/context".to_owned(), "/contexts/lib".to_owned()]
            } else {
                Vec::new()
            }
        };
        let expected: Commits = [
            at("/context", &SourceRepository::This, 'a'),
            at(
                "/contexts/lib",
                &SourceRepository::Url("https://git.example.invalid/lib.git".to_owned()),
                'b',
            ),
        ]
        .into();
        let artifact = PromotionSelect {
            unit: Some(name("web-image")),
            artifact: Some("image".to_owned()),
            pointer: Some("/reference".to_owned()),
            ..PromotionSelect::default()
        };
        assert_eq!(value_commits(&artifact, &desired, &fields), Some(expected.clone()));
        let input = PromotionSelect {
            unit: Some(name("kubernetes")),
            input: Some("/releases/web~1web/image".to_owned()),
            ..PromotionSelect::default()
        };
        assert_eq!(value_commits(&input, &desired, &fields), Some(expected));
        let literal = PromotionSelect {
            unit: Some(name("kubernetes")),
            input: Some("/releases".to_owned()),
            ..PromotionSelect::default()
        };
        assert_eq!(value_commits(&literal, &desired, &fields), None);
    }
}
