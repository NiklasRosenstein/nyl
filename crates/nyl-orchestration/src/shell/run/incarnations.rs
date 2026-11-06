//! Whether unit incarnations still exist in their environments' state: the
//! [`IncarnationDirectory`] a run gives its drivers, for owner fencing.
//!
//! Contract: [Kubernetes publication unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! ownership index. A publication refuses a prefix whose index names another
//! unit incarnation while that incarnation still exists in its environment's
//! state, active, held, deleting, or retained; it takes the prefix over once
//! the incarnation is gone. This module answers the existence question and
//! knows nothing about publications. Every answer reads the environment's
//! state again, because a driver acts on `Gone`, and an incarnation created
//! since an earlier answer (a retry after a lost compare-and-swap) must not be
//! taken over; drivers ask only when they find an owner that is not them, so
//! runs that never fence read nothing.

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{IncarnationDirectory, IncarnationStatus};
use nyl_core::orchestration::ids::{EnvironmentName, Uid, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateStore};
use nyl_core::orchestration::state::StateLocation;

/// Reads an environment's state at its location, or why it cannot.
type Snapshots<'a> = dyn Fn(&StateLocation) -> Result<EnvironmentSnapshot, String> + Send + Sync + 'a;

/// The incarnations of every declared environment, read at the locations the
/// run's request declares.
pub(super) struct StateIncarnations<'a> {
    snapshots: Box<Snapshots<'a>>,
    locations: &'a BTreeMap<EnvironmentName, StateLocation>,
}

impl<'a> StateIncarnations<'a> {
    /// Incarnations read from `store`.
    pub(super) fn over(store: &'a dyn StateStore, locations: &'a BTreeMap<EnvironmentName, StateLocation>) -> Self {
        Self::reading(
            move |location| store.snapshot(location).map_err(|error| error.to_string()),
            locations,
        )
    }

    /// Incarnations read through `snapshots`.
    pub(super) fn reading(
        snapshots: impl Fn(&StateLocation) -> Result<EnvironmentSnapshot, String> + Send + Sync + 'a,
        locations: &'a BTreeMap<EnvironmentName, StateLocation>,
    ) -> Self {
        Self {
            snapshots: Box::new(snapshots),
            locations,
        }
    }
}

impl IncarnationDirectory for StateIncarnations<'_> {
    fn status(&self, environment: &EnvironmentName, unit: &UnitName, uid: &Uid) -> IncarnationStatus {
        let Some(location) = self.locations.get(environment) else {
            return IncarnationStatus::Unknown(format!(
                "environment {environment} is not declared at this source commit, so its state cannot be located"
            ));
        };
        match (self.snapshots)(location) {
            Err(error) => IncarnationStatus::Unknown(format!(
                "the state of environment {environment} cannot be read: {error}"
            )),
            Ok(snapshot) => {
                let held = snapshot
                    .desired
                    .get(unit)
                    .is_some_and(|desired| desired.unit.uid.as_ref() == Some(uid));
                if held {
                    IncarnationStatus::Exists
                } else {
                    IncarnationStatus::Gone
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use nyl_core::orchestration::state::{Lifecycle, Tombstone};

    use super::*;
    use crate::rules::fixtures::{desired_numbered, env, name, uid_numbered};

    fn location(path: &str) -> StateLocation {
        StateLocation {
            repository: "file:///state.git".to_owned(),
            desired_ref: "nyl/desired".to_owned(),
            observed_ref: "nyl/observed".to_owned(),
            path: path.to_owned(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    fn snapshot(units: Vec<nyl_core::orchestration::state::DesiredUnit>) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            desired: units.into_iter().map(|unit| (unit.unit.name.clone(), unit)).collect(),
            ..EnvironmentSnapshot::default()
        }
    }

    fn locations() -> BTreeMap<EnvironmentName, StateLocation> {
        BTreeMap::from([(env(), location(""))])
    }

    /// Catches an incarnation in any lifecycle, such as one that is still
    /// deleting or a retained tombstone, being taken over as if it were gone,
    /// and one with another uid (a previous incarnation of the name) or no
    /// document at all counting as existing.
    #[test]
    fn test_an_incarnation_exists_while_state_holds_its_name_and_uid_in_any_lifecycle() {
        let mut deleting = desired_numbered("deleting", 2, None);
        deleting.lifecycle = Lifecycle::Deleting {
            deletion: nyl_core::orchestration::state::Deletion {
                hold: false,
                reason: nyl_core::orchestration::state::DeletionReason::Teardown,
                intent: nyl_core::orchestration::state::DeletionIntent::Teardown,
                teardown: None,
                requested: None,
            },
        };
        let mut retained = desired_numbered("retained", 3, None);
        retained.lifecycle = Lifecycle::Retained {
            tombstone: Tombstone {
                since: chrono::Utc::now(),
                receipt: None,
            },
        };
        let held = snapshot(vec![desired_numbered("active", 1, None), deleting, retained]);
        let locations = locations();
        let directory = StateIncarnations::reading(move |_| Ok(held.clone()), &locations);
        let status = |unit: &str, uid: u64| directory.status(&env(), &name(unit), &uid_numbered(uid));
        for (unit, uid) in [("active", 1), ("deleting", 2), ("retained", 3)] {
            assert_eq!(status(unit, uid), IncarnationStatus::Exists, "{unit}");
        }
        assert_eq!(
            status("active", 9),
            IncarnationStatus::Gone,
            "another incarnation of the name"
        );
        assert_eq!(status("forgotten", 1), IncarnationStatus::Gone, "no document");
    }

    /// Catches an environment that is not declared, or whose state cannot be
    /// read, answering `gone`: a driver would take the prefix over, so state
    /// that cannot say must say so.
    #[test]
    fn test_an_undeclared_environment_and_unreadable_state_are_unknown() {
        let locations = locations();
        let directory = StateIncarnations::reading(|_| Err("the remote is unreachable".to_owned()), &locations);
        let other = EnvironmentName::new("ghost").unwrap();
        let IncarnationStatus::Unknown(reason) = directory.status(&other, &name("web"), &uid_numbered(1)) else {
            panic!("an undeclared environment is unknown");
        };
        assert!(reason.contains("environment ghost is not declared"), "{reason}");
        let IncarnationStatus::Unknown(reason) = directory.status(&env(), &name("web"), &uid_numbered(1)) else {
            panic!("unreadable state is unknown");
        };
        assert!(reason.contains("the remote is unreachable"), "{reason}");
    }

    /// Catches a `gone` answer cached for the run: an incarnation created
    /// after a first answer (a retry after a lost compare-and-swap) must read
    /// as existing the next time, so a live owner is never taken over.
    #[test]
    fn test_every_answer_reads_state_again() {
        let state = Mutex::new(snapshot(Vec::new()));
        let reads = AtomicUsize::new(0);
        let locations = locations();
        let directory = StateIncarnations::reading(
            |_| {
                reads.fetch_add(1, Ordering::Relaxed);
                Ok(state.lock().unwrap().clone())
            },
            &locations,
        );
        let ask = || directory.status(&env(), &name("web"), &uid_numbered(1));
        assert_eq!(ask(), IncarnationStatus::Gone);
        state
            .lock()
            .unwrap()
            .desired
            .insert(name("web"), desired_numbered("web", 1, None));
        assert_eq!(ask(), IncarnationStatus::Exists);
        assert_eq!(reads.load(Ordering::Relaxed), 2);
    }
}
