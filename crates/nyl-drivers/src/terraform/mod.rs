//! One native implementation for Terraform and OpenTofu.
//!
//! Contract: [Infrastructure units](../../../../design/infrastructure-units.md#terraform-and-opentofu).
//! Native state and locks remain authoritative. Plans and output JSON are private;
//! only declared non-sensitive outputs and a masked change set leave this adapter.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use nyl_core::constants::API_VERSION_UNITS;
use nyl_core::orchestration::driver::{
    Capabilities, Driver, DriverError, DriverResult, ExecStatus, ExecTerminal, ExecutionContext, KeyInputs,
    LocalPathField, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext,
    Supported, ToolExit, ToolInvocation, Verification,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{
    DesiredUnit, Drift, FailureCategory, Lifecycle, Receipt, RetryAcknowledgement, UnitIdentity,
};
use nyl_core::resources::terraform::TerraformSpec;
use nyl_render::util::project_path::ProjectPaths;
use serde_json::{json, Value};

pub mod plan;

/// The two kinds differ only in executable and kind name.
pub struct TerraformDriver {
    kind: &'static str,
    program: &'static str,
}

impl TerraformDriver {
    pub const fn terraform() -> Self {
        Self {
            kind: "Terraform",
            program: "terraform",
        }
    }
    pub const fn opentofu() -> Self {
        Self {
            kind: "OpenTofu",
            program: "tofu",
        }
    }
}

fn parse(spec: &Value) -> Result<TerraformSpec, String> {
    let parsed: TerraformSpec =
        serde_json::from_value(spec.clone()).map_err(|error| format!("invalid Terraform/OpenTofu spec: {error}"))?;
    parsed.validate().map_err(|error| error.to_string())?;
    Ok(parsed)
}

fn resolved(unit: &DesiredUnit) -> Result<TerraformSpec, Fault> {
    parse(unit.resolved_spec.as_ref().unwrap_or(&unit.spec)).map_err(Fault::configuration)
}

/// Canonical backend configuration is credential-free and includes the selected workspace.
fn normalized_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

// State files need not exist yet. Resolve existing ancestors before appending
// missing components, so aliases such as macOS /var -> /private/var cannot hide
// a destination inside an execution checkout. Resolve before each `..` as well.
fn backend_destination(path: &Path) -> Result<PathBuf, Fault> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                result.push(component.as_os_str());
                continue;
            }
            std::path::Component::ParentDir => {
                result.pop();
            }
            component @ std::path::Component::Normal(_) => result.push(component.as_os_str()),
        }
        match result.canonicalize() {
            Ok(canonical) => result = canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if result.symlink_metadata().is_ok_and(|metadata| metadata.is_symlink()) {
                    return Err(Fault::configuration("backend destination contains a dangling symlink"));
                }
            }
            Err(error) => {
                return Err(Fault::configuration(format!(
                    "cannot resolve backend destination: {error}"
                )))
            }
        }
    }
    Ok(result)
}

// Native metadata records absent settings using zero or empty values. Their
// explicit spelling must not create a second ownership scope.
fn meaningful_backend_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Canonical backend configuration, with filesystem aliases normalized and the
/// local backend's inactive workspace settings excluded from resource identity.
pub fn ownership(spec: &TerraformSpec) -> OwnershipScope {
    let workspace = spec.workspace.as_deref().unwrap_or("default");
    let local_field = if workspace == "default" {
        "path"
    } else {
        "workspace_dir"
    };
    let mut backend: BTreeMap<_, _> = spec
        .backend
        .iter()
        .filter(|(_, value)| meaningful_backend_value(value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if let Some(path) = backend
        .get(local_field)
        .and_then(Value::as_str)
        .filter(|path| Path::new(path).is_absolute())
    {
        backend = BTreeMap::from([(
            local_field.to_owned(),
            Value::String(normalized_path(Path::new(path)).to_string_lossy().into_owned()),
        )]);
    }
    let backend = String::from_utf8(nyl_core::digest::canonical_json_bytes(&backend).expect("backend serializes"))
        .expect("JSON is UTF-8");
    OwnershipScope {
        namespace: "terraform-state".to_owned(),
        key: vec![backend.clone(), workspace.to_owned()],
        path: vec![],
        description: format!("backend {backend} workspace {workspace}"),
    }
}

fn source_paths(worktree: &Path, project_dir: &Path) -> DriverResult<ProjectPaths> {
    crate::command::roots(worktree, project_dir)
}

fn local_path(paths: &ProjectPaths, field: &str, path: &str) -> Result<PathBuf, Fault> {
    if path == "/" || (path == "." && paths.project_root == paths.worktree_root) {
        return Ok(paths.worktree_root.clone());
    }
    paths
        .resolve(field, path)
        .map_err(|error| Fault::configuration(error.to_string()))
}

fn patterns(spec: &TerraformSpec) -> Vec<String> {
    if spec.files.is_empty() {
        vec![format!("{}/**", spec.source.path().trim_end_matches('/'))]
    } else {
        spec.files.clone()
    }
}

fn failure(outcome: Result<Outcome, Fault>) -> Supported<Outcome> {
    Supported::Supported(outcome.unwrap_or_else(Fault::outcome))
}

impl Driver for TerraformDriver {
    fn api_version(&self) -> &'static str {
        API_VERSION_UNITS
    }
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn behavior_version(&self) -> u32 {
        1
    }
    fn default_timeout(&self) -> nyl_core::orchestration::time::Duration {
        nyl_core::orchestration::time::Duration::from_mins(60)
    }
    fn capabilities(&self, _spec: &Value) -> Capabilities {
        Capabilities {
            plan: true,
            reconcile: true,
            verify: true,
            teardown: true,
            change_digest: true,
            exec: Some(ExecTerminal::Interactive),
            ..Capabilities::default()
        }
    }
    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        RecoveryPolicies {
            execution: RecoveryPolicy::Converge,
            teardown: RecoveryPolicy::Converge,
        }
    }
    fn spec_schema(&self) -> schemars::Schema {
        schemars::schema_for!(TerraformSpec)
    }
    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        parse(spec).map(|_| ())
    }
    fn ownership_scopes(&self, _environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        parse(spec).map(|spec| vec![ownership(&spec)]).unwrap_or_default()
    }
    fn source_fields(&self, _spec: &Value) -> Vec<String> {
        vec!["/source".to_owned()]
    }
    fn local_path_fields(&self, _spec: &Value) -> Vec<LocalPathField> {
        ["/files", "/varFiles"]
            .into_iter()
            .map(|pointer| LocalPathField {
                pointer: pointer.to_owned(),
                defaults_to_project_dir: false,
            })
            .collect()
    }
    fn checkout_paths(&self, spec: &Value) -> Option<Vec<String>> {
        let spec = parse(spec).ok()?;
        let mut paths = patterns(&spec);
        paths.push(spec.source.path().to_owned());
        paths.extend(spec.var_files);
        Some(paths)
    }
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        let spec = parse(spec).map_err(DriverError::before_effects)?;
        let root = ctx.source_roots.get("/source");
        let paths = source_paths(
            root.map_or(&ctx.worktree, |root| &root.worktree),
            root.map_or(&ctx.project_dir, |root| &root.project_dir),
        )?;
        let mut files = crate::command::file_digests(&paths, &patterns(&spec), &[".terraform"])?;
        for (index, path) in spec.var_files.iter().enumerate() {
            let path = local_path(&paths, &format!("spec.varFiles[{index}]"), path)
                .map_err(|fault| DriverError::before_effects(fault.message))?;
            let bytes = std::fs::read(&path).map_err(|error| {
                DriverError::before_effects(format!("cannot read varFile {}: {error}", path.display()))
            })?;
            let key = format!(
                "/{}",
                path.strip_prefix(&paths.worktree_root)
                    .expect("local path")
                    .to_string_lossy()
                    .replace('\\', "/")
            );
            files.insert(key, nyl_core::digest::sha256_hex(&bytes));
        }
        Ok(KeyInputs {
            files,
            ..KeyInputs::default()
        })
    }
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let result = (|| {
            let native = Native::new(self, ctx, resolved(unit)?, &unit.unit)?;
            native.initialize()?;
            native.require_state()?;
            let (_, report) = native.plan(matches!(unit.lifecycle, Lifecycle::Deleting { .. }), false)?;
            Ok(Supported::Supported(report))
        })();
        result.map_err(|fault: Fault| DriverError::before_effects(fault.message))
    }
    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(failure(self.execute(ctx, unit, false)))
    }
    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        let result = (|| {
            let native = Native::new(self, ctx, resolved(unit)?, &unit.unit)?;
            native.initialize()?;
            native.require_state()?;
            let (changed, report) = native.plan(false, false)?;
            Ok(Supported::Supported(Verification {
                drift: if changed {
                    Drift::Drifted {
                        summary: report.summary,
                    }
                } else {
                    Drift::Clean
                },
                observations: Vec::new(),
                attestations: Vec::new(),
            }))
        })();
        result.map_err(|fault: Fault| DriverError::before_effects(fault.message))
    }
    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(failure(self.execute(ctx, unit, true)))
    }
    fn exec(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        let native = Native::new(
            self,
            ctx,
            resolved(unit).map_err(|fault| DriverError::before_effects(fault.message))?,
            &unit.unit,
        )
        .map_err(|fault| DriverError::before_effects(fault.message))?;
        native
            .initialize()
            .map_err(|fault| DriverError::before_effects(fault.message))?;
        // A repair may restore missing state; it deliberately bypasses only that guard.
        let result = match native.run(args.to_vec(), false) {
            Ok(ToolExit::Exited { code, .. }) => return Ok(Supported::Supported(ExecStatus::Exited { code })),
            Ok(ToolExit::Signaled { signal }) => return Ok(Supported::Supported(ExecStatus::Signaled { signal })),
            Ok(exit) => native.unsuccessful("operator command", exit, true),
            Err(fault) => fault,
        };
        Err(if result.uncertain {
            DriverError::after_effects(result.message)
        } else {
            DriverError::before_effects(result.message)
        })
    }
}

impl TerraformDriver {
    fn execute(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit, destroy: bool) -> Result<Outcome, Fault> {
        let native = Native::new(self, ctx, resolved(unit)?, &unit.unit)?;
        let version = native.initialize()?;
        native.require_state()?;
        let (changed, report) = native.plan(destroy, true)?;
        let reviewed_required = native.spec.common.approval.require_digest
            || ctx
                .observed
                .is_some_and(|observed| observed.operator_modified.is_some());
        if (reviewed_required && ctx.approved_digest.is_none())
            || ctx
                .approved_digest
                .as_ref()
                .is_some_and(|approved| report.change_digest.as_ref() != Some(approved))
        {
            return Ok(Outcome::AwaitingApproval {
                digest: report.change_digest.expect("native digest"),
            });
        }
        if changed || !native.state_exists()? {
            native.checked(
                vec![
                    "apply".into(),
                    "-input=false".into(),
                    "-json".into(),
                    "-lock=true".into(),
                    native.plan_file().into_os_string(),
                ],
                true,
            )?;
        }
        let outputs = if destroy { json!({}) } else { native.outputs()? };
        Ok(Outcome::Succeeded {
            versions: BTreeMap::from([(self.program.to_owned(), version)]),
            outputs,
            artifacts: Vec::new(),
            attestations: Vec::new(),
        })
    }
}

struct Fault {
    category: FailureCategory,
    retryable: bool,
    message: String,
    uncertain: bool,
    retry_context: Option<Value>,
}

impl Fault {
    fn configuration(message: impl Into<String>) -> Self {
        Self {
            category: FailureCategory::ToolError,
            retryable: false,
            message: message.into(),
            uncertain: false,
            retry_context: None,
        }
    }
    fn outcome(self) -> Outcome {
        if self.uncertain {
            Outcome::Uncertain { reason: self.message }
        } else if let Some(context) = self.retry_context {
            Outcome::RetryRequired {
                category: self.category,
                message: self.message,
                context,
            }
        } else {
            Outcome::Failed {
                category: self.category,
                retryable: self.retryable,
                message: self.message,
            }
        }
    }
}

struct Native<'a, 'c> {
    driver: &'a TerraformDriver,
    ctx: &'a ExecutionContext<'c>,
    spec: TerraformSpec,
    identity: &'a UnitIdentity,
    paths: ProjectPaths,
    module: PathBuf,
    directory: PathBuf,
    env: BTreeMap<String, String>,
    secrets: Vec<String>,
}

fn known_secrets<'a>(values: impl Iterator<Item = &'a String>) -> Vec<String> {
    fn leaves(value: &Value, secrets: &mut Vec<String>) {
        match value {
            Value::Object(fields) => fields.values().for_each(|value| leaves(value, secrets)),
            Value::Array(values) => values.iter().for_each(|value| leaves(value, secrets)),
            Value::String(value) => secrets.push(value.clone()),
            Value::Bool(_) | Value::Number(_) => secrets.push(value.to_string()),
            Value::Null => {}
        }
    }
    let mut secrets = Vec::new();
    for value in values {
        secrets.push(value.clone());
        if let Ok(parsed) = serde_json::from_str::<Value>(value) {
            leaves(&parsed, &mut secrets);
        }
    }
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    secrets.dedup();
    secrets
}

impl<'a, 'c> Native<'a, 'c> {
    fn new(
        driver: &'a TerraformDriver,
        ctx: &'a ExecutionContext<'c>,
        spec: TerraformSpec,
        identity: &'a UnitIdentity,
    ) -> Result<Self, Fault> {
        let root = ctx.source_roots.get("/source");
        let paths = source_paths(
            root.map_or(&ctx.worktree, |root| &root.worktree),
            root.map_or(&ctx.project_dir, |root| &root.project_dir),
        )
        .map_err(|error| Fault::configuration(error.to_string()))?;
        let module = local_path(&paths, "spec.source.path", spec.source.path())?;
        if !module.is_dir() {
            return Err(Fault::configuration(
                "spec.source.path is not a configuration directory",
            ));
        }
        let directory = ctx.temp_dir.join("native-infrastructure");
        private_directory(&directory)?;
        private_directory(&directory.join("data"))?;
        private_directory(&directory.join("home"))?;
        let mut env = ctx.env.clone();
        env.insert(
            "TF_DATA_DIR".to_owned(),
            directory.join("data").to_string_lossy().into_owned(),
        );
        env.insert("TF_IN_AUTOMATION".to_owned(), "true".to_owned());
        env.insert("TF_INPUT".to_owned(), "false".to_owned());
        env.insert(
            "TF_WORKSPACE".to_owned(),
            spec.workspace.clone().unwrap_or_else(|| "default".to_owned()),
        );
        env.insert("CHECKPOINT_DISABLE".to_owned(), "1".to_owned());
        env.insert("HOME".to_owned(), directory.join("home").to_string_lossy().into_owned());
        let secrets = known_secrets(spec.common.env.secrets.keys().filter_map(|name| env.get(name)));
        Ok(Self {
            driver,
            ctx,
            spec,
            identity,
            paths,
            module,
            directory,
            env,
            secrets,
        })
    }
    fn plan_file(&self) -> PathBuf {
        self.directory.join("reviewed.plan")
    }
    fn run(&self, args: Vec<OsString>, private_stdout: bool) -> Result<ToolExit, Fault> {
        self.ctx
            .tools
            .run(ToolInvocation {
                program: self.driver.program.into(),
                args,
                working_dir: self.module.clone(),
                env: self.env.clone(),
                stdin: None,
                timeout: Some((self.ctx.deadline - self.ctx.now).to_std().unwrap_or_default()),
                masked: self.secrets.clone(),
                interactive: self.ctx.interactive
                    && self.ctx.operation == nyl_core::orchestration::driver::DriverOperation::Exec
                    && !private_stdout,
                forward_stdout: self.ctx.operation == nyl_core::orchestration::driver::DriverOperation::Exec
                    && !private_stdout,
                private_stdout,
            })
            .map_err(|error| Fault {
                category: FailureCategory::ToolError,
                retryable: true,
                message: error.to_string(),
                uncertain: error.effects_possible(),
                retry_context: None,
            })
    }
    fn checked(&self, args: Vec<OsString>, effects: bool) -> Result<Vec<u8>, Fault> {
        let step = args
            .first()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self.run(args, true)? {
            ToolExit::Exited { code: 0, stdout, .. } => Ok(stdout),
            exit => Err(self.unsuccessful(&step, exit, effects)),
        }
    }
    fn json(&self, args: &[&str]) -> Result<Value, Fault> {
        let bytes = self.checked(args.iter().map(OsString::from).collect(), false)?;
        serde_json::from_slice(&bytes).map_err(|_| Fault::configuration("native tool returned invalid private JSON"))
    }
    fn initialize(&self) -> Result<String, Fault> {
        let version = self.json(&["version", "-json"])?;
        let version = version["terraform_version"]
            .as_str()
            .ok_or_else(|| Fault::configuration("native version JSON has no terraform_version"))?
            .to_owned();
        if self.spec.version.as_ref().is_some_and(|expected| expected != &version) {
            return Err(Fault {
                category: FailureCategory::MissingTool,
                retryable: false,
                message: format!(
                    "{} version {version} does not match required {}",
                    self.driver.program,
                    self.spec.version.as_deref().unwrap()
                ),
                uncertain: false,
                retry_context: None,
            });
        }
        let lock = self.module.join(".terraform.lock.hcl");
        if !lock.is_file() {
            return Err(Fault::configuration("a committed .terraform.lock.hcl is required; initialize the configuration locally and commit its lock file"));
        }
        let backend_path = self.directory.join("backend.hcl");
        let mut backend = String::new();
        for (name, value) in &self.spec.backend {
            let literal = value.to_string().replace("${", "$${").replace("%{", "%%{");
            writeln!(&mut backend, "{name} = {literal}").expect("writing to a string succeeds");
        }
        private_file(&backend_path, backend.as_bytes())?;
        self.checked(
            vec![
                "init".into(),
                "-input=false".into(),
                "-no-color".into(),
                "-lockfile=readonly".into(),
                format!("-backend-config={}", backend_path.display()).into(),
            ],
            false,
        )?;
        self.check_backend()?;
        self.check_modules()?;
        Ok(version)
    }
    fn check_backend(&self) -> Result<(), Fault> {
        let path = self.directory.join("data/terraform.tfstate");
        let bytes = std::fs::read(path).map_err(|_| {
            Fault::configuration("init must record a configured backend; declare a backend block in the root module")
        })?;
        let metadata: Value =
            serde_json::from_slice(&bytes).map_err(|_| Fault::configuration("invalid native backend metadata"))?;
        let kind = metadata["backend"]["type"]
            .as_str()
            .ok_or_else(|| Fault::configuration("init recorded no backend configuration"))?;
        if kind != "local" {
            let config = metadata["backend"]["config"]
                .as_object()
                .ok_or_else(|| Fault::configuration("init recorded invalid backend configuration"))?;
            for (name, value) in config.iter().filter(|(_, value)| meaningful_backend_value(value)) {
                if self.spec.backend.get(name) != Some(value) {
                    return Err(Fault::configuration(format!(
                        "declare effective backend.{name} on the unit so native ownership is explicit; supply credentials through the environment"
                    )));
                }
            }
        }
        if kind == "local" {
            let field = if self.spec.workspace.as_deref().unwrap_or("default") == "default" {
                "path"
            } else {
                "workspace_dir"
            };
            let state = metadata["backend"]["config"][field].as_str().unwrap_or_default();
            let declared = self.spec.backend.get(field).and_then(Value::as_str).ok_or_else(|| {
                Fault::configuration(format!(
                    "declare backend.{field} on the unit so native ownership is explicit"
                ))
            })?;
            let state = normalized_path(Path::new(state));
            if !state.is_absolute() || normalized_path(Path::new(declared)) != state {
                return Err(Fault::configuration(
                    "a local backend must declare its effective absolute persistent path on the unit",
                ));
            }
            let destination = backend_destination(Path::new(
                metadata["backend"]["config"][field].as_str().unwrap_or_default(),
            ))?;
            if destination.starts_with(backend_destination(&self.paths.worktree_root)?)
                || destination.starts_with(backend_destination(&self.ctx.temp_dir)?)
            {
                return Err(Fault::configuration(
                    "local backend state cannot live in a disposable execution directory",
                ));
            }
        }
        Ok(())
    }

    fn retry_context(&self) -> Value {
        json!({"version":1,"unit":self.identity,"scope":ownership(&self.spec).key})
    }

    fn state_exists(&self) -> Result<bool, Fault> {
        let bytes = self.checked(vec!["state".into(), "pull".into()], false)?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }
        let state: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Fault::configuration("native tool returned invalid private state JSON"))?;
        if !state.is_object() {
            return Err(Fault::configuration("native tool returned invalid private state JSON"));
        }
        Ok(true)
    }

    fn require_state(&self) -> Result<(), Fault> {
        let Some(observed) = self
            .ctx
            .observed
            .filter(|observed| &observed.unit == self.identity && observed.receipt.is_some())
        else {
            return Ok(());
        };
        let context = self.retry_context();
        if matches!(&observed.retry, Some(RetryAcknowledgement::Acknowledged { context: acknowledged, request }) if acknowledged == &context && request.reason.as_ref().is_some_and(|reason| !reason.trim().is_empty()))
        {
            return Ok(());
        }
        if !self.state_exists()? {
            return Err(Fault { category:FailureCategory::Ownership, retryable:false, message:"NYL-TOFU-STATE-MISSING: this unit has a receipt but its backend has no state; restore state or accept the empty ownership scope with recover --retry --reason".to_owned(), uncertain:false, retry_context:Some(context) });
        }
        Ok(())
    }
    fn check_modules(&self) -> Result<(), Fault> {
        let manifest = self.directory.join("data/modules/modules.json");
        if !manifest.exists() {
            return Ok(());
        }
        let bytes = std::fs::read(&manifest)
            .map_err(|error| Fault::configuration(format!("cannot read native module manifest: {error}")))?;
        let manifest: Value =
            serde_json::from_slice(&bytes).map_err(|_| Fault::configuration("invalid native module manifest"))?;
        let included = crate::command::file_digests(&self.paths, &patterns(&self.spec), &[".terraform"])
            .map_err(|error| Fault::configuration(error.to_string()))?;
        for module in manifest["Modules"].as_array().into_iter().flatten() {
            let source = module["Source"].as_str().unwrap_or_default();
            if !(source.starts_with("./") || source.starts_with("../")) {
                continue;
            }
            let directory = module["Dir"]
                .as_str()
                .ok_or_else(|| Fault::configuration("local module manifest has no directory"))?;
            let directory = ProjectPaths::new(self.module.clone(), self.paths.worktree_root.clone())
                .resolve("local module manifest directory", directory)
                .map_err(|error| Fault::configuration(error.to_string()))?;
            let directory = directory
                .canonicalize()
                .map_err(|error| Fault::configuration(format!("cannot resolve local module: {error}")))?;
            if !directory.starts_with(&self.paths.worktree_root) {
                return Err(Fault::configuration("local module leaves its source worktree"));
            }
            for entry in walkdir::WalkDir::new(&directory).follow_links(false) {
                let entry = entry.map_err(|error| Fault::configuration(error.to_string()))?;
                if entry.file_type().is_symlink() {
                    return Err(Fault::configuration("a local module traverses a symbolic link"));
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let key = format!(
                    "/{}",
                    entry
                        .path()
                        .strip_prefix(&self.paths.worktree_root)
                        .expect("checked module")
                        .to_string_lossy()
                        .replace('\\', "/")
                );
                if !included.contains_key(&key) {
                    return Err(Fault::configuration(format!(
                        "local module file {key} is outside spec.files; add {}/**",
                        directory.strip_prefix(&self.paths.worktree_root).unwrap().display()
                    )));
                }
            }
        }
        Ok(())
    }
    fn plan(&self, destroy: bool, locking: bool) -> Result<(bool, PlanReport), Fault> {
        let variables = self.directory.join("variables.tfvars.json");
        private_file(
            &variables,
            &serde_json::to_vec(&self.spec.variables).map_err(|error| Fault::configuration(error.to_string()))?,
        )?;
        let mut args: Vec<OsString> = vec![
            "plan".into(),
            "-input=false".into(),
            "-json".into(),
            "-detailed-exitcode".into(),
            format!("-lock={locking}").into(),
            format!("-out={}", self.plan_file().display()).into(),
        ];
        if destroy {
            args.push("-destroy".into());
        }
        for (index, path) in self.spec.var_files.iter().enumerate() {
            args.push(
                format!(
                    "-var-file={}",
                    local_path(&self.paths, &format!("spec.varFiles[{index}]"), path)?.display()
                )
                .into(),
            );
        }
        args.push(format!("-var-file={}", variables.display()).into());
        let changed = match self.run(args, true)? {
            ToolExit::Exited { code: 0, .. } => false,
            ToolExit::Exited { code: 2, .. } => true,
            exit => return Err(self.unsuccessful("plan", exit, false)),
        };
        let json = self.json(&[
            "show",
            "-json",
            self.plan_file()
                .to_str()
                .ok_or_else(|| Fault::configuration("plan path is not UTF-8"))?,
        ])?;
        plan::check_outputs(&json, &self.spec.common.outputs).map_err(|message| Fault {
            category: FailureCategory::OutputRejected,
            retryable: false,
            message,
            uncertain: false,
            retry_context: None,
        })?;
        let report = plan::report(&json, changed, destroy, &self.secrets).map_err(Fault::configuration)?;
        Ok((changed, report))
    }
    fn outputs(&self) -> Result<Value, Fault> {
        let outputs = self.json(&["output", "-json"])?;
        let mut recorded = serde_json::Map::new();
        for (name, declaration) in &self.spec.common.outputs {
            let output = outputs
                .get(name)
                .ok_or_else(|| rejected(format!("declared output {name:?} is missing")))?;
            if output["sensitive"] == true && !declaration.sensitive {
                return Err(rejected(format!(
                    "output {name:?} is sensitive but its declaration is not"
                )));
            }
            let value = output
                .get("value")
                .ok_or_else(|| rejected(format!("declared output {name:?} has no value")))?;
            if !declaration.output_type.accepts(value) {
                return Err(rejected(format!("output {name:?} does not match its declared type")));
            }
            if !declaration.sensitive {
                recorded.insert(name.clone(), value.clone());
            }
        }
        Ok(Value::Object(recorded))
    }
    fn unsuccessful(&self, step: &str, exit: ToolExit, effects: bool) -> Fault {
        let (category, retryable, mut message, uncertain) = match exit {
            ToolExit::Exited { code, stdout, stderr } => {
                let lock = lock_diagnostic(&stdout, &stderr);
                (
                    if lock.is_some() {
                        FailureCategory::Ownership
                    } else {
                        FailureCategory::ToolError
                    },
                    lock.is_none(),
                    lock.map_or_else(
                        || format!("{} {step} exited {code}", self.driver.program),
                        |detail| format!("{step}: {detail}"),
                    ),
                    false,
                )
            }
            ToolExit::NotStarted { .. } => (
                FailureCategory::MissingTool,
                false,
                format!("{} is missing or cannot be started", self.driver.program),
                false,
            ),
            ToolExit::DeadlineExceeded => (
                FailureCategory::Timeout,
                true,
                format!("{} {step} exceeded its deadline", self.driver.program),
                effects,
            ),
            ToolExit::Cancelled => (
                FailureCategory::ToolError,
                true,
                format!("{} {step} was cancelled", self.driver.program),
                effects,
            ),
            ToolExit::Signaled { signal } => (
                FailureCategory::ToolError,
                true,
                format!("{} {step} died from signal {signal}", self.driver.program),
                effects,
            ),
        };
        for secret in &self.secrets {
            if !secret.is_empty() {
                message = message.replace(secret, "***");
            }
        }
        Fault {
            category,
            retryable,
            message,
            uncertain,
            retry_context: None,
        }
    }
}

/// Only native lock diagnostics may enter a failure message; private JSON values never do.
fn lock_diagnostic(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    for line in String::from_utf8_lossy(stdout).lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let diagnostic = &value["diagnostic"];
            if diagnostic["summary"]
                .as_str()
                .is_some_and(|summary| summary.to_ascii_lowercase().contains("state lock"))
            {
                return Some(format!(
                    "{}: {}",
                    diagnostic["summary"].as_str().unwrap_or_default(),
                    diagnostic["detail"].as_str().unwrap_or_default()
                ));
            }
        }
    }
    let error = String::from_utf8_lossy(stderr);
    (error.contains("Error acquiring the state lock")
        || error.contains("Error releasing the state lock")
        || error.contains("Lock Info:"))
    .then(|| error.into_owned())
}

fn rejected(message: String) -> Fault {
    Fault {
        category: FailureCategory::OutputRejected,
        retryable: false,
        message,
        uncertain: false,
        retry_context: None,
    }
}

fn private_directory(path: &Path) -> Result<(), Fault> {
    std::fs::create_dir_all(path)
        .map_err(|error| Fault::configuration(format!("cannot create native temporary directory: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| Fault::configuration(error.to_string()))?;
    }
    Ok(())
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<(), Fault> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|error| Fault::configuration(format!("cannot create native temporary file: {error}")))?;
    file.write_all(bytes)
        .map_err(|error| Fault::configuration(format!("cannot write native temporary file: {error}")))
}
