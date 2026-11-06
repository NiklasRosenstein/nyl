//! The driver interface: how orchestration runs one unit kind.
//!
//! Contract: [Drivers](../../../../design/orchestration-core.md#drivers) and
//! [Recovery](../../../../design/orchestration-core.md#recovery).
//!
//! Every type crossing [`Driver`] is plain data with a JSON representation,
//! so a process adapter for plugin drivers stays possible. The one handle a
//! driver receives is [`ToolRunner`], through which it runs every external
//! tool: transcripts, secret masking, deadlines, cancellation, and process
//! groups are therefore uniform across drivers and live on Nyl's side.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::codes::Finding;
use super::ids::{ChangeDigest, CommitId, EnvironmentName, Uid, UnitAddress, UnitName};
use super::ports::CommitIdentity;
use super::state::{ArtifactDocument, AttestationResult, DesiredUnit, Drift, FailureCategory, ObservedUnit, Receipt};
use super::time::Timestamp;

/// A Nyl-side failure of a driver call, such as an unreadable worktree. It
/// gives the unit an `uncertain` condition when an effect may have started,
/// and otherwise fails the invocation with exit 1.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DriverError {
    message: String,
    /// Whether an effect may have started.
    effects_possible: bool,
}

impl DriverError {
    /// A failure before any effect could start.
    pub fn before_effects(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            effects_possible: false,
        }
    }

    /// A failure after an effect may have started.
    pub fn after_effects(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            effects_possible: true,
        }
    }

    pub fn effects_possible(&self) -> bool {
        self.effects_possible
    }
}

pub type DriverResult<T> = std::result::Result<T, DriverError>;

/// A method's answer, or that the kind lacks the capability. Unsupported is
/// reported, never silently skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "supported", content = "result", rename_all = "lowercase")]
pub enum Supported<T> {
    #[serde(rename = "true")]
    Supported(T),
    #[serde(rename = "false")]
    Unsupported,
}

/// What a kind can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)] // Independent capabilities, as the contract's struct lists them.
pub struct Capabilities {
    pub plan: bool,
    pub reconcile: bool,
    /// `nyl detect-drift` checks whether a current receipt still holds.
    #[serde(rename = "detectDrift")]
    pub detect_drift: bool,
    pub inspect: bool,
    pub teardown: bool,
    /// `nyl attest` asks the driver to attest current receipts, such as a
    /// publication's Argo CD acceptance and health.
    pub attest: bool,
    /// Whether the kind supports `nyl build`.
    pub build: bool,
    /// `plan` reports a change digest, so `bind: plan` is allowed.
    #[serde(rename = "changeDigest")]
    pub change_digest: bool,
    /// `nyl exec` support; `None` without it.
    pub exec: Option<ExecTerminal>,
    /// This kind requires committed inputs rather than a worktree snapshot.
    #[serde(default, rename = "requiresCommittedSource")]
    pub requires_committed_source: bool,
}

/// Whether an operator tool takes the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ExecTerminal {
    Interactive,
    NonInteractive,
}

/// What an uncertain execution or teardown needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryPolicy {
    /// Re-executing the same document is safe.
    Converge,
    /// The driver's `inspect` reports applied, not applied, or unknown.
    Inspect,
    /// An operator decides with `recover`.
    Manual,
}

/// Recovery policies for execution and for teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryPolicies {
    pub execution: RecoveryPolicy,
    pub teardown: RecoveryPolicy,
}

/// A driver's claim on a resource identity and a subtree beneath it.
/// Equal namespaces and keys overlap when either path contains the other.
/// Drivers canonicalize keys and path segments; descriptions are display-only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipScope {
    pub namespace: String,
    pub key: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    pub description: String,
}

impl OwnershipScope {
    /// Whether these claims intersect, independently of their display text.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.namespace == other.namespace
            && self.key == other.key
            && (self.path.starts_with(&other.path) || other.path.starts_with(&self.path))
    }
}

/// What resolution needs from a driver to compute an execution key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct KeyInputs {
    /// Selected source files by worktree-rooted path, with their content digests.
    pub files: BTreeMap<String, String>,
    /// Fields of the resolved spec, as JSON Pointers, that change how a unit runs but not its result.
    #[serde(rename = "excludedFields")]
    pub excluded_fields: Vec<String>,
    /// Output of a fingerprint step, digested into the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Facts the driver learned while reading its inputs that the resolved
    /// spec records, by JSON Pointer to the member that receives each value,
    /// such as a publication's effective teardown settings. Resolution writes
    /// them into the resolved spec before it computes the key, so the stored
    /// desired document carries them and commands that never render source
    /// can read them back. The engine stores each value without
    /// interpreting it; see [`KeyInputs::record_into`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recorded: BTreeMap<String, Value>,
}

/// Why a recorded field cannot join the resolved spec.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// The pointer is not `/<member>`: recorded facts are new top-level members.
    #[error("recorded field {pointer:?} is not a JSON Pointer to one top-level member of the spec")]
    NotAMember { pointer: String },
    /// The member is a common unit field, or is already in the spec the engine
    /// resolved, and the engine has used it.
    #[error("recorded field {pointer:?} would replace a member the unit declares or the engine reads; a driver records only new members")]
    Occupied { pointer: String },
}

impl KeyInputs {
    /// Write each [`recorded`](Self::recorded) value into the resolved `spec`
    /// as a new top-level member. A recorded fact never replaces a member the
    /// spec already has, nor a common unit field, because the engine reads
    /// those; such a pointer is an error, never skipped.
    pub fn record_into(&self, spec: &mut Value) -> std::result::Result<(), RecordError> {
        for (pointer, value) in &self.recorded {
            let member = pointer
                .strip_prefix('/')
                .filter(|member| !member.is_empty() && !member.contains('/'))
                .ok_or_else(|| RecordError::NotAMember {
                    pointer: pointer.clone(),
                })?
                .replace("~1", "/")
                .replace("~0", "~");
            let occupied = || RecordError::Occupied {
                pointer: pointer.clone(),
            };
            let Value::Object(members) = spec else {
                return Err(occupied());
            };
            if members.contains_key(&member) || crate::resources::units::COMMON_UNIT_FIELDS.contains(&member.as_str()) {
                return Err(occupied());
            }
            members.insert(member, value.clone());
        }
        Ok(())
    }
}

/// The planned change kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    None,
    Create,
    Update,
    Destroy,
}

/// A driver's plan for one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanReport {
    pub changes: Change,
    /// Human summary with secrets masked.
    pub summary: String,
    #[serde(rename = "changeDigest", default, skip_serializing_if = "Option::is_none")]
    pub change_digest: Option<ChangeDigest>,
    /// Pointers the driver could not plan without.
    #[serde(rename = "blockedBy", default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// An attestation a driver reports with a reconcile outcome or from `attest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttestationOutcome {
    pub name: String,
    pub result: AttestationResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Driver-specific evidence behind the result, such as each Argo CD
    /// Application's running publication and health; recorded unchanged.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

/// How a reconcile or teardown call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Succeeded {
        /// Native tool versions recorded in the receipt, independent of key material.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        versions: BTreeMap<String, String>,
        /// Every non-sensitive output the tool reported, declared or not. Nyl
        /// validates them against the declarations, warns about undeclared ones
        /// (`NYL-OUTPUT-UNDECLARED`), and records only declared ones.
        outputs: Value,
        /// Artifact documents without `metadata.unit` and `executionKey`, which Nyl fills in.
        artifacts: Vec<ArtifactDocument>,
        attestations: Vec<AttestationOutcome>,
    },
    /// The plan changed since its approval; nothing was applied.
    AwaitingApproval { digest: ChangeDigest },
    /// A two-phase teardown waits; effects so far are recorded. Only teardown
    /// returns it. `until` is when the wait ends by itself (a `delay`), or
    /// `None` when only an observation or a confirmation ends it;
    /// `published` is the phase 1 commit, the teardown's published evidence.
    Waiting {
        phase: String,
        reason: String,
        until: Option<Timestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        published: Option<CommitId>,
    },
    Failed {
        category: FailureCategory,
        retryable: bool,
        message: String,
    },
    /// A failure requiring an operator's reason before retry. The context is
    /// non-secret driver-owned data, recorded and returned without interpretation.
    RetryRequired {
        category: FailureCategory,
        message: String,
        context: Value,
    },
    /// Effects may or may not have happened.
    Uncertain { reason: String },
}

/// What a drift detection found for a current receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriftDetection {
    pub drift: Drift,
    /// Driver-specific details, recorded with the drift result unchanged.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

/// What an attestation check found for a current receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "attestation", rename_all = "kebab-case")]
pub enum AttestationCheck {
    /// The driver attested the receipt; a declared name it left out stays pending.
    Attested { outcomes: Vec<AttestationOutcome> },
    /// The driver could not tell, such as an attest step that failed; nothing is recorded.
    Unknown { reason: String },
}

impl From<Drift> for DriftDetection {
    fn from(drift: Drift) -> Self {
        Self {
            drift,
            details: Value::Null,
        }
    }
}

/// The three results of the `inspect` recovery policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "inspection", rename_all = "kebab-case")]
pub enum Inspection {
    Applied {
        outputs: Value,
        artifacts: Vec<ArtifactDocument>,
    },
    NotApplied,
    Unknown {
        reason: String,
    },
}

/// How an operator's tool ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ExecStatus {
    Exited { code: i32 },
    Signaled { signal: i32 },
}

/// The operation a driver call serves, as `NYL_OPERATION` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DriverOperation {
    Plan,
    Reconcile,
    DetectDrift,
    Attest,
    Inspect,
    Teardown,
    Fingerprint,
    Build,
    Exec,
}

impl DriverOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Reconcile => "reconcile",
            Self::DetectDrift => "detect-drift",
            Self::Attest => "attest",
            Self::Inspect => "inspect",
            Self::Teardown => "teardown",
            Self::Fingerprint => "fingerprint",
            Self::Build => "build",
            Self::Exec => "exec",
        }
    }
}

/// One external tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub working_dir: PathBuf,
    /// The complete environment: tools start from nothing else.
    pub env: BTreeMap<String, String>,
    /// Bytes on stdin; none closes it.
    pub stdin: Option<Vec<u8>>,
    /// Capture stdout for structured parsing without copying it to the transcript.
    pub private_stdout: bool,
    /// Forward operator-tool stdout directly, independently of terminal interactivity.
    pub forward_stdout: bool,
    /// Give an operator tool the terminal; callers enable this only on a terminal.
    pub interactive: bool,
    /// How long the tool may run: the driver computes it as the context's
    /// `deadline - now`, so no runner reads a clock to decide it. At expiry
    /// the runner sends SIGTERM to the process group and SIGKILL 30 seconds
    /// later, and reports [`ToolExit::DeadlineExceeded`]. `None` falls back
    /// to the runner's default, if it has one.
    pub timeout: Option<std::time::Duration>,
    /// Values the transcript replaces with `***`: the admitted secrets of
    /// this execution, in addition to any the runner masks by default.
    pub masked: Vec<String>,
}

/// How a tool invocation ended. Its stdout and stderr went to the masked
/// transcript; `stdout` is also returned for steps whose output Nyl reads,
/// such as a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExit {
    Exited {
        code: i32,
        stdout: Vec<u8>,
        /// Raw diagnostics for driver classification, bounded to 1 MiB; transcripts remain masked.
        stderr: Vec<u8>,
    },
    /// The tool died from a signal it was not sent by Nyl.
    Signaled { signal: i32 },
    /// The deadline passed; the process group was terminated.
    DeadlineExceeded,
    /// The run is being cancelled; the process group was terminated.
    Cancelled,
    /// The program could not be started, such as a missing binary.
    NotStarted { reason: String },
}

/// Runs a driver's external tools. Implemented on Nyl's side: each tool runs
/// in its own process group, its output is masked as it is written, and at
/// the deadline or on cancellation the group receives SIGTERM and, after a
/// grace period, SIGKILL.
pub trait ToolRunner: Send + Sync {
    fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit>;

    /// Run a unit's tool with its address attached to the public transcript.
    /// Captured stdout/stderr retain the tool's original bytes.
    fn run_for_unit(&self, invocation: ToolInvocation, _unit: &UnitAddress) -> DriverResult<ToolExit> {
        self.run(invocation)
    }
}

/// A source field's independent checkout and authored path root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoot {
    pub worktree: PathBuf,
    pub project_dir: PathBuf,
}

/// What one driver call may use.
pub struct ExecutionContext<'a> {
    /// Checkouts by Git source field JSON Pointer; fields may name different repositories and commits.
    pub source_roots: BTreeMap<String, SourceRoot>,
    pub unit: UnitAddress,
    pub operation: DriverOperation,
    /// The main checkout at the environment source commit S; source fields use source_roots.
    pub worktree: PathBuf,
    /// The directory containing `nyl.toml` inside `worktree`.
    pub project_dir: PathBuf,
    /// This execution's own directory for tool state.
    pub temp_dir: PathBuf,
    /// Admitted variables and secrets, by name; masking of secret values is the runner's.
    pub env: BTreeMap<String, String>,
    /// Explicit credential-helper keys, resolved only for this driver call.
    pub secrets: BTreeMap<String, String>,
    /// Whether an operator call may inherit the terminal.
    pub interactive: bool,
    /// The approved change digest of a `bind: plan` unit.
    pub approved_digest: Option<ChangeDigest>,
    pub deadline: Timestamp,
    /// The injected clock's time when the execution starts; a driver's tool
    /// timeout is `deadline - now`. Drivers never read a clock.
    pub now: Timestamp,
    /// The unit's observed file, holding its last receipt.
    pub observed: Option<&'a ObservedUnit>,
    /// Artifact documents belonging to the observed receipt.
    pub artifacts: BTreeMap<String, ArtifactDocument>,
    pub tools: &'a dyn ToolRunner,
    /// Who authors commits a driver writes, such as a publication's commit:
    /// the identity the command line resolved for state commits, with
    /// `now` as the commit time. `None` for operations that write nothing
    /// (plan, detect-drift, attest); a driver that must commit without one fails before
    /// effects.
    pub commit_identity: Option<&'a CommitIdentity>,
    /// A cache directory of this invocation's workspace, for render caches;
    /// never the user's default cache, so parallel runs stay isolated.
    pub cache_dir: PathBuf,
    /// For a teardown call after phase 1: how its `teardownWait` ended, so
    /// the driver runs phase 2; `None` while the wait has not ended, and for
    /// every other call. Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit), Teardown.
    pub teardown_wait: Option<WaitCompletion>,
    /// Whether other unit incarnations still exist in their environments'
    /// state, for a driver that fences resources by owner.
    pub incarnations: &'a dyn IncarnationDirectory,
}

/// How a teardown's wait after phase 1 ended, decided by the orchestration
/// shell (`rules::teardown_wait`) and recorded in the transition commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "completion", rename_all = "kebab-case")]
pub enum WaitCompletion {
    /// An observer saw the workload Applications gone (`observe`).
    Observed,
    /// The `delay` passed; removal was not observed.
    Delayed,
    /// An operator confirmed removal (`--confirm-removed`, a signal, or Enter on a terminal).
    Confirmed {
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Where the confirmation came from, such as a CI run URL or `terminal`.
        from: String,
    },
    /// `--allow-incomplete`: proceeding without any claim of removal.
    Incomplete,
}

/// What a driver's static check of its unit's teardown found: the preflight
/// every teardown runs, and the readiness `status`, `plan`, and `get units
/// -o wide` show. The driver evaluates its own requirements; shared code
/// only coordinates and displays the findings, which carry stable codes and
/// the setting that fixes each.
///
/// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// teardown readiness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeardownReadiness {
    /// Unmet requirements that decide whether the teardown can succeed. They
    /// keep the unit pending teardown unless the invocation passes
    /// `--allow-incomplete`.
    pub blocking: Vec<Finding>,
    /// What a teardown that succeeds leaves behind.
    pub remnants: Vec<Finding>,
    /// Requirements the driver cannot see, which never block.
    pub not_checked: Vec<String>,
}

impl TeardownReadiness {
    /// Whether no blocking requirement is unmet.
    pub fn ready(&self) -> bool {
        self.blocking.is_empty()
    }
}

/// Whether a unit incarnation still exists in its environment's state.
///
/// A driver that fences a native resource by owner asks this about the
/// incarnation it finds recorded as the owner, so a unit takes a resource
/// over only once the previous owner is gone from state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncarnationStatus {
    /// The environment's state holds the incarnation, active, held,
    /// deleting, or retained.
    Exists,
    /// The environment's state holds no such incarnation: its teardown
    /// completed, or it was released with `nyl state forget`.
    Gone,
    /// State could not tell, such as an environment that is not declared or
    /// a state location that cannot be read. A driver fails closed.
    Unknown(String),
}

/// The lookup of [`IncarnationStatus`] for one invocation. Contract:
/// [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// ownership index.
pub trait IncarnationDirectory: Send + Sync {
    /// Whether incarnation `uid` of unit `unit` still exists in `environment`.
    fn status(&self, environment: &EnvironmentName, unit: &UnitName, uid: &Uid) -> IncarnationStatus;
}

/// A directory that knows nothing: every incarnation is
/// [`IncarnationStatus::Unknown`]. For calls that never take an owned
/// resource over.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnknownIncarnations;

impl IncarnationDirectory for UnknownIncarnations {
    fn status(&self, environment: &EnvironmentName, unit: &UnitName, _uid: &Uid) -> IncarnationStatus {
        IncarnationStatus::Unknown(format!(
            "this call has no access to the state of environment {environment} for unit {unit}"
        ))
    }
}

/// What resolution provides to compute execution-key inputs.
pub struct ResolutionContext<'a> {
    /// The same source checkouts execution uses, keyed by Git source field JSON Pointer.
    pub source_roots: BTreeMap<String, SourceRoot>,
    pub environment: EnvironmentName,
    /// The main checkout at S. Read each source field's bytes from source_roots.
    pub worktree: PathBuf,
    /// The directory containing `nyl.toml` inside `worktree`.
    pub project_dir: PathBuf,
    /// Passthrough variables, without secrets, for fingerprint steps.
    pub env: BTreeMap<String, String>,
    pub deadline: Timestamp,
    /// The injected clock's time; a fingerprint's timeout is `deadline - now`.
    pub now: Timestamp,
    pub tools: &'a dyn ToolRunner,
    /// A cache directory of this invocation's workspace, as for executions.
    pub cache_dir: PathBuf,
}

/// A spec field holding a local path, or an array of local path globs, by
/// the local path rule: relative to the `nyl.toml` directory, or `/`-rooted
/// at the Git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPathField {
    /// JSON Pointer of the field, such as `/workingDir` or `/files`.
    pub pointer: String,
    /// Whether an absent field means the `nyl.toml` directory, which
    /// resolution then records explicitly.
    pub defaults_to_project_dir: bool,
}

/// One unit kind. Built-in drivers implement it in Rust; methods for
/// capabilities a kind lacks return [`Supported::Unsupported`].
pub trait Driver: Send + Sync {
    /// The kind's group, `units.gitops.nyl/v1` for built-ins.
    fn api_version(&self) -> &'static str;
    fn kind(&self) -> &'static str;
    /// Bumped only when execution semantics change in a way that requires re-execution.
    fn behavior_version(&self) -> u32;
    /// The execution timeout when the common `timeout` field is omitted.
    fn default_timeout(&self) -> super::time::Duration {
        super::time::Duration::from_mins(30)
    }
    /// Capabilities for a spec: a Command can tear down only with a `teardown` step.
    fn capabilities(&self, spec: &Value) -> Capabilities;
    /// Recovery policies for a spec: a Command converges only with `idempotent: true`.
    fn recovery(&self, spec: &Value) -> RecoveryPolicies;
    /// The kind's own fields; Nyl adds the common ones.
    fn spec_schema(&self) -> schemars::Schema;
    /// Parse and validate a rendered spec, references still as expressions.
    fn validate_spec(&self, spec: &Value) -> std::result::Result<(), String>;
    /// Provider keys explicitly declared by this spec for driver credentials.
    fn secret_refs(&self, _spec: &Value) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    /// Additional environment admitted by the driver's explicit credential declaration.
    /// The snapshot is supplied by the shell; this method performs no process reads.
    fn credential_environment(
        &self,
        _spec: &Value,
        _process_env: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    /// Attestation names the kind can produce for a spec; none by default.
    fn attestations(&self, spec: &Value) -> Vec<String> {
        let _ = spec;
        Vec::new()
    }
    /// The native resources a spec claims in an environment; an image tag prefix names the environment.
    fn ownership_scopes(&self, environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope>;
    /// JSON Pointers of the spec's Git source fields (the `GitSource` shape:
    /// `path`, `revision`, `commit`, `repositoryRef`/`repository`), such as
    /// `/source` or `/context`. Resolution writes the resolved commit and the
    /// worktree-rooted path into each; the key excludes their `commit`.
    ///
    /// Contract: [Units](../../../../design/orchestration-core.md#units),
    /// "Git source shape", and [Pinned commits](../../../../design/orchestration-core.md#pinned-commits).
    fn source_fields(&self, spec: &Value) -> Vec<String> {
        let _ = spec;
        Vec::new()
    }
    /// The spec's local path fields other than Git source fields, such as
    /// a Command's `files` globs and `workingDir`. Resolution rewrites each
    /// into its worktree-rooted form, so the desired document and the
    /// execution key mean the same wherever `nyl.toml` lives.
    ///
    /// Contract: [Units](../../../../design/orchestration-core.md#units),
    /// the local path rule.
    fn local_path_fields(&self, spec: &Value) -> Vec<LocalPathField> {
        let _ = spec;
        Vec::new()
    }
    /// Complete paths and globs this kind reads, in worktree-rooted form.
    /// None requires a full checkout; declared paths permit sparse checkouts.
    fn checkout_paths(&self, spec: &Value) -> Option<Vec<String>> {
        let _ = spec;
        None
    }
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs>;

    /// Plan an execution; for a `deleting` unit with teardown intent, its teardown, whose digest approvals bind.
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>>;
    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>>;
    /// Whether the current `receipt` still holds: `nyl detect-drift`.
    fn detect_drift(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<DriftDetection>>;
    /// The attestations the kind produces for the current `receipt`:
    /// `nyl attest`. Unsupported by default.
    fn attest(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<AttestationCheck>> {
        let _ = (ctx, unit, receipt);
        Ok(Supported::Unsupported)
    }
    /// The `inspect` recovery policy's check; unsupported by default.
    fn inspect(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        let _ = (ctx, unit);
        Ok(Supported::Unsupported)
    }
    /// Tear the unit down; unsupported by default.
    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let _ = (ctx, unit);
        Ok(Supported::Unsupported)
    }
    /// What the driver finds, from the stored desired document alone, about
    /// whether the unit's teardown can complete: the readiness the teardown
    /// preflight, `status`, `get units -e -o wide`, `plan`, and `reconcile`
    /// report. Unsupported by default (no requirements). Read statically, so
    /// commands that never render source, such as `status`, can ask it, and
    /// the answer matches what a teardown of that document would do; a
    /// driver records what it needs at resolution
    /// ([`KeyInputs::recorded`]).
    fn teardown_readiness(&self, unit: &DesiredUnit) -> DriverResult<Supported<TeardownReadiness>> {
        let _ = unit;
        Ok(Supported::Unsupported)
    }
    /// `nyl build`; unsupported by default.
    fn build(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        let _ = (ctx, unit, args);
        Ok(Supported::Unsupported)
    }
    /// `nyl exec`; unsupported by default.
    fn exec(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        let _ = (ctx, unit, args);
        Ok(Supported::Unsupported)
    }
}

/// The drivers of one invocation, by the `apiVersion` and `kind` a unit's
/// envelope names. A fake kind registers under the built-in kind it stands in
/// for, so state documents keep the real kind and only dispatch differs.
#[derive(Clone, Default)]
pub struct DriverSet {
    drivers: BTreeMap<(String, String), std::sync::Arc<dyn Driver>>,
}

impl DriverSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver under its own `apiVersion` and `kind`.
    pub fn register(&mut self, driver: std::sync::Arc<dyn Driver>) {
        let key = (driver.api_version().to_owned(), driver.kind().to_owned());
        self.drivers.insert(key, driver);
    }

    /// Register a driver for another `apiVersion` and `kind`, as a fake kind
    /// stands in for a built-in one.
    pub fn register_as(&mut self, api_version: &str, kind: &str, driver: std::sync::Arc<dyn Driver>) {
        self.drivers.insert((api_version.to_owned(), kind.to_owned()), driver);
    }

    /// The driver serving a unit's `apiVersion` and `kind`.
    pub fn get(&self, api_version: &str, kind: &str) -> Option<&dyn Driver> {
        self.drivers
            .get(&(api_version.to_owned(), kind.to_owned()))
            .map(std::convert::AsRef::as_ref)
    }
}

impl std::fmt::Debug for DriverSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.drivers.keys()).finish()
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn test_claims_compare_namespaces_keys_and_path_segments_independently_of_descriptions() {
        let parent = OwnershipScope {
            namespace: "test-tree".to_owned(),
            key: vec!["resource".to_owned()],
            path: vec!["app".to_owned()],
            description: "parent".to_owned(),
        };
        let mut child = parent.clone();
        child.path.push("web".to_owned());
        child.description = "child".to_owned();
        assert!(parent.overlaps(&child) && child.overlaps(&parent));
        child.path = vec!["application".to_owned()];
        assert!(!parent.overlaps(&child));
        child.path = parent.path.clone();
        child.key.push("other".to_owned());
        assert!(!parent.overlaps(&child));
        child.key = parent.key.clone();
        child.namespace = "other".to_owned();
        assert!(!parent.overlaps(&child));
        child.namespace = parent.namespace.clone();
        child.path.clear();
        assert!(parent.overlaps(&child));
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use serde_json::json;

    fn recording(pointer: &str, value: Value) -> KeyInputs {
        KeyInputs {
            recorded: BTreeMap::from([(pointer.to_owned(), value)]),
            ..KeyInputs::default()
        }
    }

    /// Catches a driver's recorded fact missing from the stored resolved
    /// document, where commands that never render source read it.
    #[test]
    fn test_recorded_fields_join_the_resolved_spec_as_new_members() {
        let mut spec = json!({"target": "dev"});
        let mut inputs = recording("/teardownSettings", json!({"catalogPrunes": true}));
        inputs.recorded.insert("/a~1b".to_owned(), json!(2));
        inputs.record_into(&mut spec).unwrap();
        assert_eq!(
            spec,
            json!({"target": "dev", "teardownSettings": {"catalogPrunes": true}, "a/b": 2})
        );
    }

    /// Catches a driver overwriting what the engine already used (the
    /// authored target, `dependsOn`, `approval`) after resolution, or a
    /// recorded fact with no place to go being skipped.
    #[test]
    fn test_recorded_field_may_not_replace_or_miss() {
        let mut spec = json!({"target": "dev", "nested": {"keep": 1}});
        for pointer in ["/target", "/nested", "/dependsOn", "/approval", "/timeout"] {
            assert_eq!(
                recording(pointer, json!(1)).record_into(&mut spec),
                Err(RecordError::Occupied {
                    pointer: pointer.to_owned()
                }),
                "{pointer}"
            );
        }
        for pointer in ["/nested/new", "", "/"] {
            assert!(
                matches!(
                    recording(pointer, json!(1)).record_into(&mut spec),
                    Err(RecordError::NotAMember { .. })
                ),
                "{pointer:?}"
            );
        }
        assert_eq!(spec, json!({"target": "dev", "nested": {"keep": 1}}));
    }
}
