use clap::Args;
use git2::Repository;
use nyl_core::orchestration::codes::Finding;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::config::{ConfigWarning, ProjectConfig};
use crate::util::path_for_display;
use crate::{NylError, Result};

/// Validate project configuration
#[derive(Args, Debug)]
pub struct ValidateArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Strict validation mode (treat warnings as errors)
    #[arg(short, long)]
    pub strict: bool,
}

pub async fn execute(args: ValidateArgs) -> Result<()> {
    debug!(
        "Validating project configuration at {} (strict: {})",
        args.path, args.strict
    );
    let mut report = ValidationReport::default();
    let gitops = collect(Path::new(&args.path), &mut report).await;
    if let Err(error) = gitops {
        // Report what passed before the failure; the error itself follows.
        print!("{}", report.render(None));
        return Err(error);
    }
    let failed = args.strict && !report.warnings.is_empty();
    print!("{}", report.render(Some(args.strict)));
    if failed {
        Err(NylError::Validation("Validation failed in strict mode".to_string()))
    } else {
        Ok(())
    }
}

/// Everything `nyl validate` found. The command prints it once, through
/// [`ValidationReport::render`].
#[derive(Debug, Default)]
struct ValidationReport {
    /// The project configuration file, relative to the current directory.
    config_file: Option<String>,
    /// The directory containing `nyl.toml`; paths display relative to it.
    config_dir: Option<PathBuf>,
    /// Search paths that exist.
    search_paths: Vec<(SearchPathKind, PathBuf)>,
    /// Counts from a successful GitOps validation.
    gitops: Option<GitOpsSummary>,
    warnings: Vec<ValidationWarning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchPathKind {
    Components,
    HelmChart,
}

#[derive(Debug, PartialEq, Eq)]
struct GitOpsSummary {
    resources: usize,
    units: usize,
    yaml_files: usize,
}

#[derive(Debug)]
enum ValidationWarning {
    NoProjectConfig,
    Config(ConfigWarning),
    Environment(Finding),
}

impl ValidationReport {
    /// Render the report. `strict` is the requested mode once validation
    /// completed, or `None` when it stopped at an error.
    fn render(&self, strict: Option<bool>) -> String {
        let display = |path: &Path| display_project_path(self.config_dir.as_deref(), path);
        let mut out = String::new();
        if let Some(file) = &self.config_file {
            let _ = writeln!(out, "✓ Found project config: {file}");
        }
        for (kind, path) in &self.search_paths {
            let label = match kind {
                SearchPathKind::Components => "Components",
                SearchPathKind::HelmChart => "Helm chart",
            };
            let _ = writeln!(out, "✓ {label} search path exists: {}", display(path));
        }
        if let Some(gitops) = &self.gitops {
            let _ = writeln!(
                out,
                "✓ GitOps configuration is valid ({} resource(s), {} unit(s), {} YAML file(s))",
                gitops.resources, gitops.units, gitops.yaml_files
            );
        }
        for warning in &self.warnings {
            let text = match warning {
                ValidationWarning::NoProjectConfig => "No project configuration file found, using defaults".to_string(),
                ValidationWarning::Config(warning) => warning.describe(display),
                ValidationWarning::Environment(finding) => finding.to_string(),
            };
            let _ = writeln!(out, "⚠ {text}");
        }
        let Some(strict) = strict else {
            return out;
        };
        out.push('\n');
        out.push_str(match (self.warnings.is_empty(), strict) {
            (true, _) => "✓ Validation passed\n",
            (false, false) => "✓ Validation passed with warnings\n",
            (false, true) => "✗ Validation failed in strict mode (warnings treated as errors)\n",
        });
        out
    }
}

/// Run every check, recording results in `report`. GitOps validation errors
/// end the run.
async fn collect(project_dir: &Path, report: &mut ValidationReport) -> Result<()> {
    let config_file = ProjectConfig::find(Some(project_dir))?;
    let config = ProjectConfig::load(config_file.clone())?;
    report.config_dir = config_file.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    match &config_file {
        Some(file) => report.config_file = Some(slash_path(&path_for_display(file))),
        None => report.warnings.push(ValidationWarning::NoProjectConfig),
    }
    for (kind, search_paths) in [
        (SearchPathKind::Components, config.get_components_search_paths()),
        (SearchPathKind::HelmChart, config.get_helm_chart_search_paths()),
    ] {
        for path in search_paths.iter().filter(|path| path.exists()) {
            report.search_paths.push((kind, path.clone()));
        }
    }
    report
        .warnings
        .extend(config.validate().into_iter().map(ValidationWarning::Config));

    let Some(config_file) = config_file else {
        return Ok(());
    };
    let project_root = config_file.parent().unwrap_or(project_dir);
    if Repository::discover(project_root).is_err() {
        if project_root.join("gitops.yaml").exists() {
            return Err(NylError::config(format!(
                "GitOps configuration {} requires a Git worktree",
                project_root.join("gitops.yaml").display()
            )));
        }
        return Ok(());
    }
    let inventory = crate::gitops::discover_gitops_inventory(project_root, None)?;
    crate::gitops::validate_gitops_inventory(&inventory)?;
    report.warnings.extend(
        crate::orchestration::project::validate_environments(&inventory)?
            .into_iter()
            .map(ValidationWarning::Environment),
    );
    let units = crate::gitops::units::discover_units(&inventory)?;
    // Target bindings first: their findings name the DeploymentTarget field a
    // publication unit's reference comes from.
    validate_targets(&inventory).await?;
    validate_environment_units(&inventory)?;
    report.gitops = Some(GitOpsSummary {
        resources: inventory.resources.len(),
        units: units.len(),
        yaml_files: inventory.yaml_files.len(),
    });
    Ok(())
}

/// A configured path as `nyl.toml` names it: relative to its directory, or
/// absolute. Paths of a project without `nyl.toml` display as given.
fn display_project_path(project_root: Option<&Path>, path: &Path) -> String {
    match project_root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Some(relative) => slash_path(relative),
        None => slash_path(path),
    }
}

fn slash_path(path: &Path) -> String {
    path.display().to_string().replace(std::path::MAIN_SEPARATOR, "/")
}

/// Render every Environment's units from the worktree and run the state-free
/// unit checks `nyl plan` and `nyl reconcile` run
/// ([`nyl_orchestration::rules::desired::check_units`]), with the drivers
/// they use: specs, ownership scopes, unknown dependencies,
/// cycles, and references to undeclared or sensitive outputs.
fn validate_environment_units(inventory: &crate::gitops::GitOpsInventory) -> Result<()> {
    use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
    use nyl_core::orchestration::ports::SourceRenderer;

    let wiring = crate::orchestration::run::ProcessWiring::new();
    let renderer = crate::gitops::units::UnitSourceRenderer::new(crate::orchestration::session::project_dir(inventory));
    // The worktree is rendered as it is, committed or not; the commit only labels it.
    let commit = CommitId::new("0".repeat(40)).expect("a commit ID");
    let (environments, _) = crate::orchestration::project::declared(inventory);
    let mut issues = Vec::new();
    let mut rendered_sources = std::collections::BTreeMap::new();
    for environment in &environments {
        let name =
            EnvironmentName::new(&environment.metadata.name).map_err(|error| NylError::config(error.to_string()))?;
        let source = renderer
            .render(&inventory.worktree_root, &name, &commit)
            .map_err(|error| NylError::config(error.to_string()))?;
        rendered_sources.insert(name.clone(), source.clone());
        for finding in nyl_orchestration::rules::desired::check_units(&source, &wiring.drivers) {
            issues.push(format!("{name}: {finding}"));
        }
    }
    for finding in nyl_orchestration::rules::desired::check_ownership(&rendered_sources, &wiring.drivers) {
        issues.push(finding.to_string());
    }
    if let Err(finding) = nyl_orchestration::rules::multi_environment::validate_sources(&rendered_sources) {
        issues.push(finding.to_string());
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(NylError::config(format!(
            "units are invalid:\n{}",
            issues
                .iter()
                .map(|issue| format!("  - {issue}"))
                .collect::<Vec<_>>()
                .join("\n")
        )))
    }
}

/// Compile every DeploymentTarget and check generated Argo CD names across them.
///
/// A target an environment's publication unit places compiles with
/// placeholders for the bindings orchestration resolves, and its `fromUnit`
/// references are checked against that environment's units instead. Other
/// targets compile as `render-tree` does, so those bindings stay rejected.
async fn validate_targets(inventory: &crate::gitops::GitOpsInventory) -> Result<()> {
    let target_names = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == crate::resources::GitOpsResourceKind::DeploymentTarget)
        .map(|resource| resource.identity.name.clone())
        .collect::<Vec<_>>();
    // Like `vendor --check`, validation reads fromPublication state at
    // the refreshed branch head and falls back to the cached head, so
    // it also works offline.
    let options = crate::gitops::TreeRenderOptions {
        publication_read: crate::gitops::inputs::PublicationRead::FreshOrCached,
        ..crate::gitops::TreeRenderOptions::default()
    };
    let placements = crate::gitops::units::target_placements(inventory)?;
    let mut compiled = Vec::with_capacity(target_names.len());
    for target in target_names {
        let mut target_options = options.clone();
        if let Some(environment) = placements.get(&target) {
            let issues = crate::gitops::units::check_unit_bindings(inventory, &target, environment)?;
            if !issues.is_empty() {
                return Err(NylError::config(format!(
                    "DeploymentTarget {target:?} of Environment {environment} has invalid unit references:\n{}",
                    issues
                        .iter()
                        .map(|issue| format!("  - {issue}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )));
            }
            target_options.orchestrated_inputs = Some(crate::gitops::inputs::OrchestratedInputs::Placeholders);
        }
        compiled.push(crate::gitops::compile_target_tree_with_options(inventory, &target, target_options).await?);
    }
    crate::gitops::validate_compiled_argocd_names(inventory, &compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Collect and render the report for `dir`, without the first line,
    /// which names `nyl.toml` relative to the current directory.
    async fn rendered(dir: &Path, strict: bool) -> String {
        let mut report = ValidationReport::default();
        collect(dir, &mut report).await.unwrap();
        assert!(report.config_file.is_some());
        let rendered = report.render(Some(strict));
        rendered.split_once('\n').unwrap().1.to_string()
    }

    #[tokio::test]
    async fn test_validate_with_valid_config() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        fs::create_dir(temp.path().join("components")).unwrap();

        assert_eq!(
            rendered(temp.path(), false).await,
            "✓ Components search path exists: components\n\
             ✓ Helm chart search path exists: .\n\
             \n\
             ✓ Validation passed\n"
        );
    }

    #[tokio::test]
    async fn test_validate_reports_each_warning_once_with_project_relative_paths() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("nyl.toml"),
            "[project]\ncomponents_search_paths = [\"missing\"]\n\n[project.aliases]\nbad = \"x\"\n",
        )
        .unwrap();

        assert_eq!(
            rendered(temp.path(), false).await,
            "✓ Helm chart search path exists: .\n\
             ⚠ Components search path does not exist: missing\n\
             ⚠ Alias key 'bad' is invalid; expected '<apiVersion>/<kind>'\n\
             \n\
             ✓ Validation passed with warnings\n"
        );
        assert!(rendered(temp.path(), true)
            .await
            .ends_with("\n✗ Validation failed in strict mode (warnings treated as errors)\n"));
    }

    #[tokio::test]
    async fn test_validate_displays_search_paths_relative_to_nyl_toml() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        fs::create_dir_all(temp.path().join("nyl")).unwrap();
        fs::write(
            temp.path().join("nyl/nyl.toml"),
            "[project]\ncomponents_search_paths = [\"../shared/components\"]\n",
        )
        .unwrap();

        assert!(rendered(temp.path(), false)
            .await
            .contains("⚠ Components search path does not exist: ../shared/components\n"));
    }

    #[tokio::test]
    async fn test_validate_no_config() {
        let temp = TempDir::new().unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args).await;
        // Should succeed but with warnings
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_strict_mode_fails_on_warnings() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");
        fs::write(&config_path, "[project]\ncomponents_search_paths = [\"nonexistent\"]\n").unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: true,
        };

        let result = execute(args).await;
        // Should fail in strict mode due to validation warning
        assert!(result.is_err());
    }
}
