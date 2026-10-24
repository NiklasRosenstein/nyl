//! Fault points of the orchestration shell, for crash-injection tests.
//!
//! The shell consults a [`FaultInjector`] at each defined point. Production
//! wiring passes [`NoFaults`]; tests pass one that ends the run at a chosen
//! point as a forced kill would, so recovery after an effect without a
//! checkpoint, a lost lease, or a missing final push is exercised for real.
//! The points and their names are those of the scenario format's `fault`
//! and `pause` steps (`design/reference-scenarios.md`).

use std::fmt;
use std::str::FromStr;

use nyl_core::orchestration::ids::UnitName;

/// A point in a run where a fault can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// A unit's effect happened; its result is not checkpointed yet.
    AfterEffect,
    /// A unit finished; its checkpoint is about to be written.
    BeforeCheckpoint,
    /// The lease and run record are about to be updated for the next phase.
    BeforeLeaseUpdate,
    /// The final transition commit is about to be pushed.
    BeforePush,
}

impl FaultPoint {
    pub const ALL: [Self; 4] = [
        Self::AfterEffect,
        Self::BeforeCheckpoint,
        Self::BeforeLeaseUpdate,
        Self::BeforePush,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AfterEffect => "after-effect",
            Self::BeforeCheckpoint => "before-checkpoint",
            Self::BeforeLeaseUpdate => "before-lease-update",
            Self::BeforePush => "before-push",
        }
    }
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FaultPoint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|point| point.as_str() == value)
            .ok_or_else(|| {
                format!(
                    "unknown fault point {value:?}; expected one of {}",
                    Self::ALL.map(FaultPoint::as_str).join(", ")
                )
            })
    }
}

/// What the shell does at a fault point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    Continue,
    /// Stop at once, writing nothing more, as a forced kill would.
    Kill,
}

/// Decides, at each fault point, whether the run continues.
pub trait FaultInjector: Send + Sync {
    /// `unit` is the unit the point concerns, when there is one.
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction;
}

/// The production injector: every point continues.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFaults;

impl FaultInjector for NoFaults {
    fn at(&self, _point: FaultPoint, _unit: Option<&UnitName>) -> FaultAction {
        FaultAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fault_points_use_the_scenario_format_names() {
        for point in FaultPoint::ALL {
            assert_eq!(point.as_str().parse::<FaultPoint>().unwrap(), point);
        }
        assert!("after-checkpoint".parse::<FaultPoint>().is_err());
    }
}
