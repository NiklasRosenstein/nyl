//! The supersede check: what a promotion on one path may replace of what
//! other paths promoted into the same environment.
//!
//! Contract: [Several paths into one environment](../../../../../design/promotion.md#several-paths-into-one-environment)
//! ("Rule", "Source commit of a value", "Refusal", "Override", "Record") and
//! decision D5 of the [M6 plan](../../../../../design/m6-plan.md#open-decisions).
//!
//! - **Rule.** Before writing an item on path P (the target's source commit,
//!   or a value), every record for that item that another path wrote since P
//!   last promoted it must be contained in the item's source commits: per
//!   repository, [`SourceResolver::missing_commits`] (`git cherry`) of the
//!   new item's commit against the record's lists nothing. [`plan`] lists
//!   these obligations; the shell answers each [`Check`] once.
//! - **Source commits.** The source item has the state's source commit; a
//!   value has the commits its producing unit's desired document resolved
//!   into its Git source fields ([`value_commits`]). A value without one is
//!   unchecked (`NYL-PROMOTE-UNCHECKED`, a warning), and so is a record's
//!   value whose source state cannot be read.
//! - **Override.** `--supersede [<repository>:]<commit or range>[=<reason>]`
//!   ([`SupersedeRequest`]) accepts missing commits as replaced; an entry
//!   with neither its own reason nor `--reason` is refused
//!   ([`require_reasons`], `NYL-PROMOTE-SUPERSEDE-REASON`, exit 1). Commits a
//!   range names that are not missing are ignored.
//! - **Refusal.** A missing commit no entry covers refuses the promotion
//!   ([`judge`], `NYL-PROMOTE-SUPERSEDES`, exit 2), listing per record the
//!   missing commits with their subjects and a range to copy.
//! - **Record.** Each superseded commit is recorded on its own, with its
//!   repository, reason, and who gave it, never the range.
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

/// The source commits of one item, each in its own repository.
pub type Commits = BTreeSet<(SourceRepository, CommitId)>;

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

/// A commit of another path's record that an item must contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub item: Item,
    /// The other path and the sequence of its record.
    pub path: PromotionPathName,
    pub sequence: u64,
    pub repository: SourceRepository,
    pub record: CommitId,
    /// One check per source commit the item has in `repository`; any one
    /// that finds nothing missing satisfies the obligation.
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

    /// The repositories the obligations compare in.
    pub fn repositories(&self) -> BTreeSet<SourceRepository> {
        self.obligations
            .iter()
            .map(|obligation| obligation.repository.clone())
            .collect()
    }
}

/// The obligations of a promotion on `path` that writes `writes`, against
/// `records`: every record into the target (current, and from the target's
/// history), on any path. Records on `path` itself only say since when
/// another path's records count: for each item, after the newest record on
/// `path` that carries it.
pub fn plan(path: &PromotionPathName, writes: &BTreeMap<Item, Option<Commits>>, records: &[RecordCommits]) -> Plan {
    let mut plan = Plan::default();
    let mut unchecked = BTreeSet::new();
    for (item, written) in writes {
        let since = records
            .iter()
            .filter(|record| record.path == *path && record.items.contains_key(item))
            .map(|record| record.sequence)
            .max()
            .unwrap_or(0);
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
                unchecked.insert(format!(
                    "{item} of record {} #{} has no known source commit, so path {path} cannot check that it keeps it",
                    record.path, record.sequence
                ));
                continue;
            };
            let Some(written) = written else {
                unchecked.insert(format!(
                    "{item} has no known source commit, so path {path} cannot check that it keeps what record {} #{} promoted",
                    record.path, record.sequence
                ));
                continue;
            };
            for (repository, commit) in theirs {
                let checks: Vec<Check> = written
                    .iter()
                    .filter(|(written_repository, _)| written_repository == repository)
                    .map(|(_, value)| Check {
                        repository: repository.clone(),
                        value: value.clone(),
                        record: commit.clone(),
                    })
                    .collect();
                if checks.is_empty() {
                    unchecked.insert(format!(
                        "{item} has no source commit in {} (repository of record {} #{}'s {commit}), so path {path} cannot check that it keeps it",
                        describe(repository),
                        record.path,
                        record.sequence
                    ));
                    continue;
                }
                plan.obligations.push(Obligation {
                    item: item.clone(),
                    path: record.path.clone(),
                    sequence: record.sequence,
                    repository: repository.clone(),
                    record: commit.clone(),
                    checks,
                });
            }
        }
    }
    plan.unchecked = unchecked
        .into_iter()
        .map(|message| {
            Finding::new(
                Code::PromoteUnchecked,
                message,
                "check by hand that the promotion keeps what the other path promoted, such as by its image's build inputs",
            )
        })
        .collect();
    plan
}

fn describe(repository: &SourceRepository) -> String {
    match repository {
        SourceRepository::This => "this repository".to_owned(),
        SourceRepository::Url(url) => url.clone(),
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
        commits.insert((repository, commit));
    }
    (!commits.is_empty()).then_some(commits)
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
/// the obligations compare in) by their `names`.
pub fn qualified(
    entry: &SupersedeRequest,
    repositories: &BTreeSet<SourceRepository>,
    names: &BTreeMap<SourceRepository, String>,
) -> Result<Option<SourceRepository>, Finding> {
    let Some(wanted) = &entry.repository else {
        return Ok(None);
    };
    repositories
        .iter()
        .find(|repository| {
            names.get(*repository) == Some(wanted) || matches!(repository, SourceRepository::Url(url) if url == wanted)
        })
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
                        .map(|repository| name_of(repository, names))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
}

/// The name a refusal prints for a repository, which qualifies an entry.
pub fn name_of(repository: &SourceRepository, names: &BTreeMap<SourceRepository, String>) -> String {
    names.get(repository).cloned().unwrap_or_else(|| match repository {
        SourceRepository::This => "project".to_owned(),
        SourceRepository::Url(url) => url.clone(),
    })
}

/// A `--supersede` entry resolved to the commits it names in its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeEntry {
    pub repository: SourceRepository,
    pub commits: BTreeSet<CommitId>,
    pub reason: String,
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
}

/// Why the promotion is refused, with what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeRefusal {
    pub finding: Finding,
    pub unsatisfied: Vec<Unsatisfied>,
}

/// What the check decided: the superseded commits the record lists, or the
/// refusal. `answers` holds each check's missing commits; `entries` the
/// resolved `--supersede` entries; `by` who gives them; `recorded` the
/// repository string the record names each repository by.
pub struct Judged<'a> {
    pub path: &'a PromotionPathName,
    pub source: &'a EnvironmentName,
    pub target: &'a EnvironmentName,
    pub by: &'a str,
    pub names: &'a BTreeMap<SourceRepository, String>,
    pub recorded: &'a dyn Fn(&SourceRepository) -> String,
}

/// Decide the check: each obligation is satisfied by a check whose missing
/// commits the entries cover entirely, and the record lists the covered
/// ones; otherwise `NYL-PROMOTE-SUPERSEDES` (exit 2) lists every obligation
/// still unsatisfied.
pub fn judge(
    context: &Judged<'_>,
    plan: &Plan,
    answers: &BTreeMap<Check, Vec<MissingCommit>>,
    entries: &[SupersedeEntry],
) -> Result<Vec<SupersededCommit>, SupersedeRefusal> {
    let covering = |repository: &SourceRepository, commit: &CommitId| {
        entries
            .iter()
            .find(|entry| entry.repository == *repository && entry.commits.contains(commit))
    };
    let mut superseded: Vec<SupersededCommit> = Vec::new();
    let mut unsatisfied: Vec<Unsatisfied> = Vec::new();
    for obligation in &plan.obligations {
        let mut best: Option<(&Check, Vec<MissingCommit>)> = None;
        let mut satisfied = None;
        for check in &obligation.checks {
            let missing = answers.get(check).cloned().unwrap_or_default();
            let uncovered: Vec<MissingCommit> = missing
                .iter()
                .filter(|commit| covering(&check.repository, &commit.commit).is_none())
                .cloned()
                .collect();
            if uncovered.is_empty() {
                satisfied = Some(missing);
                break;
            }
            if best.as_ref().is_none_or(|(_, known)| uncovered.len() < known.len()) {
                best = Some((check, uncovered));
            }
        }
        if let Some(missing) = satisfied {
            for commit in missing {
                let entry = covering(&obligation.repository, &commit.commit).expect("covered");
                let repository = (context.recorded)(&obligation.repository);
                if !superseded
                    .iter()
                    .any(|known| known.commit == commit.commit && known.repository == repository)
                {
                    superseded.push(SupersededCommit {
                        repository,
                        commit: commit.commit,
                        reason: entry.reason.clone(),
                        by: context.by.to_owned(),
                    });
                }
            }
            continue;
        }
        let (check, missing) = best.expect("an obligation has a check");
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
            continue;
        }
        unsatisfied.push(Unsatisfied {
            path: obligation.path.clone(),
            sequence: obligation.sequence,
            repository: check.repository.clone(),
            value: check.value.clone(),
            record: check.record.clone(),
            items: vec![obligation.item.clone()],
            missing,
        });
    }
    if unsatisfied.is_empty() {
        return Ok(superseded);
    }
    Err(SupersedeRefusal {
        finding: refusal_finding(context, &unsatisfied),
        unsatisfied,
    })
}

/// 12 characters of a commit, as messages abbreviate it.
fn short(commit: &CommitId) -> &str {
    &commit.as_str()[..12]
}

/// The range a refusal offers to copy: from the parent of the oldest missing
/// commit to the record's commit.
pub fn range_of(unsatisfied: &Unsatisfied) -> String {
    let oldest = unsatisfied
        .missing
        .first()
        .map_or(&unsatisfied.record, |commit| &commit.commit);
    format!("{}^..{}", short(oldest), short(&unsatisfied.record))
}

fn refusal_finding(context: &Judged<'_>, unsatisfied: &[Unsatisfied]) -> Finding {
    let several = unsatisfied
        .iter()
        .map(|unsatisfied| &unsatisfied.repository)
        .collect::<BTreeSet<_>>()
        .len()
        > 1;
    let qualified = |unsatisfied: &Unsatisfied| {
        if several {
            format!(
                "{}:{}",
                name_of(&unsatisfied.repository, context.names),
                range_of(unsatisfied)
            )
        } else {
            range_of(unsatisfied)
        }
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
                format!(" in {}", name_of(&unsatisfied.repository, context.names))
            } else {
                String::new()
            };
            format!(
                "{} is missing {count} commit{} from {} #{} ({}){repository}: {} ({commits})",
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
                qualified(unsatisfied),
            )
        })
        .collect();
    let first = &unsatisfied[0];
    Finding::new(
        Code::PromoteSupersedes,
        format!(
            "path {} would replace what other paths promoted into {}: {}; merge them or pass --supersede {}=<reason>",
            context.path,
            context.target,
            parts.join("; "),
            qualified(first)
        ),
        format!(
            "merge the missing commits into the source {} runs, reconcile it, and promote again; or record that this promotion replaces them with `--supersede {}=<reason>` (repeatable)",
            context.source,
            qualified(first)
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{desired, name};

    fn commit(fill: char) -> CommitId {
        CommitId::new(fill.to_string().repeat(40)).unwrap()
    }

    fn path(name: &str) -> PromotionPathName {
        PromotionPathName::new(name).unwrap()
    }

    fn this(fill: char) -> Commits {
        [(SourceRepository::This, commit(fill))].into()
    }

    /// A record that carries the source commit and `webImage`, both built
    /// from the same commit, as a source path's records do.
    fn source_record(on: &str, sequence: u64, fill: char) -> RecordCommits {
        RecordCommits {
            path: path(on),
            sequence,
            items: [
                (Item::Source, Some(this(fill))),
                (Item::Value("webImage".to_owned()), Some(this(fill))),
            ]
            .into(),
        }
    }

    fn writes(fill: char) -> BTreeMap<Item, Option<Commits>> {
        [
            (Item::Source, Some(this(fill))),
            (Item::Value("webImage".to_owned()), Some(this(fill))),
        ]
        .into()
    }

    fn missing(fill: char, subject: &str) -> MissingCommit {
        MissingCommit {
            commit: commit(fill),
            subject: subject.to_owned(),
        }
    }

    fn judge_with(
        plan: &Plan,
        answers: &BTreeMap<Check, Vec<MissingCommit>>,
        entries: &[SupersedeEntry],
    ) -> Result<Vec<SupersededCommit>, SupersedeRefusal> {
        let names = BTreeMap::new();
        let recorded = |_: &SourceRepository| "https://git.example.invalid/platform.git".to_owned();
        let context = Judged {
            path: &path("dev-to-prod"),
            source: &EnvironmentName::new("dev").unwrap(),
            target: &EnvironmentName::new("prod").unwrap(),
            by: "release-bot",
            names: &names,
            recorded: &recorded,
        };
        judge(&context, plan, answers, entries)
    }

    /// Only records of other paths written after this path's own record
    /// count, each item for itself, and one check answers the source and a
    /// value built from the same commit. Catches a check against this path's
    /// own history, or against what this path already promoted past.
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

    /// Missing commits refuse with `NYL-PROMOTE-SUPERSEDES` (exit 2),
    /// naming the other path, every missing commit by subject, and a range
    /// to pass to `--supersede`; nothing missing passes with no superseded
    /// commits. Catches a refusal a user cannot act on.
    #[test]
    fn test_judge_refuses_missing_commits_with_the_range_to_supersede() {
        let plan = plan(
            &path("dev-to-prod"),
            &writes('d'),
            &[source_record("hotfix-to-prod", 2, 'c')],
        );
        let check = plan.checks().into_iter().next().unwrap();
        let answers = BTreeMap::from([(
            check.clone(),
            vec![missing('a', "Fix pool size"), missing('c', "Bump timeout")],
        )]);
        let refusal = judge_with(&plan, &answers, &[]).unwrap_err();
        assert_eq!(refusal.finding.code, Code::PromoteSupersedes);
        assert_eq!(crate::run_report::ExitCategory::of_finding(&refusal.finding).code(), 2);
        for expected in [
            "hotfix-to-prod #2",
            "Fix pool size",
            "Bump timeout",
            "--supersede aaaaaaaaaaaa^..cccccccccccc",
        ] {
            assert!(
                refusal.finding.message.contains(expected),
                "{}",
                refusal.finding.message
            );
        }
        assert_eq!(refusal.unsatisfied.len(), 1);
        assert_eq!(
            refusal.unsatisfied[0].items,
            [Item::Source, Item::Value("webImage".to_owned())]
        );

        let none = BTreeMap::from([(check, Vec::new())]);
        assert_eq!(judge_with(&plan, &none, &[]).unwrap(), Vec::new());
    }

    /// An entry that covers only some missing commits keeps the promotion
    /// refused, naming only the rest; entries that cover all pass and the
    /// record lists each missing commit with its reason and who gave it,
    /// oldest first, never the range or commits that were not missing.
    /// Catches a partial supersede passing, or a range recorded as given.
    #[test]
    fn test_judge_records_each_superseded_commit() {
        let plan = plan(
            &path("dev-to-prod"),
            &writes('d'),
            &[source_record("hotfix-to-prod", 2, 'c')],
        );
        let check = plan.checks().into_iter().next().unwrap();
        let answers = BTreeMap::from([(check, vec![missing('a', "Fix cache"), missing('c', "Raise cache size")])]);
        let entry = |commits: &[char], reason: &str| SupersedeEntry {
            repository: SourceRepository::This,
            commits: commits.iter().map(|fill| commit(*fill)).collect(),
            reason: reason.to_owned(),
        };
        let partial = judge_with(&plan, &answers, &[entry(&['a'], "squashed")]).unwrap_err();
        assert!(partial.finding.message.contains("Raise cache size"));
        assert!(!partial.finding.message.contains("Fix cache ("));
        assert_eq!(partial.unsatisfied[0].missing, [missing('c', "Raise cache size")]);

        let superseded = judge_with(&plan, &answers, &[entry(&['b', 'c', 'a'], "squashed")]).unwrap();
        assert_eq!(
            superseded
                .iter()
                .map(|commit| (commit.commit.clone(), commit.reason.as_str(), commit.by.as_str()))
                .collect::<Vec<_>>(),
            [
                (commit('a'), "squashed", "release-bot"),
                (commit('c'), "squashed", "release-bot")
            ]
        );
        assert_eq!(superseded[0].repository, "https://git.example.invalid/platform.git");

        // An entry in another repository covers nothing here.
        let elsewhere = SupersedeEntry {
            repository: SourceRepository::Url("https://git.example.invalid/web.git".to_owned()),
            ..entry(&['a', 'c'], "other")
        };
        assert!(judge_with(&plan, &answers, &[elsewhere]).is_err());
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
        assert_eq!(url.repository.as_deref(), Some("https://git.example.invalid/web.git"));
        for malformed in ["", "a1b2=", ":a1b2", "a b", "--all=x"] {
            assert!(SupersedeRequest::parse(malformed).is_err(), "{malformed}");
        }

        let entries = [bare_entry("a..b"), SupersedeRequest::parse("c=why").unwrap()];
        let refused = require_reasons(&entries, None).unwrap_err();
        assert_eq!(refused.code, Code::PromoteSupersedeReason);
        assert_eq!(crate::run_report::ExitCategory::of_finding(&refused).code(), 1);
        assert!(refused.message.contains("a..b") && !refused.message.contains("c,"));
        assert!(require_reasons(&entries, Some("Release 3")).is_ok());
        assert!(require_reasons(&entries[1..], None).is_ok());
    }

    fn bare_entry(range: &str) -> SupersedeRequest {
        SupersedeRequest::parse(range).unwrap()
    }

    /// A qualifier names one of the repositories the check compares in, by
    /// its project name or URL; another is refused with the names to use.
    #[test]
    fn test_qualified_entries_name_a_compared_repository() {
        let web = SourceRepository::Url("https://git.example.invalid/web.git".to_owned());
        let repositories = BTreeSet::from([SourceRepository::This, web.clone()]);
        let names = BTreeMap::from([(web.clone(), "web".to_owned())]);
        let resolve = |text: &str| qualified(&SupersedeRequest::parse(text).unwrap(), &repositories, &names);
        assert_eq!(resolve("web:a1b2").unwrap(), Some(web.clone()));
        assert_eq!(resolve("https://git.example.invalid/web.git:a1b2").unwrap(), Some(web));
        assert_eq!(resolve("a1b2").unwrap(), None);
        let unknown = resolve("api:a1b2").unwrap_err();
        assert!(
            unknown.fix.contains("project") && unknown.fix.contains("web"),
            "{}",
            unknown.fix
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
            (SourceRepository::This, commit('a')),
            (
                SourceRepository::Url("https://git.example.invalid/lib.git".to_owned()),
                commit('b'),
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
