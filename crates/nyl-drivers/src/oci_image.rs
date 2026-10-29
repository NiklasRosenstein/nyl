//! Docker Buildx image lifecycle.
//!
//! Contract: `design/infrastructure-units.md#ociimage`. Directory membership
//! is shared between execution-key hashing and staging, so ignored files or
//! disposable Git worktree metadata never become unrecorded build inputs.
use crate::dockerignore::ContextFiles;
use base64::Engine;
use nyl_core::constants::{API_VERSION_ARTIFACTS, API_VERSION_UNITS};
use nyl_core::orchestration::driver::{
    Capabilities, Change, Driver, DriverError, DriverResult, ExecutionContext, KeyInputs, Outcome, OwnershipScope,
    PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported, ToolExit, ToolInvocation, Verification,
};
use nyl_core::orchestration::ids::{EnvironmentName, Uid};
use nyl_core::orchestration::state::{
    ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, Drift, FailureCategory, Receipt,
};
use nyl_core::resources::oci_image::{ImageContext, OciImageSpec};
use nyl_render::util::project_path::ProjectPaths;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Builds and publishes immutable images with `docker buildx`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OciImageDriver;

/// Arguments owned by the image driver; the generic build transport is opaque.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageBuildArgs {
    pub push: bool,
    pub tags: Vec<String>,
}

impl ImageBuildArgs {
    pub(crate) fn parse(args: &[OsString]) -> DriverResult<Self> {
        let mut options = Self::default();
        let mut load = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = arg
                .to_str()
                .ok_or_else(|| DriverError::before_effects("image driver build arguments must be UTF-8"))?;
            let tag = match arg {
                "--push" => {
                    options.push = true;
                    None
                }
                "--load" => {
                    load = true;
                    None
                }
                "--tag" | "-t" => Some(
                    args.next()
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| DriverError::before_effects("image driver --tag requires a UTF-8 value"))?,
                ),
                value if value.starts_with("--tag=") => Some(&value[6..]),
                _ => {
                    return Err(DriverError::before_effects(format!(
                        "unknown image driver build argument {arg:?}; supported flags: --push, --load, --tag <tag>"
                    )))
                }
            };
            if let Some(tag) = tag {
                if tag.is_empty() || tag.starts_with('-') {
                    return Err(DriverError::before_effects(
                        "image driver --tag requires a nonempty tag value",
                    ));
                }
                options.tags.push(tag.to_owned());
            }
        }
        if options.push && load {
            return Err(DriverError::before_effects(
                "image driver --push and --load are mutually exclusive",
            ));
        }
        Ok(options)
    }
}

fn parse(spec: &Value) -> DriverResult<OciImageSpec> {
    serde_json::from_value(spec.clone())
        .map_err(|error| DriverError::before_effects(format!("invalid OciImage spec: {error}")))
}
fn resolved(unit: &DesiredUnit) -> DriverResult<&Value> {
    crate::command::resolved_spec(unit)
}
fn escaped(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}
fn directories(spec: &OciImageSpec) -> Vec<(String, &nyl_core::resources::GitSource)> {
    let mut contexts = vec![("/context".into(), &spec.context)];
    contexts.extend(spec.contexts.iter().filter_map(|(name, context)| match context {
        ImageContext::Directory(source) => Some((format!("/contexts/{}", escaped(name)), source)),
        ImageContext::Image(_) => None,
    }));
    contexts
}
fn path(root: &Path, project: &Path, path: &str) -> DriverResult<PathBuf> {
    let paths = ProjectPaths::new(project.to_owned(), root.to_owned());
    if path == "/" {
        return Ok(root.to_owned());
    }
    paths
        .resolve("image context", path)
        .map_err(|error| DriverError::before_effects(error.to_string()))
}
fn scalar(value: &Value) -> DriverResult<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(_) | Value::Number(_) => Ok(value.to_string()),
        _ => Err(DriverError::before_effects(
            "image build arguments must resolve to strings, numbers, or booleans",
        )),
    }
}
fn sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
}
fn masked(ctx: &ExecutionContext<'_>) -> Vec<String> {
    ctx.env
        .iter()
        .filter(|(name, _)| name.as_str() != "PATH" && name.as_str() != "DOCKER_CONFIG")
        .map(|(_, value)| value.clone())
        .chain(ctx.secrets.values().cloned())
        .filter(|value| !value.is_empty())
        .collect()
}
#[allow(clippy::needless_pass_by_value)] // Used directly as an I/O map_err adapter.
fn fail_io(error: std::io::Error) -> DriverError {
    DriverError::before_effects(error.to_string())
}

/// Copy the runner's builder/context state without importing undeclared auths.
fn copy_tree(source: &Path, destination: &Path) -> DriverResult<()> {
    if !source.exists() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| DriverError::before_effects(error.to_string()))?;
        let relative = entry.path().strip_prefix(source).expect("configuration member");
        if entry.file_type().is_symlink() {
            return Err(DriverError::before_effects(
                "Docker builder configuration cannot contain symbolic links",
            ));
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target).map_err(fail_io)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(fail_io)?;
            }
            std::fs::copy(entry.path(), target).map_err(fail_io)?;
        }
    }
    Ok(())
}

struct DockerEnv {
    values: BTreeMap<String, String>,
    masks: Vec<String>,
    temporary: Option<PathBuf>,
}
impl Drop for DockerEnv {
    fn drop(&mut self) {
        if let Some(path) = &self.temporary {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
fn docker_env(ctx: &ExecutionContext<'_>, spec: &OciImageSpec) -> DriverResult<DockerEnv> {
    let mut values = ctx.env.clone();
    let mut masks = masked(ctx);
    let home = ctx.temp_dir.join("home");
    std::fs::create_dir_all(&home).map_err(fail_io)?;
    values.insert("HOME".into(), home.display().to_string());
    if !spec.registry_auth.is_empty() {
        let directory = ctx.temp_dir.join("docker-config");
        std::fs::create_dir_all(&directory).map_err(fail_io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).map_err(fail_io)?;
        }
        let mut config = json!({"auths": {}});
        if let Some(original) = ctx.env.get("DOCKER_CONFIG") {
            let original = Path::new(original);
            for name in ["buildx", "contexts"] {
                copy_tree(&original.join(name), &directory.join(name))?;
            }
            match std::fs::read(original.join("config.json")) {
                Ok(bytes) => {
                    let original: Value = serde_json::from_slice(&bytes)
                        .map_err(|_| DriverError::before_effects("runner Docker configuration is invalid JSON"))?;
                    if let Some(current) = original.get("currentContext").filter(|value| value.is_string()) {
                        config["currentContext"] = current.clone();
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(fail_io(error)),
            }
        }
        for (registry, auth) in &spec.registry_auth {
            let username = ctx.secrets.get(&auth.username_secret).ok_or_else(|| {
                DriverError::before_effects(format!(
                    "registry username secret {:?} was not admitted",
                    auth.username_secret
                ))
            })?;
            let password = ctx.secrets.get(&auth.password_secret).ok_or_else(|| {
                DriverError::before_effects(format!(
                    "registry password secret {:?} was not admitted",
                    auth.password_secret
                ))
            })?;
            let credential = format!("{username}:{password}");
            let encoded = base64::engine::general_purpose::STANDARD.encode(&credential);
            masks.extend([credential, encoded.clone()]);
            config["auths"][registry] = json!({"auth": encoded});
        }
        let target = directory.join("config.json");
        std::fs::write(&target, serde_json::to_vec(&config).expect("configuration serializes")).map_err(fail_io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).map_err(fail_io)?;
        }
        values.insert("DOCKER_CONFIG".into(), directory.display().to_string());
    }
    Ok(DockerEnv {
        values,
        masks,
        temporary: (!spec.registry_auth.is_empty()).then(|| ctx.temp_dir.join("docker-config")),
    })
}
fn tool(
    ctx: &ExecutionContext<'_>,
    env: &DockerEnv,
    args: Vec<String>,
    private_stdout: bool,
) -> DriverResult<ToolExit> {
    ctx.tools.run(ToolInvocation {
        program: "docker".into(),
        args: args.into_iter().map(OsString::from).collect(),
        working_dir: ctx.project_dir.clone(),
        env: env.values.clone(),
        stdin: None,
        timeout: Some((ctx.deadline - ctx.now).to_std().unwrap_or_default()),
        masked: env.masks.clone(),
        private_stdout,
        forward_stdout: false,
        interactive: false,
    })
}
fn completed(exit: ToolExit, effects: bool) -> Result<Vec<u8>, Outcome> {
    match exit {
        ToolExit::Exited { code: 0, stdout, .. } => Ok(stdout),
        ToolExit::Exited { code, .. } => Err(Outcome::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
            message: format!("docker buildx exited {code}"),
        }),
        ToolExit::NotStarted { reason } => Err(Outcome::Failed {
            category: FailureCategory::MissingTool,
            retryable: false,
            message: reason,
        }),
        _ if effects => Err(Outcome::Uncertain {
            reason: "docker buildx was interrupted; the image may already have been pushed".into(),
        }),
        _ => Err(Outcome::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
            message: "docker buildx inspection was interrupted".into(),
        }),
    }
}
fn artifact(
    unit: &DesiredUnit,
    spec: &OciImageSpec,
    digest: &str,
    tags: &[String],
    platforms: &[String],
) -> DriverResult<ArtifactDocument> {
    Ok(ArtifactDocument {
        api_version: API_VERSION_ARTIFACTS.into(),
        kind: "ContainerImage".into(),
        metadata: ArtifactMetadata {
            name: "image".into(),
            unit: ArtifactUnit {
                environment: unit.unit.environment.clone(),
                name: unit.unit.name.clone(),
                uid: unit
                    .unit
                    .uid
                    .clone()
                    .unwrap_or(Uid::new("00000000-0000-7000-8000-000000000000").expect("UUID")),
            },
        },
        execution_key: unit
            .execution_key
            .clone()
            .ok_or_else(|| DriverError::before_effects("image build requires an execution key"))?,
        spec: json!({"repository":spec.repository, "digest":digest, "reference":format!("{}@{digest}",spec.repository),"platforms":platforms,"tags":tags}),
    })
}

impl OciImageDriver {
    #[allow(clippy::too_many_lines)] // One build preserves credential/input staging, push, and metadata admission order.
    fn build_image(
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        push: bool,
        recorded: bool,
    ) -> DriverResult<Result<ArtifactDocument, Outcome>> {
        let spec = parse(resolved(unit)?)?;
        spec.validate()
            .map_err(|error| DriverError::before_effects(error.to_string()))?;
        let env = docker_env(ctx, &spec)?;
        let metadata = ctx.temp_dir.join("image-metadata.json");
        let _ = std::fs::remove_file(&metadata);
        let mut args = vec![
            "buildx".into(),
            "build".into(),
            if push { "--push".into() } else { "--load".into() },
            "--metadata-file".into(),
            metadata.display().to_string(),
        ];
        if let Some(builder) = &spec.builder {
            args.extend(["--builder".into(), builder.clone()]);
        }
        if !spec.platforms.is_empty() {
            args.extend(["--platform".into(), spec.platforms.join(",")]);
        }
        if let Some(target) = &spec.target {
            args.extend(["--target".into(), target.clone()]);
        }
        if let Some(cache) = &spec.cache {
            for value in &cache.from {
                args.extend(["--cache-from".into(), value.clone()]);
            }
            for value in &cache.to {
                args.extend(["--cache-to".into(), value.clone()]);
            }
        }
        for (name, value) in &spec.build_args {
            args.extend(["--build-arg".into(), format!("{name}={}", scalar(value)?)]);
        }
        let mut tags = spec.tags.clone();
        if recorded {
            let key = unit
                .execution_key
                .as_ref()
                .ok_or_else(|| DriverError::before_effects("image execution requires a key"))?;
            tags.insert(0, format!("nyl-{}-{}", unit.unit.environment, &key.as_str()[7..23]));
        }
        if push && tags.is_empty() {
            return Err(DriverError::before_effects(
                "a direct push requires spec.tags or --tag; execution tags belong only to reconcile",
            ));
        }
        for tag in &tags {
            args.extend(["--tag".into(), format!("{}:{tag}", spec.repository)]);
        }
        let mut main = None;
        for (index, (pointer, source)) in directories(&spec).into_iter().enumerate() {
            let roots = ctx.source_roots.get(&pointer);
            let root = roots.map_or(ctx.worktree.as_path(), |root| root.worktree.as_path());
            let project = roots.map_or(ctx.project_dir.as_path(), |root| root.project_dir.as_path());
            let source = path(root, project, source.path())?;
            let staged = ctx.temp_dir.join(format!("build-context-{index}"));
            ContextFiles::read(&source, (pointer == "/context").then_some(spec.dockerfile.as_str()))?.stage(&staged)?;
            if pointer == "/context" {
                main = Some(staged);
            } else {
                let name = pointer
                    .trim_start_matches("/contexts/")
                    .replace("~1", "/")
                    .replace("~0", "~");
                args.extend(["--build-context".into(), format!("{name}={}", staged.display())]);
            }
        }
        for (name, context) in &spec.contexts {
            if let ImageContext::Image(reference) = context {
                let reference = reference
                    .image
                    .as_str()
                    .ok_or_else(|| DriverError::before_effects("named image context has an unresolved reference"))?;
                args.extend(["--build-context".into(), format!("{name}=docker-image://{reference}")]);
            }
        }
        let main = main.expect("main context");
        args.extend([
            "--file".into(),
            main.join(&spec.dockerfile).display().to_string(),
            main.display().to_string(),
        ]);
        let exit = tool(ctx, &env, args, false)?;
        if let Err(outcome) = completed(exit, true) {
            return Ok(Err(outcome));
        }
        let file = std::fs::symlink_metadata(&metadata)
            .map_err(|_| DriverError::after_effects("build succeeded but its metadata file is missing"))?;
        if !file.is_file() || file.file_type().is_symlink() || file.len() > 1024 * 1024 {
            return Err(DriverError::after_effects(
                "build succeeded but its metadata is not a regular file smaller than 1 MiB",
            ));
        }
        let bytes = std::fs::read(&metadata).map_err(|_| {
            DriverError::after_effects(
                "build succeeded but its metadata file is missing; the image may already be pushed",
            )
        })?;
        let metadata: Value = serde_json::from_slice(&bytes)
            .map_err(|_| DriverError::after_effects("build succeeded but its metadata file is invalid JSON"))?;
        let digest = metadata
            .get("containerimage.digest")
            .and_then(Value::as_str)
            .filter(|digest| sha256(digest))
            .ok_or_else(|| {
                DriverError::after_effects("build succeeded but its metadata contains no valid image digest")
            })?;
        let mut platforms = spec.platforms.clone();
        if platforms.is_empty() {
            let args = if push {
                vec![
                    "buildx".into(),
                    "imagetools".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{json .Image}}".into(),
                    format!("{}@{digest}", spec.repository),
                ]
            } else {
                let config = metadata
                    .get("containerimage.config.digest")
                    .and_then(Value::as_str)
                    .filter(|digest| sha256(digest))
                    .ok_or_else(|| {
                        DriverError::after_effects(
                            "loaded image metadata has no configuration digest for platform inspection",
                        )
                    })?;
                vec![
                    "image".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{json .}}".into(),
                    config.into(),
                ]
            };
            let exit = tool(ctx, &env, args, true)?;
            let bytes = completed(exit, false)
                .map_err(|_| DriverError::after_effects("image was built but its platform could not be inspected"))?;
            let image: Value = serde_json::from_slice(&bytes)
                .map_err(|_| DriverError::after_effects("image platform inspection returned invalid JSON"))?;
            let os = image.get("os").or_else(|| image.get("Os")).and_then(Value::as_str);
            let architecture = image
                .get("architecture")
                .or_else(|| image.get("Architecture"))
                .and_then(Value::as_str);
            let (Some(os), Some(architecture)) = (os, architecture) else {
                return Err(DriverError::after_effects(
                    "image platform inspection returned no OS or architecture",
                ));
            };
            let mut platform = format!("{os}/{architecture}");
            if let Some(variant) = image
                .get("variant")
                .or_else(|| image.get("Variant"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                platform.push('/');
                platform.push_str(variant);
            }
            platforms.push(platform);
        }
        Ok(Ok(artifact(unit, &spec, digest, &tags, &platforms)?))
    }
}
impl Driver for OciImageDriver {
    fn api_version(&self) -> &'static str {
        API_VERSION_UNITS
    }
    fn kind(&self) -> &'static str {
        "OciImage"
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
            build: true,
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
        schemars::schema_for!(OciImageSpec)
    }
    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        parse(spec)
            .and_then(|spec| {
                spec.validate()
                    .map_err(|error| DriverError::before_effects(error.to_string()))
            })
            .map_err(|error| error.to_string())
    }
    fn secret_refs(&self, spec: &Value) -> Result<Vec<String>, String> {
        Ok(parse(spec)
            .map_err(|error| error.to_string())?
            .registry_auth
            .into_values()
            .flat_map(|auth| [auth.username_secret, auth.password_secret])
            .collect())
    }
    fn credential_environment(&self, spec: &Value, process_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        if spec
            .get("registryAuth")
            .and_then(Value::as_object)
            .is_none_or(serde_json::Map::is_empty)
        {
            return BTreeMap::new();
        }
        process_env
            .get("DOCKER_CONFIG")
            .cloned()
            .or_else(|| {
                process_env
                    .get("HOME")
                    .or_else(|| process_env.get("USERPROFILE"))
                    .map(|home| Path::new(home).join(".docker").display().to_string())
            })
            .map(|path| ("DOCKER_CONFIG".to_owned(), path))
            .into_iter()
            .collect()
    }
    fn ownership_scopes(&self, environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        spec.get("repository")
            .and_then(Value::as_str)
            .map(|repository| OwnershipScope {
                namespace: "oci-tags".to_owned(),
                key: vec![repository.to_owned(), format!("nyl-{environment}-")],
                path: vec![],
                description: format!("OCI repository {repository} tags nyl-{environment}-*"),
            })
            .into_iter()
            .collect()
    }
    fn source_fields(&self, spec: &Value) -> Vec<String> {
        parse(spec).map_or_else(
            |_| vec!["/context".into()],
            |spec| directories(&spec).into_iter().map(|(pointer, _)| pointer).collect(),
        )
    }
    fn checkout_paths(&self, spec: &Value) -> Option<Vec<String>> {
        parse(spec).ok().map(|spec| {
            directories(&spec)
                .into_iter()
                .map(|(_, source)| source.path().to_owned())
                .collect()
        })
    }
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        let spec = parse(spec)?;
        let mut files = BTreeMap::new();
        for (pointer, source) in directories(&spec) {
            let roots = ctx.source_roots.get(&pointer);
            let root = roots.map_or(ctx.worktree.as_path(), |root| root.worktree.as_path());
            let project = roots.map_or(ctx.project_dir.as_path(), |root| root.project_dir.as_path());
            let source = path(root, project, source.path())?;
            for (path, digest) in
                ContextFiles::read(&source, (pointer == "/context").then_some(spec.dockerfile.as_str()))?.digests
            {
                files.insert(format!("{pointer}:{path}"), digest);
            }
        }
        Ok(KeyInputs {
            files,
            excluded_fields: vec!["/builder".into(), "/cache".into()],
            fingerprint: None,
        })
    }
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let current = ctx
            .observed
            .and_then(|observed| observed.receipt.as_ref())
            .is_some_and(|receipt| Some(&receipt.execution_key) == unit.execution_key.as_ref());
        let spec = parse(resolved(unit)?)?;
        let mutable = spec
            .contexts
            .iter()
            .filter_map(|(name, context)| match context {
                ImageContext::Image(image) if image.image.as_str().is_some_and(|image| !image.contains("@sha256:")) => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut summary = if current {
            "image is current".into()
        } else {
            "build and push a new image".into()
        };
        if !mutable.is_empty() {
            summary = format!(
                "{summary}; warning: named image contexts {} use mutable tags; pin digests",
                mutable.join(", ")
            );
        }
        Ok(Supported::Supported(PlanReport {
            changes: if current { Change::None } else { Change::Create },
            summary,
            change_digest: None,
            blocked_by: Vec::new(),
        }))
    }
    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let spec = parse(resolved(unit)?)?;
        let env = docker_env(ctx, &spec)?;
        let version = match completed(tool(ctx, &env, vec!["buildx".into(), "version".into()], true)?, false) {
            Ok(version) => version,
            Err(outcome) => return Ok(Supported::Supported(outcome)),
        };
        if version.len() > 4096 {
            return Err(DriverError::before_effects(
                "Docker Buildx version output exceeds 4 KiB",
            ));
        }
        Ok(Supported::Supported(match Self::build_image(ctx, unit, true, true)? {
            Ok(artifact) => Outcome::Succeeded {
                versions: BTreeMap::from([(
                    "docker-buildx".into(),
                    String::from_utf8_lossy(&version).trim().to_owned(),
                )]),
                outputs: json!({}),
                artifacts: vec![artifact],
                attestations: Vec::new(),
            },
            Err(outcome) => outcome,
        }))
    }
    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        let spec = parse(resolved(unit)?)?;
        let env = docker_env(ctx, &spec)?;
        let artifact = ctx
            .artifacts
            .get("image")
            .ok_or_else(|| DriverError::before_effects("the current image artifact is unavailable"))?;
        if !receipt
            .artifacts
            .iter()
            .any(|record| record.name == "image" && record.digest == artifact.digest())
        {
            return Err(DriverError::before_effects(
                "the image artifact does not match its receipt",
            ));
        }
        let reference = artifact
            .spec
            .get("reference")
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::before_effects("the image artifact has no digest reference"))?;
        let result = tool(
            ctx,
            &env,
            vec!["buildx".into(), "imagetools".into(), "inspect".into(), reference.into()],
            true,
        )?;
        let drift = match result {
            ToolExit::Exited { code: 0, .. } => Drift::Clean,
            ToolExit::Exited { stderr, .. }
                if String::from_utf8_lossy(&stderr)
                    .to_ascii_lowercase()
                    .contains("manifest unknown")
                    || String::from_utf8_lossy(&stderr).contains(&format!("{reference}: not found")) =>
            {
                Drift::Drifted {
                    summary: "the published image no longer exists".into(),
                }
            }
            _ => {
                return Err(DriverError::before_effects(
                    "cannot verify published image; check registry access",
                ))
            }
        };
        Ok(Supported::Supported(Verification {
            drift,
            observations: Vec::new(),
            attestations: Vec::new(),
        }))
    }
    fn build(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        let options = ImageBuildArgs::parse(args)?;
        let mut unit = unit.clone();
        if let Some(Value::Object(spec)) = unit.resolved_spec.as_mut() {
            let tags = spec
                .entry("tags")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| DriverError::before_effects("spec.tags must be an array"))?;
            tags.extend(options.tags.iter().cloned().map(Value::String));
        }
        match Self::build_image(ctx, &unit, options.push, false)? {
            Ok(artifact) => Ok(Supported::Supported(vec![artifact])),
            Err(outcome) => Err(DriverError::after_effects(format!(
                "image build did not complete: {outcome:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_credential_policy_is_declared_by_the_image_driver() {
        let spec = json!({"context":{"path":"/app"},"repository":"registry.invalid/team/web","registryAuth":{"registry.invalid":{"usernameSecret":"user","passwordSecret":"token"}}});
        assert_eq!(OciImageDriver.secret_refs(&spec).unwrap(), ["user", "token"]);
        let process = BTreeMap::from([
            ("HOME".to_owned(), "/home/operator".to_owned()),
            ("UNDECLARED".to_owned(), "private".to_owned()),
        ]);
        assert_eq!(
            OciImageDriver.credential_environment(&spec, &process),
            BTreeMap::from([(
                "DOCKER_CONFIG".to_owned(),
                Path::new("/home/operator").join(".docker").display().to_string()
            )])
        );
        let mut explicit = process.clone();
        explicit.insert("DOCKER_CONFIG".to_owned(), "/configured".to_owned());
        assert_eq!(
            OciImageDriver.credential_environment(&spec, &explicit)["DOCKER_CONFIG"],
            "/configured"
        );
        assert!(OciImageDriver.credential_environment(&json!({}), &explicit).is_empty());
    }
    use nyl_core::orchestration::driver::{DriverOperation, SourceRoot, ToolRunner};
    use nyl_core::orchestration::ids::{CommitId, ExecutionKey, UnitAddress, UnitName};
    use nyl_core::orchestration::state::{DesiredUnitKind, Lifecycle, Readiness, StateV1, UnitIdentity};
    use std::sync::Mutex;

    #[test]
    fn test_image_driver_owns_build_arguments_and_rejects_invalid_flags_before_tools() {
        let args = ["--push", "--tag", "first", "--tag=second"].map(OsString::from);
        assert_eq!(
            ImageBuildArgs::parse(&args).unwrap(),
            ImageBuildArgs {
                push: true,
                tags: vec!["first".into(), "second".into()],
            }
        );
        assert!(!ImageBuildArgs::parse(&["--load".into()]).unwrap().push);
        let root = fixture();
        let temp = tempfile::TempDir::new().unwrap();
        let docker = Docker::default();
        let ctx = context(root.path(), temp.path(), &docker);
        let desired = unit(json!({"context":{"path":"/app"},"repository":"registry.invalid/web"}));
        for args in [vec!["--push", "--load"], vec!["--tag"], vec!["--tag="], vec!["--other"]] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            let error = OciImageDriver.build(&ctx, &desired, &args).unwrap_err();
            assert!(error.to_string().contains("image driver"));
        }
        assert!(docker.calls.lock().unwrap().is_empty());
    }

    #[derive(Default)]
    struct Docker {
        calls: Mutex<Vec<ToolInvocation>>,
        configs: Mutex<Vec<Value>>,
        bad_metadata: bool,
        inspection: Mutex<Option<ToolExit>>,
    }
    fn success(stdout: Vec<u8>) -> ToolExit {
        ToolExit::Exited {
            code: 0,
            stdout,
            stderr: Vec::new(),
        }
    }
    impl ToolRunner for Docker {
        fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit> {
            let args = invocation
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if let Some(path) = invocation.env.get("DOCKER_CONFIG") {
                if let Ok(bytes) = std::fs::read(Path::new(path).join("config.json")) {
                    self.configs
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(&bytes).unwrap());
                }
            }
            if args.starts_with(&["buildx".into(), "build".into()]) {
                let metadata = args.windows(2).find(|pair| pair[0] == "--metadata-file").unwrap()[1].clone();
                let body = if self.bad_metadata {
                    "invalid".into()
                } else {
                    json!({"containerimage.digest":format!("sha256:{}","a".repeat(64))}).to_string()
                };
                std::fs::write(metadata, body).unwrap();
                let context = Path::new(args.last().unwrap());
                assert!(context.join("Dockerfile").is_file());
                assert!(!context.join("ignored").exists());
                assert!(!context.join(".git").exists());
            }
            self.calls.lock().unwrap().push(invocation);
            if args.get(1).is_some_and(|command| command == "imagetools") {
                if let Some(exit) = self.inspection.lock().unwrap().take() {
                    return Ok(exit);
                }
                return Ok(success(br#"{"os":"linux","architecture":"amd64"}"#.to_vec()));
            }
            Ok(success(if args.get(1).is_some_and(|command| command == "version") {
                b"buildx v0.30.0\n".to_vec()
            } else {
                Vec::new()
            }))
        }
    }
    fn unit(spec: Value) -> DesiredUnit {
        DesiredUnit {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment: EnvironmentName::new("dev").unwrap(),
                name: UnitName::new("web").unwrap(),
                uid: Some(Uid::new("00000000-0000-7000-8000-000000000001").unwrap()),
                api_version: API_VERSION_UNITS.into(),
                kind: "OciImage".into(),
            },
            lifecycle: Lifecycle::Active,
            source_commit: Some(CommitId::new("a".repeat(40)).unwrap()),
            source_revision: Some(CommitId::new("a".repeat(40)).unwrap()),
            spec: spec.clone(),
            resolved_spec: Some(spec),
            provenance: BTreeMap::new(),
            execution_key: Some(ExecutionKey::from_digest_of(b"build")),
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        }
    }
    fn context<'a>(root: &Path, temp: &Path, tools: &'a dyn ToolRunner) -> ExecutionContext<'a> {
        let now = "2026-10-28T12:00:00Z".parse().unwrap();
        ExecutionContext {
            unit: UnitAddress::new(EnvironmentName::new("dev").unwrap(), UnitName::new("web").unwrap()),
            operation: DriverOperation::Reconcile,
            worktree: root.into(),
            project_dir: root.into(),
            source_roots: BTreeMap::new(),
            temp_dir: temp.into(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
            secrets: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            interactive: false,
            approved_digest: None,
            deadline: now + chrono::Duration::minutes(5),
            now,
            observed: None,
            tools,
            commit_identity: None,
            cache_dir: temp.join("cache"),
            teardown_wait: None,
        }
    }
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("app")).unwrap();
        for (name, bytes) in [
            ("Dockerfile", "FROM scratch\nCOPY file /file\n"),
            ("file", "hello"),
            ("ignored", "not an input"),
            (".dockerignore", "ignored\n"),
            (".git", "disposable checkout administration"),
        ] {
            std::fs::write(root.path().join("app").join(name), bytes).unwrap();
        }
        root
    }
    #[test]
    fn test_reconcile_publishes_metadata_digest_and_only_admitted_credentials() {
        let root = fixture();
        let temp = tempfile::TempDir::new().unwrap();
        let runner = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(runner.path().join("buildx")).unwrap();
        std::fs::write(runner.path().join("buildx/current"), "builder").unwrap();
        std::fs::write(
            runner.path().join("config.json"),
            r#"{"currentContext":"remote","auths":{"unrelated":{"auth":"must-not-copy"}}}"#,
        )
        .unwrap();
        let docker = Docker::default();
        let mut ctx = context(root.path(), temp.path(), &docker);
        ctx.env
            .insert("DOCKER_CONFIG".into(), runner.path().display().to_string());
        ctx.secrets = BTreeMap::from([("user".into(), "alice".into()), ("token".into(), "top-secret".into())]);
        let desired = unit(
            json!({"context":{"path":"/app"},"repository":"registry.invalid/team/web","tags":["dev"],"platforms":["linux/amd64"],"registryAuth":{"registry.invalid":{"usernameSecret":"user","passwordSecret":"token"}}}),
        );
        let Supported::Supported(Outcome::Succeeded {
            artifacts, versions, ..
        }) = OciImageDriver.reconcile(&ctx, &desired).unwrap()
        else {
            panic!("image did not build")
        };
        assert_eq!(
            artifacts[0].spec["reference"],
            format!("registry.invalid/team/web@sha256:{}", "a".repeat(64))
        );
        assert_eq!(versions["docker-buildx"], "buildx v0.30.0");
        let calls = docker.calls.lock().unwrap();
        let build = calls
            .iter()
            .find(|call| call.args.get(1).is_some_and(|arg| arg == "build"))
            .unwrap();
        assert!(build
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("registry.invalid/team/web:nyl-dev-")));
        assert!(build.masked.contains(&"top-secret".into()));
        assert!(!build.env.values().any(|value| value == "top-secret"));
        let configs = docker.configs.lock().unwrap();
        assert_eq!(configs[0]["currentContext"], "remote");
        assert!(configs[0]["auths"].get("unrelated").is_none());
        assert!(!temp.path().join("docker-config").exists());
    }
    #[test]
    fn test_direct_build_pushes_only_requested_tags_and_missing_metadata_is_uncertain() {
        let root = fixture();
        let temp = tempfile::TempDir::new().unwrap();
        let docker = Docker::default();
        let ctx = context(root.path(), temp.path(), &docker);
        let desired =
            unit(json!({"context":{"path":"/app"},"repository":"registry.invalid/web","platforms":["linux/amd64"]}));
        let Supported::Supported(images) = OciImageDriver
            .build(&ctx, &desired, &["--push".into(), "--tag".into(), "try".into()])
            .unwrap()
        else {
            panic!("unsupported")
        };
        assert_eq!(images[0].spec["tags"], json!(["try"]));
        let calls = docker.calls.lock().unwrap();
        assert!(!calls[0].args.iter().any(|arg| arg.to_string_lossy().contains(":nyl-")));
        drop(calls);
        let bad = Docker {
            bad_metadata: true,
            ..Docker::default()
        };
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = context(root.path(), temp.path(), &bad);
        assert!(OciImageDriver.reconcile(&ctx, &desired).unwrap_err().effects_possible());
    }
    #[test]
    fn test_named_contexts_hash_their_own_source_roots() {
        let root = fixture();
        let other = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(other.path().join("shared")).unwrap();
        std::fs::write(other.path().join("shared/input"), "other revision").unwrap();
        let docker = Docker::default();
        let now = "2026-10-28T12:00:00Z".parse().unwrap();
        let ctx = ResolutionContext {
            environment: EnvironmentName::new("dev").unwrap(),
            worktree: root.path().into(),
            project_dir: root.path().into(),
            source_roots: BTreeMap::from([(
                "/contexts/shared".into(),
                SourceRoot {
                    worktree: other.path().into(),
                    project_dir: other.path().into(),
                },
            )]),
            env: BTreeMap::new(),
            deadline: now,
            now,
            tools: &docker,
            cache_dir: root.path().join("cache"),
        };
        let spec = json!({"context":{"path":"/app"},"contexts":{"shared":{"path":"/shared"},"base":{"image":"base@sha256:abc"}},"repository":"registry.invalid/web"});
        let before = OciImageDriver.execution_key_inputs(&ctx, &spec).unwrap();
        assert!(before.files.contains_key("/contexts/shared:input"));
        assert!(!before.files.contains_key("/context:ignored"));
        std::fs::write(other.path().join("shared/input"), "updated revision").unwrap();
        assert_ne!(
            before.files,
            OciImageDriver.execution_key_inputs(&ctx, &spec).unwrap().files
        );
    }
    #[test]
    fn test_verify_uses_recorded_digest_and_distinguishes_registry_errors_from_deletion() {
        let root = fixture();
        let temp = tempfile::TempDir::new().unwrap();
        let docker = Docker::default();
        let mut ctx = context(root.path(), temp.path(), &docker);
        let desired =
            unit(json!({"context":{"path":"/app"},"repository":"registry.invalid/web","platforms":["linux/amd64"]}));
        let Supported::Supported(Outcome::Succeeded { artifacts, .. }) =
            OciImageDriver.reconcile(&ctx, &desired).unwrap()
        else {
            panic!("build failed")
        };
        let image = artifacts[0].clone();
        ctx.artifacts.insert("image".into(), image.clone());
        let receipt = Receipt {
            execution_key: desired.execution_key.clone().unwrap(),
            behavior_version: 1,
            run: nyl_core::orchestration::ids::RunId::new("00000000-0000-7000-8000-000000000002").unwrap(),
            read_desired: None,
            started_at: ctx.now,
            finished_at: ctx.now,
            versions: BTreeMap::new(),
            approval: None,
            provenance: BTreeMap::new(),
            outputs: json!({}),
            artifacts: vec![nyl_core::orchestration::state::ArtifactDigest {
                name: "image".into(),
                kind: "ContainerImage".into(),
                digest: image.digest(),
            }],
        };
        *docker.inspection.lock().unwrap() = Some(ToolExit::Exited {
            code: 1,
            stdout: Vec::new(),
            stderr: b"manifest unknown".to_vec(),
        });
        assert!(matches!(
            OciImageDriver.verify(&ctx, &desired, &receipt).unwrap(),
            Supported::Supported(Verification {
                drift: Drift::Drifted { .. },
                ..
            })
        ));
        let calls = docker.calls.lock().unwrap();
        let reference = calls.last().unwrap().args.last().unwrap().to_string_lossy();
        assert_eq!(reference, image.spec["reference"].as_str().unwrap());
        drop(calls);
        *docker.inspection.lock().unwrap() = Some(ToolExit::Exited {
            code: 1,
            stdout: Vec::new(),
            stderr: b"registry host not found: DNS lookup failed".to_vec(),
        });
        assert!(OciImageDriver.verify(&ctx, &desired, &receipt).is_err());
    }
    #[test]
    fn test_missing_docker_is_a_nonretryable_missing_tool_failure() {
        struct Missing;
        impl ToolRunner for Missing {
            fn run(&self, _invocation: ToolInvocation) -> DriverResult<ToolExit> {
                Ok(ToolExit::NotStarted {
                    reason: "docker not installed".into(),
                })
            }
        }
        let root = fixture();
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = context(root.path(), temp.path(), &Missing);
        let desired = unit(json!({"context":{"path":"/app"},"repository":"registry.invalid/web"}));
        assert!(matches!(
            OciImageDriver.reconcile(&ctx, &desired).unwrap(),
            Supported::Supported(Outcome::Failed {
                category: FailureCategory::MissingTool,
                retryable: false,
                ..
            })
        ));
    }
}
