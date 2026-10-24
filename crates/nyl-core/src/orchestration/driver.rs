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

use super::ids::{ChangeDigest, EnvironmentName, UnitAddress};
use super::ports::CommitIdentity;
use super::state::{ArtifactDocument, AttestationResult, DesiredUnit, Drift, FailureCategory, ObservedUnit, Receipt};
use super::time::Timestamp;

/// A Nyl-side failure of a driver call, such as an unreadable worktree. It
/// gives the unit an `uncertain` condition when an effect may have started,
/// and otherwise fails the invocation with exit 1.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("{message}")]
    Failed {
        message: String,
        /// Whether an effect may have started.
        effects_possible: bool,
    },
}

impl DriverError {
    /// A failure before any effect could start.
    pub fn before_effects(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            effects_possible: false,
        }
    }

    /// A failure after an effect may have started.
    pub fn after_effects(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            effects_possible: true,
        }
    }

    pub fn effects_possible(&self) -> bool {
        match self {
            Self::Failed { effects_possible, .. } => *effects_possible,
        }
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
    pub verify: bool,
    pub inspect: bool,
    pub teardown: bool,
    /// `verify` records observations, such as Argo CD health.
    pub observe: bool,
    /// `nyl build` support; `None` without it.
    pub build: Option<BuildPush>,
    /// `plan` reports a change digest, so `bind: plan` is allowed.
    #[serde(rename = "changeDigest")]
    pub change_digest: bool,
    /// `nyl exec` support; `None` without it.
    pub exec: Option<ExecTerminal>,
}

/// Whether publishing a direct build's artifact outside orchestration is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum BuildPush {
    Safe,
    LoadOnly,
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

/// A typed claim on native resources, checked for overlap across units.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OwnershipScope {
    /// A Terraform or OpenTofu backend: its canonical configuration and key, and workspace.
    TerraformBackend { backend: String, workspace: String },
    /// An OCI repository and the tag prefix one environment moves.
    OciRepository { repository: String, tag_prefix: String },
    /// A publication repository, revision, and path prefix.
    Publication {
        repository: String,
        revision: String,
        path_prefix: String,
    },
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

/// An attestation a driver reports with its outcome or verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttestationOutcome {
    pub name: String,
    pub result: AttestationResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How a reconcile or teardown call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Succeeded {
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
    /// A two-phase teardown waits; effects so far are recorded. Only teardown returns it.
    Waiting {
        phase: String,
        reason: String,
        until: Option<Timestamp>,
    },
    Failed {
        category: FailureCategory,
        retryable: bool,
        message: String,
    },
    /// Effects may or may not have happened.
    Uncertain { reason: String },
}

/// A drift verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Verification {
    pub drift: Drift,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<AttestationOutcome>,
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
#[serde(rename_all = "lowercase")]
pub enum DriverOperation {
    Plan,
    Reconcile,
    Verify,
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
            Self::Verify => "verify",
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
    },
    /// The tool died from a signal it was not sent by Nyl.
    Signaled {
        signal: i32,
    },
    /// The deadline passed; the process group was terminated.
    DeadlineExceeded,
    /// The run is being cancelled; the process group was terminated.
    Cancelled,
    /// The program could not be started, such as a missing binary.
    NotStarted {
        reason: String,
    },
}

/// Runs a driver's external tools. Implemented on Nyl's side: each tool runs
/// in its own process group, its output is masked as it is written, and at
/// the deadline or on cancellation the group receives SIGTERM and, after a
/// grace period, SIGKILL.
pub trait ToolRunner: Send + Sync {
    fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit>;
}

/// What one driver call may use.
pub struct ExecutionContext<'a> {
    pub unit: UnitAddress,
    pub operation: DriverOperation,
    /// A detached checkout of the effective source revision, owned by this execution.
    pub worktree: PathBuf,
    /// The directory containing `nyl.toml` inside `worktree`.
    pub project_dir: PathBuf,
    /// This execution's own directory for tool state.
    pub temp_dir: PathBuf,
    /// Admitted variables and secrets, by name; masking of secret values is the runner's.
    pub env: BTreeMap<String, String>,
    /// The approved change digest of a `bind: plan` unit.
    pub approved_digest: Option<ChangeDigest>,
    pub deadline: Timestamp,
    /// The injected clock's time when the execution starts; a driver's tool
    /// timeout is `deadline - now`. Drivers never read a clock.
    pub now: Timestamp,
    /// The unit's observed file, holding its last receipt.
    pub observed: Option<&'a ObservedUnit>,
    pub tools: &'a dyn ToolRunner,
    /// Who signs commits a driver writes, such as a publication's commit:
    /// the identity the command line resolved for state commits, with
    /// `now` as the commit time. `None` for operations that write nothing
    /// (plan, verify); a driver that must commit without one fails before
    /// effects.
    pub commit_identity: Option<&'a CommitIdentity>,
    /// A cache directory of this invocation's workspace, for render caches;
    /// never the user's default cache, so parallel runs stay isolated.
    pub cache_dir: PathBuf,
}

/// What resolution provides to compute execution-key inputs.
pub struct ResolutionContext<'a> {
    pub environment: EnvironmentName,
    /// A Git worktree of the unit's effective commit: the commit resolution
    /// writes into its Git source fields (S for a unit without `revision`).
    /// Execution-key inputs are read from these bytes.
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

/// One unit kind. Built-in drivers implement it in Rust; methods for
/// capabilities a kind lacks return [`Supported::Unsupported`].
pub trait Driver: Send + Sync {
    /// The kind's group, `units.gitops.nyl/v1` for built-ins.
    fn api_version(&self) -> &'static str;
    fn kind(&self) -> &'static str;
    /// Bumped only when execution semantics change in a way that requires re-execution.
    fn behavior_version(&self) -> u32;
    /// Capabilities for a spec: a Command can tear down only with a `teardown` step.
    fn capabilities(&self, spec: &Value) -> Capabilities;
    /// Recovery policies for a spec: a Command converges only with `idempotent: true`.
    fn recovery(&self, spec: &Value) -> RecoveryPolicies;
    /// The kind's own fields; Nyl adds the common ones.
    fn spec_schema(&self) -> schemars::Schema;
    /// Parse and validate a rendered spec, references still as expressions.
    fn validate_spec(&self, spec: &Value) -> std::result::Result<(), String>;
    /// Attestation names the kind can produce for a spec.
    fn attestations(&self, spec: &Value) -> Vec<String>;
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
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs>;

    /// Plan an execution; for a `deleting` unit with teardown intent, its teardown, whose digest approvals bind.
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>>;
    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>>;
    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>>;
    fn inspect(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Inspection>>;
    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>>;
    fn build(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        push: bool,
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>>;
    fn exec(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>>;
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

    /// Every registered `(apiVersion, kind)`.
    pub fn kinds(&self) -> impl Iterator<Item = (&str, &str)> {
        self.drivers
            .keys()
            .map(|(api_version, kind)| (api_version.as_str(), kind.as_str()))
    }
}

impl std::fmt::Debug for DriverSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.drivers.keys()).finish()
    }
}
