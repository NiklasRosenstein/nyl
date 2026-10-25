use clap::Args;
use git2::Repository;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::config::ProjectConfig;
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
    info!("Validating project configuration");
    debug!("Validation path: {}", args.path);
    debug!("Strict mode: {}", args.strict);

    // Load project configuration
    let project_dir = Path::new(&args.path);

    // Try to find config file
    let config_file = ProjectConfig::find(Some(project_dir))?;

    if let Some(ref file) = config_file {
        println!("✓ Found project config: {}", file.display());
    } else {
        warn!("No project configuration file found, using defaults");
        println!("⚠ No project configuration file found, using defaults");
    }

    // Load configuration
    let config = ProjectConfig::load(config_file.clone())?;

    for path in config.get_components_search_paths() {
        if path.exists() {
            println!("✓ Components search path exists: {}", path.display());
        } else {
            warn!("Components search path does not exist: {}", path.display());
            println!("⚠ Components search path does not exist: {}", path.display());
        }
    }

    for path in config.get_helm_chart_search_paths() {
        if path.exists() {
            println!("✓ Helm chart search path exists: {}", path.display());
        } else {
            warn!("Helm chart search path does not exist: {}", path.display());
            println!("⚠ Helm chart search path does not exist: {}", path.display());
        }
    }

    // Validate configuration
    let validation_warnings = config.validate();

    // Print any additional warnings from validation
    let mut has_warnings = false;
    if !validation_warnings.is_empty() {
        println!("\nValidation warnings:");
        for warning in &validation_warnings {
            has_warnings = true;
            warn!("{}", warning);
            println!("  ⚠ {}", warning);
        }
    }

    if let Some(config_file) = config_file {
        let project_root = config_file.parent().unwrap_or(project_dir);
        if Repository::discover(project_root).is_ok() {
            let inventory = crate::gitops::discover_gitops_inventory(project_root, None)?;
            crate::gitops::validate_gitops_inventory(&inventory)?;
            for warning in crate::orchestration::project::validate_environments(&inventory)? {
                has_warnings = true;
                warn!("{}", warning);
                println!("  ⚠ {}: {} ({})", warning.code, warning.message, warning.fix);
            }
            let units = crate::gitops::units::discover_units(&inventory)?;
            // Target bindings first: their findings name the DeploymentTarget
            // field a publication unit's reference comes from.
            validate_targets(&inventory).await?;
            validate_environment_units(&inventory)?;
            println!(
                "✓ GitOps configuration is valid ({} resource(s), {} unit(s), {} YAML file(s))",
                inventory.resources.len(),
                units.len(),
                inventory.yaml_files.len()
            );
        } else if project_root.join("gitops.yaml").exists() {
            return Err(NylError::config(format!(
                "GitOps configuration {} requires a Git worktree",
                project_root.join("gitops.yaml").display()
            )));
        }
    }

    // Determine result
    if args.strict && has_warnings {
        println!("\n✗ Validation failed in strict mode (warnings treated as errors)");
        Err(NylError::Validation("Validation failed in strict mode".to_string()))
    } else {
        if has_warnings {
            println!("\n✓ Validation passed with warnings");
        } else {
            println!("\n✓ Validation passed");
        }
        Ok(())
    }
}

/// Render every Environment's units from the worktree and run the state-free
/// unit checks `nyl plan` and `nyl reconcile` run
/// ([`nyl_orchestration::rules::desired::check_units`]), with the drivers
/// they use: specs, `env.secrets`, ownership scopes, unknown dependencies,
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
    for environment in &environments {
        let name =
            EnvironmentName::new(&environment.metadata.name).map_err(|error| NylError::config(error.to_string()))?;
        let source = renderer
            .render(&inventory.worktree_root, &name, &commit)
            .map_err(|error| NylError::config(error.to_string()))?;
        for finding in nyl_orchestration::rules::desired::check_units(&source, &wiring.drivers) {
            issues.push(format!("{name}: {finding}"));
        }
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

    #[tokio::test]
    async fn test_validate_with_valid_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");
        let components_dir = temp.path().join("components");

        fs::write(&config_path, "[project]\n").unwrap();
        fs::create_dir(&components_dir).unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args).await;
        assert!(result.is_ok());
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

    #[tokio::test]
    async fn test_validate_missing_components_dir() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");
        fs::write(&config_path, "[project]\n").unwrap();
        // Don't create components directory

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args).await;
        // Should succeed with warnings
        assert!(result.is_ok());
    }
}
