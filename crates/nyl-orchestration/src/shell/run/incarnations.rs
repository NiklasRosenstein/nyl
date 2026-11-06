//! Whether unit incarnations still exist in their environments' state: the
//! [`IncarnationDirectory`] a run gives its drivers, for owner fencing.
//!
//! Contract: [Kubernetes publication unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! ownership index. A publication refuses a prefix whose index names another
//! unit incarnation while that incarnation still exists in its environment's
//! state, active, held, deleting, or retained; it takes the prefix over once
//! the incarnation is gone. This module answers the existence question and
//! knows nothing about publications: each declared environment's state is
//! read once per run, lazily, so runs whose drivers never ask read nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use nyl_core::orchestration::driver::{IncarnationDirectory, IncarnationStatus};
use nyl_core::orchestration::ids::{EnvironmentName, Uid, UnitName};
use nyl_core::orchestration::ports::StateStore;
use nyl_core::orchestration::state::StateLocation;

/// What one environment's state held when it was first asked about.
type Incarnations = Result<BTreeSet<(UnitName, Uid)>, String>;

/// The incarnations of every declared environment, read from `store` at the
/// locations the run's request declares.
pub(super) struct StateIncarnations<'a> {
    store: &'a dyn StateStore,
    locations: &'a BTreeMap<EnvironmentName, StateLocation>,
    read: Mutex<BTreeMap<EnvironmentName, Incarnations>>,
}

impl<'a> StateIncarnations<'a> {
    pub(super) fn new(store: &'a dyn StateStore, locations: &'a BTreeMap<EnvironmentName, StateLocation>) -> Self {
        Self {
            store,
            locations,
            read: Mutex::new(BTreeMap::new()),
        }
    }

    fn read(&self, environment: &EnvironmentName) -> Incarnations {
        let Some(location) = self.locations.get(environment) else {
            return Err(format!(
                "environment {environment} is not declared at this source commit, so its state cannot be located"
            ));
        };
        let snapshot = self
            .store
            .snapshot(location)
            .map_err(|error| format!("the state of environment {environment} cannot be read: {error}"))?;
        Ok(snapshot
            .desired
            .values()
            .filter_map(|unit| Some((unit.unit.name.clone(), unit.unit.uid.clone()?)))
            .collect())
    }
}

impl IncarnationDirectory for StateIncarnations<'_> {
    fn status(&self, environment: &EnvironmentName, unit: &UnitName, uid: &Uid) -> IncarnationStatus {
        let mut read = self.read.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let incarnations = read
            .entry(environment.clone())
            .or_insert_with(|| self.read(environment));
        match incarnations {
            Ok(incarnations) if incarnations.contains(&(unit.clone(), uid.clone())) => IncarnationStatus::Exists,
            Ok(_) => IncarnationStatus::Gone,
            Err(reason) => IncarnationStatus::Unknown(reason.clone()),
        }
    }
}
