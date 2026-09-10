mod report;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use git2::Repository;

use crate::git::GitManager;
use crate::gitops::{
    compile_target_tree_cached_with_observer_and_options, discover_gitops_inventory, resolve_deployment_target_name,
    GitOpsCache, RenderIndex, TreeCacheArgs, TreeRenderOptions,
};
use crate::{NylError, Result};

use report::{DiffMode, Report, ReportFormat, ReportOutput, TreeDiff};

use super::super::tree_progress::{TreeProgressArgs, TreeProgressReporter};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DiffTreeBase {
    /// Compare with the currently published revision.
    Published,
    /// Render and compare with the source repository at --source-ref.
    Source,
}

/// Diff a target without modifying its publication tree.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI switches compose without hidden state.
pub struct DiffTreeArgs {
    #[command(flatten)]
    pub validation: crate::validation::ValidationArgs,
    #[command(flatten)]
    pub cache: TreeCacheArgs,

    #[command(flatten)]
    pub progress: TreeProgressArgs,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// DeploymentTarget to diff. Defaults to the sole configured target.
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long, value_enum, default_value = "published")]
    pub against: DiffTreeBase,
    /// Source revision used by --against source.
    #[arg(long, requires = "against")]
    pub source_ref: Option<String>,
    /// Source repository URL. Defaults to the current repository's origin.
    #[arg(long)]
    pub source_repository: Option<String>,
    /// Write the unified diff to a file instead of stdout.
    #[arg(short, long, default_value = "-")]
    pub output: PathBuf,
    /// Compare original bytes, including YAML formatting and comments.
    #[arg(long)]
    pub raw: bool,
    /// Include per-file line counts in text and Markdown reports.
    #[arg(long)]
    stats_files: bool,
    /// Export a complete report (repeatable). Formats: text, markdown, json. PATH=- selects stdout.
    #[arg(long, value_name = "FORMAT:PATH")]
    stats_output: Vec<ReportOutput>,
    /// Suppress the default stderr report; progress and errors remain on stderr.
    #[arg(long)]
    no_stats_stderr: bool,
    /// Compare only the generated Argo CD catalog.
    #[arg(long, conflicts_with_all = ["applications", "application"])]
    pub catalog: bool,
    /// Compare all generated workload Applications and their payloads.
    #[arg(long, conflicts_with = "catalog")]
    pub applications: bool,
    /// Compare only this generated Argo CD Application (repeatable).
    #[arg(long, value_name = "NAMESPACE/NAME", conflicts_with = "catalog")]
    pub application: Vec<String>,
    /// Return an error when differences exist.
    #[arg(long)]
    pub fail_on_diff: bool,

    /// Allow project secrets and NYL_* environment variables to affect rendered output.
    #[arg(long)]
    pub allow_secret_inputs: bool,
}

#[derive(Debug)]
pub(super) struct PublishedRenderedTree {
    pub(super) files: BTreeMap<PathBuf, Vec<u8>>,
    pub(super) index: Option<RenderIndex>,
}

struct PublishedBaseline {
    files: BTreeMap<PathBuf, Vec<u8>>,
    commit: git2::Oid,
}

struct SourceBaseline {
    compiled: crate::gitops::CompiledTargetTree,
    repository: String,
    revision: String,
    commit: git2::Oid,
}

enum ResolvedBaseline {
    Published(PublishedBaseline),
    Source(Box<SourceBaseline>),
}

impl ResolvedBaseline {
    fn files(&self) -> &BTreeMap<PathBuf, Vec<u8>> {
        match self {
            Self::Published(baseline) => &baseline.files,
            Self::Source(baseline) => &baseline.compiled.files,
        }
    }

    fn publication_path_prefix<'a>(&'a self, desired: &'a crate::gitops::CompiledTargetTree) -> &'a str {
        match self {
            Self::Published(_) => desired.target.publication_path_prefix(),
            Self::Source(baseline) => baseline.compiled.target.publication_path_prefix(),
        }
    }
}

enum DiffSelection {
    Tree,
    Catalog,
    Applications(BTreeSet<String>),
}

impl DiffSelection {
    fn from_args(args: &DiffTreeArgs) -> Self {
        if args.catalog {
            Self::Catalog
        } else if args.applications || !args.application.is_empty() {
            Self::Applications(args.application.iter().cloned().collect())
        } else {
            Self::Tree
        }
    }
}

#[derive(Debug)]
struct ApplicationView {
    catalog_file: PathBuf,
    payload_path: PathBuf,
    catalog_application: bool,
}

struct ComparisonFiles {
    base: BTreeMap<PathBuf, Vec<u8>>,
    desired: BTreeMap<PathBuf, Vec<u8>>,
}

struct ComparisonSummary<'a> {
    mode: DiffMode,
    target: &'a str,
    desired_source_repository: Option<&'a str>,
    desired_source_commit: Option<&'a str>,
    desired_dirty: bool,
    desired: &'a crate::gitops::CompiledTargetTree,
    baseline: &'a ResolvedBaseline,
    selection: &'a DiffSelection,
    output: &'a Path,
}

/// Compare rendered trees with the automatic report color policy.
pub async fn execute(args: DiffTreeArgs) -> Result<()> {
    execute_with_color(args, crate::cli::ColorChoice::Auto).await
}

pub(crate) async fn execute_with_color(args: DiffTreeArgs, color: crate::cli::ColorChoice) -> Result<()> {
    let protected = std::iter::once(args.output.clone())
        .chain(args.stats_output.iter().map(|o| o.path.clone()))
        .filter(|p| p != Path::new("-"))
        .collect::<Vec<_>>();
    args.validation.validate_outputs(false, &protected, &[])?;
    report::validate_outputs(&args.output, &args.stats_output)?;
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let target_name = resolve_deployment_target_name(&inventory, args.target.as_deref())?;
    let (desired_source_commit, desired_dirty) = super::render_tree::source_state(&inventory.project_root)?;
    let desired_source_repository = source_repository_url(&inventory.project_root)?;
    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let desired_phase = matches!(args.against, DiffTreeBase::Source).then(|| "Desired".to_string());
    let mut desired_progress = TreeProgressReporter::new(args.progress, desired_phase);
    let options = TreeRenderOptions {
        allow_secret_inputs: args.allow_secret_inputs,
    };
    let desired = compile_target_tree_cached_with_observer_and_options(
        &inventory,
        &target_name,
        &cache,
        &mut desired_progress,
        options,
    )
    .await?;
    crate::validation::validate_tree(&args.validation, &inventory, &desired).await?;
    let baseline = resolve_baseline(&args, &inventory.project_root, &target_name, &desired, &cache, options).await?;
    let selection = DiffSelection::from_args(&args);
    let comparison = comparison_files(&selection, &baseline, &desired)?;
    let mode = if args.raw { DiffMode::Raw } else { DiffMode::Normalized };
    let diff = TreeDiff::between(&comparison.base, &comparison.desired, mode)?;
    let report = Report::new(
        &ComparisonSummary {
            mode,
            target: &target_name,
            desired_source_repository: desired_source_repository.as_deref(),
            desired_source_commit: desired_source_commit.as_deref(),
            desired_dirty,
            desired: &desired,
            baseline: &baseline,
            selection: &selection,
            output: &args.output,
        },
        diff.stats,
        cache.stats(),
    );
    let exports = args
        .stats_output
        .iter()
        .map(|destination| {
            let ansi = match color {
                crate::cli::ColorChoice::Always => true,
                crate::cli::ColorChoice::Never => false,
                crate::cli::ColorChoice::Auto => destination.path == Path::new("-") && color.should_use_ansi(),
            };
            report.format(destination.format, args.stats_files, ansi)
        })
        .collect::<Result<Vec<_>>>()?;
    let terminal_report = if args.no_stats_stderr {
        None
    } else {
        Some(report.format(ReportFormat::Text, args.stats_files, color.should_use_ansi())?)
    };
    write_diff_output(&args.output, diff.patch.as_bytes())?;
    for (destination, contents) in args.stats_output.iter().zip(exports) {
        write_diff_output(&destination.path, contents.as_bytes())?;
    }
    if let Some(contents) = terminal_report {
        let mut stderr = io::stderr().lock();
        if (args.output == Path::new("-") && !diff.patch.is_empty())
            || args.stats_output.iter().any(|output| output.path == Path::new("-"))
        {
            stderr.write_all(b"\n")?;
        }
        stderr.write_all(contents.as_bytes())?;
        stderr.flush()?;
    }
    if args.fail_on_diff && report.diff.has_changes {
        Err(NylError::validation(format!(
            "deployment target {:?} has rendered differences",
            target_name
        )))
    } else {
        Ok(())
    }
}

async fn resolve_baseline(
    args: &DiffTreeArgs,
    project_root: &Path,
    target_name: &str,
    desired: &crate::gitops::CompiledTargetTree,
    cache: &GitOpsCache,
    options: TreeRenderOptions,
) -> Result<ResolvedBaseline> {
    match args.against {
        DiffTreeBase::Published => Ok(ResolvedBaseline::Published(published_tree(desired, cache)?)),
        DiffTreeBase::Source => {
            let source_ref = args
                .source_ref
                .as_deref()
                .ok_or_else(|| NylError::config("--source-ref is required with --against source"))?;
            let baseline = source_derived_tree(
                project_root,
                args.source_repository.as_deref(),
                source_ref,
                target_name,
                cache,
                args.progress,
                options,
            )
            .await?;
            Ok(ResolvedBaseline::Source(Box::new(baseline)))
        }
    }
}

fn comparison_files(
    selection: &DiffSelection,
    baseline: &ResolvedBaseline,
    desired: &crate::gitops::CompiledTargetTree,
) -> Result<ComparisonFiles> {
    match selection {
        DiffSelection::Tree => {
            let mut base = baseline.files().clone();
            let mut desired_files = desired.files.clone();
            if let ResolvedBaseline::Source(source) = baseline {
                let marker = PathBuf::from("_nyl/publication.json");
                base.insert(marker.clone(), publication_marker(&source.compiled)?);
                desired_files.insert(marker, publication_marker(desired)?);
            }
            Ok(ComparisonFiles {
                base,
                desired: desired_files,
            })
        }
        DiffSelection::Catalog => Ok(ComparisonFiles {
            base: files_beneath(baseline.files(), Path::new("_nyl/catalog")),
            desired: files_beneath(&desired.files, Path::new("_nyl/catalog")),
        }),
        DiffSelection::Applications(selectors) => application_comparison_files(
            selectors,
            baseline.files(),
            baseline.publication_path_prefix(desired),
            &desired.files,
            desired.target.publication_path_prefix(),
        ),
    }
}

fn application_comparison_files(
    selectors: &BTreeSet<String>,
    base: &BTreeMap<PathBuf, Vec<u8>>,
    base_path_prefix: &str,
    desired: &BTreeMap<PathBuf, Vec<u8>>,
    desired_path_prefix: &str,
) -> Result<ComparisonFiles> {
    let base_views = derive_application_views(base, base_path_prefix)?;
    let desired_views = derive_application_views(desired, desired_path_prefix)?;
    let available = base_views
        .keys()
        .chain(desired_views.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let selected = if selectors.is_empty() {
        available
            .iter()
            .filter(|identity| {
                !base_views.get(*identity).is_none_or(|view| view.catalog_application)
                    || !desired_views.get(*identity).is_none_or(|view| view.catalog_application)
            })
            .cloned()
            .collect()
    } else {
        for selector in selectors {
            validate_application_selector(selector)?;
            if !available.contains(selector) {
                let available = if available.is_empty() {
                    "<none>".to_owned()
                } else {
                    available.iter().cloned().collect::<Vec<_>>().join(", ")
                };
                return Err(NylError::config(format!(
                    "Generated Argo CD Application {selector:?} exists on neither side of the comparison; available Applications: {available}"
                )));
            }
        }
        selectors.clone()
    };
    Ok(ComparisonFiles {
        base: select_application_views(base, &base_views, &selected),
        desired: select_application_views(desired, &desired_views, &selected),
    })
}

fn validate_application_selector(selector: &str) -> Result<()> {
    let Some((namespace, name)) = selector.split_once('/') else {
        return Err(NylError::config(format!(
            "--application {selector:?} must use NAMESPACE/NAME"
        )));
    };
    if namespace.is_empty() || name.is_empty() || name.contains('/') {
        return Err(NylError::config(format!(
            "--application {selector:?} must use NAMESPACE/NAME"
        )));
    }
    Ok(())
}

fn derive_application_views(
    files: &BTreeMap<PathBuf, Vec<u8>>,
    publication_path_prefix: &str,
) -> Result<BTreeMap<String, ApplicationView>> {
    let catalog_root = Path::new("_nyl/catalog/applications");
    let mut views = BTreeMap::new();
    for (path, bytes) in files.iter().filter(|(path, _)| path.starts_with(catalog_root)) {
        let expected_identity = application_identity_from_catalog_path(path)?;
        let text = std::str::from_utf8(bytes).map_err(|error| {
            NylError::config(format!(
                "Generated Argo CD Application {} is not UTF-8: {error}",
                path.display()
            ))
        })?;
        let application = crate::yaml::parse_yaml_value_k8s_compatible(text).map_err(|error| {
            NylError::config(format!(
                "Failed to parse generated Argo CD Application {}: {error}",
                path.display()
            ))
        })?;
        if application.get("apiVersion").and_then(serde_json::Value::as_str) != Some("argoproj.io/v1alpha1")
            || application.get("kind").and_then(serde_json::Value::as_str) != Some("Application")
        {
            return Err(NylError::config(format!(
                "Generated catalog path {} does not contain an argoproj.io/v1alpha1 Application",
                path.display()
            )));
        }
        let namespace = required_application_string(&application, "/metadata/namespace", path)?;
        let name = required_application_string(&application, "/metadata/name", path)?;
        let identity = format!("{namespace}/{name}");
        if identity != expected_identity {
            return Err(NylError::config(format!(
                "Generated Argo CD Application {} has identity {identity:?}, expected {expected_identity:?} from its catalog path",
                path.display()
            )));
        }
        let rendered_path = required_application_string(&application, "/spec/source/path", path)?;
        crate::resources::validate_relative_path(
            "generated Application spec.source.path",
            rendered_path,
            false,
            false,
        )?;
        let payload_path = strip_publication_prefix(rendered_path, publication_path_prefix, path)?;
        let view = ApplicationView {
            catalog_file: path.clone(),
            catalog_application: payload_path == Path::new("_nyl/catalog"),
            payload_path,
        };
        if views.insert(identity.clone(), view).is_some() {
            return Err(NylError::config(format!(
                "Generated Argo CD Application identity {identity:?} occurs more than once"
            )));
        }
    }
    validate_application_payloads(&views)?;
    Ok(views)
}

fn application_identity_from_catalog_path(path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix("_nyl/catalog/applications")
        .expect("caller filters catalog Application paths");
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 2 {
        return Err(NylError::config(format!(
            "Generated Argo CD Application path {} must use _nyl/catalog/applications/<namespace>/<name>.yaml",
            path.display()
        )));
    }
    let namespace = components[0].as_os_str().to_str().ok_or_else(|| {
        NylError::config(format!(
            "Generated Argo CD Application path {} is not UTF-8",
            path.display()
        ))
    })?;
    let filename = components[1].as_os_str().to_str().ok_or_else(|| {
        NylError::config(format!(
            "Generated Argo CD Application path {} is not UTF-8",
            path.display()
        ))
    })?;
    let name = filename.strip_suffix(".yaml").ok_or_else(|| {
        NylError::config(format!(
            "Generated Argo CD Application path {} must end in .yaml",
            path.display()
        ))
    })?;
    Ok(format!("{namespace}/{name}"))
}

fn required_application_string<'a>(application: &'a serde_json::Value, pointer: &str, path: &Path) -> Result<&'a str> {
    application
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            NylError::config(format!(
                "Generated Argo CD Application {} field {} must be a string",
                path.display(),
                pointer.trim_start_matches('/')
            ))
        })
}

fn strip_publication_prefix(rendered_path: &str, path_prefix: &str, catalog_file: &Path) -> Result<PathBuf> {
    let rendered_path = Path::new(rendered_path);
    let payload = if path_prefix.is_empty() {
        rendered_path
    } else {
        rendered_path.strip_prefix(path_prefix).map_err(|_| {
            NylError::config(format!(
                "Generated Argo CD Application {} source path {} is outside publication path prefix {:?}",
                catalog_file.display(),
                rendered_path.display(),
                path_prefix
            ))
        })?
    };
    if payload.as_os_str().is_empty() {
        return Err(NylError::config(format!(
            "Generated Argo CD Application {} source path resolves to the publication root",
            catalog_file.display()
        )));
    }
    crate::resources::relative_path_to_posix("generated Application payload path", payload)?;
    Ok(payload.to_path_buf())
}

fn validate_application_payloads(views: &BTreeMap<String, ApplicationView>) -> Result<()> {
    let workloads = views
        .iter()
        .filter(|(_, view)| !view.catalog_application)
        .collect::<Vec<_>>();
    for (index, (left_identity, left)) in workloads.iter().enumerate() {
        for (right_identity, right) in workloads.iter().skip(index + 1) {
            if left.payload_path.starts_with(&right.payload_path) || right.payload_path.starts_with(&left.payload_path)
            {
                return Err(NylError::config(format!(
                    "Generated Argo CD Applications {left_identity:?} and {right_identity:?} have overlapping payload paths {} and {}",
                    left.payload_path.display(),
                    right.payload_path.display()
                )));
            }
        }
    }
    Ok(())
}

fn select_application_views(
    files: &BTreeMap<PathBuf, Vec<u8>>,
    views: &BTreeMap<String, ApplicationView>,
    selected: &BTreeSet<String>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut output = BTreeMap::new();
    for identity in selected {
        let Some(view) = views.get(identity) else {
            continue;
        };
        if let Some(bytes) = files.get(&view.catalog_file) {
            output.insert(view.catalog_file.clone(), bytes.clone());
        }
        output.extend(files_beneath(files, &view.payload_path));
    }
    output
}

fn files_beneath(files: &BTreeMap<PathBuf, Vec<u8>>, prefix: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    files
        .iter()
        .filter(|(path, _)| path.starts_with(prefix))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect()
}

/// Unix null-device destinations discard bytes without replacing the device or an alias.
fn is_null_output(output: &Path) -> bool {
    output != Path::new("-")
        && cfg!(unix)
        && (output == Path::new("/dev/null") || output.canonicalize().is_ok_and(|path| path == Path::new("/dev/null")))
}

fn write_diff_output(output: &Path, contents: &[u8]) -> Result<()> {
    if is_null_output(output) {
        return Ok(());
    }
    if output == Path::new("-") {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(contents)?;
        stdout.flush()?;
        return Ok(());
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    Ok(())
}

fn published_tree(compiled: &crate::gitops::CompiledTargetTree, cache: &GitOpsCache) -> Result<PublishedBaseline> {
    let mut manager = git_manager(cache)?;
    let checkout = manager
        .resolve_ref_fresh(
            &compiled.repository.repo_url,
            Some(&compiled.target.spec.publication.revision),
            None,
        )
        .map_err(NylError::Git)?;
    let commit = checkout_commit(&checkout)?;
    let root = checked_published_root(&checkout, compiled.target.publication_path_prefix())?;
    let published = read_rendered_tree(&root)?;
    if let Some(index) = published.index {
        let repository = compiled
            .repository_name
            .as_deref()
            .unwrap_or(&compiled.repository.repo_url);
        if index.target != compiled.target.metadata.name
            || index.cluster != compiled.cluster.metadata.name
            || index.publication.repository != repository
            || index.publication.revision != compiled.target.spec.publication.revision
            || index.publication.path_prefix != compiled.target.publication_path_prefix()
        {
            return Err(NylError::config(format!(
                "Published ownership index at {} belongs to a different target, cluster, or publication",
                root.display()
            )));
        }
    }
    Ok(PublishedBaseline {
        files: published.files,
        commit,
    })
}

pub(super) fn checked_published_root(checkout: &Path, path_prefix: &str) -> Result<PathBuf> {
    crate::resources::validate_relative_path("DeploymentTarget publication.pathPrefix", path_prefix, true, false)?;
    let canonical_checkout = checkout.canonicalize().map_err(|error| {
        NylError::config(format!(
            "Failed to resolve published checkout {}: {error}",
            checkout.display()
        ))
    })?;
    let mut selected = checkout.to_path_buf();
    for component in Path::new(path_prefix).components() {
        selected.push(component.as_os_str());
        match std::fs::symlink_metadata(&selected) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "Published rendered tree contains symbolic link {}",
                    selected.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if selected.exists() {
        let canonical_selected = selected.canonicalize()?;
        if !canonical_selected.starts_with(&canonical_checkout) {
            return Err(NylError::config(format!(
                "Published rendered root {} resolves outside checkout {}",
                selected.display(),
                checkout.display()
            )));
        }
    }
    Ok(selected)
}

async fn source_derived_tree(
    project_root: &Path,
    source_repository: Option<&str>,
    source_ref: &str,
    target: &str,
    cache: &GitOpsCache,
    progress_args: TreeProgressArgs,
    options: TreeRenderOptions,
) -> Result<SourceBaseline> {
    let repository_url = if let Some(url) = source_repository {
        url.to_string()
    } else {
        let repository = Repository::discover(project_root)
            .map_err(|error| NylError::config(format!("Failed to inspect source repository: {error}")))?;
        repository
            .find_remote("origin")
            .ok()
            .and_then(|remote| remote.url().ok().map(ToOwned::to_owned))
            .ok_or_else(|| NylError::config("Source repository has no origin; pass --source-repository"))?
    };
    let mut manager = git_manager(cache)?;
    let checkout = manager
        .resolve_ref_fresh(&repository_url, Some(source_ref), None)
        .map_err(NylError::Git)?;
    let commit = checkout_commit(&checkout)?;
    let inventory = discover_gitops_inventory(&checkout, None)?;
    let mut progress = TreeProgressReporter::new(progress_args, Some(format!("Baseline {source_ref}")));
    let compiled =
        compile_target_tree_cached_with_observer_and_options(&inventory, target, cache, &mut progress, options).await?;
    Ok(SourceBaseline {
        compiled,
        repository: repository_url,
        revision: source_ref.to_owned(),
        commit,
    })
}

fn checkout_commit(checkout: &Path) -> Result<git2::Oid> {
    let repository = Repository::open(checkout).map_err(crate::git::GitError::from)?;
    let commit = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(crate::git::GitError::from)?
        .id();
    Ok(commit)
}

fn source_repository_url(project_root: &Path) -> Result<Option<String>> {
    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source repository: {error}")))?;
    Ok(repository
        .find_remote("origin")
        .ok()
        .and_then(|remote| remote.url().ok().map(crate::util::sanitize_url)))
}

fn git_manager(cache: &GitOpsCache) -> Result<GitManager> {
    if let Some(cache_root) = cache.external_cache_root() {
        Ok(GitManager::with_cache_dir(cache_root).with_render_cache(Some(cache.clone())))
    } else {
        GitManager::new()
            .map(|manager| manager.with_render_cache(Some(cache.clone())))
            .map_err(NylError::Git)
    }
}

fn publication_marker(compiled: &crate::gitops::CompiledTargetTree) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "cluster": compiled.cluster.metadata.name,
        "repoURL": compiled.repository.repo_url,
        "publishURL": compiled.repository.publish_url,
        "revision": compiled.target.spec.publication.revision,
        "pathPrefix": compiled.target.publication_path_prefix(),
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) fn read_rendered_tree(root: &Path) -> Result<PublishedRenderedTree> {
    if !root.exists() {
        return Ok(PublishedRenderedTree {
            files: BTreeMap::new(),
            index: None,
        });
    }
    let index_path = root.join(crate::gitops::reconcile::DEFAULT_INDEX_PATH);
    if !index_path.is_file() {
        return Err(NylError::config(format!(
            "Published rendered tree {} has no ownership index",
            root.display()
        )));
    }
    reject_published_symlink(root, &index_path)?;
    let index: RenderIndex = serde_json::from_slice(&std::fs::read(&index_path)?)?;
    if index.version != crate::gitops::reconcile::RENDER_INDEX_VERSION {
        return Err(NylError::config(format!(
            "Published ownership index {} uses unsupported version {}",
            index_path.display(),
            index.version
        )));
    }
    let mut files = BTreeMap::new();
    for (relative, expected_hash) in &index.files {
        crate::resources::validate_relative_path("published owned path", relative, false, false)?;
        let path = root.join(relative);
        reject_published_symlink(root, &path)?;
        let bytes = std::fs::read(&path).map_err(|error| {
            NylError::config(format!(
                "Published owned file {} is missing or unreadable: {error}",
                path.display()
            ))
        })?;
        if crate::gitops::reconcile::sha256(&bytes) != *expected_hash {
            return Err(NylError::config(format!(
                "Published owned file {} does not match its ownership index",
                path.display()
            )));
        }
        files.insert(PathBuf::from(relative), bytes);
    }
    Ok(PublishedRenderedTree {
        files,
        index: Some(index),
    })
}

fn reject_published_symlink(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| NylError::config(format!("Published path escaped its root: {error}")))?;
    let mut current = root.to_path_buf();
    if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(NylError::config(format!(
            "Published rendered tree contains symbolic link {}",
            current.display()
        )));
    }
    for component in relative.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(NylError::config(format!(
                "Published rendered tree contains symbolic link {}",
                current.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Cluster, DeploymentTarget, InlineGitRepository};

    #[cfg(unix)]
    #[test]
    fn null_outputs_discard_bytes_and_preserve_symlink_aliases() {
        let temp = tempfile::TempDir::new().unwrap();
        let alias = temp.path().join("null");
        std::os::unix::fs::symlink("/dev/null", &alias).unwrap();
        report::validate_outputs(
            Path::new("/dev/null"),
            &[
                ReportOutput {
                    format: ReportFormat::Text,
                    path: "/dev/null".into(),
                },
                ReportOutput {
                    format: ReportFormat::Json,
                    path: alias.clone(),
                },
            ],
        )
        .unwrap();
        for destination in [Path::new("/dev/null"), alias.as_path()] {
            write_diff_output(destination, b"discarded output\n").unwrap();
            write_diff_output(destination, b"").unwrap();
        }
        assert_eq!(fs::read_link(&alias).unwrap(), Path::new("/dev/null"));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    fn application_yaml(namespace: &str, name: &str, path: &str) -> Vec<u8> {
        crate::yaml::serialize_yaml_document(&serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Application",
            "metadata": {"namespace": namespace, "name": name},
            "spec": {"source": {"path": path}}
        }))
        .unwrap()
        .into_bytes()
    }

    #[test]
    fn derives_application_views_from_generated_catalog() {
        let files = BTreeMap::from([
            (
                PathBuf::from("_nyl/catalog/applications/argocd/api.yaml"),
                application_yaml("argocd", "api", "production/workloads/api"),
            ),
            (
                PathBuf::from("_nyl/catalog/applications/argocd/production-catalog.yaml"),
                application_yaml("argocd", "production-catalog", "production/_nyl/catalog"),
            ),
            (
                PathBuf::from("_nyl/catalog/projects/workloads.yaml"),
                b"kind: AppProject\n".to_vec(),
            ),
            (
                PathBuf::from("workloads/api/resources.yaml"),
                b"kind: ConfigMap\n".to_vec(),
            ),
        ]);

        let views = derive_application_views(&files, "production").unwrap();
        assert_eq!(views["argocd/api"].payload_path, Path::new("workloads/api"));
        assert!(!views["argocd/api"].catalog_application);
        assert!(views["argocd/production-catalog"].catalog_application);

        let comparison =
            application_comparison_files(&BTreeSet::new(), &files, "production", &files, "production").unwrap();
        for selected in [&comparison.base, &comparison.desired] {
            assert!(selected.contains_key(Path::new("_nyl/catalog/applications/argocd/api.yaml")));
            assert!(selected.contains_key(Path::new("workloads/api/resources.yaml")));
            assert!(!selected.contains_key(Path::new("_nyl/catalog/projects/workloads.yaml")));
            assert!(!selected.contains_key(Path::new("_nyl/catalog/applications/argocd/production-catalog.yaml")));
        }

        let selectors = BTreeSet::from(["argocd/production-catalog".to_owned()]);
        let comparison = application_comparison_files(&selectors, &files, "production", &files, "production").unwrap();
        assert!(comparison
            .base
            .contains_key(Path::new("_nyl/catalog/projects/workloads.yaml")));
        assert!(comparison
            .base
            .contains_key(Path::new("_nyl/catalog/applications/argocd/production-catalog.yaml")));
    }

    #[test]
    fn rejects_ambiguous_or_escaping_application_payloads() {
        let overlapping = BTreeMap::from([
            (
                PathBuf::from("_nyl/catalog/applications/argocd/parent.yaml"),
                application_yaml("argocd", "parent", "production/workloads"),
            ),
            (
                PathBuf::from("_nyl/catalog/applications/argocd/child.yaml"),
                application_yaml("argocd", "child", "production/workloads/child"),
            ),
        ]);
        let error = derive_application_views(&overlapping, "production").unwrap_err();
        assert!(error.to_string().contains("overlapping payload paths"));

        let escaping = BTreeMap::from([(
            PathBuf::from("_nyl/catalog/applications/argocd/api.yaml"),
            application_yaml("argocd", "api", "another-target/workloads/api"),
        )]);
        let error = derive_application_views(&escaping, "production").unwrap_err();
        assert!(error.to_string().contains("outside publication path prefix"));
    }

    #[test]
    fn publication_marker_changes_when_ownership_coordinates_change() {
        let target: DeploymentTarget = serde_json::from_value(serde_json::json!({
            "apiVersion": crate::constants::API_VERSION_K8S_GITOPS,
            "kind": "DeploymentTarget",
            "metadata": {"name": "production"},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "publication": {
                    "repository": {"repoURL": "https://example.invalid/deploy.git"},
                    "revision": "deploy/production",
                    "pathPrefix": "production"
                }
            }
        }))
        .unwrap();
        let cluster: Cluster = serde_json::from_value(serde_json::json!({
            "apiVersion": crate::constants::API_VERSION_K8S_GITOPS,
            "kind": "Cluster",
            "metadata": {"name": "kasoku"},
            "spec": {
                "destination": {"server": "https://kubernetes.default.svc"},
                "kubernetes": {"kubeVersion": "1.31.4", "apiVersions": ["v1"]}
            }
        }))
        .unwrap();
        let baseline = crate::gitops::CompiledTargetTree {
            provenance: BTreeMap::new(),
            target: target.clone(),
            cluster,
            repository_name: None,
            repository: InlineGitRepository {
                repo_url: "https://example.invalid/deploy.git".to_string(),
                publish_url: None,
            },
            files: BTreeMap::new(),
            inputs: BTreeSet::new(),
        };
        let baseline_marker = publication_marker(&baseline).unwrap();
        let mut desired = baseline;
        desired.target.spec.publication.path_prefix = Some("new-prefix".to_string());
        assert_ne!(baseline_marker, publication_marker(&desired).unwrap());

        let changed_publication_marker = publication_marker(&desired).unwrap();
        desired.cluster.metadata.name = "magnolia".to_string();
        assert_ne!(changed_publication_marker, publication_marker(&desired).unwrap());
    }

    #[test]
    fn published_tree_requires_an_ownership_index() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("unrelated.yaml"), "kind: ConfigMap\n").unwrap();
        let error = read_rendered_tree(temp.path()).unwrap_err();
        assert!(error.to_string().contains("no ownership index"));
    }

    #[cfg(unix)]
    #[test]
    fn published_root_rejects_a_symlinked_prefix_ancestor() {
        use std::os::unix::fs::symlink;

        let checkout = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), checkout.path().join("production")).unwrap();

        let error = checked_published_root(checkout.path(), "production/apps").unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }
}
