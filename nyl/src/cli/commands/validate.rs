use clap::Args;
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

pub fn execute(args: ValidateArgs) -> Result<()> {
    info!("Validating project configuration");
    debug!("Validation path: {}", args.path);
    debug!("Strict mode: {}", args.strict);

    // Load project configuration
    let project_dir = Path::new(&args.path);

    // Change to project directory to ensure relative paths are resolved correctly
    let original_dir = std::env::current_dir()?;
    std::env::set_current_dir(project_dir)?;

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

    // Get components path and check if it exists
    let components_path = config.get_components_path();
    if components_path.exists() {
        println!("✓ Components directory exists: {}", components_path.display());
    } else {
        warn!("Components directory does not exist: {}", components_path.display());
        println!("⚠ Components directory does not exist: {}", components_path.display());
    }

    // Validate search paths
    for path in &config.config.settings.search_path {
        if path.exists() {
            println!("✓ Search path exists: {}", path.display());
        } else {
            warn!("Search path does not exist: {}", path.display());
            println!("⚠ Search path does not exist: {}", path.display());
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

    // Determine result before restoring directory
    let result = if args.strict && has_warnings {
        println!("\n✗ Validation failed in strict mode (warnings treated as errors)");
        Err(NylError::Validation("Validation failed in strict mode".to_string()))
    } else {
        if has_warnings {
            println!("\n✓ Validation passed with warnings");
        } else {
            println!("\n✓ Validation passed");
        }
        Ok(())
    };

    // Always restore original directory
    std::env::set_current_dir(original_dir)?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_validate_with_valid_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");
        let components_dir = temp.path().join("components");

        fs::write(&config_path, "settings: {}").unwrap();
        fs::create_dir(&components_dir).unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_no_config() {
        let temp = TempDir::new().unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args);
        // Should succeed but with warnings
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_strict_mode_fails_on_warnings() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        // Create config (components dir won't exist, causing a warning)
        fs::write(&config_path, "settings:\n  components_path: nonexistent").unwrap();

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: true,
        };

        let result = execute(args);
        // Should fail in strict mode due to validation warning
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_missing_components_dir() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        fs::write(&config_path, "settings: {}").unwrap();
        // Don't create components directory

        let args = ValidateArgs {
            path: temp.path().to_str().unwrap().to_string(),
            strict: false,
        };

        let result = execute(args);
        // Should succeed with warnings
        assert!(result.is_ok());
    }
}
