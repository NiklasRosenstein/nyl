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
    /// An explicit worktree run uses an archived snapshot of uncommitted source.
    SourceWorktree => "NYL-SOURCE-WORKTREE",
    /// Native repair requires a held incarnation.
    ExecNotHeld => "NYL-EXEC-NOT-HELD",
    /// The unit's driver cannot run an operator tool.
    ExecUnsupported => "NYL-EXEC-UNSUPPORTED",
    /// The repair spec claims another unit's resources.
    ExecScopeConflict => "NYL-EXEC-SCOPE-CONFLICT",
    /// The repair's spec, source, credentials, or arguments are unavailable.
    ExecInvalid => "NYL-EXEC-INVALID",
    /// An approval or approver lookup is invalid; no effects start.
    ApprovalInvalid => "NYL-APPROVAL-INVALID",
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
    /// A target's publication found the shared catalog manifest on the branch different from the one it renders; it left it alone. A warning.
    KubernetesSharedCatalogDiffers => "NYL-KUBERNETES-SHARED-CATALOG-DIFFERS",
    /// A direct target's apply kept objects whose `Prune=confirm` sync option awaits a confirmation the driver never gives. A warning.
    KubernetesPruneAwaitingConfirmation => "NYL-KUBERNETES-PRUNE-AWAITING-CONFIRMATION",
    /// A Cluster's recorded capabilities (vendored CRDs included) give a kind one scope and the live cluster another; the command used the recorded one. A warning.
    KubernetesScopeMismatch => "NYL-KUBERNETES-SCOPE-MISMATCH",
    /// A Release declares hooks that have no sync phase (Helm's `test`, `pre-delete`, `post-delete`, `pre-rollback`, `post-rollback`, Argo CD's `PreDelete` and `PostDelete`), which Argo CD does not sync either; the apply left them out. A warning.
    KubernetesHookIgnored => "NYL-KUBERNETES-HOOK-IGNORED",
    /// A direct target's apply found a Release that left the tree and is not this unit's to purge, and left it. A warning.
    KubernetesReleaseLeftBehind => "NYL-KUBERNETES-RELEASE-LEFT-BEHIND",
    /// A cross-environment reference or `dependsOn` entry names an instance of an EnvironmentTemplate as its producer. Instances are consumers only.
    ReferenceInstanceProducer => "NYL-REFERENCE-INSTANCE-PRODUCER",
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
    /// A teardown with `--source` took a unit's files from another revision than its recorded source commit. A warning.
    SourceOverride => "NYL-SOURCE-OVERRIDE",
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
    /// A reference or `dependsOn` entry requires an attestation its producer does not declare.
    AttestationUndeclared => "NYL-ATTESTATION-UNDECLARED",
    /// A reference or `dependsOn` entry requires an attestation only an external source supplies, which Nyl cannot record yet.
    AttestationExternalUnsupported => "NYL-ATTESTATION-EXTERNAL-UNSUPPORTED",
    /// A consumer waits for an attestation of its producer's current receipt that is not recorded yet.
    AttestationPending => "NYL-ATTESTATION-PENDING",
    /// A consumer waits for an attestation of its producer's current receipt that is recorded as failing.
    AttestationFailing => "NYL-ATTESTATION-FAILING",
    /// A unit has an uncertain condition: its effects may or may not have happened.
    UnitUncertain => "NYL-UNIT-UNCERTAIN",
    /// A leftover run's checkpoint no longer matches the unit's desired document; it was reported as superseded.
    RunSuperseded => "NYL-RUN-SUPERSEDED",
    /// A run checkpointed a unit after the unit was marked uncertain; it is evidence for `nyl recover --accept`.
    RunLateCheckpoint => "NYL-RUN-LATE-CHECKPOINT",
    /// `nyl recover` names a unit that has no condition to clear.
    /// A driver requested an acknowledgement with a nonempty reason before retry.
    RecoverReasonRequired => "NYL-RECOVER-REASON-REQUIRED",
    RecoverNothing => "NYL-RECOVER-NOTHING",
    /// `nyl recover --accept <run>` names a run without a checkpoint of the unit.
    RecoverCheckpointMissing => "NYL-RECOVER-CHECKPOINT-MISSING",
    /// `nyl recover --accept <run>` names a checkpoint whose execution key differs from the unit's current desired document.
    RecoverKeyMismatch => "NYL-RECOVER-KEY-MISMATCH",
    /// `nyl detect-drift` found no current receipt to check for a unit.
    DriftNoReceipt => "NYL-DRIFT-NO-RECEIPT",
    /// A unit's driver could not tell whether the unit drifted.
    DriftUnknown => "NYL-DRIFT-UNKNOWN",
    /// A unit's kind cannot detect drift; it was reported, not skipped.
    DriftUnsupported => "NYL-DRIFT-UNSUPPORTED",
    /// A unit drifted from its current receipt.
    DriftDetected => "NYL-DRIFT-DETECTED",
    /// `nyl attest --unit` named a unit without a current receipt to attest.
    AttestNoReceipt => "NYL-ATTEST-NO-RECEIPT",
    /// A unit's driver could not produce its attestations.
    AttestUnknown => "NYL-ATTEST-UNKNOWN",
    /// A unit declares `observations`, but its kind cannot read observed values.
    ObservationsUnsupported => "NYL-OBSERVATIONS-UNSUPPORTED",
    /// A consumer waits for an observed value of its producer's current receipt that has none yet.
    ObservationPending => "NYL-OBSERVATION-PENDING",
    /// An observed value could not be read; the last recorded value for the current receipt stays usable.
    ObservationFailed => "NYL-OBSERVATION-FAILED",
    /// An observed value changed, so the consumers that read it are ready for the next reconcile.
    ObservationChanged => "NYL-OBSERVATION-CHANGED",
    /// `nyl observe` found no current receipt to observe for a unit.
    ObserveNoReceipt => "NYL-OBSERVE-NO-RECEIPT",
    /// `nyl observe --unit` named a unit whose kind reads no observed values for its spec.
    ObserveUnsupported => "NYL-OBSERVE-UNSUPPORTED",
    /// `nyl attest --unit` named a unit whose kind produces no attestations for its spec.
    AttestUnsupported => "NYL-ATTEST-UNSUPPORTED",
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
    /// Applications on its own, because it does not sync with pruning or its
    /// `Prune` sync option waits for confirmation or skips them (blocking).
    TeardownCatalogManualSync => "NYL-TEARDOWN-CATALOG-MANUAL-SYNC",
    /// Teardown readiness: deleting an Application orphans its workloads (blocking).
    TeardownApplicationsOrphaned => "NYL-TEARDOWN-APPLICATIONS-ORPHANED",
    /// Teardown readiness: a resource a workload Application would delete,
    /// whose own sync options or the Application's `syncPolicy.syncOptions`
    /// resolve `Delete` or `Prune` to `confirm`, holds the Application's
    /// finalizer until someone confirms the deletion in Argo CD (blocking).
    /// An owned Namespace with the default `prunePolicy` or `deletePolicy`
    /// of `Confirm` is the usual case.
    TeardownNamespaceConfirm => "NYL-TEARDOWN-NAMESPACE-CONFIRM",
    /// Teardown readiness: owned Namespaces remain after teardown (remnant).
    TeardownNamespacesRemain => "NYL-TEARDOWN-NAMESPACES-REMAIN",
    /// Teardown readiness: a self-managing catalog Application and its
    /// AppProjects remain after teardown (remnant).
    TeardownCatalogRemains => "NYL-TEARDOWN-CATALOG-REMAINS",
    /// Teardown readiness: the unit's teardown waits for an operator to
    /// confirm the removal, so an unattended removal, such as the expiry of a
    /// template instance, stops after its first phase. A warning.
    TeardownWaitManual => "NYL-TEARDOWN-WAIT-MANUAL",
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
    /// `--template` names no EnvironmentTemplate of the project.
    InstanceTemplateUnknown => "NYL-INSTANCE-TEMPLATE-UNKNOWN",
    /// An instance's parameters do not satisfy its template's declarations.
    InstanceParameterInvalid => "NYL-INSTANCE-PARAMETER-INVALID",
    /// An environment name is used by a declared Environment or by an
    /// instance of another template.
    InstanceNameTaken => "NYL-INSTANCE-NAME-TAKEN",
    /// `--param` without `--template`: parameters are recorded when an instance is created or updated.
    InstanceTemplateRequired => "NYL-INSTANCE-TEMPLATE-REQUIRED",
    /// The state of an environment name belongs to instances of several templates.
    InstanceAmbiguous => "NYL-INSTANCE-AMBIGUOUS",
    /// An EnvironmentTemplate does not render to a valid Environment for an instance.
    TemplateInvalid => "NYL-TEMPLATE-INVALID",
    /// `--renew` or `--no-extend` where it does not apply: for a declared
    /// Environment, which has no expiry, or for the fleet reconcile.
    InstanceOptionInvalid => "NYL-INSTANCE-OPTION-INVALID",
    /// An instance is expired: the next `reconcile` of it removes it, unless
    /// `--renew` or the pull request job's `--template` renews it. A warning.
    InstanceExpired => "NYL-INSTANCE-EXPIRED",
    /// The removal of an expired instance waits: its teardown needs
    /// `allowTeardown` on the template or `--allow-teardown`. Exit 2.
    InstanceRemovalPending => "NYL-INSTANCE-REMOVAL-PENDING",
    /// An instance was renewed while its removal was starting, so nothing was removed. Exit 2.
    InstanceRenewed => "NYL-INSTANCE-RENEWED",
    /// A template is at `maxInstances`: no instance is created until an
    /// expired one is removed (`--make-room`). Exit 2.
    InstanceLimit => "NYL-INSTANCE-LIMIT",
    /// The instances of a template cannot be listed, which `maxInstances` and
    /// the fleet reconcile need: its state location depends on the instance name.
    InstanceNotListable => "NYL-INSTANCE-NOT-LISTABLE",
    /// An instance's source commit could not be kept fetchable with a keep ref
    /// in the source repository, or its keep ref could not be removed.
    InstanceKeepFailed => "NYL-INSTANCE-KEEP-FAILED",
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

/// A labelled fact or action for presenting a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticField {
    pub label: String,
    pub value: String,
}

/// Presentation facts for an error or warning.
///
/// When populated, these fields describe the same facts and remedies as
/// the complete `message` and `fix` record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticPresentation {
    /// A concise headline for structured presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Labelled facts supporting the headline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<DiagnosticField>,
    /// Labelled ways to act on the finding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<DiagnosticField>,
}

/// An error or warning: its code, what happened, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    pub code: Code,
    pub message: String,
    pub fix: String,
    /// Optional facts for human presentation, serialized at the top level.
    #[serde(flatten)]
    pub presentation: Box<DiagnosticPresentation>,
}

impl Finding {
    pub fn new(code: Code, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fix: fix.into(),
            presentation: Box::default(),
        }
    }
    /// Set the headline used for structured presentation.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.presentation.title = Some(title.into());
        self
    }

    /// Add a labelled fact.
    #[must_use]
    pub fn with_detail(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.presentation.details.push(DiagnosticField {
            label: label.into(),
            value: value.into(),
        });
        self
    }

    /// Add a labelled action.
    #[must_use]
    pub fn with_action(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.presentation.actions.push(DiagnosticField {
            label: label.into(),
            value: value.into(),
        });
        self
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
    fn test_finding_presentation_fields_are_optional_and_round_trip() {
        let legacy = serde_json::json!({"code": "NYL-STATE-MISSING", "message": "missing", "fix": "init"});
        let finding: Finding = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(&finding).unwrap(), legacy);
        let rich = finding
            .with_title("State missing")
            .with_detail("Environment", "dev")
            .with_action("Init", "nyl state init");
        let json = serde_json::to_value(&rich).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "code": "NYL-STATE-MISSING", "message": "missing", "fix": "init",
                "title": "State missing", "details": [{"label": "Environment", "value": "dev"}],
                "actions": [{"label": "Init", "value": "nyl state init"}]
            })
        );
        assert_eq!(serde_json::from_value::<Finding>(json).unwrap(), rich);
    }

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
