//! Stable codes for orchestration errors and warnings, each with its fix.
//!
//! The same problem reads the same in every place it appears: tables,
//! `--output json`, transition commit summaries, and status. Scripts match
//! on the code, never on the message.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A stable code. Its string form never changes once released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// Another run holds the environment's lease.
    LeaseHeld,
    /// The run's lease was taken over or deleted; it pushed nothing.
    LeaseLost,
    /// The configured location has no `state.yaml`.
    StateMissing,
    /// The state store cannot be reached or read, or the location cannot be resolved.
    StateUnavailable,
    /// The configured location's `state.yaml` names another environment.
    StateEnvironmentMismatch,
    /// The state moved to another location, which `state.yaml` names.
    StateMoved,
    /// `state.yaml` records another location than the configured one.
    StateLocationMismatch,
    /// A state ref is a full `refs/…` name that forges cannot protect.
    StateUnprotected,
    /// A state ref was rewritten outside Nyl.
    StateNotFastForward,
    /// Another writer changed the environment's state while an operation ran.
    StateConflict,
    /// State refs or coordination refs of environments cannot coexist in Git.
    StateRefConflict,
    /// `state forget` names a unit that is neither retained nor pending teardown.
    StateForgetRefused,
    /// A unit name the environment's state does not know.
    UnitUnknown,
    /// More than one EnvironmentGroup with shared fields selects an environment.
    EnvironmentGroupOverlap,
    /// An environment name no Environment declares.
    EnvironmentUnknown,
    /// Neither `--author` nor Git's `user.name` and `user.email` name who writes state commits.
    CommitIdentityMissing,
    /// Two unit documents share a name; names are unique per environment across kinds.
    UnitNameDuplicate,
    /// No driver serves a unit's `apiVersion` and `kind`.
    UnitKindUnknown,
    /// A unit's rendered spec fails its kind's validation.
    UnitInvalid,
    /// Two units of the ownership set claim overlapping native resources.
    UnitOwnershipOverlap,
    /// A reference names a unit outside the ownership set, an undeclared or sensitive output, a mismatched artifact kind, or a value of the wrong type.
    ReferenceInvalid,
    /// A `nyl get` value form found no current receipt to read.
    ReferenceUnresolved,
    /// `--allow-stale` printed a value from a receipt that is not current.
    ReferenceStale,
    /// `dependsOn` names a unit outside the ownership set.
    DependencyUnknown,
    /// Dependencies and references form a cycle.
    DependencyCycle,
    /// The source commit cannot be resolved, checked out, or rendered.
    SourceUnavailable,
    /// A source commit is not reachable from the refs it must be reachable from.
    SourceUnreachable,
    /// A driver call failed on Nyl's side before any effect started.
    DriverError,
    /// A unit reported outputs it does not declare; they were ignored.
    OutputUndeclared,
    /// Units leave the ownership set; their deletion follows.
    PlanUnitsLeaving,
    /// Publication units of two environments place the same DeploymentTarget.
    TargetEnvironmentConflict,
    /// A unit declares `env.secrets`, but no secrets provider is wired into
    /// orchestration yet (M3).
    UnitSecretsUnsupported,
    /// A reference or `dependsOn` entry requires `evidence: attested` or
    /// names attestations, which no kind records yet (M3).
    AttestationsUnsupported,
    /// A unit has an uncertain condition: its effects may or may not have happened.
    UnitUncertain,
    /// A leftover run's checkpoint no longer matches the unit's desired document; it was reported as superseded.
    RunSuperseded,
    /// A run checkpointed a unit after the unit was marked uncertain; it is evidence for `nyl recover --accept`.
    RunLateCheckpoint,
    /// `nyl recover` names a unit that has no condition to clear.
    RecoverNothing,
    /// `nyl recover --accept <run>` names a run without a checkpoint of the unit.
    RecoverCheckpointMissing,
    /// `nyl recover --accept <run>` names a checkpoint whose execution key differs from the unit's current desired document.
    RecoverKeyMismatch,
    /// `nyl verify` found no current receipt to verify for a unit.
    VerifyNoReceipt,
    /// A unit's driver could not tell whether the unit drifted.
    VerifyUnknown,
    /// A unit's kind cannot verify; it was reported, not skipped.
    VerifyUnsupported,
    /// A unit drifted from its current receipt.
    VerifyDrift,
    /// A driver attestation of a unit's current receipt failed.
    AttestationFailed,
    /// A remote state ref's tip is a `Nyl-Local` commit, which only a manual push of local branches produces.
    StateLocalTip,
    /// The clone's local state branches hold local transitions or are behind the remote.
    StateLocalDiverged,
    /// `nyl state reset --local` would forget local receipts the remote does not reflect.
    StateResetUndiscarded,
    /// `nyl state reset --local` needs remote state; without a remote the local branches are the state.
    StateResetUnsupported,
    /// `nyl state restore --to` names a commit that is not a non-local commit of the environment's state refs.
    StateRestoreInvalid,
    /// A `--local` run executed a unit next to remote state; the remote does not record it.
    LocalRunUnrecorded,
    /// A `--local` run skipped a unit whose recovery policy is not `converge`.
    LocalRunNotConverge,
    /// `--local` cannot run against this state, such as state refs with full `refs/…` names, which have no local branch.
    LocalRunUnsupported,
    /// A `--local` run found remote run refs of dead runs that remote state
    /// has not taken over yet.
    LocalRunLeftovers,
    /// A run stopped on SIGINT or SIGTERM: it checkpointed what finished,
    /// left the units still running `uncertain`, and wrote no transition commit.
    RunCancelled,
}

impl Code {
    pub const ALL: [Self; 54] = [
        Self::LeaseHeld,
        Self::LeaseLost,
        Self::StateMissing,
        Self::StateUnavailable,
        Self::StateEnvironmentMismatch,
        Self::StateMoved,
        Self::StateLocationMismatch,
        Self::StateUnprotected,
        Self::StateNotFastForward,
        Self::StateConflict,
        Self::StateRefConflict,
        Self::StateForgetRefused,
        Self::UnitUnknown,
        Self::EnvironmentGroupOverlap,
        Self::EnvironmentUnknown,
        Self::CommitIdentityMissing,
        Self::UnitNameDuplicate,
        Self::UnitKindUnknown,
        Self::UnitInvalid,
        Self::UnitOwnershipOverlap,
        Self::ReferenceInvalid,
        Self::ReferenceUnresolved,
        Self::ReferenceStale,
        Self::DependencyUnknown,
        Self::DependencyCycle,
        Self::SourceUnavailable,
        Self::SourceUnreachable,
        Self::DriverError,
        Self::OutputUndeclared,
        Self::PlanUnitsLeaving,
        Self::TargetEnvironmentConflict,
        Self::UnitSecretsUnsupported,
        Self::AttestationsUnsupported,
        Self::UnitUncertain,
        Self::RunSuperseded,
        Self::RunLateCheckpoint,
        Self::RecoverNothing,
        Self::RecoverCheckpointMissing,
        Self::RecoverKeyMismatch,
        Self::VerifyNoReceipt,
        Self::VerifyUnknown,
        Self::VerifyUnsupported,
        Self::VerifyDrift,
        Self::AttestationFailed,
        Self::StateLocalTip,
        Self::StateLocalDiverged,
        Self::StateResetUndiscarded,
        Self::StateResetUnsupported,
        Self::StateRestoreInvalid,
        Self::LocalRunUnrecorded,
        Self::LocalRunNotConverge,
        Self::LocalRunUnsupported,
        Self::LocalRunLeftovers,
        Self::RunCancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeaseHeld => "NYL-LEASE-HELD",
            Self::LeaseLost => "NYL-LEASE-LOST",
            Self::StateMissing => "NYL-STATE-MISSING",
            Self::StateUnavailable => "NYL-STATE-UNAVAILABLE",
            Self::StateEnvironmentMismatch => "NYL-STATE-ENVIRONMENT-MISMATCH",
            Self::StateMoved => "NYL-STATE-MOVED",
            Self::StateLocationMismatch => "NYL-STATE-LOCATION-MISMATCH",
            Self::StateUnprotected => "NYL-STATE-UNPROTECTED",
            Self::StateNotFastForward => "NYL-STATE-NOT-FAST-FORWARD",
            Self::StateConflict => "NYL-STATE-CONFLICT",
            Self::StateRefConflict => "NYL-STATE-REF-CONFLICT",
            Self::StateForgetRefused => "NYL-STATE-FORGET-REFUSED",
            Self::UnitUnknown => "NYL-UNIT-UNKNOWN",
            Self::EnvironmentGroupOverlap => "NYL-ENVIRONMENT-GROUP-OVERLAP",
            Self::EnvironmentUnknown => "NYL-ENVIRONMENT-UNKNOWN",
            Self::CommitIdentityMissing => "NYL-COMMIT-IDENTITY-MISSING",
            Self::UnitNameDuplicate => "NYL-UNIT-NAME-DUPLICATE",
            Self::UnitKindUnknown => "NYL-UNIT-KIND-UNKNOWN",
            Self::UnitInvalid => "NYL-UNIT-INVALID",
            Self::UnitOwnershipOverlap => "NYL-UNIT-OWNERSHIP-OVERLAP",
            Self::ReferenceInvalid => "NYL-REFERENCE-INVALID",
            Self::ReferenceUnresolved => "NYL-REFERENCE-UNRESOLVED",
            Self::ReferenceStale => "NYL-REFERENCE-STALE",
            Self::DependencyUnknown => "NYL-DEPENDENCY-UNKNOWN",
            Self::DependencyCycle => "NYL-DEPENDENCY-CYCLE",
            Self::SourceUnavailable => "NYL-SOURCE-UNAVAILABLE",
            Self::SourceUnreachable => "NYL-SOURCE-UNREACHABLE",
            Self::DriverError => "NYL-DRIVER-ERROR",
            Self::OutputUndeclared => "NYL-OUTPUT-UNDECLARED",
            Self::PlanUnitsLeaving => "NYL-PLAN-UNITS-LEAVING",
            Self::TargetEnvironmentConflict => "NYL-TARGET-ENVIRONMENT-CONFLICT",
            Self::UnitSecretsUnsupported => "NYL-UNIT-SECRETS-UNSUPPORTED",
            Self::AttestationsUnsupported => "NYL-ATTESTATIONS-UNSUPPORTED",
            Self::UnitUncertain => "NYL-UNIT-UNCERTAIN",
            Self::RunSuperseded => "NYL-RUN-SUPERSEDED",
            Self::RunLateCheckpoint => "NYL-RUN-LATE-CHECKPOINT",
            Self::RecoverNothing => "NYL-RECOVER-NOTHING",
            Self::RecoverCheckpointMissing => "NYL-RECOVER-CHECKPOINT-MISSING",
            Self::RecoverKeyMismatch => "NYL-RECOVER-KEY-MISMATCH",
            Self::VerifyNoReceipt => "NYL-VERIFY-NO-RECEIPT",
            Self::VerifyUnknown => "NYL-VERIFY-UNKNOWN",
            Self::VerifyUnsupported => "NYL-VERIFY-UNSUPPORTED",
            Self::VerifyDrift => "NYL-VERIFY-DRIFT",
            Self::AttestationFailed => "NYL-ATTESTATION-FAILED",
            Self::StateLocalTip => "NYL-STATE-LOCAL-TIP",
            Self::StateLocalDiverged => "NYL-STATE-LOCAL-DIVERGED",
            Self::StateResetUndiscarded => "NYL-STATE-RESET-UNDISCARDED",
            Self::StateResetUnsupported => "NYL-STATE-RESET-UNSUPPORTED",
            Self::StateRestoreInvalid => "NYL-STATE-RESTORE-INVALID",
            Self::LocalRunUnrecorded => "NYL-LOCAL-RUN-UNRECORDED",
            Self::LocalRunNotConverge => "NYL-LOCAL-RUN-NOT-CONVERGE",
            Self::LocalRunUnsupported => "NYL-LOCAL-RUN-UNSUPPORTED",
            Self::LocalRunLeftovers => "NYL-LOCAL-RUN-LEFTOVERS",
            Self::RunCancelled => "NYL-RUN-CANCELLED",
        }
    }
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
            .into_iter()
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
            "enum": Self::ALL.map(Code::as_str).to_vec()
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
        for code in Code::ALL {
            assert!(seen.insert(code.as_str()), "{code} is used twice");
            assert!(code.as_str().starts_with("NYL-"));
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(serde_json::from_value::<Code>(json).unwrap(), code);
        }
    }
}
