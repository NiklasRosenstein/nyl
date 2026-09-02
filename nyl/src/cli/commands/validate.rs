use clap::{Args, Subcommand};
use std::path::Path;
use tracing::{debug, info, warn};

use crate::config::ProjectConfig;
use crate::{NylError, Result};

/// Validate project configuration
#[derive(Args, Debug)]
pub struct ValidateArgs {
    #[command(subcommand)]
    pub command: Option<ValidateSubcommand>,

    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Strict validation mode (treat warnings as errors)
    #[arg(short, long)]
    pub strict: bool,
}

#[derive(Subcommand, Debug)]
pub enum ValidateSubcommand {
    /// Validate all discovered rendered GitOps resources and references.
    Gitops {
        /// Project directory or a path beneath it.
        #[arg(default_value = ".")]
        path: String,
    },
}

pub async fn execute(args: ValidateArgs) -> Result<()> {
    if let Some(ValidateSubcommand::Gitops { path }) = args.command {
        let inventory = crate::gitops::discover_gitops_inventory(Path::new(&path), None)?;
        crate::gitops::validate_gitops_inventory(&inventory)?;
        let target_names = inventory
            .resources
            .values()
            .filter(|resource| resource.identity.kind == crate::resources::GitOpsResourceKind::DeploymentTarget)
            .map(|resource| resource.identity.name.clone())
            .collect::<Vec<_>>();
        for target in target_names {
            crate::gitops::compile_target_tree(&inventory, &target).await?;
        }
        println!(
            "✓ GitOps configuration is valid ({} resource(s), {} YAML file(s))",
            inventory.resources.len(),
            inventory.yaml_files.len()
        );
        return Ok(());
    }
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
    let config = ProjectConfig::load(config_file)?;

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
            command: None,
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
            command: None,
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
            command: None,
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
            command: None,
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args).await;
        // Should succeed with warnings
        assert!(result.is_ok());
    }
}
