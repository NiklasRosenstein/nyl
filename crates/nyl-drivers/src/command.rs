//! The `Command` unit kind: runs argv steps, without a shell, in a pinned
//! worktree of the source commit.
//!
//! Contract: [Command unit](../../../design/orchestration-core.md#command-unit)
//! and [Drivers](../../../design/orchestration-core.md#drivers). Every
//! process runs through the context's [`ToolRunner`](nyl_core::orchestration::driver::ToolRunner)
//! with an environment built here from nothing: `PATH` from the runner,
//! `HOME` as a fresh directory, the `NYL_*` variables, and the admitted
//! passthrough and secrets. The spec type is
//! [`CommandSpec`](nyl_core::resources::units::CommandSpec).
//!
//! - Every step's timeout is the context's `deadline - now`; the driver never
//!   reads a clock.
//! - `reconcile` returns every non-sensitive output the command wrote,
//!   declared or not; the shell records the declared ones and warns about the
//!   rest (`NYL-OUTPUT-UNDECLARED`). Sensitive outputs are validated, then
//!   dropped.
//! - A failure is retryable, and an uncertain execution converges, only with
//!   `idempotent: true`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use nyl_core::orchestration::driver::{
    AttestationOutcome, Capabilities, Change, Driver, DriverError, DriverOperation, DriverResult, ExecStatus,
    ExecutionContext, Inspection, KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy,
    ResolutionContext, Supported, ToolExit, ToolInvocation, ToolRunner, Verification,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{
    ArtifactDocument, AttestationResult, DesiredUnit, Drift, FailureCategory, Lifecycle, Receipt,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::units::{CommandSpec, KIND_COMMAND};
use nyl_render::util::project_path::ProjectPaths;
use serde_json::{Map, Value};

/// Largest outputs file a step may write: 1 MiB.
pub const MAX_OUTPUTS_BYTES: u64 = 1024 * 1024;

/// The built-in `Command` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandDriver;

/// Parse a spec, as rendered or resolved.
fn parse(spec: &Value) -> Result<CommandSpec, String> {
    serde_json::from_value::<CommandSpec>(spec.clone()).map_err(|error| error.to_string())
}

fn parse_for_driver(spec: &Value) -> DriverResult<CommandSpec> {
    parse(spec).map_err(|error| DriverError::before_effects(format!("invalid Command spec: {error}")))
}

/// The resolved spec of a desired document, which execution requires.
fn resolved(unit: &DesiredUnit) -> DriverResult<CommandSpec> {
    let spec = unit
        .resolved_spec
        .as_ref()
        .ok_or_else(|| DriverError::before_effects(format!("unit {} has no resolved spec", unit.unit.name)))?;
    parse_for_driver(spec)
}

/// The tool timeout of a context: `deadline - now`, never negative.
fn timeout(deadline: Timestamp, now: Timestamp) -> std::time::Duration {
    (deadline - now).to_std().unwrap_or_default()
}

/// The attestation names the command itself produces.
fn own_attestations(spec: &CommandSpec) -> Vec<String> {
    spec.common
        .attestations
        .iter()
        .filter(|attestation| attestation.from.is_none())
        .map(|attestation| attestation.name.clone())
        .collect()
}

fn values_of(spec: &CommandSpec) -> Value {
    if spec.values.is_null() {
        Value::Object(Map::new())
    } else {
        spec.values.clone()
    }
}

fn io_error(what: &str, path: &Path, error: &std::io::Error) -> DriverError {
    DriverError::before_effects(format!("cannot {what} {}: {error}", path.display()))
}

/// Canonical worktree and project directory.
fn roots(worktree: &Path, project_dir: &Path) -> DriverResult<ProjectPaths> {
    let worktree = worktree
        .canonicalize()
        .map_err(|error| io_error("resolve the worktree", worktree, &error))?;
    let project_dir = project_dir
        .canonicalize()
        .map_err(|error| io_error("resolve the project directory", project_dir, &error))?;
    if !project_dir.starts_with(&worktree) {
        return Err(DriverError::before_effects(format!(
            "the project directory {} is outside the worktree {}",
            project_dir.display(),
            worktree.display()
        )));
    }
    Ok(ProjectPaths::new(project_dir, worktree))
}

/// The working directory of every step: `workingDir` under the local path
/// rule, `/` for the worktree root, default the `nyl.toml` directory.
fn working_dir(paths: &ProjectPaths, spec: &CommandSpec) -> DriverResult<PathBuf> {
    let directory = match spec.working_dir.as_deref() {
        None => paths.project_root.clone(),
        Some("/") => paths.worktree_root.clone(),
        Some(directory) => paths
            .resolve("spec.workingDir", directory)
            .map_err(|error| DriverError::before_effects(error.to_string()))?,
    };
    if !directory.is_dir() {
        return Err(DriverError::before_effects(format!(
            "spec.workingDir {} is not a directory in the worktree",
            directory.display()
        )));
    }
    Ok(directory)
}

/// Lexically normalize `path`, which must be absolute.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized
}

/// The program of an argv: a relative first element containing `/` resolves
/// against the working directory and must stay in the worktree; a bare name
/// is looked up on the step's `PATH`.
fn program(paths: &ProjectPaths, working_dir: &Path, argv0: &str) -> DriverResult<PathBuf> {
    let path = Path::new(argv0);
    if path.is_absolute() || !argv0.contains('/') {
        return Ok(path.to_path_buf());
    }
    let resolved = normalize(&working_dir.join(path));
    if !resolved.starts_with(&paths.worktree_root) {
        return Err(DriverError::before_effects(format!(
            "program {argv0:?} resolves outside the worktree"
        )));
    }
    Ok(resolved)
}

/// A fresh directory beneath `parent`, named after `stem`.
fn fresh_dir(parent: &Path, stem: &str) -> DriverResult<PathBuf> {
    std::fs::create_dir_all(parent).map_err(|error| io_error("create", parent, &error))?;
    for index in 0u32.. {
        let candidate = parent.join(format!("{stem}-{index}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("create", &candidate, &error)),
        }
    }
    unreachable!("u32 directory names are exhausted")
}

fn write_file(path: &Path, bytes: &[u8]) -> DriverResult<()> {
    std::fs::write(path, bytes).map_err(|error| io_error("write", path, &error))
}

/// One step to run: its argv, operation, and `NYL_INPUTS` content.
struct Step<'a> {
    argv: &'a [String],
    operation: DriverOperation,
    inputs: Value,
    /// Whether the step gets `NYL_OUTPUTS`.
    outputs: bool,
}

/// What a step needs from its context.
struct StepContext<'a> {
    worktree: &'a Path,
    project_dir: &'a Path,
    /// Where the step's fresh directory goes.
    scratch: &'a Path,
    /// Admitted variables and secrets.
    env: &'a BTreeMap<String, String>,
    unit: Option<String>,
    environment: String,
    timeout: std::time::Duration,
    tools: &'a dyn ToolRunner,
    /// Whether secrets are admitted (never for a fingerprint).
    secrets: bool,
}

/// A prepared step: the invocation and the files it reports through.
struct Prepared {
    invocation: ToolInvocation,
    directory: PathBuf,
    outputs: Option<PathBuf>,
    attestations: Option<PathBuf>,
}

fn prepare(ctx: &StepContext<'_>, spec: &CommandSpec, step: &Step<'_>) -> DriverResult<Prepared> {
    let paths = roots(ctx.worktree, ctx.project_dir)?;
    let working_dir = working_dir(&paths, spec)?;
    let program = program(&paths, &working_dir, &step.argv[0])?;
    let directory = fresh_dir(ctx.scratch, step.operation.as_str())?;
    let home = directory.join("home");
    std::fs::create_dir(&home).map_err(|error| io_error("create", &home, &error))?;

    let mut env = BTreeMap::new();
    if let Some(path) = ctx.env.get("PATH") {
        env.insert("PATH".to_owned(), path.clone());
    }
    let mut masked = Vec::new();
    for name in &spec.common.env.passthrough {
        if let Some(value) = ctx.env.get(name) {
            env.insert(name.clone(), value.clone());
        }
    }
    if ctx.secrets {
        for name in spec.common.env.secrets.keys() {
            if let Some(value) = ctx.env.get(name) {
                env.insert(name.clone(), value.clone());
                masked.push(value.clone());
            }
        }
    }
    env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
    let inputs = directory.join("inputs.json");
    let bytes = serde_json::to_vec(&step.inputs).expect("JSON values serialize");
    write_file(&inputs, &bytes)?;
    env.insert("NYL_INPUTS".to_owned(), inputs.to_string_lossy().into_owned());
    let outputs = if step.outputs {
        let path = directory.join("outputs.json");
        write_file(&path, b"")?;
        env.insert("NYL_OUTPUTS".to_owned(), path.to_string_lossy().into_owned());
        Some(path)
    } else {
        None
    };
    let attestations = if own_attestations(spec).is_empty() || step.operation == DriverOperation::Fingerprint {
        None
    } else {
        let path = directory.join("attestations.json");
        write_file(&path, b"")?;
        env.insert("NYL_ATTESTATIONS".to_owned(), path.to_string_lossy().into_owned());
        Some(path)
    };
    if let Some(unit) = &ctx.unit {
        env.insert("NYL_UNIT".to_owned(), unit.clone());
    }
    env.insert("NYL_ENVIRONMENT".to_owned(), ctx.environment.clone());
    env.insert("NYL_OPERATION".to_owned(), step.operation.as_str().to_owned());

    Ok(Prepared {
        invocation: ToolInvocation {
            program,
            args: step.argv[1..].iter().map(OsString::from).collect(),
            working_dir,
            env,
            stdin: None,
            timeout: Some(ctx.timeout),
            masked,
        },
        directory,
        outputs,
        attestations,
    })
}

/// The finished step: how it exited and its prepared files.
struct Ran {
    exit: ToolExit,
    prepared: Prepared,
}

fn run(ctx: &StepContext<'_>, spec: &CommandSpec, step: &Step<'_>) -> DriverResult<Ran> {
    let prepared = prepare(ctx, spec, step)?;
    let exit = ctx.tools.run(prepared.invocation.clone())?;
    Ok(Ran { exit, prepared })
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // The step's inputs, outputs, and HOME live only for the step.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn execution_context<'a>(ctx: &'a ExecutionContext<'a>, secrets: bool) -> StepContext<'a> {
    StepContext {
        worktree: &ctx.worktree,
        project_dir: &ctx.project_dir,
        scratch: &ctx.temp_dir,
        env: &ctx.env,
        unit: Some(ctx.unit.unit.to_string()),
        environment: ctx.unit.environment.to_string(),
        timeout: timeout(ctx.deadline, ctx.now),
        tools: ctx.tools,
        secrets,
    }
}

/// Read and validate the outputs file of a successful step. Returns every
/// non-sensitive output, declared or not.
fn read_outputs(spec: &CommandSpec, path: &Path) -> Result<Value, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("cannot read NYL_OUTPUTS: {error}"))?
        .len();
    if size > MAX_OUTPUTS_BYTES {
        return Err(format!(
            "NYL_OUTPUTS holds {size} bytes, more than the limit of {MAX_OUTPUTS_BYTES}"
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read NYL_OUTPUTS: {error}"))?;
    let mut outputs = if text.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(outputs)) => outputs,
            Ok(other) => {
                return Err(format!(
                    "NYL_OUTPUTS must hold one JSON object, not {}",
                    nyl_core::bindings::json_type_name(&other)
                ))
            }
            Err(error) => return Err(format!("NYL_OUTPUTS is not valid JSON: {error}")),
        }
    };
    let mut problems = Vec::new();
    for (name, declaration) in &spec.common.outputs {
        match outputs.get(name) {
            None => problems.push(format!("declared output {name} is missing")),
            Some(value) if !declaration.output_type.accepts(value) => problems.push(format!(
                "declared output {name} must be of type {}, not {}",
                declaration.output_type.name(),
                nyl_core::bindings::json_type_name(value)
            )),
            Some(_) => {}
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    for (name, declaration) in &spec.common.outputs {
        if declaration.sensitive {
            outputs.remove(name);
        }
    }
    Ok(Value::Object(outputs))
}

/// The attestation results a step wrote, for the names the unit declares.
/// A declared name left out stays pending; anything unreadable yields none.
fn read_attestations(spec: &CommandSpec, path: Option<&Path>) -> Vec<AttestationOutcome> {
    let Some(path) = path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Object(results)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    own_attestations(spec)
        .into_iter()
        .filter_map(|name| {
            let result = match results.get(&name).and_then(Value::as_str) {
                Some("pass") => AttestationResult::Pass,
                Some("fail") => AttestationResult::Fail,
                _ => return None,
            };
            Some(AttestationOutcome {
                name,
                result,
                reason: None,
            })
        })
        .collect()
}

/// The outcome of a `command` or `teardown` step that did not exit 0.
fn unsuccessful(spec: &CommandSpec, step: &str, exit: &ToolExit) -> Outcome {
    let retryable = spec.idempotent;
    match exit {
        ToolExit::Exited { code, .. } => Outcome::Failed {
            category: FailureCategory::ToolError,
            retryable,
            message: format!("{step} exited with code {code}"),
        },
        ToolExit::NotStarted { reason } => Outcome::Failed {
            category: FailureCategory::MissingTool,
            retryable,
            message: format!("{step} could not start: {reason}"),
        },
        ToolExit::Signaled { signal } => Outcome::Uncertain {
            reason: format!("{step} died from signal {signal}"),
        },
        ToolExit::DeadlineExceeded => Outcome::Uncertain {
            reason: format!("{step} passed the unit's timeout and was terminated"),
        },
        ToolExit::Cancelled => Outcome::Uncertain {
            reason: format!("{step} was terminated because the run is being cancelled"),
        },
    }
}

fn is_deleting(unit: &DesiredUnit) -> bool {
    matches!(
        unit.lifecycle,
        Lifecycle::Deleting { .. } | Lifecycle::Held { deletion: Some(_), .. }
    )
}

impl Driver for CommandDriver {
    fn api_version(&self) -> &'static str {
        nyl_core::constants::API_VERSION_UNITS
    }

    fn kind(&self) -> &'static str {
        KIND_COMMAND
    }

    fn behavior_version(&self) -> u32 {
        1
    }

    fn capabilities(&self, spec: &Value) -> Capabilities {
        let parsed = parse(spec).ok();
        Capabilities {
            plan: true,
            reconcile: true,
            verify: parsed.as_ref().is_some_and(|spec| spec.verify.is_some()),
            teardown: parsed.as_ref().is_some_and(|spec| spec.teardown.is_some()),
            ..Capabilities::default()
        }
    }

    fn recovery(&self, spec: &Value) -> RecoveryPolicies {
        let policy = if parse(spec).is_ok_and(|spec| spec.idempotent) {
            RecoveryPolicy::Converge
        } else {
            RecoveryPolicy::Manual
        };
        RecoveryPolicies {
            execution: policy,
            teardown: policy,
        }
    }

    fn spec_schema(&self) -> schemars::Schema {
        schemars::schema_for!(CommandSpec)
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        parse(spec)?.validate().map_err(|error| error.to_string())
    }

    fn attestations(&self, spec: &Value) -> Vec<String> {
        parse(spec).map(|spec| own_attestations(&spec)).unwrap_or_default()
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, _spec: &Value) -> Vec<OwnershipScope> {
        Vec::new()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        let spec = parse_for_driver(spec)?;
        let paths = roots(&ctx.worktree, &ctx.project_dir)?;
        let files = file_digests(&paths, &spec.files)?;
        let fingerprint = match &spec.fingerprint {
            None => None,
            Some(argv) => {
                let scratch = ctx.cache_dir.join("command-fingerprint");
                let step_ctx = StepContext {
                    worktree: &ctx.worktree,
                    project_dir: &ctx.project_dir,
                    scratch: &scratch,
                    env: &ctx.env,
                    unit: None,
                    environment: ctx.environment.to_string(),
                    timeout: timeout(ctx.deadline, ctx.now),
                    tools: ctx.tools,
                    secrets: false,
                };
                let step = Step {
                    argv,
                    operation: DriverOperation::Fingerprint,
                    inputs: serde_json::json!({"values": values_of(&spec)}),
                    outputs: false,
                };
                let ran = run(&step_ctx, &spec, &step)?;
                match ran.exit {
                    ToolExit::Exited { code: 0, stdout } => Some(String::from_utf8_lossy(&stdout).into_owned()),
                    ToolExit::Exited { code, .. } => {
                        return Err(DriverError::before_effects(format!(
                            "fingerprint exited with code {code}; it must exit 0"
                        )))
                    }
                    ToolExit::NotStarted { reason } => {
                        return Err(DriverError::before_effects(format!(
                            "fingerprint could not start: {reason}"
                        )))
                    }
                    ToolExit::Signaled { signal } => {
                        return Err(DriverError::before_effects(format!(
                            "fingerprint died from signal {signal}"
                        )))
                    }
                    ToolExit::DeadlineExceeded => {
                        return Err(DriverError::before_effects(
                            "fingerprint passed the resolution deadline and was terminated",
                        ))
                    }
                    ToolExit::Cancelled => {
                        return Err(DriverError::before_effects("fingerprint was cancelled"));
                    }
                }
            }
        };
        Ok(KeyInputs {
            files,
            excluded_fields: Vec::new(),
            fingerprint,
        })
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let spec = resolved(unit)?;
        if is_deleting(unit) {
            let report = match &spec.teardown {
                Some(argv) => PlanReport {
                    changes: Change::Destroy,
                    summary: format!("run {}", argv[0]),
                    change_digest: None,
                    blocked_by: Vec::new(),
                },
                None => return Ok(Supported::Unsupported),
            };
            return Ok(Supported::Supported(report));
        }
        let current = ctx
            .observed
            .and_then(|observed| observed.receipt.as_ref())
            .is_some_and(|receipt| Some(&receipt.execution_key) == unit.execution_key.as_ref());
        Ok(Supported::Supported(if current {
            PlanReport {
                changes: Change::None,
                summary: "no changes".to_owned(),
                change_digest: None,
                blocked_by: Vec::new(),
            }
        } else {
            PlanReport {
                changes: Change::Create,
                summary: format!("run {}", spec.command[0]),
                change_digest: None,
                blocked_by: Vec::new(),
            }
        }))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let spec = resolved(unit)?;
        let step = Step {
            argv: &spec.command,
            operation: DriverOperation::Reconcile,
            inputs: serde_json::json!({"values": values_of(&spec)}),
            outputs: true,
        };
        let ran = run(&execution_context(ctx, true), &spec, &step)?;
        let outcome = match &ran.exit {
            ToolExit::Exited { code: 0, .. } => {
                let path = ran.prepared.outputs.as_deref().expect("reconcile has an outputs file");
                match read_outputs(&spec, path) {
                    Ok(outputs) => Outcome::Succeeded {
                        outputs,
                        artifacts: Vec::new(),
                        attestations: read_attestations(&spec, ran.prepared.attestations.as_deref()),
                    },
                    Err(message) => Outcome::Failed {
                        category: FailureCategory::OutputRejected,
                        retryable: spec.idempotent,
                        message,
                    },
                }
            }
            exit => unsuccessful(&spec, "command", exit),
        };
        Ok(Supported::Supported(outcome))
    }

    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        let spec = resolved(unit)?;
        let Some(argv) = &spec.verify else {
            return Ok(Supported::Unsupported);
        };
        let step = Step {
            argv,
            operation: DriverOperation::Verify,
            inputs: serde_json::json!({"values": values_of(&spec)}),
            outputs: false,
        };
        let ran = run(&execution_context(ctx, true), &spec, &step)?;
        let drift = match &ran.exit {
            ToolExit::Exited { code: 0, .. } => Drift::Clean,
            ToolExit::Exited { code: 2, .. } => Drift::Drifted {
                summary: "verify reported drift (exit code 2)".to_owned(),
            },
            ToolExit::Exited { code, .. } => Drift::Unknown {
                reason: format!("verify failed with exit code {code}"),
            },
            ToolExit::NotStarted { reason } => Drift::Unknown {
                reason: format!("verify could not start: {reason}"),
            },
            ToolExit::Signaled { signal } => Drift::Unknown {
                reason: format!("verify died from signal {signal}"),
            },
            ToolExit::DeadlineExceeded => Drift::Unknown {
                reason: "verify passed the unit's timeout and was terminated".to_owned(),
            },
            ToolExit::Cancelled => Drift::Unknown {
                reason: "verify was cancelled".to_owned(),
            },
        };
        Ok(Supported::Supported(Verification {
            drift,
            observations: Vec::new(),
            attestations: read_attestations(&spec, ran.prepared.attestations.as_deref()),
        }))
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    /// Teardown is part 5; a Command supports it only with a `teardown` step.
    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let spec = resolved(unit)?;
        let Some(argv) = &spec.teardown else {
            return Ok(Supported::Unsupported);
        };
        let outputs = ctx
            .observed
            .and_then(|observed| observed.receipt.as_ref())
            .map_or_else(|| Value::Object(Map::new()), |receipt| receipt.outputs.clone());
        let step = Step {
            argv,
            operation: DriverOperation::Teardown,
            inputs: serde_json::json!({"values": values_of(&spec), "outputs": outputs}),
            outputs: false,
        };
        let ran = run(&execution_context(ctx, true), &spec, &step)?;
        let outcome = match &ran.exit {
            ToolExit::Exited { code: 0, .. } => Outcome::Succeeded {
                outputs: Value::Object(Map::new()),
                artifacts: Vec::new(),
                attestations: Vec::new(),
            },
            exit => unsuccessful(&spec, "teardown", exit),
        };
        Ok(Supported::Supported(outcome))
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _push: bool,
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        Ok(Supported::Unsupported)
    }

    fn exec(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        Ok(Supported::Unsupported)
    }
}

/// Digests of the files the `files` globs match, keyed worktree-rooted, such
/// as `/scripts/seed/run.sh`. Directories behind symbolic links are never
/// entered, and a pattern matching a symbolic link is an error.
fn file_digests(paths: &ProjectPaths, patterns: &[String]) -> DriverResult<BTreeMap<String, String>> {
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let project_key = worktree_key(&paths.worktree_root, &paths.project_root)?;
    let mut digests = BTreeMap::new();
    for (index, pattern) in patterns.iter().enumerate() {
        let field = format!("spec.files[{index}]");
        nyl_core::local_path::validate_local_path(&field, pattern)
            .map_err(|error| DriverError::before_effects(error.to_string()))?;
        let rooted = rooted_pattern(&project_key, pattern)
            .ok_or_else(|| DriverError::before_effects(format!("{field} {pattern:?} leaves the worktree")))?;
        let matcher = glob::Pattern::new(&rooted)
            .map_err(|error| DriverError::before_effects(format!("invalid {field} glob {pattern:?}: {error}")))?;
        // Walk only beneath the pattern's literal prefix.
        let literal = rooted
            .trim_start_matches('/')
            .split('/')
            .take_while(|segment| !segment.contains(['*', '?', '[']))
            .collect::<Vec<_>>();
        let literal = &literal[..literal.len().min(rooted.trim_start_matches('/').split('/').count() - 1)];
        let mut start = paths.worktree_root.clone();
        for segment in literal {
            start.push(segment);
            match std::fs::symlink_metadata(&start) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(DriverError::before_effects(format!(
                        "{field} {pattern:?} traverses symbolic link {}",
                        start.display()
                    )))
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if !start.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&start).follow_links(false) {
            let entry = entry.map_err(|error| DriverError::before_effects(error.to_string()))?;
            let file_type = entry.file_type();
            if file_type.is_dir() {
                continue;
            }
            let key = worktree_key(&paths.worktree_root, entry.path())?;
            if key.starts_with("/.git/") || !matcher.matches_with(&key, options) {
                continue;
            }
            if file_type.is_symlink() {
                return Err(DriverError::before_effects(format!(
                    "{field} {pattern:?} matches symbolic link {key}"
                )));
            }
            let bytes = std::fs::read(entry.path()).map_err(|error| io_error("read", entry.path(), &error))?;
            digests.insert(key, nyl_core::digest::sha256_hex(&bytes));
        }
    }
    Ok(digests)
}

/// `/` plus the worktree-relative path of `path`, with `/` separators.
fn worktree_key(worktree: &Path, path: &Path) -> DriverResult<String> {
    let relative = path
        .strip_prefix(worktree)
        .map_err(|_| DriverError::before_effects(format!("{} is outside the worktree", path.display())))?;
    let parts = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    Ok(format!("/{}", parts.join("/")))
}

/// A local-path glob as a worktree-rooted glob; `None` when its leading `..`
/// segments climb above the worktree root.
fn rooted_pattern(project_key: &str, pattern: &str) -> Option<String> {
    if pattern.starts_with('/') {
        return Some(pattern.to_owned());
    }
    let mut base = project_key
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let mut rest = pattern;
    while let Some(after) = rest.strip_prefix("../") {
        base.pop()?;
        rest = after;
    }
    base.push(rest);
    Some(format!("/{}", base.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rooted_pattern_resolves_leading_parents_against_the_project() {
        assert_eq!(rooted_pattern("/nyl", "../scripts/**").as_deref(), Some("/scripts/**"));
        assert_eq!(rooted_pattern("/nyl", "seed/*.sh").as_deref(), Some("/nyl/seed/*.sh"));
        assert_eq!(rooted_pattern("/", "a/*").as_deref(), Some("/a/*"));
        assert_eq!(rooted_pattern("/nyl", "/x/**").as_deref(), Some("/x/**"));
        assert_eq!(rooted_pattern("/", "../x"), None);
    }

    #[test]
    fn test_capabilities_and_recovery_follow_the_spec() {
        let driver = CommandDriver;
        let plain = serde_json::json!({"command": ["./run.sh"]});
        let full = serde_json::json!({
            "command": ["./run.sh"], "verify": ["./verify.sh"], "teardown": ["./rm.sh"], "idempotent": true
        });
        let capabilities = driver.capabilities(&plain);
        assert!(capabilities.plan && capabilities.reconcile && !capabilities.verify && !capabilities.teardown);
        let capabilities = driver.capabilities(&full);
        assert!(capabilities.verify && capabilities.teardown);
        assert_eq!(driver.recovery(&plain).execution, RecoveryPolicy::Manual);
        assert_eq!(driver.recovery(&full).execution, RecoveryPolicy::Converge);
        assert!(driver.validate_spec(&plain).is_ok());
        assert!(driver.validate_spec(&serde_json::json!({"command": []})).is_err());
    }
}
