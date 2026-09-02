/// Helm integration module
///
/// This module handles:
/// - Chart resolution (local paths in Phase 2)
/// - Template command building
/// - Chart caching (stubbed for git/OCI in Phase 3)
use crate::git::CredentialProvider;
use crate::resources::ChartRef;
use crate::{NylError, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod oci;
mod template;
pub use oci::OciChartPuller;
pub use template::HelmTemplateExecutor;
pub(crate) use template::HELM_SOURCE_ANNOTATION;

/// Repository protocol type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositoryProtocol {
    /// Git repository (git+ prefix)
    Git,
    /// OCI registry (oci:// prefix)
    Oci,
    /// Traditional Helm repository (https:// or no prefix)
    Helm,
}

/// Parse protocol prefix from repository URL
///
/// Returns (protocol, url_without_prefix)
/// Supported protocols: "git+", "oci://"
fn parse_repository_protocol(repository: &str) -> (RepositoryProtocol, &str) {
    if let Some(url) = repository.strip_prefix("git+") {
        (RepositoryProtocol::Git, url)
    } else if repository.starts_with("oci://") {
        (RepositoryProtocol::Oci, repository)
    } else {
        (RepositoryProtocol::Helm, repository)
    }
}

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

    /// Optional cache directory for Git operations (defaults to env var/system default)
    cache_dir: Option<PathBuf>,

    credential_provider: Option<Arc<CredentialProvider>>,

    render_cache: Option<crate::render::cache::RenderCache>,
}

impl HelmChartResolver {
    /// Create a new chart resolver
    ///
    /// # Arguments
    /// * `search_paths` - Directories to search for charts
    /// * `working_dir` - Working directory for relative paths
    pub fn new(search_paths: Vec<PathBuf>, working_dir: PathBuf) -> Self {
        Self::with_cache_dir_and_provider(search_paths, working_dir, None, None)
    }

    /// Create a new chart resolver with explicit cache directory
    ///
    /// # Arguments
    /// * `search_paths` - Directories to search for charts
    /// * `working_dir` - Working directory for relative paths
    /// * `cache_dir` - Optional cache directory for Git operations (defaults to env var/system default)
    pub fn with_cache_dir(search_paths: Vec<PathBuf>, working_dir: PathBuf, cache_dir: Option<PathBuf>) -> Self {
        Self::with_cache_dir_and_provider(search_paths, working_dir, cache_dir, None)
    }

    pub fn with_cache_dir_and_provider(
        search_paths: Vec<PathBuf>,
        working_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        credential_provider: Option<Arc<CredentialProvider>>,
    ) -> Self {
        Self {
            search_paths,
            working_dir,
            cache_dir,
            credential_provider,
            render_cache: None,
        }
    }

    #[must_use]
    pub fn with_render_cache(mut self, cache: Option<crate::render::cache::RenderCache>) -> Self {
        self.render_cache = cache;
        self
    }

    /// Resolve a chart reference to an absolute path
    ///
    /// Supports:
    /// - Git repositories (repository with git+ prefix + version + name as subpath)
    /// - OCI repositories (repository with oci:// prefix + version)
    /// - Helm repositories (repository + version + name as chart name)
    /// - Local paths (name as filesystem path)
    /// - Chart names from search paths (name without path separators)
    ///
    /// # Arguments
    /// * `chart_ref` - The chart reference to resolve
    ///
    /// # Returns
    /// ResolvedChart with absolute path
    pub fn resolve_chart(&self, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        // Handle repository field (Git, OCI, or traditional Helm)
        if let Some(ref repository) = chart_ref.repository {
            let (protocol, url) = parse_repository_protocol(repository);

            match protocol {
                RepositoryProtocol::Git => {
                    // Git repository: git+https://... or git+git@...
                    return self.resolve_git(url, chart_ref);
                }
                RepositoryProtocol::Oci | RepositoryProtocol::Helm => {
                    // OCI or traditional Helm repository
                    let version = chart_ref.version.as_ref().ok_or_else(|| {
                        NylError::Config("Chart version is required when using repository".to_string())
                    })?;
                    return self.resolve_repository(repository, version, chart_ref);
                }
            }
        }

        // Handle name field (local path or search)
        if let Some(ref name) = chart_ref.name {
            // If name looks like a path (contains / or \ or starts with . or ~ or /), treat as local path
            // Otherwise, search in search_paths
            if name.contains('/')
                || name.contains('\\')
                || name.starts_with('.')
                || name.starts_with('~')
                || name.starts_with('/')
            {
                return self.resolve_local_path(name, chart_ref);
            } else {
                return self.resolve_by_name(name, chart_ref);
            }
        }

        Err(NylError::Config(
            "Chart reference must have either 'repository' or 'name'".to_string(),
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

    /// Resolve a chart from an OCI or Helm repository using `helm pull`
    fn resolve_repository(&self, repository: &str, version: &str, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        let puller = if let Some(cache_dir) = &self.cache_dir {
            OciChartPuller::with_cache_dir(cache_dir)
        } else {
            OciChartPuller::new()?
        }
        .with_render_cache(self.render_cache.clone());
        let chart_path = puller.pull(repository, version, chart_ref.name.as_deref())?;

        Ok(ResolvedChart {
            path: chart_path,
            chart_ref: chart_ref.clone(),
        })
    }

    /// Resolve a Git chart reference
    fn resolve_git(&self, repository_url: &str, chart_ref: &ChartRef) -> Result<ResolvedChart> {
        let mut git_manager = if let Some(ref cache_dir) = self.cache_dir {
            crate::git::GitManager::with_cache_dir_and_provider(cache_dir, self.credential_provider.clone())
        } else {
            crate::git::GitManager::with_credential_provider(self.credential_provider.clone())?
        }
        .with_render_cache(self.render_cache.clone());

        // Use 'name' field as subpath for Git repos
        let subpath = chart_ref.name.as_deref();

        let worktree_path = git_manager.resolve_ref(repository_url, chart_ref.version.as_deref(), subpath)?;

        // Verify Chart.yaml exists
        let chart_yaml = worktree_path.join("Chart.yaml");
        if !chart_yaml.exists() {
            return Err(NylError::Config(format!(
                "Chart.yaml not found at: {}",
                worktree_path.display()
            )));
        }

        // Run helm dependency build if the chart has dependencies
        if chart_has_dependencies(&worktree_path)? {
            build_helm_dependencies(&worktree_path)?;
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
            .field("cache_dir", &self.cache_dir)
            .field(
                "credential_provider",
                &self.credential_provider.as_ref().map(|_| "<redacted>"),
            )
            .field("render_cache", &self.render_cache.as_ref().map(|_| "<attached>"))
            .finish()
    }
}

/// Check if a Helm chart has dependencies
///
/// Returns true if the chart has a Chart.lock file or dependencies defined in Chart.yaml
fn chart_has_dependencies(chart_path: &Path) -> Result<bool> {
    // Check for Chart.lock (existing dependency state)
    let chart_lock = chart_path.join("Chart.lock");
    if chart_lock.exists() {
        return Ok(true);
    }

    // Check Chart.yaml for dependencies field
    let chart_yaml = chart_path.join("Chart.yaml");
    if !chart_yaml.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&chart_yaml)
        .map_err(|e| NylError::Config(format!("Failed to read Chart.yaml: {}", e)))?;

    // Parse as YAML and check for dependencies field
    let yaml: serde_json::Value =
        serde_norway::from_str(&content).map_err(|e| NylError::Config(format!("Failed to parse Chart.yaml: {}", e)))?;

    Ok(yaml.get("dependencies").is_some())
}

/// Check if helm dependencies are already built
///
/// Returns true if Chart.lock exists and the charts/ directory contains .tgz files.
/// This ensures we only skip the build if dependencies were previously built successfully.
fn dependencies_already_built(chart_path: &Path) -> bool {
    // Check if Chart.lock exists - this is created only after successful dependency build
    let chart_lock = chart_path.join("Chart.lock");
    if !chart_lock.exists() {
        return false;
    }

    let charts_dir = chart_path.join("charts");
    if !charts_dir.exists() {
        return false;
    }

    // Check if the charts directory contains any .tgz files (packaged dependencies)
    if let Ok(entries) = std::fs::read_dir(&charts_dir) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension() {
                if ext == "tgz" {
                    return true;
                }
            }
        }
    }

    false
}

/// Run helm dependency build for a chart
///
/// Executes `helm dependency build` in the chart directory to fetch and build chart dependencies.
/// Skips if dependencies are already built to avoid unnecessary work.
fn build_helm_dependencies(chart_path: &Path) -> Result<()> {
    use std::process::Command;

    // Skip if dependencies are already built
    if dependencies_already_built(chart_path) {
        tracing::debug!(
            "Helm dependencies already built for chart at: {}, skipping",
            chart_path.display()
        );
        return Ok(());
    }

    tracing::debug!("Building Helm dependencies for chart at: {}", chart_path.display());

    let output = Command::new("helm")
        .arg("dependency")
        .arg("build")
        .arg(chart_path)
        .output()
        .map_err(|e| NylError::Process(format!("Failed to execute helm dependency build: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NylError::HelmChart(format!("helm dependency build failed: {}", stderr)));
    }

    tracing::debug!(
        "Successfully built Helm dependencies for chart at: {}",
        chart_path.display()
    );

    Ok(())
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
            name: Some(temp.path().join("mychart").to_string_lossy().to_string()),
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
            name: Some("./mychart".to_string()),
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
            name: Some("./missing".to_string()),
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
            name: Some("./bad-chart".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Chart.yaml not found"));
    }

    // Git chart resolution test is now an integration test
    // as it requires actual Git operations

    #[test]
    fn test_parse_repository_protocol_git_https() {
        let (protocol, url) = parse_repository_protocol("git+https://github.com/user/repo.git");
        assert_eq!(protocol, RepositoryProtocol::Git);
        assert_eq!(url, "https://github.com/user/repo.git");
    }

    #[test]
    fn test_parse_repository_protocol_git_ssh() {
        let (protocol, url) = parse_repository_protocol("git+git@github.com:user/repo.git");
        assert_eq!(protocol, RepositoryProtocol::Git);
        assert_eq!(url, "git@github.com:user/repo.git");
    }

    #[test]
    fn test_parse_repository_protocol_oci() {
        let (protocol, url) = parse_repository_protocol("oci://ghcr.io/owner/chart");
        assert_eq!(protocol, RepositoryProtocol::Oci);
        assert_eq!(url, "oci://ghcr.io/owner/chart");
    }

    #[test]
    fn test_parse_repository_protocol_helm() {
        let (protocol, url) = parse_repository_protocol("https://charts.example.com");
        assert_eq!(protocol, RepositoryProtocol::Helm);
        assert_eq!(url, "https://charts.example.com");
    }

    #[test]
    fn test_parse_repository_protocol_plain_url() {
        let (protocol, url) = parse_repository_protocol("charts.example.com");
        assert_eq!(protocol, RepositoryProtocol::Helm);
        assert_eq!(url, "charts.example.com");
    }

    #[test]
    fn test_resolve_repository_requires_version() {
        let temp = TempDir::new().unwrap();
        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef {
            repository: Some("oci://ghcr.io/owner/nyl/chart".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version is required"));
    }

    #[test]
    fn test_resolve_no_repository_or_name() {
        let temp = TempDir::new().unwrap();
        let resolver = HelmChartResolver::new(vec![], temp.path().to_path_buf());

        let chart_ref = ChartRef::default();

        let result = resolver.resolve_chart(&chart_ref);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must have either 'repository' or 'name'"));
    }

    #[test]
    fn test_chart_has_dependencies_with_chart_lock() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create Chart.yaml without dependencies
        fs::write(
            chart_dir.join("Chart.yaml"),
            "apiVersion: v2\nname: test-chart\nversion: 1.0.0\n",
        )
        .unwrap();

        // Create Chart.lock
        fs::write(
            chart_dir.join("Chart.lock"),
            "dependencies:\n- name: nginx\n  version: 1.0.0\n",
        )
        .unwrap();

        assert!(chart_has_dependencies(&chart_dir).unwrap());
    }

    #[test]
    fn test_chart_has_dependencies_in_chart_yaml() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create Chart.yaml with dependencies
        let chart_yaml = r#"apiVersion: v2
name: test-chart
version: 1.0.0
dependencies:
  - name: nginx
    version: "1.0.0"
    repository: "https://charts.bitnami.com/bitnami"
"#;
        fs::write(chart_dir.join("Chart.yaml"), chart_yaml).unwrap();

        assert!(chart_has_dependencies(&chart_dir).unwrap());
    }

    #[test]
    fn test_chart_has_no_dependencies() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create Chart.yaml without dependencies
        fs::write(
            chart_dir.join("Chart.yaml"),
            "apiVersion: v2\nname: test-chart\nversion: 1.0.0\n",
        )
        .unwrap();

        assert!(!chart_has_dependencies(&chart_dir).unwrap());
    }

    #[test]
    fn test_chart_has_dependencies_no_chart_yaml() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // No Chart.yaml exists
        assert!(!chart_has_dependencies(&chart_dir).unwrap());
    }

    #[test]
    fn test_dependencies_already_built_with_tgz() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create Chart.lock (required for dependencies to be considered built)
        fs::write(
            chart_dir.join("Chart.lock"),
            "dependencies:\n- name: test\n  version: 1.0.0\n",
        )
        .unwrap();

        // Create charts directory with a .tgz file
        let charts_dir = chart_dir.join("charts");
        fs::create_dir_all(&charts_dir).unwrap();
        fs::write(charts_dir.join("dependency-1.0.0.tgz"), "fake tgz content").unwrap();

        assert!(dependencies_already_built(&chart_dir));
    }

    #[test]
    fn test_dependencies_not_built_no_chart_lock() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create charts directory with a .tgz file but NO Chart.lock
        let charts_dir = chart_dir.join("charts");
        fs::create_dir_all(&charts_dir).unwrap();
        fs::write(charts_dir.join("dependency-1.0.0.tgz"), "fake tgz content").unwrap();

        // Should return false because Chart.lock doesn't exist
        assert!(!dependencies_already_built(&chart_dir));
    }

    #[test]
    fn test_dependencies_not_built_no_charts_dir() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // No charts directory exists
        assert!(!dependencies_already_built(&chart_dir));
    }

    #[test]
    fn test_dependencies_not_built_empty_charts_dir() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create empty charts directory
        let charts_dir = chart_dir.join("charts");
        fs::create_dir_all(&charts_dir).unwrap();

        assert!(!dependencies_already_built(&chart_dir));
    }

    #[test]
    fn test_dependencies_not_built_no_tgz_files() {
        let temp = TempDir::new().unwrap();
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();

        // Create charts directory with non-.tgz files
        let charts_dir = chart_dir.join("charts");
        fs::create_dir_all(&charts_dir).unwrap();
        fs::write(charts_dir.join("README.md"), "readme content").unwrap();

        assert!(!dependencies_already_built(&chart_dir));
    }
}
