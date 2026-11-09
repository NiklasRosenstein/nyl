//! Reading other environments' state, for every command that does.
//!
//! Contract: [Other environments](../../../../design/orchestration-core.md#other-environments).
//! Every command reads another environment through [`Peers::read`], which
//! applies the one decision of [`crate::rules::peers`]: an error when the
//! command requires it, a warning when it only advises. Reads take no lease
//! and write nothing.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateStore};
use nyl_core::orchestration::state::StateLocation;

use super::run::RunRequest;
use super::store_error;
use crate::rules::peers::{judge, Need, Problem};

/// The environments of the project a command can read, and the ones whose
/// declaration cannot be used, with why.
#[derive(Debug, Clone, Default)]
pub struct Peers {
    /// Where each environment whose location resolves keeps its state.
    pub locations: BTreeMap<EnvironmentName, StateLocation>,
    /// The environments whose location could not be resolved, with why.
    pub errors: BTreeMap<EnvironmentName, Finding>,
}

impl Peers {
    /// The environments a run request knows.
    pub fn of(request: &RunRequest) -> Self {
        Self {
            locations: request.environments.clone(),
            errors: request.environment_errors.clone(),
        }
    }

    /// Whether the project declares `peer`, usable or not.
    pub fn knows(&self, peer: &EnvironmentName) -> bool {
        self.locations.contains_key(peer) || self.errors.contains_key(peer)
    }

    /// Every environment other than `own`, usable or not.
    pub fn others(&self, own: &EnvironmentName) -> BTreeSet<&EnvironmentName> {
        self.locations
            .keys()
            .chain(self.errors.keys())
            .filter(|name| *name != own)
            .collect()
    }

    /// Where `peer` keeps its state, or why that is not known: its
    /// declaration cannot be used, or the project declares no such environment.
    pub fn locate(&self, peer: &EnvironmentName) -> Result<&StateLocation, Finding> {
        if let Some(finding) = self.errors.get(peer) {
            return Err(finding.clone());
        }
        self.locations.get(peer).ok_or_else(|| {
            Finding::new(
                Code::EnvironmentUnknown,
                format!("the project declares no Environment {peer}"),
                format!("declare Environment {peer}, or correct the reference"),
            )
        })
    }

    /// The snapshot of `peer`, or the problem that keeps it from being read,
    /// judged by `need`: an error when the command requires the environment,
    /// a `NYL-STATE-UNAVAILABLE` warning when it only advises.
    pub fn read(
        &self,
        store: &dyn StateStore,
        peer: &EnvironmentName,
        need: Need,
    ) -> Result<EnvironmentSnapshot, Problem> {
        self.locate(peer)
            .and_then(|location| store.snapshot(location).map_err(|error| store_error(peer, &error)))
            .map_err(|cause| judge(need, peer, cause))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> EnvironmentName {
        EnvironmentName::new(name).unwrap()
    }

    /// An environment whose declaration cannot be used keeps its own finding,
    /// and one nobody declared is `NYL-ENVIRONMENT-UNKNOWN`, whichever
    /// command asks. Catches an unusable declaration read as a missing
    /// environment, which hides the fix the declaration needs.
    #[test]
    fn test_a_peer_without_a_usable_location_names_why() {
        let broken = Finding::new(
            Code::StateUnavailable,
            "cannot resolve the state location of prod",
            "fix it",
        );
        let peers = Peers {
            locations: BTreeMap::new(),
            errors: BTreeMap::from([(name("prod"), broken.clone())]),
        };
        assert_eq!(peers.locate(&name("prod")), Err(broken));
        assert_eq!(peers.locate(&name("ghost")).unwrap_err().code, Code::EnvironmentUnknown);
        assert!(peers.knows(&name("prod")) && !peers.knows(&name("ghost")));
        assert_eq!(peers.others(&name("dev")).len(), 1);
        assert!(peers.others(&name("prod")).is_empty());
    }
}
