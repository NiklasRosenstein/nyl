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
//! - [`admit`] decides whether a template at `maxInstances` admits another
//!   instance, and which expired instances `--make-room` removes first:
//!   oldest expiry first, only as many as needed, never a live instance.
//!
//! A template without `ttl` never expires, whatever an older record says.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::StateRecord;
use nyl_core::orchestration::time::{Duration, Timestamp};

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
    /// The template is at its limit and `--make-room` is given: remove these
    /// expired instances, in this order, then create it.
    Evict(Vec<EnvironmentName>),
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
    let evictable = expired_instances
        .into_iter()
        .map(|instance| instance.name.clone())
        .collect::<Vec<_>>();
    if make_room && evictable.len() >= needed {
        return Admission::Evict(evictable.into_iter().take(needed).collect());
    }
    Admission::Refuse(Refusal {
        max,
        count,
        needed,
        evictable,
    })
}

/// The finding of a template that refuses another instance: `NYL-INSTANCE-LIMIT`
/// (exit 2). `incomplete` are the evictions `--make-room` started whose
/// removal did not finish.
pub fn limit_finding(template: &str, refusal: &Refusal, incomplete: &[EnvironmentName]) -> Finding {
    let list = |names: &[EnvironmentName]| names.iter().map(EnvironmentName::as_str).collect::<Vec<_>>().join(", ");
    let at_limit = format!(
        "EnvironmentTemplate {template} is at maxInstances ({} of {})",
        refusal.count, refusal.max
    );
    if !incomplete.is_empty() {
        return Finding::new(
            Code::InstanceLimit,
            format!(
                "{at_limit}, and the removal of the expired instances {} has not finished",
                list(incomplete)
            ),
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
        let finding = limit_finding("preview", &refusal, &[]);
        assert_eq!(finding.code, Code::InstanceLimit);
        assert!(finding.message.contains("older, newer"));
        assert!(finding.fix.contains("--make-room"));
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
        assert_eq!(
            admit(Some(3), TTL, &instances, now, true),
            Admission::Evict(vec![name("older")])
        );
        // Two slots over the limit need two removals.
        assert_eq!(
            admit(Some(2), TTL, &instances, now, true),
            Admission::Evict(vec![name("older"), name("newer")])
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
        assert!(limit_finding("preview", &refusal, &[])
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
        assert!(limit_finding("preview", &refusal, &[]).message.contains("1 short"));
        let unfinished = limit_finding("preview", &refusal, &[name("a")]);
        assert!(unfinished.message.contains("has not finished"));
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
}
