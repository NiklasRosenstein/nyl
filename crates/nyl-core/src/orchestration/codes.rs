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
    /// Neither `--author` nor Git's `user.name` and `user.email` name who writes a commit Nyl makes: a state commit, or a commit a driver pushes, such as `nyl build --push`.
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
    /// Direct mode (`nyl render`, `diff`, `apply` outside orchestration): a Cluster's recorded capabilities (vendored CRDs included) give a kind one scope and the live cluster another; the command used the recorded one. A unit raises `NYL-UNIT-CAPABILITY-MISMATCH` instead. A warning.
    KubernetesScopeMismatch => "NYL-KUBERNETES-SCOPE-MISMATCH",
    /// Direct mode (`nyl apply` and the `release` commands outside orchestration): a Release declares delete hooks (Helm's `pre-delete` and `post-delete`, Argo CD's `PreDelete` and `PostDelete`), which Argo CD would run when it removes the Release and Nyl never runs; the apply left them out. A unit raises `NYL-UNIT-HOOK-IGNORED` instead. A warning.
    KubernetesHookIgnored => "NYL-KUBERNETES-HOOK-IGNORED",
    /// A unit left a shared object it does not own alone because it differs from what the unit renders. A warning; the `detail` names the object, such as `shared-catalog`.
    UnitSharedDiffers => "NYL-UNIT-SHARED-DIFFERS",
    /// A unit's removal of some of its objects waits for a confirmation the driver never gives, so they remain. A warning.
    UnitRemovalUnconfirmed => "NYL-UNIT-REMOVAL-UNCONFIRMED",
    /// A unit left something in place that is not its to remove, such as a deployment another owner placed. A warning.
    UnitLeftBehind => "NYL-UNIT-LEFT-BEHIND",
    /// What a unit's declaration records about the system it changes disagrees with what the live system reports; the unit used the recorded value. A warning.
    UnitCapabilityMismatch => "NYL-UNIT-CAPABILITY-MISMATCH",
    /// A unit's deployment declares hooks its driver never runs; they were left out. A warning.
    UnitHookIgnored => "NYL-UNIT-HOOK-IGNORED",
    /// Another unit incarnation that still exists owns what this unit would change, so nothing was changed. The unit retries on its own once that incarnation is gone.
    UnitOwnerFenced => "NYL-UNIT-OWNER-FENCED",
    /// Something this unit would change names an owning incarnation whose existence state cannot confirm, so the unit failed closed and changed nothing.
    UnitOwnerUnknown => "NYL-UNIT-OWNER-UNKNOWN",
    /// The inputs a unit's driver read from state changed while it ran, so it changed nothing; the next run resolves the unit again.
    UnitInputsChanged => "NYL-UNIT-INPUTS-CHANGED",
    /// The destination a unit delivered to, or the way it delivers, changed since its current receipt, which the engine does not replace; nothing was changed.
    UnitDestinationChanged => "NYL-UNIT-DESTINATION-CHANGED",
    /// A unit's deployments were not healthy within the time it waits for them. Its effects stay in place, and the next execution waits again.
    UnitHealthTimeout => "NYL-UNIT-HEALTH-TIMEOUT",
    /// A unit's deployments reached a health they do not recover from without a change, so the unit stopped waiting.
    UnitHealthDegraded => "NYL-UNIT-HEALTH-DEGRADED",
    /// A unit's declaration does not say how its driver reaches the system it changes, such as a missing context or endpoint; Nyl never falls back to an ambient one.
    UnitAccessUndeclared => "NYL-UNIT-ACCESS-UNDECLARED",
    /// A unit has a receipt, but the native state its tool keeps of what it manages is missing, so the unit refuses to run until an operator restores it or accepts the empty state.
    UnitNativeStateMissing => "NYL-UNIT-NATIVE-STATE-MISSING",
    /// A unit cannot tell what it delivered before because the history that records it is deeper than the driver reads, so it removed nothing it no longer delivers and failed closed.
    UnitHistoryTooDeep => "NYL-UNIT-HISTORY-TOO-DEEP",
    /// A unit's tool output could not be written to its log file. A warning on the unit's output.
    UnitLog => "NYL-UNIT-LOG",
    /// A unit declares an observation of something it does not produce.
    ObservationUnowned => "NYL-OBSERVATION-UNOWNED",
    /// `nyl build` was given arguments after `--` the unit's kind does not accept, or that do not apply to its spec.
    BuildArgument => "NYL-BUILD-ARGUMENT",
    /// `nyl build --push` would write where an orchestrated unit owns the destination.
    BuildOwned => "NYL-BUILD-OWNED",
    /// `nyl build` of a unit whose declaration renders only for an environment, such as one that declares what it deploys to inline, was not given `-e <environment>`.
    BuildEnvironmentRequired => "NYL-BUILD-ENVIRONMENT-REQUIRED",
    /// `nyl build --push` refuses a worktree with uncommitted changes, which it could not attribute to a source commit.
    BuildDirtySource => "NYL-BUILD-DIRTY-SOURCE",
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
    /// A reference or `dependsOn` entry requires an attestation its producer does not declare, or `nyl attest --name` names one its scope does not declare with `from: external`.
    AttestationUndeclared => "NYL-ATTESTATION-UNDECLARED",
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
    /// `nyl reconcile --detect-drift` was combined with an option that leaves it nothing to check.
    DriftOptionInvalid => "NYL-DRIFT-OPTION-INVALID",
    /// `nyl attest --unit` named a unit without a current receipt to attest, or `nyl attest --name` without `--unit` found no recorded state to attest.
    AttestNoReceipt => "NYL-ATTEST-NO-RECEIPT",
    /// A PromotionPath uses a form M6 does not support: `changeGate: pullRequest`, or a DeploymentTarget in `from` or `to` (M11).
    PromoteUnsupported => "NYL-PROMOTE-UNSUPPORTED",
    /// A PromotionPath's source or target is not a declared Environment.
    PromoteEnvironmentUnknown => "NYL-PROMOTE-ENVIRONMENT-UNKNOWN",
    /// A PromotionPath selector, or a path attestation scope, names a unit the source environment does not select.
    PromoteSelectorUnit => "NYL-PROMOTE-SELECTOR-UNIT",
    /// A PromotionPath selector is malformed, or its pointer, output, or artifact does not resolve against the source unit's kind.
    PromoteSelectorInvalid => "NYL-PROMOTE-SELECTOR-INVALID",
    /// A promoted value's type does not fit a binding that reads it.
    PromoteValueType => "NYL-PROMOTE-VALUE-TYPE",
    /// A `fromPromotion` binding names a path or a value no PromotionPath declares.
    PromoteBindingUnknown => "NYL-PROMOTE-BINDING-UNKNOWN",
    /// A PromotionPath's `coverage` names a unit the source environment does not select, a unit whose kind runs no deployments, or a group or deployment the unit's driver does not list for it.
    PromoteCoverageUnknown => "NYL-PROMOTE-COVERAGE-UNKNOWN",
    /// A PromotionPath's `coverage` cannot take effect as written: a path that promotes only values sets `coverage.<unit>.groups`, which only a path that promotes the target's source uses, or the unit's kind attests nothing per deployment for its spec (no running-scoped attestations), so its `groups` and `exclude` select nothing and a `require` name would always refuse the promotion.
    PromoteCoverageIneffective => "NYL-PROMOTE-COVERAGE-INEFFECTIVE",
    /// The deployments of a unit a PromotionPath's `coverage` names could not be listed, such as when what the unit's declaration renders from cannot be read, so its names are not checked. An error in `nyl validate`; a warning in `plan`, `reconcile`, `promote`, and `get promotion-candidates`, which judge the source by what they need. A promotion still refuses a name no attestation lists.
    PromoteCoverageUnchecked => "NYL-PROMOTE-COVERAGE-UNCHECKED",
    /// The declared type of a field a `fromPromotion` binding fills could not be read, so the promoted value's type is not compared with it: the unit's driver could not read what declares the type, such as a remote source that is neither fetched nor cached (`nyl validate --offline`, a `--local` run). An error in `nyl validate`; a warning in `plan`, `reconcile`, `promote`, and `get promotion-candidates`. Resolution still checks the value against the declaration.
    PromoteValueTypeUnchecked => "NYL-PROMOTE-VALUE-TYPE-UNCHECKED",
    /// `nyl promote` found no recorded state of the source whose evidence reaches the path's level: no state, a unit without a current receipt, a required attestation that is pending, aged past `maxAttestationAge`, or failing, or a publication's running-scoped attestation that no longer describes what runs (only its current receipt's counts; `--state-revision` judges an older state on its own). Nothing was written. Exit 2.
    PromoteNoEvidence => "NYL-PROMOTE-NO-EVIDENCE",
    /// `--evidence published` promoted a state whose evidence is below the path's level; the record says so. A warning.
    PromoteEvidenceOverride => "NYL-PROMOTE-EVIDENCE-OVERRIDE",
    /// A promoted artifact no longer exists where its receipt says it is (`verifyArtifacts`). Nothing was written. Exit 1.
    PromoteArtifactMissing => "NYL-PROMOTE-ARTIFACT-MISSING",
    /// A promoted artifact's kind cannot verify artifacts, so the record says `artifactsVerified: false`. A warning.
    PromoteArtifactUnverified => "NYL-PROMOTE-ARTIFACT-UNVERIFIED",
    /// An environment's `fromPromotion` source, or a `fromPromotion` binding, names a path with no record yet. Exit 2.
    PromotionMissing => "NYL-PROMOTION-MISSING",
    /// The `record: source` block in an Environment's source has a lower `sequence` than the one a run applied, such as an older checkout or a reverted promotion commit. Rolling back is a new promotion. Exit 1.
    PromotionSequenceOlder => "NYL-PROMOTION-SEQUENCE-OLDER",
    /// The `record: source` block in an Environment's source does not match the source state it names, or names a path the Environment does not promote along. Exit 1.
    PromotionRecordInvalid => "NYL-PROMOTION-RECORD-INVALID",
    /// The `record: source` block of an Environment whose `fromPromotion.verify` names an allowed signers file was last changed by a commit `git verify-commit` does not accept against it (unsigned, or signed by a key the file does not list), or its provenance cannot be read (an untracked or changed Environment file, a missing allowed signers file, a shallow history). Nothing ran. Exit 1.
    PromotionRecordUnsigned => "NYL-PROMOTION-RECORD-UNSIGNED",
    /// `nyl promote` cannot commit or push a `record: source` block: the entry worktree has no branch checked out (detached HEAD), the branch has no upstream or commits its upstream lacks, the Environment's file is untracked or has uncommitted changes, the block cannot be placed in the Environment's own document, the Environment is not declared in a file (a template's instance), with `fromPromotion.verify` the commit would not be signed as a run requires (it is undone before the push), the remote refused the push for another reason than a moved branch (branch protection, a hook), or the push never started because a configured Git credential cannot be used or `git` cannot run (the commit is undone). The message says whether anything was written; the fix is the cause's. Exit 1.
    PromoteSourceUnwritable => "NYL-PROMOTE-SOURCE-UNWRITABLE",
    /// The push of a `record: source` commit failed without the remote refusing it, such as a network error after the upload, so it may or may not have reached the remote. The commit is kept on the local branch, and no marker was written into the target's state. Nothing is known to have failed: fetch the branch, check whether the commit reached the remote, and run `nyl promote` again, which writes only the marker when the pushed block is current, or promotes again. Exit 2.
    PromoteSourceUncertain => "NYL-PROMOTE-SOURCE-UNCERTAIN",
    /// `nyl promote` pushed a `record: source` commit, but could not write its marker into the target's desired state, so the sequence is not reserved in state and `nyl get promotions` does not list the promotion. The promotion stands; running `nyl promote` again writes only the marker. Exit 1.
    PromoteMarkerUnwritten => "NYL-PROMOTE-MARKER-UNWRITTEN",
    /// The remote refused the fast-forward push of a `record: source` commit because the branch moved since the checkout (non-fast-forward). The commit was undone, so nothing was written; update the checkout and promote again. Exit 2.
    PromoteSourceRejected => "NYL-PROMOTE-SOURCE-REJECTED",
    /// `nyl promote` would replace commits another PromotionPath promoted into the target since this path last promoted: the promoted source commit or value does not contain them (`git cherry` lists them, by their changes). Nothing was written; merge the missing commits or pass `--supersede`. Exit 2.
    PromoteSupersedes => "NYL-PROMOTE-SUPERSEDES",
    /// The entry checkout is a shallow clone, so `nyl promote` cannot compare histories for the supersede check; fetch the full history (`git fetch --unshallow`, or `fetch-depth: 0`). Nothing was written. Exit 1.
    PromoteShallowClone => "NYL-PROMOTE-SHALLOW-CLONE",
    /// A `nyl promote --supersede` entry has no reason of its own, and the promotion has no `--reason`. Exit 1.
    PromoteSupersedeReason => "NYL-PROMOTE-SUPERSEDE-REASON",
    /// The supersede check cannot check a promoted value: it, or another path's record of it, has no known source commit, such as an image from outside Nyl or a command output. `nyl plan` lists such values of every path into the environment that another path also promotes. A warning.
    PromoteUnchecked => "NYL-PROMOTE-UNCHECKED",
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
    /// A driver attestation of a unit's current receipt failed; `status` also warns with it for every recorded attestation that fails for what the environment runs.
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
    /// Teardown readiness (blocking): an unmet requirement decides that the
    /// unit's teardown cannot complete, such as a deployment it would not
    /// remove, workloads a removal would leave running, or a deletion that
    /// waits for a confirmation. The driver names the requirement in the
    /// message, the setting that fixes it, and the `detail`.
    TeardownBlocked => "NYL-TEARDOWN-BLOCKED",
    /// Teardown readiness (remnant): something the unit owns remains after a
    /// teardown that succeeds. The driver names it in the message and the
    /// `detail`.
    TeardownRemnant => "NYL-TEARDOWN-REMNANT",
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
    /// A short kebab-case slug the driver that raised the finding supplies to
    /// tell its specifics apart under a generic code, such as
    /// `catalog-manual-sync` under `NYL-TEARDOWN-BLOCKED`. For scripts; the
    /// engine never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
            detail: None,
            presentation: Box::default(),
        }
    }

    /// Set the driver's [`detail`](Self::detail) slug.
    #[must_use]
    pub fn with_slug(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
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
            .with_slug("no-state-file")
            .with_title("State missing")
            .with_detail("Environment", "dev")
            .with_action("Init", "nyl state init");
        let json = serde_json::to_value(&rich).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "code": "NYL-STATE-MISSING", "message": "missing", "fix": "init", "detail": "no-state-file",
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
