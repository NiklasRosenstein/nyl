//! Sliding expiry and `maxInstances` of template instances.
//!
//! Contract: [Expiry](../../../../design/orchestration-core.md#expiry) and
//! [Instances](../../../../design/orchestration-core.md#instances).
//!
//! Nyl has no daemon, so expiry is decided by whichever command touches an
//! instance, from the `expiresAt` its record holds and the injected clock's
//! time. Nothing here reads a clock, state, or the process:
//!
//! - [`decide`] says what a reconcile does with one instance for what its
//!   invocation says ([`Intent`]): extend the expiry, reconcile as
//!   maintenance, or remove an expired instance. Only activity extends it.
//! - [`guard_removal`] is the check a removal repeats under the lease, so an
//!   instance renewed between the decision and the teardown is kept.
//! - [`renew_units`] is what a renewal does to the units of an instance whose
//!   removal was recorded but has not started: they are active again.
//! - [`removal_action`] and [`eviction_result`] map what a removal did to what
//!   its report says.
//! - [`admit`] decides whether a template at `maxInstances` admits another
//!   instance, and which expired instances `--make-room` removes first:
//!   oldest expiry first, only as many as needed, never a live instance;
//!   [`Eviction`] follows the removals to that end.
//!
//! A template without `ttl` never expires, whatever an older record says.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::state::{DesiredUnit, StateRecord};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::lifecycle::{transition, Event, Next};
use crate::run_report::InstanceAction;

/// What an invocation says about an instance's expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// `reconcile -e <instance>`: activity. It extends a live instance and
    /// removes an expired one.
    Activity,
    /// `reconcile -e <instance> --no-extend` and the fleet reconcile
    /// (`reconcile --template <name>`): maintenance. It never extends, and
    /// removes an expired instance.
    Maintenance,
    /// `--renew`, `state init --template`, and `reconcile -e <instance>
    /// --template <name>`: activity that keeps an expired instance, because
    /// updating an active pull request never removes its preview.
    Renew,
}

/// What a reconcile does with one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Reconcile it; with `extend`, move `expiresAt` to now plus `ttl` first.
    Reconcile { extend: bool },
    /// The instance is expired: remove it (`teardown --all`, then `state
    /// delete`) instead of reconciling it.
    Remove,
}

/// Whether an instance is expired at `now`: its template has a `ttl` and the
/// recorded `expiresAt` has been reached.
pub fn expired(expires_at: Option<Timestamp>, ttl: Option<Duration>, now: Timestamp) -> bool {
    ttl.is_some() && expires_at.is_some_and(|at| at <= now)
}

/// The `expiresAt` activity at `now` records: now plus `ttl`, or none for a
/// template without one.
pub fn extended(ttl: Option<Duration>, now: Timestamp) -> Option<Timestamp> {
    ttl.map(|ttl| ttl.after(now))
}

/// What a reconcile with `intent` does with an instance whose record holds
/// `expires_at`.
pub fn decide(intent: Intent, expires_at: Option<Timestamp>, ttl: Option<Duration>, now: Timestamp) -> Disposition {
    match (intent, expired(expires_at, ttl, now)) {
        (Intent::Maintenance | Intent::Activity, true) => Disposition::Remove,
        (Intent::Maintenance, false) => Disposition::Reconcile { extend: false },
        (Intent::Renew, _) | (Intent::Activity, false) => Disposition::Reconcile { extend: true },
    }
}

/// The `expiresAt` a removal of `environment` is decided on: the record's,
/// when the instance is expired at `now`. An instance that is not (renewed
/// since a listing, or never expired) is `NYL-INSTANCE-RENEWED`, exit 2, and
/// nothing is removed.
pub fn decided_expiry(
    environment: &EnvironmentName,
    expires_at: Option<Timestamp>,
    ttl: Option<Duration>,
    now: Timestamp,
) -> Result<Timestamp, Finding> {
    expires_at.filter(|_| expired(expires_at, ttl, now)).ok_or_else(|| {
        Finding::new(
            Code::InstanceRenewed,
            format!("{environment} is not expired, so nothing is removed"),
            format!(
                "run `nyl reconcile -e {environment}` again; the next run decides on the instance's current expiry"
            ),
        )
    })
}

/// The check a removal repeats under the lease: the record still holds the
/// `expiresAt` the decision was made on. Anything else means another run
/// renewed or removed the instance meanwhile, so nothing is torn down
/// (`NYL-INSTANCE-RENEWED`, exit 2).
pub fn guard_removal(
    environment: &EnvironmentName,
    decided_on: Timestamp,
    record: Option<&StateRecord>,
) -> Result<(), Finding> {
    if record.is_some_and(|record| record.expires_at == Some(decided_on)) {
        return Ok(());
    }
    Err(Finding::new(
        Code::InstanceRenewed,
        format!("{environment} was renewed or changed while its removal started, so nothing was removed"),
        format!("run `nyl reconcile -e {environment}` again; the next run decides on the instance's current expiry"),
    ))
}

/// The units a renewal writes: those whose `expired` deletion has not started
/// are active again, in the same commit that moves the expiry, so a renewed
/// preview is not left fenced behind a removal nobody authorized. A teardown
/// that has started continues (the units are recreated after it completes).
pub fn renew_units(desired: &BTreeMap<UnitName, DesiredUnit>) -> BTreeMap<UnitName, Option<DesiredUnit>> {
    desired
        .iter()
        .filter_map(|(name, unit)| match transition(&unit.lifecycle, Event::Renewed) {
            Ok(Next::Lifecycle(lifecycle)) => Some((
                name.clone(),
                Some(DesiredUnit {
                    lifecycle,
                    ..unit.clone()
                }),
            )),
            _ => None,
        })
        .collect()
}

/// Whether a removal's teardown is authorized: the template's `allowTeardown`
/// (read at the entry checkout, which is the running job's) or `--allow-teardown`.
pub fn removal_authorized(template_allows: bool, flag: bool) -> bool {
    template_allows || flag
}

/// What a removal did, for its report: `removed` when state was deleted;
/// `removal-pending` when nothing was authorized and units were recorded as
/// pending teardown for that reason; otherwise `removal-incomplete`. A lease
/// held by another run or an instance renewed meanwhile gated nothing, so
/// they are incomplete, never pending.
pub fn removal_action(removed: bool, authorized: bool, error_codes: &[Code], pending_units: usize) -> InstanceAction {
    let interrupted = error_codes
        .iter()
        .any(|code| matches!(code, Code::LeaseHeld | Code::LeaseBroken | Code::InstanceRenewed));
    if removed {
        InstanceAction::Removed
    } else if !authorized && pending_units > 0 && !interrupted {
        InstanceAction::RemovalPending
    } else {
        InstanceAction::RemovalIncomplete
    }
}

/// The finding of a removal that waits for authorization:
/// `NYL-INSTANCE-REMOVAL-PENDING` (exit 2).
pub fn removal_pending_finding(environment: &EnvironmentName, template: &str) -> Finding {
    Finding::new(
        Code::InstanceRemovalPending,
        format!(
            "{environment} is expired, and its teardown waits: the template does not set allowTeardown and --allow-teardown was not given"
        ),
        format!(
            "set allowTeardown: true on EnvironmentTemplate {template}, or pass --allow-teardown to the run that removes expired instances"
        ),
    )
}

/// What a removal of an instance did, as eviction sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionResult {
    /// State was deleted: room was made.
    Removed,
    /// The instance was renewed meanwhile, so it is live and kept.
    Renewed,
    /// The removal did not finish.
    Incomplete,
}

/// The eviction result of a removal reported as `action` with `error_codes`.
pub fn eviction_result(action: InstanceAction, error_codes: &[Code]) -> EvictionResult {
    // State already deleted: another job removed the instance first.
    if action == InstanceAction::Removed || error_codes.contains(&Code::StateDeleted) {
        EvictionResult::Removed
    } else if error_codes.contains(&Code::InstanceRenewed) {
        EvictionResult::Renewed
    } else {
        EvictionResult::Incomplete
    }
}

/// The eviction result of a removal that stopped with the finding `code`
/// before it reported anything.
pub fn eviction_result_of_stop(code: Code) -> EvictionResult {
    eviction_result(InstanceAction::RemovalIncomplete, &[code])
}

/// An instance of a template that counts against `maxInstances`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub name: EnvironmentName,
    pub expires_at: Option<Timestamp>,
}

/// Whether a template admits another instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Create it.
    Admit,
    /// The template is at its limit and `--make-room` is given: remove
    /// expired instances as the eviction directs, then create it.
    Evict(Eviction),
    /// The template is at its limit.
    Refuse(Refusal),
}

/// Why a template refuses another instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub max: u32,
    /// The instances that exist.
    pub count: usize,
    /// How many must go for the new instance to fit.
    pub needed: usize,
    /// The expired instances that `--make-room` would remove, first removed first.
    pub evictable: Vec<EnvironmentName>,
    /// Evictions that were started and did not finish.
    pub incomplete: Vec<EnvironmentName>,
    /// Evictions skipped because the instance was renewed meanwhile.
    pub renewed: Vec<EnvironmentName>,
}

/// The removals `--make-room` performs: expired instances, oldest expiry
/// first, until `needed` are removed. An instance renewed meanwhile is
/// skipped, and one whose removal does not finish is left; either way the
/// next candidate is tried, and if the candidates run out before enough are
/// removed, nothing is created ([`Eviction::refusal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eviction {
    max: u32,
    count: usize,
    needed: usize,
    candidates: Vec<EnvironmentName>,
    removed: Vec<EnvironmentName>,
    incomplete: Vec<EnvironmentName>,
    renewed: Vec<EnvironmentName>,
}

impl Eviction {
    /// The next instance to remove: the oldest expired one not tried yet, while room is still needed.
    pub fn next(&self) -> Option<&EnvironmentName> {
        if self.removed.len() >= self.needed {
            return None;
        }
        self.candidates.iter().find(|candidate| {
            ![&self.removed, &self.incomplete, &self.renewed]
                .iter()
                .any(|tried| tried.contains(candidate))
        })
    }

    /// Record what the removal of `name` did.
    pub fn record(&mut self, name: &EnvironmentName, result: EvictionResult) {
        match result {
            EvictionResult::Removed => self.removed.push(name.clone()),
            EvictionResult::Renewed => self.renewed.push(name.clone()),
            EvictionResult::Incomplete => self.incomplete.push(name.clone()),
        }
    }

    /// The instances removed so far.
    pub fn removed(&self) -> &[EnvironmentName] {
        &self.removed
    }

    /// The refusal once no candidate is left and room was not made; `None`
    /// while it was (or can still be).
    pub fn refusal(&self) -> Option<Refusal> {
        (self.removed.len() < self.needed && self.next().is_none()).then(|| Refusal {
            max: self.max,
            count: self.count,
            needed: self.needed,
            evictable: self.candidates.clone(),
            incomplete: self.incomplete.clone(),
            renewed: self.renewed.clone(),
        })
    }
}

/// Decide whether a template with `max` as `maxInstances` and `existing` as
/// its live instances (the one being created not among them) admits another
/// at `now`. Expired instances count until they are removed.
///
/// Without `make_room`, a template at its limit refuses and names the expired
/// instances that could be removed: removing them is the fleet job's work,
/// never a side effect of another pull request's job. With it, the oldest
/// expired instances go first, and only as many as are needed; a live
/// instance is never evicted, so too few expired ones refuse as well.
pub fn admit(
    max: Option<u32>,
    ttl: Option<Duration>,
    existing: &[Existing],
    now: Timestamp,
    make_room: bool,
) -> Admission {
    let Some(max) = max else {
        return Admission::Admit;
    };
    let count = existing.len();
    let max_usize = usize::try_from(max).unwrap_or(usize::MAX);
    if count < max_usize {
        return Admission::Admit;
    }
    let needed = count - max_usize + 1;
    let mut expired_instances = existing
        .iter()
        .filter(|instance| expired(instance.expires_at, ttl, now))
        .collect::<Vec<_>>();
    expired_instances.sort_by(|left, right| (left.expires_at, &left.name).cmp(&(right.expires_at, &right.name)));
    let candidates = expired_instances
        .into_iter()
        .map(|instance| instance.name.clone())
        .collect::<Vec<_>>();
    if make_room && candidates.len() >= needed {
        return Admission::Evict(Eviction {
            max,
            count,
            needed,
            candidates,
            removed: Vec::new(),
            incomplete: Vec::new(),
            renewed: Vec::new(),
        });
    }
    Admission::Refuse(Refusal {
        max,
        count,
        needed,
        evictable: candidates,
        incomplete: Vec::new(),
        renewed: Vec::new(),
    })
}

/// The finding of a template that refuses another instance: `NYL-INSTANCE-LIMIT`
/// (exit 2).
pub fn limit_finding(template: &str, refusal: &Refusal) -> Finding {
    let list = |names: &[EnvironmentName]| names.iter().map(EnvironmentName::as_str).collect::<Vec<_>>().join(", ");
    let at_limit = format!(
        "EnvironmentTemplate {template} is at maxInstances ({} of {})",
        refusal.count, refusal.max
    );
    if !refusal.incomplete.is_empty() || !refusal.renewed.is_empty() {
        let mut what = Vec::new();
        if !refusal.incomplete.is_empty() {
            what.push(format!(
                "the removal of the expired instances {} has not finished",
                list(&refusal.incomplete)
            ));
        }
        if !refusal.renewed.is_empty() {
            what.push(format!("{} was renewed meanwhile", list(&refusal.renewed)));
        }
        return Finding::new(
            Code::InstanceLimit,
            format!("{at_limit}, and {}", what.join(" and ")),
            "finish their removal (see `nyl status` of each, for example with `--confirm-removed` or `--allow-teardown`), then create the instance again",
        );
    }
    if refusal.evictable.is_empty() {
        return Finding::new(
            Code::InstanceLimit,
            format!("{at_limit}, and no instance is expired"),
            "close a preview with `nyl state delete -e <instance> --teardown`, or raise maxInstances of the template",
        );
    }
    if refusal.evictable.len() < refusal.needed {
        return Finding::new(
            Code::InstanceLimit,
            format!(
                "{at_limit}, and only the expired instances {} could be removed, {} short of the {} needed",
                list(&refusal.evictable),
                refusal.needed - refusal.evictable.len(),
                refusal.needed
            ),
            "close a preview with `nyl state delete -e <instance> --teardown`, or raise maxInstances of the template",
        );
    }
    Finding::new(
        Code::InstanceLimit,
        format!(
            "{at_limit}; the expired instances {} could be removed",
            list(&refusal.evictable)
        ),
        "run the scheduled `nyl reconcile --template <name>` to remove them, or pass --make-room to remove them first",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    fn name(text: &str) -> EnvironmentName {
        EnvironmentName::new(text).unwrap()
    }

    const TTL: Option<Duration> = Some(Duration::from_secs(7 * 86_400));

    fn existing(name_: &str, expires_at: &str) -> Existing {
        Existing {
            name: name(name_),
            expires_at: Some(at(expires_at)),
        }
    }

    /// Catches expiry being decided at the boundary the wrong way round:
    /// an instance is expired from the moment its `expiresAt` is reached, and
    /// one without a `ttl` never is, whatever an older record says.
    #[test]
    fn test_an_instance_is_expired_once_its_expiry_is_reached_and_only_with_a_ttl() {
        let expires = Some(at("2026-10-01T10:00:00Z"));
        assert!(!expired(expires, TTL, at("2026-10-01T09:59:59Z")));
        assert!(expired(expires, TTL, at("2026-10-01T10:00:00Z")));
        assert!(!expired(expires, None, at("2027-01-01T00:00:00Z")));
        assert!(!expired(None, TTL, at("2027-01-01T00:00:00Z")));
        assert_eq!(
            extended(TTL, at("2026-09-24T10:00:00Z")),
            Some(at("2026-10-01T10:00:00Z"))
        );
        assert_eq!(extended(None, at("2026-09-24T10:00:00Z")), None);
    }

    /// The contract's table: activity extends a live instance and removes an
    /// expired one, maintenance never extends, and only a renewal keeps an
    /// expired instance. Catches a plain reconcile that keeps an expired
    /// instance alive forever, or maintenance that extends it.
    #[test]
    fn test_decide_follows_the_expiry_table() {
        let live = Some(at("2026-10-01T10:00:00Z"));
        let now = at("2026-09-30T10:00:00Z");
        let late = at("2026-10-02T10:00:00Z");
        assert_eq!(
            decide(Intent::Activity, live, TTL, now),
            Disposition::Reconcile { extend: true }
        );
        assert_eq!(
            decide(Intent::Maintenance, live, TTL, now),
            Disposition::Reconcile { extend: false }
        );
        assert_eq!(
            decide(Intent::Renew, live, TTL, now),
            Disposition::Reconcile { extend: true }
        );
        assert_eq!(decide(Intent::Activity, live, TTL, late), Disposition::Remove);
        assert_eq!(decide(Intent::Maintenance, live, TTL, late), Disposition::Remove);
        assert_eq!(
            decide(Intent::Renew, live, TTL, late),
            Disposition::Reconcile { extend: true }
        );
    }

    /// Catches a removal that proceeds although another run renewed the
    /// instance between the decision and the lease.
    #[test]
    fn test_a_removal_refuses_when_the_recorded_expiry_changed() {
        let decided = at("2026-10-01T10:00:00Z");
        let record = |expires_at| StateRecord {
            api_version: nyl_core::orchestration::state::StateV1,
            kind: nyl_core::orchestration::state::StateRecordKind,
            environment: name("pr-1"),
            template: Some("preview".to_owned()),
            params: std::collections::BTreeMap::new(),
            expires_at,
            source_commit: None,
            keep_id: None,
            location: nyl_core::orchestration::state::StateLocation {
                repository: "local".to_owned(),
                desired_ref: "d".to_owned(),
                observed_ref: "o".to_owned(),
                path: String::new(),
                coordination_ref_prefix: "refs/nyl/".to_owned(),
            },
            events: Vec::new(),
        };
        assert!(guard_removal(&name("pr-1"), decided, Some(&record(Some(decided)))).is_ok());
        let renewed =
            guard_removal(&name("pr-1"), decided, Some(&record(Some(at("2026-10-08T10:00:00Z"))))).unwrap_err();
        assert_eq!(renewed.code, Code::InstanceRenewed);
        assert_eq!(
            guard_removal(&name("pr-1"), decided, None).unwrap_err().code,
            Code::InstanceRenewed
        );
    }

    /// Below the limit, or without one, any instance is admitted. Catches a
    /// limit that counts wrongly at the boundary.
    #[test]
    fn test_admit_allows_below_the_limit() {
        let now = at("2026-10-01T10:00:00Z");
        let two = [
            existing("a", "2026-10-09T00:00:00Z"),
            existing("b", "2026-10-09T00:00:00Z"),
        ];
        assert_eq!(admit(None, TTL, &two, now, false), Admission::Admit);
        assert_eq!(admit(Some(3), TTL, &two, now, false), Admission::Admit);
        assert!(matches!(admit(Some(2), TTL, &two, now, false), Admission::Refuse(_)));
    }

    /// At the limit without `--make-room`, the refusal names the expired
    /// instances oldest first, and a live instance is never among them.
    #[test]
    fn test_admit_refuses_at_the_limit_naming_the_expired_oldest_first() {
        let now = at("2026-10-10T10:00:00Z");
        let instances = [
            existing("live", "2026-10-20T00:00:00Z"),
            existing("newer", "2026-10-05T00:00:00Z"),
            existing("older", "2026-10-02T00:00:00Z"),
        ];
        let Admission::Refuse(refusal) = admit(Some(3), TTL, &instances, now, false) else {
            panic!("a template at its limit refuses");
        };
        assert_eq!(refusal.evictable, vec![name("older"), name("newer")]);
        assert_eq!((refusal.count, refusal.max, refusal.needed), (3, 3, 1));
        let finding = limit_finding("preview", &refusal);
        assert_eq!(finding.code, Code::InstanceLimit);
        assert!(finding.message.contains("older, newer"));
        assert!(finding.fix.contains("--make-room"));
    }

    fn eviction(admission: Admission) -> Eviction {
        let Admission::Evict(eviction) = admission else {
            panic!("expected an eviction, got {admission:?}");
        };
        eviction
    }

    /// `--make-room` removes only as many expired instances as it needs,
    /// oldest expiry first; catches over-eviction and eviction of a live one.
    #[test]
    fn test_make_room_evicts_the_fewest_oldest_expired_instances() {
        let now = at("2026-10-10T10:00:00Z");
        let instances = [
            existing("live", "2026-10-20T00:00:00Z"),
            existing("newer", "2026-10-05T00:00:00Z"),
            existing("older", "2026-10-02T00:00:00Z"),
        ];
        let mut one = eviction(admit(Some(3), TTL, &instances, now, true));
        assert_eq!(one.next(), Some(&name("older")));
        one.record(&name("older"), EvictionResult::Removed);
        assert_eq!(one.next(), None, "one slot was needed");
        assert!(one.refusal().is_none());
        assert_eq!(one.removed(), [name("older")]);
        // Two slots over the limit need two removals.
        let mut two = eviction(admit(Some(2), TTL, &instances, now, true));
        two.record(&name("older"), EvictionResult::Removed);
        assert_eq!(two.next(), Some(&name("newer")));
        two.record(&name("newer"), EvictionResult::Removed);
        assert_eq!(two.next(), None);
    }

    /// An instance renewed meanwhile is skipped, and one that does not finish
    /// is left; the next expired instance is tried, and only when none is left
    /// does the eviction refuse, saying what happened. Catches an eviction
    /// that gives up at the first renewed preview, or removes a live one.
    #[test]
    fn test_eviction_continues_past_a_renewed_or_unfinished_instance() {
        let now = at("2026-10-10T10:00:00Z");
        let instances = [
            existing("a", "2026-10-01T00:00:00Z"),
            existing("b", "2026-10-02T00:00:00Z"),
            existing("c", "2026-10-03T00:00:00Z"),
        ];
        let mut plan = eviction(admit(Some(3), TTL, &instances, now, true));
        plan.record(&name("a"), EvictionResult::Renewed);
        assert_eq!(plan.next(), Some(&name("b")));
        plan.record(&name("b"), EvictionResult::Removed);
        assert!(plan.refusal().is_none());

        let mut stuck = eviction(admit(Some(3), TTL, &instances, now, true));
        stuck.record(&name("a"), EvictionResult::Renewed);
        stuck.record(&name("b"), EvictionResult::Incomplete);
        stuck.record(&name("c"), EvictionResult::Incomplete);
        assert_eq!(stuck.next(), None);
        let refusal = stuck.refusal().expect("no room was made");
        let finding = limit_finding("preview", &refusal);
        assert_eq!(finding.code, Code::InstanceLimit);
        assert!(finding.message.contains("b, c has not finished"), "{}", finding.message);
        assert!(
            finding.message.contains("a was renewed meanwhile"),
            "{}",
            finding.message
        );
    }

    /// With too few expired instances `--make-room` still refuses, and the
    /// finding says whether none or only some could go.
    #[test]
    fn test_make_room_refuses_when_too_few_instances_are_expired() {
        let now = at("2026-10-10T10:00:00Z");
        let all_live = [
            existing("a", "2026-10-20T00:00:00Z"),
            existing("b", "2026-10-20T00:00:00Z"),
        ];
        let Admission::Refuse(refusal) = admit(Some(2), TTL, &all_live, now, true) else {
            panic!("no expired instance, nothing to evict");
        };
        assert!(refusal.evictable.is_empty());
        assert!(limit_finding("preview", &refusal)
            .message
            .contains("no instance is expired"));
        let one_expired = [
            existing("a", "2026-10-02T00:00:00Z"),
            existing("b", "2026-10-20T00:00:00Z"),
        ];
        let Admission::Refuse(refusal) = admit(Some(1), TTL, &one_expired, now, true) else {
            panic!("two removals are needed and one instance is expired");
        };
        assert_eq!((refusal.needed, refusal.evictable.len()), (2, 1));
        assert!(limit_finding("preview", &refusal).message.contains("1 short"));
    }

    /// A template without `ttl` has no expired instances, so a limit can only
    /// be relieved by closing one.
    #[test]
    fn test_a_template_without_ttl_has_nothing_to_evict() {
        let now = at("2030-01-01T00:00:00Z");
        let instances = [existing("a", "2026-10-02T00:00:00Z")];
        let Admission::Refuse(refusal) = admit(Some(1), None, &instances, now, true) else {
            panic!("without a ttl nothing expires");
        };
        assert!(refusal.evictable.is_empty());
    }

    /// Which units a renewal writes, through the lifecycle table: a pending
    /// expired removal is undone, and nothing else is touched.
    #[test]
    fn test_a_renewal_writes_only_the_units_whose_expired_removal_has_not_started() {
        use nyl_core::orchestration::state::{Deletion, DeletionIntent, DeletionReason, Lifecycle, TeardownProgress};
        let unit = |lifecycle: Lifecycle| DesiredUnit {
            lifecycle,
            ..crate::rules::fixtures::desired_numbered("site", 1, None)
        };
        let deleting = |reason, teardown| {
            unit(Lifecycle::Deleting {
                deletion: Deletion {
                    hold: false,
                    reason,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(teardown),
                    requested: None,
                },
            })
        };
        let desired = BTreeMap::from([
            (UnitName::new("active").unwrap(), unit(Lifecycle::Active)),
            (
                UnitName::new("pending").unwrap(),
                deleting(DeletionReason::Expired, TeardownProgress::Pending),
            ),
            (
                UnitName::new("started").unwrap(),
                deleting(DeletionReason::Expired, TeardownProgress::InProgress),
            ),
            (
                UnitName::new("omitted").unwrap(),
                deleting(DeletionReason::Omission, TeardownProgress::Pending),
            ),
        ]);
        let written = renew_units(&desired);
        assert_eq!(written.keys().map(UnitName::as_str).collect::<Vec<_>>(), ["pending"]);
        assert_eq!(
            written[&UnitName::new("pending").unwrap()].as_ref().unwrap().lifecycle,
            Lifecycle::Active
        );
    }

    /// The report of a removal: pending only when the missing authorization is
    /// what held units back, never for a held lease or a renewed instance.
    #[test]
    fn test_a_removal_is_pending_only_when_units_were_gated_by_authorization() {
        let action = |removed, authorized, codes: &[Code], pending| removal_action(removed, authorized, codes, pending);
        assert_eq!(action(true, true, &[], 0), InstanceAction::Removed);
        assert_eq!(action(false, false, &[], 2), InstanceAction::RemovalPending);
        assert_eq!(action(false, true, &[], 2), InstanceAction::RemovalIncomplete);
        assert_eq!(
            action(false, false, &[Code::LeaseHeld], 0),
            InstanceAction::RemovalIncomplete
        );
        assert_eq!(
            action(false, false, &[Code::InstanceRenewed], 0),
            InstanceAction::RemovalIncomplete
        );
        assert_eq!(action(false, false, &[], 0), InstanceAction::RemovalIncomplete);
        assert!(
            removal_authorized(true, false) && removal_authorized(false, true) && !removal_authorized(false, false)
        );
        assert_eq!(
            eviction_result(InstanceAction::RemovalIncomplete, &[Code::InstanceRenewed]),
            EvictionResult::Renewed
        );
        assert_eq!(
            eviction_result(InstanceAction::RemovalPending, &[]),
            EvictionResult::Incomplete
        );
        assert_eq!(eviction_result(InstanceAction::Removed, &[]), EvictionResult::Removed);
        assert_eq!(eviction_result_of_stop(Code::StateDeleted), EvictionResult::Removed);
        assert_eq!(eviction_result_of_stop(Code::InstanceRenewed), EvictionResult::Renewed);
        assert_eq!(
            eviction_result_of_stop(Code::StateUnavailable),
            EvictionResult::Incomplete
        );

        let environment = name("pr-1");
        let expires = Some(at("2026-10-01T10:00:00Z"));
        assert_eq!(
            decided_expiry(&environment, expires, TTL, at("2026-10-02T00:00:00Z")),
            Ok(at("2026-10-01T10:00:00Z"))
        );
        for (expires_at, now) in [(expires, "2026-09-30T00:00:00Z"), (None, "2026-10-02T00:00:00Z")] {
            assert_eq!(
                decided_expiry(&environment, expires_at, TTL, at(now)).unwrap_err().code,
                Code::InstanceRenewed
            );
        }
    }
}
