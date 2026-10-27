//! Stable codes for orchestration errors and warnings, each with its fix.
//!
//! The same problem reads the same in every place it appears: tables,
//! `--output json`, transition commit summaries, and status. Scripts match
//! on the code, never on the message.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! codes {
    ($($(#[$meta:meta])* $variant:ident => $text:literal,)*) => {
        /// A stable code. Its string form never changes once released.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Code {
            $($(#[$meta])* $variant,)*
        }

        impl Code {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)*
                }
            }
        }
    };
}

codes! {
    /// Another run holds the environment's lease.
    LeaseHeld => "NYL-LEASE-HELD",
    /// The run's lease was taken over or deleted; it pushed nothing.
    LeaseLost => "NYL-LEASE-LOST",
    /// The configured location has no `state.yaml`.
    StateMissing => "NYL-STATE-MISSING",
    /// The state store cannot be reached or read, or the location cannot be resolved.
    StateUnavailable => "NYL-STATE-UNAVAILABLE",
    /// The configured location's `state.yaml` names another environment.
    StateEnvironmentMismatch => "NYL-STATE-ENVIRONMENT-MISMATCH",
    /// The state moved to another location, which `state.yaml` names.
    StateMoved => "NYL-STATE-MOVED",
    /// `state.yaml` records another location than the configured one.
    StateLocationMismatch => "NYL-STATE-LOCATION-MISMATCH",
    /// A state ref is a full `refs/…` name that forges cannot protect.
    StateUnprotected => "NYL-STATE-UNPROTECTED",
    /// A state ref was rewritten outside Nyl.
    StateNotFastForward => "NYL-STATE-NOT-FAST-FORWARD",
    /// Another writer changed the environment's state while an operation ran.
    StateConflict => "NYL-STATE-CONFLICT",
    /// State refs or coordination refs of environments cannot coexist in Git.
    StateRefConflict => "NYL-STATE-REF-CONFLICT",
    /// `state forget` names a unit that is neither retained nor pending teardown.
    StateForgetRefused => "NYL-STATE-FORGET-REFUSED",
    /// A unit name the environment's state does not know.
    UnitUnknown => "NYL-UNIT-UNKNOWN",
    /// More than one EnvironmentGroup with shared fields selects an environment.
    EnvironmentGroupOverlap => "NYL-ENVIRONMENT-GROUP-OVERLAP",
    /// An environment name no Environment declares.
    EnvironmentUnknown => "NYL-ENVIRONMENT-UNKNOWN",
    /// Neither `--author` nor Git's `user.name` and `user.email` name who writes state commits.
    CommitIdentityMissing => "NYL-COMMIT-IDENTITY-MISSING",
    /// Two unit documents share a name; names are unique per environment across kinds.
    UnitNameDuplicate => "NYL-UNIT-NAME-DUPLICATE",
    /// No driver serves a unit's `apiVersion` and `kind`.
    UnitKindUnknown => "NYL-UNIT-KIND-UNKNOWN",
    /// A unit's rendered spec fails its kind's validation.
    UnitInvalid => "NYL-UNIT-INVALID",
    /// Two units of the ownership set claim overlapping native resources.
    UnitOwnershipOverlap => "NYL-UNIT-OWNERSHIP-OVERLAP",
    /// A reference names a unit outside the ownership set, an undeclared or sensitive output, a mismatched artifact kind, or a value of the wrong type.
    ReferenceInvalid => "NYL-REFERENCE-INVALID",
    /// A `nyl get` value form found no current receipt to read.
    ReferenceUnresolved => "NYL-REFERENCE-UNRESOLVED",
    /// `--allow-stale` printed a value from a receipt that is not current.
    ReferenceStale => "NYL-REFERENCE-STALE",
    /// `dependsOn` names a unit outside the ownership set.
    DependencyUnknown => "NYL-DEPENDENCY-UNKNOWN",
    /// Dependencies and references form a cycle.
    DependencyCycle => "NYL-DEPENDENCY-CYCLE",
    /// The source commit cannot be resolved, checked out, or rendered.
    SourceUnavailable => "NYL-SOURCE-UNAVAILABLE",
    /// A source commit is not reachable from the refs it must be reachable from.
    SourceUnreachable => "NYL-SOURCE-UNREACHABLE",
    /// A driver call failed on Nyl's side before any effect started.
    DriverError => "NYL-DRIVER-ERROR",
    /// A unit reported outputs it does not declare; they were ignored.
    OutputUndeclared => "NYL-OUTPUT-UNDECLARED",
    /// Units leave the ownership set; their deletion follows.
    PlanUnitsLeaving => "NYL-PLAN-UNITS-LEAVING",
    /// Publication units of two environments place the same DeploymentTarget.
    TargetEnvironmentConflict => "NYL-TARGET-ENVIRONMENT-CONFLICT",
    /// A unit declares `env.secrets`, but no secrets provider is wired into
    /// orchestration yet (M3).
    UnitSecretsUnsupported => "NYL-UNIT-SECRETS-UNSUPPORTED",
    /// A reference or `dependsOn` entry requires `evidence: attested` or
    /// names attestations, which no kind records yet (M3).
    AttestationsUnsupported => "NYL-ATTESTATIONS-UNSUPPORTED",
    /// A unit has an uncertain condition: its effects may or may not have happened.
    UnitUncertain => "NYL-UNIT-UNCERTAIN",
    /// A leftover run's checkpoint no longer matches the unit's desired document; it was reported as superseded.
    RunSuperseded => "NYL-RUN-SUPERSEDED",
    /// A run checkpointed a unit after the unit was marked uncertain; it is evidence for `nyl recover --accept`.
    RunLateCheckpoint => "NYL-RUN-LATE-CHECKPOINT",
    /// `nyl recover` names a unit that has no condition to clear.
    RecoverNothing => "NYL-RECOVER-NOTHING",
    /// `nyl recover --accept <run>` names a run without a checkpoint of the unit.
    RecoverCheckpointMissing => "NYL-RECOVER-CHECKPOINT-MISSING",
    /// `nyl recover --accept <run>` names a checkpoint whose execution key differs from the unit's current desired document.
    RecoverKeyMismatch => "NYL-RECOVER-KEY-MISMATCH",
    /// `nyl verify` found no current receipt to verify for a unit.
    VerifyNoReceipt => "NYL-VERIFY-NO-RECEIPT",
    /// A unit's driver could not tell whether the unit drifted.
    VerifyUnknown => "NYL-VERIFY-UNKNOWN",
    /// A unit's kind cannot verify; it was reported, not skipped.
    VerifyUnsupported => "NYL-VERIFY-UNSUPPORTED",
    /// A unit drifted from its current receipt.
    VerifyDrift => "NYL-VERIFY-DRIFT",
    /// A driver attestation of a unit's current receipt failed.
    AttestationFailed => "NYL-ATTESTATION-FAILED",
    /// A remote state ref's tip is a `Nyl-Local` commit, which only a manual push of local branches produces.
    StateLocalTip => "NYL-STATE-LOCAL-TIP",
    /// The clone's local state branches hold local transitions or are behind the remote.
    StateLocalDiverged => "NYL-STATE-LOCAL-DIVERGED",
    /// `nyl state reset --local` would forget local receipts the remote does not reflect.
    StateResetUndiscarded => "NYL-STATE-RESET-UNDISCARDED",
    /// `nyl state reset --local` needs remote state; without a remote the local branches are the state.
    StateResetUnsupported => "NYL-STATE-RESET-UNSUPPORTED",
    /// `nyl state restore --to` names a commit that is not a non-local commit of the environment's state refs.
    StateRestoreInvalid => "NYL-STATE-RESTORE-INVALID",
    /// A `--local` run executed a unit next to remote state; the remote does not record it.
    LocalRunUnrecorded => "NYL-LOCAL-RUN-UNRECORDED",
    /// `--local` cannot run against this state, such as state refs with full `refs/…` names, which have no local branch.
    LocalRunUnsupported => "NYL-LOCAL-RUN-UNSUPPORTED",
    /// A `--local` run found remote run refs of dead runs that remote state
    /// has not taken over yet.
    LocalRunLeftovers => "NYL-LOCAL-RUN-LEFTOVERS",
    /// A run stopped on SIGINT or SIGTERM: it checkpointed what finished,
    /// left the units still running `uncertain`, and wrote no transition commit.
    RunCancelled => "NYL-RUN-CANCELLED",
    /// A run ended without its transition commit, cancelled or after losing
    /// its lease, and its run ref waits for the next reconcile to import it.
    RunUnimported => "NYL-RUN-UNIMPORTED",
    /// A broken lease's run has not been imported yet; only `reconcile`,
    /// `teardown`, and `recover` take such a lease over. Exit 2.
    LeaseBroken => "NYL-LEASE-BROKEN",
    /// A unit left the ownership set and its teardown waits for
    /// `--allow-teardown` (omission or kind change). Exit 2.
    TeardownPending => "NYL-TEARDOWN-PENDING",
    /// Tearing a unit down would leave a `retained` tombstone whose receipt
    /// cites it without its producer; `--allow-dependents` overrides. Exit 2.
    TeardownHasDependents => "NYL-TEARDOWN-HAS-DEPENDENTS",
    /// A publication's teardown waits under its `teardownWait`, and the units
    /// it depends on wait with it. Exit 2.
    TeardownWaiting => "NYL-TEARDOWN-WAITING",
    /// A teardown proceeded, or would proceed, without everything removed:
    /// an unmet readiness requirement or an unconfirmed removal.
    TeardownIncomplete => "NYL-TEARDOWN-INCOMPLETE",
    /// Teardown readiness: the catalog does not prune removed workload
    /// Applications on its own (blocking).
    TeardownCatalogManualSync => "NYL-TEARDOWN-CATALOG-MANUAL-SYNC",
    /// Teardown readiness: deleting an Application orphans its workloads (blocking).
    TeardownApplicationsOrphaned => "NYL-TEARDOWN-APPLICATIONS-ORPHANED",
    /// Teardown readiness: an owned Namespace's `deletePolicy: Confirm` holds
    /// the Application's finalizer until someone confirms (blocking).
    TeardownNamespaceConfirm => "NYL-TEARDOWN-NAMESPACE-CONFIRM",
    /// Teardown readiness: owned Namespaces remain after teardown (remnant).
    TeardownNamespacesRemain => "NYL-TEARDOWN-NAMESPACES-REMAIN",
    /// Teardown readiness: a self-managing catalog Application and its
    /// AppProjects remain after teardown (remnant).
    TeardownCatalogRemains => "NYL-TEARDOWN-CATALOG-REMAINS",
    /// A unit whose kind cannot tear down was released: its resources were
    /// left in place and the transition commit lists them.
    UnitReleased => "NYL-UNIT-RELEASED",
    /// A unit's `apiVersion` or `kind` changed: the old incarnation is
    /// deleted under its policy before the new kind starts.
    UnitKindChange => "NYL-UNIT-KIND-CHANGE",
    /// A unit is held: reconcile changes nothing about it until `nyl resume`.
    UnitHeld => "NYL-UNIT-HELD",
    /// `nyl hold` cannot hold this unit: a `retained` tombstone.
    HoldRefused => "NYL-HOLD-REFUSED",
    /// `--confirm-removed` was rejected: no teardown of that unit waits for
    /// a confirmation, or the waiting run is not waiting on it.
    ConfirmationRejected => "NYL-CONFIRMATION-REJECTED",
    /// `--confirm-removed` was delivered to the run waiting on the unit's teardown.
    ConfirmationDelivered => "NYL-CONFIRMATION-DELIVERED",
    /// The environment's state was deleted with `nyl state delete`.
    StateDeleted => "NYL-STATE-DELETED",
    /// `nyl state delete` refuses while units still have an incarnation.
    StateDeleteIncarnations => "NYL-STATE-DELETE-INCARNATIONS",
    /// `nyl hold` was delivered to the run holding the lease, through its
    /// signal ref: the run starts no held unit and writes the hold in its
    /// own transition commit. Exit 0.
    HoldDelivered => "NYL-HOLD-DELIVERED",
    /// `--fail-fast` stopped the run after a unit failed, became uncertain,
    /// or waited for an approval: it started no further unit, and the units
    /// it did not start report what their state gives. A warning.
    RunStopped => "NYL-RUN-STOPPED",
}

impl fmt::Display for Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::ALL
            .iter()
            .copied()
            .find(|code| code.as_str() == text)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown code {text:?}")))
    }
}

impl JsonSchema for Code {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Code".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": Self::ALL.iter().map(|code| code.as_str()).collect::<Vec<_>>()
        })
    }
}

/// An error or warning: its code, what happened, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    pub code: Code,
    pub message: String,
    pub fix: String,
}

impl Finding {
    pub fn new(code: Code, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fix: fix.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {} ({})", self.code, self.message, self.fix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codes_round_trip_and_never_collide() {
        let mut seen = std::collections::BTreeSet::new();
        for &code in Code::ALL {
            assert!(seen.insert(code.as_str()), "{code} is used twice");
            assert!(code.as_str().starts_with("NYL-"));
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(serde_json::from_value::<Code>(json).unwrap(), code);
        }
    }
}
