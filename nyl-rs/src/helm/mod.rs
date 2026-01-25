/// Helm integration module
///
/// This module handles:
/// - Chart resolution (local paths in Phase 2)
/// - Template command building
/// - Chart caching (stubbed for git/OCI in Phase 3)
use crate::resources::ChartRef;
use crate::{NylError, Result};
use std::path::{Path, PathBuf};

mod template;
pub use template::HelmTemplateExecutor;

/// Resolved chart reference with absolute path
#[derive(Debug, Clone)]
pub struct ResolvedChart {
    /// Absolute path to the chart directory
    pub path: PathBuf,

    /// Original chart reference
    pub chart_ref: ChartRef,
}

/// Helm chart resolver
///
/// Phase 2: Supports local paths and search path resolution only
/// Phase 3: Git and OCI repository support
pub struct HelmChartResolver {
    /// Search paths for charts
    search_paths: Vec<PathBuf>,

    /// Working directory for relative path resolution
    working_dir: PathBuf,
}

impl HelmChartResolver {
    /// Create a new chart resolver
    ///
    /// # Arguments
    /// * `search_paths` - Directories to search for charts
    /// * `working_dir` - Working directory for relative paths
    pub fn new(search_paths: Vec<PathBuf>, working_dir: PathBuf) -> Self {
        Self {
            search_paths,
            working_dir,
        }
    }

    /// Resolve a chart reference to an absolute path
    ///
    /// Supports:
    /// - Git repositories (git + git_ref + path)
    /// - Local paths (path)
    /// - Chart names from search paths (name)
    ///
    /// # Arguments
    /// * `chart_ref` - The chart reference to resolve
    ///
    /// # Returns
    /// ResolvedChart with absolute path
    pub fn resolve_chart(&self, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        // Handle Git chart references
        if let Some(ref git_url) = chart_ref.git {
            return self.resolve_git(git_url, chart_ref);
        }

        if chart_ref.repository.is_some() {
            return Err(NylError::Config(
                "Repository chart references not supported in Phase 2".to_string(),
            ));
        }

        // Handle local path
        if let Some(ref path) = chart_ref.path {
            return self.resolve_local_path(path, chart_ref);
        }

        // Handle chart by name (search in search_paths)
        if let Some(ref name) = chart_ref.name {
            return self.resolve_by_name(name, chart_ref);
        }

        Err(NylError::Config(
            "Chart reference must have either 'path' or 'name'".to_string(),
        ))
    }

    /// Resolve a local file path
    fn resolve_local_path(&self, path: &str, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        let path = Path::new(path);

        // Make absolute if relative
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        };

        // Verify the chart exists
        if !abs_path.exists() {
            return Err(NylError::Config(format!(
                "Chart path does not exist: {}",
                abs_path.display()
            )));
        }

        // Verify Chart.yaml exists
        let chart_yaml = abs_path.join("Chart.yaml");
        if !chart_yaml.exists() {
            return Err(NylError::Config(format!(
                "Chart.yaml not found in: {}",
                abs_path.display()
            )));
        }

        Ok(ResolvedChart {
            path: abs_path,
            chart_ref: chart_ref.clone(),
        })
    }

    /// Resolve a chart by name from search paths
    fn resolve_by_name(&self, name: &str, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        for search_path in &self.search_paths {
            let chart_path = search_path.join(name);

            if chart_path.exists() {
                let chart_yaml = chart_path.join("Chart.yaml");
                if chart_yaml.exists() {
                    return Ok(ResolvedChart {
                        path: chart_path,
                        chart_ref: chart_ref.clone(),
                    });
                }
            }
        }

        Err(NylError::Config(format!(
            "Chart '{}' not found in search paths: {}",
            name,
            self.search_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// Get the search paths
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.search_paths
    }

    /// Get the working directory
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Resolve a Git chart reference
    fn resolve_git(&self, git_url: &str, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        let mut git_manager = crate::git::GitManager::new()?;

        let worktree_path =
            git_manager.resolve_ref(git_url, chart_ref.git_ref.as_deref(), chart_ref.path.as_deref())?;

        // Verify Chart.yaml exists
        let chart_yaml = worktree_path.join("Chart.yaml");
        if !chart_yaml.exists() {
            return Err(NylError::Config(format!(
                "Chart.yaml not found at: {}",
                worktree_path.display()
            )));
        }

        Ok(ResolvedChart {
            path: worktree_path,
            chart_ref: chart_ref.clone(),
        })
    }
}

impl std::fmt::Debug for HelmChartResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelmChartResolver")
            .field("search_paths", &self.search_paths)
            .field("working_dir", &self.working_dir)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_chart(base: &Path, name: &str) {
        let chart_dir = base.join(name);
        fs::create_dir_all(&chart_dir).unwrap();
        fs::write(
            chart_dir.join("Chart.yaml"),
            format!("apiVersion: v2\nname: {}\nversion: 1.0.0\n", name),
        )
        .unwrap();
    }

    #[test]
    fn test_resolver_new() {
        let temp = TempDir::new().unwrap();
        let search_paths = vec![temp.path().to_path_buf()];
        let resolver = HelmChartResolver::new(search_paths.clone(), temp.path().to_path_buf());

        assert_eq!(resolver.search_paths(), &search_paths);
        assert_eq!(resolver.working_dir(), temp.path());
    }

    #[test]
    fn test_resolve_local_absolute_path() {
        let temp = TempDir::new().unwrap();
        create_test_chart(temp.path(), "mychart");

        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            path: Some(temp.path().join("mychart").to_string_lossy().to_string()),
            ..Default::default()
        };

        let resolved = resolver.resolve_chart(&chart_ref).unwrap();
        assert_eq!(resolved.path, temp.path().join("mychart"));
        assert!(resolved.path.join("Chart.yaml").exists());
    }

    #[test]
    fn test_resolve_local_relative_path() {
        let temp = TempDir::new().unwrap();
        create_test_chart(temp.path(), "mychart");

        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            path: Some("mychart".to_string()),
            ..Default::default()
        };

        let resolved = resolver.resolve_chart(&chart_ref).unwrap();
        assert_eq!(resolved.path, temp.path().join("mychart"));
    }

    #[test]
    fn test_resolve_by_name() {
        let temp = TempDir::new().unwrap();
        let search_dir = temp.path().join("charts");
        fs::create_dir_all(&search_dir).unwrap();
        create_test_chart(&search_dir, "nginx");

        let resolver = HelmChartResolver::new(vec![search_dir.clone()], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            name: Some("nginx".to_string()),
            ..Default::default()
        };

        let resolved = resolver.resolve_chart(&chart_ref).unwrap();
        assert_eq!(resolved.path, search_dir.join("nginx"));
    }

    #[test]
    fn test_resolve_by_name_multiple_search_paths() {
        let temp = TempDir::new().unwrap();
        let search1 = temp.path().join("charts1");
        let search2 = temp.path().join("charts2");
        fs::create_dir_all(&search1).unwrap();
        fs::create_dir_all(&search2).unwrap();

        create_test_chart(&search1, "chart1");
        create_test_chart(&search2, "chart2");

        let resolver = HelmChartResolver::new(vec![search1.clone(), search2.clone()], temp.path().to_path_buf());

        // Chart1 from first path
        let chart1_ref = ChartRef {
            name: Some("chart1".to_string()),
            ..Default::default()
        };
        let resolved1 = resolver.resolve_chart(&chart1_ref).unwrap();
        assert_eq!(resolved1.path, search1.join("chart1"));

        // Chart2 from second path
        let chart2_ref = ChartRef {
            name: Some("chart2".to_string()),
            ..Default::default()
        };
        let resolved2 = resolver.resolve_chart(&chart2_ref).unwrap();
        assert_eq!(resolved2.path, search2.join("chart2"));
    }

    #[test]
    fn test_resolve_path_not_found() {
        let temp = TempDir::new().unwrap();
        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            path: Some("missing".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_resolve_name_not_found() {
        let temp = TempDir::new().unwrap();
        let search_dir = temp.path().join("charts");
        fs::create_dir_all(&search_dir).unwrap();

        let resolver = HelmChartResolver::new(vec![search_dir], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            name: Some("missing".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found in search paths"));
    }

    #[test]
    fn test_resolve_missing_chart_yaml() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("bad-chart");
        fs::create_dir_all(&chart_dir).unwrap();
        // Don't create Chart.yaml

        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            path: Some("bad-chart".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Chart.yaml not found"));
    }

    // Git chart resolution test is now an integration test
    // as it requires actual Git operations

    #[test]
    fn test_resolve_repository_not_supported() {
        let temp = TempDir::new().unwrap();
        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            repository: Some("https://charts.example.com".to_string()),
            name: Some("nginx".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Repository chart references not supported"));
    }

    #[test]
    fn test_resolve_no_path_or_name() {
        let temp = TempDir::new().unwrap();
        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef::default();

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must have either 'path' or 'name'"));
    }
}
