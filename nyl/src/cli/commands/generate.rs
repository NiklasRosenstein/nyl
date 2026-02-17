use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::{resources::extract_nyl_release, NylError, Result};

/// Generate configurations (ArgoCD Applications, etc.)
#[derive(Args, Debug)]
pub struct GenerateArgs {
    #[command(subcommand)]
    pub command: GenerateSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum GenerateSubcommand {
    /// Generate ArgoCD Applications from Nyl manifests
    Argocd {
        /// Directory containing Nyl manifests (default: current directory)
        #[arg(default_value = ".")]
        path: String,

        /// Git repository URL
        #[arg(long)]
        repo_url: String,

        /// Git revision/branch
        #[arg(long, default_value = "main")]
        revision: String,

        /// ArgoCD namespace
        #[arg(long, default_value = "argocd")]
        argocd_namespace: String,

        /// ArgoCD project
        #[arg(long, default_value = "default")]
        project: String,

        /// Output directory (if not specified, outputs to stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Generate JSON schemas
    Schema {
        #[command(subcommand)]
        command: SchemaSubcommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum SchemaSubcommand {
    /// Generate JSON schema for nyl.toml project configuration
    Config,
}

pub fn execute(args: GenerateArgs) -> Result<()> {
    match args.command {
        GenerateSubcommand::Argocd {
            path,
            repo_url,
            revision,
            argocd_namespace,
            project,
            output,
        } => generate_argocd_applications(
            &path,
            &repo_url,
            &revision,
            &argocd_namespace,
            &project,
            output.as_deref(),
        ),
        GenerateSubcommand::Schema {
            command: SchemaSubcommand::Config,
        } => {
            let schema = crate::config::schema::generate_project_config_schema();
            let output = serde_json::to_string_pretty(&schema)
                .map_err(|e| NylError::Config(format!("Failed to serialize schema JSON: {}", e)))?;
            println!("{}", output);
            Ok(())
        }
    }
}

fn generate_argocd_applications(
    manifests_dir: &str,
    repo_url: &str,
    revision: &str,
    argocd_namespace: &str,
    project: &str,
    output_dir: Option<&Path>,
) -> Result<()> {
    // Find all .yaml/.yml files in directory
    let yaml_files = find_yaml_files(manifests_dir)?;

    if yaml_files.is_empty() {
        tracing::warn!("No YAML files found in {}", manifests_dir);
        return Ok(());
    }

    let mut applications = Vec::new();
    let mut skipped = 0;

    for file_path in yaml_files {
        tracing::debug!("Reading YAML file: {}", file_path.display());

        // Read and parse file
        let content = std::fs::read_to_string(&file_path)
            .map_err(|e| NylError::Config(format!("Failed to read {}: {}", file_path.display(), e)))?;

        // Parse YAML documents with source context for better error messages
        let source_ctx = crate::util::SourceContext::new(file_path.clone());
        let manifests: Vec<serde_json::Value> = source_ctx.parse_yaml_documents(&content)?;

        // Extract NylRelease metadata
        let (nyl_release, _) = extract_nyl_release(&manifests)?;

        if let Some(release) = nyl_release {
            // Generate ArgoCD Application
            let app = create_argocd_application(
                &release,
                &file_path,
                manifests_dir,
                repo_url,
                revision,
                argocd_namespace,
                project,
            )?;

            applications.push(app);
        } else {
            skipped += 1;
            tracing::warn!("Skipping {} - no NylRelease resource found", file_path.display());
        }
    }

    if applications.is_empty() {
        tracing::warn!("No ArgoCD Applications generated");
        tracing::info!("Files must contain a NylRelease resource to generate Applications");
        return Err(NylError::Config("No valid NylRelease resources found".to_string()));
    }

    // Output applications
    if let Some(out_dir) = output_dir {
        // Write each application to separate file
        std::fs::create_dir_all(out_dir)?;

        for app in &applications {
            let app_name = app["metadata"]["name"].as_str().unwrap();
            let output_file = out_dir.join(format!("{}.yaml", app_name));

            let yaml = serde_norway::to_string(app)?;
            std::fs::write(&output_file, yaml)?;

            println!("✓ Generated {}", output_file.display());
        }

        println!(
            "\nGenerated {} ArgoCD Application(s) ({} file(s) skipped)",
            applications.len(),
            skipped
        );
    } else {
        // Output all to stdout
        for (i, app) in applications.iter().enumerate() {
            if i > 0 {
                println!("---");
            }
            let yaml = serde_norway::to_string(app)?;
            print!("{}", yaml);
        }
    }

    Ok(())
}

/// Create an ArgoCD Application manifest from a NylRelease
fn create_argocd_application(
    release: &crate::resources::NylRelease,
    file_path: &Path,
    base_dir: &str,
    repo_url: &str,
    revision: &str,
    argocd_namespace: &str,
    project: &str,
) -> Result<serde_json::Value> {
    // Calculate path and template input relative to base_dir.
    let base_path = std::fs::canonicalize(base_dir)?;
    let file_abs = std::fs::canonicalize(file_path)?;
    let rel_dir = file_abs
        .strip_prefix(&base_path)
        .unwrap_or(file_path)
        .parent()
        .unwrap_or(Path::new(""));
    let rel_dir_normalized = normalize_relative_path_to_posix(rel_dir);
    let source_path = if rel_dir_normalized.is_empty() {
        ".".to_string()
    } else {
        rel_dir_normalized
    };
    let template_input = file_abs
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| NylError::Config(format!("Invalid file name: {}", file_path.display())))?;

    Ok(serde_json::json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {
            "name": release.metadata.name,
            "namespace": argocd_namespace,
        },
        "spec": {
            "project": project,
            "source": {
                "repoURL": repo_url,
                "path": source_path,
                "targetRevision": revision,
                "plugin": {
                    "name": "nyl-v2",
                    "env": [
                        {
                            "name": "NYL_RELEASE_NAME",
                            "value": release.metadata.name,
                        },
                        {
                            "name": "NYL_RELEASE_NAMESPACE",
                            "value": release.metadata.namespace,
                        },
                        {
                            "name": "NYL_CMP_TEMPLATE_INPUT",
                            "value": template_input,
                        },
                    ],
                },
            },
            "destination": {
                "server": "https://kubernetes.default.svc",
                "namespace": release.metadata.namespace,
            },
            "syncPolicy": {
                "automated": {
                    "prune": true,
                    "selfHeal": true,
                },
            },
        },
    }))
}

/// Normalize a relative path to POSIX-style separators.
fn normalize_relative_path_to_posix(path: &Path) -> String {
    let mut normalized = String::new();
    for component in path.components() {
        if let std::path::Component::Normal(os_str) = component {
            if !normalized.is_empty() {
                normalized.push('/');
            }
            normalized.push_str(&os_str.to_string_lossy());
        }
    }
    normalized
}

/// Find all YAML files in a directory (recursively)
/// Skips hidden files (starting with .) to avoid processing config files
fn find_yaml_files(dir: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(dir).follow_links(true) {
        let entry = entry.map_err(|e| NylError::Config(format!("Failed to read directory: {}", e)))?;
        let path = entry.path();

        // Skip hidden files and directories (starting with .)
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if file_name.starts_with('.') {
                continue;
            }
        }

        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "yaml" || ext == "yml" {
                    files.push(path.to_path_buf());
                }
            }
        }
    }

    Ok(files)
}

// Note: parse_yaml_documents is now replaced by SourceContext::parse_yaml_documents
// to provide better error messages with file context

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_yaml_documents() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: test
  namespace: default
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: config
";
        let source_ctx = crate::util::SourceContext::new(std::path::PathBuf::from("test.yaml"));
        let docs = source_ctx.parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "NylRelease");
        assert_eq!(docs[1]["kind"], "ConfigMap");
    }

    #[test]
    fn test_create_argocd_application() {
        use crate::resources::{NylRelease, NylReleaseMetadata, NylReleaseSpec};
        use std::env;

        let release = NylRelease {
            api_version: "nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "NylRelease".to_string(),
            metadata: NylReleaseMetadata {
                name: "myapp".to_string(),
                namespace: "production".to_string(),
            },
            spec: NylReleaseSpec::default(),
        };

        // Create a temporary test file for the test
        let temp_dir = env::temp_dir();
        let test_file = temp_dir.join("test-manifest.yaml");
        std::fs::write(&test_file, "test").unwrap();

        let app = create_argocd_application(
            &release,
            &test_file,
            temp_dir.to_str().unwrap(),
            "https://github.com/example/repo",
            "main",
            "argocd",
            "default",
        )
        .unwrap();

        // Clean up
        std::fs::remove_file(&test_file).ok();

        assert_eq!(app["apiVersion"], "argoproj.io/v1alpha1");
        assert_eq!(app["kind"], "Application");
        assert_eq!(app["metadata"]["name"], "myapp");
        assert_eq!(app["metadata"]["namespace"], "argocd");
        assert_eq!(app["spec"]["source"]["repoURL"], "https://github.com/example/repo");
        assert_eq!(app["spec"]["destination"]["namespace"], "production");
        assert_eq!(app["spec"]["source"]["path"], ".");
        assert_eq!(app["spec"]["source"]["plugin"]["name"], "nyl-v2");

        let env = app["spec"]["source"]["plugin"]["env"].as_array().unwrap();
        let template_input = env
            .iter()
            .find(|v| v["name"] == "NYL_CMP_TEMPLATE_INPUT")
            .and_then(|v| v["value"].as_str())
            .unwrap();
        assert_eq!(template_input, "test-manifest.yaml");
    }
}
