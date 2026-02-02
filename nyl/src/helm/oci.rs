/// OCI registry chart pulling via `helm pull`
///
/// Downloads and caches Helm charts from OCI registries (e.g. ghcr.io).
/// Charts are cached locally to avoid redundant pulls.
///
/// # Cache Layout
///
/// ```text
/// $NYL_CACHE_DIR/helm/oci/
/// └── {repo_hash}-{version}/  # Extracted chart directory
/// ```
use crate::{NylError, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

/// Pulls Helm charts from OCI registries and caches them locally
pub struct OciChartPuller {
    cache_dir: PathBuf,
}

impl OciChartPuller {
    /// Create a new OCI chart puller
    ///
    /// Uses `NYL_CACHE_DIR` if set, otherwise falls back to `.nyl/cache/` in the
    /// current directory.
    pub fn new() -> Result<Self> {
        let root = if let Ok(cache_dir) = std::env::var("NYL_CACHE_DIR") {
            PathBuf::from(cache_dir)
        } else {
            std::env::current_dir()?.join(".nyl").join("cache")
        };

        Ok(Self {
            cache_dir: root.join("helm").join("oci"),
        })
    }

    /// Create a puller with an explicit cache directory (useful for testing)
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into().join("helm").join("oci"),
        }
    }

    /// Pull a chart from an OCI repository
    ///
    /// # Arguments
    /// * `repository` - Full OCI URL like `oci://ghcr.io/owner/repo/chart`
    /// * `version` - Chart version like `0.1.0` or `0.1.0-sha-abc1234`
    ///
    /// # Returns
    /// Path to the extracted chart directory
    pub fn pull(&self, repository: &str, version: &str) -> Result<PathBuf> {
        let chart_dir = self.chart_cache_path(repository, version);

        // Return cached chart if it exists and contains Chart.yaml
        if chart_dir.join("Chart.yaml").exists() {
            return Ok(chart_dir);
        }

        // Ensure cache directory exists
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| NylError::Process(format!("Failed to create OCI cache directory: {}", e)))?;

        // Run: helm pull <repository> --version <version> --untar -d <cache_dir>
        let output = Command::new("helm")
            .arg("pull")
            .arg(repository)
            .arg("--version")
            .arg(version)
            .arg("--untar")
            .arg("-d")
            .arg(&self.cache_dir)
            .output()
            .map_err(|e| NylError::Process(format!("Failed to execute helm pull: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NylError::HelmChart(format!(
                "helm pull failed for {}@{}: {}",
                repository, version, stderr
            )));
        }

        // helm pull --untar extracts to <cache_dir>/<chart-name>/
        // Find the extracted chart directory (there should be exactly one new directory)
        let chart_name = extract_chart_name(repository);
        let extracted = self.cache_dir.join(&chart_name);

        if !extracted.join("Chart.yaml").exists() {
            // Rename extracted directory to our expected cache path
            // This handles the case where the directory name differs
            return Err(NylError::HelmChart(format!(
                "Chart.yaml not found after pulling {}@{} (expected at {})",
                repository,
                version,
                extracted.display()
            )));
        }

        // Move to the versioned cache path if different
        if extracted != chart_dir {
            std::fs::rename(&extracted, &chart_dir)
                .map_err(|e| NylError::Process(format!("Failed to move cached chart: {}", e)))?;
        }

        Ok(chart_dir)
    }

    /// Compute the cache path for a given repository and version
    fn chart_cache_path(&self, repository: &str, version: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(repository.as_bytes());
        let repo_hash = hex::encode(hasher.finalize());

        self.cache_dir.join(format!("{}-{}", &repo_hash[..16], version))
    }
}

/// Extract the chart name from an OCI repository URL
///
/// The chart name is the last path segment of the OCI URL.
/// e.g. `oci://ghcr.io/owner/repo/chart` → `chart`
fn extract_chart_name(repository: &str) -> String {
    repository
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("chart")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_chart_name() {
        assert_eq!(extract_chart_name("oci://ghcr.io/owner/repo/mychart"), "mychart");
        assert_eq!(extract_chart_name("oci://ghcr.io/niklasrosenstein/nyl/chart"), "chart");
        assert_eq!(extract_chart_name("oci://registry.example.com/charts/nginx"), "nginx");
    }

    #[test]
    fn test_extract_chart_name_trailing_slash() {
        assert_eq!(extract_chart_name("oci://ghcr.io/owner/repo/mychart/"), "mychart");
    }

    #[test]
    fn test_chart_cache_path_deterministic() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        assert_eq!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_versions() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-abc1234");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner/nyl/chart", "0.1.0-sha-def5678");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_chart_cache_path_different_repos() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let path1 = puller.chart_cache_path("oci://ghcr.io/owner1/nyl/chart", "0.1.0");
        let path2 = puller.chart_cache_path("oci://ghcr.io/owner2/nyl/chart", "0.1.0");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_pull_returns_cached_chart() {
        let temp = TempDir::new().unwrap();
        let puller = OciChartPuller::with_cache_dir(temp.path());

        let repo = "oci://ghcr.io/owner/nyl/chart";
        let version = "0.1.0";

        // Pre-populate the cache
        let cache_path = puller.chart_cache_path(repo, version);
        std::fs::create_dir_all(&cache_path).unwrap();
        std::fs::write(
            cache_path.join("Chart.yaml"),
            "apiVersion: v2\nname: chart\nversion: 0.1.0\n",
        )
        .unwrap();

        // Pull should return the cached path without running helm
        let result = puller.pull(repo, version).unwrap();
        assert_eq!(result, cache_path);
        assert!(result.join("Chart.yaml").exists());
    }
}
